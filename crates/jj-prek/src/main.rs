//! jj-prek: Pre-commit hooks for jj (Jujutsu), following the PREK model.
//!
//! A Rust pre-commit tool designed for the jj version control system.
//! Unlike traditional pre-commit tools, this does not depend on git at all —
//! it works directly with jj workspaces.
//!
//! # Key differences from PREK (git-based):
//! - No `.git/hooks` installation — installs to `.jj/hooks/` instead
//! - Uses `jj diff --summary` for changed files (no staging area)
//! - Uses `jj file list` for all tracked files
//! - Supports jj's revset language for revision ranges
//! - No `GIT_DIR` / `GIT_WORK_TREE` environment variable management

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize;
use tracing::debug;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod config;
mod filter;
mod hook;
mod install;
mod jj;
mod printer;
mod process;
mod run;

#[derive(Debug, Parser)]
#[command(
    name = "jj-prek",
    about = "Pre-commit hooks for jj (Jujutsu)",
    long_about = "A fast, Rust-native pre-commit hook runner for jj workspaces.\n\
                  Follows the PREK model: config-driven hooks with file filtering,\n\
                  type tags, parallel execution, and priority scheduling.\n\n\
                  Does not require or depend on git.",
    version,
    propagate_version = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the config file.
    #[arg(short = 'c', long, global = true)]
    config: Option<PathBuf>,

    /// Change directory before running.
    #[arg(long, global = true)]
    cd: Option<PathBuf>,

    /// Increase verbosity (-v, -vv, -vvv).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

    /// Suppress output.
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    quiet: u8,

    /// Disable colored output.
    #[arg(long, global = true)]
    no_color: bool,

    // Run args (when no subcommand is given, default to `run`)
    #[command(flatten)]
    run_args: RunArgs,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run hooks (default when no subcommand given).
    Run(RunArgs),

    /// Install jj-prek hook into the jj workspace.
    Install(InstallArgs),

    /// Uninstall jj-prek hook from the jj workspace.
    Uninstall,

    /// List configured hooks.
    List(ListArgs),

    /// Validate the config file.
    ValidateConfig(ValidateConfigArgs),

    /// Show the config file path.
    ConfigPath,

    /// Initialize a new jj-prek.toml config file.
    Init(InitArgs),
}

#[derive(Debug, Clone, clap::Args)]
#[allow(clippy::struct_excessive_bools)]
struct RunArgs {
    /// Run on all tracked files, not just changed files.
    #[arg(short = 'a', long)]
    all_files: bool,

    /// Run on specific files.
    #[arg(long, num_args = 1..)]
    files: Vec<PathBuf>,

    /// Run on files changed between revisions (jj revsets).
    #[arg(long)]
    from_ref: Option<String>,

    /// Target revision for --from-ref (defaults to @).
    #[arg(long)]
    to_ref: Option<String>,

    /// Run on files changed in the parent change (@-).
    #[arg(long)]
    last_change: bool,

    /// Stop after the first hook failure.
    #[arg(long)]
    fail_fast: bool,

    /// Continue even if hooks fail.
    #[arg(long)]
    no_fail_fast: bool,

    /// Show jj diff output on failure.
    #[arg(long)]
    show_diff_on_failure: bool,

    /// Only show what would be run, don't execute.
    #[arg(long)]
    dry_run: bool,

    /// Only run hooks matching these IDs.
    #[arg(long = "hook", short = 'H', num_args = 1..)]
    includes: Vec<String>,

    /// Skip hooks matching these IDs.
    #[arg(long, num_args = 1..)]
    skip: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct InstallArgs {
    /// Overwrite existing hook script.
    #[arg(long)]
    overwrite: bool,
}

#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Only list hooks matching these IDs.
    #[arg(long = "hook", short = 'H', num_args = 1..)]
    includes: Vec<String>,

    /// Skip hooks matching these IDs.
    #[arg(long, num_args = 1..)]
    skip: Vec<String>,
}

#[derive(Debug, clap::Args)]
struct ValidateConfigArgs {
    /// Path to the config file to validate.
    config_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, clap::Args)]
struct InitArgs {
    /// Force overwrite if config already exists.
    #[arg(long)]
    force: bool,
}

fn setup_logging(verbose: u8) {
    let directive = match verbose {
        0 => "warn",
        1 => "jj_prek=info",
        2 => "jj_prek=debug",
        3 => "jj_prek=trace",
        _ => "trace",
    };

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(directive));

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(filter),
        )
        .init();
}

async fn run_app(cli: Cli) -> Result<run::ExitStatus> {
    if let Some(dir) = cli.cd.as_ref() {
        debug!("Changing directory to: {}", dir.display());
        std::env::set_current_dir(dir)?;
    }

    if cli.no_color {
        unsafe { std::env::set_var("NO_COLOR", "1") };
    }

    let printer_mode = if cli.quiet >= 2 {
        printer::Printer::Silent
    } else if cli.quiet == 1 {
        printer::Printer::Quiet
    } else if cli.verbose > 0 {
        printer::Printer::Verbose
    } else {
        printer::Printer::Default
    };

    let command = cli.command.unwrap_or(Command::Run(cli.run_args));

    match command {
        Command::Run(args) => {
            let file_source = if args.all_files {
                run::FileSource::AllFiles
            } else if !args.files.is_empty() {
                run::FileSource::Explicit(args.files)
            } else if let Some(from) = args.from_ref {
                let to = args.to_ref.unwrap_or_else(|| "@".to_string());
                run::FileSource::RevRange { from, to }
            } else if args.last_change {
                run::FileSource::LastChange
            } else {
                run::FileSource::WorkingCopy
            };

            let fail_fast = if args.fail_fast {
                Some(true)
            } else if args.no_fail_fast {
                Some(false)
            } else {
                None
            };

            run::run(run::RunArgs {
                config_path: cli.config,
                file_source,
                includes: args.includes,
                skips: args.skip,
                fail_fast,
                show_diff_on_failure: args.show_diff_on_failure,
                verbose: cli.verbose > 0,
                dry_run: args.dry_run,
                printer: printer_mode,
            })
            .await
        }

        Command::Install(args) => {
            install::install(cli.config.as_ref(), args.overwrite, printer_mode)
        }

        Command::Uninstall => install::uninstall(printer_mode),

        Command::List(args) => cmd_list(cli.config, &args, printer_mode),

        Command::ValidateConfig(args) => cmd_validate_config(args, cli.config),

        Command::ConfigPath => cmd_config_path(cli.config),

        Command::Init(args) => cmd_init(args),
    }
}

/// List configured hooks.
#[allow(clippy::print_stdout)]
fn cmd_list(
    config_path: Option<PathBuf>,
    args: &ListArgs,
    _printer: printer::Printer,
) -> Result<run::ExitStatus> {
    let root = jj::JJ_ROOT
        .as_ref()
        .map_err(|e| anyhow::anyhow!("Not in a jj workspace: {e}"))?;

    let config_path = config_path
        .or_else(|| config::find_config(root))
        .ok_or_else(|| anyhow::anyhow!("No config file found"))?;

    let config = config::read_config(&config_path)?;

    println!("{}", "Configured hooks:".bold());
    println!();

    for repo in &config.repos {
        match repo {
            config::Repo::Local { hooks } => {
                for hook in hooks {
                    // Apply include/skip filters
                    if !args.includes.is_empty()
                        && !args
                            .includes
                            .iter()
                            .any(|inc| hook.id == *inc || hook.name == *inc)
                    {
                        continue;
                    }
                    if args.skip.iter().any(|s| hook.id == *s || hook.name == *s) {
                        continue;
                    }

                    println!(
                        "  {} - {} [{}]",
                        hook.id.cyan(),
                        hook.name,
                        hook.language.to_string().dimmed()
                    );
                    if let Some(ref desc) = hook.description {
                        println!("    {desc}");
                    }
                    println!("    entry: {}", hook.entry.dimmed());
                    if let Some(ref files) = hook.files {
                        println!("    files: {files:?}");
                    }
                    println!();
                }
            }
        }
    }

    Ok(run::ExitStatus::Success)
}

/// Validate a config file.
#[allow(clippy::print_stdout)]
fn cmd_validate_config(
    args: ValidateConfigArgs,
    global_config: Option<PathBuf>,
) -> Result<run::ExitStatus> {
    let config_path = args
        .config_file
        .or(global_config)
        .or_else(|| {
            jj::JJ_ROOT
                .as_ref()
                .ok()
                .and_then(|root| config::find_config(root))
        })
        .ok_or_else(|| anyhow::anyhow!("No config file found"))?;

    match config::read_config(&config_path) {
        Ok(config) => {
            let mut hook_count = 0;
            for repo in &config.repos {
                match repo {
                    config::Repo::Local { hooks } => hook_count += hooks.len(),
                }
            }
            println!(
                "{} {} ({} hook(s))",
                "Valid:".green().bold(),
                config_path.display(),
                hook_count
            );
            Ok(run::ExitStatus::Success)
        }
        Err(e) => {
            println!("{} {}: {e}", "Invalid:".red().bold(), config_path.display());
            Ok(run::ExitStatus::Failure)
        }
    }
}

/// Show the config file path.
#[allow(clippy::print_stdout)]
fn cmd_config_path(config_path: Option<PathBuf>) -> Result<run::ExitStatus> {
    let root = jj::JJ_ROOT
        .as_ref()
        .map_err(|e| anyhow::anyhow!("Not in a jj workspace: {e}"))?;

    let path = config_path
        .or_else(|| config::find_config(root))
        .ok_or_else(|| anyhow::anyhow!("No config file found"))?;

    println!("{}", path.display().cyan());
    Ok(run::ExitStatus::Success)
}

/// Initialize a new config file.
#[allow(clippy::print_stdout)]
fn cmd_init(args: InitArgs) -> Result<run::ExitStatus> {
    let root = jj::JJ_ROOT
        .as_ref()
        .map_err(|e| anyhow::anyhow!("Not in a jj workspace: {e}"))?;

    let config_path = root.join("jj-prek.toml");

    if config_path.exists() && !args.force {
        println!(
            "{} {} already exists. Use --force to overwrite.",
            "Error:".red().bold(),
            config_path.display()
        );
        return Ok(run::ExitStatus::Failure);
    }

    let template = r#"# jj-prek configuration
# https://github.com/MarkMarine/prek

[[repos]]
repo = "local"

# Example: cargo fmt
[[repos.hooks]]
id = "cargo-fmt"
name = "cargo fmt"
entry = "cargo fmt --"
language = "system"
types_or = ["rust"]

# Example: cargo clippy
[[repos.hooks]]
id = "cargo-clippy"
name = "cargo clippy"
entry = "cargo clippy --all-targets --all-features -- -D warnings"
language = "system"
pass_filenames = false
types_or = ["rust"]

# Example: trailing whitespace
# [[repos.hooks]]
# id = "trailing-whitespace"
# name = "Trim trailing whitespace"
# entry = "sed -i '' -e 's/[[:space:]]*$//'"
# language = "system"
# types_or = ["text"]
"#;

    std::fs::write(&config_path, template)?;
    println!(
        "{} Created {}",
        "Success:".green().bold(),
        config_path.display().cyan()
    );
    Ok(run::ExitStatus::Success)
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) => err.exit(),
    };

    setup_logging(cli.verbose);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime");

    let result = runtime.block_on(run_app(cli));

    match result {
        Ok(status) => status.into(),
        Err(err) => {
            let mut causes = err.chain();
            #[allow(clippy::print_stderr)]
            {
                eprintln!("{}: {}", "error".red().bold(), causes.next().unwrap());
                for cause in causes {
                    eprintln!("  {}: {cause}", "caused by".red().bold());
                }
            }
            run::ExitStatus::Error.into()
        }
    }
}
