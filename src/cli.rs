//! Command-line interface.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{ArgAction, CommandFactory, Parser, Subcommand};
use log::{LevelFilter, error};

use crate::commands::{example, version};

pub const VERSION: &str = env!("GIT_DESCRIBE_VERSION");

#[derive(Debug, Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version = VERSION, about, long_about = None)]
pub struct Cli {
    /// Increase verbosity (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Subcommand to execute
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands. Register new ones here and dispatch them in
/// [`run`]. The variant doc comments become the `--help` descriptions.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Display package version
    Version,
    /// Example subcommand (replace with your own)
    Example(example::Args),
}

/// Map a `-v` count to a log level.
///
/// 0 = WARN, 1 = INFO, 2 = DEBUG, 3+ = TRACE.
#[must_use]
pub const fn log_level(verbose: u8) -> LevelFilter {
    match verbose {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

/// Configure logging based on verbosity level.
///
/// The `RUST_LOG` environment variable, when set, overrides the count-based
/// verbosity entirely (e.g. `RUST_LOG=debug`).
pub fn setup_logging(verbose: u8) {
    let level = log_level(verbose);
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level.as_str()))
        .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
        .init();
    log::debug!("Logging configured: level={level}");
}

/// Execute the main CLI entry point.
#[must_use]
pub fn run() -> ExitCode {
    let cli = Cli::parse();

    // If no command specified, show help
    let Some(command) = cli.command else {
        // Ignore the write error: printing help only fails on a broken pipe,
        // and there's nothing useful to do about it here.
        let _ = Cli::command().print_help();
        return ExitCode::SUCCESS;
    };

    setup_logging(cli.verbose);

    let result = match command {
        Command::Version => version::run(),
        Command::Example(args) => example::run(&args),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            error!("Command failed: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_definition_is_valid() {
        // Catches conflicting flags, missing metadata, etc. at test time
        Cli::command().debug_assert();
    }

    #[test]
    fn verbose_defaults_to_zero() {
        let cli = Cli::parse_from(["rust-template", "version"]);
        assert_eq!(cli.verbose, 0);
    }

    #[test]
    fn verbose_flag_counts_repetitions() {
        let cli = Cli::parse_from(["rust-template", "-vv", "version"]);
        assert_eq!(cli.verbose, 2);
    }

    #[test]
    fn verbose_flag_is_global() {
        // Global args work after the subcommand too
        let cli = Cli::parse_from(["rust-template", "version", "-v"]);
        assert_eq!(cli.verbose, 1);
    }

    #[test]
    fn log_level_mapping() {
        assert_eq!(log_level(0), LevelFilter::Warn);
        assert_eq!(log_level(1), LevelFilter::Info);
        assert_eq!(log_level(2), LevelFilter::Debug);
        assert_eq!(log_level(3), LevelFilter::Trace);
        assert_eq!(log_level(u8::MAX), LevelFilter::Trace);
    }
}
