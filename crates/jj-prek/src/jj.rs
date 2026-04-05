//! Jujutsu (jj) VCS integration.
//!
//! This module replaces PREK's `git.rs` with equivalent jj operations.
//! Key differences from git:
//!   - jj has no staging area; the working copy IS the current change
//!   - `jj diff --summary` replaces `git diff --staged`
//!   - `jj file list` replaces `git ls-files`
//!   - `jj workspace root` replaces `git rev-parse --show-toplevel`
//!   - Revisions use jj's revset language, not git refs

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::Result;
use thiserror::Error;
use tracing::{debug, instrument};

use crate::process::{self, Cmd};

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error(transparent)]
    Command(#[from] process::Error),

    #[error("Failed to find jj: {0}")]
    JjNotFound(#[from] which::Error),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Utf8(#[from] std::str::Utf8Error),
}

pub(crate) static JJ: LazyLock<Result<PathBuf, which::Error>> =
    LazyLock::new(|| which::which("jj"));

pub(crate) static JJ_ROOT: LazyLock<Result<PathBuf, Error>> = LazyLock::new(|| {
    get_root().inspect(|root| {
        debug!("jj workspace root: {}", root.display());
    })
});

/// Create a `Cmd` for running jj with standard flags.
pub(crate) fn jj_cmd(summary: &str) -> Result<Cmd, Error> {
    let cmd = Cmd::new(JJ.as_ref().map_err(|&e| Error::JjNotFound(e))?, summary);
    Ok(cmd)
}

/// Split newline-delimited output into paths.
fn newline_split(s: &[u8]) -> Result<Vec<PathBuf>, std::str::Utf8Error> {
    std::str::from_utf8(s)?
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| Ok(PathBuf::from(line)))
        .collect()
}

/// Parse `jj diff --summary` output into file paths.
///
/// Output format is: `<status> <path>` where status is one of:
///   M (modified), A (added), D (deleted), R (renamed: "R {from} => {to}")
///
/// We return all non-deleted paths.
fn parse_diff_summary(output: &[u8]) -> Result<Vec<PathBuf>, std::str::Utf8Error> {
    let text = std::str::from_utf8(output)?;
    let mut paths = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Format: "M path" or "A path" or "D path" or "R {from} => {to}"
        if line.starts_with("D ") {
            // Skip deleted files
            continue;
        }
        if let Some(rest) = line.strip_prefix("R ") {
            // Renamed: "R {from} => {to}", take the destination
            if let Some((_from, to)) = rest.split_once(" => ") {
                paths.push(PathBuf::from(to));
            }
        } else if line.len() > 2 {
            // "M path" or "A path" or "C path"
            let path = &line[2..];
            paths.push(PathBuf::from(path));
        }
    }
    Ok(paths)
}

/// Get the workspace root directory.
#[instrument(level = "trace")]
pub(crate) fn get_root() -> Result<PathBuf, Error> {
    let jj = JJ.as_ref().map_err(|&e| Error::JjNotFound(e))?;
    let output = std::process::Command::new(jj)
        .arg("workspace")
        .arg("root")
        .output()?;
    if !output.status.success() {
        return Err(Error::Command(process::Error::Status {
            summary: "get jj workspace root".to_string(),
            error: process::StatusError {
                status: output.status,
                output: Some(output),
            },
        }));
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}

/// Get files changed in the working copy (the current change).
///
/// In jj, the working copy automatically becomes a change. This is equivalent
/// to `git diff --staged` in the pre-commit sense — these are the files that
/// will be part of the commit.
#[instrument(level = "trace")]
pub(crate) async fn get_working_copy_changes(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = jj_cmd("get working copy changes")?
        .current_dir(root)
        .arg("diff")
        .arg("--summary")
        .check(true)
        .output()
        .await?;
    Ok(parse_diff_summary(&output.stdout)?)
}

/// Get files changed between two revisions.
///
/// Uses jj's revset language for `from` and `to`.
#[instrument(level = "trace")]
pub(crate) async fn get_changed_files(
    from: &str,
    to: &str,
    root: &Path,
) -> Result<Vec<PathBuf>, Error> {
    let output = jj_cmd("get changed files between revisions")?
        .current_dir(root)
        .arg("diff")
        .arg("--summary")
        .arg("--from")
        .arg(from)
        .arg("--to")
        .arg(to)
        .check(true)
        .output()
        .await?;
    Ok(parse_diff_summary(&output.stdout)?)
}

/// Get all tracked files in the repository.
///
/// Equivalent to `git ls-files`.
#[instrument(level = "trace")]
pub(crate) async fn list_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = jj_cmd("jj file list")?
        .current_dir(root)
        .arg("file")
        .arg("list")
        .check(true)
        .output()
        .await?;
    Ok(newline_split(&output.stdout)?)
}

/// Get files changed in the last committed change (the parent of @).
#[instrument(level = "trace")]
pub(crate) async fn get_last_change_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    let output = jj_cmd("get last change files")?
        .current_dir(root)
        .arg("diff")
        .arg("--summary")
        .arg("-r")
        .arg("@-")
        .check(true)
        .output()
        .await?;
    Ok(parse_diff_summary(&output.stdout)?)
}

/// Get the current change ID (short form).
pub(crate) async fn get_current_change_id(root: &Path) -> Result<String, Error> {
    let output = jj_cmd("get current change id")?
        .current_dir(root)
        .arg("log")
        .arg("--no-graph")
        .arg("-r")
        .arg("@")
        .arg("-T")
        .arg("change_id.shortest()")
        .check(true)
        .output()
        .await?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Check if the working copy has any changes.
pub(crate) async fn is_working_copy_clean(root: &Path) -> Result<bool, Error> {
    let output = jj_cmd("check working copy clean")?
        .current_dir(root)
        .arg("diff")
        .arg("--summary")
        .check(true)
        .output()
        .await?;
    Ok(output.stdout.trim_ascii().is_empty())
}

/// Check if there are any conflicts in the working copy.
pub(crate) async fn has_conflicts(root: &Path) -> Result<bool, Error> {
    let output = jj_cmd("check conflicts")?
        .current_dir(root)
        .arg("resolve")
        .arg("--list")
        .check(false)
        .output()
        .await?;
    // `jj resolve --list` outputs conflicted files; empty means no conflicts
    Ok(!output.stdout.trim_ascii().is_empty())
}

/// Get the diff output for display purposes.
pub(crate) async fn get_diff(root: &Path) -> Result<Vec<u8>, Error> {
    let output = jj_cmd("jj diff")?
        .current_dir(root)
        .arg("diff")
        .check(true)
        .output()
        .await?;
    Ok(output.stdout)
}

/// Get diff for a specific revision.
pub(crate) async fn get_diff_for_rev(root: &Path, rev: &str) -> Result<Vec<u8>, Error> {
    let output = jj_cmd("jj diff for rev")?
        .current_dir(root)
        .arg("diff")
        .arg("-r")
        .arg(rev)
        .check(true)
        .output()
        .await?;
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_diff_summary_basic() {
        let output = b"M src/main.rs\nA src/new.rs\nD src/old.rs\n";
        let paths = parse_diff_summary(output).unwrap();
        assert_eq!(
            paths,
            vec![PathBuf::from("src/main.rs"), PathBuf::from("src/new.rs")]
        );
    }

    #[test]
    fn parse_diff_summary_rename() {
        let output = b"R src/old.rs => src/new.rs\n";
        let paths = parse_diff_summary(output).unwrap();
        assert_eq!(paths, vec![PathBuf::from("src/new.rs")]);
    }

    #[test]
    fn parse_diff_summary_empty() {
        let output = b"";
        let paths = parse_diff_summary(output).unwrap();
        assert!(paths.is_empty());
    }
}
