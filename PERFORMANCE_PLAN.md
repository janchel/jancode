# Performance Improvement Plan

## Current Bottlenecks (Identified)

| # | Component | Location | Impact | Priority |
|---|-----------|----------|--------|----------|
| 1 | Sync file I/O | `src/storage.rs` | High - blocks runtime on every turn | High |
| 2 | Blocking stdin | `src/client.rs:233` | Medium - blocks runtime during input | Medium |
| 3 | Sequential tools | `src/server.rs` | Medium - no parallel tool execution | Medium |
| 4 | Session copy on every turn | `src/server.rs` | Low - clones full history | Low |

---

## 1. Async File I/O for Session Persistence

### Current
```rust
// src/storage.rs - blocking
fs::write(&tmp, data)?;
fs::read_to_string(&path)?;
```

### Target
```rust
// Use tokio::fs for async I/O
tokio::fs::write(&tmp, data).await?;
tokio::fs::read_to_string(&path).await?;
```

### Scope
- `src/storage.rs`: `save_session`, `load_session`, `list_sessions`
- `src/server.rs`: session loading/saving in hot path

### Risk
- Low - pure I/O change, no logic changes
- Test: existing unit tests + manual `jancode connect`/`run`

---

## 2. Async Stdin for Client

### Current
```rust
// src/client.rs - blocking
let n = std::io::stdin().read_line(&mut raw_input)?;
```

### Target
```rust
use tokio::io::{self, AsyncBufReadExt};
let mut stdin = io::BufReader::new(io::stdin());
let n = stdin.read_line(&mut raw_input).await?;
```

### Scope
- `src/client.rs`: `connect()`, `run_prompt()` (approval prompts)
- Need `tokio::io::AsyncBufReadExt` import

### Risk
- Low - straightforward async conversion
- Test: `jancode connect`, `jancode run`, approval prompts

---

## 3. Parallel Tool Execution

### Current
```rust
// src/server.rs - sequential
for tc in &tool_calls {
    let result = tool.execute(&tc.input, &ctx).await?;
}
```

### Target
```rust
// Group independent tools, execute concurrently
let futures: Vec<_> = tool_calls.iter()
    .filter(|tc| tool.is_independent(tc))  // read-only, no shared state
    .map(|tc| tool.execute(&tc.input, &ctx))
    .collect();
let results = futures::future::join_all(futures).await;
```

### Criteria for Parallelization
- **Safe**: `read`, `list_dir`, `glob`, `agentgrep`, `fetch_url`, `sql` (SELECT)
- **Unsafe**: `write`, `edit`, `bash`, `edit`, `docker exec/run`, `sql` (INSERT/DELETE), `git` (mutating)

### Risk
- Medium - need to track file dependencies, avoid write-after-read conflicts
- Mitigation: start with read-only tools only

---

## 4. Session Copy Optimization

### Current
```rust
// Full history copied on every turn
let msgs = sessions.read().await.get(&id).map(|e| e.messages.clone());
```

### Target
- Use `Arc<Vec<Message>>` for shared immutable history
- Copy-on-write for new messages only

### Risk
- Medium - requires restructuring session storage
- Lower priority - current copy is fast for typical session sizes

---

## Implementation Order

| Phase | Tasks | Est. Time |
|-------|-------|-----------|
| 1 | Async file I/O (storage) | 1-2 hrs |
| 2 | Async stdin (client) | 1 hr |
| 3 | Parallel read-only tools | 2-3 hrs |
| 4 | Session copy optimization | 2-3 hrs |

---

## Testing Strategy

| Test | Scope |
|------|-------|
| Unit tests | `cargo test` - all existing pass |
| `jancode connect` | Manual: input, history, tools, approvals |
| `jancode run` | Headless: one-shot, tools, sessions |
| `jancode serve` | Daemon startup, idle shutdown |
| Load test | Simulate 50+ turns, verify no deadlocks |

---

## Rollout Plan

1. **Branch**: `perf-async-io` from `tools-web-note-docker`
2. **PR**: Each phase as separate commit
2. **CI**: Must pass `cargo test` + `cargo clippy`
3. **Merge**: Squash to `tools-web-note-docker`

---

## Success Criteria

| Metric | Target |
|--------|--------|
| `cargo test` | All pass |
| `clippy` | No new warnings |
| Session save (1000 msgs) | < 5ms (vs ~50ms sync) |
| Tool parallel (3 reads) | ~1/3 sequential time |
| No regressions | `jancode connect`/`run` work identically |