//! iTerm2 tab-color OSC 6 scanner.
//!
//! Mirrors [`crate::osc133`]'s scan-and-resync approach (split-across-reads
//! carry, malformed-mid-buffer resync) rather than sharing code with it, for
//! the same reason `osc633`/`osc1337` do: the scanners are short,
//! independently tested, and diverging here can't regress an
//! already-hardened path.
//!
//! Sequence shape: `ESC ] 6 ; <params> (BEL | ESC \)`. iTerm2's tab-color
//! protocol reports one channel of the tab's RGB color per sequence, or
//! resets to the default. Only the report-1 form is tracked (report 2 is a
//! different, unsupported request/response variant):
//!   `1;bg;red;brightness;<0-255>`   — set the red channel
//!   `1;bg;green;brightness;<0-255>` — set the green channel
//!   `1;bg;blue;brightness;<0-255>`  — set the blue channel
//!   `1;bg;*;default`                — reset to no tab color

/// Which channel a [`Osc6::Channel`] report sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chan {
    Red,
    Green,
    Blue,
}

/// A parsed OSC 6 sequence (the tracked subset).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Osc6 {
    /// `1;bg;<red|green|blue>;brightness;<0-255>` — set one channel.
    Channel(Chan, u8),
    /// `1;bg;*;default` — reset to no tab color.
    Reset,
}

const PREFIX: &[u8] = b"\x1b]6;";

/// Longest sequence we'll treat as "possibly split across reads". Real OSC 6
/// marks are tens of bytes; anything longer unterminated is garbage (binary
/// output that happened to contain the prefix), not a split.
const MAX_SEQ: usize = 256;

/// One scan pass: the complete marks found, plus where a **possibly split**
/// sequence starts at the end of the buffer, so the caller can carry those
/// bytes into the next read.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Each mark, paired with the byte index just past its terminator.
    pub marks: Vec<(usize, Osc6)>,
    /// Start of an incomplete suffix to carry into the next read, if any.
    pub incomplete: Option<usize>,
}

/// Full scan with split-detection. A malformed sequence mid-buffer (bare ESC,
/// oversized params, unrecognized body) resyncs and keeps scanning — it must
/// not suppress later legitimate marks in the same read; only a *plausible*
/// split at the buffer end is reported as `incomplete`.
pub fn scan_split(bytes: &[u8]) -> ScanResult {
    let mut out = ScanResult::default();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        // How much of the prefix is present starting here?
        let n = PREFIX.len().min(len - i);
        if bytes[i..i + n] != PREFIX[..n] {
            i += 1;
            continue;
        }
        if n < PREFIX.len() {
            // A prefix fragment ends the buffer — possibly split across reads.
            out.incomplete = Some(i);
            break;
        }
        let start = i + PREFIX.len();
        // Find the terminator: BEL (0x07) or ST (ESC \).
        let mut j = start;
        let mut term: Option<usize> = None;
        let mut malformed = false;
        while j < len {
            if j - start > MAX_SEQ {
                malformed = true; // unterminated garbage, not a split
                break;
            }
            match bytes[j] {
                0x07 => {
                    term = Some(j);
                    break;
                }
                0x1b if j + 1 < len && bytes[j + 1] == 0x5c => {
                    term = Some(j);
                    break;
                }
                // A bare ESC mid-buffer is malformed — but it may START a new
                // sequence (or split ST at the very end, handled below).
                0x1b if j + 1 < len => {
                    malformed = true;
                    break;
                }
                _ => j += 1,
            }
        }
        match (term, malformed) {
            (Some(t), _) => {
                let past = if bytes[t] == 0x07 { t + 1 } else { t + 2 };
                if let Some(ev) = parse_body(&bytes[start..t]) {
                    out.marks.push((past, ev));
                }
                i = past;
            }
            (None, true) => {
                // Resync AT the offending byte — it may begin a new prefix.
                i = j.max(i + 1);
            }
            (None, false) => {
                // Ran off the end of the buffer inside a plausible sequence
                // (including a trailing lone ESC of a split ST).
                out.incomplete = Some(i);
                break;
            }
        }
    }
    out
}

/// Parse the bytes between the `6;` prefix and the terminator. Only the
/// report-1 form (`1;bg;...`) is tracked; report 2 and any other shape is
/// left as a no-op mark (dropped, not an error — matches OSC 133/633's
/// treatment of subcommands they don't track).
fn parse_body(body: &[u8]) -> Option<Osc6> {
    let s = std::str::from_utf8(body).ok()?;
    let mut parts = s.split(';');
    if parts.next()? != "1" {
        return None;
    }
    if parts.next()? != "bg" {
        return None;
    }
    let selector = parts.next()?;
    let kind = parts.next()?;
    match (selector, kind) {
        ("red", "brightness") => Some(Osc6::Channel(Chan::Red, parts.next()?.parse().ok()?)),
        ("green", "brightness") => Some(Osc6::Channel(Chan::Green, parts.next()?.parse().ok()?)),
        ("blue", "brightness") => Some(Osc6::Channel(Chan::Blue, parts.next()?.parse().ok()?)),
        ("*", "default") => Some(Osc6::Reset),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn marks(bytes: &[u8]) -> Vec<Osc6> {
        scan_split(bytes)
            .marks
            .into_iter()
            .map(|(_, m)| m)
            .collect()
    }

    #[test]
    fn channel_sequences_parse() {
        assert_eq!(
            marks(b"\x1b]6;1;bg;red;brightness;255\x07"),
            vec![Osc6::Channel(Chan::Red, 255)]
        );
        assert_eq!(
            marks(b"\x1b]6;1;bg;green;brightness;0\x1b\\"),
            vec![Osc6::Channel(Chan::Green, 0)]
        );
    }

    #[test]
    fn reset_parses() {
        assert_eq!(marks(b"\x1b]6;1;bg;*;default\x07"), vec![Osc6::Reset]);
    }

    #[test]
    fn malformed_and_oversized_resync() {
        assert_eq!(marks(b"\x1b]6;1;bg;red;brightness;999\x07"), Vec::new()); // >255 rejected
        assert_eq!(marks(b"\x1b]6;2;bg;red;brightness;4\x07"), Vec::new()); // only report-1 form
    }

    #[test]
    fn split_across_reads_reports_incomplete() {
        let r = scan_split(b"x\x1b]6;1;bg;bl");
        assert!(r.marks.is_empty());
        assert_eq!(r.incomplete, Some(1));
    }

    #[test]
    fn blue_channel_parses() {
        assert_eq!(
            marks(b"\x1b]6;1;bg;blue;brightness;128\x07"),
            vec![Osc6::Channel(Chan::Blue, 128)]
        );
    }

    #[test]
    fn other_sequences_and_noise_ignored() {
        assert_eq!(
            marks(b"noise\x1b]6;1;bg;red;brightness;abc\x07"),
            Vec::new()
        );
        assert_eq!(marks(b"plain text, no escape at all"), Vec::new());
    }
}
