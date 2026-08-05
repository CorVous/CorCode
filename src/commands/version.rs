//! Version subcommand - display package version.

use anyhow::Result;
use log::info;

/// Execute the version command.
pub fn run() -> Result<()> {
    info!("Displaying version information");
    println!("{} {}", env!("CARGO_PKG_NAME"), crate::cli::VERSION);
    Ok(())
}
