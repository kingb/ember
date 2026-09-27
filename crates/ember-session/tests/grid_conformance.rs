//! Cross-producer conformance: Ember's local projection against the shared
//! `gridwire` grid corpus (`conformance/grid/` in the public gridwire repo).
//!
//! This is a conformance test, not a unit test. Its value is that the corpus is
//! not Ember's: it asserts that this projection turns each input into the same
//! delta as every other producer of the neutral grid, byte-for-byte on the wire.
//! Ember's own unit tests can cover every line of the projection and still miss
//! a disagreement with another producer; only this catches that. Keep it
//! separate from unit coverage and don't fold it into ordinary tests.
//!
//! The corpus is fetched at the pinned revision in `conformance/GRIDWIRE_REV` by
//! `scripts/conformance/fetch-grid-corpus.sh`, which CI runs before this test.
//! Locally:
//!
//! ```sh
//! scripts/conformance/fetch-grid-corpus.sh
//! cargo test -p ember-session --test grid_conformance -- --ignored
//! ```
//!
//! A missing, empty, or half-paired corpus FAILS rather than skipping: a
//! conformance test that can quietly not run is no guarantee.

use std::path::PathBuf;

use alacritty_terminal::event::VoidListener;
use ember_core::{GridDelta, GridDims, VtProjection};
use ember_session::AlacrittyProjection;
use serde::Deserialize;
use serde_json::Value;

/// One corpus input: exactly one of `feed` (drain once) or `steps` (drain
/// after each chunk).
#[derive(Deserialize)]
struct CorpusInput {
    dims: GridDims,
    #[serde(default)]
    feed: Option<String>,
    #[serde(default)]
    steps: Option<Vec<String>>,
}

/// Where the fetched corpus lives: `EMBER_GRID_CORPUS` if set, else the fetch
/// script's default under `target/`.
fn corpus_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("EMBER_GRID_CORPUS") {
        return PathBuf::from(dir);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/gridwire-corpus/conformance/grid")
}

/// Feed each chunk into a fresh projection and drain after each, exactly as a
/// producer ships frames.
fn produce(dims: GridDims, chunks: &[String]) -> Vec<GridDelta> {
    let mut proj = AlacrittyProjection::new(dims, VoidListener);
    chunks
        .iter()
        .map(|chunk| {
            proj.advance(chunk.as_bytes());
            let mut delta = GridDelta::default();
            proj.drain_damage_into(&mut delta);
            delta
        })
        .collect()
}

/// The first place two JSON values differ, as a readable path, so a failure
/// names the field or cell instead of dumping two whole frames.
fn first_difference(path: &str, got: &Value, want: &Value) -> Option<String> {
    match (got, want) {
        (Value::Object(g), Value::Object(w)) => {
            let mut keys: Vec<&String> = g.keys().chain(w.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| match (g.get(k), w.get(k)) {
                (Some(gv), Some(wv)) => first_difference(&format!("{path}.{k}"), gv, wv),
                (Some(gv), None) => Some(format!(
                    "{path}.{k}: produced {gv}, corpus has no such field"
                )),
                (None, Some(wv)) => Some(format!(
                    "{path}.{k}: missing from produced, corpus expects {wv}"
                )),
                (None, None) => None,
            })
        }
        (Value::Array(g), Value::Array(w)) => g
            .iter()
            .zip(w)
            .enumerate()
            .find_map(|(i, (gv, wv))| first_difference(&format!("{path}[{i}]"), gv, wv))
            .or_else(|| {
                (g.len() != w.len()).then(|| {
                    format!(
                        "{path}: produced {} entries, corpus expects {}",
                        g.len(),
                        w.len()
                    )
                })
            }),
        _ => (got != want).then(|| format!("{path}: produced {got}, corpus expects {want}")),
    }
}

/// The first field present in the corpus's JSON that is missing, or changed,
/// after decoding into Ember's type and re-encoding: that is, something the
/// corpus says that Ember's `GridDelta` can't carry.
fn dropped_field(path: &str, corpus: &Value, decoded: &Value) -> Option<String> {
    match (corpus, decoded) {
        (Value::Object(c), Value::Object(d)) => c.iter().find_map(|(k, cv)| match d.get(k) {
            Some(dv) => dropped_field(&format!("{path}.{k}"), cv, dv),
            None => Some(format!("{path}.{k}")),
        }),
        (Value::Array(c), Value::Array(d)) if c.len() == d.len() => c
            .iter()
            .zip(d)
            .enumerate()
            .find_map(|(i, (cv, dv))| dropped_field(&format!("{path}[{i}]"), cv, dv)),
        _ => (corpus != decoded).then(|| path.to_string()),
    }
}

#[test]
#[ignore = "needs the gridwire corpus: run scripts/conformance/fetch-grid-corpus.sh, then --ignored"]
fn local_projection_agrees_with_the_grid_corpus() {
    let dir = corpus_dir();
    let entries = std::fs::read_dir(&dir).unwrap_or_else(|e| {
        panic!(
            "grid corpus not found at {} ({e}). Run scripts/conformance/fetch-grid-corpus.sh \
             or set EMBER_GRID_CORPUS. This test fails rather than skipping on a missing corpus.",
            dir.display()
        )
    });

    let mut names: Vec<String> = entries
        .map(|e| {
            e.expect("corpus dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|n| n.ends_with(".json") && !n.ends_with(".expect.json"))
        .collect();
    names.sort();
    assert!(
        !names.is_empty(),
        "grid corpus at {} has no cases",
        dir.display()
    );

    let mut failures = Vec::new();
    for name in &names {
        let stem = &name[..name.len() - ".json".len()];
        let input: CorpusInput = serde_json::from_str(
            &std::fs::read_to_string(dir.join(name)).expect("read corpus input"),
        )
        .unwrap_or_else(|e| panic!("case '{stem}': input is malformed: {e}"));
        let expect_raw = std::fs::read_to_string(dir.join(format!("{stem}.expect.json")))
            .unwrap_or_else(|_| panic!("case '{stem}': no {stem}.expect.json pair"));
        let want: Value = serde_json::from_str(&expect_raw)
            .unwrap_or_else(|e| panic!("case '{stem}': expected output is not JSON: {e}"));

        // A `feed` case expects one delta, a `steps` case an array: normalize
        // both to a list so each drain is checked the same way.
        let (got, want_frames, indexed) = match (input.feed, input.steps) {
            (Some(feed), None) => (produce(input.dims, &[feed]), vec![want], false),
            (None, Some(steps)) => {
                let Value::Array(frames) = want else {
                    panic!("case '{stem}': a `steps` case expects an array of deltas");
                };
                assert_eq!(
                    frames.len(),
                    steps.len(),
                    "case '{stem}': {} steps but {} expected deltas",
                    steps.len(),
                    frames.len()
                );
                (produce(input.dims, &steps), frames, true)
            }
            _ => panic!("case '{stem}': input must have exactly one of `feed` or `steps`"),
        };

        for (i, (got, want_json)) in got.iter().zip(&want_frames).enumerate() {
            let at = if indexed {
                format!("[{i}]")
            } else {
                String::new()
            };
            // The corpus omits fields at their defaults (the wire types read
            // them with `#[serde(default)]`), so equality is on the decoded
            // delta, not on raw JSON text.
            let want: GridDelta = serde_json::from_value(want_json.clone()).unwrap_or_else(|e| {
                panic!("case '{stem}'{at}: expected delta doesn't decode: {e}")
            });
            // Guard against a silent pass: every field the corpus states must
            // survive into Ember's type. A field Ember can't represent would
            // otherwise be dropped on decode and never compared.
            let decoded = serde_json::to_value(&want).expect("serialize expected delta");
            if let Some(lost) = dropped_field("", want_json, &decoded) {
                failures.push(format!(
                    "  {stem}{at}: corpus field {lost} has no counterpart in Ember's GridDelta"
                ));
                continue;
            }
            if *got != want {
                let got_json = serde_json::to_value(got).expect("serialize produced delta");
                let diff = first_difference(&at, &got_json, &decoded)
                    .unwrap_or_else(|| "differs (no JSON-visible difference)".into());
                failures.push(format!("  {stem}: {diff}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "Ember's projection disagrees with the gridwire corpus on {} of {} case(s):\n{}\n\
         The corpus is the contract shared with other producers; fix the projection, or \
         raise it with the corpus owners if the corpus is wrong. Don't edit expectations here.",
        failures.len(),
        names.len(),
        failures.join("\n")
    );
    println!("grid conformance: all {} corpus cases agree", names.len());
}
