//! VT throughput benchmark (the vtebench-equivalent, ): how fast the
//! projection ingests PTY bytes and drains them into `GridDelta`s — the hot
//! path an emulation thread runs. Reported as bytes/sec via criterion's
//! Throughput, so a regression shows up as MB/s dropping.
//!
//!   cargo bench -p ember-session --bench throughput
//!
//! Patterns mirror vtebench's stress cases: dense ASCII scroll, heavy SGR
//! color churn, and cursor-motion-heavy redraws (a TUI like htop).

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use ember_core::{GridDelta, GridDims, VtProjection};
use ember_session::AlacrittyProjection;

use alacritty_terminal::event::VoidListener;

/// A screenful of dense ASCII, newline-terminated — models `cat`-ing a big file
/// (pure scroll throughput).
fn dense_ascii(lines: usize, cols: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(lines * (cols + 1));
    for i in 0..lines {
        for c in 0..cols {
            out.push(b'0' + ((i + c) % 10) as u8);
        }
        out.push(b'\n');
    }
    out
}

/// Heavy SGR churn: every cell wrapped in a distinct 24-bit color — models a
/// syntax-highlighted diff or `ls --color` flood (interner + style pressure).
fn sgr_churn(lines: usize, cols: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..lines {
        for c in 0..cols {
            let (r, g, b) = ((i * 7) % 256, (c * 5) % 256, (i * c) % 256);
            out.extend_from_slice(format!("\x1b[38;2;{r};{g};{b}m#").as_bytes());
        }
        out.extend_from_slice(b"\x1b[0m\r\n");
    }
    out
}

/// Cursor-motion-heavy full-screen repaint — models htop/vim redrawing in place
/// (absolute cursor addressing + partial line rewrites).
fn cursor_motion(lines: usize, cols: usize) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"\x1b[2J");
    for i in 0..lines {
        // Jump to (row, 1), write a bar of variable width.
        out.extend_from_slice(format!("\x1b[{};1H", i + 1).as_bytes());
        let w = 1 + (i * 3) % cols;
        out.extend(std::iter::repeat_n(b'|', w));
    }
    out
}

fn run_stream(bytes: &[u8], dims: GridDims) {
    let mut proj = AlacrittyProjection::new(dims, VoidListener);
    // Feed in 8 KB chunks (the reader's read size), draining after each — the
    // real emulation-loop cadence.
    let mut delta = GridDelta::default();
    for chunk in bytes.chunks(8192) {
        black_box(proj.advance(chunk));
        proj.drain_damage_into(&mut delta);
        delta = GridDelta::default();
    }
}

fn throughput(c: &mut Criterion) {
    let dims = GridDims::new(120, 40);
    let cases: [(&str, Vec<u8>); 3] = [
        ("dense_ascii", dense_ascii(2000, 120)),
        ("sgr_churn", sgr_churn(800, 120)),
        ("cursor_motion", cursor_motion(2000, 120)),
    ];
    let mut group = c.benchmark_group("vt_throughput");
    for (name, bytes) in &cases {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_function(*name, |b| b.iter(|| run_stream(black_box(bytes), dims)));
    }
    group.finish();
}

criterion_group!(benches, throughput);
criterion_main!(benches);
