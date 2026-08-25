# Integration Guide

ContextGC exposes two interfaces:

1. The `contextgc` command-line tool.
2. A newline-delimited JSON protocol over stdio (`contextgc protocol`).

Both are model-agnostic and harness-agnostic.  Adapters only translate harness
events into `ContextItem` objects and apply the materialized working set.

## CLI quick start

```bash
contextgc protocol
```

Then send JSONL lines on stdin:

```json
{"type":"session.start","request_id":"1","session_id":"my-session","model":{"name":"generic-200k","context_window":200000}}
{"type":"context.add","request_id":"2","item":{"id":"...","kind":"UserMessage","content":"Fix the login middleware."}}
{"type":"context.plan","request_id":"3"}
{"type":"context.compact","request_id":"4"}
{"type":"context.stats","request_id":"5"}
```

`ContextItem` fields such as `token_count`, `created_at`, `source`, metadata,
and lifecycle state are optional at the wire boundary when the adapter does
not provide them; the engine fills deterministic defaults and counts tokens.

Responses are written to stdout, one JSON object per line.  Diagnostics go to
stderr.

## Adapter API

A minimal adapter implements:

- `getModelInfo(): ModelInfo` — return the model name and context window.
- `getContext(): ContextItem[]` — return the current harness context.
- `replaceWorkingContext(items: MaterializedContextItem[]): void` — replace the
  active context with the compacted working set.
- `storeArtifact(item: ContextItem): Promise<ArtifactReference>` — optional;
  ContextGC can store artifacts itself via the stdio server.

Do not implement compaction intelligence in the adapter.

## Pi adapter

The Pi adapter in `adapters/pi/` provides a small `ContextGCClient` that
spawns `contextgc protocol`, forwards messages and tool results, and asks the
server for a materialized context before each model call. It also includes a
generic `piMessageToContextGC` translator; actual Pi hook registration remains
host-specific. If Pi's extension API does not permit replacing the working
context, document the limitation rather than work around it.

## Configuration

Configuration can be supplied with the CLI `--config` flag, the
`CONTEXTGC_CONFIG` environment variable, or directly to the stdio server.
The protocol model still supplies the active model context window; the TOML
file supplies policy and reserve defaults.

```toml
[context]
target_pressure = 0.55
compact_pressure = 0.72
aggressive_pressure = 0.82
emergency_pressure = 0.90

[reserve]
output_tokens = 16000
safety_tokens = 12000

[preservation]
recent_tokens = 24000
pin_user_constraints = true
pin_unresolved_errors = true

[tools]
deduplicate = true
reduce_logs = true
externalize_large_results = true
large_result_tokens = 6000

[prediction]
enabled = true
ema_alpha = 0.25

[semantic]
enabled = false
```
