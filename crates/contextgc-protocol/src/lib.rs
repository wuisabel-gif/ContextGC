//! Newline-delimited JSON/stdio protocol for ContextGC.
//!
//! Adapters send `Request` objects as single lines over stdin and receive
//! `Response` objects as single lines over stdout.  Human-readable diagnostics
//! must be written to stderr, never stdout.

use contextgc_core::{CompactionPlan, ContextItem, PressureState, WorkingSet};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// A unique request id echoed back in the response.
pub type RequestId = String;

/// Incoming adapter request.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)]
pub enum Request {
    #[serde(rename = "session.start")]
    SessionStart {
        request_id: RequestId,
        session_id: String,
        model: ModelInfoMsg,
    },
    #[serde(rename = "context.add")]
    ContextAdd {
        request_id: RequestId,
        item: ContextItem,
    },
    #[serde(rename = "context.plan")]
    ContextPlan {
        request_id: RequestId,
        #[serde(default)]
        predicted_extra_tokens: u64,
    },
    #[serde(rename = "context.compact")]
    ContextCompact {
        request_id: RequestId,
        #[serde(default)]
        predicted_extra_tokens: u64,
    },
    #[serde(rename = "context.materialize")]
    ContextMaterialize { request_id: RequestId },
    #[serde(rename = "context.stats")]
    ContextStats { request_id: RequestId },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfoMsg {
    pub name: String,
    pub context_window: u64,
    #[serde(default)]
    pub reserved_output_tokens: u64,
}

/// Outgoing adapter response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok {
        request_id: RequestId,
    },
    Plan {
        request_id: RequestId,
        plan: CompactionPlan,
    },
    WorkingSet {
        request_id: RequestId,
        working_set: WorkingSet,
    },
    Stats {
        request_id: RequestId,
        stats: StatsMsg,
    },
    Error {
        request_id: RequestId,
        error: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatsMsg {
    pub session_id: String,
    pub context_window: u64,
    pub current_tokens: u64,
    pub usable_context: u64,
    pub pressure: f32,
    pub predicted_pressure: f32,
    pub pressure_state: PressureState,
    pub item_count: u64,
    pub composition: Vec<(String, u64)>,
}

impl Response {
    pub fn request_id(&self) -> &str {
        match self {
            Response::Ok { request_id, .. }
            | Response::Plan { request_id, .. }
            | Response::WorkingSet { request_id, .. }
            | Response::Stats { request_id, .. }
            | Response::Error { request_id, .. } => request_id,
        }
    }
}

/// Write a response line to the adapter.
pub fn write_response<W: Write>(writer: &mut W, resp: &Response) -> Result<(), ProtocolError> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(())
}

/// Read the next request line from the adapter.
pub fn read_request<R: BufRead>(reader: &mut R) -> Result<Option<Request>, ProtocolError> {
    loop {
        let mut buf = String::new();
        let n = reader.read_line(&mut buf)?;
        if n == 0 {
            return Ok(None);
        }
        let trimmed = buf.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req = serde_json::from_str(trimmed)?;
        return Ok(Some(req));
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn round_trip_request_response() {
        let req = Request::ContextStats {
            request_id: "r1".to_string(),
        };
        let mut buf = Vec::new();
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        buf.extend_from_slice(line.as_bytes());
        let mut reader = BufReader::new(&buf[..]);
        let read = read_request(&mut reader).unwrap().unwrap();
        match read {
            Request::ContextStats { request_id } => assert_eq!(request_id, "r1"),
            _ => panic!("unexpected request"),
        }
    }

    #[test]
    fn write_response_format() {
        let resp = Response::Ok {
            request_id: "r1".to_string(),
        };
        let mut buf = Vec::new();
        write_response(&mut buf, &resp).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.ends_with('\n'));
        assert!(s.contains("\"type\":\"ok\""));
    }
}
