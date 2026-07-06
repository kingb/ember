# Clickable URLs — design

Status: approved direction (Brandon, 2026-07-05); spec pending review
Sequencing: PR 1 = auto-detected plain-text URLs; PR 2 (immediately after) =
OSC 8 explicit hyperlinks on the same span layer.

## UX

- URLs in pane text are **always subtly underlined** (a dimmed underline).
  No hidden mode: what is clickable is visible at a glance, like a browser.
- Hovering a URL brightens its underline and switches the mouse cursor to a
  pointer (hand).
- **At a normal shell prompt** (the pane is not mouse-reporting): a **plain
  left click opens** the URL in the default browser via the existing
  `PlatformBackend::open_path` seam. Click-vs-drag is discriminated the
  standard way: the open fires on *release*, only when the press landed on
  the same link and no drag/selection started in between. Drag-to-select
  through a URL keeps working exactly as before.
- **When an app captures the mouse** (vim, tmux, htop — mouse-reporting on):
  plain clicks forward to the app, as they must. Opening falls back to
  **Cmd+click (macOS) / Ctrl+click (Linux)**. Hover feedback inside a
  captured pane appears only while that modifier is held, so the affordance
  always tells the truth about what a click will do.
- **http/https only**, enforced twice: the matcher recognizes only those
  schemes, and the open call re-checks the prefix before spawning anything
  (defense in depth; `file:`, `javascript:` etc. can never reach the OS).

Follow-ups explicitly out of scope for both PRs: other schemes (`mailto:`,
`ftp://`…), a keyboard hint/quick-select mode (Alacritty-style labels), and
user-configurable link rules (regex territory; would live in the app layer).

## Architecture

Three layers, one per crate seam. The span layer is **source-agnostic from
day one** so PR 2 adds a link source without reshaping anything.

### 1. `ember-core::links` — the matcher (pure, PR 1)

```rust
/// A URL found in a line of text: byte range + char range into the input.
pub struct UrlMatch { pub range: Range<usize>, /* byte */ pub chars: Range<usize> }

pub fn find_urls(line: &str) -> Vec<UrlMatch>
```

Hand-rolled scanner, no regex dependency. Rationale: the workspace has no
regex dep today and `ember-core` is the pure leaf crate; the hard parts of
URL detection (trailing-punctuation trimming, balanced-paren counting) are
exactly what regex cannot express — the terminals that use regex for this
all bolt the same logic on as post-processing anyway. Character classes come
from RFC 3986. A scanner is also immune by construction to pathological
backtracking on adversarial output (this code scans untrusted program output
on every grid change).

Algorithm:
1. Find `http://` or `https://` (case-insensitive scheme).
2. If the next char is `[`, consume an IPv6 literal host through the
   matching `]` (dev servers print `http://[::1]:8000/` constantly).
3. Extend over the RFC 3986 charset: alphanumerics and
   `-._~:/?#[]@!$&'()*+,;=%`.
4. Trim the tail:
   - Strip trailing `.` `,` `;` `:` `!` `?` `'` `"` (sentence punctuation).
   - Paren/bracket **depth counting** left-to-right over the URL body: a
     trailing `)` / `]` that closes nothing is trimmed; balanced ones are
     kept. This keeps `…/Rust_(video_game)` intact while excluding the
     wrapper in `(https://example.com)`. Where the grand-four disagree
     (`url))`: WezTerm keeps both, Ghostty trims to balance) we follow the
     depth rule — trim to balance — because it is the principled reading.
5. A match must have at least one host character after the scheme.

### 2. `ember-render` — spans, drawing, hit-test (PR 1 + PR 2)

```rust
pub struct LinkSpan {
    pub row: u16, pub cols: Range<u16>,   // one entry per touched row
    pub url: String,
    pub source: LinkSource,               // Detected | Explicit
}
```

- Computed per pane **when its grid changes** (the existing `dirty` flag —
  no per-frame rescans), stored on `PaneRender`.
- Row text for scanning is built cell-by-cell with an explicit char→column
  map, because wide (CJK/emoji) cells make string index ≠ column.
- **Soft-wrapped rows are joined** via the existing per-cell `wrapped` flag
  before scanning, so a long URL that wraps across rows matches as one link
  with one `LinkSpan` per touched row. (Hard-wrapped URLs — mutt-style — are
  a documented non-goal; kitty is the only terminal that attempts it.)
- Drawing: one dimmed underline quad per span segment, emitted in the same
  decoration pass as SGR underline; the hovered link's segments draw at full
  link color instead.
- Hit-test: `link_at(session, row, col) -> Option<&str>`, same shape as the
  proven `about_link_at`.
- Priority (PR 2): a cell covered by an `Explicit` (OSC 8) span suppresses
  `Detected` matching over the same cells — an app that declares its link
  beats our guess.

### 3. `ember-app` — hover + click (PR 1)

- `CursorMoved`: hit-test the hovered cell; set `CursorIcon::Pointer` and the
  renderer's hovered-link state (both already have precedents: divider-resize
  cursors, `hovered_tab`). In mouse-reporting panes, only while the platform
  open-modifier is held.
- Left press: record `(session, link-identity)` if the press landed on a link
  and either the pane is not mouse-reporting or the modifier is held.
- Left release: if the pointer is still on the same link and no
  drag/selection intervened → `platform.open_path(url)` (after the http/https
  re-check). Otherwise fall through to today's behavior untouched.

### PR 2 — OSC 8 explicit hyperlinks

`alacritty_terminal` already parses OSC 8 and stores an optional hyperlink
(`id` + `uri`) per cell; our projection currently drops it. PR 2:

- `NeutralCell` gains an optional link reference. To keep the cell small and
  the deltas compact, links are interned like styles: a per-frame link table
  (`Vec<String>` of URIs shipped once per new link, mirroring `new_styles`)
  and a `Option<LinkId>` on the cell (serde-defaulted so old serialized
  frames still parse — same trick as the `wide` field).
- The projection resolves alacritty's per-cell hyperlink to a `LinkId`.
- `GridModel` turns runs of same-`LinkId` cells into `Explicit` `LinkSpan`s;
  everything downstream (underline, hover, click) is already built.
- Explicit links are opened with the same http/https-only guard. OSC 8 URIs
  with other schemes (`file://…` from `ls --hyperlink`) still *render* as
  links but refuse to open until a scheme policy exists (follow-up; opening
  arbitrary schemes from untrusted output is a real attack surface).

## Study of prior art (licenses checked)

WezTerm (MIT), Ghostty (MIT), Alacritty (Apache-2.0) — source and tests
studied for behavior and corner cases. kitty (GPL-3.0) — **documented
behavior only, no source examined**, to keep provenance clean. Used for
ideas and expected-behavior comparison; no code copied from any of them.

Key takeaways adopted: Ghostty's trailing-punctuation and paren rules plus
its IPv6 test corpus; WezTerm's bracket-wrapper cases and multi-match
ordering; Alacritty's exclusion-charset sanity check (`<` `>` `"` backtick
whitespace always terminate); kitty's hover affordance (underline + hand)
and its hard-wrap note (documented here as a non-goal).

## Test plan (TDD — the matcher corpus is written first)

`ember-core::links` unit corpus, all asserted as `(input, expected matches)`:

**Basics**
- `hello https://example.com world` → `https://example.com`
- `http://example.com` (http too); `HTTPS://EXAMPLE.COM` (scheme case)
- entire line is the URL; URL at column 0; URL at end of line
- two URLs on one line → both, in order
- `https://` alone (no host) → no match; `http:/broken` → no match
- `dot.http://example.com` → matches from `http` (preceding text ignored)

**Trailing punctuation**
- `https://example.com.` → drops `.`; same for `,` `;` `:` `!` `?`
- `See https://example.com/docs.` vs `https://example.com/v1.2/docs` (dot
  inside path survives; only trailing dots trim)
- `https://example.com...` → all trailing dots trim

**Quotes and wrappers**
- `"https://example.com"` and `'https://example.com'` → inner
- `(https://example.com)` `[https://example.com]` `<https://example.com>` → inner
- Markdown: `[mode 2027](https://github.com/contour/spec)` → inner URL

**Parens/brackets in the URL body (depth rule)**
- `https://en.wikipedia.org/wiki/Rust_(video_game)` → kept whole
- `https://example.com/foo(bar)baz` → kept whole
- `https://example.com/foo(bar))` → trims to `…foo(bar)` (depth)
- `https://example.com)` → trims `)` (nothing to close)
- `https://example.com/[foo]` → kept whole (balanced brackets in path)

**Query/fragment/userinfo/ports/encoding**
- `https://example.com?query=1&other=2` · `…/~user/?q=1#hash`
- `https://user:pass@example.com:8443/path` · `…/a%20b+c=d`

**IPv6 literals**
- `Serving HTTP on :: port 8000 (http://[::]:8000/)` → `http://[::]:8000/`
- `https://[2001:db8::1]:8080/path` · compressed forms · with query/fragment
- bare `[::1]:8000` without a scheme → no match

**Grid integration (ember-render tests)**
- wide chars before/inside-adjacent-to a URL → underline and hit-test land on
  the right columns (char→column map)
- soft-wrapped URL across two and three rows (wrap point mid-scheme,
  mid-host, mid-path) → one logical match, per-row spans, hit-test works on
  every row
- URL in a concealed (SGR 8) region → not matched (hidden text is not a link)
- span cache invalidates when the grid changes, not per frame

**Interaction (live verification via the control socket)**
- echo a URL → screenshot shows the dimmed underline
- `ctl click` on it at a plain prompt → `open_path` fires (verifiable: point
  the opener at a test hook, or assert via the URL reaching the browser)
- drag across the URL → selection works, nothing opens
- inside `vim` with mouse on → plain click does not open; modifier+click does

## Security notes

- Only `http://`/`https://` ever reach `open_path`, checked at both the
  matcher and the open site.
- The matcher runs on untrusted output; the scanner is linear-time by
  construction with no backtracking.
- OSC 8 URIs from untrusted programs render but do not open unless
  http/https (PR 2), pending a real scheme policy.
