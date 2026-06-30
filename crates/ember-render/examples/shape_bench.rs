//! Micro-benchmark: the cost of ONE full-grid glyphon reshape — the per-frame
//! work that `renderer.rs` Pass-1 does for each visible pane. This is the number
//! behind the "htop renders slowly" question: how expensive is reshaping a whole
//! 80×24 colored grid, and how much does a release build help?
//!
//!   cargo run -q --example shape_bench            # debug
//!   cargo run -q --release --example shape_bench  # release
//!
//! Each iteration regenerates novel content (a rolling counter) so glyphon's
//! shape-run cache can't hide the cost — i.e. it models a fully-changing screen,
//! an upper bound on a real TUI like htop (which changes a subset of cells/tick).

use std::time::Instant;

use glyphon::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping};

fn build_spans(rows: usize, runs_per_row: usize, run_len: usize, iter: usize) -> Vec<(String, Color)> {
    let palette = [
        Color::rgb(0xcc, 0xcc, 0xcc),
        Color::rgb(0x4e, 0xc9, 0xb0),
        Color::rgb(0xff, 0x9d, 0x3c),
        Color::rgb(0x56, 0x9c, 0xd6),
    ];
    let mut spans = Vec::with_capacity(rows * (runs_per_row + 1));
    for r in 0..rows {
        for k in 0..runs_per_row {
            // Vary content by iter so each reshape is novel (defeats caching).
            let base = (b'a' + ((r + k + iter) % 26) as u8) as char;
            let chunk: String = std::iter::repeat(base).take(run_len).collect();
            spans.push((chunk, palette[k % palette.len()]));
        }
        spans.push(("\n".to_string(), palette[0]));
    }
    spans
}

fn main() {
    let cols = 80usize;
    let rows = 24usize;
    let runs_per_row = 4usize; // ~moderately colored, like a TUI
    let run_len = cols / runs_per_row;
    let iters = 300usize;

    let mut fs = FontSystem::new();
    let mut buf = Buffer::new(&mut fs, Metrics::new(12.0, 15.0));
    buf.set_size(&mut fs, Some(cols as f32 * 7.2), Some(rows as f32 * 15.0));
    let attrs = Attrs::new().family(Family::Monospace);

    let reshape = |fs: &mut FontSystem, buf: &mut Buffer, spans: &[(String, Color)]| {
        buf.set_rich_text(
            fs,
            spans
                .iter()
                .map(|(t, c)| (t.as_str(), Attrs::new().family(Family::Monospace).color(*c))),
            &attrs,
            Shaping::Advanced,
            None,
        );
        buf.shape_until_scroll(fs, false);
    };

    for i in 0..5 {
        let spans = build_spans(rows, runs_per_row, run_len, i);
        reshape(&mut fs, &mut buf, &spans);
    }

    let t0 = Instant::now();
    for i in 0..iters {
        let spans = build_spans(rows, runs_per_row, run_len, i + 100);
        reshape(&mut fs, &mut buf, &spans);
    }
    let el = t0.elapsed();
    let per_ms = el.as_secs_f64() * 1000.0 / iters as f64;

    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!(
        "[{profile}] {cols}x{rows} full reshape: {per_ms:.3} ms each  (~{:.0} reshapes/sec max; at 60fps that's {:.1}% of the frame budget)",
        1000.0 / per_ms,
        per_ms / 16.67 * 100.0
    );
}
