# CorCode

Cassidy's opinionated Rust template. Enter at your own peril.

## Usage

<!-- generated-usage:start -->

```text
Cassidy's opinionated Rust template

Usage: cor-code [OPTIONS] [COMMAND]

Commands:
  version        Display package version
  hash-password  Hash a password read from stdin, for the deployment's password setting
  help           Print this message or the help of the given subcommand(s)

Options:
  -v, --verbose...
          Increase verbosity (can be repeated: -v, -vv, -vvv)

  -h, --help
          Print help

  -V, --version
          Print version
```

<!-- generated-usage:end -->

## Configuration

Serving (the default action) reads its configuration from the environment:

| Variable                   | Required | Default        |
| --------------------------- | -------- | -------------- |
| `CORCODE_DATA_DIR`          | yes      | -              |
| `CORCODE_USERNAME`          | yes      | -              |
| `CORCODE_PASSWORD_HASH`     | yes      | -              |
| `CORCODE_WORKSPACE_IMAGE`   | yes      | -              |
| `CORCODE_REPOS`             | yes      | -              |
| `CORCODE_HOST_DATA_DIR`     | no       | `CORCODE_DATA_DIR` |
| `CORCODE_BIND_ADDR`         | no       | `0.0.0.0:8080` |
| `CORCODE_CONTAINER_MEMORY_MB` | no     | `4096`         |
| `CORCODE_CONTAINER_CPUS`    | no       | `2`            |
| `CORCODE_WARM_POOL`         | no       | `2`            |
| `CORCODE_REGISTRY_USER`     | no       | -              |
| `CORCODE_REGISTRY_TOKEN`    | no       | -              |
| `CORCODE_GITHUB_TOKEN`      | no       | -              |
| `CORCODE_ANTHROPIC_API_KEY` | no       | -              |

`CORCODE_REPOS` is the comma-separated `owner/name` list the new-chat form
offers, first entry the default. `CORCODE_GITHUB_TOKEN` clones private
repositories and pushes at archive; `CORCODE_ANTHROPIC_API_KEY` reaches the
agent as `ANTHROPIC_API_KEY`. `CORCODE_WARM_POOL` is how many chats keep a
container: the rest are parked, workspaces kept. A chat taking a turn keeps
its container until the turn ends, so the pool can sit one over its size.

`CORCODE_HOST_DATA_DIR` is the dataset root as the Docker daemon sees it,
which differs from `CORCODE_DATA_DIR` when the core is itself containerised:
agent containers are siblings, so their binds resolve against the host. Set
it to the host path bound into the core, and leave it unset anywhere else.

Deploying on TrueNAS: [`deploy/README.md`](deploy/README.md).

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
