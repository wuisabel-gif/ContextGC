//! Optional long-term memory backend contracts for ContextGC.
//!
//! ContextGC answers: "What should the model know right now?"
//! A memory backend answers: "What might be useful again later?"
//!
//! This crate deliberately does not depend on MemWhale. A MemWhale adapter can
//! use these types over MCP or another local transport without coupling the
//! ContextGC core to a specific memory implementation.

use async_trait::async_trait;
use contextgc_core::{ContextId, ContextKind, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("memory backend is disabled")]
    Disabled,
    #[error("memory backend error: {0}")]
    Backend(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Stable reference to a long-term memory record.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryRef {
    pub backend: String,
    pub key: String,
}

impl MemoryRef {
    pub fn uri(&self) -> String {
        format!("memory://{}/{}", self.backend, self.key)
    }
}

/// A context object selected for possible long-term storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalizedContext {
    pub session_id: SessionId,
    pub context_id: ContextId,
    pub kind: ContextKind,
    pub summary: String,
    pub original_tokens: u64,
    pub importance: f32,
    pub memory_value: f32,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_artifact_ref: Option<String>,
}

/// Query issued when an old memory may be relevant again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetrievalQuery {
    pub session_id: SessionId,
    pub query: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_max_results")]
    pub max_results: u32,
}

fn default_max_results() -> u32 {
    5
}

/// A lightweight result suitable for ranking before promotion into context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCandidate {
    pub reference: MemoryRef,
    pub summary: String,
    pub relevance: f32,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Full stored memory returned after a candidate is selected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredContext {
    pub reference: MemoryRef,
    pub summary: String,
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_context_id: Option<ContextId>,
}

/// Pluggable long-term memory interface.
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn store(&self, item: ExternalizedContext) -> Result<MemoryRef, MemoryError>;

    async fn retrieve(&self, query: RetrievalQuery) -> Result<Vec<MemoryCandidate>, MemoryError>;

    async fn get(&self, reference: &MemoryRef) -> Result<Option<StoredContext>, MemoryError>;
}

/// Safe default backend. It returns references that are explicitly marked as
/// non-persistent and never returns retrieval results.
pub struct NullMemoryBackend;

#[async_trait]
impl MemoryBackend for NullMemoryBackend {
    async fn store(&self, item: ExternalizedContext) -> Result<MemoryRef, MemoryError> {
        Ok(MemoryRef {
            backend: "none".to_string(),
            key: item.context_id.as_str().to_string(),
        })
    }

    async fn retrieve(&self, _query: RetrievalQuery) -> Result<Vec<MemoryCandidate>, MemoryError> {
        Ok(Vec::new())
    }

    async fn get(&self, _reference: &MemoryRef) -> Result<Option<StoredContext>, MemoryError> {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_ref_has_stable_uri() {
        let reference = MemoryRef {
            backend: "memwhale".to_string(),
            key: "memory-42".to_string(),
        };
        assert_eq!(reference.uri(), "memory://memwhale/memory-42");
    }

    #[test]
    fn retrieval_query_defaults_result_limit() {
        let query: RetrievalQuery =
            serde_json::from_str(r#"{"session_id":"session-1","query":"serde error"}"#).unwrap();
        assert_eq!(query.max_results, 5);
    }
}
