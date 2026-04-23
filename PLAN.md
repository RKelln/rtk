# Upstream Contribution Plan

Detailed implementation plan for upstreaming agent-safety features from this fork into [rtk-ai/rtk](https://github.com/rtk-ai/rtk). All work is developed and dogfooded in this fork first, then submitted as focused PRs targeting upstream's `develop` branch.

References: [UPSTREAM_TODO.md](UPSTREAM_TODO.md), [upstream issue #1313](https://github.com/rtk-ai/rtk/issues/1313)

---

## PR 1: `[limits] lossless = true` config flag — COMPLETED

**Addresses:** Issue #1313 — silent truncation causes agent failures.

**Principle:** All lossy truncation (line caps, result limits) becomes opt-out. Lossless ops (ANSI strip, dedup, reformat, regex substitution) are unaffected.

**Status:** Implemented and committed. See commit for full diff.

### Implementation notes (for future PRs to reference)

**Config caching pattern:** Config is cached per-process via a single `OnceLock<Config>` in `src/core/config.rs`. The private `cached_config()` accessor loads the config once; all public helpers reference it. Any new config helpers should follow this pattern:

```rust
pub fn foo() -> &'static FooType {
    &cached_config().foo
}
```

**Do NOT create separate `OnceLock`s per field** — that re-introduces multiple disk reads.

**`limits()` returns `&'static LimitsConfig`** (not owned). Callers access fields via auto-deref. This was a signature change from the original `LimitsConfig` return type. Adding new fields to `LimitsConfig` works transparently — `PartialEq` is derived so the startup warning comparison auto-covers new fields.

**`apply_filter` vs `apply_filter_with_safety`:** The original `apply_filter()` delegates to `apply_filter_with_safety(filter, stdout, false)` for backward compat. The `_with_safety` variant accepts a `lossless: bool` parameter, making it testable without touching the filesystem. PR 2 should follow this pattern: add parameters to the function signature rather than reading config inside filter logic.

**Stage 5 (`truncate_lines_at`) is intentionally NOT skipped** by `lossless`. It caps individual line *width* (a display concern), not line *count* (data loss). This is documented in the function's doc comment. If PR 2 adds new stages, classify each as lossy (line/result count reduction) or lossless (formatting) and guard accordingly.

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
| `src/cmds/rust/cargo_cmd.rs:280` | `.take(15)` for install errors | Yes |
| `src/cmds/rust/cargo_cmd.rs:630` | `.take(15)` for build errors | Yes |
| `src/cmds/rust/cargo_cmd.rs:842` | `.take(10)` + `truncate(200)` for test failures | Yes |
| `src/cmds/rust/cargo_cmd.rs:1013` | `.take(10)` error blocks + `.take(15)` rules + `.take(3)` locs + `truncate(160)` | Yes |
| `src/cmds/rust/cargo_cmd.rs:856` | `.take(5)` fallback meaningful lines | Not guarded — last-resort fallback, rarely hit |
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
- **The `tee_and_hint()` signature change** will affect multiple call sites. Run `grep -n "tee_and_hint\|force_tee_hint" src/` to find them all. Rather than adding individual parameters (`Option<&[CompiledTeeIndexRule]>`, `Option<usize>`), use a context struct to avoid future signature churn:

```rust
pub struct TeeHintContext<'a> {
    pub index_rules: &'a [CompiledTeeIndexRule],
    pub truncation_line: Option<usize>,
}
```

This costs nothing now and prevents PR 3/4 from requiring another signature sweep across all call sites.

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

When `lossless=false` and lines were dropped, auto-inject a truncation pointer:
```
  truncated → output truncated after L89; full results from L89 in tee file
```

This requires `tee_and_hint()` and `force_tee_hint()` to accept an `Option<&TeeHintContext>` parameter (see context struct above).

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
- **Routing in `main.rs`:** The Clap `Commands` enum is matched in `run_cli()` at the big `match cli.command` block (~line 1322). The override check should happen *inside* each relevant Clap match arm (not before it via `env::args()`), using the Clap-resolved command name to avoid divergence between Clap's alias/case resolution and a manual `env::args()` extraction. Each guarded arm falls through to `run_fallback()` when the command is in `rust_override`.
- **Config access:** Use the `OnceLock` caching pattern. Add a `pub fn rust_overrides() -> &'static Vec<String>` helper, or access via `Config::load()` once at the routing decision point.
- **The `run_fallback()` call** already handles the TOML filter path including `apply_filter_with_safety` with `lossless`. No changes needed there — routing to it "just works" with PR 1's safety flag.

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
- **`apply_filter_with_safety` is the right entry point** for the compose step — it already accepts the `lossless` flag, so composed output respects the safety config automatically.

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
PR 1 (lossless) ✅ DONE
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

1. **Config caching:** Use single `CACHED_CONFIG: OnceLock<Config>` with `cached_config()` accessor. All field accessors (`limits()`, `lossless()`) reference this single cached instance — one disk read per process. Never call `Config::load()` in a loop or hot path. See `config::limits()`, `config::lossless()`, `config::passthrough_limit()` for examples.

2. **Backward compatibility:** All new config sections MUST use `#[serde(default)]` on the parent `Config` field AND on individual fields within the new struct. Test deserialization of TOML that omits the new section entirely.

3. **Testability:** Prefer passing config values as function parameters (like `apply_filter_with_safety(filter, stdout, lossless)`) over reading config inside the function. This makes unit tests filesystem-independent.

4. **Startup warnings:** Scope to `is_operational_command()` in `run_cli()`. Don't warn on meta commands (`--version`, `gain`, `init`, `config`, `verify`).

5. **Truncation classification:** When adding new filter stages or output caps, classify as lossy (reduces data — must respect `lossless`) or lossless (reformats — always active). Document the classification in the function's doc comment.

6. **Pre-commit gate:** `cargo fmt --all && cargo clippy --all-targets && cargo test --all` — zero tolerance for warnings or failures. Run after every logical change.

7. **Duplicate code:** Extract helpers early. The `config::passthrough_limit()` pattern (combining a safety check with a config value) should be used whenever the same guard appears in 2+ places. Extract pure logic into testable helpers (e.g. `compute_passthrough_limit()`) to avoid tests duplicating implementation logic.

8. **Truncation site classification for grep:** `max_line_len` (display-width, user-controlled via `--max-len`) and `max_results` (user-controlled via `--max`) are both CLI-arg-driven, not config-driven silent truncation. Classified as display concerns, same as pipeline stage 5. No `lossless` guard needed.

9. **Effective limits vs raw limits:** `config::passthrough_limit()` bakes in the `lossless` guard, but `config::limits()` returns raw values — callers must check `lossless()` separately. In Rust handler code (like `cargo_cmd.rs`), use the inline pattern `let cap = if config::lossless() { usize::MAX } else { N };` at each truncation site. If a future PR introduces many new limit fields, consider an `effective_limits()` helper that returns a `LimitsConfig` with all values pre-set to `usize::MAX` when `lossless=true`.

10. **`apply_filter()` call site safety:** Only one production call site exists (`main.rs:1139`), which already uses `apply_filter_with_safety()`. The plain `apply_filter()` is used by TOML inline test runner and unit tests — both correct. When adding new TOML filter call sites, always use `apply_filter_with_safety()` with `config::lossless()`.

11. **Lossy output caps — always use `config::lossless_cap(n)`:** Every place that limits how many items (lines, errors, results, files) are shown to the user must call `config::lossless_cap(n)` instead of a bare integer literal. This returns `usize::MAX` in lossless mode and `n` otherwise. Three exemption categories exist (add a trailing comment to silence `scripts/check-raw-take.sh`): `// summarization` (top-N stat summaries like "Top linters"), `// display` (`chars().take(N)` line-width), `// internal` (data structures not shown to user). The check script runs as part of the pre-commit gate.

---

## Remaining `.take(N)` hard caps audit — PR 1 follow-up

Full grep of `src/cmds/` on branch `feature/no-truncation` (2026-04-17). Sites are classified into four categories:

### LOSSY → needs `lossless` guard

Apply pattern: `let cap = if config::lossless() { usize::MAX } else { N };` then `.take(cap)`, and guard any `+N more` footer similarly.

| File | Lines | N | Notes |
|------|-------|---|-------|
| `cloud/aws_cmd.rs` | L1503 | 10 | S3 transfer errors |
| `cloud/aws_cmd.rs` | L551,574,594,621,650,675,844,964,1003,1128,1154,1257,1304,1349,1407 | `MAX_ITEMS` | Check if `MAX_ITEMS` already driven by `passthrough_limit()` before guarding |
| `cloud/aws_cmd.rs` | L729 | `MAX_LOG_EVENTS` | Same check |
| `cloud/container.rs` | L91, L181, L298, L323, L401 | 10–20 | Containers/services/issues shown |
| `cloud/wget_cmd.rs` | L88 | 10 | wget output lines capped |
| `dotnet/dotnet_cmd.rs` | L384, L1003, L1016, L1072, L1089, L1099, L1128, L1138 | 10–20 | Files/errors/warnings/failures |
| `git/gh_cmd.rs` | L252, L510, L592 | 5–20 | PRs/issues shown |
| `git/git.rs` | L1367 | 10 | Remote-only branches (previously noted as "display-only" in table above — reclassified as LOSSY) |
| `go/go_cmd.rs` | L582, L671 | 20 | Errors/issues shown |
| `js/next_cmd.rs` | L134 | 10 | Bundle routes shown |
| `js/prettier_cmd.rs` | L98 | 10 | Files to format |
| `js/prisma_cmd.rs` | L368 | 5 | Prisma errors |
| `python/mypy_cmd.rs` | L176 | 5 | Mypy errors |
| `python/pip_cmd.rs` | L180, L209 | 10 | Packages shown |
| `python/pytest_cmd.rs` | L172 | 5 | Test failures |
| `ruby/rake_cmd.rs` | L201, L208 | varies | Tasks shown |
| `ruby/rubocop_cmd.rs` | L222 | varies | Offenses shown |
| `ruby/rspec_cmd.rs` | L227, L350 | varies | Failures shown |
| `rust/runner.rs` | L46 | 10 | Last N lines of failed command output |
| `rust/runner.rs` | L228 | 10 | Failures (already has `+N more` footer) |
| `system/deps.rs` | L110, L119, L184, L219, L262 | 5–15 | Dependencies shown |
| `system/env_cmd.rs` | L74 | 5 | PATH entries |
| `system/env_cmd.rs` | L109 | 20 | Other env vars |
| `system/format_cmd.rs` | L245 | 10 | Files to format |
| `system/log_cmd.rs` | L132, L173 | 5–10 | Error/warning log groups |
| `system/summary.rs` | L160 | 5 | Test failures in summary |
| `system/summary.rs` | L239, L258, L280 | 5–10 | Lines of various output |

### SUMMARIZATION → leave alone (intentional top-N stats)

| File | Lines | Reason |
|------|-------|--------|
| `go/golangci_cmd.rs` | L320, L328 | "Top linters" / "Top files" stat summary |
| `go/golangci_cmd.rs` | L344 | Top-3 linters per file drill-down |
| `js/lint_cmd.rs` | L288, L296 | "Top rules" / "Top files" stat summary |
| `js/lint_cmd.rs` | L311, L428 | Top-3 rules per file drill-down |
| `js/lint_cmd.rs` | L406, L414 | Symbol/file count stat summaries |
| `js/tsc_cmd.rs` | L126 | "Top codes" summary line |
| `system/ls.rs` | L220 | Top-5 extension types in dir listing |
| `system/find_cmd.rs` | L369 | Top-5 extensions in find result |

### DISPLAY → leave alone (`chars().take(N)` line-width truncation)

| File | Lines |
|------|-------|
| `cloud/wget_cmd.rs` | L246, L260 |
| `system/env_cmd.rs` | L44 |
| `system/log_cmd.rs` | L144, L184 |
| `system/grep_cmd.rs` | L200 |

### INTERNAL → leave alone (intermediate pipeline data, not output caps)

| File | Lines | Reason |
|------|-------|--------|
| `system/local_llm.rs` | L88, L99, L159, L195, L211, L225, L274 | Building `key_imports`/`key_fns`/`patterns` internal structs |
| `rust/cargo_cmd.rs` | L883 | `meaningful.iter().rev().take(5).rev()` — selects representative fallback lines |

### ALREADY GUARDED (use `*_cap` variables driven by `config::lossless()`)

| File | Lines |
|------|-------|
| `rust/cargo_cmd.rs` | L286, L635, L845, L1012, L1030, L1032 |

---

## Design Note: Magic Numbers & Output Density Vocabulary

*Status: future work / upstream proposal candidate*

### The Problem

After auditing all `.take(N)` sites, there are 47+ hard-coded integer literals scattered across 20+ files with no shared vocabulary and no documentation for *why* any specific value was chosen. Common questions that are currently unanswerable without source-diving:

- "What's the cap for test failures in RSpec?" → `grep` required
- "Why is it 5 for warnings but 10 for errors?" → unknown, likely vibes
- "Can I tune these per-project without recompiling?" → no

This is a natural consequence of organic CLI growth, and agentic coding accelerates the problem — agents write scattered magic numbers just as fluently as structured ones, so the mess compounds faster.

### Key Insight: The Vibes Are Real, Just Unnamed

When auditing the `.take(N)` sites, the implicit mental model behind every number was:

> *"How many of these do I need to understand what's going on?"*

The answer clusters naturally by **density intent**, not by command or output type:

| Intent | Default | Typical use |
|--------|---------|-------------|
| `few`  | 3       | tight context — stack lines, per-file breakdowns |
| `some` | 5       | focused attention — failures, warnings |
| `many` | 10      | pattern recognition — files, rules, deps |
| `lots` | 20      | inventory browsing — env vars, full dep lists |

The numbers *are* vibes. The insight is that they're **consistent vibes** — `some` is always "enough to understand the problem without being overwhelmed", regardless of whether it's RSpec failures or lint warnings. The vocabulary makes the intent auditable without over-specifying it.

### Proposed Architecture

**Code is the source of truth.** Config is a sparse user override layer.

```rust
// src/core/caps.rs
pub enum Cap { Few, Some, Many, Lots }

pub fn cap(c: Cap) -> usize {
    // checks: per-command override → global override → compiled default
    match effective(c) {
        Cap::Few  => 3,
        Cap::Some => 5,
        Cap::Many => 10,
        Cap::Lots => 20,
    }
}
```

Call sites become expressive without naming the thing twice:

```rust
// rspec_cmd.rs — "some failures is enough"
failures.iter().take(config::cap(Cap::Some))

// deps.rs — "many deps for browsing"
deps.iter().take(config::cap(Cap::Many))

// log_cmd.rs — "few context lines per entry"
stack.lines().take(config::cap(Cap::Few))
```

`--lossless` maps cleanly: sets all levels to `usize::MAX`.

### User Config (`~/.config/rtk/caps.toml`)

Users express their personal density contract — not command knowledge:

```toml
[caps]
few  = 3
some = 8    # I like more context than default
many = 15
lots = 25
```

Per-command overrides for genuine outliers — two forms, both valid:

```toml
# Form 1: raw number (escape hatch)
[caps.overrides.rspec]
some = 3

# Form 2: level alias — "for rspec, treat 'some' as if it were 'few'"
[caps.overrides.rspec]
some = few
```

Form 2 is the more idiomatic expression: you're saying "rspec output is dense, dial `some` down a level" without knowing or caring what `few` resolves to numerically. If the user later adjusts `few = 4`, rspec automatically inherits that without a second edit.

Resolution is **one level only** — `some = few` resolves to the global value of `few`, not to any further override of `few`. No transitive chains; that way lies madness.

Edge cases:
- `some = some` → no-op, uses global default (useful for explicit "reset to global")
- `some = lots` → valid, user knows what they're doing
- `few = lots` → chaotic but permitted; the config is yours

### Discovery: `rtk caps --dump`

Rather than shipping a default `caps.toml` (which gets stale), the binary generates it on demand:

```
$ rtk caps --dump
# RTK output density defaults. Override any value in ~/.config/rtk/caps.toml.
# --lossless bypasses all caps regardless of these settings.

[caps]
few  = 3    # tight context: stack traces, per-file breakdowns
some = 5    # focused: failures, warnings
many = 10   # pattern recognition: files, rules, deps
lots = 20   # inventory: env vars, dep lists

# Per-command overrides (sparse — only set what differs from your [caps] values)
# [caps.overrides.rspec]
# some = 3
```

The dump is always in sync because it reads compiled constants, not a static file. Users copy relevant lines, tune, done.

### What This Replaces

Current: `lossless_cap(10)` — opaque, scattered, untunable
Proposed: `config::cap(Cap::Many)` — intent-named, centrally defaulted, user-overridable

The `format_capped_list(items, cap, footer_label)` helper in `src/core/utils.rs` can eliminate the repetitive `for/if/footer` pattern at the same time, since every call site would be touching the `.take()` anyway.

### Suggested Upstream PR Sequence

1. Add `src/core/caps.rs` with `Cap` enum and `cap()` resolver
2. Add `[caps]` + `[caps.overrides.*]` to config schema
3. Replace all `lossless_cap(N)` literals with `config::cap(Cap::*)` 
4. Add `rtk caps --dump` subcommand
5. (Bonus) Extract `format_capped_list()` helper to kill the for/if/footer repetition


