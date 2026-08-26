# ContextGC

**Predictive, selective context management for long-running AI agents.**

ContextGC helps coding agents and agent harnesses keep working as sessions grow beyond a model's practical context window. Instead of waiting until context is nearly full and summarizing everything at once, ContextGC continuously tracks context pressure and decides what should stay, what can be compressed, and what can safely leave the active working set.

Session history is persistent state. Model context is a temporary working set. ContextGC manages the working set.

## Origin

ContextGC started from a problem I ran into while using Ox Alpha through the Rho agent harness.

During longer coding sessions, the agent would eventually stop being able to continue once the session accumulated too much context. The immediate problem looked like a token or context-window limit, but it pointed to a broader issue: long-running agents need a better way to manage what stays in active context.

Simply waiting until the context is almost full and summarizing the oldest messages did not feel sufficient. Coding sessions contain very different kinds of information—user requirements, active errors, file contents, build logs, decisions, failed experiments, and completed subtasks—and they should not all be compressed the same way.

That experience became the motivation for ContextGC: a general context-management layer that can work across harnesses such as Rho, Pi, and other agent systems, proactively detecting future context pressure and selectively deciding what should remain verbatim, what can be compressed, and what can safely be externalized or removed from the model's active working set.

The goal is not to work around one limitation in Ox Alpha. The goal is to solve the underlying context-lifecycle problem for long-running agents in general.

## Why ContextGC?

Long-running agents accumulate enormous amounts of context:

- conversation history
- file reads
- tool results
- compiler output
- test logs
- errors
- diffs
- decisions
- completed debugging branches

Eventually the harness has to make room.

Most auto-compaction approaches treat old context as one large block:

```text
old history
    ↓
summarize
    ↓
continue
```

But not all context is equally valuable.

A 20,000-token build log may be disposable. An explicit user constraint may need to survive for the entire session.

ContextGC manages context at the individual-object level:

```text
                    active context
                          │
                          ▼
          importance + recoverability
             + pressure analysis
                          │
         ┌────────────────┼────────────────┐
         ▼                ▼                ▼
       KEEP            COMPRESS          REMOVE
                         │
          ┌──────────────┼──────────────┐
          ▼              ▼              ▼
       reduce         extract       summarize
                                      │
                            externalize / evict
                          │
                          ▼
                 budgeted working set
```

The goal is not to produce the smallest prompt.

The goal is to preserve the most useful information possible within a safe context budget.

## What it does

ContextGC can selectively:

- **Pin** critical instructions, constraints, and active state.
- **Deduplicate** repeated file reads and tool results.
- **Reduce** noisy logs without throwing away useful diagnostics.
- **Extract** important errors and signals from large outputs.
- **Summarize** completed or lower-priority history through a deterministic V1 stub and future semantic backends.
- **Externalize** recoverable data while keeping a reference to it.
- **Evict** low-value information from the model's active context.
- **Predict** upcoming context pressure before the model reaches its limit.

| Context item | Action |
|---|---|
| System instructions | PIN |
| `Do not modify the database schema` | PIN |
| Current compiler error | KEEP |
| Current diff | KEEP |
| Repeated `src/auth.ts` reads | DEDUPLICATE |
| 30k-token successful test log | REDUCE |
| Completed debugging branch | SUMMARIZE |
| Old recoverable file contents | EXTERNALIZE |
| Obsolete tool output | EVICT |

The original session history remains intact.

## A representative run

```text
$ contextgc compact --session coding --predicted-extra 30000
compacted session 'coding'
  tokens: 100019 → 30024 (reclaimed 69995)
  items in working set: 4
```

The corresponding plan can explain the decisions:

```text
constraint-1  PIN     explicit user constraint
error-1       PIN     unresolved error
file-1        KEEP    recoverable, but current working set still has room
build-1       REDUCE  structural cleanup of a large command output
```

Compaction changes what is sent to the model. It does not destroy the underlying history.

## Predictive compaction

ContextGC does not need to wait until a model reaches 99% of its context window.

It tracks both current and projected pressure:

```text
Current context                112k
Model context window           200k
Current usage                   56%
Predicted tool output           38k
Predicted assistant output      16k
Safety reserve                  15k
Projected usage                181k
Projected pressure             90.5%
→ compact before continuing
```

This matters for autonomous agents because a session can overflow inside a tool loop, even when there was plenty of room at the beginning of the turn.

## Context as a managed working set

ContextGC separates two concepts that agent harnesses often treat as the same thing:

```text
Persistent session history
2,400,000 tokens
        │
        │ ContextGC
        ▼
Active model working set
~70,000 tokens
```

Compaction changes what is sent to the model. It does not overwrite the original session history.

## Quick start

Build and test the workspace:

```bash
cargo build --workspace
cargo test --workspace
```

Ingest a session:

```bash
contextgc ingest --session coding --file session.jsonl
```

Inspect its current context pressure:

```bash
contextgc status --session coding
```

Preview what ContextGC would reclaim:

```bash
contextgc plan --session coding --predicted-extra 30000
```

Apply the compaction plan:

```bash
contextgc compact --session coding
```

Inspect accumulated statistics:

```bash
contextgc stats --session coding
```

Use custom policy and model defaults:

```bash
contextgc status \
  --session coding \
  --config contextgc.toml
```

The default database is `.contextgc.db`. Set `CONTEXTGC_DB` or pass `--db` to use another path. Configuration can be supplied with `--config` or `CONTEXTGC_CONFIG`.

## Harness integration

ContextGC is designed to sit underneath different agent harnesses rather than being tied to a specific model or agent.

```text
       Pi
        │
       Rho
        │
  custom harness
        │
        ▼
   ┌───────────┐
   │ ContextGC │
   └─────┬─────┘
         │
         ▼
 Claude / GPT / Gemini / Kimi / local models / ...
```

Harnesses can communicate with ContextGC through the newline-delimited JSON protocol:

```bash
contextgc protocol
```

One JSON request or response is exchanged per line over stdin/stdout. Diagnostics are written to stderr, keeping stdout machine-safe.

Example:

```json
{"type":"session.start","request_id":"1","session_id":"coding","model":{"name":"generic-200k","context_window":200000}}
{"type":"context.add","request_id":"2","item":{"kind":"UserMessage","content":"Fix the login middleware."}}
{"type":"context.plan","request_id":"3","predicted_extra_tokens":30000}
{"type":"context.compact","request_id":"4"}
{"type":"context.stats","request_id":"5"}
```

The integration layer should remain thin: harness adapters translate events into ContextGC's generic context model while compaction policy stays inside the core engine.

## ContextGC + MemWhale

[MemWhale](https://github.com/wuisabel-gif/MemWhale) is an optional long-term
memory companion, not a replacement for ContextGC.

> ContextGC manages what stays in an agent's context. MemWhale preserves what
> is worth remembering after it leaves.

The distinction is deliberate:

- **ContextGC:** What should the model know right now?
- **MemWhale:** What happened before, and what might be useful again?

When an object leaves the active working set, ContextGC can make a second
decision based on long-term memory value. A novel successful fix or architecture
decision may be stored; a noisy successful build log may simply be discarded.
Relevant memories can later be retrieved, scored, and promoted back into the
working set.

The projects remain independent. The optional interface lives in
`crates/contextgc-memory`, and the preferred first integration path is MCP:

```text
ContextGC ── MCP/stdio ──> mw-mcp ──> MemWhale SQLite memory
```

Read the [MemWhale integration guide](docs/memwhale-integration.md) for the
lifecycle, memory-value policy, and backend boundary.

## Pressure model

ContextGC operates at multiple pressure levels rather than using a single emergency threshold.

```text
GREEN       plenty of room
OBSERVE     begin tracking likely compaction candidates
TRIM        remove redundancy and cheap noise
COMPACT     selectively compress low-value context
AGGRESSIVE  collapse completed work and externalize data
EMERGENCY   guarantee room for another model/tool cycle
```

Thresholds are configurable. The planner attempts to reclaim context using the least destructive action first:

```text
deduplicate
    ↓
reduce
    ↓
extract
    ↓
summarize
    ↓
externalize
    ↓
evict
```

Critical state can be pinned and excluded from normal reclamation.

## Architecture

ContextGC is implemented as a Rust workspace:

```text
crates/
├── contextgc-core
├── contextgc-tokenizer
├── contextgc-policy
├── contextgc-store
├── contextgc-engine
├── contextgc-memory
├── contextgc-protocol
└── contextgc-cli
adapters/
└── pi
```

### `contextgc-core`

Core domain model:

- context objects
- token budgets
- pressure states
- importance scores
- compression levels
- checkpoints
- configuration

### `contextgc-tokenizer`

Token-count abstraction with support for exact provider tokenizers when available and an explicit approximate fallback otherwise.

### `contextgc-policy`

Deterministic context policy:

- importance scoring
- recoverability scoring
- pinned-context rules
- content hashing and deduplication
- shell, test, and compiler reducers
- candidate ranking and compaction planning

### `contextgc-store`

Append-only SQLite persistence for:

- sessions
- original events
- context objects
- artifacts
- compaction runs and actions
- token statistics
- persisted working-set projections

Compaction never overwrites the original history.

### `contextgc-engine`

Session orchestration:

- pressure prediction
- deterministic ingestion
- compaction planning
- working-set materialization
- checkpoint ingestion
- persistent working-set recovery

### `contextgc-memory`

Optional long-term memory backend interface. ContextGC remains independent of
any particular memory system; MemWhale can be connected through MCP or a thin
backend adapter.

### `contextgc-protocol`

Generic newline-delimited JSON protocol used by harness adapters.

### `contextgc-cli`

The `contextgc` command-line interface.

### `adapters/pi`

Thin TypeScript integration for Pi. Harness-specific behavior belongs in the adapter. Context management policy does not.

## Design principles

### Predict, don't react

Context management should happen before the model rejects the next request.

### Compress selectively

A user requirement and a build log should not receive the same treatment.

### Prefer reversible operations

Deduplicating or externalizing recoverable information is safer than immediately summarizing it.

### Preserve provenance

Compacted information should remain traceable to its original session events and artifact references.

### Keep the critical path deterministic

Token accounting, deduplication, pressure calculation, recoverability, structural reduction, and planning should not require an LLM.

Semantic summarization can be an optional strategy rather than a dependency.

### Make every decision inspectable

ContextGC should be able to explain:

- what was compacted
- why it was compacted
- how many tokens it saved
- what was preserved
- where the original data lives

## Status

ContextGC is under active development.

The initial focus is on:

- deterministic selective compaction
- predictive context pressure
- persistent session history
- explainable compaction plans
- a generic stdio protocol
- thin integrations with existing agent harnesses

Semantic compaction and additional harness adapters can build on top of this core.

## License

MIT OR Apache-2.0
