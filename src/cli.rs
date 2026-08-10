//! Command-line interface.

use std::io::Write as _;
use std::process::ExitCode;

use clap::{ArgAction, Parser, Subcommand};
use log::{Level, LevelFilter, Log, Metadata, Record, error};

use crate::commands::{hash_password, serve, version};

pub const VERSION: &str = env!("GIT_DESCRIBE_VERSION");

#[derive(Debug, Parser)]
#[command(name = env!("CARGO_PKG_NAME"), version = VERSION, about, long_about = None)]
pub struct Cli {
    /// Increase verbosity (can be repeated: -v, -vv, -vvv)
    #[arg(short, long, action = ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Subcommand to execute; without one, the HTTP service is served
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// Available subcommands. Register new ones here and dispatch them in
/// [`run`]. The variant doc comments become the `--help` descriptions.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Display package version
    Version,
    /// Hash a password read from stdin, for the deployment's password setting
    HashPassword,
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

/// Modules that log credentials at their louder levels, and the loudest level
/// they may reach. `bollard` traces raw Docker API request bodies, which carry
/// the agent container's environment — tokens included.
const CAPPED_MODULES: [(&str, LevelFilter); 1] = [("bollard", LevelFilter::Info)];

fn within_module(target: &str, module: &str) -> bool {
    target == module
        || target
            .strip_prefix(module)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn above_cap(target: &str, level: Level) -> bool {
    CAPPED_MODULES
        .iter()
        .any(|(module, cap)| within_module(target, module) && level > *cap)
}

/// A logger that drops capped modules' loud records whatever the spec says.
struct CappedLogger(env_logger::Logger);

impl CappedLogger {
    fn max_level(&self) -> LevelFilter {
        self.0.filter()
    }
}

impl Log for CappedLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        !above_cap(metadata.target(), metadata.level()) && self.0.enabled(metadata)
    }

    fn log(&self, record: &Record) {
        if !above_cap(record.target(), record.level()) {
            self.0.log(record);
        }
    }

    fn flush(&self) {
        self.0.flush();
    }
}

/// Build the logger for a filter spec in `RUST_LOG` syntax.
fn build_logger(spec: &str) -> CappedLogger {
    CappedLogger(
        env_logger::Builder::new()
            .parse_filters(spec)
            .format(|buf, record| writeln!(buf, "{}: {}", record.level(), record.args()))
            .build(),
    )
}

/// Configure logging based on verbosity level.
///
/// The `RUST_LOG` environment variable, when set, overrides the count-based
/// verbosity entirely (e.g. `RUST_LOG=debug`).
pub fn setup_logging(verbose: u8) {
    let level = log_level(verbose);
    let spec = std::env::var("RUST_LOG").unwrap_or_else(|_| level.as_str().to_owned());
    let logger = build_logger(&spec);
    log::set_max_level(logger.max_level());
    log::set_boxed_logger(Box::new(logger)).expect("logging configured once per process");
    log::debug!("Logging configured: level={level}");
}

/// Execute the main CLI entry point.
#[must_use]
pub fn run() -> ExitCode {
    let cli = Cli::parse();
    setup_logging(cli.verbose);

    let result = match cli.command {
        Some(Command::Version) => version::run(),
        Some(Command::HashPassword) => hash_password::run(),
        None => serve::run(),
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
    use clap::CommandFactory as _;
    use log::{Level, Log as _, Metadata};

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

    fn enabled(spec: &str, target: &str, level: Level) -> bool {
        let logger = build_logger(spec);
        logger.enabled(&Metadata::builder().target(target).level(level).build())
    }

    #[test]
    fn the_docker_client_stays_at_info_however_loud_the_rest_is_turned_up() {
        for spec in ["debug", "trace"] {
            assert!(
                !enabled(spec, "bollard::docker", Level::Debug),
                "{spec} let bollard::docker debug through"
            );
            assert!(
                !enabled(spec, "bollard", Level::Trace),
                "{spec} let bollard trace through"
            );
            assert!(
                enabled(spec, "bollard::docker", Level::Info),
                "{spec} silenced bollard info"
            );
            assert!(
                enabled(spec, "cor_code::plane", Level::Debug),
                "{spec} silenced our own debug"
            );
        }
    }

    #[test]
    fn asking_for_docker_client_traces_by_name_does_not_get_them_either() {
        assert!(!enabled(
            "warn,bollard=trace",
            "bollard::docker",
            Level::Trace
        ));
        assert!(!enabled(
            "bollard::docker=debug",
            "bollard::docker",
            Level::Debug
        ));
    }

    #[test]
    fn a_module_merely_starting_with_the_capped_name_is_untouched() {
        assert!(enabled("debug", "bollardish::inner", Level::Debug));
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
