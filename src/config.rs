//! Configuration loaded from an optional committed `.rewind.toml`.
//!
//! ```toml
//! test_command = "npm test"
//! max_file_size = 1048576
//! ignore = ["coverage/", "generated/"]
//! ```

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Result, RewindError};

/// Name of the optional per-repository configuration file.
pub const CONFIG_FILE: &str = ".rewind.toml";

/// Default upper bound on tracked file size, in bytes (1 MiB).
pub const DEFAULT_MAX_FILE_SIZE: u64 = 1_048_576;

/// Default seconds before a test run is cancelled. `0` means no timeout.
pub const DEFAULT_TEST_TIMEOUT_SECS: u64 = 0;

/// Parsed `.rewind.toml`, with defaults applied for missing fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Command run by `rewind test` (e.g. `"npm test"`). Executed via the
    /// user's shell so pipelines and operators work.
    pub test_command: Option<String>,

    /// Files larger than this many bytes are not snapshotted.
    pub max_file_size: u64,

    /// Additional ignore patterns, layered on top of the built-in defaults and
    /// the repository's `.gitignore`.
    pub ignore: Vec<String>,

    /// Seconds after which a running test command is cancelled. `0` disables.
    pub test_timeout_secs: u64,

    /// When true, files matching the built-in secret patterns are tracked
    /// anyway. Off by default; enabling it prints a warning.
    pub track_secrets: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            test_command: None,
            max_file_size: DEFAULT_MAX_FILE_SIZE,
            ignore: Vec::new(),
            test_timeout_secs: DEFAULT_TEST_TIMEOUT_SECS,
            track_secrets: false,
        }
    }
}

impl Config {
    /// Load configuration from `<repo_root>/.rewind.toml`, or return defaults
    /// when the file is absent. Parse errors are surfaced, never ignored.
    pub fn load(repo_root: &Path) -> Result<Config> {
        let path = repo_root.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let cfg: Config = toml::from_str(&text)?;
                cfg.validate()?;
                Ok(cfg)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(RewindError::Io(e)),
        }
    }

    /// Whether a config file exists for this repository.
    pub fn exists(repo_root: &Path) -> bool {
        repo_root.join(CONFIG_FILE).is_file()
    }

    /// Serialize to a TOML document suitable for writing to `.rewind.toml`.
    pub fn to_toml_string(&self) -> Result<String> {
        toml::to_string_pretty(self)
            .map_err(|e| RewindError::Config(format!("failed to serialize config: {e}")))
    }

    /// Write this configuration to `<repo_root>/.rewind.toml`.
    pub fn save(&self, repo_root: &Path) -> Result<()> {
        let path = repo_root.join(CONFIG_FILE);
        std::fs::write(path, self.to_toml_string()?)?;
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.max_file_size == 0 {
            return Err(RewindError::Config(
                "max_file_size must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// Built-in glob patterns for files that should be excluded because they
/// commonly contain secrets. Matching is case-sensitive on the file name and
/// on any path segment.
pub const SECRET_PATTERNS: &[&str] = &[
    ".env",
    ".env.*",
    "*.pem",
    "*.key",
    "*.keystore",
    "*.p12",
    "*.pfx",
    "id_rsa",
    "id_dsa",
    "id_ecdsa",
    "id_ed25519",
    "*.der",
    "credentials",
    ".netrc",
    ".pgpass",
    ".htpasswd",
    "secrets.yml",
    "secrets.yaml",
    "*_rsa",
];

/// Built-in directory names that are never tracked (dependencies, build
/// outputs, virtual environments, caches, and VCS/Rewind internals).
pub const DEFAULT_IGNORED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".rewind",
    ".rewind-report",
    "node_modules",
    "bower_components",
    "vendor",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".env.d",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".idea",
    ".vscode",
    "coverage",
    ".cache",
    ".parcel-cache",
    "DerivedData",
    "Pods",
    ".terraform",
    "elm-stuff",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = Config::default();
        assert_eq!(c.max_file_size, DEFAULT_MAX_FILE_SIZE);
        assert!(c.test_command.is_none());
        assert!(!c.track_secrets);
    }

    #[test]
    fn parses_example() {
        let toml = r#"
            test_command = "npm test"
            max_file_size = 2048
            ignore = ["coverage/", "generated/"]
        "#;
        let c: Config = toml::from_str(toml).unwrap();
        assert_eq!(c.test_command.as_deref(), Some("npm test"));
        assert_eq!(c.max_file_size, 2048);
        assert_eq!(c.ignore.len(), 2);
    }

    #[test]
    fn rejects_unknown_fields() {
        let toml = r#"nope = 1"#;
        assert!(toml::from_str::<Config>(toml).is_err());
    }

    #[test]
    fn roundtrips() {
        let c = Config {
            test_command: Some("cargo test".into()),
            ..Default::default()
        };
        let s = c.to_toml_string().unwrap();
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.test_command, c.test_command);
    }
}
