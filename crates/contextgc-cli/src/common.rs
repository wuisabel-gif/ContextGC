//! Shared CLI helpers: session reconstruction and model persistence.

use anyhow::{Context, Result};
use contextgc_core::{Config, ContextItem, ContextSource, SessionId};
use contextgc_engine::{ModelInfo, Session};
use contextgc_store::Store;
use std::path::{Path, PathBuf};

/// Resolve the database path: explicit flag, env var, or local default.
pub fn resolve_db_path(explicit: Option<&Path>) -> PathBuf {
    explicit
        .map(|p| p.to_path_buf())
        .or_else(|| std::env::var("CONTEXTGC_DB").ok().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from(".contextgc.db"))
}

/// Load configuration from an explicit path, `CONTEXTGC_CONFIG`, or defaults.
pub fn load_config(explicit: Option<&Path>) -> Result<Config> {
    let path = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var("CONTEXTGC_CONFIG").ok().map(PathBuf::from));
    let Some(path) = path else {
        return Ok(Config::default());
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read config {}", path.display()))?;
    Config::from_toml(&text).map_err(|error| anyhow::anyhow!(error))
}

/// Load the model info persisted for a session, if any.
pub fn load_model_info(store: &Store, session_id: &SessionId) -> Result<Option<ModelInfo>> {
    let payload = store
        .latest_event(session_id, "session.start")
        .context("query session.start event")?;
    match payload {
        Some(json) => Ok(Some(serde_json::from_str(&json)?)),
        None => Ok(None),
    }
}

/// Persist model info as a `session.start` event.
pub fn save_model_info(store: &Store, session_id: &SessionId, model: &ModelInfo) -> Result<()> {
    store.append_event(session_id, "session.start", model)?;
    Ok(())
}

/// Open the session, creating it on first use.
#[allow(clippy::too_many_arguments)]
pub fn open_session(
    db_path: &Path,
    session_id: &str,
    config_path: Option<&Path>,
    model_name: Option<String>,
    context_window: Option<u64>,
    reserved_output: Option<u64>,
) -> Result<Session> {
    let store = Store::open(db_path).context("open ContextGC database")?;
    let sid = SessionId::new(session_id);
    let config = load_config(config_path)?;
    store.ensure_session(&sid, &config)?;

    let mut model = load_model_info(&store, &sid)?.unwrap_or_else(|| ModelInfo {
        context_window: config.context.max_context_window,
        reserved_output_tokens: config.reserve.output_tokens,
        ..ModelInfo::default()
    });
    if let Some(name) = model_name {
        model.name = name;
    }
    if let Some(w) = context_window {
        model.context_window = w;
    }
    if let Some(r) = reserved_output {
        model.reserved_output_tokens = r;
    }
    save_model_info(&store, &sid, &model)?;

    Session::new(sid, config, model, Some(db_path)).context("open session")
}

/// Parse one JSONL line into a context item.
///
/// Accepts either a bare `ContextItem` or a protocol-style
/// `{"type":"context.add","item":{...}}` envelope.
pub fn parse_item_line(line: &str) -> Result<ContextItem> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        anyhow::bail!("empty line");
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)?;
    if value.get("type").and_then(|t| t.as_str()) == Some("context.add") {
        let item = value
            .get("item")
            .ok_or_else(|| anyhow::anyhow!("context.add envelope missing `item`"))?;
        Ok(serde_json::from_value(item.clone())?)
    } else {
        Ok(serde_json::from_value(value)?)
    }
}

/// Ingest a JSONL stream of context items into the session.
pub fn ingest_lines(session: &mut Session, lines: impl Iterator<Item = String>) -> Result<usize> {
    let mut count = 0usize;
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        // Session fixtures often contain a protocol-level start record before
        // their context.add records. The CLI session is already configured by
        // flags, so that envelope is metadata rather than an item to ingest.
        if serde_json::from_str::<serde_json::Value>(&line)
            .ok()
            .and_then(|value| {
                value
                    .get("type")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned)
            })
            .as_deref()
            == Some("session.start")
        {
            continue;
        }
        let item = parse_item_line(&line).with_context(|| format!("line {}", i + 1))?;
        session.ingest(item, ContextSource::Ingest)?;
        count += 1;
    }
    Ok(count)
}
