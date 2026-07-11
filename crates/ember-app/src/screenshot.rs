//! Headless screenshot driver (debug / self-review;  follow-up).
//!
//! `ember-term --screenshot out.png [flags]` builds a deterministic multiplexer
//! scene with the *real* `ember-core` layout + real `LocalPty` shells, lets the
//! shells settle, and renders one frame to a PNG via `ember_render::headless`.
//! This is how the (display-less) agent and CI can actually *see* what the
//! renderer produces. Pass `--scale 2` to reproduce a Retina target.

use std::collections::HashMap;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use ember_core::{
    Axis, BackendControl, BackendHandle, LayoutCommand, LayoutNode, PaneId, Rect, SessionBackend,
    SessionId, Tab, TabId, WindowTree, apply, layout,
};
use ember_render::headless::{self, PaneShot, Shot};
use ember_render::{GridModel, Renderer, TabLabel};
use ember_session::{LocalPty, LocalPtyConfig};

use crate::{PAD, dims_for_rect, inset};

/// A parsed `--screenshot` invocation.
pub struct Opts {
    pub path: String,
    pub scale: f32,
    /// Logical window size in px.
    pub width: f32,
    pub height: f32,
    /// Split the active tab once, if set.
    pub split: Option<Axis>,
    /// Commands to run, one per pane (index 0, 1, …); empty → bare prompt.
    pub runs: Vec<String>,
    /// Total tabs (>=1); extras exist to show the tab strip.
    pub tabs: usize,
    /// Tab-drag preview: `(dragged slot, cursor x logical)` — for the lifted tab.
    pub tab_drag: Option<(usize, f32)>,
    /// Hovered tab (0-based) — draws the hover highlight + "✕" close affordance.
    pub hover_tab: Option<usize>,
    /// Draw a sample "Close this tab?" confirm modal over the panes.
    pub confirm: bool,
    /// Split drop-zone preview on the focused pane: `(horizontal, ratio)`.
    pub split_preview: Option<(bool, f32)>,
    /// How long to let the shells produce output before capturing.
    pub settle_ms: u64,
    /// Draw the campfire backdrop (warm gradient + legibility scrim).
    pub backdrop: bool,
    /// Draw the drifting ember sparks (additive glow).
    pub ember: bool,
    /// Animation time (seconds) to pin the sparks at, for a deterministic frame.
    pub ember_phase: f32,
    /// Path to a backdrop image (PNG) to draw behind the cells.
    pub bg_image: Option<String>,
    /// Backdrop image fit: `cover` | `contain` | `stretch` | `tile`.
    pub bg_fit: String,
    /// A selection to highlight on the active pane: `(r1, c1, r2, c2)`.
    pub select: Option<(u16, u16, u16, u16)>,
    /// Selection mode: `simple` | `word` | `line`.
    pub select_mode: String,
    /// FPS/frame-time debug overlay text (bottom-right), for verifying its layout.
    pub fps: Option<String>,
    /// Visual-bell flash intensity (`0..1`) over the panes.
    pub bell: f32,
    /// Mark this tab index as having an unseen bell (draws the amber dot).
    pub bell_tab: Option<usize>,
    /// Terminal font point size.
    pub font_size: f32,
    /// Terminal font family (None → monospace default).
    pub font: Option<String>,
    /// Draw the keyboard-shortcuts cheat-sheet overlay (Cmd+/) over the panes.
    pub help_overlay: bool,
    /// Draw the Settings overlay (Cmd+,), for documenting it. Its rows resolve
    /// from `--font`/`--font-size` so a doc shot can show a chosen font, and
    /// the highlight lands on the Font family row.
    pub settings: bool,
    /// Hold-to-wisp ring (v1.1) preview: `(logical x, logical y, progress
    /// 0..1)` — debug plumbing so the ring's quad geometry can be eyeballed
    /// via a headless screenshot without a live windowed hold.
    pub hold_ring: Option<(f32, f32, f32)>,
    /// `--wisp-preview <style>` (v0.4.1's 6-style wisp): when set, `run`
    /// skips the whole pane-based scene entirely and instead renders that
    /// style's particle cluster (one animated frame, `t`/`intensity`/
    /// `velocity` fixed for a deterministic, comparable shot) onto an
    /// opaque dark canvas — see [`ember_render::headless::capture_wisp_preview`].
    /// Accepts `cinder`|`coal`|`willowisp`|`comet`|`goo`|`star`
    /// (case-insensitive; `ember` is the pre-v0.4.1 alias for `cinder`);
    /// anything else is a hard parse error (this is debug tooling, not the
    /// forgiving `config.toml` `wisp_style` knob — a typo here should fail
    /// loudly, not silently render the wrong style).
    pub wisp_preview: Option<ember_core::WispStyle>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            path: "ember.png".to_string(),
            scale: 2.0,
            width: 900.0,
            height: 560.0,
            split: None,
            runs: Vec::new(),
            tabs: 1,
            tab_drag: None,
            hover_tab: None,
            confirm: false,
            split_preview: None,
            settle_ms: 700,
            backdrop: false,
            ember: false,
            ember_phase: 1.4,
            bg_image: None,
            bg_fit: "cover".to_string(),
            select: None,
            select_mode: "simple".to_string(),
            fps: None,
            bell: 0.0,
            bell_tab: None,
            font_size: 12.0,
            font: None,
            help_overlay: false,
            settings: false,
            hold_ring: None,
            wisp_preview: None,
        }
    }
}

/// Parse a `--wisp-preview` style name into a concrete [`ember_core::WispStyle`].
/// Case-insensitive over the same 6 names `config.toml`'s `wisp_style` accepts
/// (not `"random"` — a preview names one concrete style). Unlike the config
/// loader, an unrecognized name is a hard `Err`: this is debug/comparison
/// tooling, so a typo should fail loudly rather than silently fall back.
fn parse_wisp_style(s: &str) -> Result<ember_core::WispStyle, String> {
    use ember_core::WispStyle;
    match s.to_ascii_lowercase().as_str() {
        "cinder" => Ok(WispStyle::Cinder),
        "ember" => Ok(WispStyle::Cinder), // pre-v0.4.1 name, kept as an alias
        "coal" => Ok(WispStyle::Coal),
        "willowisp" => Ok(WispStyle::WillOWisp),
        "comet" => Ok(WispStyle::Comet),
        "goo" => Ok(WispStyle::Goo),
        "star" => Ok(WispStyle::Star),
        other => Err(format!(
            "--wisp-preview: unknown style {other:?} (expected cinder|coal|willowisp|comet|goo|star)"
        )),
    }
}

/// Parse the flags following `--screenshot`. Errors on a malformed value.
pub fn parse(args: &[String]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        let mut next = || {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("{arg} needs a value"))
        };
        match arg.as_str() {
            "--screenshot" => opts.path = next()?,
            "--scale" => opts.scale = next()?.parse().map_err(|e| format!("--scale: {e}"))?,
            "--font-size" => {
                opts.font_size = next()?.parse().map_err(|e| format!("--font-size: {e}"))?
            }
            "--font" => opts.font = Some(next()?),
            "--width" => opts.width = next()?.parse().map_err(|e| format!("--width: {e}"))?,
            "--height" => opts.height = next()?.parse().map_err(|e| format!("--height: {e}"))?,
            "--tabs" => opts.tabs = next()?.parse().map_err(|e| format!("--tabs: {e}"))?,
            "--hover-tab" => {
                opts.hover_tab = Some(next()?.parse().map_err(|e| format!("--hover-tab: {e}"))?)
            }
            "--confirm" => opts.confirm = true,
            "--help-overlay" => opts.help_overlay = true,
            "--settings" => opts.settings = true,
            "--tab-drag" => {
                let slot = next()?
                    .parse()
                    .map_err(|e| format!("--tab-drag slot: {e}"))?;
                let cx = next()?.parse().map_err(|e| format!("--tab-drag x: {e}"))?;
                opts.tab_drag = Some((slot, cx));
            }
            "--split-preview" => {
                let h = next()?.as_str() == "h"; // h = side-by-side, else stacked
                let ratio = next()?
                    .parse()
                    .map_err(|e| format!("--split-preview ratio: {e}"))?;
                opts.split_preview = Some((h, ratio));
            }
            "--wisp-preview" => {
                opts.wisp_preview = Some(parse_wisp_style(&next()?)?);
            }
            "--hold-ring" => {
                let x = next()?.parse().map_err(|e| format!("--hold-ring x: {e}"))?;
                let y = next()?.parse().map_err(|e| format!("--hold-ring y: {e}"))?;
                let progress = next()?
                    .parse()
                    .map_err(|e| format!("--hold-ring progress: {e}"))?;
                opts.hold_ring = Some((x, y, progress));
            }
            "--settle" => opts.settle_ms = next()?.parse().map_err(|e| format!("--settle: {e}"))?,
            "--split" => {
                opts.split = Some(match next()?.as_str() {
                    "h" | "horizontal" => Axis::Horizontal,
                    "v" | "vertical" => Axis::Vertical,
                    other => return Err(format!("--split expects h|v, got {other}")),
                })
            }
            "--run" => opts.runs.push(next()?),
            "--backdrop" => opts.backdrop = true,
            "--ember" => {
                opts.backdrop = true;
                opts.ember = true;
            }
            "--ember-phase" => {
                opts.ember_phase = next()?.parse().map_err(|e| format!("--ember-phase: {e}"))?
            }
            "--bg-image" => opts.bg_image = Some(next()?),
            "--bg-fit" => opts.bg_fit = next()?,
            "--select" => {
                let v = next()?;
                let nums: Vec<u16> = v
                    .split(',')
                    .map(|s| s.trim().parse())
                    .collect::<Result<_, _>>()
                    .map_err(|e| format!("--select expects r1,c1,r2,c2: {e}"))?;
                if nums.len() != 4 {
                    return Err("--select expects r1,c1,r2,c2".to_string());
                }
                opts.select = Some((nums[0], nums[1], nums[2], nums[3]));
            }
            "--select-mode" => opts.select_mode = next()?,
            "--fps" => opts.fps = Some(next()?),
            "--bell" => opts.bell = next()?.parse().map_err(|e| format!("--bell: {e}"))?,
            "--bell-tab" => {
                opts.bell_tab = Some(next()?.parse().map_err(|e| format!("--bell-tab: {e}"))?)
            }
            _ => {}
        }
        i += 1;
    }
    opts.tabs = opts.tabs.max(1);
    Ok(opts)
}

/// Build the scene, run it, and write the PNG. Returns the output path.
pub fn run(opts: Opts) -> Result<String, String> {
    // `--wisp-preview <style>`: a completely separate, much smaller path —
    // no pane tree, no shells to spawn/settle, just one style's particle
    // cluster rendered onto an opaque dark canvas at a fixed, representative
    // animated frame (some velocity, so the trail/tail geometry is visible
    // too). The pane-scene machinery below never runs for this case.
    if let Some(style) = opts.wisp_preview {
        headless::capture_wisp_preview(
            style,
            0.4,             // t: mid-animation, not the (less interesting) t=0 start
            1.0,             // intensity: full, steady-state (no fade-in/out ramp)
            (180.0, -120.0), // velocity: up-and-right, exercises trails/tails
            230.0,           // logical size: matches the live wisp window (WISP_SIZE)
            opts.scale,
            Path::new(&opts.path),
        )
        .map_err(|e| e.to_string())?;
        return Ok(opts.path);
    }

    let (cw, ch) = headless::cell_metrics_for(opts.font_size, opts.font.as_deref());
    let pad = PAD as f64;
    let chrome = Renderer::chrome_height() as f64;
    let vp = Rect::new(
        0.0,
        chrome,
        opts.width as f64,
        (opts.height as f64 - chrome).max(1.0),
    );

    // Active tab (tab 0), optionally split once; extra tabs are bare shells that
    // only populate the strip.
    let mut next_pane = 1u64;
    let mut next_sess = 1u64;
    let alloc = |np: &mut u64, ns: &mut u64| {
        let p = PaneId(*np);
        let s = SessionId::new(format!("s{}", *ns));
        *np += 1;
        *ns += 1;
        (p, s)
    };
    let (p1, s1) = alloc(&mut next_pane, &mut next_sess);
    let mut tree = WindowTree {
        tabs: vec![Tab {
            id: TabId(1),
            title: String::new(),
            root: LayoutNode::pane(p1, s1),
            focus: p1,
        }],
        active: 0,
    };
    if let Some(axis) = opts.split {
        let (p2, s2) = alloc(&mut next_pane, &mut next_sess);
        apply(
            &mut tree,
            LayoutCommand::SplitPane {
                target: p1,
                axis,
                ratio: 0.5,
                new_pane: p2,
                new_session: s2,
                min_px: 0.0, // screenshot scenes are fixed-size; no min constraint
            },
            vp,
        );
    }
    for t in 1..opts.tabs {
        let (pid, sid) = alloc(&mut next_pane, &mut next_sess);
        apply(
            &mut tree,
            LayoutCommand::NewTab {
                id: TabId((t + 1) as u64),
                session: sid,
                pane: pid,
            },
            vp,
        );
    }
    tree.active = 0;

    // Spawn a real shell per active-tab leaf, sized to its rect; run the command.
    let rectmap: HashMap<PaneId, Rect> = layout(&tree.tabs[0].root, vp).into_iter().collect();
    let leaves = tree.tabs[0].root.leaves();
    let mut panes: Vec<(SessionId, BackendHandle, GridModel, Rect)> = Vec::new();
    for (idx, (pid, sid)) in leaves.iter().enumerate() {
        let inner = inset(rectmap[pid], pad);
        let dims = dims_for_rect(inner, cw, ch);
        let handle = LocalPty::spawn(LocalPtyConfig::new(sid.clone(), dims))
            .map_err(|e| format!("spawn {sid:?}: {e}"))?;
        if let Some(cmd) = opts.runs.get(idx).filter(|c| !c.is_empty()) {
            let line = format!("{cmd}\n");
            let _ = handle
                .control
                .send(BackendControl::Input(line.into_bytes().into_boxed_slice()));
        }
        panes.push((sid.clone(), handle, GridModel::new(dims), inner));
    }

    // Let the shells produce output, draining the pixel lanes into the grids.
    let deadline = Instant::now() + Duration::from_millis(opts.settle_ms);
    while Instant::now() < deadline {
        for (_, handle, grid, _) in panes.iter_mut() {
            while let Some(delta) = handle.frames.take() {
                grid.apply(delta);
            }
        }
        thread::sleep(Duration::from_millis(15));
    }
    for (_, handle, grid, _) in panes.iter_mut() {
        while let Some(delta) = handle.frames.take() {
            grid.apply(delta);
        }
    }

    // Capture.
    let focus_session = tree.tabs[0].root.session_of(tree.tabs[0].focus).cloned();
    let selection = opts.select.map(|(r1, c1, r2, c2)| {
        let mode = match opts.select_mode.as_str() {
            "word" => ember_render::SelectionMode::Word,
            "line" => ember_render::SelectionMode::Line,
            _ => ember_render::SelectionMode::Simple,
        };
        let mut s = ember_render::Selection::new(ember_render::Point::new(r1, c1), mode);
        s.update(ember_render::Point::new(r2, c2));
        s
    });
    let shots: Vec<PaneShot> = panes
        .iter()
        .map(|(sid, _, grid, rect)| {
            let focused = Some(sid) == focus_session.as_ref();
            PaneShot {
                grid,
                rect: *rect,
                focused,
                selection: if focused { selection } else { None },
                // The CLI demo `--split` flag only ever demos the
                // Ctrl+Opt manual-split case (new pane on the far side).
                split_preview: if focused {
                    opts.split_preview.map(|(h, r)| (h, r, false))
                } else {
                    None
                },
            }
        })
        .collect();
    let tabs: Vec<TabLabel> = tree
        .tabs
        .iter()
        .enumerate()
        .map(|(i, t)| TabLabel {
            title: if t.title.is_empty() {
                format!("{}", i + 1)
            } else {
                t.title.clone()
            },
            active: i == tree.active,
            editing: false,
            bell: opts.bell_tab == Some(i),
        })
        .collect();
    let shot = Shot {
        logical_w: opts.width,
        logical_h: opts.height,
        scale: opts.scale,
        panes: shots,
        tabs,
        tab_drag: opts.tab_drag,
        hovered_tab: opts.hover_tab,
        help: opts.help_overlay.then(crate::help_lines),
        help_title: None,
        about: None,
        settings: opts.settings.then(|| {
            // Resolve the overlay from a config reflecting --font/--font-size,
            // so a doc shot can showcase a specific font. Highlight the Font
            // family row (fall back to the first selectable row otherwise).
            let mut config = ember_core::Config::default();
            config.font.family = opts.font.clone();
            config.font.size = opts.font_size;
            let rows = ember_core::resolve_rows(&config);
            let sel = rows
                .iter()
                .position(|r| r.label == "Font family")
                .unwrap_or(1);
            (rows, sel)
        }),
        backdrop: ember_render::BackdropParams {
            gradient: opts.backdrop,
            // An image backdrop supplies its own base; keep the scrim on so text
            // stays legible over either a gradient or an image.
            scrim: if opts.backdrop || opts.bg_image.is_some() {
                0.4
            } else {
                0.0
            },
            sparks: opts.ember,
            density: 1.0,
            time: opts.ember_phase,
            // Trail sizing must match the cadence rendered frames will be
            // played back at (animation GIFs step this via EMBER_SPARK_DT);
            // stills default to a 30fps-equivalent streak length.
            frame_dt: std::env::var("EMBER_SPARK_DT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1.0 / 30.0),
        },
        image: opts
            .bg_image
            .as_deref()
            .and_then(crate::load_backdrop_image),
        image_fit: ember_render::ImageFit::parse(&opts.bg_fit),
        fps_overlay: opts.fps.clone(),
        search_bar: None,
        ime_preedit: None,
        bell_flash: opts.bell,
        font_size: opts.font_size,
        font_family: opts.font.clone(),
        confirm: opts.confirm.then(|| ember_render::ConfirmView {
            title: "Close this tab?".to_string(),
            message: "The command is still running.".to_string(),
            cancel_label: "Cancel".to_string(),
            confirm_label: "Close".to_string(),
            focused: 0,
        }),
        hold_ring: opts.hold_ring,
        // No offline `--screenshot` flag for these (v0.4.0): both are live
        // cross-window-drag/tear-off previews with no meaningful "just render
        // this state" CLI equivalent — `ctl screenshot` against a live,
        // scripted `ctl drag` is the verification path (design doc).
        ghost_tab: None,
        morph: None,
    };
    if opts.bg_image.is_some() && shot.image.is_none() {
        return Err(format!(
            "--bg-image: could not load {:?} as a PNG",
            opts.bg_image.as_deref().unwrap_or("")
        ));
    }
    headless::capture(&shot, Path::new(&opts.path)).map_err(|e| e.to_string())?;

    for (_, handle, _, _) in &panes {
        let _ = handle.control.send(BackendControl::Shutdown);
    }
    Ok(opts.path)
}
