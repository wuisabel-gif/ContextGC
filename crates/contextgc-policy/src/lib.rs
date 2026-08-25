//! Deterministic compaction policy for ContextGC.
//!
//! This crate holds all policy intelligence that is *not* an LLM:
//!
//! - content reducers for common tool outputs,
//! - exact-content deduplication,
//! - explainable importance scoring,
//! - recoverability scoring,
//! - pinned-context detection,
//! - the `CompactionPlanner` that chooses an action per item.

use contextgc_core::{
    CompactionPlan, CompressionLevel, Config, ContextAction, ContextBudget, ContextId, ContextItem,
    ContextKind, ContextState, ImportanceComponents, ImportanceScore, ImportanceWeights,
    PlannedAction, TokenPrediction,
};
use contextgc_tokenizer::TokenCounter;
use regex::Regex;
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("empty context window")]
    EmptyWindow,
    #[error("no token counter available")]
    MissingTokenizer,
}

// ---------------------------------------------------------------------------
// Reducers
// ---------------------------------------------------------------------------

/// A deterministic, lossy or lossless content reducer.
pub trait Reducer: Send + Sync {
    fn can_reduce(&self, item: &ContextItem) -> bool;
    /// Lossless-ish structural reduction.
    fn reduce(&self, item: &ContextItem) -> String;
    /// Extractive reduction: keep only the most important parts.
    fn extract(&self, item: &ContextItem) -> String;
}

pub struct ShellReducer;

impl Default for ShellReducer {
    fn default() -> Self {
        Self
    }
}

impl ShellReducer {
    fn ansi_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[A-Za-z]").unwrap())
    }

    fn spinner_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^[\s\|/\\\-]*$").unwrap())
    }

    fn progress_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| {
            Regex::new(r"(?i)\b\d+%|\[={0,50}>\s*\]|\b(eta|downloading|extracting)\b").unwrap()
        })
    }
}

impl Reducer for ShellReducer {
    fn can_reduce(&self, item: &ContextItem) -> bool {
        matches!(item.kind, ContextKind::CommandOutput)
    }

    fn reduce(&self, item: &ContextItem) -> String {
        let text = Self::ansi_regex().replace_all(&item.content, "");
        let lines: Vec<&str> = text.lines().collect();
        let mut out: Vec<&str> = Vec::with_capacity(lines.len());
        for line in lines {
            if Self::spinner_regex().is_match(line) {
                continue;
            }
            if Self::progress_regex().is_match(line) {
                continue;
            }
            if out.last() == Some(&line) {
                continue;
            }
            out.push(line);
        }
        out.join("\n")
    }

    fn extract(&self, item: &ContextItem) -> String {
        let reduced = self.reduce(item);
        let mut parts = Vec::new();
        if let Some(cmd) = &item.metadata.command {
            parts.push(format!("command: {cmd}"));
        }
        if let Some(code) = item.metadata.exit_code {
            parts.push(format!("exit_code: {code}"));
        }
        // Keep stderr-looking / error-ish lines and the final few lines.
        let error_like = Regex::new(r"(?i)error|fail|fatal|panic").unwrap();
        let mut errors: Vec<&str> = reduced.lines().filter(|l| error_like.is_match(l)).collect();
        // Avoid flooding with repeated errors.
        errors.dedup();
        parts.push("errors:".to_string());
        if errors.is_empty() {
            parts.push("  <none>".to_string());
        } else {
            for e in errors.iter().take(20) {
                parts.push(format!("  {e}"));
            }
        }
        // Add the tail as a final status.
        let tail: Vec<&str> = reduced.lines().rev().take(3).collect::<Vec<_>>();
        if !tail.is_empty() {
            parts.push("final_status:".to_string());
            for line in tail.iter().rev() {
                parts.push(format!("  {line}"));
            }
        }
        parts.join("\n")
    }
}

pub struct TestReducer;

impl Default for TestReducer {
    fn default() -> Self {
        Self
    }
}

impl TestReducer {
    fn pass_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^test .* \.\.\. ok$").unwrap())
    }

    fn fail_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^test .* \.\.\. FAILED$").unwrap())
    }

    fn duration_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"test result: .*\.|ran for|time:.*s|duration").unwrap())
    }
}

impl Reducer for TestReducer {
    fn can_reduce(&self, item: &ContextItem) -> bool {
        if item.kind == ContextKind::TestResult {
            return true;
        }
        item.kind == ContextKind::CommandOutput
            && (item
                .content
                .lines()
                .any(|line| Self::pass_regex().is_match(line))
                || item
                    .content
                    .lines()
                    .any(|line| Self::fail_regex().is_match(line))
                || item.content.contains("test result:"))
    }

    fn reduce(&self, item: &ContextItem) -> String {
        let lines: Vec<&str> = item.content.lines().collect();
        let pass_count = lines
            .iter()
            .filter(|l| Self::pass_regex().is_match(l))
            .count();
        let fail_count = lines
            .iter()
            .filter(|l| Self::fail_regex().is_match(l))
            .count();
        if fail_count == 0 && pass_count > 0 {
            let duration_line = lines
                .iter()
                .rev()
                .find(|l| Self::duration_regex().is_match(l))
                .copied()
                .unwrap_or("");
            let duration_part = if duration_line.is_empty() {
                String::new()
            } else {
                format!("duration {duration_line}")
            };
            format!("{pass_count} tests passed\n0 failed\n{duration_part}")
                .trim_end()
                .to_string()
        } else {
            // Failure run: keep failing test names, errors, stack subset.
            let mut out = Vec::new();
            let fail_header = Regex::new(r"(?i)failures:").unwrap();
            for line in &lines {
                if Self::fail_regex().is_match(line)
                    || fail_header.is_match(line)
                    || line.starts_with("----")
                {
                    out.push(*line);
                }
                if out.len() > 200 {
                    break;
                }
            }
            out.join("\n")
        }
    }

    fn extract(&self, item: &ContextItem) -> String {
        // For test output, extraction is essentially the same as reduction.
        self.reduce(item)
    }
}

pub struct CompilerReducer;

impl Default for CompilerReducer {
    fn default() -> Self {
        Self
    }
}

impl CompilerReducer {
    fn error_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"(?i)^\s*error(\[.*\])?\s*:.*$").unwrap())
    }

    fn location_regex() -> &'static Regex {
        static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
        RE.get_or_init(|| Regex::new(r"^\s*-->\s*([^\s]+):(\d+):(\d+)").unwrap())
    }
}

impl Reducer for CompilerReducer {
    fn can_reduce(&self, item: &ContextItem) -> bool {
        matches!(item.kind, ContextKind::Error | ContextKind::CommandOutput)
            && item.content.contains("error[")
    }

    fn reduce(&self, item: &ContextItem) -> String {
        let lines: Vec<&str> = item.content.lines().collect();
        let mut out = Vec::new();
        let mut prev_error = "";
        for line in &lines {
            if (Self::error_regex().is_match(line) || Self::location_regex().is_match(line))
                && *line != prev_error
            {
                out.push(*line);
                prev_error = line;
            }
        }
        out.join("\n")
    }

    fn extract(&self, item: &ContextItem) -> String {
        self.reduce(item)
    }
}

pub struct FileReadReducer;

impl Default for FileReadReducer {
    fn default() -> Self {
        Self
    }
}

impl Reducer for FileReadReducer {
    fn can_reduce(&self, _item: &ContextItem) -> bool {
        false
    }

    fn reduce(&self, item: &ContextItem) -> String {
        item.content.clone()
    }

    fn extract(&self, item: &ContextItem) -> String {
        item.content.clone()
    }
}

pub fn default_reducers() -> Vec<Box<dyn Reducer>> {
    vec![
        Box::new(CompilerReducer),
        Box::new(TestReducer),
        Box::new(ShellReducer),
        Box::new(FileReadReducer),
    ]
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct DedupIndex {
    /// kind + content_hash -> (canonical_id, duplicate_count)
    groups: HashMap<String, DedupGroup>,
    item_to_hash: HashMap<ContextId, String>,
}

#[derive(Debug, Clone)]
struct DedupGroup {
    canonical_id: ContextId,
    count: usize,
}

impl DedupIndex {
    pub fn build(items: &[ContextItem]) -> Self {
        let mut groups: HashMap<String, DedupGroup> = HashMap::new();
        let mut item_to_hash = HashMap::with_capacity(items.len());
        // Use the *newest* item as canonical (last in the slice is assumed latest).
        for item in items {
            let hash = item
                .content_hash
                .clone()
                .unwrap_or_else(|| contextgc_core::hash_content(&item.content));
            let group_key = format!("{:?}:{hash}", item.kind);
            item_to_hash.insert(item.id.clone(), group_key.clone());
            groups
                .entry(group_key)
                .and_modify(|g| {
                    g.canonical_id = item.id.clone();
                    g.count += 1;
                })
                .or_insert(DedupGroup {
                    canonical_id: item.id.clone(),
                    count: 1,
                });
        }
        Self {
            groups,
            item_to_hash,
        }
    }

    pub fn duplicate_count(&self, id: &ContextId) -> u64 {
        self.item_to_hash
            .get(id)
            .and_then(|hash| self.groups.get(hash))
            .map(|group| group.count as u64)
            .unwrap_or(1)
    }

    pub fn is_canonical(&self, id: &ContextId) -> bool {
        self.item_to_hash
            .get(id)
            .and_then(|hash| self.groups.get(hash))
            .map(|group| group.count > 1 && group.canonical_id == *id)
            .unwrap_or(false)
    }

    pub fn is_duplicate(&self, id: &ContextId) -> bool {
        self.item_to_hash
            .get(id)
            .and_then(|hash| self.groups.get(hash))
            .map(|group| group.count > 1 && group.canonical_id != *id)
            .unwrap_or(false)
    }
}

fn is_deduplicable(item: &ContextItem) -> bool {
    matches!(
        item.kind,
        ContextKind::FileContent
            | ContextKind::ToolResult
            | ContextKind::CommandOutput
            | ContextKind::TestResult
            | ContextKind::Error
            | ContextKind::AssistantMessage
    )
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

/// Recoverability score: higher means easier to obtain the content again.
pub fn recoverability_score(item: &ContextItem) -> f32 {
    match item.kind {
        ContextKind::FileContent => {
            if item.metadata.recoverable || item.metadata.file_path.is_some() {
                0.9
            } else {
                0.5
            }
        }
        ContextKind::ToolResult | ContextKind::CommandOutput => {
            if item.metadata.recoverable || item.metadata.command.is_some() {
                0.8
            } else {
                0.4
            }
        }
        ContextKind::Diff => 0.7,
        ContextKind::UserMessage
        | ContextKind::Constraint
        | ContextKind::Decision
        | ContextKind::SystemPrompt
        | ContextKind::DeveloperPrompt => 0.0,
        _ => 0.3,
    }
}

/// Detect items that should be exempt from eviction.
pub fn is_pinned(item: &ContextItem, config: &Config) -> bool {
    if item.metadata.pinned {
        return true;
    }
    match item.kind {
        ContextKind::SystemPrompt | ContextKind::DeveloperPrompt => true,
        ContextKind::Constraint if config.preservation.pin_user_constraints => true,
        ContextKind::UserMessage
            if config.preservation.pin_user_constraints
                && looks_like_explicit_constraint(&item.content) =>
        {
            true
        }
        ContextKind::Error if config.preservation.pin_unresolved_errors && is_unresolved(item) => {
            true
        }
        _ => false,
    }
}

fn looks_like_explicit_constraint(content: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\b(do not|don't|must not|never|always|must|avoid|only)\b").unwrap()
    })
    .is_match(content)
}

fn is_unresolved(item: &ContextItem) -> bool {
    item.state == ContextState::Active
        && (item.kind == ContextKind::Error
            || item.metadata.exit_code.map(|c| c != 0).unwrap_or(false))
}

/// Compute the explainable importance of an item relative to the session.
pub fn importance_score(
    item: &ContextItem,
    index: usize,
    total: usize,
    dedup: &DedupIndex,
    weights: &ImportanceWeights,
) -> ImportanceScore {
    let relevance = relevance_score(item);
    let recency = if total == 0 {
        1.0
    } else {
        (index as f32 / total as f32).clamp(0.0, 1.0)
    };
    let dependency = if item.parent_id.is_some() { 0.8 } else { 0.0 };
    let unresolved = if is_unresolved(item) { 1.0 } else { 0.0 };
    let authority = authority_score(item);
    let uniqueness = if dedup.duplicate_count(&item.id) <= 1 {
        1.0
    } else if dedup.is_canonical(&item.id) {
        0.7
    } else {
        0.1
    };
    let recoverability = recoverability_score(item);
    let redundancy = if dedup.duplicate_count(&item.id) > 1 {
        1.0
    } else {
        0.0
    };

    ImportanceScore::weighted(
        weights,
        &ImportanceComponents {
            relevance,
            recency,
            dependency,
            unresolved,
            authority,
            uniqueness,
            recoverability,
            redundancy,
        },
    )
}

fn relevance_score(item: &ContextItem) -> f32 {
    match item.kind {
        ContextKind::SystemPrompt
        | ContextKind::DeveloperPrompt
        | ContextKind::Constraint
        | ContextKind::Decision => 1.0,
        ContextKind::UserMessage => 0.95,
        ContextKind::AssistantMessage => 0.85,
        ContextKind::ToolCall => 0.75,
        ContextKind::Error => 0.9,
        ContextKind::FileContent => 0.55,
        ContextKind::CommandOutput | ContextKind::ToolResult => 0.35,
        ContextKind::TestResult => 0.4,
        ContextKind::Diff => 0.7,
        ContextKind::Checkpoint => 0.6,
        _ => 0.3,
    }
}

fn authority_score(item: &ContextItem) -> f32 {
    match item.kind {
        ContextKind::Constraint => 1.0,
        ContextKind::UserMessage => 0.9,
        ContextKind::SystemPrompt | ContextKind::DeveloperPrompt => 1.0,
        _ => 0.0,
    }
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

pub trait CompactionPlanner: Send + Sync {
    fn plan(
        &self,
        items: &[ContextItem],
        budget: &ContextBudget,
        config: &Config,
        prediction: &TokenPrediction,
    ) -> Result<CompactionPlan, PolicyError>;
}

pub struct DefaultPlanner {
    reducers: Vec<Box<dyn Reducer>>,
}

impl Default for DefaultPlanner {
    fn default() -> Self {
        Self {
            reducers: default_reducers(),
        }
    }
}

impl DefaultPlanner {
    pub fn new(reducers: Vec<Box<dyn Reducer>>) -> Self {
        Self { reducers }
    }

    fn find_reducer(&self, item: &ContextItem) -> Option<&dyn Reducer> {
        self.reducers
            .iter()
            .find(|r| r.can_reduce(item))
            .map(|b| b.as_ref())
    }

    fn compute_plan(
        &self,
        items: &[ContextItem],
        budget: &ContextBudget,
        config: &Config,
        prediction: &TokenPrediction,
        counter: &dyn TokenCounter,
    ) -> Result<CompactionPlan, PolicyError> {
        let usable = budget.usable_context();
        if usable == 0 {
            return Err(PolicyError::EmptyWindow);
        }
        let before_tokens = items
            .iter()
            .fold(0u64, |total, item| total.saturating_add(item.token_count));
        let predicted_extra = prediction
            .expected_next_input
            .saturating_add(prediction.expected_next_tool_output)
            .saturating_add(prediction.expected_next_assistant_output);
        let predicted_pressure =
            before_tokens.saturating_add(predicted_extra) as f32 / usable as f32;
        // The plan is predictive: its pressure state reflects the projected
        // next cycle, not only the tokens already resident.
        let pressure_state = config.pressure_state(predicted_pressure);
        let configured_target = (config.context.target_pressure * usable as f32) as u64;
        // When the forecast crosses the compact threshold, leave enough room
        // for the predicted operation instead of waiting for current usage to
        // cross the ordinary target first.
        let forecast_ceiling = (config.context.thresholds.compact * usable as f32) as u64;
        let forecast_target = forecast_ceiling.saturating_sub(predicted_extra);
        let target_tokens = if pressure_state.needs_compaction() {
            configured_target.min(forecast_target)
        } else {
            configured_target
        };

        let dedup = DedupIndex::build(items);
        let weights = ImportanceWeights::default();

        // Pre-compute candidate outputs and scores.
        let mut candidates: Vec<Candidate> = Vec::with_capacity(items.len());
        for (idx, item) in items.iter().enumerate() {
            let mut score = importance_score(item, idx, items.len(), &dedup, &weights);
            if is_pinned(item, config) {
                score.total = 1.0;
            }
            let reducer = if config.tools.reduce_logs {
                self.find_reducer(item)
            } else {
                None
            };
            let reduced_tokens = reducer
                .map(|r| counter.count(&r.reduce(item)))
                .unwrap_or(item.token_count);
            let extracted_tokens = reducer
                .map(|r| counter.count(&r.extract(item)))
                .unwrap_or(item.token_count);
            let can_externalize = config.tools.externalize_large_results
                && item.metadata.recoverable
                && item.token_count >= config.tools.large_result_tokens;
            let external_tokens = if can_externalize {
                counter.count(&format!(
                    "artifact://{}/{}",
                    item.kind_as_string(),
                    item.id.as_str()
                ))
            } else {
                extracted_tokens
            };
            candidates.push(Candidate {
                item,
                score,
                reduced_tokens,
                extracted_tokens,
                external_tokens,
                action: ContextAction::Keep,
                from_level: item.compression_level,
                to_level: item.compression_level,
                estimated_after: item.token_count,
                reason: "keep: baseline".to_string(),
            });
        }

        // Pin mandatory items first.
        for c in candidates.iter_mut() {
            if is_pinned(c.item, config) {
                c.action = ContextAction::Pin;
                c.to_level = c.item.compression_level;
                c.estimated_after = c.item.token_count;
                c.reason = "pin: mandatory or user constraint".to_string();
            }
        }

        // Deduplicate non-canonical copies. These actions are already selected
        // before the greedy level-by-level pass, so account for their savings
        // in the running token total as well as in the returned actions.
        let mut current = before_tokens;
        for c in candidates.iter_mut() {
            if c.action == ContextAction::Pin {
                continue;
            }
            if config.tools.deduplicate && is_deduplicable(c.item) && dedup.is_duplicate(&c.item.id)
            {
                c.action = ContextAction::Deduplicate;
                c.to_level = CompressionLevel::L5;
                c.estimated_after = 0;
                c.reason = "deduplicate: repeated content".to_string();
                current = current.saturating_sub(c.item.token_count);
            }
        }

        // Greedy compaction until we hit the target.
        let mut changed = true;
        while current > target_tokens && changed {
            changed = false;
            // Rank candidates by utility: token savings / information loss.
            // Skip pinned and already evicted.
            let mut best_idx = None;
            let mut best_utility = 0.0f32;
            for (i, c) in candidates.iter().enumerate() {
                if c.action == ContextAction::Pin || c.to_level == CompressionLevel::L5 {
                    continue;
                }
                let Some((next_level, after)) = next_effective_level(c) else {
                    continue;
                };
                // Once ordinary target pressure has been reached, do not
                // evict the last useful reduced representation merely to
                // shave a few tokens from a speculative forecast. Emergency
                // eviction is still allowed while current usage exceeds the
                // normal target.
                if next_level == CompressionLevel::L5 && current <= configured_target {
                    continue;
                }
                let savings = c.estimated_after.saturating_sub(after);
                if savings == 0 {
                    continue;
                }
                let info_loss = (1.0 - c.score.total).max(0.01);
                // Penalize actions that move away from verbatim.
                let level_penalty = 1.0 + (next_level.ordinal() as f32 * 0.15);
                let utility = savings as f32 / (info_loss * level_penalty);
                if utility > best_utility {
                    best_utility = utility;
                    best_idx = Some(i);
                }
            }
            if let Some(i) = best_idx {
                let c = &mut candidates[i];
                let Some((next, after)) = next_effective_level(c) else {
                    continue;
                };
                let reason = match next {
                    CompressionLevel::L1 => "reduce: structural cleanup",
                    CompressionLevel::L2 => "extract: keep only key parts",
                    CompressionLevel::L4 => "externalize: recoverable content",
                    CompressionLevel::L5 => "evict: low importance",
                    _ => "keep: baseline",
                };
                let savings = c.estimated_after.saturating_sub(after);
                c.action = action_for_level(next);
                c.to_level = next;
                c.estimated_after = after;
                c.reason = reason.to_string();
                current = current.saturating_sub(savings);
                changed = true;
            }
        }

        let actions: Vec<PlannedAction> = candidates
            .iter()
            .map(|c| PlannedAction {
                context_id: c.item.id.clone(),
                action: c.action,
                from_level: c.from_level,
                to_level: c.to_level,
                estimated_tokens_before: c.item.token_count,
                estimated_tokens_after: c.estimated_after,
                importance: c.score,
                reason: c.reason.clone(),
            })
            .collect();

        Ok(CompactionPlan {
            before_tokens,
            current_tokens: before_tokens,
            pressure_before: budget.pressure(),
            predicted_pressure,
            target_tokens,
            expected_tokens_after: current,
            pressure_state,
            actions,
        })
    }
}

impl CompactionPlanner for DefaultPlanner {
    fn plan(
        &self,
        items: &[ContextItem],
        budget: &ContextBudget,
        config: &Config,
        prediction: &TokenPrediction,
    ) -> Result<CompactionPlan, PolicyError> {
        // Default to a fast approximate counter for planning estimates.
        let counter = contextgc_tokenizer::ApproximateCounter;
        self.compute_plan(items, budget, config, prediction, &counter)
    }
}

struct Candidate<'a> {
    item: &'a ContextItem,
    score: ImportanceScore,
    reduced_tokens: u64,
    extracted_tokens: u64,
    external_tokens: u64,
    action: ContextAction,
    from_level: CompressionLevel,
    to_level: CompressionLevel,
    estimated_after: u64,
    reason: String,
}

/// Pick the least destructive later level that actually saves tokens. This
/// lets a file read with no structural reducer move directly from L0 to L4
/// instead of getting stuck at two no-op intermediate levels.
fn next_effective_level(c: &Candidate<'_>) -> Option<(CompressionLevel, u64)> {
    let mut level = next_compression_level(c.to_level);
    loop {
        let after = match level {
            CompressionLevel::L1 => c.reduced_tokens,
            CompressionLevel::L2 => c.extracted_tokens,
            CompressionLevel::L4 => c.external_tokens,
            CompressionLevel::L5 => 0,
            CompressionLevel::L0 | CompressionLevel::L3 => return None,
        };
        if level == CompressionLevel::L5 || c.estimated_after.saturating_sub(after) > 0 {
            return Some((level, after));
        }
        level = next_compression_level(level);
    }
}

fn next_compression_level(level: CompressionLevel) -> CompressionLevel {
    match level {
        CompressionLevel::L0 => CompressionLevel::L1,
        CompressionLevel::L1 => CompressionLevel::L2,
        CompressionLevel::L2 => CompressionLevel::L4,
        CompressionLevel::L4 => CompressionLevel::L5,
        CompressionLevel::L5 => CompressionLevel::L5,
        CompressionLevel::L3 => CompressionLevel::L4,
    }
}

fn action_for_level(level: CompressionLevel) -> ContextAction {
    match level {
        CompressionLevel::L0 => ContextAction::Keep,
        CompressionLevel::L1 => ContextAction::Reduce,
        CompressionLevel::L2 => ContextAction::Extract,
        CompressionLevel::L3 => ContextAction::Summarize,
        CompressionLevel::L4 => ContextAction::Externalize,
        CompressionLevel::L5 => ContextAction::Evict,
    }
}

trait KindString {
    fn kind_as_string(&self) -> &'static str;
}

impl KindString for ContextItem {
    fn kind_as_string(&self) -> &'static str {
        match self.kind {
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
}

// ---------------------------------------------------------------------------
// Prediction helpers
// ---------------------------------------------------------------------------

/// Update an exponential moving average.
pub fn update_ema(prev: f32, value: f32, alpha: f32) -> f32 {
    alpha * value + (1.0 - alpha) * prev
}

/// Event-type key used for prediction statistics.
pub fn event_type_for(item: &ContextItem) -> String {
    match item.kind {
        ContextKind::ToolCall | ContextKind::ToolResult => item
            .metadata
            .tool_name
            .clone()
            .unwrap_or_else(|| "tool".to_string()),
        ContextKind::CommandOutput => "command".to_string(),
        ContextKind::FileContent => "file-read".to_string(),
        ContextKind::AssistantMessage => "assistant".to_string(),
        _ => "other".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use contextgc_core::{ContextItem, ContextKind};

    fn item(kind: ContextKind, content: &str) -> ContextItem {
        ContextItem::new(kind, content).with_tokens(content.split_whitespace().count() as u64)
    }

    #[test]
    fn shell_reducer_strips_ansi_and_spinner() {
        let content = "\x1b[32m✓\x1b[0m Building\n\n|\n/\n|\n\x1b[32mDone\x1b[0m\nDone".to_string();
        let item = item(ContextKind::CommandOutput, &content);
        let reducer = ShellReducer;
        let out = reducer.reduce(&item);
        assert!(!out.contains("\x1b["));
        assert!(!out.contains('|'));
        assert!(out.contains("Done"));
    }

    #[test]
    fn test_reducer_collapses_successes() {
        let mut lines = vec!["test foo ... ok", "test bar ... ok", "test baz ... ok"];
        lines.push("test result: ok. 3 passed; 0 failed; finished in 0.12s");
        let content = lines.join("\n");
        let item = item(ContextKind::TestResult, &content);
        let reducer = TestReducer;
        let out = reducer.reduce(&item);
        assert!(!out.contains("test foo"));
        assert!(out.contains("3 tests passed"));
        assert!(out.contains("test result"));
    }

    #[test]
    fn generic_command_output_uses_shell_reducer_not_test_reducer() {
        let item = item(
            ContextKind::CommandOutput,
            "Building crate\n[==========> ] 50%\nBuild succeeded",
        );
        assert!(!TestReducer.can_reduce(&item));
        assert!(ShellReducer.can_reduce(&item));
        let reduced = ShellReducer.reduce(&item);
        assert!(reduced.contains("Build succeeded"));
    }

    #[test]
    fn compiler_reducer_keeps_errors() {
        let content = r#"error[E0308]: mismatched types
  --> src/lib.rs:42:14
   |
42 | let x: u32 = "hello";
   |              ^^^^^^^ expected `u32`, found `&str`

error[E0308]: mismatched types
  --> src/lib.rs:88:10
"#
        .to_string();
        let item = item(ContextKind::Error, &content);
        let reducer = CompilerReducer;
        let out = reducer.reduce(&item);
        assert!(out.contains("error[E0308]"));
        assert!(out.contains("src/lib.rs:42"));
        assert!(out.contains("src/lib.rs:88"));
        assert!(!out.contains('^'));
    }

    #[test]
    fn importance_pins_constraints() {
        let cfg = Config::default();
        let constraint = ContextItem::new(
            ContextKind::Constraint,
            "Do not modify the database schema.",
        );
        assert!(is_pinned(&constraint, &cfg));
        let explicit = ContextItem::new(ContextKind::UserMessage, "Do not modify migrations.");
        assert!(is_pinned(&explicit, &cfg));
        let msg = ContextItem::new(ContextKind::UserMessage, "hello");
        assert!(!is_pinned(&msg, &cfg));
    }

    #[test]
    fn dedup_detects_duplicates() {
        let items = vec![
            item(ContextKind::FileContent, "same"),
            item(ContextKind::FileContent, "same"),
            item(ContextKind::FileContent, "different"),
        ];
        let idx = DedupIndex::build(&items);
        assert!(idx.is_duplicate(&items[0].id));
        assert!(idx.is_canonical(&items[1].id));
        assert!(!idx.is_duplicate(&items[1].id));
    }

    #[test]
    fn planner_respects_target_pressure() {
        let items: Vec<ContextItem> = (0..20)
            .map(|i| {
                item(
                    ContextKind::CommandOutput,
                    &format!("build log line {i} \u{1b}[32m progress {i}% \u{1b}[0m"),
                )
            })
            .collect();
        let total_tokens: u64 = items.iter().map(|i| i.token_count).sum();
        let budget = ContextBudget {
            context_window: 128_000,
            current_tokens: total_tokens,
            system_tokens: 0,
            tool_schema_tokens: 0,
            reserved_output_tokens: 8_000,
            safety_tokens: 8_000,
        };
        let planner = DefaultPlanner::default();
        let plan = planner
            .plan(
                &items,
                &budget,
                &Config::default(),
                &TokenPrediction::default(),
            )
            .unwrap();
        assert!(plan.expected_tokens_after <= plan.before_tokens);
        assert!(
            plan.expected_tokens_after <= plan.target_tokens
                || plan.actions.iter().any(|a| a.action != ContextAction::Keep)
        );
    }

    #[test]
    fn forecast_can_trigger_reclamation_before_current_target() {
        let mut file = item(ContextKind::FileContent, "recoverable source");
        file.token_count = 50_000;
        file.metadata.recoverable = true;
        let items = vec![file];
        let budget = ContextBudget {
            context_window: 120_000,
            current_tokens: 50_000,
            system_tokens: 0,
            tool_schema_tokens: 0,
            reserved_output_tokens: 10_000,
            safety_tokens: 10_000,
        };
        let prediction = TokenPrediction {
            expected_next_input: 0,
            expected_next_tool_output: 30_000,
            expected_next_assistant_output: 0,
            confidence: 1.0,
        };
        let plan = DefaultPlanner::default()
            .plan(&items, &budget, &Config::default(), &prediction)
            .unwrap();
        assert!(plan.pressure_before < Config::default().context.target_pressure);
        assert!(plan.predicted_pressure >= Config::default().context.thresholds.compact);
        assert!(plan.target_tokens < plan.before_tokens);
        assert_eq!(plan.actions[0].action, ContextAction::Externalize);
    }
}
