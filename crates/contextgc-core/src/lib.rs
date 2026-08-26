use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub type Timestamp = DateTime<Utc>;

/// Stable identifier for a context item.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextId(pub String);

impl ContextId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for ContextId {
    fn default() -> Self {
        Self(uuid())
    }
}

fn uuid() -> String {
    format!("cgc-{}", uuid::Uuid::new_v4())
}

/// Session identifier.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self(uuid())
    }
}

/// What kind of thing is occupying context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ContextKind {
    SystemPrompt,
    DeveloperPrompt,
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    FileContent,
    CommandOutput,
    Error,
    Decision,
    Constraint,
    Checkpoint,
    Diff,
    TestResult,
    Other,
}

/// Lifecycle state of a context item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ContextState {
    #[default]
    Active,
    Resolved,
    Superseded,
    Abandoned,
    Unknown,
}

/// Where an item came from.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSource {
    #[default]
    Ingest,
    Adapter(String),
    System,
    Compaction,
}

/// Per-item metadata that the core can use for scoring and recovery.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContextMetadata {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
    #[serde(default)]
    pub recoverable: bool,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A single occupant of context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    #[serde(default)]
    pub id: ContextId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<ContextId>,
    pub kind: ContextKind,
    pub content: String,
    #[serde(default)]
    pub token_count: u64,
    #[serde(default = "default_timestamp")]
    pub created_at: Timestamp,
    #[serde(default)]
    pub source: ContextSource,
    #[serde(default)]
    pub metadata: ContextMetadata,
    #[serde(default)]
    pub state: ContextState,
    #[serde(default)]
    pub compression_level: CompressionLevel,
    /// Hash of `content`, stable for deduplication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

fn default_timestamp() -> Timestamp {
    Utc::now()
}

impl ContextItem {
    pub fn new(kind: ContextKind, content: impl Into<String>) -> Self {
        let content = content.into();
        let hash = hash_content(&content);
        Self {
            id: ContextId::default(),
            parent_id: None,
            kind,
            content,
            token_count: 0,
            created_at: Utc::now(),
            source: ContextSource::default(),
            metadata: ContextMetadata::default(),
            state: ContextState::Active,
            compression_level: CompressionLevel::L0,
            content_hash: Some(hash),
        }
    }

    pub fn with_tokens(mut self, tokens: u64) -> Self {
        self.token_count = tokens;
        self
    }

    pub fn with_metadata(mut self, metadata: ContextMetadata) -> Self {
        self.metadata = metadata;
        self
    }

    pub fn with_parent(mut self, parent_id: ContextId) -> Self {
        self.parent_id = Some(parent_id);
        self
    }

    pub fn with_state(mut self, state: ContextState) -> Self {
        self.state = state;
        self
    }

    pub fn with_source(mut self, source: ContextSource) -> Self {
        self.source = source;
        self
    }
}

/// Compute a stable SHA-256 hash of text.
pub fn hash_content(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

/// Action the planner assigns to an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum ContextAction {
    Keep,
    Deduplicate,
    Reduce,
    Extract,
    Summarize,
    Externalize,
    Evict,
    Pin,
}

/// Compaction continuum.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CompressionLevel {
    #[default]
    L0, // Verbatim
    L1, // Structural reduction
    L2, // Extractive reduction
    L3, // Semantic summary
    L4, // External reference
    L5, // Excluded from active context
}

impl CompressionLevel {
    pub fn is_active(self) -> bool {
        !matches!(self, Self::L5)
    }

    pub fn ordinal(self) -> u8 {
        match self {
            Self::L0 => 0,
            Self::L1 => 1,
            Self::L2 => 2,
            Self::L3 => 3,
            Self::L4 => 4,
            Self::L5 => 5,
        }
    }
}
/// One proposed action in a compaction plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlannedAction {
    pub context_id: ContextId,
    pub action: ContextAction,
    pub from_level: CompressionLevel,
    pub to_level: CompressionLevel,
    pub estimated_tokens_before: u64,
    pub estimated_tokens_after: u64,
    pub importance: ImportanceScore,
    pub reason: String,
}

/// Inspectable compaction plan returned before any working set is changed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactionPlan {
    pub before_tokens: u64,
    pub pressure_before: f32,
    pub current_tokens: u64,
    pub predicted_pressure: f32,
    pub target_tokens: u64,
    pub expected_tokens_after: u64,
    pub pressure_state: PressureState,
    pub actions: Vec<PlannedAction>,
}

/// Materialized item in the active working set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaterializedContextItem {
    pub id: ContextId,
    pub parent_id: Option<ContextId>,
    pub kind: ContextKind,
    pub content: String,
    pub token_count: u64,
    pub compression_level: CompressionLevel,
    pub artifact_ref: Option<String>,
}

/// The output of ContextGC: a budgeted working set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkingSet {
    pub items: Vec<MaterializedContextItem>,
    pub token_count: u64,
    pub budget: ContextBudget,
}

/// A structured decision captured in a task checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rationale: Option<String>,
}

/// A file changed while completing a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    pub path: PathBuf,
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
}

/// A command executed during a task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandSummary {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub summary: String,
}

/// A validation result captured in a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub name: String,
    pub passed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Compact structured state for a resolved or paused subtask.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCheckpoint {
    pub goal: String,
    pub status: String,
    pub summary: String,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub modified_files: Vec<FileChange>,
    #[serde(default)]
    pub commands: Vec<CommandSummary>,
    #[serde(default)]
    pub validation: Vec<ValidationResult>,
    #[serde(default)]
    pub unresolved: Vec<String>,
    #[serde(default)]
    pub artifact_refs: Vec<String>,
}

/// Model / tokenizer metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub name: String,
    pub context_window: u64,
    pub reserved_output_tokens: u64,
}

/// Token budget for the current model context.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ContextBudget {
    pub context_window: u64,
    pub current_tokens: u64,
    pub system_tokens: u64,
    pub tool_schema_tokens: u64,
    pub reserved_output_tokens: u64,
    pub safety_tokens: u64,
}

impl ContextBudget {
    pub fn usable_context(&self) -> u64 {
        self.context_window
            .saturating_sub(self.reserved_output_tokens)
            .saturating_sub(self.safety_tokens)
    }

    pub fn pressure(&self) -> f32 {
        let usable = self.usable_context();
        if usable == 0 {
            return f32::INFINITY;
        }
        self.current_tokens as f32 / usable as f32
    }

    pub fn projected_pressure(&self, predicted_extra_tokens: u64) -> f32 {
        let usable = self.usable_context();
        if usable == 0 {
            return f32::INFINITY;
        }
        (self.current_tokens.saturating_add(predicted_extra_tokens)) as f32 / usable as f32
    }
}

/// Discrete pressure state derived from configured thresholds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub enum PressureState {
    Green,
    Observe,
    Trim,
    Compact,
    Aggressive,
    Emergency,
}

impl PressureState {
    pub fn from_pressure(pressure: f32, thresholds: &PressureThresholds) -> Self {
        if pressure >= thresholds.emergency {
            Self::Emergency
        } else if pressure >= thresholds.aggressive {
            Self::Aggressive
        } else if pressure >= thresholds.compact {
            Self::Compact
        } else if pressure >= thresholds.trim {
            Self::Trim
        } else if pressure >= thresholds.observe {
            Self::Observe
        } else {
            Self::Green
        }
    }

    pub fn needs_compaction(self) -> bool {
        matches!(self, Self::Compact | Self::Aggressive | Self::Emergency)
    }

    pub fn is_emergency(self) -> bool {
        matches!(self, Self::Emergency)
    }
}

/// Configurable pressure thresholds.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PressureThresholds {
    #[serde(alias = "observe_pressure", default = "default_observe")]
    pub observe: f32,
    #[serde(alias = "trim_pressure", default = "default_trim")]
    pub trim: f32,
    #[serde(alias = "compact_pressure", default = "default_compact")]
    pub compact: f32,
    #[serde(alias = "aggressive_pressure", default = "default_aggressive")]
    pub aggressive: f32,
    #[serde(alias = "emergency_pressure", default = "default_emergency")]
    pub emergency: f32,
}

impl Default for PressureThresholds {
    fn default() -> Self {
        Self {
            observe: 0.45,
            trim: 0.60,
            compact: 0.72,
            aggressive: 0.82,
            emergency: 0.90,
        }
    }
}

fn default_observe() -> f32 {
    0.45
}

fn default_trim() -> f32 {
    0.60
}

fn default_compact() -> f32 {
    0.72
}

fn default_aggressive() -> f32 {
    0.82
}

fn default_emergency() -> f32 {
    0.90
}

impl PressureThresholds {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut prev = 0.0f32;
        for (name, value) in [
            ("observe", self.observe),
            ("trim", self.trim),
            ("compact", self.compact),
            ("aggressive", self.aggressive),
            ("emergency", self.emergency),
        ] {
            if !(0.0..=1.0).contains(&value) {
                return Err(ConfigError::InvalidThreshold {
                    name: name.to_string(),
                    value,
                });
            }
            if value < prev {
                return Err(ConfigError::NonMonotonicThreshold {
                    name: name.to_string(),
                    value,
                    prev,
                });
            }
            prev = value;
        }
        Ok(())
    }
}

/// A prediction of how much context the next operation may consume.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TokenPrediction {
    pub expected_next_input: u64,
    pub expected_next_tool_output: u64,
    pub expected_next_assistant_output: u64,
    pub confidence: f32,
}

impl Default for TokenPrediction {
    fn default() -> Self {
        Self {
            expected_next_input: 0,
            expected_next_tool_output: 0,
            expected_next_assistant_output: 0,
            confidence: 0.0,
        }
    }
}

/// Explainable importance score.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImportanceScore {
    pub total: f32,
    pub relevance: f32,
    pub recency: f32,
    pub dependency: f32,
    pub unresolved: f32,
    pub authority: f32,
    pub uniqueness: f32,
    pub recoverability_penalty: f32,
    pub redundancy_penalty: f32,
}

impl ImportanceScore {
    pub fn weighted(weights: &ImportanceWeights, components: &ImportanceComponents) -> Self {
        let total = weights.relevance * components.relevance
            + weights.recency * components.recency
            + weights.dependency * components.dependency
            + weights.unresolved * components.unresolved
            + weights.authority * components.authority
            + weights.uniqueness * components.uniqueness
            - weights.recoverability * components.recoverability
            - weights.redundancy * components.redundancy;
        Self {
            total: total.clamp(0.0, 1.0),
            relevance: components.relevance,
            recency: components.recency,
            dependency: components.dependency,
            unresolved: components.unresolved,
            authority: components.authority,
            uniqueness: components.uniqueness,
            recoverability_penalty: components.recoverability,
            redundancy_penalty: components.redundancy,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImportanceWeights {
    pub relevance: f32,
    pub recency: f32,
    pub dependency: f32,
    pub unresolved: f32,
    pub authority: f32,
    pub uniqueness: f32,
    pub recoverability: f32,
    pub redundancy: f32,
}

impl Default for ImportanceWeights {
    fn default() -> Self {
        Self {
            relevance: 0.15,
            recency: 0.15,
            dependency: 0.10,
            unresolved: 0.20,
            authority: 0.15,
            uniqueness: 0.10,
            recoverability: 0.10,
            redundancy: 0.15,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ImportanceComponents {
    pub relevance: f32,
    pub recency: f32,
    pub dependency: f32,
    pub unresolved: f32,
    pub authority: f32,
    pub uniqueness: f32,
    pub recoverability: f32,
    pub redundancy: f32,
}

/// High-level configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub context: ContextConfig,
    #[serde(default)]
    pub reserve: ReserveConfig,
    #[serde(default)]
    pub preservation: PreservationConfig,
    #[serde(default)]
    pub tools: ToolConfig,
    #[serde(default)]
    pub prediction: PredictionConfig,
    #[serde(default)]
    pub semantic: SemanticConfig,
    #[serde(default)]
    pub memory: MemoryConfig,
}

impl Config {
    /// Parse a partial TOML configuration over the built-in defaults.
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Self =
            toml::from_str(text).map_err(|error| ConfigError::Toml(error.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        self.context.thresholds.validate()?;
        if !self.context.target_pressure.is_finite()
            || !(0.0..=1.0).contains(&self.context.target_pressure)
        {
            return Err(ConfigError::InvalidValue {
                name: "context.target_pressure".to_string(),
                value: self.context.target_pressure,
            });
        }
        if !self.prediction.ema_alpha.is_finite()
            || !(0.0..=1.0).contains(&self.prediction.ema_alpha)
        {
            return Err(ConfigError::InvalidValue {
                name: "prediction.ema_alpha".to_string(),
                value: self.prediction.ema_alpha,
            });
        }
        if self
            .reserve
            .output_tokens
            .saturating_add(self.reserve.safety_tokens)
            >= self.context.max_context_window
        {
            return Err(ConfigError::ReserveExceedsWindow);
        }
        if self.context.max_context_window == 0 {
            return Err(ConfigError::ZeroWindow);
        }
        Ok(())
    }

    pub fn pressure_state(&self, pressure: f32) -> PressureState {
        PressureState::from_pressure(pressure, &self.context.thresholds)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    pub max_context_window: u64,
    pub target_pressure: f32,
    #[serde(flatten)]
    #[serde(default)]
    pub thresholds: PressureThresholds,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_context_window: 200_000,
            target_pressure: 0.55,
            thresholds: PressureThresholds::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ReserveConfig {
    pub output_tokens: u64,
    pub safety_tokens: u64,
}

impl Default for ReserveConfig {
    fn default() -> Self {
        Self {
            output_tokens: 16_000,
            safety_tokens: 12_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct PreservationConfig {
    pub recent_tokens: u64,
    pub pin_user_constraints: bool,
    pub pin_unresolved_errors: bool,
}

impl Default for PreservationConfig {
    fn default() -> Self {
        Self {
            recent_tokens: 24_000,
            pin_user_constraints: true,
            pin_unresolved_errors: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolConfig {
    pub deduplicate: bool,
    pub reduce_logs: bool,
    pub externalize_large_results: bool,
    pub large_result_tokens: u64,
}

impl Default for ToolConfig {
    fn default() -> Self {
        Self {
            deduplicate: true,
            reduce_logs: true,
            externalize_large_results: true,
            large_result_tokens: 6_000,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct PredictionConfig {
    pub enabled: bool,
    pub ema_alpha: f32,
}

impl Default for PredictionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ema_alpha: 0.25,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SemanticConfig {
    pub enabled: bool,
}

/// Optional long-term memory policy. The backend is intentionally represented
/// as configuration data in core; implementations live outside `core`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    pub backend: String,
    pub store_externalized: bool,
    pub store_checkpoints: bool,
    pub store_errors: bool,
    pub store_successful_fixes: bool,
    pub store_raw_logs: bool,
    pub memwhale: MemWhaleConfig,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            backend: "none".to_string(),
            store_externalized: true,
            store_checkpoints: true,
            store_errors: true,
            store_successful_fixes: true,
            store_raw_logs: false,
            memwhale: MemWhaleConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemWhaleConfig {
    pub transport: String,
    pub command: String,
}

impl Default for MemWhaleConfig {
    fn default() -> Self {
        Self {
            transport: "stdio".to_string(),
            command: "mw-mcp".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ConfigError {
    #[error("threshold `{name}` out of range: {value}")]
    InvalidThreshold { name: String, value: f32 },
    #[error("threshold `{name}` ({value}) is lower than previous threshold ({prev})")]
    NonMonotonicThreshold { name: String, value: f32, prev: f32 },
    #[error("configuration value `{name}` out of range: {value}")]
    InvalidValue { name: String, value: f32 },
    #[error("reserved tokens exceed or equal the context window")]
    ReserveExceedsWindow,
    #[error("context window must be greater than zero")]
    ZeroWindow,
    #[error("TOML configuration error: {0}")]
    Toml(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_budget_pressure() {
        let budget = ContextBudget {
            context_window: 200_000,
            current_tokens: 118_493,
            system_tokens: 9_220,
            tool_schema_tokens: 0,
            reserved_output_tokens: 16_000,
            safety_tokens: 12_000,
        };
        assert_eq!(budget.usable_context(), 172_000);
        let p = budget.pressure();
        assert!((p - 0.6889).abs() < 0.001, "pressure was {p}");
    }

    #[test]
    fn pressure_state_transitions() {
        let th = PressureThresholds::default();
        assert_eq!(
            PressureState::from_pressure(0.30, &th),
            PressureState::Green
        );
        assert_eq!(
            PressureState::from_pressure(0.50, &th),
            PressureState::Observe
        );
        assert_eq!(PressureState::from_pressure(0.65, &th), PressureState::Trim);
        assert_eq!(
            PressureState::from_pressure(0.75, &th),
            PressureState::Compact
        );
        assert_eq!(
            PressureState::from_pressure(0.85, &th),
            PressureState::Aggressive
        );
        assert_eq!(
            PressureState::from_pressure(0.95, &th),
            PressureState::Emergency
        );
    }

    #[test]
    fn config_validation_rejects_bad_thresholds() {
        let mut cfg = Config::default();
        cfg.context.thresholds.trim = 0.55;
        cfg.context.thresholds.observe = 0.60;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn config_validation_accepts_default() {
        let cfg = Config::default();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn partial_toml_config_uses_defaults_and_aliases() {
        let cfg = Config::from_toml(
            "[context]\ntarget_pressure = 0.60\ncompact_pressure = 0.75\n[reserve]\noutput_tokens = 12000\n",
        )
        .unwrap();
        assert_eq!(cfg.context.target_pressure, 0.60);
        assert_eq!(cfg.context.thresholds.compact, 0.75);
        assert_eq!(cfg.context.thresholds.emergency, 0.90);
        assert_eq!(cfg.reserve.output_tokens, 12_000);
        assert_eq!(cfg.reserve.safety_tokens, 12_000);
    }

    #[test]
    fn projected_pressure_calculation() {
        let budget = ContextBudget {
            context_window: 128_000,
            current_tokens: 112_000,
            system_tokens: 0,
            tool_schema_tokens: 0,
            reserved_output_tokens: 8_000,
            safety_tokens: 8_000,
        };
        assert_eq!(budget.usable_context(), 112_000);
        let predicted = budget.projected_pressure(38_000 + 16_000);
        assert!((predicted - (166_000.0 / 112_000.0)).abs() < 0.001);
    }

    #[test]
    fn content_hash_stable() {
        let a = hash_content("hello");
        let b = hash_content("hello");
        let c = hash_content("world");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
