//! Regenerates docs using special comment markers.
//!
//! ```text
//! cargo run --bin gen-docs
//! ```
//!
//! Content between the `generated-usage` markers in README.md and SKILL.md
//! is replaced; everything outside them is left alone. A missing SKILL.md
//! is created from a skeleton first, so its frontmatter is hand-editable.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use clap::CommandFactory;

use rust_template::cli::Cli;

const MARKER_START: &str = "<!-- generated-usage:start -->";
const MARKER_END: &str = "<!-- generated-usage:end -->";

fn main() -> Result<()> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Cli::command();
    cmd.build();

    let skeleton = skill_skeleton(&cmd);
    update(&root.join("README.md"), &render_usage(&mut cmd)?, None)?;
    update(
        &root.join("SKILL.md"),
        &render_skill_body(&mut cmd)?,
        Some(&skeleton),
    )?;
    Ok(())
}

/// Top-level help as a fenced code block.
fn render_usage(cmd: &mut clap::Command) -> Result<String> {
    let mut usage = String::new();
    writeln!(usage, "```text")?;
    write!(usage, "{}", cmd.render_long_help())?;
    writeln!(usage, "```")?;
    Ok(usage)
}

/// Top-level help plus a section per subcommand.
fn render_skill_body(cmd: &mut clap::Command) -> Result<String> {
    let name = cmd.get_name().to_string();
    let mut body = render_usage(cmd)?;

    let subcommands: Vec<String> = cmd
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help")
        .map(|sub| sub.get_name().to_string())
        .collect();
    for sub_name in subcommands {
        let sub = cmd
            .find_subcommand_mut(&sub_name)
            .with_context(|| format!("subcommand {sub_name} vanished mid-render"))?;
        writeln!(body)?;
        writeln!(body, "## {name} {sub_name}")?;
        writeln!(body)?;
        writeln!(body, "```text")?;
        write!(body, "{}", sub.render_long_help())?;
        writeln!(body, "```")?;
    }
    Ok(body)
}

/// Initial SKILL.md for when none exists; regeneration never touches the
/// parts outside the markers, so the frontmatter can be edited freely.
fn skill_skeleton(cmd: &clap::Command) -> String {
    let name = cmd.get_name();
    let description = cmd.get_about().map(ToString::to_string).unwrap_or_default();
    format!(
        "---\nname: {name}\ndescription: {description}\n---\n\n# {name}\n\n{MARKER_START}\n{MARKER_END}\n"
    )
}

/// Replace the marked region of `path` with `replacement`. A missing file
/// starts from `skeleton` when one is provided.
fn update(path: &Path, replacement: &str, skeleton: Option<&str>) -> Result<()> {
    let content = match skeleton {
        Some(skeleton) if !path.exists() => skeleton.to_string(),
        _ => fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?,
    };

    let start = content
        .find(MARKER_START)
        .with_context(|| format!("{MARKER_START} marker missing from {}", path.display()))?;
    let end = content
        .find(MARKER_END)
        .with_context(|| format!("{MARKER_END} marker missing from {}", path.display()))?;
    ensure!(
        start < end,
        "markers are out of order in {}",
        path.display()
    );

    let before = &content[..start + MARKER_START.len()];
    let after = &content[end..];
    let updated = format!("{before}\n\n{replacement}\n{after}");
    fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    println!("Updated {}", path.display());
    Ok(())
}
