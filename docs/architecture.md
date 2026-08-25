# ContextGC Architecture

ContextGC sits between an agent harness and the model.  It observes every
context object, persists the full history, and materializes a budgeted working
set for each model call.

```
┌─────────────┐     JSONL/stdio      ┌─────────────────────┐
│   Adapter   │ ◄──────────────────► │   contextgc-cli     │
│  (Pi, etc.) │                      │   contextgc-engine  │
└─────────────┘                      └──────────┬──────────┘
                                                │
              ┌─────────────────────────────────┼──────────────┐
              │                                 │              │
       ┌──────▼──────┐  ┌──────────▼──────┐  ┌─▼──────────┐  │
       │   policy    │  │    tokenizer    │  │   store    │  │
       │  (dedupe,  │  │ (exact / approx)│  │  (SQLite)  │  │
       │  scoring,  │  │                 │  │            │  │
       │  planner)   │  │                 │  │            │  │
       └─────────────┘  └─────────────────┘  └────────────┘  │
              │                                 │              │
              └─────────────────────────────────┴──────────────┘
                                │
                         ┌──────▼──────┐
                         │  contextgc  │
                         │   -core     │
                         │  (types)    │
                         └─────────────┘
```

## Layers

- **contextgc-core** — domain types: context items, budgets, pressure, actions, plans, configuration.
- **contextgc-tokenizer** — `TokenCounter` trait; exact or approximate token counting.
- **contextgc-policy** — deterministic reducers, deduplication, importance/recoverability scoring, pinned-context rules, and the `CompactionPlanner`.
- **contextgc-store** — append-only SQLite persistence for sessions, events, items, artifacts, compaction runs, and token statistics.
- **contextgc-engine** — `Session` orchestration: ingestion, EMA prediction, planning, materialization, and telemetry.
- **contextgc-protocol** — newline-delimited JSON request/response types for stdio integration.
- **contextgc-cli** — command-line tool and daemon entry point.

## Data flow

1. The adapter sends `context.add` for each new message, tool result, file read, etc.
2. The engine hashes the item, counts tokens, classifies it, persists it, and updates EMA statistics.
3. Before a model call the engine computes current and predicted pressure.
4. If predicted pressure exceeds the configured threshold, the engine invokes the planner.
5. The planner returns an inspectable `CompactionPlan` that maps each item to an action and a compression level.
6. The engine materializes the plan into a `WorkingSet` and returns it to the adapter.
7. The original items remain in the SQLite event log and can be audited.

## Compaction continuum

| Level | Name | Meaning |
|-------|------|---------|
| L0 | Verbatim | Keep content unchanged |
| L1 | Structural reduction | Strip ANSI, collapse whitespace, deduplicate lines |
| L2 | Extractive reduction | Keep command/exit/errors/status only |
| L3 | Semantic summary | Optional LLM-based summary (disabled in V1) |
| L4 | External reference | Store original, keep artifact ref |
| L5 | Evicted | Not in active working set; still in history |

## Design principles

- Deterministic policies are the default; LLM-based summarization is optional.
- Session history is append-only; compaction only changes the working set.
- Explainability: every action has a reason and an importance score.
- Predictive: compaction is triggered by projected pressure, not by current pressure alone.
