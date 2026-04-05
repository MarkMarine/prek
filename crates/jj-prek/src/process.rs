//! Process execution utilities.
//!
//! Provides a `Cmd` wrapper around `tokio::process::Command` with logging,
//! error context, and status checking — adapted from PREK's process module.

use std::ffi::OsStr;
use std::fmt::Display;
use std::path::Path;
use std::process::{CommandArgs, ExitStatus, Output, Stdio};

use owo_colors::OwoColorize;
use thiserror::Error;
use tracing::trace;

/// An error from executing a Command.
#[derive(Debug, Error)]
pub enum Error {
    /// The command fundamentally failed to execute (usually means it didn't exist).
    #[error("Run command `{summary}` failed")]
    Exec {
        summary: String,
        #[source]
        cause: std::io::Error,
    },
    /// The command ran but exited with a non-zero status.
    #[error("Command `{summary}` exited with an error:\n{error}")]
    Status { summary: String, error: StatusError },
}

/// The command ran but signaled an error via exit code.
#[derive(Debug)]
pub struct StatusError {
    pub status: ExitStatus,
    pub output: Option<Output>,
}

impl Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "\n{}\n{}", "[status]".red(), self.status)?;

        if let Some(output) = &self.output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout: Vec<&str> = stdout
                .split('\n')
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() { None } else { Some(line) }
                })
                .collect();
            let stderr: Vec<&str> = stderr
                .split('\n')
                .filter_map(|line| {
                    let line = line.trim();
                    if line.is_empty() { None } else { Some(line) }
                })
                .collect();

            if !stdout.is_empty() {
                writeln!(f, "\n{}\n{}", "[stdout]".red(), stdout.join("\n"))?;
            }
            if !stderr.is_empty() {
                writeln!(f, "\n{}\n{}", "[stderr]".red(), stderr.join("\n"))?;
            }
        }

        Ok(())
    }
}

/// A wrapped async Command with summary context and status checking.
pub struct Cmd {
    pub inner: tokio::process::Command,
    summary: String,
    check_status: bool,
}

// Constructors
impl Cmd {
    /// Create a new Command with a summary of what this is trying to do.
    pub fn new(command: impl AsRef<OsStr>, summary: impl Into<String>) -> Self {
        let inner = tokio::process::Command::new(command);
        Self {
            summary: summary.into(),
            inner,
            check_status: true,
        }
    }
}

// Builder APIs
impl Cmd {
    /// Set whether `Status::success` should be checked after execution.
    /// Defaults to `true`.
    pub fn check(&mut self, checked: bool) -> &mut Self {
        self.check_status = checked;
        self
    }
}

// Execution APIs
impl Cmd {
    /// Run the command and return its output.
    pub async fn output(&mut self) -> Result<Output, Error> {
        self.log_command();
        let output = self.inner.output().await.map_err(|cause| Error::Exec {
            summary: self.summary.clone(),
            cause,
        })?;
        self.maybe_check_output(&output)?;
        Ok(output)
    }

    /// Run the command and return its exit status.
    pub async fn status(&mut self) -> Result<ExitStatus, Error> {
        self.log_command();
        let status = self.inner.status().await.map_err(|cause| Error::Exec {
            summary: self.summary.clone(),
            cause,
        })?;
        self.maybe_check_status(status)?;
        Ok(status)
    }

    /// Spawn the command without waiting for completion.
    pub fn spawn(&mut self) -> Result<tokio::process::Child, Error> {
        self.log_command();
        self.inner.spawn().map_err(|cause| Error::Exec {
            summary: self.summary.clone(),
            cause,
        })
    }
}

// Forwarded std::process::Command APIs
impl Cmd {
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner.env(key, val);
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn stdin<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdin(cfg);
        self
    }

    pub fn stdout<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stdout(cfg);
        self
    }

    pub fn stderr<T: Into<Stdio>>(&mut self, cfg: T) -> &mut Self {
        self.inner.stderr(cfg);
        self
    }

    pub fn get_program(&self) -> &OsStr {
        self.inner.as_std().get_program()
    }

    pub fn get_args(&self) -> CommandArgs<'_> {
        self.inner.as_std().get_args()
    }

    pub fn get_current_dir(&self) -> Option<&Path> {
        self.inner.as_std().get_current_dir()
    }
}

// Diagnostic APIs
impl Cmd {
    fn maybe_check_status(&self, status: ExitStatus) -> Result<(), Error> {
        if self.check_status && !status.success() {
            return Err(Error::Status {
                summary: self.summary.clone(),
                error: StatusError {
                    status,
                    output: None,
                },
            });
        }
        Ok(())
    }

    fn maybe_check_output(&self, output: &Output) -> Result<(), Error> {
        if self.check_status && !output.status.success() {
            return Err(Error::Status {
                summary: self.summary.clone(),
                error: StatusError {
                    status: output.status,
                    output: Some(output.clone()),
                },
            });
        }
        Ok(())
    }

    fn log_command(&self) {
        trace!("Executing `{self}`");
    }
}

impl Display for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(cwd) = self.get_current_dir() {
            write!(f, "cd {} && ", cwd.to_string_lossy())?;
        }
        let program = self.get_program();
        write!(f, "{}", program.to_string_lossy())?;

        let mut len = 0;
        for arg in self.get_args() {
            write!(f, " {}", arg.to_string_lossy())?;
            len += arg.len() + 1;
            if len > 120 {
                write!(f, " [...]")?;
                break;
            }
        }
        Ok(())
    }
}
