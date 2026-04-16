# Upstream Contribution Plan

Detailed implementation plan for upstreaming agent-safety features from this fork into [rtk-ai/rtk](https://github.com/rtk-ai/rtk). All work is developed and dogfooded in this fork first, then submitted as focused PRs targeting upstream's `develop` branch.

References: [UPSTREAM_TODO.md](UPSTREAM_TODO.md), [upstream issue #1313](https://github.com/rtk-ai/rtk/issues/1313)

---

## PR 1: `[safety] no_truncation = true` config flag — COMPLETED

**Addresses:** Issue #1313 — silent truncation causes agent failures.

**Principle:** All lossy truncation (line caps, result limits) becomes opt-out. Lossless ops (ANSI strip, dedup, reformat, regex substitution) are unaffected.

**Status:** Implemented and committed. See commit for full diff.

### Implementation notes (for future PRs to reference)

**Config caching pattern:** Config is cached per-process via `OnceLock` in `src/core/config.rs`. The helpers `config::limits()`, `config::no_truncation()`, and `config::passthrough_limit()` all read from cache. Any new config helpers should follow the same pattern:

```rust
static CACHED_FOO: OnceLock<FooType> = OnceLock::new();

pub fn foo() -> &'static FooType {
    CACHED_FOO.get_or_init(|| Config::load().map(|c| c.foo).unwrap_or_default())
}
```

**`limits()` returns `&'static LimitsConfig`** (not owned). Callers access fields via auto-deref. This was a signature change from the original `LimitsConfig` return type. Adding new fields to `LimitsConfig` works transparently — `PartialEq` is derived so the startup warning comparison auto-covers new fields.

**`apply_filter` vs `apply_filter_with_safety`:** The original `apply_filter()` delegates to `apply_filter_with_safety(filter, stdout, false)` for backward compat. The `_with_safety` variant accepts a `no_truncation: bool` parameter, making it testable without touching the filesystem. PR 2 should follow this pattern: add parameters to the function signature rather than reading config inside filter logic.

**Stage 5 (`truncate_lines_at`) is intentionally NOT skipped** by `no_truncation`. It caps individual line *width* (a display concern), not line *count* (data loss). This is documented in the function's doc comment. If PR 2 adds new stages, classify each as lossy (line/result count reduction) or lossless (formatting) and guard accordingly.

**Truncation sites found during audit** (beyond what was originally planned):

| File | Truncation | Guarded? |
|------|-----------|----------|
| `src/cmds/git/git.rs:693` | `status_max_files`, `status_max_untracked` | Yes |
| `src/cmds/system/grep_cmd.rs:125` | `grep_max_per_file` | Yes |
| `src/cmds/python/ruff_cmd.rs:107` | `passthrough_max_chars` | Yes |
| `src/cmds/go/golangci_cmd.rs:273` | `passthrough_max_chars` | Yes |
| `src/cmds/js/lint_cmd.rs:242,334` | `passthrough_max_chars` | Yes |
| `src/cmds/js/lint_cmd.rs:466-471` | Hard-coded 20-issue cap + 100-char truncation in `filter_generic_lint` | Yes (was missed in original plan) |
| `src/parser/mod.rs:104` | `truncate_passthrough()` | Yes |
| `src/cmds/system/summary.rs` | Various `.take(N)` | Not guarded — display-only, not data loss |
| `src/cmds/system/deps.rs` | Various `.take(N)` | Not guarded — display-only |
| `src/cmds/rust/cargo_cmd.rs` | `.take(15)`, `.take(10)` for errors/failures | Not guarded — consider for future |
| `src/cmds/git/git.rs:1355` | `.take(10)` for remote-only branches | Not guarded — display-only |

**Startup warning** is scoped to `is_operational_command()` only (not `--version`, `gain`, `init`, etc.). It uses the cached helpers, not raw `Config::load()`. PR 2-4 should not add additional config warnings without checking the same scope.

**Pre-commit gate:** `cargo fmt --all && cargo clippy --all-targets && cargo test --all` — all 1459 tests pass.

---

## PR 2: Tee index — structured hints for agent navigation

**Addresses:** Agents reading tee files blindly. Gives `Read offset=N` targets without scanning.

### 2.1 Config types

File: `src/core/tee.rs`

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeeIndexRule {
    pub name: String,
    /// Regex pattern to match against raw output lines
    #[serde(rename = "match")]
    pub match_pattern: String,
    /// Which matches to keep: "first", "last", or "all"
    #[serde(default = "default_keep_first")]
    pub keep: KeepMode,
    /// Include matched line text in the hint (not just line number)
    #[serde(default)]
    pub show_line: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum KeepMode {
    #[default]
    First,
    Last,
    All,
}
```

Add to `TeeConfig`:
```rust
#[serde(default)]
pub index: Vec<TeeIndexRule>,
```

Config usage in `config.toml`:
```toml
[[tee.index]]
name = "first_error"
match = "error|^FAIL|^fatal"
keep = "first"
show_line = true

[[tee.index]]
name = "test_summary"
match = "^(ok|FAIL|---)"
keep = "all"
show_line = true
```

### Implementation guidance from PR 1

- **Regex compilation:** All regex in `match_pattern` MUST use `lazy_static!` or be pre-compiled into a `CompiledTeeIndexRule` struct at config load time. Never compile regex inside `compute_index()`.
- **Config caching:** `TeeConfig` is already a field on `Config`. If you need a cached accessor (like `config::limits()`), follow the `OnceLock` pattern from PR 1. However, tee index rules may be better passed as parameters (like `apply_filter_with_safety` does) for testability.
- **The `tee_and_hint()` signature change** will affect multiple call sites. Run `grep -n "tee_and_hint\|force_tee_hint" src/` to find them all. Each needs the new `Option<&[CompiledTeeIndexRule]>` parameter.

### 2.2 Per-filter index rules

File: `src/core/toml_filter.rs`

Add to `TomlFilterDef`:
```rust
#[serde(default)]
tee_index: Vec<TeeIndexRule>,
```

And to `CompiledFilter`:
```rust
pub tee_index: Vec<CompiledTeeIndexRule>,  // pre-compiled regexes
```

Per-filter rules augment (append to) global rules when that filter is active.

### 2.3 Index computation

File: `src/core/tee.rs`

After writing the tee file, iterate rules over the raw line buffer:

```rust
fn compute_index(raw: &str, rules: &[CompiledTeeIndexRule]) -> Vec<IndexResult> {
    let mut results = Vec::new();
    for rule in rules {
        let matches: Vec<(usize, &str)> = raw.lines()
            .enumerate()
            .filter(|(_, line)| rule.pattern.is_match(line))
            .map(|(i, line)| (i + 1, line))  // 1-indexed
            .collect();

        let kept = match rule.keep {
            KeepMode::First => matches.into_iter().take(1).collect(),
            KeepMode::Last => matches.into_iter().last().into_iter().collect(),
            KeepMode::All => matches,
        };

        if !kept.is_empty() {
            results.push(IndexResult {
                name: rule.name.clone(),
                matches: kept,
                show_line: rule.show_line,
            });
        }
    }
    results
}
```

### 2.4 Hint rendering

Extend `format_hint()` to append index block:

```
[full output: ~/.local/share/rtk/tee/cargo-test-171320.log]
  first_error  → L47: "FAIL: TestFoo (timeout)"
  test_summary → L112, L118, L203
```

When `no_truncation=false` and lines were dropped, auto-inject a truncation pointer:
```
  truncated → output truncated after L89; full results from L89 in tee file
```

This requires `tee_and_hint()` and `force_tee_hint()` to accept:
- `Option<&[CompiledTeeIndexRule]>` — the active index rules
- `Option<usize>` — line number where truncation started (if applicable)

### 2.5 Tests

- `TeeIndexRule` config deserialization (global and per-filter)
- `compute_index()` with first/last/all keep modes
- `compute_index()` with no matches returns empty
- Hint rendering with index results
- Truncation pointer emitted when lines were dropped
- Backward compat: no `[[tee.index]]` config = no index block (hint unchanged)

### 2.6 Docs

- Document `[[tee.index]]` config in config.toml template
- Add examples to `src/core/README.md`
- CHANGELOG entry

**Estimated size:** ~200 lines of code + tests. Medium PR, no pipeline changes.

---

## PR 3: TOML/Rust filter composability

**Addresses:** TOML filters silently ignored for Rust-handled commands. Users can't customize output for commands like `cargo`, `go`, `git`.

### PR 3a: `rust_override` — opt specific commands out of Rust handling

**Scope:** Config-driven routing bypass. Simple and safe.

Add to `FilterConfig` in `src/core/config.rs`:
```rust
/// Commands to route through TOML filters instead of Rust handlers.
/// Bypasses the built-in Rust module entirely for these commands.
#[serde(default)]
pub rust_override: Vec<String>,
```

Config usage:
```toml
[filters]
rust_override = ["make", "go"]
```

Routing change in `main.rs`: Before dispatching to the Clap-matched handler, check if the base command is in `rust_override`. If so, skip the Rust handler and fall through to `run_fallback()` (TOML filter path).

Remove the shadow warning in `toml_filter.rs` for commands listed in `rust_override` (since it's now intentional).

### Implementation guidance from PR 1

- **`FilterConfig` already has fields** (`ignore_dirs`, `ignore_files`). Adding `rust_override: Vec<String>` with `#[serde(default)]` is safe — existing configs without the field will deserialize fine (tested pattern in PR 1).
- **Routing in `main.rs`:** The Clap `Commands` enum is matched in `run_cli()` at the big `match cli.command` block (~line 1322). The override check needs to happen *before* this match. Extract the base command from `std::env::args()` (not from Clap, since Clap already parsed it). See how `run_fallback()` extracts `args` for reference.
- **Config access:** Use the `OnceLock` caching pattern. Add a `pub fn rust_overrides() -> &'static Vec<String>` helper, or access via `Config::load()` once at the routing decision point.
- **The `run_fallback()` call** already handles the TOML filter path including `apply_filter_with_safety` with `no_truncation`. No changes needed there — routing to it "just works" with PR 1's safety flag.

**Tests:**
- Command in `rust_override` routes to TOML filter
- Command NOT in `rust_override` routes to Rust handler (existing behavior)
- Config deserializes with/without the field

**Estimated size:** ~50 lines. Small PR.

### PR 3b: `mode = "compose"` — TOML pipeline after Rust output

**Scope:** Run TOML filter stages on the output of a Rust handler. More invasive.

Add `mode` field to `TomlFilterDef`:
```rust
#[serde(default)]
mode: FilterMode,  // Replace (default) | Compose
```

When `mode = "compose"`:
1. Rust handler runs and produces output
2. Output is captured as a string (not printed to stdout)
3. TOML filter pipeline runs on that string
4. Final result is printed

**Complexity:** Some Rust handlers print directly to stdout via `println!()`. These would need refactoring to return `String` instead. This is a larger change and should be scoped carefully.

### Implementation guidance from PR 1

- **Stdout capture problem:** Many handlers (e.g., `grep_cmd::run`, git status in `git.rs`) call `print!()` / `println!()` directly. To compose, they'd need to return `String`. This is a significant refactor — audit which handlers print directly vs return strings. Start with handlers that already return strings (e.g., `filter_ruff_json` in `ruff_cmd.rs` returns `String`).
- **`apply_filter_with_safety` is the right entry point** for the compose step — it already accepts the `no_truncation` flag, so composed output respects the safety config automatically.

**Recommendation:** Defer PR 3b until PR 3a proves the concept. Open a GitHub Discussion upstream to gauge interest before investing in the refactor.

**Estimated size:** Variable — depends on how many handlers need stdout refactoring.

---

## PR 4: Documentation — `tee.mode = "always"` for agents

**Deferred** until PRs 1-2 are dogfooded. Then it becomes a natural recommendation alongside the new features.

**Scope:**
- Add note to docs recommending `tee.mode = "always"` in agent contexts
- Consider auto-setting it when `rtk init` detects an agent context (`--agent` flag)
- CHANGELOG entry

**Estimated size:** ~20 lines. Trivial PR.

---

## Execution Order

```
PR 1 (no_truncation) ✅ DONE
  ↓
PR 2 (tee index)
  ↓
Dogfood on Togather project — validate workflow
  ↓
PR 3a (rust_override) — open Discussion upstream first
  ↓
PR 3b (compose mode) — if upstream receptive
  ↓
PR 4 (docs) — after features prove value
```

## Upstream Strategy

- All PRs target upstream `develop` branch
- Each PR is self-contained: code + tests + docs + CHANGELOG
- Reference issue #1313 in PR 1
- Keep PRs small and focused (upstream explicitly prefers this)
- Consider opening GitHub Discussion for PR 3 (routing philosophy change)
- Sign CLA when submitting first upstream PR

## Pre-Implementation Checklist

- [x] Full audit of truncation sites across `src/cmds/` (for PR 1)
- [x] Verify `apply_filter()` call sites (for PR 1)
- [ ] Map all `tee_and_hint()` / `force_tee_hint()` call sites (for PR 2)
- [ ] Test with real agent workflow before upstreaming

## Cross-Cutting Implementation Patterns (learned from PR 1)

These patterns apply to all future PRs:

1. **Config caching:** Use single `CACHED_CONFIG: OnceLock<Config>` with `cached_config()` accessor. All field accessors (`limits()`, `no_truncation()`) reference this single cached instance — one disk read per process. Never call `Config::load()` in a loop or hot path. See `config::limits()`, `config::no_truncation()`, `config::passthrough_limit()` for examples.

2. **Backward compatibility:** All new config sections MUST use `#[serde(default)]` on the parent `Config` field AND on individual fields within the new struct. Test deserialization of TOML that omits the new section entirely.

3. **Testability:** Prefer passing config values as function parameters (like `apply_filter_with_safety(filter, stdout, no_truncation)`) over reading config inside the function. This makes unit tests filesystem-independent.

4. **Startup warnings:** Scope to `is_operational_command()` in `run_cli()`. Don't warn on meta commands (`--version`, `gain`, `init`, `config`, `verify`).

5. **Truncation classification:** When adding new filter stages or output caps, classify as lossy (reduces data — must respect `no_truncation`) or lossless (reformats — always active). Document the classification in the function's doc comment.

6. **Pre-commit gate:** `cargo fmt --all && cargo clippy --all-targets && cargo test --all` — zero tolerance for warnings or failures. Run after every logical change.

7. **Duplicate code:** Extract helpers early. The `config::passthrough_limit()` pattern (combining a safety check with a config value) should be used whenever the same guard appears in 2+ places. Extract pure logic into testable helpers (e.g. `compute_passthrough_limit()`) to avoid tests duplicating implementation logic.

8. **Truncation site classification for grep:** `max_line_len` (display-width, user-controlled via `--max-len`) and `max_results` (user-controlled via `--max`) are both CLI-arg-driven, not config-driven silent truncation. Classified as display concerns, same as pipeline stage 5. No `no_truncation` guard needed.
