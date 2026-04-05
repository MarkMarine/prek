//! Hook model and execution.
//!
//! Follows the PREK hook abstraction: a `Hook` is an atomic unit of work that
//! runs a command against a set of filtered files.

use std::fmt::Display;
use std::path::{Path, PathBuf};

use anyhow::Result;
use prek_identify::TagSet;
use tracing::{debug, instrument};

use crate::config::{FilePattern, Language, LocalHook};
use crate::filter;
use crate::process::Cmd;

/// A fully resolved hook ready for execution.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Hook {
    /// Unique identifier.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// Command entry point.
    pub entry: String,
    /// Language/runtime.
    pub language: Language,
    /// Execution priority (lower runs first, default 0).
    pub priority: u32,
    /// File include pattern.
    pub files: Option<FilePattern>,
    /// File exclude pattern.
    pub exclude: Option<FilePattern>,
    /// File type OR filter (at least one must match).
    pub types_or: Option<TagSet>,
    /// File type AND filter (all must match).
    pub types: Option<TagSet>,
    /// File type exclusion filter (none may match).
    pub exclude_types: Option<TagSet>,
    /// Extra arguments.
    pub args: Vec<String>,
    /// Always run even with no files.
    pub always_run: bool,
    /// Fail fast (overrides global).
    pub fail_fast: Option<bool>,
    /// Pass filenames to the command.
    pub pass_filenames: bool,
    /// Description.
    pub description: Option<String>,
    /// Must run serially.
    pub require_serial: bool,
    /// Verbose output.
    pub verbose: bool,
    /// Additional environment variables.
    pub env: Vec<(String, String)>,
}

impl Hook {
    /// Build a `Hook` from a `LocalHook` config definition.
    pub fn from_local(hook: &LocalHook) -> Self {
        Self {
            id: hook.id.clone(),
            name: hook.name.clone(),
            entry: hook.entry.clone(),
            language: hook.language,
            priority: hook.priority.unwrap_or(0),
            files: hook.files.clone(),
            exclude: hook.exclude.clone(),
            types_or: hook
                .types_or
                .as_ref()
                .map(|v| filter::tags_from_strings(v)),
            types: hook.types.as_ref().map(|v| filter::tags_from_strings(v)),
            exclude_types: hook
                .exclude_types
                .as_ref()
                .map(|v| filter::tags_from_strings(v)),
            args: hook.args.clone().unwrap_or_default(),
            always_run: hook.always_run.unwrap_or(false),
            fail_fast: hook.fail_fast,
            pass_filenames: hook.pass_filenames,
            description: hook.description.clone(),
            require_serial: hook.require_serial.unwrap_or(false),
            verbose: hook.verbose.unwrap_or(false),
            env: hook
                .env
                .as_ref()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
        }
    }

    /// Filter a set of file paths down to those this hook should run on.
    pub fn filter_files<'a>(&self, files: &'a [PathBuf]) -> Vec<&'a Path> {
        filter::filter_files(
            files,
            self.files.as_ref(),
            self.exclude.as_ref(),
            self.types_or.as_ref(),
            self.types.as_ref(),
            self.exclude_types.as_ref(),
        )
    }

    /// Execute this hook against a set of files.
    ///
    /// Returns `(exit_code, combined_output)`.
    #[instrument(level = "debug", skip(self, files), fields(hook = %self.id))]
    pub async fn run(&self, files: &[&Path]) -> Result<HookResult> {
        match self.language {
            Language::Fail => {
                // "fail" language: always fail with the entry as the message
                Ok(HookResult {
                    hook_id: self.id.clone(),
                    status: HookStatus::Failed,
                    output: self.entry.clone().into_bytes(),
                    files_count: files.len(),
                })
            }
            Language::System
            | Language::Script
            | Language::Python
            | Language::Node
            | Language::Rust
            | Language::Ruby
            | Language::Go => self.run_system(files).await,
        }
    }

    /// Run a system/script hook by shelling out.
    async fn run_system(&self, files: &[&Path]) -> Result<HookResult> {
        let parts = shlex_entry(&self.entry);
        let (program, base_args) = parts
            .split_first()
            .ok_or_else(|| anyhow::anyhow!("Hook `{}` has empty entry", self.id))?;

        let mut cmd = Cmd::new(program, format!("hook: {}", self.id));

        // Set additional environment variables
        for (key, val) in &self.env {
            cmd.env(key, val);
        }

        // Base arguments from the entry
        cmd.args(base_args);

        // Extra arguments from config
        cmd.args(&self.args);

        // Pass filenames if configured
        if self.pass_filenames && !files.is_empty() {
            cmd.args(files);
        }

        cmd.check(false);

        debug!("Running hook `{}` on {} file(s)", self.id, files.len());

        let output = cmd.output().await?;

        let status = if output.status.success() {
            HookStatus::Passed
        } else {
            HookStatus::Failed
        };

        let mut combined = output.stdout;
        if !output.stderr.is_empty() {
            if !combined.is_empty() {
                combined.push(b'\n');
            }
            combined.extend_from_slice(&output.stderr);
        }

        Ok(HookResult {
            hook_id: self.id.clone(),
            status,
            output: combined,
            files_count: files.len(),
        })
    }
}

impl Display for Hook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

/// Split a command entry into program + args, handling shell quoting.
fn shlex_entry(entry: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut escape_next = false;

    for ch in entry.chars() {
        if escape_next {
            current.push(ch);
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if !in_single_quote => {
                escape_next = true;
            }
            '\'' if !in_double_quote => {
                in_single_quote = !in_single_quote;
            }
            '"' if !in_single_quote => {
                in_double_quote = !in_double_quote;
            }
            ' ' | '\t' if !in_single_quote && !in_double_quote => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => {
                current.push(ch);
            }
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

/// Result of running a single hook.
#[derive(Debug)]
pub(crate) struct HookResult {
    pub hook_id: String,
    pub status: HookStatus,
    pub output: Vec<u8>,
    pub files_count: usize,
}

/// The outcome of a hook execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookStatus {
    /// Hook passed (exit code 0).
    Passed,
    /// Hook failed (non-zero exit code).
    Failed,
    /// Hook was skipped (no matching files and not `always_run`).
    Skipped,
}

impl Display for HookStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Passed => write!(f, "Passed"),
            Self::Failed => write!(f, "Failed"),
            Self::Skipped => write!(f, "Skipped"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex_simple() {
        assert_eq!(shlex_entry("cargo fmt --"), vec!["cargo", "fmt", "--"]);
    }

    #[test]
    fn shlex_quoted() {
        assert_eq!(
            shlex_entry(r#"echo "hello world" --flag"#),
            vec!["echo", "hello world", "--flag"]
        );
    }

    #[test]
    fn shlex_single_quoted() {
        assert_eq!(
            shlex_entry("echo 'hello world' --flag"),
            vec!["echo", "hello world", "--flag"]
        );
    }

    #[test]
    fn shlex_escaped() {
        assert_eq!(
            shlex_entry(r"echo hello\ world"),
            vec!["echo", "hello world"]
        );
    }
}
