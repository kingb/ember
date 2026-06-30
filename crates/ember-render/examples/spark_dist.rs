//! Diagnostic: is the ember emission continuous or bursty? Replicates the phase
//! math from `paint::spark_quads` and, over time, counts how many sparks are
//! "just emitted" (low phase, near the bottom) and how many are visible. A steady
//! emission count = continuous; big swings = bursts.
//!
//!   cargo run -q --example spark_dist

fn frac(x: f32) -> f32 {
    x - x.floor()
}

fn main() {
    let n = 50usize;
    let mut emit_counts = Vec::new();
    let mut t = 0.0f32;
    while t < 20.0 {
        let mut emitting = 0; // phase in [0, 0.08): near the bottom, just born
        for i in 0..n {
            let fi = i as f32;
            let hash = |a: f32, b: f32| {
                let s = ((fi * a + b).sin() * 43758.547).abs();
                s - s.floor()
            };
            let seed = hash(12.9898, 4.1);
            // FIX: stratified phase offset (evenly spread across the cycle) + small
            // jitter, with lifetime from an INDEPENDENT seed so rate doesn't
            // correlate with offset. Guarantees continuous emission.
            let offset = (fi + 0.5 * seed) / n as f32;
            let life = 2.5 + hash(78.233, 1.7) * 2.5;
            let phase = frac(t / life + offset);
            if phase < 0.08 {
                emitting += 1;
            }
        }
        emit_counts.push(emitting);
        t += 0.05;
    }

    let max = *emit_counts.iter().max().unwrap();
    let min = *emit_counts.iter().min().unwrap();
    let mean = emit_counts.iter().sum::<usize>() as f32 / emit_counts.len() as f32;
    // How often is the emission zone empty (a visible gap → "burst" feel)?
    let empties = emit_counts.iter().filter(|&&c| c == 0).count();
    println!("emission-zone (phase<0.08) count over 20s @ 20Hz sampling:");
    println!(
        "  min={min}  max={max}  mean={mean:.2}  empty-frames={empties}/{}",
        emit_counts.len()
    );
    // Crude sparkline of the first ~6s.
    let bars = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let line: String = emit_counts
        .iter()
        .take(120)
        .map(|&c| bars[c.min(7)])
        .collect();
    println!("  first 6s: {line}");
}
