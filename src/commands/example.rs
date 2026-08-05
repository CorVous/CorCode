//! Example subcommand template.
//!
//! To add a new subcommand:
//!
//! 1. Copy this file to a new name (e.g., `fetch_data.rs`)
//! 2. Declare the module in `commands/mod.rs`
//! 3. Update the [`Args`] struct and implement your logic in [`run`]
//! 4. Add a variant to `Command` in `cli.rs` and dispatch it in `run()`

use anyhow::Result;
use log::{debug, info};

/// Arguments for the example subcommand.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// Example positional argument
    pub name: String,

    /// Example optional argument
    #[arg(long, default_value = "Hello")]
    pub greeting: String,
}

/// Execute the example command.
///
/// # Errors
///
/// Never fails as written; returns `Result` so real logic can use `?`.
pub fn run(args: &Args) -> Result<()> {
    info!("Running example command with name={}", args.name);
    debug!("Greeting: {}", args.greeting);

    // Your command logic here
    println!("{}, {}!", args.greeting, args.name);

    Ok(())
}
