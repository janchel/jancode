# jancode project instructions

## Build / test

- Build: `cargo build` (Rust 2021 edition, tokio + reqwest stack).
- Tests: `cargo test`.
- Formatting: `cargo fmt`; keep `cargo clippy` clean when practical.

## Conventions

- Keep tools in `src/tools.rs` following the existing `#[async_trait] impl Tool`
  pattern, with JSON-schema parameters and a short self-documenting description.
- Approval gating lives in `src/server.rs` `gate_tool`: read-only actions should
  return `None`; anything that mutates the host, writes files, or hits a remote
  should return `Some(ApprovalGate{..})` with a clear reasoned reason.
- New tools must be registered in `default_registry()` and exposed to clients in
  the tool allow-lists in `src/client.rs`.
- Never hardcode API keys; read them from config/env via `resolve_api_key`.

## Rules for agents

- Explore the repo with the file tools before editing; run `cargo build` and
  `cargo test` after changes before reporting done.
- Commit messages: concise imperative summary + 1-2 line context, matching the
  existing history style.