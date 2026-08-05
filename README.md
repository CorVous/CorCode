# Rust Template

Cassidy's opinionated Rust template. Enter at your own peril.

## Usage

<!-- generated-usage:start -->

```text
Cassidy's opinionated Rust template

Usage: rust-template [OPTIONS] [COMMAND]

Commands:
  version  Display package version
  example  Example subcommand (replace with your own)
  help     Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose...
          Increase verbosity (can be repeated: -v, -vv, -vvv)

  -h, --help
          Print help

  -V, --version
          Print version
```

<!-- generated-usage:end -->


## Development

Requires [Rust](https://rustup.rs/)

Build: `cargo build`

Run: `cargo run -- --help`

Lint: `cargo clippy --all-targets --all-features -- -D warnings`

Format: `cargo fmt`

Test: `cargo test`

Regenerate docs: `cargo run --bin gen-docs`

###  Advanced

Benchmark: `cargo bench` (HTML report at `target/criterion/report/index.html`;
fast sanity pass: `cargo bench --bench cli_bench -- --quick`)

Security audit: `cargo audit` (requires `cargo install cargo-audit`)

Release:

1. `cargo publish patch` (or `minor` or `major`)
2. `git push main`
3. git push your tag
4. Wait for CI to finish the job

## Template Setup

1. **Create a new repository** from this template on GitHub (click "Use this template")

2. **Clone your new repository** and navigate to it

3. **Update project metadata** in `Cargo.toml`:
   - Change `name` to your project name (use kebab-case, e.g., `my-awesome-tool`)
   - Update the `[lib]` name to match (snake_case, e.g., `my_awesome_tool`)
   - Update the first `[[bin]]` name and `default-run` to match (kebab-case)
   - Update `authors`, `description`, `repository`, `keywords`, and `categories`
   - Update dependencies as required

4. **Update the crate references in source**:
   - `src/main.rs`: change `rust_template::cli::run()` to `your_crate_name::cli::run()`
   - `src/bin/gen_docs.rs` and `benches/cli_bench.rs`: update the `use rust_template::...` imports
   - Everything else uses `crate::` paths or `env!("CARGO_PKG_NAME")` and
     picks up the rename automatically

5. **Update `.github/workflows/ci-cd.yml`**:
   - Change `BIN="rust-template"` in the Package binary step to your binary name

6. **Update `.vscode/launch.json`** (if using VS Code):
   - Replace `rust-template` / `rust_template` in the `cargo` sections

7. **Update the README**:
   - Replace the title and description
   - Remove or customize this Template Setup section

8. **Verify everything works**:
   ```bash
   cargo test
   cargo clippy --all-targets --all-features -- -D warnings
   cargo fmt --check
   cargo run --bin gen-docs
   ```
