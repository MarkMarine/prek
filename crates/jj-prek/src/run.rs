//! Hook execution engine.
//!
//! Orchestrates the full run: config loading, file collection, filtering,
//! and hook execution — following the PREK run model adapted for jj.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Instant;

use anyhow::{Context, Result};
use futures::stream::{self, StreamExt};
use owo_colors::OwoColorize;
use tracing::debug;

use crate::config::{self, Config, Repo};
use crate::hook::{Hook, HookResult, HookStatus};
use crate::jj;
use crate::printer::{self, Printer};

/// Whether to use ANSI colors in output.
pub(crate) static USE_COLOR: LazyLock<bool> = LazyLock::new(|| {
    // Check NO_COLOR env var — if set, disable colors
    #[allow(clippy::disallowed_methods)]
    std::env::var("NO_COLOR").is_err()
});

/// Maximum number of concurrent hook executions.
fn max_concurrency() -> usize {
    #[allow(clippy::disallowed_methods)]
    let from_env = std::env::var("JJ_PREK_MAX_CONCURRENCY")
        .ok()
        .and_then(|v| v.parse().ok());
    from_env.unwrap_or_else(|| {
        std::thread::available_parallelism().map_or(4, std::num::NonZero::get)
    })
}

/// How files are collected for the run.
#[derive(Debug)]
pub(crate) enum FileSource {
    /// Changed files in the working copy (default).
    WorkingCopy,
    /// All tracked files.
    AllFiles,
    /// Specific files provided on the command line.
    Explicit(Vec<PathBuf>),
    /// Changed files between two revisions.
    RevRange { from: String, to: String },
    /// Files changed in the last committed change (@-).
    LastChange,
}

/// Arguments for a run.
pub(crate) struct RunArgs {
    pub config_path: Option<PathBuf>,
    pub file_source: FileSource,
    pub includes: Vec<String>,
    pub skips: Vec<String>,
    pub fail_fast: Option<bool>,
    pub show_diff_on_failure: bool,
    pub verbose: bool,
    pub dry_run: bool,
    pub printer: Printer,
}

/// Execute a full hook run.
pub(crate) async fn run(args: RunArgs) -> Result<ExitStatus> {
    let start = Instant::now();

    // 1. Discover workspace root
    let root = jj::JJ_ROOT
        .as_ref()
        .map_err(|e| anyhow::anyhow!("Not in a jj workspace: {e}"))?;
    debug!("Workspace root: {}", root.display());

    // 2. Find and parse config
    let config_path = args
        .config_path
        .or_else(|| config::find_config(root))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No config file found. Create a `jj-prek.toml` or `prek.toml` in {}",
                root.display()
            )
        })?;
    debug!("Using config: {}", config_path.display());
    let config = config::read_config(&config_path)?;

    // 3. Build hooks from config
    let mut hooks = build_hooks(&config);
    debug!("Loaded {} hook(s)", hooks.len());

    // 4. Apply include/skip filters
    if !args.includes.is_empty() {
        hooks.retain(|h| {
            args.includes
                .iter()
                .any(|inc| h.id == *inc || h.name == *inc)
        });
    }
    if !args.skips.is_empty() {
        hooks.retain(|h| {
            !args
                .skips
                .iter()
                .any(|skip| h.id == *skip || h.name == *skip)
        });
    }

    // Also check JJ_PREK_SKIP env var
    #[allow(clippy::disallowed_methods)]
    if let Ok(skip_env) = std::env::var("JJ_PREK_SKIP") {
        let skip_ids: Vec<&str> = skip_env.split(',').map(str::trim).collect();
        hooks.retain(|h| !skip_ids.contains(&h.id.as_str()));
    }

    if hooks.is_empty() {
        if !args.printer.is_quiet() {
            #[allow(clippy::print_stdout)]
            {
                println!("{}", "No hooks to run.".dimmed());
            }
        }
        return Ok(ExitStatus::Success);
    }

    // Sort by priority
    hooks.sort_by_key(|h| h.priority);

    // 5. Collect files
    let files = collect_files(root, &args.file_source).await?;
    debug!("Collected {} file(s)", files.len());

    // Apply global filters from config
    let files = apply_global_filters(&config, &files);
    debug!("{} file(s) after global filters", files.len());

    // Convert back to owned for hook filtering
    let files: Vec<PathBuf> = files.into_iter().map(Path::to_path_buf).collect();

    // 6. Run hooks
    if !args.printer.is_quiet() {
        printer::print_header("jj-prek hooks");
    }

    let fail_fast = args.fail_fast.or(config.fail_fast).unwrap_or(false);

    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    if args.dry_run {
        // Dry run: just show what would be run
        for hook in &hooks {
            let filtered = hook.filter_files(&files);
            let count = filtered.len();
            #[allow(clippy::print_stdout)]
            if count > 0 || hook.always_run {
                println!("  {} ({count} file(s))", hook.name);
            } else {
                println!("  {} (skipped, no files)", hook.name.dimmed());
            }
        }
        return Ok(ExitStatus::Success);
    }

    // Partition hooks into priority groups for parallel execution
    let priority_groups = group_by_priority(&hooks);

    for group in priority_groups {
        let serial_hooks: Vec<&Hook> =
            group.iter().filter(|h| h.require_serial).copied().collect();
        let parallel_hooks: Vec<&Hook> = group
            .iter()
            .filter(|h| !h.require_serial)
            .copied()
            .collect();

        // Run serial hooks first
        for hook in serial_hooks {
            let result = run_single_hook(hook, &files, args.verbose, &args.printer).await?;
            match result.status {
                HookStatus::Passed => passed += 1,
                HookStatus::Failed => {
                    failed += 1;
                    if fail_fast || hook.fail_fast.unwrap_or(false) {
                        break;
                    }
                }
                HookStatus::Skipped => skipped += 1,
            }
        }

        if failed > 0 && fail_fast {
            break;
        }

        // Run parallel hooks concurrently
        if !parallel_hooks.is_empty() {
            let concurrency = max_concurrency().min(parallel_hooks.len());
            let results: Vec<Result<(HookResult, std::time::Duration)>> =
                stream::iter(parallel_hooks.into_iter().map(|hook| {
                    let files = files.clone();
                    async move {
                        let hook_start = Instant::now();
                        let filtered = hook.filter_files(&files);

                        let result = if filtered.is_empty() && !hook.always_run {
                            HookResult {
                                hook_id: hook.id.clone(),
                                status: HookStatus::Skipped,
                                output: Vec::new(),
                                files_count: 0,
                            }
                        } else {
                            hook.run(&filtered).await?
                        };

                        Ok((result, hook_start.elapsed()))
                    }
                }))
                .buffer_unordered(concurrency)
                .collect()
                .await;

            for result in results {
                let (result, duration) = result?;
                let hook = hooks.iter().find(|h| h.id == result.hook_id).unwrap();

                if !args.printer.is_silent() {
                    if args.verbose {
                        printer::print_hook_result_verbose(&result, &hook.name, duration);
                    } else if !args.printer.is_quiet() || result.status == HookStatus::Failed {
                        printer::print_hook_result(&result, &hook.name, duration);
                    }
                }

                match result.status {
                    HookStatus::Passed => passed += 1,
                    HookStatus::Failed => {
                        failed += 1;
                        if fail_fast {
                            break;
                        }
                    }
                    HookStatus::Skipped => skipped += 1,
                }
            }
        }

        if failed > 0 && fail_fast {
            break;
        }
    }

    // 7. Show summary
    if !args.printer.is_silent() {
        printer::print_summary(passed, failed, skipped, start.elapsed());
    }

    // 8. Show diff on failure if requested
    if failed > 0 && args.show_diff_on_failure {
        #[allow(clippy::print_stdout)]
        {
            println!("\n{}", "All changes made by hooks:".yellow().bold());
        }
        if let Ok(diff) = jj::get_diff(root).await {
            let diff_str = String::from_utf8_lossy(&diff);
            #[allow(clippy::print_stdout)]
            {
                println!("{diff_str}");
            }
        }
    }

    if failed > 0 {
        Ok(ExitStatus::Failure)
    } else {
        Ok(ExitStatus::Success)
    }
}

/// Run a single hook with timing and output.
async fn run_single_hook(
    hook: &Hook,
    files: &[PathBuf],
    verbose: bool,
    printer_mode: &Printer,
) -> Result<HookResult> {
    let hook_start = Instant::now();
    let filtered = hook.filter_files(files);

    let result = if filtered.is_empty() && !hook.always_run {
        HookResult {
            hook_id: hook.id.clone(),
            status: HookStatus::Skipped,
            output: Vec::new(),
            files_count: 0,
        }
    } else {
        hook.run(&filtered).await?
    };

    let duration = hook_start.elapsed();

    if !printer_mode.is_silent() {
        if verbose {
            printer::print_hook_result_verbose(&result, &hook.name, duration);
        } else if !printer_mode.is_quiet() || result.status == HookStatus::Failed {
            printer::print_hook_result(&result, &hook.name, duration);
        }
    }

    Ok(result)
}

/// Build hooks from the parsed config.
fn build_hooks(config: &Config) -> Vec<Hook> {
    let mut hooks = Vec::new();
    for repo in &config.repos {
        match repo {
            Repo::Local { hooks: local_hooks } => {
                for local_hook in local_hooks {
                    hooks.push(Hook::from_local(local_hook));
                }
            }
        }
    }
    hooks
}

/// Collect files based on the file source.
async fn collect_files(root: &Path, source: &FileSource) -> Result<Vec<PathBuf>> {
    match source {
        FileSource::WorkingCopy => jj::get_working_copy_changes(root)
            .await
            .context("Failed to get working copy changes"),
        FileSource::AllFiles => jj::list_files(root)
            .await
            .context("Failed to list all files"),
        FileSource::Explicit(files) => Ok(files.clone()),
        FileSource::RevRange { from, to } => jj::get_changed_files(from, to, root)
            .await
            .context("Failed to get changed files between revisions"),
        FileSource::LastChange => jj::get_last_change_files(root)
            .await
            .context("Failed to get last change files"),
    }
}

/// Apply global include/exclude filters from the config.
fn apply_global_filters<'a>(config: &Config, files: &'a [PathBuf]) -> Vec<&'a Path> {
    files
        .iter()
        .filter(|path| {
            let path_str = path.to_string_lossy();

            if let Some(ref include) = config.files {
                if !include.is_match(&path_str) {
                    return false;
                }
            }

            if let Some(ref exclude) = config.exclude {
                if exclude.is_match(&path_str) {
                    return false;
                }
            }

            true
        })
        .map(PathBuf::as_path)
        .collect()
}

/// Group hooks by priority for parallel execution within groups.
fn group_by_priority(hooks: &[Hook]) -> Vec<Vec<&Hook>> {
    if hooks.is_empty() {
        return vec![];
    }

    let mut groups: Vec<Vec<&Hook>> = Vec::new();
    let mut current_priority = hooks[0].priority;
    let mut current_group = Vec::new();

    for hook in hooks {
        if hook.priority != current_priority {
            groups.push(std::mem::take(&mut current_group));
            current_priority = hook.priority;
        }
        current_group.push(hook);
    }
    if !current_group.is_empty() {
        groups.push(current_group);
    }

    groups
}

/// Exit status for the run command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExitStatus {
    Success,
    Failure,
    Error,
}

impl From<ExitStatus> for std::process::ExitCode {
    fn from(status: ExitStatus) -> Self {
        match status {
            ExitStatus::Success => Self::SUCCESS,
            ExitStatus::Failure => Self::from(1),
            ExitStatus::Error => Self::from(3),
        }
    }
}
