//! Newline-delimited JSON/stdio protocol server.
//!
//! Reads `Request` lines from stdin and writes `Response` lines to stdout.
//! All diagnostics go to stderr so stdout stays machine-safe.

use anyhow::{Context as _, Result};
use contextgc_core::{Config, ContextSource, SessionId, TokenPrediction};
use contextgc_engine::{ModelInfo, Session};
use contextgc_protocol::{ModelInfoMsg, Request, Response, read_request, write_response};
use contextgc_store::Store;
use std::io::{BufReader, Write};
use std::path::PathBuf;

/// State for the stdio server: one session at a time.
struct ServerState {
    db_path: PathBuf,
    config: Config,
    session: Option<Session>,
}

impl ServerState {
    fn new(db_path: PathBuf, config: Config) -> Self {
        Self {
            db_path,
            config,
            session: None,
        }
    }

    fn require_session(&mut self) -> Result<&mut Session> {
        match &mut self.session {
            Some(s) => Ok(s),
            None => anyhow::bail!("no active session: send session.start first"),
        }
    }
}

fn prediction_from(extra: u64) -> Option<TokenPrediction> {
    (extra > 0).then_some(TokenPrediction {
        expected_next_input: 0,
        expected_next_tool_output: extra,
        expected_next_assistant_output: 0,
        confidence: 0.5,
    })
}

fn handle_request(state: &mut ServerState, req: Request) -> Response {
    match req {
        Request::SessionStart {
            request_id,
            session_id,
            model,
        } => match start_session(state, session_id, model) {
            Ok(()) => Response::Ok { request_id },
            Err(e) => error_response(request_id, e),
        },
        Request::ContextAdd { request_id, item } => {
            let result = state
                .require_session()
                .and_then(|sess| Ok(sess.ingest(item, ContextSource::Adapter("stdio".into()))?))
                .context("context.add");
            match result {
                Ok(id) => {
                    eprintln!("context.add: ingested {}", id.as_str());
                    Response::Ok { request_id }
                }
                Err(e) => error_response(request_id, e),
            }
        }
        Request::ContextPlan {
            request_id,
            predicted_extra_tokens,
        } => {
            let result = state
                .require_session()
                .and_then(|sess| Ok(sess.plan(prediction_from(predicted_extra_tokens))?))
                .context("context.plan");
            match result {
                Ok(plan) => Response::Plan { request_id, plan },
                Err(e) => error_response(request_id, e),
            }
        }
        Request::ContextCompact {
            request_id,
            predicted_extra_tokens,
        } => {
            let result = state
                .require_session()
                .and_then(|sess| Ok(sess.compact(prediction_from(predicted_extra_tokens))?))
                .context("context.compact");
            match result {
                Ok(working_set) => Response::WorkingSet {
                    request_id,
                    working_set,
                },
                Err(e) => error_response(request_id, e),
            }
        }
        Request::ContextMaterialize { request_id } => {
            let result = state
                .require_session()
                .and_then(|sess| {
                    let plan = sess.plan(None)?;
                    Ok(sess.materialize(&plan)?)
                })
                .context("context.materialize");
            match result {
                Ok(working_set) => Response::WorkingSet {
                    request_id,
                    working_set,
                },
                Err(e) => error_response(request_id, e),
            }
        }
        Request::ContextStats { request_id } => {
            let result = state
                .require_session()
                .and_then(|sess| Ok(sess.status()?))
                .context("context.stats");
            match result {
                Ok(status) => {
                    let stats = contextgc_protocol::StatsMsg {
                        session_id: status.session_id,
                        context_window: status.context_window,
                        current_tokens: status.current_tokens,
                        usable_context: status.usable_context,
                        pressure: status.pressure,
                        predicted_pressure: status.predicted_pressure,
                        pressure_state: status.pressure_state,
                        item_count: status.item_count as u64,
                        composition: status.composition,
                    };
                    Response::Stats { request_id, stats }
                }
                Err(e) => error_response(request_id, e),
            }
        }
    }
}

fn error_response(request_id: String, err: anyhow::Error) -> Response {
    Response::Error {
        request_id,
        error: format!("{err:#}"),
    }
}

fn start_session(state: &mut ServerState, session_id: String, model: ModelInfoMsg) -> Result<()> {
    let store = Store::open(&state.db_path)?;
    let sid = SessionId::new(session_id);
    let config = state.config.clone();
    store.ensure_session(&sid, &config)?;

    let info = ModelInfo {
        name: model.name,
        context_window: model.context_window,
        reserved_output_tokens: if model.reserved_output_tokens == 0 {
            config.reserve.output_tokens
        } else {
            model.reserved_output_tokens
        },
    };
    crate::common::save_model_info(&store, &sid, &info)?;

    let session =
        Session::new(sid, config, info, Some(&state.db_path)).context("create session")?;
    state.session = Some(session);
    Ok(())
}

/// Run the stdio protocol loop until EOF.
pub fn run(db: Option<&std::path::Path>, config_path: Option<&std::path::Path>) -> Result<()> {
    let db_path = crate::common::resolve_db_path(db);
    let config = crate::common::load_config(config_path)?;
    let mut state = ServerState::new(db_path, config);
    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    loop {
        let req = match read_request(&mut reader) {
            Ok(Some(r)) => r,
            Ok(None) => break,
            Err(e) => {
                // Malformed input must produce a machine-readable error on
                // stdout without killing the server.
                let resp = Response::Error {
                    request_id: "unknown".to_string(),
                    error: format!("malformed request: {e}"),
                };
                write_response(&mut out, &resp)?;
                continue;
            }
        };
        let resp = handle_request(&mut state, req);
        write_response(&mut out, &resp)?;
        out.flush()?;
    }
    Ok(())
}
