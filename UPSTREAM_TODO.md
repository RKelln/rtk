# Upstream Contributions TODO

Planned contributions to rtk from the Togather project context.
See issue #1313 for the truncation safety concern.

## 1. `[limits] no_truncation = true` config flag

**Problem:** `[limits]` caps (`grep_max_results`, `status_max_files`, etc.) silently truncate
output. An agent reading truncated `git status` or `grep` output gets a partial picture with no
indication data was dropped. This causes subtle, hard-to-debug failures.

**Proposed:** Add a `no_truncation = true` flag to the existing `[limits]` section that disables all
lossy truncation while preserving lossless ops (ANSI strip, dedup, reformat). When enabled,
all `[limits]` caps are bypassed and full output is emitted.

```toml
[limits]
no_truncation = true   # disable lossy caps; preserves lossless ops
```

**Implementation sketch:**
- Add `no_truncation: bool` field to `LimitsConfig` with `#[serde(default)]`
- Thread config through all command handlers that apply limits
- When `no_truncation = true`, skip the `take(n)` / `truncate` calls
- Emit a one-time warning at startup if `no_truncation = true` and any other `[limits]` value
  differs from defaults (contradictory config)

---

## 2. TOML filters override Rust built-ins for `make`, `go`, `cargo`

**Problem:** These commands are in `RUST_HANDLED_COMMANDS` so TOML `match_output` /
`keep_lines_matching` filters never activate for them. Project-local `.rtk/filters.toml` is
silently ignored for the most important build commands.

**Proposed:** Either:
- Remove the shadow warning and let TOML filters compose with (not replace) the Rust handler, OR
- Add `[overrides] rust_handled = ["make", "go"]` config to opt specific commands out of the
  Rust built-in path entirely

---

## 3. Config-driven tee index: what to surface and where it lives

**Problem:** Tee writes full raw output to `~/.local/share/rtk/tee/<file>.log` but the hint
printed to the agent is just `[full output: ~/...file.log]` — no guidance on what's relevant or
where to look. The agent must scan the whole file blindly. Worse, when `no_truncation` is off,
lines that were *dropped from the compressed output* have no pointer back to their location in
the raw log at all.

**Proposed:** A `[[tee.index]]` config table — an ordered list of named extraction rules. After
writing the tee file, rtk applies each rule against the in-memory raw lines, records the matched
line numbers, and appends a structured index block to the agent-facing hint.

```toml
[[tee.index]]
name = "first_error"
match = "error|^FAIL|^fatal"   # regex (case-insensitive)
keep = "first"                  # first | last | all
show_line = true                # include matched line text in hint

[[tee.index]]
name = "test_summary"
match = "^(ok|FAIL|---)"
keep = "all"
show_line = true

[[tee.index]]
name = "truncated_warning"
# Special built-in token — emitted automatically when no_truncation=false
# and lines were dropped. Points to where dropped content begins in the tee file.
match = "__rtk_truncation_point__"
keep = "all"
show_line = false               # just emit the line numbers, no text
```

**Agent-facing output example:**

```
[full output: ~/.local/share/rtk/tee/make-test-1713200000.log]
  first_error  → L47: "FAIL: TestFoo (timeout)"
  test_summary → L112, L118, L203
  truncated_warning → output truncated after L89 (grep_max_results=200); full results from L89
```

This gives the agent precise `Read offset=N` targets with no scanning, and — critically — when
truncation is active it can still find the dropped lines in the raw log via the
`truncated_warning` pointer.

**Implementation sketch:**
- Add `TeeIndexRule { name, match_re, keep: First|Last|All, show_line }` and a
  `Vec<TeeIndexRule>` on `TeeConfig`
- After writing the tee file, iterate rules over the in-memory line buffer (already present),
  collect `(line_no, line_text)` matches per rule
- When `no_truncation = false`, inject a synthetic `__rtk_truncation_point__` sentinel at the
  first line that would have been dropped; rules with that token resolve to a pointer
- Render the index block and append it to the hint string already printed to the agent

No pipeline changes needed — the raw line buffer is in memory at tee-write time.

---

## 4. Document `tee.mode = "always"` as recommended for agent contexts

**Problem:** Default is `tee.mode = "failures"`, which is correct for human use but suboptimal
for AI agents that benefit from always having the raw log available via `Read offset=N`.

**Proposed:** Add a note to the docs / `rtk init` output recommending `tee.mode = "always"` when
used with AI agents, or auto-set it when `rtk init` detects an agent context (e.g.
`--opencode`, `--agent claude`, etc.).

---

## Notes

- rtk is a 2790-line `main.rs` monolith — major architectural refactors are high-effort.
  Prefer small, targeted PRs over big restructures.
- Contribution 4 is low-effort / high-value, a good first PR.
- Contribution 3 is medium-effort — requires a new config struct, index computation, and hint rendering, but no pipeline changes.
- Contributions 1 and 3 are complementary: the `truncated_warning` index rule only makes full sense when paired with `no_truncation` semantics.
