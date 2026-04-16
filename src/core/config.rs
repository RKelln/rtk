//! Reads user settings from config.toml.

use super::constants::{CONFIG_TOML, DEFAULT_HISTORY_DAYS, RTK_DATA_DIR};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

/// Process-scoped cached config. Loaded once on first access, avoids repeated disk I/O.
static CACHED_CONFIG: OnceLock<Config> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub tracking: TrackingConfig,
    #[serde(default)]
    pub display: DisplayConfig,
    #[serde(default)]
    pub filters: FilterConfig,
    #[serde(default)]
    pub tee: crate::core::tee::TeeConfig,
    #[serde(default)]
    pub telemetry: TelemetryConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
    #[serde(default)]
    pub limits: LimitsConfig,
    #[serde(default)]
    pub safety: SafetyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SafetyConfig {
    /// When true, disables all lossy truncation (line caps, result limits).
    /// Lossless operations (ANSI strip, dedup, reformat) are preserved.
    #[serde(default)]
    pub no_truncation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HooksConfig {
    /// Commands to exclude from auto-rewrite (e.g. ["curl", "playwright"]).
    /// Survives `rtk init -g` re-runs since config.toml is user-owned.
    #[serde(default)]
    pub exclude_commands: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackingConfig {
    pub enabled: bool,
    pub history_days: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_path: Option<PathBuf>,
}

impl Default for TrackingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            history_days: DEFAULT_HISTORY_DAYS as u32,
            database_path: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayConfig {
    pub colors: bool,
    pub emoji: bool,
    pub max_width: usize,
}

impl Default for DisplayConfig {
    fn default() -> Self {
        Self {
            colors: true,
            emoji: true,
            max_width: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilterConfig {
    pub ignore_dirs: Vec<String>,
    pub ignore_files: Vec<String>,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            ignore_dirs: vec![
                ".git".into(),
                "node_modules".into(),
                "target".into(),
                "__pycache__".into(),
                ".venv".into(),
                "vendor".into(),
            ],
            ignore_files: vec!["*.lock".into(), "*.min.js".into(), "*.min.css".into()],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelemetryConfig {
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_given: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consent_date: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LimitsConfig {
    /// Max total grep results to show (default: 200)
    pub grep_max_results: usize,
    /// Max matches per file in grep output (default: 25)
    pub grep_max_per_file: usize,
    /// Max staged/modified files shown in git status (default: 15)
    pub status_max_files: usize,
    /// Max untracked files shown in git status (default: 10)
    pub status_max_untracked: usize,
    /// Max chars for parser passthrough fallback (default: 2000)
    pub passthrough_max_chars: usize,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            grep_max_results: 200,
            grep_max_per_file: 25,
            status_max_files: 15,
            status_max_untracked: 10,
            passthrough_max_chars: 2000,
        }
    }
}

/// Get the cached config (loaded once per process). Falls back to defaults.
fn cached_config() -> &'static Config {
    CACHED_CONFIG.get_or_init(|| Config::load().unwrap_or_default())
}

/// Get limits config (cached per process).
/// Returns a `&'static` reference — callers access fields directly via auto-deref.
pub fn limits() -> &'static LimitsConfig {
    &cached_config().limits
}

/// Check if no_truncation safety flag is enabled (cached). Falls back to false.
pub fn no_truncation() -> bool {
    cached_config().safety.no_truncation
}

/// Compute effective passthrough limit from components (testable without OnceLock).
fn compute_passthrough_limit(no_trunc: bool, max_chars: usize) -> usize {
    if no_trunc {
        usize::MAX
    } else {
        max_chars
    }
}

/// Effective passthrough char limit: usize::MAX when no_truncation, else configured limit.
pub fn passthrough_limit() -> usize {
    compute_passthrough_limit(no_truncation(), limits().passthrough_max_chars)
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = get_config_path()?;

        if path.exists() {
            let content = std::fs::read_to_string(&path)?;
            let config: Config = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Config::default())
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = get_config_path()?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }

    pub fn create_default() -> Result<PathBuf> {
        let config = Config::default();
        config.save()?;
        get_config_path()
    }
}

fn get_config_path() -> Result<PathBuf> {
    let config_dir = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
    Ok(config_dir.join(RTK_DATA_DIR).join(CONFIG_TOML))
}

pub fn show_config() -> Result<()> {
    let path = get_config_path()?;
    println!("Config: {}", path.display());
    println!();

    if path.exists() {
        let config = Config::load()?;
        println!("{}", toml::to_string_pretty(&config)?);
    } else {
        println!("(default config, file not created)");
        println!();
        let config = Config::default();
        println!("{}", toml::to_string_pretty(&config)?);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hooks_config_deserialize() {
        let toml = r#"
[hooks]
exclude_commands = ["curl", "gh"]
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert_eq!(config.hooks.exclude_commands, vec!["curl", "gh"]);
    }

    #[test]
    fn test_hooks_config_default_empty() {
        let config = Config::default();
        assert!(config.hooks.exclude_commands.is_empty());
    }

    #[test]
    fn test_config_without_hooks_section_is_valid() {
        let toml = r#"
[tracking]
enabled = true
history_days = 90
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert!(config.hooks.exclude_commands.is_empty());
    }

    #[test]
    fn test_old_toml_without_consent_fields() {
        let toml = r#"
[telemetry]
enabled = true
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert!(config.telemetry.enabled);
        assert!(config.telemetry.consent_given.is_none());
        assert!(config.telemetry.consent_date.is_none());
    }

    #[test]
    fn test_telemetry_default_disabled() {
        let config = Config::default();
        assert!(!config.telemetry.enabled);
        assert!(config.telemetry.consent_given.is_none());
    }

    #[test]
    fn test_safety_config_default() {
        let config = Config::default();
        assert!(!config.safety.no_truncation);
    }

    #[test]
    fn test_safety_config_deserialize_true() {
        let toml = r#"
[safety]
no_truncation = true
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert!(config.safety.no_truncation);
    }

    #[test]
    fn test_safety_config_deserialize_false() {
        let toml = r#"
[safety]
no_truncation = false
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert!(!config.safety.no_truncation);
    }

    #[test]
    fn test_config_without_safety_section_is_valid() {
        let toml = r#"
[tracking]
enabled = true
history_days = 90
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert!(!config.safety.no_truncation);
    }

    #[test]
    fn test_safety_config_default_no_truncation_is_false() {
        let safety = SafetyConfig::default();
        assert!(!safety.no_truncation);
    }

    #[test]
    fn test_passthrough_limit_default() {
        let limits = LimitsConfig::default();
        assert_eq!(
            compute_passthrough_limit(false, limits.passthrough_max_chars),
            2000
        );
    }

    #[test]
    fn test_passthrough_limit_no_truncation() {
        assert_eq!(
            compute_passthrough_limit(true, LimitsConfig::default().passthrough_max_chars),
            usize::MAX
        );
    }

    #[test]
    fn test_limits_config_partial_eq() {
        let a = LimitsConfig::default();
        let b = LimitsConfig::default();
        assert_eq!(a, b);

        let c = LimitsConfig {
            grep_max_results: 999,
            ..LimitsConfig::default()
        };
        assert_ne!(a, c);
    }

    #[test]
    fn test_telemetry_consent_roundtrip() {
        let toml = r#"
[telemetry]
enabled = true
consent_given = true
consent_date = "2026-04-10T12:00:00Z"
"#;
        let config: Config = toml::from_str(toml).expect("valid toml");
        assert_eq!(config.telemetry.consent_given, Some(true));
        assert_eq!(
            config.telemetry.consent_date.as_deref(),
            Some("2026-04-10T12:00:00Z")
        );
    }
}
