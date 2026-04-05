//! Configuration parsing for jj-prek.
//!
//! Supports the PREK config format (`prek.toml` / `.pre-commit-config.yaml` compatible TOML)
//! with adaptations for jj's model:
//!   - No staging-related concepts
//!   - Stages map to jj operations (pre-commit equivalent only for now)
//!
//! Config file discovery: `jj-prek.toml` > `prek.toml`

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fancy_regex::Regex;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;

/// The config file name priority order.
pub(crate) const CONFIG_FILENAMES: &[&str] = &["jj-prek.toml", "prek.toml"];

/// Discover the config file in the given directory.
pub(crate) fn find_config(dir: &Path) -> Option<PathBuf> {
    for name in CONFIG_FILENAMES {
        let path = dir.join(name);
        if path.is_file() {
            return Some(path);
        }
    }
    None
}

/// Read and parse a config file.
pub(crate) fn read_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path.display()))?;
    let config: Config = toml::from_str(&content)
        .with_context(|| format!("Failed to parse config file: {}", path.display()))?;
    Ok(config)
}

/// Top-level configuration.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Config {
    /// Hook repositories / sources.
    pub repos: Vec<Repo>,

    /// Global file include pattern.
    #[serde(default)]
    pub files: Option<FilePattern>,

    /// Global file exclude pattern.
    #[serde(default)]
    pub exclude: Option<FilePattern>,

    /// Stop running hooks after the first failure.
    #[serde(default)]
    pub fail_fast: Option<bool>,

    /// Minimum jj-prek version required.
    #[serde(default)]
    pub minimum_jj_prek_version: Option<String>,
}

/// A repository source for hooks.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "repo")]
pub(crate) enum Repo {
    /// Local hooks defined inline.
    #[serde(rename = "local")]
    Local { hooks: Vec<LocalHook> },
}

/// A locally defined hook.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LocalHook {
    /// Unique identifier for this hook.
    pub id: String,

    /// Human-readable name for display.
    pub name: String,

    /// The command to run.
    pub entry: String,

    /// Language / runtime for the hook.
    #[serde(default = "default_language")]
    pub language: Language,

    /// Execution priority (lower runs first).
    #[serde(default)]
    pub priority: Option<u32>,

    /// File include pattern.
    #[serde(default)]
    pub files: Option<FilePattern>,

    /// File exclude pattern.
    #[serde(default)]
    pub exclude: Option<FilePattern>,

    /// File type tags (OR filter).
    #[serde(default)]
    pub types_or: Option<Vec<String>>,

    /// File type tags (AND filter).
    #[serde(default)]
    pub types: Option<Vec<String>>,

    /// File type exclusion tags.
    #[serde(default)]
    pub exclude_types: Option<Vec<String>>,

    /// Extra arguments to pass to the hook command.
    #[serde(default)]
    pub args: Option<Vec<String>>,

    /// Whether to always run even if no matching files.
    #[serde(default)]
    pub always_run: Option<bool>,

    /// Whether to stop on first failure (overrides global).
    #[serde(default)]
    pub fail_fast: Option<bool>,

    /// Whether to pass filenames to the hook command.
    #[serde(default = "default_pass_filenames")]
    pub pass_filenames: bool,

    /// Description of what this hook does.
    #[serde(default)]
    pub description: Option<String>,

    /// Whether this hook must run serially (not in parallel).
    #[serde(default)]
    pub require_serial: Option<bool>,

    /// Whether to show verbose output.
    #[serde(default)]
    pub verbose: Option<bool>,

    /// Additional environment variables.
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,

    /// Catch-all for unknown keys (forward compatibility).
    #[serde(flatten)]
    pub _extra: BTreeMap<String, toml::Value>,
}

fn default_language() -> Language {
    Language::System
}

fn default_pass_filenames() -> bool {
    true
}

/// Supported hook languages/runtimes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Language {
    /// Use whatever is on the system PATH.
    System,
    /// A script to run directly.
    Script,
    /// Explicitly fail with a message.
    Fail,
    /// Python hook.
    Python,
    /// Node.js hook.
    Node,
    /// Rust hook.
    Rust,
    /// Ruby hook.
    Ruby,
    /// Go hook.
    Go,
}

impl Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System => write!(f, "system"),
            Self::Script => write!(f, "script"),
            Self::Fail => write!(f, "fail"),
            Self::Python => write!(f, "python"),
            Self::Node => write!(f, "node"),
            Self::Rust => write!(f, "rust"),
            Self::Ruby => write!(f, "ruby"),
            Self::Go => write!(f, "go"),
        }
    }
}

/// A file matching pattern — either a regex or a glob.
#[derive(Debug, Clone)]
pub(crate) enum FilePattern {
    Regex(Regex),
    Glob(GlobPatterns),
}

impl FilePattern {
    pub(crate) fn is_match(&self, path: &str) -> bool {
        match self {
            Self::Regex(regex) => regex.is_match(path).unwrap_or(false),
            Self::Glob(globs) => globs.is_match(path),
        }
    }
}

/// Deserialize a `FilePattern` from either a plain string (regex) or a
/// `{ glob = "..." }` / `{ glob = ["...", "..."] }` table.
impl<'de> Deserialize<'de> for FilePattern {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            String(String),
            GlobTable { glob: GlobWire },
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum GlobWire {
            Single(String),
            List(Vec<String>),
        }

        match Wire::deserialize(deserializer)? {
            Wire::String(s) => {
                let regex = Regex::new(&s).map_err(serde::de::Error::custom)?;
                Ok(FilePattern::Regex(regex))
            }
            Wire::GlobTable { glob } => {
                let patterns = match glob {
                    GlobWire::Single(s) => vec![s],
                    GlobWire::List(list) => list,
                };
                let globs = GlobPatterns::new(patterns).map_err(serde::de::Error::custom)?;
                Ok(FilePattern::Glob(globs))
            }
        }
    }
}

/// A compiled set of glob patterns.
#[derive(Clone)]
pub(crate) struct GlobPatterns {
    patterns: Vec<String>,
    set: GlobSet,
}

impl GlobPatterns {
    pub(crate) fn new(patterns: Vec<String>) -> Result<Self, globset::Error> {
        let mut builder = GlobSetBuilder::new();
        for pattern in &patterns {
            builder.add(Glob::new(pattern)?);
        }
        let set = builder.build()?;
        Ok(Self { patterns, set })
    }

    fn is_match(&self, value: &str) -> bool {
        self.set.is_match(Path::new(value))
    }
}

impl std::fmt::Debug for GlobPatterns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GlobPatterns")
            .field("patterns", &self.patterns)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[[repos]]
repo = "local"

[[repos.hooks]]
id = "cargo-fmt"
name = "cargo fmt"
entry = "cargo fmt --"
language = "system"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.repos.len(), 1);
        match &config.repos[0] {
            Repo::Local { hooks } => {
                assert_eq!(hooks.len(), 1);
                assert_eq!(hooks[0].id, "cargo-fmt");
            }
        }
    }

    #[test]
    fn parse_file_pattern_regex() {
        let toml = r#"
[[repos]]
repo = "local"

[[repos.hooks]]
id = "test"
name = "test"
entry = "echo"
files = "^src/"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        match &config.repos[0] {
            Repo::Local { hooks } => {
                let pat = hooks[0].files.as_ref().unwrap();
                assert!(pat.is_match("src/main.rs"));
                assert!(!pat.is_match("tests/test.rs"));
            }
        }
    }

    #[test]
    fn parse_file_pattern_glob() {
        let toml = r#"
[[repos]]
repo = "local"

[[repos.hooks]]
id = "test"
name = "test"
entry = "echo"

[repos.hooks.files]
glob = ["*.rs", "*.toml"]
"#;
        let config: Config = toml::from_str(toml).unwrap();
        match &config.repos[0] {
            Repo::Local { hooks } => {
                let pat = hooks[0].files.as_ref().unwrap();
                assert!(pat.is_match("main.rs"));
                assert!(pat.is_match("Cargo.toml"));
                assert!(!pat.is_match("README.md"));
            }
        }
    }
}
