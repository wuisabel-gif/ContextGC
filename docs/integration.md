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

## Optional long-term memory: MemWhale

[MemWhale](https://github.com/wuisabel-gif/MemWhale) can complement ContextGC
as a persistent local memory backend. Keep the responsibilities separate:

- ContextGC manages the active model working set.
- MemWhale stores durable debugging memories that may become relevant later.

ContextGC should not send every evicted object to memory. It should use a
separate long-term memory-value decision so novel fixes and architecture
decisions can be retained while noisy raw logs are discarded.

The reusable contracts are in `crates/contextgc-memory`; the first transport
boundary should be MCP over stdio rather than a direct Rust dependency:

```toml
[memory]
backend = "memwhale"
store_externalized = true
store_checkpoints = true
store_errors = true
store_successful_fixes = true
store_raw_logs = false

[memory.memwhale]
transport = "stdio"
command = "mw-mcp"
```

See [`docs/memwhale-integration.md`](memwhale-integration.md) for the full
working-memory/long-term-memory lifecycle.

## Tokenomist model routing

[Tokenomist](https://github.com/wuisabel-gif/Tokenomist) can sit one layer
above ContextGC. Tokenomist chooses a capable model or agent by task success
and cost; ContextGC receives that model's context budget and manages what it
sees during the run.

```text
Tokenomist selects model/agent
            │
            ▼
ContextGC receives context_window + reserve
            │
            ▼
ContextGC materializes the active working set
            │
            ▼
agent calls the selected model
            │
            ▼
ContextGC returns token/pressure telemetry
            │
            ▼
Tokenomist improves future routing
```

The JSONL protocol already provides the handoff: `session.start` accepts model
metadata, while `context.stats` reports active tokens, pressure, projected
pressure, and composition. This keeps Tokenomist and ContextGC independent and
allows the same boundary to work with Pi, Rho, Python, TypeScript, and custom
harnesses.

See [`docs/tokenomist-integration.md`](tokenomist-integration.md) for the
recommended lifecycle.

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
