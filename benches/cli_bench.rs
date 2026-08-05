//! Criterion benchmark

use std::hint::black_box;

use clap::Parser as _;
use criterion::{Criterion, criterion_group, criterion_main};

use rust_template::cli::{Cli, log_level};

fn bench_arg_parsing(c: &mut Criterion) {
    c.bench_function("parse_example_subcommand", |b| {
        b.iter(|| {
            Cli::try_parse_from(black_box([
                "rust-template",
                "-vv",
                "example",
                "World",
                "--greeting",
                "Hi",
            ]))
        });
    });
}

fn bench_log_level(c: &mut Criterion) {
    c.bench_function("log_level", |b| b.iter(|| log_level(black_box(2))));
}

criterion_group!(benches, bench_arg_parsing, bench_log_level);
criterion_main!(benches);
