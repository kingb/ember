//! End to end: OSC 8 hyperlinks written by a program (Claude Code writes its
//! labelled links this way) come out of the projection and the render-side
//! grid model as clickable spans carrying the declared URL, not the label.

use alacritty_terminal::event::VoidListener;
use ember_core::{GridDelta, GridDims, VtProjection};
use ember_render::grid_model::{GridModel, LinkSource};
use ember_session::AlacrittyProjection;

fn spans_for(chunks: &[&[u8]]) -> Vec<ember_render::grid_model::LinkSpan> {
    let dims = GridDims::new(40, 4);
    let mut proj = AlacrittyProjection::new(dims, VoidListener);
    let mut model = GridModel::new(dims);
    for chunk in chunks {
        proj.advance(chunk);
        let mut d = GridDelta::default();
        proj.drain_damage_into(&mut d);
        model.apply(d);
    }
    model.link_spans()
}

#[test]
fn labelled_link_opens_its_declared_url() {
    let spans = spans_for(&[
        b"\xe2\xa7\x89 \x1b]8;;https://claude.ai/artifact/abc\x1b\\MVP checklist\x1b]8;;\x1b\\\r\n",
    ]);
    assert_eq!(spans.len(), 1, "{spans:?}");
    assert_eq!(spans[0].source, LinkSource::Explicit);
    assert_eq!(spans[0].url, "https://claude.ai/artifact/abc");
    assert_eq!(spans[0].cols, 2..15, "the label's columns");
}

#[test]
fn link_learned_in_one_drain_still_resolves_in_later_drains() {
    // The label is drawn across two reads, so the URI ships in the first
    // delta and the second delta's cells reference it by id only.
    let spans = spans_for(&[
        b"\x1b]8;;https://example.com/docs\x1b\\read ",
        b"the docs\x1b]8;;\x1b\\ plain https://example.org/raw",
    ]);
    let explicit: Vec<_> = spans
        .iter()
        .filter(|s| s.source == LinkSource::Explicit)
        .collect();
    assert_eq!(explicit.len(), 1, "{spans:?}");
    assert_eq!(explicit[0].url, "https://example.com/docs");
    assert_eq!(explicit[0].cols, 0..13);
    // Plain-text detection still works alongside.
    assert!(
        spans
            .iter()
            .any(|s| s.source == LinkSource::Detected && s.url == "https://example.org/raw")
    );
}
