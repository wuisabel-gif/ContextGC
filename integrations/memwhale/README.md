# MemWhale integration

This directory documents the optional MCP boundary between ContextGC and
[MemWhale](https://github.com/wuisabel-gif/MemWhale).

The projects remain independent:

- ContextGC governs active model context.
- MemWhale stores durable local debugging memory.

The preferred first implementation is a stdio MCP adapter that translates the
`contextgc-memory` contracts into MemWhale queries and writes, rather than a
direct Rust dependency on `memorywhale-core`.

A future adapter can be configured with:

```toml
[memory]
backend = "memwhale"

[memory.memwhale]
transport = "stdio"
command = "mw-mcp"
```

See [`docs/memwhale-integration.md`](../../docs/memwhale-integration.md) for
the lifecycle, memory-value policy, and promotion flow.
