# Jancode Standalone Migration Plan

## Objective
Make `jancode` fully independent from `jcode` — its own binary, config paths, sockets, and identity — so both can run on the same machine without conflict.

---

## Current Coupling Points

| Area | Current Value (jcode) | Target (jancode) |
|------|-----------------------|------------------|
| Config directory | `~/.jcode/` | `~/.jancode/` |
| Runtime / socket dir | `/run/user/$(id -u)/` with `jcode.sock` | `/run/user/$(id -u)/` with `jancode.sock` |
| Socket filename | `jcode.sock` | `jancode.sock` |
| Environment variables | `JCODE_HOME`, `JCODE_RUNTIME_DIR` | `JANCODE_HOME`, `JANCODE_RUNTIME_DIR` |
| Comments / docs | "jcode-style", "mirrors jcode" | Update to "jancode" terminology |

---

## Migration Steps

### 1. Configuration Paths (`src/config.rs`)
- [ ] Change `jcode_dir()` → `jancode_dir()`: `~/.jancode`
- [ ] Change `config_path()` → `~/.jancode/config.toml`
- [ ] Change `runtime_dir()` → `/run/user/$(id -u)/jancode` + socket `jancode.sock`
- [ ] Change `sessions_dir()` → `~/.jancode/sessions`
- [ ] Rename env vars:
  - `JCODE_HOME` → `JANCODE_HOME`
  - `JCODE_RUNTIME_DIR` → `JANCODE_RUNTIME_DIR`

### 2. Socket Filename (`src/server.rs`, `src/client.rs`)
- [ ] Rename socket from `jcode.sock` → `jancode.sock`
- [ ] Update all references in server bind and client connect logic

### 3. Code Comments & Terminology
- [ ] `src/tools.rs`: Remove "jcode-tool-types" / "jcode-tool-core" references → "jancode"
- [ ] `src/server.rs`: Replace "jcode-style" / "jcode reloads" / "jcode's" → "jancode"
- [ ] `src/protocol.rs`: Replace "jcode-style" / "jcode's report_back" / "jcode run" → "jancode"
- [ ] `src/client.rs`: Replace "jcode's lazy shared-server model" / "jcode run" → "jancode"
- [ ] `src/swarm.rs`: Replace "jcode's design" / "jcode's behavior" → "jancode"

### 4. Default Config Template (README / docs)
- [ ] Update sample `config.toml` in README to show `[provider]` under jancode paths
- [ ] Document new env vars: `JANCODE_HOME`, `JANCODE_RUNTIME_DIR`

### 5. Build & Verification
- [ ] `cargo build --release`
- [ ] Run `jancode serve` → verify socket at `/run/user/$(id -u)/jancode.sock`
- [ ] Run `jancode connect` → verify config loads from `~/.jancode/config.toml`
- [ ] Run `jancode run "test"` → verify one-shot works
- [ ] Verify no conflict when `jcode` is also running

---

## Files to Modify

| File | Priority | Changes |
|------|----------|---------|
| `src/config.rs` | Critical | Paths, env vars, function names |
| `src/server.rs` | Critical | Socket name, comments |
| `src/client.rs` | Critical | Socket name, comments |
| `src/tools.rs` | Medium | Comments |
| `src/protocol.rs` | Medium | Comments |
| `src/swarm.rs` | Medium | Comments |
| `README.md` | Medium | Config example, env vars |
| `Cargo.toml` | Low | Verify package name is `jancode` |

---

## Verification Checklist

- [ ] `jancode serve` creates `jancode.sock` (not `jcode.sock`)
- [ ] `jancode connect` reads `~/.jancode/config.toml`
- [ ] Sessions stored in `~/.jancode/sessions/`
- [ ] Can run `jancode` and `jcode` simultaneously on different sockets
- [ ] All tests pass (if any exist)

---

## Notes
- This is a **renaming-only** migration; no logic changes required.
- Keep backward compatibility for a transition period? (Optional: support both env var names)
- The `JCODE_HOME` / `JCODE_RUNTIME_DIR` env vars in the *existing* config file should be documented as deprecated.

---

## Timeline Estimate
- Code changes: ~30 minutes
- Build & test: ~15 minutes
- Total: ~45 minutes