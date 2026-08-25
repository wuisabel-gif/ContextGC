# ContextGC Policy

ContextGC's default policy is deterministic.  It does not require an LLM to
make basic compaction decisions.

## Actions

| Action | Meaning |
|--------|---------|
| Keep | Preserve verbatim |
| Pin | Preserve verbatim and exempt from eviction |
| Deduplicate | Remove redundant copies; keep newest canonical |
| Reduce | Structural cleanup (ANSI, progress bars, repeated lines) |
| Extract | Keep selected portions only |
| Summarize | Semantic summary (stubbed in V1) |
| Externalize | Move payload to artifact store; keep reference |
| Evict | Remove from active context; remain in history |

## Compression levels

L0 → L1 → L2 → L4 → L5 (L3 is reserved for optional semantic summarization).

The planner prefers the least destructive action that reaches the token target.

## Pinned context

The following are pinned by default:

- System and developer prompts
- `Constraint` items (when `pin_user_constraints = true`)
- Unresolved `Error` items with non-zero exit code (when `pin_unresolved_errors = true`)
- Items explicitly marked `pinned: true` in metadata

Adapters can pin any item via metadata.

## Recoverability

Items whose contents can be cheaply obtained again are deprioritized.  Examples:

- File content on disk: highly recoverable
- Command output: recoverable if the command is known
- User instructions: not recoverable

## Importance scoring

The score is a weighted combination of:

- relevance
- recency
- dependency
- unresolved status
- user authority
- uniqueness
- recoverability penalty
- redundancy penalty

Each component is returned separately so plans are explainable.

## Prediction

Token statistics are maintained per event type using exponential moving
averages.  Before an expensive tool call, ContextGC predicts the projected
pressure and compacts if necessary.
