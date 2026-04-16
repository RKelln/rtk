//! Raw output recovery -- saves unfiltered output to disk on command failure.

use super::constants::RTK_DATA_DIR;
use crate::core::config::Config;
use regex::Regex;
use std::path::PathBuf;

/// Minimum output size to tee (smaller outputs don't need recovery)
const MIN_TEE_SIZE: usize = 500;

/// Default max files to keep in tee directory
const DEFAULT_MAX_FILES: usize = 20;

/// Default max file size (1MB)
const DEFAULT_MAX_FILE_SIZE: usize = 1_048_576;

/// Sanitize a command slug for use in filenames.
/// Replaces non-alphanumeric chars (except underscore/hyphen) with underscore,
/// truncates at 40 chars.
fn sanitize_slug(slug: &str) -> String {
    let sanitized: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 40 {
        sanitized[..40].to_string()
    } else {
        sanitized
    }
}

/// Get the tee directory, respecting config and env overrides.
fn get_tee_dir(config: &Config) -> Option<PathBuf> {
    // Env var override
    if let Ok(dir) = std::env::var("RTK_TEE_DIR") {
        return Some(PathBuf::from(dir));
    }

    // Config override
    if let Some(ref dir) = config.tee.directory {
        return Some(dir.clone());
    }

    // Default: ~/.local/share/rtk/tee/
    dirs::data_local_dir().map(|d| d.join(RTK_DATA_DIR).join("tee"))
}

/// Rotate old tee files: keep only the last `max_files`, delete oldest.
fn cleanup_old_files(dir: &std::path::Path, max_files: usize) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "log"))
        .collect();

    if entries.len() <= max_files {
        return;
    }

    // Sort by filename (which starts with epoch timestamp = chronological)
    entries.sort_by_key(|e| e.file_name());

    let to_remove = entries.len() - max_files;
    for entry in entries.iter().take(to_remove) {
        let _ = std::fs::remove_file(entry.path());
    }
}

/// Check if tee should be skipped based on config, mode, exit code, and size.
/// Returns None if should skip, Some(tee_dir) if should proceed.
fn should_tee(
    config: &TeeConfig,
    raw_len: usize,
    exit_code: i32,
    tee_dir: Option<PathBuf>,
) -> Option<PathBuf> {
    if !config.enabled {
        return None;
    }

    match config.mode {
        TeeMode::Never => return None,
        TeeMode::Failures => {
            if exit_code == 0 {
                return None;
            }
        }
        TeeMode::Always => {}
    }

    if raw_len < MIN_TEE_SIZE {
        return None;
    }

    tee_dir
}

/// Write raw output to a tee file in the given directory.
/// Returns file path on success.
fn write_tee_file(
    raw: &str,
    command_slug: &str,
    tee_dir: &std::path::Path,
    max_file_size: usize,
    max_files: usize,
) -> Option<PathBuf> {
    std::fs::create_dir_all(tee_dir).ok()?;

    let slug = sanitize_slug(command_slug);
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    let filename = format!("{}_{}.log", epoch, slug);
    let filepath = tee_dir.join(filename);

    // Truncate at max_file_size (find a safe UTF-8 char boundary)
    let content = if raw.len() > max_file_size {
        let boundary = raw
            .char_indices()
            .take_while(|(i, _)| *i < max_file_size)
            .last()
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        format!(
            "{}\n\n--- truncated at {} bytes ---",
            &raw[..boundary],
            max_file_size
        )
    } else {
        raw.to_string()
    };

    std::fs::write(&filepath, content).ok()?;

    // Rotate old files
    cleanup_old_files(tee_dir, max_files);

    Some(filepath)
}

/// Write raw output to tee file if conditions are met.
/// Returns file path on success, None if skipped/failed.
pub fn tee_raw(raw: &str, command_slug: &str, exit_code: i32) -> Option<PathBuf> {
    // Check RTK_TEE=0 env override (disable)
    if std::env::var("RTK_TEE").ok().as_deref() == Some("0") {
        return None;
    }

    let config = Config::load().ok()?;
    let tee_dir = get_tee_dir(&config)?;

    let tee_dir = should_tee(&config.tee, raw.len(), exit_code, Some(tee_dir))?;

    write_tee_file(
        raw,
        command_slug,
        &tee_dir,
        config.tee.max_file_size,
        config.tee.max_files,
    )
}

/// Format the hint line with ~ shorthand for home directory.
/// If `ctx` is provided, appends index block with line-number hints.
fn format_hint(path: &std::path::Path, raw: &str, ctx: Option<&TeeHintContext>) -> String {
    let display = if let Some(home) = dirs::home_dir() {
        if let Ok(relative) = path.strip_prefix(&home) {
            format!("~/{}", relative.display())
        } else {
            path.display().to_string()
        }
    } else {
        path.display().to_string()
    };

    let mut hint = format!("[full output: {}]", display);

    if let Some(ctx) = ctx {
        let index = compute_index(raw, ctx.index_rules);
        let block = format_index_block(&index, ctx.truncation_line);
        if !block.is_empty() {
            hint.push('\n');
            hint.push_str(&block);
        }
    }

    hint
}

/// Convenience: tee + format hint in one call.
/// Returns hint string if file was written, None if skipped.
/// Pass `ctx` to include index hints in the output.
pub fn tee_and_hint(
    raw: &str,
    command_slug: &str,
    exit_code: i32,
    ctx: Option<&TeeHintContext>,
) -> Option<String> {
    let path = tee_raw(raw, command_slug, exit_code)?;
    Some(format_hint(&path, raw, ctx))
}

/// Force tee output regardless of exit code (used when filters truncate).
/// Always writes file if size >= MIN_TEE_SIZE and tee is enabled.
/// Returns hint string if file was written, None if skipped/disabled.
///
/// Used by AWS filters when FilterResult.truncated = true, ensuring
/// the LLM has access to full untruncated output via the hint path.
pub fn force_tee_hint(
    raw: &str,
    command_slug: &str,
    ctx: Option<&TeeHintContext>,
) -> Option<String> {
    // Check RTK_TEE=0 env override (disable)
    if std::env::var("RTK_TEE").ok().as_deref() == Some("0") {
        return None;
    }

    // Skip if output too small
    if raw.len() < MIN_TEE_SIZE {
        return None;
    }

    let config = Config::load().ok()?;

    // Respect enabled flag but ignore mode (force tee)
    if !config.tee.enabled {
        return None;
    }

    let tee_dir = get_tee_dir(&config)?;
    let tee_dir = std::fs::create_dir_all(&tee_dir).ok().and(Some(tee_dir))?;

    let path = write_tee_file(
        raw,
        command_slug,
        &tee_dir,
        config.tee.max_file_size,
        config.tee.max_files,
    )?;

    Some(format_hint(&path, raw, ctx))
}

/// TeeMode controls when tee writes files.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum TeeMode {
    #[default]
    Failures,
    Always,
    Never,
}

/// Configuration for the tee feature.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeeConfig {
    pub enabled: bool,
    pub mode: TeeMode,
    pub max_files: usize,
    pub max_file_size: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<PathBuf>,
    /// Global tee index rules — emit line-number hints in tee output.
    #[serde(default)]
    pub index: Vec<TeeIndexRule>,
}

impl Default for TeeConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: TeeMode::default(),
            max_files: DEFAULT_MAX_FILES,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            directory: None,
            index: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tee index — structured hints for agent navigation
// ---------------------------------------------------------------------------

/// User-facing config rule (TOML-deserializable).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TeeIndexRule {
    pub name: String,
    /// Regex pattern to match against raw output lines.
    #[serde(rename = "match")]
    pub match_pattern: String,
    /// Which matches to keep: "first", "last", or "all".
    #[serde(default)]
    pub keep: KeepMode,
    /// Include matched line text in the hint (not just line number).
    #[serde(default)]
    pub show_line: bool,
}

/// Which matched lines to keep in the index result.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum KeepMode {
    #[default]
    First,
    Last,
    All,
}

/// A compiled index rule with pre-compiled regex (avoids per-line recompilation).
#[derive(Debug)]
pub struct CompiledTeeIndexRule {
    pub name: String,
    pub pattern: Regex,
    pub keep: KeepMode,
    pub show_line: bool,
}

impl CompiledTeeIndexRule {
    /// Compile a `TeeIndexRule` into a `CompiledTeeIndexRule`.
    /// Returns `None` if the regex pattern is invalid.
    pub fn compile(rule: &TeeIndexRule) -> Option<Self> {
        match Regex::new(&rule.match_pattern) {
            Ok(pattern) => Some(Self {
                name: rule.name.clone(),
                pattern,
                keep: rule.keep.clone(),
                show_line: rule.show_line,
            }),
            Err(e) => {
                eprintln!(
                    "[rtk] warning: tee index rule '{}': invalid regex: {}",
                    rule.name, e
                );
                None
            }
        }
    }
}

/// Max chars to show for matched line text in index hints.
const INDEX_LINE_TRUNCATE: usize = 80;

/// Truncate a string at a UTF-8-safe boundary, appending "…" if truncated.
fn truncate_utf8(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let boundary = s
        .char_indices()
        .take_while(|(i, _)| *i < max_bytes)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    format!("{}…", &s[..boundary])
}
#[derive(Debug)]
pub struct IndexResult {
    pub name: String,
    /// (1-indexed line number, line text)
    pub matches: Vec<(usize, String)>,
    pub show_line: bool,
}

/// Context passed to tee hint functions for index and truncation info.
#[derive(Debug)]
pub struct TeeHintContext<'a> {
    pub index_rules: &'a [CompiledTeeIndexRule],
    /// If set, indicates output was truncated after this line number.
    pub truncation_line: Option<usize>,
}

/// Compute index results by running rules against raw output.
fn compute_index(raw: &str, rules: &[CompiledTeeIndexRule]) -> Vec<IndexResult> {
    let mut results = Vec::new();
    for rule in rules {
        let kept: Vec<(usize, String)> = match rule.keep {
            KeepMode::First => {
                // Short-circuit: stop after first match
                raw.lines()
                    .enumerate()
                    .find(|(_, line)| rule.pattern.is_match(line))
                    .map(|(i, line)| vec![(i + 1, line.to_string())])
                    .unwrap_or_default()
            }
            KeepMode::Last => {
                // Must scan all lines to find last match
                raw.lines()
                    .enumerate()
                    .filter(|(_, line)| rule.pattern.is_match(line))
                    .last()
                    .map(|(i, line)| vec![(i + 1, line.to_string())])
                    .unwrap_or_default()
            }
            KeepMode::All => raw
                .lines()
                .enumerate()
                .filter(|(_, line)| rule.pattern.is_match(line))
                .map(|(i, line)| (i + 1, line.to_string()))
                .collect(),
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

/// Format index results as hint lines.
fn format_index_block(index: &[IndexResult], truncation_line: Option<usize>) -> String {
    let mut lines = Vec::new();
    for result in index {
        if result.show_line {
            for (line_num, text) in &result.matches {
                // Truncate line text to keep hints concise (UTF-8 safe)
                let display_text = truncate_utf8(text, INDEX_LINE_TRUNCATE);
                lines.push(format!(
                    "  {} → L{}: \"{}\"",
                    result.name, line_num, display_text
                ));
            }
        } else {
            let lnums: Vec<String> = result
                .matches
                .iter()
                .map(|(n, _)| format!("L{}", n))
                .collect();
            lines.push(format!("  {} → {}", result.name, lnums.join(", ")));
        }
    }
    if let Some(trunc_line) = truncation_line {
        lines.push(format!(
            "  truncated → output truncated after L{}; full results from L{} in tee file",
            trunc_line, trunc_line
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_sanitize_slug() {
        assert_eq!(sanitize_slug("cargo_test"), "cargo_test");
        assert_eq!(sanitize_slug("cargo test"), "cargo_test");
        assert_eq!(sanitize_slug("cargo-test"), "cargo-test");
        assert_eq!(sanitize_slug("go/test/./pkg"), "go_test___pkg");
        // Truncate at 40
        let long = "a".repeat(50);
        assert_eq!(sanitize_slug(&long).len(), 40);
    }

    #[test]
    fn test_should_tee_disabled() {
        let config = TeeConfig {
            enabled: false,
            ..TeeConfig::default()
        };
        let dir = PathBuf::from("/tmp/tee");
        assert!(should_tee(&config, 1000, 1, Some(dir)).is_none());
    }

    #[test]
    fn test_should_tee_never_mode() {
        let config = TeeConfig {
            mode: TeeMode::Never,
            ..TeeConfig::default()
        };
        let dir = PathBuf::from("/tmp/tee");
        assert!(should_tee(&config, 1000, 1, Some(dir)).is_none());
    }

    #[test]
    fn test_should_tee_skip_small_output() {
        let config = TeeConfig::default();
        let dir = PathBuf::from("/tmp/tee");
        // Below MIN_TEE_SIZE (500)
        assert!(should_tee(&config, 100, 1, Some(dir)).is_none());
    }

    #[test]
    fn test_should_tee_skip_success_in_failures_mode() {
        let config = TeeConfig::default(); // mode = Failures
        let dir = PathBuf::from("/tmp/tee");
        assert!(should_tee(&config, 1000, 0, Some(dir)).is_none());
    }

    #[test]
    fn test_should_tee_proceed_on_failure() {
        let config = TeeConfig::default(); // mode = Failures
        let dir = PathBuf::from("/tmp/tee");
        assert!(should_tee(&config, 1000, 1, Some(dir)).is_some());
    }

    #[test]
    fn test_should_tee_always_mode_success() {
        let config = TeeConfig {
            mode: TeeMode::Always,
            ..TeeConfig::default()
        };
        let dir = PathBuf::from("/tmp/tee");
        assert!(should_tee(&config, 1000, 0, Some(dir)).is_some());
    }

    #[test]
    fn test_write_tee_file_creates_file() {
        let tmpdir = tempfile::tempdir().unwrap();
        let content = "error: test failed\n".repeat(50);
        let result = write_tee_file(
            &content,
            "cargo_test",
            tmpdir.path(),
            DEFAULT_MAX_FILE_SIZE,
            20,
        );
        assert!(result.is_some());

        let path = result.unwrap();
        assert!(path.exists());
        let written = fs::read_to_string(&path).unwrap();
        assert!(written.contains("error: test failed"));
    }

    #[test]
    fn test_write_tee_file_truncation() {
        let tmpdir = tempfile::tempdir().unwrap();
        let big_output = "x".repeat(2000);
        // Set max_file_size to 1000 bytes
        let result = write_tee_file(&big_output, "test", tmpdir.path(), 1000, 20);
        assert!(result.is_some());

        let path = result.unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("--- truncated at 1000 bytes ---"));
        assert!(content.len() < 2000);
    }

    #[test]
    fn test_write_tee_file_truncation_utf8_boundary() {
        let tmpdir = tempfile::tempdir().unwrap();
        // Create a string where the truncation point falls inside a multi-byte char.
        // Japanese chars are 3 bytes each in UTF-8.
        // 332 chars * 3 bytes = 996 bytes, then one more = 999 bytes.
        // With max_file_size=998, the cut falls mid-character.
        let japanese = "\u{6F22}".repeat(333); // 999 bytes of 3-byte chars
        assert_eq!(japanese.len(), 999);

        // Truncate at 998 — falls in the middle of the 333rd character
        let result = write_tee_file(&japanese, "test_utf8", tmpdir.path(), 998, 20);
        assert!(result.is_some());

        let path = result.unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("--- truncated at 998 bytes ---"));
        // Should contain 332 full characters (996 bytes), not panic
        assert!(content.starts_with(&"\u{6F22}".repeat(332)));
    }

    #[test]
    fn test_write_tee_file_truncation_emoji() {
        let tmpdir = tempfile::tempdir().unwrap();
        // Emoji are 4 bytes each in UTF-8
        let emojis = "\u{1F600}".repeat(100); // 400 bytes
        assert_eq!(emojis.len(), 400);

        // Truncate at 201 — falls mid-emoji (4-byte boundary is at 200, 204)
        let result = write_tee_file(&emojis, "test_emoji", tmpdir.path(), 201, 20);
        assert!(result.is_some());

        let path = result.unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("--- truncated at 201 bytes ---"));
        // The emoji portion should be exactly 200 bytes (50 emojis),
        // rounded down from 201 to the nearest char boundary
        let target = "\u{1F600}".repeat(50);
        assert!(content.starts_with(&target));
    }

    #[test]
    fn test_cleanup_old_files() {
        let tmpdir = tempfile::tempdir().unwrap();
        let dir = tmpdir.path();

        // Create 25 .log files
        for i in 0..25 {
            let filename = format!("{:010}_{}.log", 1000000 + i, "test");
            fs::write(dir.join(&filename), "content").unwrap();
        }

        cleanup_old_files(dir, 20);

        let remaining: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(remaining.len(), 20);

        // Oldest 5 should be removed
        for i in 0..5 {
            let filename = format!("{:010}_{}.log", 1000000 + i, "test");
            assert!(!dir.join(&filename).exists());
        }
        // Newest 20 should remain
        for i in 5..25 {
            let filename = format!("{:010}_{}.log", 1000000 + i, "test");
            assert!(dir.join(&filename).exists());
        }
    }

    #[test]
    fn test_format_hint() {
        let path = PathBuf::from("/tmp/rtk/tee/123_cargo_test.log");
        let hint = format_hint(&path, "", None);
        assert!(hint.starts_with("[full output: "));
        assert!(hint.ends_with(']'));
        assert!(hint.contains("123_cargo_test.log"));
    }

    #[test]
    fn test_tee_config_default() {
        let config = TeeConfig::default();
        assert!(config.enabled);
        assert_eq!(config.mode, TeeMode::Failures);
        assert_eq!(config.max_files, 20);
        assert_eq!(config.max_file_size, 1_048_576);
        assert!(config.directory.is_none());
    }

    #[test]
    fn test_tee_config_deserialize() {
        let toml_str = r#"
enabled = true
mode = "always"
max_files = 10
max_file_size = 524288
directory = "/tmp/rtk-tee"
"#;
        let config: TeeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.mode, TeeMode::Always);
        assert_eq!(config.max_files, 10);
        assert_eq!(config.max_file_size, 524288);
        assert_eq!(config.directory, Some(PathBuf::from("/tmp/rtk-tee")));

        // Round-trip
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: TeeConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.mode, TeeMode::Always);
        assert_eq!(deserialized.max_files, 10);
    }

    #[test]
    fn test_tee_mode_serde() {
        // Test all modes via JSON
        let mode: TeeMode = serde_json::from_str(r#""always""#).unwrap();
        assert_eq!(mode, TeeMode::Always);

        let mode: TeeMode = serde_json::from_str(r#""failures""#).unwrap();
        assert_eq!(mode, TeeMode::Failures);

        let mode: TeeMode = serde_json::from_str(r#""never""#).unwrap();
        assert_eq!(mode, TeeMode::Never);
    }

    #[test]
    fn test_force_tee_hint_skip_small_output() {
        // force_tee_hint should respect MIN_TEE_SIZE
        let small_output = "short error";
        let hint = force_tee_hint(small_output, "test_cmd", None);
        assert!(hint.is_none(), "Should skip output < MIN_TEE_SIZE");
    }

    #[test]
    fn test_force_tee_hint_respects_env_disable() {
        // When RTK_TEE=0, force_tee_hint should return None
        std::env::set_var("RTK_TEE", "0");
        let large_output = "x".repeat(1000);
        let hint = force_tee_hint(&large_output, "test_cmd", None);
        std::env::remove_var("RTK_TEE");
        assert!(hint.is_none(), "Should respect RTK_TEE=0");
    }

    // -----------------------------------------------------------------------
    // Tee index tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_keep_mode_default_is_first() {
        let mode = KeepMode::default();
        assert_eq!(mode, KeepMode::First);
    }

    #[test]
    fn test_tee_index_rule_deserialize() {
        let toml_str = r#"
name = "first_error"
match = "error|^FAIL|^fatal"
keep = "first"
show_line = true
"#;
        let rule: TeeIndexRule = toml::from_str(toml_str).unwrap();
        assert_eq!(rule.name, "first_error");
        assert_eq!(rule.match_pattern, "error|^FAIL|^fatal");
        assert_eq!(rule.keep, KeepMode::First);
        assert!(rule.show_line);
    }

    #[test]
    fn test_tee_index_rule_defaults() {
        let toml_str = r#"
name = "summary"
match = "^test result"
"#;
        let rule: TeeIndexRule = toml::from_str(toml_str).unwrap();
        assert_eq!(rule.keep, KeepMode::First);
        assert!(!rule.show_line);
    }

    #[test]
    fn test_tee_config_with_index_rules() {
        let toml_str = r#"
enabled = true
mode = "always"
max_files = 20
max_file_size = 1048576

[[index]]
name = "first_error"
match = "error|^FAIL"
keep = "first"
show_line = true

[[index]]
name = "test_summary"
match = "^(ok|FAIL|---)"
keep = "all"
show_line = true
"#;
        let config: TeeConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.index.len(), 2);
        assert_eq!(config.index[0].name, "first_error");
        assert_eq!(config.index[1].keep, KeepMode::All);
    }

    #[test]
    fn test_tee_config_without_index_backward_compat() {
        let toml_str = r#"
enabled = true
mode = "failures"
max_files = 20
max_file_size = 1048576
"#;
        let config: TeeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.index.is_empty());
    }

    #[test]
    fn test_compiled_tee_index_rule_valid() {
        let rule = TeeIndexRule {
            name: "errors".into(),
            match_pattern: r"error|^FAIL".into(),
            keep: KeepMode::First,
            show_line: true,
        };
        let compiled = CompiledTeeIndexRule::compile(&rule);
        assert!(compiled.is_some());
        let compiled = compiled.unwrap();
        assert!(compiled.pattern.is_match("error: something"));
        assert!(compiled.pattern.is_match("FAIL test_foo"));
        assert!(!compiled.pattern.is_match("ok: passed"));
    }

    #[test]
    fn test_compiled_tee_index_rule_invalid_regex() {
        let rule = TeeIndexRule {
            name: "bad".into(),
            match_pattern: r"[invalid".into(),
            keep: KeepMode::First,
            show_line: false,
        };
        assert!(CompiledTeeIndexRule::compile(&rule).is_none());
    }

    #[test]
    fn test_compute_index_first() {
        let raw = "line 1 ok\nerror: bad thing\nline 3 ok\nerror: another";
        let rule = CompiledTeeIndexRule {
            name: "first_error".into(),
            pattern: Regex::new("error").unwrap(),
            keep: KeepMode::First,
            show_line: true,
        };
        let results = compute_index(raw, &[rule]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].matches.len(), 1);
        assert_eq!(results[0].matches[0].0, 2); // 1-indexed
        assert!(results[0].matches[0].1.contains("bad thing"));
    }

    #[test]
    fn test_compute_index_last() {
        let raw = "error: first\nok\nerror: last";
        let rule = CompiledTeeIndexRule {
            name: "last_error".into(),
            pattern: Regex::new("error").unwrap(),
            keep: KeepMode::Last,
            show_line: true,
        };
        let results = compute_index(raw, &[rule]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].matches.len(), 1);
        assert_eq!(results[0].matches[0].0, 3);
        assert!(results[0].matches[0].1.contains("last"));
    }

    #[test]
    fn test_compute_index_all() {
        let raw = "error: a\nok\nerror: b\nerror: c";
        let rule = CompiledTeeIndexRule {
            name: "all_errors".into(),
            pattern: Regex::new("error").unwrap(),
            keep: KeepMode::All,
            show_line: false,
        };
        let results = compute_index(raw, &[rule]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].matches.len(), 3);
        assert_eq!(results[0].matches[0].0, 1);
        assert_eq!(results[0].matches[1].0, 3);
        assert_eq!(results[0].matches[2].0, 4);
    }

    #[test]
    fn test_compute_index_no_matches() {
        let raw = "all good\neverything fine";
        let rule = CompiledTeeIndexRule {
            name: "errors".into(),
            pattern: Regex::new("error").unwrap(),
            keep: KeepMode::First,
            show_line: true,
        };
        let results = compute_index(raw, &[rule]);
        assert!(results.is_empty());
    }

    #[test]
    fn test_compute_index_multiple_rules() {
        let raw = "error: broken\nwarning: deprecated\ntest result: ok";
        let rules = vec![
            CompiledTeeIndexRule {
                name: "errors".into(),
                pattern: Regex::new("error").unwrap(),
                keep: KeepMode::First,
                show_line: true,
            },
            CompiledTeeIndexRule {
                name: "summary".into(),
                pattern: Regex::new("^test result").unwrap(),
                keep: KeepMode::Last,
                show_line: true,
            },
        ];
        let results = compute_index(raw, &rules);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].name, "errors");
        assert_eq!(results[1].name, "summary");
    }

    #[test]
    fn test_format_index_block_with_show_line() {
        let index = vec![IndexResult {
            name: "first_error".into(),
            matches: vec![(47, "FAIL: TestFoo (timeout)".into())],
            show_line: true,
        }];
        let block = format_index_block(&index, None);
        assert_eq!(block, "  first_error → L47: \"FAIL: TestFoo (timeout)\"");
    }

    #[test]
    fn test_format_index_block_without_show_line() {
        let index = vec![IndexResult {
            name: "summary".into(),
            matches: vec![(112, "".into()), (118, "".into()), (203, "".into())],
            show_line: false,
        }];
        let block = format_index_block(&index, None);
        assert_eq!(block, "  summary → L112, L118, L203");
    }

    #[test]
    fn test_format_index_block_with_truncation() {
        let block = format_index_block(&[], Some(89));
        assert!(block.contains("truncated → output truncated after L89"));
        assert!(block.contains("full results from L89 in tee file"));
    }

    #[test]
    fn test_format_index_block_combined() {
        let index = vec![IndexResult {
            name: "first_error".into(),
            matches: vec![(47, "FAIL: TestFoo".into())],
            show_line: true,
        }];
        let block = format_index_block(&index, Some(89));
        assert!(block.contains("first_error → L47"));
        assert!(block.contains("truncated → output truncated after L89"));
    }

    #[test]
    fn test_format_index_block_empty() {
        let block = format_index_block(&[], None);
        assert!(block.is_empty());
    }

    #[test]
    fn test_truncate_utf8_ascii() {
        let short = "hello";
        assert_eq!(truncate_utf8(short, 80), "hello");
        let long = "x".repeat(100);
        let result = truncate_utf8(&long, 80);
        assert!(result.ends_with('…'));
        assert!(result.len() <= 84); // 80 + 3 bytes for …
    }

    #[test]
    fn test_truncate_utf8_multibyte() {
        // Japanese chars are 3 bytes each
        let japanese = "\u{6F22}".repeat(30); // 90 bytes
        let result = truncate_utf8(&japanese, 80);
        assert!(result.ends_with('…'));
        // Should not panic — must truncate at char boundary
        assert!(result.is_char_boundary(result.len() - '…'.len_utf8()));
    }

    #[test]
    fn test_format_index_block_long_line_truncated() {
        let long_text = format!("error: {}", "x".repeat(100));
        let index = vec![IndexResult {
            name: "err".into(),
            matches: vec![(1, long_text)],
            show_line: true,
        }];
        let block = format_index_block(&index, None);
        assert!(block.contains('…'));
    }
}
