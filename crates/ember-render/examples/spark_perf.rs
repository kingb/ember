//! Benchmark: the per-frame CPU cost the ember animation *adds* — i.e. building
//! the spark instance list each frame (`paint::spark_quads`). Everything else a
//! spark frame does (no reshape thanks to the dirty-flag; quad upload; GPU encode)
//! is shared with a normal redraw. This isolates "how expensive are the sparks
//! themselves," to decide whether moving the sim to the GPU is worth it.
//!
//!   cargo run -q --example spark_perf            # debug
//!   cargo run -q --release --example spark_perf  # release

use std::time::Instant;

fn frac(x: f32) -> f32 {
    x - x.floor()
}

/// Mirrors `paint::spark_quads` (post burst-fix): returns the count of quads built.
fn build_sparks(density: f32, t: f32, w: f32, h: f32) -> usize {
    use std::f32::consts::PI;
    let n = ((50.0 * density).round() as i32).clamp(0, 240) as usize;
    let mut out: Vec<([f32; 4], [f32; 4])> = Vec::with_capacity(n);
    for i in 0..n {
        let fi = i as f32;
        let hash = |a: f32, b: f32| {
            let s = ((fi * a + b).sin() * 43758.547).abs();
            s - s.floor()
        };
        let seed = hash(12.9898, 4.1);
        let offset = (fi + 0.5 * seed) / n as f32;
        let life = 2.5 + hash(78.233, 1.7) * 2.5;
        let phase = frac(t / life + offset);
        let base_x = hash(37.719, 2.3) * w;
        let x = base_x + (t * 0.6 + fi * 1.7).sin() * (10.0 + seed * 14.0);
        let y = h + 12.0 - phase * (h + 24.0);
        let flicker = 0.8 + 0.2 * (t * (8.0 + seed * 6.0) + fi).sin();
        let alpha = (PI * phase).sin().max(0.0) * 0.85 * flicker;
        let size = 2.0 + seed * 4.0;
        // rect + rgba (the lin_rgba/lerp_rgb color math is comparable arithmetic).
        out.push((
            [x - size * 0.5, y - size * 0.5, size, size],
            [1.0, 0.6, 0.2, alpha],
        ));
    }
    out.len()
}

fn bench(density: f32) {
    let iters = 100_000usize;
    let (w, h) = (1400.0f32, 900.0f32);
    let mut sink = 0usize;
    // warm
    for i in 0..1000 {
        sink ^= build_sparks(density, i as f32 * 0.016, w, h);
    }
    let t0 = Instant::now();
    for i in 0..iters {
        sink ^= build_sparks(density, i as f32 * 0.016, w, h);
    }
    let el = t0.elapsed();
    let per_us = el.as_secs_f64() * 1e6 / iters as f64;
    let n = ((50.0 * density).round() as i32).clamp(0, 240);
    println!(
        "  density {density:.1} ({n:>3} sparks): {per_us:.3} µs/frame  ({:.4}% of a 16.67ms frame)  [sink={}]",
        per_us / 16_670.0 * 100.0,
        sink & 1
    );
}

fn main() {
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    println!("[{profile}] per-frame spark-sim CPU cost (paint::spark_quads):");
    for d in [0.5f32, 1.0, 2.0] {
        bench(d);
    }
}
