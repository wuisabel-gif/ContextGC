//! SQLite-backed append-only persistent history for ContextGC.
//!
//! The store keeps the complete event stream and all context items.  Compaction
//! only changes the *active* working set; the original rows stay immutable.

use chrono::Utc;
use contextgc_core::{
    CompactionPlan, CompressionLevel, Config, ContextId, ContextItem, MaterializedContextItem,
    SessionId, hash_content,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("missing session: {0}")]
    MissingSession(String),
    #[error("numeric value is too large for SQLite: {0}")]
    NumericOverflow(&'static str),
}

/// Persistent store for one ContextGC database.
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (and create/migrate) a store at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StoreError> {
        let conn = Connection::open(path)?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory store, useful for tests and transient sessions.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        let conn = Connection::open_in_memory()?;
        let store = Self { conn };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<(), StoreError> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                config_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS events (
                event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                type TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_events_session ON events(session_id);

            CREATE TABLE IF NOT EXISTS context_items (
                item_id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_id TEXT,
                kind TEXT NOT NULL,
                content_hash TEXT NOT NULL,
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                created_at INTEGER NOT NULL,
                source_json TEXT NOT NULL,
                metadata_json TEXT NOT NULL,
                state TEXT NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_items_session ON context_items(session_id);
            CREATE INDEX IF NOT EXISTS idx_items_hash ON context_items(session_id, content_hash);

            CREATE TABLE IF NOT EXISTS artifacts (
                artifact_id TEXT PRIMARY KEY,
                item_id TEXT NOT NULL,
                content TEXT NOT NULL,
                created_at INTEGER NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_artifacts_item ON artifacts(item_id);

            CREATE TABLE IF NOT EXISTS compaction_runs (
                run_id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                before_tokens INTEGER NOT NULL,
                after_tokens INTEGER NOT NULL,
                pressure_state TEXT NOT NULL,
                created_at INTEGER NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_compactions_session ON compaction_runs(session_id);

            CREATE TABLE IF NOT EXISTS compaction_actions (
                run_id INTEGER NOT NULL,
                item_id TEXT NOT NULL,
                action TEXT NOT NULL,
                from_level TEXT NOT NULL,
                to_level TEXT NOT NULL,
                estimated_before INTEGER NOT NULL,
                estimated_after INTEGER NOT NULL,
                importance_json TEXT NOT NULL,
                reason TEXT NOT NULL
            ) STRICT;

            CREATE INDEX IF NOT EXISTS idx_actions_run ON compaction_actions(run_id);

            CREATE TABLE IF NOT EXISTS token_stats (
                session_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                ema REAL NOT NULL,
                count INTEGER NOT NULL,
                PRIMARY KEY (session_id, event_type)
            ) STRICT;

            CREATE TABLE IF NOT EXISTS working_set_meta (
                session_id TEXT PRIMARY KEY,
                materialized_at INTEGER NOT NULL
            ) STRICT;

            CREATE TABLE IF NOT EXISTS working_set_items (
                session_id TEXT NOT NULL,
                item_id TEXT NOT NULL,
                parent_id TEXT,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                token_count INTEGER NOT NULL,
                compression_level TEXT NOT NULL,
                artifact_ref TEXT,
                PRIMARY KEY (session_id, item_id)
            ) STRICT;
            "#,
        )?;
        Ok(())
    }

    /// Ensure a session row exists.
    pub fn ensure_session(
        &self,
        session_id: &SessionId,
        config: &Config,
    ) -> Result<(), StoreError> {
        let config_json = serde_json::to_string(config)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO sessions (session_id, config_json, created_at)
             VALUES (?1, ?2, ?3)",
            params![
                session_id.as_str(),
                config_json,
                Utc::now().timestamp_millis()
            ],
        )?;
        Ok(())
    }

    /// Append an event to the immutable event log.
    pub fn append_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
        payload: &impl Serialize,
    ) -> Result<i64, StoreError> {
        let payload_json = serde_json::to_string(payload)?;
        self.conn.execute(
            "INSERT INTO events (session_id, type, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                session_id.as_str(),
                event_type,
                payload_json,
                Utc::now().timestamp_millis()
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Insert a context item.  The original row is never mutated.
    pub fn insert_item(
        &self,
        session_id: &SessionId,
        item: &ContextItem,
    ) -> Result<(), StoreError> {
        let source_json = serde_json::to_string(&item.source)?;
        let metadata_json = serde_json::to_string(&item.metadata)?;
        // The database is the trust boundary for deduplication. Never trust
        // an adapter-supplied hash without recomputing it from the payload.
        let hash = hash_content(&item.content);
        let token_count = i64::try_from(item.token_count)
            .map_err(|_| StoreError::NumericOverflow("context item token_count"))?;
        self.conn.execute(
            "INSERT INTO context_items
                (item_id, session_id, parent_id, kind, content_hash, content,
                 token_count, created_at, source_json, metadata_json, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                item.id.as_str(),
                session_id.as_str(),
                item.parent_id.as_ref().map(|p| p.as_str()),
                format!("{:?}", item.kind),
                hash,
                &item.content,
                token_count,
                item.created_at.timestamp_millis(),
                source_json,
                metadata_json,
                format!("{:?}", item.state),
            ],
        )?;
        Ok(())
    }

    /// Fetch a single item by id.
    pub fn get_item(&self, id: &ContextId) -> Result<Option<ContextItem>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT item_id, session_id, parent_id, kind, content_hash, content,
                    token_count, created_at, source_json, metadata_json, state
             FROM context_items WHERE item_id = ?1",
        )?;
        let mut rows = stmt.query(params![id.as_str()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Self::row_to_item(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all items for a session in creation order.
    pub fn items_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<ContextItem>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT item_id, session_id, parent_id, kind, content_hash, content,
                    token_count, created_at, source_json, metadata_json, state
             FROM context_items
             WHERE session_id = ?1
             ORDER BY created_at ASC, item_id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.as_str()], Self::row_to_item)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Return items that have not been abandoned/superseded.
    pub fn active_items(&self, session_id: &SessionId) -> Result<Vec<ContextItem>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT item_id, session_id, parent_id, kind, content_hash, content,
                    token_count, created_at, source_json, metadata_json, state
             FROM context_items
             WHERE session_id = ?1 AND state != 'Abandoned' AND state != 'Superseded'
             ORDER BY created_at ASC, item_id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.as_str()], Self::row_to_item)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Count how many times each content hash appears in the session.
    pub fn duplicate_counts(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<DedupSummary>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT content_hash, COUNT(*) as c, MAX(item_id)
             FROM context_items
             WHERE session_id = ?1
             GROUP BY content_hash
             HAVING c > 1
             ORDER BY c DESC",
        )?;
        let rows = stmt.query_map(params![session_id.as_str()], |row| {
            Ok(DedupSummary {
                content_hash: row.get(0)?,
                duplicate_count: sqlite_u64(row.get::<_, i64>(1)?, 1)?,
                canonical_id: ContextId::new(row.get::<_, String>(2)?),
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Record the result of a compaction run.
    pub fn record_compaction(
        &mut self,
        session_id: &SessionId,
        plan: &CompactionPlan,
    ) -> Result<i64, StoreError> {
        let tx = self.conn.transaction()?;
        let before_tokens = i64::try_from(plan.before_tokens)
            .map_err(|_| StoreError::NumericOverflow("compaction before_tokens"))?;
        let after_tokens = i64::try_from(plan.expected_tokens_after)
            .map_err(|_| StoreError::NumericOverflow("compaction after_tokens"))?;
        tx.execute(
            "INSERT INTO compaction_runs
                (session_id, before_tokens, after_tokens, pressure_state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                session_id.as_str(),
                before_tokens,
                after_tokens,
                format!("{:?}", plan.pressure_state),
                Utc::now().timestamp_millis()
            ],
        )?;
        let run_id = tx.last_insert_rowid();
        {
            let mut stmt = tx.prepare(
                "INSERT INTO compaction_actions
                    (run_id, item_id, action, from_level, to_level,
                     estimated_before, estimated_after, importance_json, reason)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for action in &plan.actions {
                let estimated_before =
                    i64::try_from(action.estimated_tokens_before).map_err(|_| {
                        StoreError::NumericOverflow("compaction estimated_tokens_before")
                    })?;
                let estimated_after =
                    i64::try_from(action.estimated_tokens_after).map_err(|_| {
                        StoreError::NumericOverflow("compaction estimated_tokens_after")
                    })?;
                stmt.execute(params![
                    run_id,
                    action.context_id.as_str(),
                    format!("{:?}", action.action),
                    format!("{:?}", action.from_level),
                    format!("{:?}", action.to_level),
                    estimated_before,
                    estimated_after,
                    serde_json::to_string(&action.importance)?,
                    &action.reason,
                ])?;
            }
        }
        tx.commit()?;
        Ok(run_id)
    }

    /// Store an artifact and return its id.
    pub fn insert_artifact(
        &self,
        artifact_id: &str,
        item_id: &ContextId,
        content: &str,
    ) -> Result<String, StoreError> {
        self.conn.execute(
            "INSERT INTO artifacts (artifact_id, item_id, content, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                artifact_id,
                item_id.as_str(),
                content,
                Utc::now().timestamp_millis()
            ],
        )?;
        Ok(artifact_id.to_string())
    }

    /// Retrieve artifact content by id.
    pub fn get_artifact(&self, artifact_id: &str) -> Result<Option<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT content FROM artifacts WHERE artifact_id = ?1")?;
        let content: Option<String> = stmt
            .query_row(params![artifact_id], |row| row.get(0))
            .optional()?;
        Ok(content)
    }

    /// Load persisted token statistics for a session.
    pub fn load_token_stats(&self, session_id: &SessionId) -> Result<Vec<TokenStat>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT event_type, ema, count FROM token_stats WHERE session_id = ?1")?;
        let rows = stmt.query_map(params![session_id.as_str()], |row| {
            Ok(TokenStat {
                event_type: row.get(0)?,
                ema: row.get::<_, f64>(1)? as f32,
                count: sqlite_u64(row.get::<_, i64>(2)?, 2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Upsert a token-stat row.
    pub fn upsert_token_stat(
        &mut self,
        session_id: &SessionId,
        event_type: &str,
        ema: f32,
        count: u64,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO token_stats (session_id, event_type, ema, count)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(session_id, event_type) DO UPDATE SET
                 ema = excluded.ema,
                 count = excluded.count",
            params![
                session_id.as_str(),
                event_type,
                ema as f64,
                i64::try_from(count)
                    .map_err(|_| StoreError::NumericOverflow("token stat count"))?,
            ],
        )?;
        Ok(())
    }

    /// Load the persisted active working-set projection.
    ///
    /// `None` means no compaction has been materialized for this session;
    /// `Some(empty)` is a valid projection in which every item was evicted.
    pub fn load_working_set(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<Vec<MaterializedContextItem>>, StoreError> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM working_set_meta WHERE session_id = ?1",
                params![session_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT item_id, parent_id, kind, content, token_count,
                    compression_level, artifact_ref
             FROM working_set_items
             WHERE session_id = ?1
             ORDER BY rowid ASC",
        )?;
        let rows = stmt.query_map(params![session_id.as_str()], |row| {
            let kind = row.get::<_, String>(2)?;
            let compression_level = row.get::<_, String>(5)?;
            Ok(MaterializedContextItem {
                id: ContextId::new(row.get::<_, String>(0)?),
                parent_id: row.get::<_, Option<String>>(1)?.map(ContextId::new),
                kind: parse_kind(&kind),
                content: row.get(3)?,
                token_count: sqlite_u64(row.get::<_, i64>(4)?, 4)?,
                compression_level: parse_compression_level(&compression_level),
                artifact_ref: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map(Some)
            .map_err(Into::into)
    }

    /// Atomically replace the active working-set projection.
    pub fn replace_working_set(
        &mut self,
        session_id: &SessionId,
        items: &[MaterializedContextItem],
    ) -> Result<(), StoreError> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO working_set_meta (session_id, materialized_at)
             VALUES (?1, ?2)
             ON CONFLICT(session_id) DO UPDATE SET materialized_at = excluded.materialized_at",
            params![session_id.as_str(), Utc::now().timestamp_millis()],
        )?;
        tx.execute(
            "DELETE FROM working_set_items WHERE session_id = ?1",
            params![session_id.as_str()],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO working_set_items
                    (session_id, item_id, parent_id, kind, content, token_count,
                     compression_level, artifact_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for item in items {
                let token_count = i64::try_from(item.token_count)
                    .map_err(|_| StoreError::NumericOverflow("working set token_count"))?;
                stmt.execute(params![
                    session_id.as_str(),
                    item.id.as_str(),
                    item.parent_id.as_ref().map(|p| p.as_str()),
                    format!("{:?}", item.kind),
                    &item.content,
                    token_count,
                    format!("{:?}", item.compression_level),
                    item.artifact_ref.as_deref(),
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Load compaction-run history for a session.
    pub fn compaction_history(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<CompactionRunSummary>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, before_tokens, after_tokens, pressure_state, created_at
             FROM compaction_runs
             WHERE session_id = ?1
             ORDER BY run_id ASC",
        )?;
        let rows = stmt.query_map(params![session_id.as_str()], |row| {
            Ok(CompactionRunSummary {
                run_id: row.get(0)?,
                before_tokens: sqlite_u64(row.get::<_, i64>(1)?, 1)?,
                after_tokens: sqlite_u64(row.get::<_, i64>(2)?, 2)?,
                pressure_state: row.get(3)?,
                created_at: sqlite_u64(row.get::<_, i64>(4)?, 4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Count total persisted items and tokens for a session (full history).
    pub fn history_totals(&self, session_id: &SessionId) -> Result<(u64, u64), StoreError> {
        let (count, tokens): (i64, i64) = self.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(token_count), 0)
             FROM context_items WHERE session_id = ?1",
            params![session_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((count as u64, tokens as u64))
    }

    /// Return the newest payload (JSON string) for an event type.
    pub fn latest_event(
        &self,
        session_id: &SessionId,
        event_type: &str,
    ) -> Result<Option<String>, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT payload_json FROM events
             WHERE session_id = ?1 AND type = ?2
             ORDER BY event_id DESC LIMIT 1",
        )?;
        let payload: Option<String> = stmt
            .query_row(params![session_id.as_str(), event_type], |row| row.get(0))
            .optional()?;
        Ok(payload)
    }

    fn row_to_item(row: &rusqlite::Row<'_>) -> Result<ContextItem, rusqlite::Error> {
        let source_json: String = row.get("source_json")?;
        let metadata_json: String = row.get("metadata_json")?;
        let kind_str: String = row.get("kind")?;
        let state_str: String = row.get("state")?;
        let source = serde_json::from_str(&source_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let metadata = serde_json::from_str(&metadata_json).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })?;
        let item = ContextItem {
            id: ContextId::new(row.get::<_, String>("item_id")?),
            parent_id: row
                .get::<_, Option<String>>("parent_id")?
                .map(ContextId::new),
            kind: parse_kind(&kind_str),
            content: row.get("content")?,
            token_count: sqlite_u64(row.get::<_, i64>("token_count")?, 6)?,
            created_at: chrono::DateTime::from_timestamp_millis(row.get::<_, i64>("created_at")?)
                .unwrap_or_else(Utc::now),
            source,
            metadata,
            state: parse_state(&state_str),
            compression_level: CompressionLevel::L0,
            content_hash: row.get("content_hash")?,
        };
        Ok(item)
    }
}

fn parse_kind(s: &str) -> contextgc_core::ContextKind {
    match s {
        "SystemPrompt" => contextgc_core::ContextKind::SystemPrompt,
        "DeveloperPrompt" => contextgc_core::ContextKind::DeveloperPrompt,
        "UserMessage" => contextgc_core::ContextKind::UserMessage,
        "AssistantMessage" => contextgc_core::ContextKind::AssistantMessage,
        "ToolCall" => contextgc_core::ContextKind::ToolCall,
        "ToolResult" => contextgc_core::ContextKind::ToolResult,
        "FileContent" => contextgc_core::ContextKind::FileContent,
        "CommandOutput" => contextgc_core::ContextKind::CommandOutput,
        "Error" => contextgc_core::ContextKind::Error,
        "Decision" => contextgc_core::ContextKind::Decision,
        "Constraint" => contextgc_core::ContextKind::Constraint,
        "Checkpoint" => contextgc_core::ContextKind::Checkpoint,
        "Diff" => contextgc_core::ContextKind::Diff,
        "TestResult" => contextgc_core::ContextKind::TestResult,
        _ => contextgc_core::ContextKind::Other,
    }
}

fn parse_state(s: &str) -> contextgc_core::ContextState {
    match s {
        "Active" => contextgc_core::ContextState::Active,
        "Resolved" => contextgc_core::ContextState::Resolved,
        "Superseded" => contextgc_core::ContextState::Superseded,
        "Abandoned" => contextgc_core::ContextState::Abandoned,
        _ => contextgc_core::ContextState::Unknown,
    }
}

fn sqlite_u64(value: i64, column: usize) -> Result<u64, rusqlite::Error> {
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn parse_compression_level(s: &str) -> CompressionLevel {
    match s {
        "L1" => CompressionLevel::L1,
        "L2" => CompressionLevel::L2,
        "L3" => CompressionLevel::L3,
        "L4" => CompressionLevel::L4,
        "L5" => CompressionLevel::L5,
        _ => CompressionLevel::L0,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DedupSummary {
    pub content_hash: String,
    pub canonical_id: ContextId,
    pub duplicate_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenStat {
    pub event_type: String,
    pub ema: f32,
    pub count: u64,
}

/// One recorded compaction run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionRunSummary {
    pub run_id: i64,
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub pressure_state: String,
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use contextgc_core::{ContextItem, ContextKind, ContextSource};

    #[test]
    fn round_trip_item() {
        let store = Store::open_in_memory().unwrap();
        let session = SessionId::new("sess-1");
        store.ensure_session(&session, &Config::default()).unwrap();
        let item = ContextItem::new(ContextKind::UserMessage, "hello world")
            .with_tokens(2)
            .with_source(ContextSource::System);
        store.insert_item(&session, &item).unwrap();
        let loaded = store.get_item(&item.id).unwrap().expect("item present");
        assert_eq!(loaded.content, "hello world");
        assert_eq!(loaded.token_count, 2);
        assert_eq!(loaded.source, ContextSource::System);
    }

    #[test]
    fn duplicate_detection() {
        let store = Store::open_in_memory().unwrap();
        let session = SessionId::new("sess-dedup");
        store.ensure_session(&session, &Config::default()).unwrap();
        for _ in 0..3 {
            let item = ContextItem::new(ContextKind::FileContent, "same content").with_tokens(5);
            store.insert_item(&session, &item).unwrap();
        }
        let dups = store.duplicate_counts(&session).unwrap();
        assert_eq!(dups.len(), 1);
        assert_eq!(dups[0].duplicate_count, 3);
    }

    #[test]
    fn event_log_is_append_only() {
        let store = Store::open_in_memory().unwrap();
        let session = SessionId::new("sess-events");
        store.ensure_session(&session, &Config::default()).unwrap();
        let id = store
            .append_event(&session, "context.add", &"payload")
            .unwrap();
        assert!(id > 0);
    }

    #[test]
    fn working_set_projection_round_trips_without_changing_history() {
        let mut store = Store::open_in_memory().unwrap();
        let session = SessionId::new("sess-working-set");
        store.ensure_session(&session, &Config::default()).unwrap();
        let original =
            ContextItem::new(ContextKind::FileContent, "original payload").with_tokens(5);
        store.insert_item(&session, &original).unwrap();

        assert!(store.load_working_set(&session).unwrap().is_none());
        let projection = vec![MaterializedContextItem {
            id: original.id.clone(),
            parent_id: None,
            kind: ContextKind::FileContent,
            content: "artifact://file/example".to_string(),
            token_count: 3,
            compression_level: CompressionLevel::L4,
            artifact_ref: Some("artifact://file/example".to_string()),
        }];
        store.replace_working_set(&session, &projection).unwrap();

        let loaded = store.load_working_set(&session).unwrap().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "artifact://file/example");
        assert_eq!(store.history_totals(&session).unwrap(), (1, 5));
    }
}
