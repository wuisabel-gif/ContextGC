# ContextGC — Initial Design & Milestone 1 Plan

## 1. Proposed Cargo workspace layout

```
contextgc/
├── Cargo.toml
├── README.md
├── DESIGN.md
├── crates/
│   ├── contextgc-core/          # Domain model, budgets, pressure, importance, config
│   ├── contextgc-tokenizer/     # TokenCounter trait + exact/fallback counters
│   ├── contextgc-policy/        # Importance / recoverability / compaction policy rules
│   ├── contextgc-store/         # SQLite persistent history + event log
│   ├── contextgc-engine/        # Session orchestration and working-set materialization
│   ├── contextgc-memory/        # Optional long-term memory backend contracts
│   ├── contextgc-cli/           # `contextgc` binary (status, plan, compact, stats)
│   └── contextgc-protocol/        # JSON-RPC/stdio protocol types + framing
├── adapters/pi/                 # TypeScript adapter placeholder (Phase 10)
└── fixtures/                    # Test data
```

## 2. Principal Rust types

Core identity / item model:

```rust
pub struct ContextId(pub String);
pub struct SessionId(pub String);
pub type Timestamp = chrono::DateTime<chrono::Utc>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ContextState {
    Active,
    Resolved,
    Superseded,
    Abandoned,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMetadata {
    pub file_path: Option<PathBuf>,
    pub command: Option<String>,
    pub exit_code: Option<i32>,
    pub tool_name: Option<String>,
    pub artifact_ref: Option<String>,
    pub recoverable: bool,
    pub pinned: bool,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextItem {
    pub id: ContextId,
    pub parent_id: Option<ContextId>,
    pub kind: ContextKind,
    pub content: String,
    pub token_count: u64,
    pub created_at: Timestamp,
    pub source: ContextSource,
    pub metadata: ContextMetadata,
    pub state: ContextState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContextSource { ... }
```

Budget / pressure:

```rust
pub struct ContextBudget { pub context_window: u64, pub current_tokens: u64, ... }
pub enum PressureState { Green, Observe, Trim, Compact, Aggressive, Emergency }
```

Actions / levels:

```rust
pub enum ContextAction { Keep, Deduplicate, Reduce, Extract, Summarize, Externalize, Evict, Pin }
pub enum CompressionLevel { L0, L1, L2, L3, L4, L5 }
```

Planner:

```rust
pub trait CompactionPlanner {
    fn plan(&self, session: &SessionState, budget: &ContextBudget) -> Result<CompactionPlan>;
}
```

Tokenizer:

```rust
pub trait TokenCounter: Send + Sync {
    fn count(&self, text: &str) -> u64;
    fn exact(&self) -> bool;
}
```

## 3. Dependencies

- `serde` + `serde_json` — serialization everywhere.
- `chrono` — timestamps.
- `thiserror` — typed errors.
- `tracing` / `tracing-subscriber` — structured logging.
- `sha2` + `hex` — content hashing.
- `regex` — deterministic reducers.
- `unicode-segmentation` — fallback token approximation.
- `rusqlite` (store crate) — SQLite persistence.
- `clap` (cli crate) — CLI.
- `anyhow` allowed only in CLI/tests; core uses `thiserror`.

## 4. Persistence schema (SQLite)

```sql
CREATE TABLE sessions (session_id TEXT PRIMARY KEY, config_json TEXT, created_at INTEGER);
CREATE TABLE events (event_id INTEGER PRIMARY KEY, session_id TEXT, type TEXT, payload_json TEXT, created_at INTEGER);
CREATE TABLE context_items (
  item_id TEXT PRIMARY KEY,
  session_id TEXT,
  parent_id TEXT,
  kind TEXT,
  content_hash TEXT,
  content TEXT,
  token_count INTEGER,
  created_at INTEGER,
  source_json TEXT,
  metadata_json TEXT,
  state TEXT
);
CREATE TABLE artifacts (artifact_id TEXT PRIMARY KEY, item_id TEXT, content TEXT, created_at INTEGER);
CREATE TABLE compaction_runs (
  run_id INTEGER PRIMARY KEY,
  session_id TEXT,
  before_tokens INTEGER,
  after_tokens INTEGER,
  pressure_state TEXT,
  created_at INTEGER
);
CREATE TABLE compaction_actions (
  run_id INTEGER,
  item_id TEXT,
  action TEXT,
  from_level TEXT,
  to_level TEXT,
  estimated_before INTEGER,
  estimated_after INTEGER,
  importance_json TEXT,
  reason TEXT
);
CREATE TABLE token_stats (session_id TEXT, event_type TEXT, ema REAL, count INTEGER);
```

## 5. Milestone 1 implementation plan

**Goal:** End-to-end proof with a 128k synthetic session where predicted pressure crosses 72%, the planner preserves a constraint + error verbatim, deduplicates file reads, reduces a build log, externalizes recoverable content, and active context falls toward 55–60% while history remains intact.

### Phase 1 (this session)
- Create Cargo workspace and all crate skeletons.
- Implement `contextgc-core` domain model, budgets, pressure states, config validation, token-count abstraction.
- Unit tests for pressure, budgets, thresholds, token counting.

### Phase 2
- Implement `contextgc-store` SQLite event log and item persistence.
- Add hashing + exact deduplication in core.

### Phase 3
- Deterministic reducers: shell, test, compiler, file-read.
- Fixtures and reducer tests.

### Phase 4
- Importance / recoverability scoring, pinned-context rules, explainability.

### Phase 5
- CompactionPlanner with Keep/Pin/Deduplicate/Reduce/Extract/Externalize/Evict.
- Inspectable plan output.

### Phase 6
- EMA prediction and projected-pressure triggering.

### Phase 7
- Checkpoints and working-set materialization.
- Million-token synthetic-session integration test.

The first implementation also introduces `contextgc-engine` between the
policy/store crates and the CLI so session orchestration stays reusable by
the stdio server and future adapters.

### Phase 8
- CLI (`contextgc ingest`, `status`, `plan`, `compact`, `stats`).

### Phase 9
- JSONL stdio protocol.

### Phase 10
- Pi adapter (TypeScript placeholder / thin translator).

### Optional memory integration
- Keep ContextGC and MemWhale as separate projects.
- Expose a generic `MemoryBackend` interface from `contextgc-memory`.
- Prefer MCP/stdio transport for the first MemWhale adapter.
- Score long-term memory value separately from active-context retention value.

### Optional routing integration
- Keep Tokenomist above ContextGC as the model/agent selection layer.
- Pass the selected model's context-window metadata through `session.start`.
- Feed ContextGC token and pressure telemetry back to Tokenomist for future
  cost-aware routing decisions.

I will now begin implementation at Phase 1.
