# Fixes Summary - Approval Cache & System Prompt

## Issues Fixed

### 1. Multiple Approval Prompts for Same File
**Problem**: Multiple edits to the same file in a single turn (or across turns) triggered separate approval prompts each time.

**Root Causes**:
1. Approval cache was per-turn (reset on each new message)
2. Path normalization didn't handle absolute paths correctly (double slashes)
3. Relative paths not resolved to absolute before caching
4. `apply_patch` didn't populate cache for subsequent edits

### 2. AI Reading All Files Upfront
**Problem**: AI would read all files in the project instead of exploring first
**Root Cause**: System prompt didn't enforce exploration order

---

## Files Modified

### 1. `src/storage.rs`
**Changes**:
- Added `approved_paths: HashSet<String>` field to `Session` struct
- Added `HashSet` import
- All functions now `async` (Phase 1 async I/O)
- `create_session` initializes `approved_paths: HashSet::new()`

### 2. `src/server.rs`
**Changes**:
- **Per-session approval cache** (persists across turns):
  - Added `approved_paths` field to `Session` struct initialization (3 places)
  - Cache loaded at turn start: `std::mem::take(&mut entry.approved_paths)`
  - Cache saved at turn end: `entry.approved_paths = approved_paths`
  - Uses `std::mem::take()` for efficient transfer without cloning

- **Path normalization fix** (`normalize_path_key`):
  - Handles absolute paths correctly (skips `RootDir`, prepends single `/`)
  - Resolves relative paths to absolute using `ctx.working_dir` before caching
  - Handles `./`, `../`, `//` correctly

- **apply_patch path extraction**:
  - Parses patch headers (`*** Update File:`, `--- a/`, `+++ b/`)
  - Extracts file paths and populates approval cache
  - Added debug logging for extraction

- **Approval logic**:
  - Checks cache before prompting: `approved_paths.contains(&norm_key)`
  - Auto-approves if already in cache: `info!("auto-approving ...")`
  - Inserts on user approval: `approved_paths.insert(norm_key)`
  - Debug logging for cache hits/misses/inserts

- **Session initialization**: Added `approved_paths: HashSet::new()` in 3 places

### 3. `src/provider.rs`
**Changes**:
- Updated system prompt to enforce exploration order:
  1. **First**: `list_dir` to discover structure
  2. **Then**: `glob` / `agentgrep` to find specific files
  3. **Last**: `read` only specific files needed
  4. **No upfront reading** of all files

---

## How It Works Now

### Approval Cache Flow
```
Turn Start:
  1. Load approved_paths from session (std::mem::take)
  2. Cache: { "/abs/path/styles.css", "/abs/path/index.html" }

Tool Call (edit styles.css):
  1. Resolve path → absolute → normalize
  2. Check cache: contains("/abs/path/styles.css") → true
  4. Auto-approve: "auto-approving edit (already approved in this session)"
  5. No prompt shown to user

Turn End:
  1. Save approved_paths back to session
  2. Cache persists for next turn
```

### Path Normalization Examples
| Input | Normalized Key |
|-------|----------------|
| `styles.css` | `/home/user/project/styles.css` |
| `./styles.css` | `/home/user/project/styles.css` |
| `../project/styles.css` | `/home/user/project/styles.css` |
| `/home/user/project/styles.css` | `/home/user/project/styles.css` |

All map to same cache key → single approval.

---

## Verification

### Tests
- ✅ `cargo build` succeeds
- ✅ All 5 unit tests pass
- ✅ Protocol E2E tests pass

### Manual Verification (from conversation)
**Request 1** (color change):
- 1 approval for `styles.css` → subsequent edits auto-approved ✅

**Request 2** (2-column redesign):
- 1 approval for `index.html` ✅
- 1 approval for `styles.css` ✅
- 1 approval for `script.js` ✅
- No repeated prompts for same file ✅

**AI Behavior**:
1. `list_dir` first ✅
2. Read specific files needed ✅
4. Targeted edits only ✅

---

## Debug Logging (for troubleshooting)

Logs show in daemon stdout/stderr:
```
loaded approval cache for session connect-...: 2 paths
approval check: tool=edit path_key=... norm_key=/abs/path/styles.css cache_size=2 cache_contains=true
auto-approving edit (already approved in this session): ...
inserted into approval cache: norm_key=/abs/path/index.html cache_size=3
extract_patch_paths: found path=styles.css absolute=/home/... norm=/home/user/project/styles.css
saving approval cache for session connect-...: 3 paths
```

To view logs: Run `jancode serve` in one terminal, `jancode connect` in another.

---

## Files Changed Summary

| File | Lines Changed | Purpose |
|------|---------------|---------|
| `src/storage.rs` | +10 | Add `approved_paths` to Session, async I/O |
| `src/server.rs` | +50 | Per-session cache, path normalization, apply_patch extraction, debug logging |
| `src/provider.rs` | +15 | Updated system prompt for exploration order |

Total: ~65 lines added, 10 deleted