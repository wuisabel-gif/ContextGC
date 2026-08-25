//! ContextGC engine orchestration crate.
//!
//! A `Session` is the high-level handle adapters and the CLI use to ingest
//! context, predict pressure, plan compaction, and materialize a working set.

use contextgc_core::{
    CompactionPlan, CompressionLevel, Config, ContextAction, ContextBudget, ContextId, ContextItem,
    ContextKind, ContextSource, MaterializedContextItem, PressureState, SessionId, TaskCheckpoint,
    TokenPrediction, WorkingSet,
};
use contextgc_policy::{
    CompactionPlanner, DefaultPlanner, Reducer, default_reducers, event_type_for, update_ema,
};
use contextgc_store::{Store, StoreError, TokenStat};
use contextgc_tokenizer::{ApproximateCounter, TokenCounter};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("store error: {0}")]
    Store(#[from] StoreError),
    #[error("planning error: {0}")]
    Policy(#[from] contextgc_policy::PolicyError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("invalid model budget: {0}")]
    InvalidModelBudget(String),
    #[error("token count is too large: {0}")]
    InvalidTokenCount(String),
    #[error("content hash does not match content")]
    InvalidContentHash,
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("missing item: {0}")]
    MissingItem(String),
}

/// Model metadata used by a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub context_window: u64,
    pub reserved_output_tokens: u64,
}

impl Default for ModelInfo {
    fn default() -> Self {
        Self {
            name: "generic-200k".to_string(),
            context_window: 200_000,
            reserved_output_tokens: 16_000,
        }
    }
}

/// Explainable candidate for reclamation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandidateStatus {
    pub context_id: ContextId,
    pub kind: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub action: ContextAction,
    pub importance: f32,
    pub reason: String,
}

/// Human-readable session status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub model_name: String,
    pub context_window: u64,
    pub current_tokens: u64,
    pub usable_context: u64,
    pub pressure: f32,
    pub predicted_pressure: f32,
    pub pressure_state: PressureState,
    pub item_count: usize,
    pub composition: Vec<(String, u64)>,
    pub top_candidates: Vec<CandidateStatus>,
}

/// A ContextGC session.
pub struct Session {
    session_id: SessionId,
    config: Config,
    model: ModelInfo,
    store: Store,
    token_counter: Box<dyn TokenCounter>,
    reducers: Vec<Box<dyn Reducer>>,
    planner: DefaultPlanner,
    stats: HashMap<String, TokenStat>,
    last_plan: Option<CompactionPlan>,
}

impl Session {
    /// Open or create a session.
    pub fn new(
        session_id: SessionId,
        config: Config,
        model: ModelInfo,
        store_path: Option<&Path>,
    ) -> Result<Self, EngineError> {
        config
            .validate()
            .map_err(|e| EngineError::Config(e.to_string()))?;
        let reserved = model
            .reserved_output_tokens
            .checked_add(config.reserve.safety_tokens)
            .ok_or_else(|| {
                EngineError::InvalidModelBudget(
                    "reserved output and safety tokens overflowed".to_string(),
                )
            })?;
        if model.context_window == 0 || reserved >= model.context_window {
            return Err(EngineError::InvalidModelBudget(format!(
                "window={} must exceed reserved_output={} + safety={}",
                model.context_window, model.reserved_output_tokens, config.reserve.safety_tokens
            )));
        }
        let store = match store_path {
            Some(p) => Store::open(p)?,
            None => Store::open_in_memory()?,
        };
        store.ensure_session(&session_id, &config)?;

        let mut stats: HashMap<String, TokenStat> = HashMap::new();
        for stat in store.load_token_stats(&session_id)? {
            stats.insert(stat.event_type.clone(), stat);
        }

        Ok(Self {
            session_id,
            config,
            model,
            store,
            token_counter: Box::new(ApproximateCounter),
            reducers: default_reducers(),
            planner: DefaultPlanner::default(),
            stats,
            last_plan: None,
        })
    }

    /// Return the session id.
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    /// Ingest a new context item.
    pub fn ingest(
        &mut self,
        mut item: ContextItem,
        source: ContextSource,
    ) -> Result<ContextId, EngineError> {
        if item.token_count == 0 {
            item.token_count = self.token_counter.count(&item.content);
        }
        if item.token_count > i64::MAX as u64 {
            return Err(EngineError::InvalidTokenCount(
                "token_count exceeds SQLite's signed integer range".to_string(),
            ));
        }
        let computed_hash = contextgc_core::hash_content(&item.content);
        if let Some(supplied_hash) = &item.content_hash {
            if supplied_hash != &computed_hash {
                return Err(EngineError::InvalidContentHash);
            }
        }
        item.content_hash = Some(computed_hash);
        item.source = source;
        let id = item.id.clone();
        self.store.insert_item(&self.session_id, &item)?;
        self.store
            .append_event(&self.session_id, "context.add", &item)?;

        if self.config.prediction.enabled {
            self.update_prediction_stat(&item)?;
        }

        // If a compacted projection already exists, append the new item to
        // that working set rather than immediately resurrecting the full
        // transcript. The immutable history row above remains unchanged.
        if let Some(mut working_set) = self.store.load_working_set(&self.session_id)? {
            working_set.push(MaterializedContextItem {
                id: item.id.clone(),
                parent_id: item.parent_id.clone(),
                kind: item.kind,
                content: item.content.clone(),
                token_count: item.token_count,
                compression_level: CompressionLevel::L0,
                artifact_ref: item.metadata.artifact_ref.clone(),
            });
            self.store
                .replace_working_set(&self.session_id, &working_set)?;
        }

        self.last_plan = None;

        Ok(id)
    }

    /// Ingest a structured checkpoint as a compact context object.
    pub fn ingest_checkpoint(
        &mut self,
        checkpoint: &TaskCheckpoint,
    ) -> Result<ContextId, EngineError> {
        let content = serde_json::to_string_pretty(checkpoint)?;
        self.ingest(
            ContextItem::new(ContextKind::Checkpoint, content),
            ContextSource::Compaction,
        )
    }

    /// Plan a compaction without applying it.
    pub fn plan(
        &mut self,
        prediction: Option<TokenPrediction>,
    ) -> Result<CompactionPlan, EngineError> {
        let items = self.active_context_items()?;
        let budget = self.current_budget(&items);
        let prediction = prediction.unwrap_or_else(|| self.predicted_extra_tokens());
        let plan = self
            .planner
            .plan(&items, &budget, &self.config, &prediction)?;
        self.last_plan = Some(plan.clone());
        Ok(plan)
    }

    /// Apply a plan and return the materialized working set.
    pub fn materialize(&mut self, plan: &CompactionPlan) -> Result<WorkingSet, EngineError> {
        let mut items = Vec::new();
        let mut total = 0u64;
        // Plan actions refer to the current working projection. The original
        // transcript is intentionally not used for materialization.
        let active_items = self.active_context_items()?;
        let by_id: HashMap<ContextId, ContextItem> = active_items
            .into_iter()
            .map(|i| (i.id.clone(), i))
            .collect();

        for action in &plan.actions {
            if action.to_level == CompressionLevel::L5 {
                continue;
            }
            let item = by_id
                .get(&action.context_id)
                .ok_or_else(|| EngineError::MissingItem(action.context_id.as_str().to_string()))?;

            let (content, level, artifact_ref) = match action.action {
                ContextAction::Keep | ContextAction::Pin => (
                    item.content.clone(),
                    item.compression_level,
                    item.metadata.artifact_ref.clone(),
                ),
                ContextAction::Reduce => {
                    let reduced = self.apply_reducer(item, |r, it| r.reduce(it));
                    (reduced, CompressionLevel::L1, None)
                }
                ContextAction::Extract => {
                    let extracted = self.apply_reducer(item, |r, it| r.extract(it));
                    (extracted, CompressionLevel::L2, None)
                }
                ContextAction::Summarize => {
                    // Semantic summarization is disabled in V1.
                    let summary = format!("[semantic summary disabled: {}]", item.id.as_str());
                    (summary, CompressionLevel::L3, None)
                }
                ContextAction::Externalize => {
                    let ref_str = self.externalize(item)?;
                    (ref_str.clone(), CompressionLevel::L4, Some(ref_str))
                }
                ContextAction::Deduplicate | ContextAction::Evict => continue,
            };

            // Preserve the source accounting for verbatim items. Reduced and
            // derived representations are recounted from their materialized
            // text so the returned working-set total matches its payload.
            let tokens = if level == CompressionLevel::L0 {
                item.token_count.max(1)
            } else {
                self.token_counter.count(&content).max(1)
            };
            total += tokens;
            items.push(MaterializedContextItem {
                id: item.id.clone(),
                parent_id: item.parent_id.clone(),
                kind: item.kind,
                content,
                token_count: tokens,
                compression_level: level,
                artifact_ref,
            });
        }

        let budget = self.budget_for_tokens(total);
        let working_set = WorkingSet {
            items,
            token_count: total,
            budget,
        };
        self.store
            .replace_working_set(&self.session_id, &working_set.items)?;
        self.last_plan = None;
        Ok(working_set)
    }

    /// Plan, record, and materialize a compaction.
    pub fn compact(
        &mut self,
        prediction: Option<TokenPrediction>,
    ) -> Result<WorkingSet, EngineError> {
        let plan = self.plan(prediction)?;
        self.store.record_compaction(&self.session_id, &plan)?;
        self.materialize(&plan)
    }

    /// Current session status, including top reclaim candidates.
    pub fn status(&mut self) -> Result<SessionStatus, EngineError> {
        let items = self.active_context_items()?;
        let budget = self.current_budget(&items);
        let plan = self.last_plan.clone().unwrap_or_else(|| {
            self.plan(None).unwrap_or_else(|_| CompactionPlan {
                before_tokens: budget.current_tokens,
                current_tokens: budget.current_tokens,
                pressure_before: budget.pressure(),
                predicted_pressure: budget.pressure(),
                target_tokens: 0,
                expected_tokens_after: budget.current_tokens,
                pressure_state: self.config.pressure_state(budget.pressure()),
                actions: Vec::new(),
            })
        });
        let predicted_pressure = plan.predicted_pressure;
        let pressure_state = self.config.pressure_state(predicted_pressure);

        let mut composition: HashMap<String, u64> = HashMap::new();
        for item in &items {
            *composition.entry(format!("{:?}", item.kind)).or_insert(0) += item.token_count;
        }
        let mut composition: Vec<(String, u64)> = composition.into_iter().collect();
        composition.sort_by_key(|entry| std::cmp::Reverse(entry.1));

        let mut candidates: Vec<CandidateStatus> = plan
            .actions
            .iter()
            .filter(|a| a.action != ContextAction::Keep && a.action != ContextAction::Pin)
            .map(|a| CandidateStatus {
                context_id: a.context_id.clone(),
                kind: format!("{:?}", a.from_level),
                tokens_before: a.estimated_tokens_before,
                tokens_after: a.estimated_tokens_after,
                action: a.action,
                importance: a.importance.total,
                reason: a.reason.clone(),
            })
            .collect();
        candidates.sort_by(|a, b| {
            let sa = a.tokens_before.saturating_sub(a.tokens_after);
            let sb = b.tokens_before.saturating_sub(b.tokens_after);
            sb.cmp(&sa)
        });
        candidates.truncate(5);

        Ok(SessionStatus {
            session_id: self.session_id.as_str().to_string(),
            model_name: self.model.name.clone(),
            context_window: self.model.context_window,
            current_tokens: budget.current_tokens,
            usable_context: budget.usable_context(),
            pressure: budget.pressure(),
            predicted_pressure,
            pressure_state,
            item_count: items.len(),
            composition,
            top_candidates: candidates,
        })
    }

    /// Predict how many tokens the next operation may consume.
    pub fn predicted_extra_tokens(&self) -> TokenPrediction {
        if !self.config.prediction.enabled || self.stats.is_empty() {
            return TokenPrediction::default();
        }
        let input = self.avg_ema();
        let tool = self.max_tool_ema();
        let assistant = self
            .stats
            .get("assistant")
            .map(|s| s.ema)
            .unwrap_or(input * 0.5);
        TokenPrediction {
            expected_next_input: input as u64,
            expected_next_tool_output: tool as u64,
            expected_next_assistant_output: assistant as u64,
            confidence: 0.5,
        }
    }

    /// Return the model info used by the session.
    pub fn model(&self) -> &ModelInfo {
        &self.model
    }

    fn current_budget(&self, items: &[ContextItem]) -> ContextBudget {
        let current: u64 = items.iter().map(|i| i.token_count).sum();
        self.budget_for_tokens(current)
    }

    fn budget_for_tokens(&self, current: u64) -> ContextBudget {
        ContextBudget {
            context_window: self.model.context_window,
            current_tokens: current,
            system_tokens: 0,
            tool_schema_tokens: 0,
            reserved_output_tokens: self.model.reserved_output_tokens,
            safety_tokens: self.config.reserve.safety_tokens,
        }
    }

    fn active_context_items(&self) -> Result<Vec<ContextItem>, EngineError> {
        let Some(working_set) = self.store.load_working_set(&self.session_id)? else {
            return Ok(self.store.active_items(&self.session_id)?);
        };

        let mut current = Vec::with_capacity(working_set.len());
        for projected in working_set {
            let mut original = self
                .store
                .get_item(&projected.id)?
                .ok_or_else(|| EngineError::MissingItem(projected.id.as_str().to_string()))?;
            original.parent_id = projected.parent_id;
            original.content = projected.content.clone();
            original.content_hash = Some(contextgc_core::hash_content(&original.content));
            original.token_count = projected.token_count;
            original.compression_level = projected.compression_level;
            if projected.artifact_ref.is_some() {
                original.metadata.artifact_ref = projected.artifact_ref;
                original.metadata.recoverable = true;
            }
            current.push(original);
        }
        Ok(current)
    }

    fn update_prediction_stat(&mut self, item: &ContextItem) -> Result<(), EngineError> {
        let event_type = event_type_for(item);
        let value = item.token_count as f32;
        let alpha = self.config.prediction.ema_alpha.clamp(0.0, 1.0);
        let stat = self.stats.remove(&event_type).unwrap_or(TokenStat {
            event_type: event_type.clone(),
            ema: value,
            count: 0,
        });
        let new_ema = update_ema(stat.ema, value, alpha);
        let new_stat = TokenStat {
            event_type: event_type.clone(),
            ema: new_ema,
            count: stat.count.saturating_add(1),
        };
        self.store.upsert_token_stat(
            &self.session_id,
            &event_type,
            new_stat.ema,
            new_stat.count,
        )?;
        self.stats.insert(event_type, new_stat);
        Ok(())
    }

    fn avg_ema(&self) -> f32 {
        if self.stats.is_empty() {
            return 0.0;
        }
        let sum: f32 = self.stats.values().map(|s| s.ema).sum();
        sum / self.stats.len() as f32
    }

    fn max_tool_ema(&self) -> f32 {
        let tool_keys = ["tool", "command", "file-read", "test"];
        self.stats
            .iter()
            .filter(|(k, _)| tool_keys.iter().any(|tk| k.contains(tk)))
            .map(|(_, s)| s.ema)
            .fold(0.0f32, |a, b| a.max(b))
    }

    fn apply_reducer<F>(&self, item: &ContextItem, f: F) -> String
    where
        F: FnOnce(&dyn Reducer, &ContextItem) -> String,
    {
        for reducer in &self.reducers {
            if reducer.can_reduce(item) {
                return f(reducer.as_ref(), item);
            }
        }
        item.content.clone()
    }

    fn externalize(&mut self, item: &ContextItem) -> Result<String, EngineError> {
        let ref_str = format!("artifact://{}/{}", kind_slug(item.kind), item.id.as_str());
        self.store
            .insert_artifact(&ref_str, &item.id, &item.content)?;
        Ok(ref_str)
    }
}

fn kind_slug(kind: ContextKind) -> &'static str {
    match kind {
        ContextKind::SystemPrompt => "system",
        ContextKind::DeveloperPrompt => "developer",
        ContextKind::UserMessage => "user",
        ContextKind::AssistantMessage => "assistant",
        ContextKind::ToolCall => "tool-call",
        ContextKind::ToolResult => "tool-result",
        ContextKind::FileContent => "file",
        ContextKind::CommandOutput => "command-output",
        ContextKind::Error => "error",
        ContextKind::Decision => "decision",
        ContextKind::Constraint => "constraint",
        ContextKind::Checkpoint => "checkpoint",
        ContextKind::Diff => "diff",
        ContextKind::TestResult => "test-result",
        ContextKind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contextgc_core::{ContextItem, ContextKind, ContextMetadata, ContextState};

    fn test_session() -> Session {
        let mut cfg = Config::default();
        cfg.context.max_context_window = 128_000;
        cfg.reserve.output_tokens = 8_000;
        cfg.reserve.safety_tokens = 8_000;
        cfg.context.target_pressure = 0.55;
        Session::new(
            SessionId::new("test-sess"),
            cfg,
            ModelInfo {
                name: "generic-128k".to_string(),
                context_window: 128_000,
                reserved_output_tokens: 8_000,
            },
            None,
        )
        .unwrap()
    }

    fn large_log() -> String {
        (0..500)
            .map(|i| format!("\x1b[32mBuilding\x1b[0m component {i} [{i}%]\n"))
            .chain(std::iter::once("Build succeeded".to_string()))
            .collect()
    }

    #[test]
    fn ingest_counts_tokens() {
        let mut sess = test_session();
        let id = sess
            .ingest(
                ContextItem::new(ContextKind::UserMessage, "hello world"),
                ContextSource::Ingest,
            )
            .unwrap();
        let item = sess.store.get_item(&id).unwrap().unwrap();
        assert_eq!(item.content, "hello world");
        assert!(item.token_count > 0);
    }

    #[test]
    fn pinned_constraint_survives_compaction() {
        let mut sess = test_session();
        // Fill session with recoverable noise.
        for _ in 0..50 {
            sess.ingest(
                ContextItem::new(ContextKind::CommandOutput, large_log()).with_metadata(
                    ContextMetadata {
                        recoverable: true,
                        ..Default::default()
                    },
                ),
                ContextSource::Ingest,
            )
            .unwrap();
        }
        // Add a pinned constraint.
        let constraint_id = sess
            .ingest(
                ContextItem::new(
                    ContextKind::Constraint,
                    "Do not modify the database schema.",
                )
                .with_metadata(ContextMetadata {
                    pinned: true,
                    ..Default::default()
                }),
                ContextSource::Ingest,
            )
            .unwrap();

        let ws = sess.compact(None).unwrap();
        let ids: Vec<_> = ws.items.iter().map(|i| &i.id).collect();
        assert!(ids.contains(&&constraint_id));
        let constraint_item = ws.items.iter().find(|i| i.id == constraint_id).unwrap();
        assert_eq!(
            constraint_item.content,
            "Do not modify the database schema."
        );
    }

    #[test]
    fn unresolved_error_survives_compaction() {
        let mut sess = test_session();
        for _ in 0..50 {
            sess.ingest(
                ContextItem::new(ContextKind::CommandOutput, large_log()).with_metadata(
                    ContextMetadata {
                        recoverable: true,
                        ..Default::default()
                    },
                ),
                ContextSource::Ingest,
            )
            .unwrap();
        }
        let error_id = sess
            .ingest(
                ContextItem::new(
                    ContextKind::Error,
                    "error[E0308]: mismatched types at src/lib.rs:42",
                )
                .with_metadata(ContextMetadata {
                    exit_code: Some(1),
                    ..Default::default()
                })
                .with_state(ContextState::Active),
                ContextSource::Ingest,
            )
            .unwrap();
        let ws = sess.compact(None).unwrap();
        assert!(ws.items.iter().any(|i| i.id == error_id));
    }

    #[test]
    fn deduplicates_repeated_file_reads() {
        let mut sess = test_session();
        let content = "fn main() {}\n";
        let mut ids = Vec::new();
        for _ in 0..10 {
            let id = sess
                .ingest(
                    ContextItem::new(ContextKind::FileContent, content).with_metadata(
                        ContextMetadata {
                            file_path: Some(std::path::PathBuf::from("src/main.rs")),
                            recoverable: true,
                            ..Default::default()
                        },
                    ),
                    ContextSource::Ingest,
                )
                .unwrap();
            ids.push(id);
        }
        let ws = sess.compact(None).unwrap();
        let file_items: Vec<_> = ws
            .items
            .iter()
            .filter(|i| matches!(i.kind, ContextKind::FileContent))
            .collect();
        assert!(file_items.len() <= 1, "expected at most one canonical file");
    }

    #[test]
    fn large_command_output_is_reduced() {
        // Small window so a single large build log creates real pressure.
        let mut cfg = Config::default();
        cfg.context.max_context_window = 6_000;
        cfg.reserve.output_tokens = 300;
        cfg.reserve.safety_tokens = 300;
        cfg.context.target_pressure = 0.55;
        let mut sess = Session::new(
            SessionId::new("test-small"),
            cfg,
            ModelInfo {
                name: "generic-6k".to_string(),
                context_window: 6_000,
                reserved_output_tokens: 300,
            },
            None,
        )
        .unwrap();
        let id = sess
            .ingest(
                ContextItem::new(ContextKind::CommandOutput, large_log()).with_metadata(
                    ContextMetadata {
                        command: Some("cargo build".to_string()),
                        exit_code: Some(0),
                        recoverable: true,
                        ..Default::default()
                    },
                ),
                ContextSource::Ingest,
            )
            .unwrap();
        let before = sess.store.get_item(&id).unwrap().unwrap().token_count;
        assert!(before > 0);
        let ws = sess.compact(None).unwrap();
        match ws.items.iter().find(|i| i.id == id) {
            Some(item) => assert!(
                item.token_count < before,
                "expected reduction: {} -> {}",
                before,
                item.token_count
            ),
            None => panic!("expected the command output to remain in the working set"),
        }
    }

    #[test]
    fn prediction_triggers_compaction() {
        let mut sess = test_session();
        // Fill to ~65% pressure.
        for _ in 0..40 {
            sess.ingest(
                ContextItem::new(ContextKind::CommandOutput, large_log()).with_metadata(
                    ContextMetadata {
                        recoverable: true,
                        ..Default::default()
                    },
                ),
                ContextSource::Ingest,
            )
            .unwrap();
        }
        let status = sess.status().unwrap();
        assert!(status.pressure > 0.60);

        // Predict a 30k-token tool call.
        let prediction = TokenPrediction {
            expected_next_input: 0,
            expected_next_tool_output: 30_000,
            expected_next_assistant_output: 0,
            confidence: 0.5,
        };
        let plan = sess.plan(Some(prediction)).unwrap();
        assert!(plan.predicted_pressure > plan.pressure_before);
    }

    #[test]
    fn reopening_preserves_pinned_metadata_and_working_projection() {
        let path =
            std::env::temp_dir().join(format!("contextgc-{}.db", SessionId::default().as_str()));
        let model = ModelInfo {
            name: "generic-120k".to_string(),
            context_window: 120_000,
            reserved_output_tokens: 10_000,
        };
        let session_id = SessionId::new("reopen-test");
        let pinned_id;
        {
            let mut sess = Session::new(
                session_id.clone(),
                Config::default(),
                model.clone(),
                Some(&path),
            )
            .unwrap();
            pinned_id = sess
                .ingest(
                    ContextItem::new(ContextKind::UserMessage, "must remain authoritative")
                        .with_tokens(20_000)
                        .with_metadata(ContextMetadata {
                            pinned: true,
                            ..Default::default()
                        }),
                    ContextSource::Ingest,
                )
                .unwrap();
            sess.ingest(
                ContextItem::new(ContextKind::FileContent, "recoverable source")
                    .with_tokens(50_000)
                    .with_metadata(ContextMetadata {
                        recoverable: true,
                        file_path: Some(std::path::PathBuf::from("src/lib.rs")),
                        ..Default::default()
                    }),
                ContextSource::Ingest,
            )
            .unwrap();
            let working = sess.compact(None).unwrap();
            assert!(working.token_count < 70_000);
        }

        let mut reopened = Session::new(session_id, Config::default(), model, Some(&path)).unwrap();
        let status = reopened.status().unwrap();
        assert!(status.current_tokens < 70_000);
        let plan = reopened.plan(None).unwrap();
        let pinned = plan
            .actions
            .iter()
            .find(|action| action.context_id == pinned_id)
            .unwrap();
        assert_eq!(pinned.action, ContextAction::Pin);
        assert_eq!(pinned.to_level, CompressionLevel::L0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_invalid_model_budget() {
        let result = Session::new(
            SessionId::new("invalid-budget"),
            Config::default(),
            ModelInfo {
                name: "invalid".to_string(),
                context_window: 1,
                reserved_output_tokens: 1,
            },
            None,
        );
        assert!(matches!(result, Err(EngineError::InvalidModelBudget(_))));
    }
}
