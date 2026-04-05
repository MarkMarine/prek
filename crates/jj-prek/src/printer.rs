//! Output formatting and display.
//!
//! Following the PREK model, provides different output levels (default, quiet,
//! verbose) and consistent hook result formatting.

use std::time::Duration;

use owo_colors::OwoColorize;

use crate::hook::{HookResult, HookStatus};

/// Output verbosity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Printer {
    /// Normal output.
    Default,
    /// Quiet: only show failures.
    Quiet,
    /// Silent: no output at all.
    Silent,
    /// Verbose: show all details.
    Verbose,
}

impl Printer {
    pub(crate) fn is_quiet(self) -> bool {
        matches!(self, Self::Quiet | Self::Silent)
    }

    pub(crate) fn is_silent(self) -> bool {
        matches!(self, Self::Silent)
    }
}

/// The maximum width for hook name display.
const HOOK_NAME_WIDTH: usize = 60;
/// The separator between hook name and status.
const SEPARATOR: char = '.';

/// Print a hook result line in PREK style.
///
/// Format: `hook name...............Passed` or `hook name...............Failed`
#[allow(clippy::print_stdout)]
pub(crate) fn print_hook_result(result: &HookResult, hook_name: &str, duration: Duration) {
    let name_len = hook_name.len();
    let padding = if name_len < HOOK_NAME_WIDTH {
        SEPARATOR.to_string().repeat(HOOK_NAME_WIDTH - name_len)
    } else {
        String::from(".")
    };

    let status_str = match result.status {
        HookStatus::Passed => "Passed".green().to_string(),
        HookStatus::Failed => "Failed".red().to_string(),
        HookStatus::Skipped => "(no files)".dimmed().to_string(),
    };

    let duration_str = format_duration(duration);

    println!(
        "{}{}{} {}",
        hook_name,
        padding.dimmed(),
        status_str,
        duration_str.dimmed()
    );

    // Print output if there's any relevant output
    let output = String::from_utf8_lossy(&result.output);
    let output = output.trim();
    if !output.is_empty() && result.status == HookStatus::Failed {
        for line in output.lines() {
            println!("  {line}");
        }
    }
}

/// Print a hook result line for verbose mode (always show output).
#[allow(clippy::print_stdout)]
pub(crate) fn print_hook_result_verbose(result: &HookResult, hook_name: &str, duration: Duration) {
    print_hook_result(result, hook_name, duration);

    if result.status == HookStatus::Passed {
        let output = String::from_utf8_lossy(&result.output);
        let output = output.trim();
        if !output.is_empty() {
            for line in output.lines() {
                println!("  {line}");
            }
        }
    }
}

/// Print a header for the run.
#[allow(clippy::print_stdout)]
pub(crate) fn print_header(title: &str) {
    println!("{}", title.bold());
}

/// Print a summary line.
#[allow(clippy::print_stdout)]
pub(crate) fn print_summary(passed: usize, failed: usize, skipped: usize, duration: Duration) {
    let total = passed + failed + skipped;
    let summary = format!(
        "{total} hook(s): {passed} passed, {failed} failed, {skipped} skipped in {}",
        format_duration(duration)
    );
    if failed > 0 {
        println!("\n{}", summary.red().bold());
    } else {
        println!("\n{}", summary.green().bold());
    }
}

/// Format a duration for display.
fn format_duration(d: Duration) -> String {
    let secs = d.as_secs_f64();
    if secs < 1.0 {
        format!("{:.0}ms", d.as_millis())
    } else if secs < 60.0 {
        format!("{secs:.1}s")
    } else {
        let mins = secs / 60.0;
        format!("{mins:.1}m")
    }
}
