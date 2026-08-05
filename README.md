# CorCode

Cassidy's opinionated Rust template. Enter at your own peril.

## Usage

<!-- generated-usage:start -->

```text
Cassidy's opinionated Rust template

Usage: cor-code [OPTIONS] [COMMAND]

Commands:
  version  Display package version
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
