//! Placeholder perf harness wired into CI from commit #1 (design §6, §11).
//!
//! Real keypress-to-glyph latency and flood-throughput benchmarks land in
//! Epic B (B8). This keeps the harness compiling and runnable so perf gates
//! are structural ("seen, not felt"), not retrofitted.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn placeholder(c: &mut Criterion) {
    c.bench_function("placeholder_noop", |b| {
        b.iter(|| black_box(ember_core::version()));
    });
}

criterion_group!(benches, placeholder);
criterion_main!(benches);
