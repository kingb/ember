//! OSC 133 (FinalTerm / iTerm2 shell-integration) mark scanner.
//!
//! alacritty_terminal doesn't parse OSC 133, so the projection pre-scans the raw
//! PTY byte stream for these (invisible) sequences and turns them into semantic
//! marks. The bytes are still fed to the engine unchanged — OSC 133 is a no-op to
//! the grid, so nothing is lost by leaving it in.
//!
//! Sequence shape: `ESC ] 133 ; <letter>[;params…] (BEL | ESC \)`
//!   A = prompt start · B = command start (prompt end) · C = output start
//!   D[;<exit>] = command end, with an optional exit code.

/// A parsed OSC 133 mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Osc133 {
    /// `A` — a fresh prompt begins here (the gutter marks this line).
    PromptStart,
    /// `B` — the prompt ended / the typed command begins.
    CommandStart,
    /// `C` — the command's output begins.
    OutputStart,
    /// `D[;code]` — the command finished, with an optional exit code.
    CommandEnd(Option<i32>),
}

const PREFIX: &[u8] = b"\x1b]133;";

/// Longest sequence we'll treat as "possibly split across reads". Real OSC 133
/// marks are tens of bytes; anything longer unterminated is garbage (binary
/// output that happened to contain the prefix), not a split.
const MAX_SEQ: usize = 256;

/// One scan pass: the complete marks found, plus where a **possibly split**
/// sequence starts at the end of the buffer (a prefix fragment or an
/// unterminated-but-plausible sequence) so the caller can carry those bytes
/// into the next read.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Each mark, paired with the byte index just past its terminator.
    pub marks: Vec<(usize, Osc133)>,
    /// Start of an incomplete suffix to carry into the next read, if any.
    pub incomplete: Option<usize>,
}

/// Scan `bytes` for complete OSC 133 sequences, in order.
pub fn scan(bytes: &[u8]) -> Vec<Osc133> {
    scan_indexed(bytes).into_iter().map(|(_, m)| m).collect()
}

/// Like [`scan`], but each mark is paired with the byte index **just past** its
/// terminator, so the caller can feed the engine incrementally and read the cursor
/// at the exact point a mark was emitted (needed to anchor a prompt to its line).
pub fn scan_indexed(bytes: &[u8]) -> Vec<(usize, Osc133)> {
    scan_split(bytes).marks
}

/// Full scan with split-detection. A malformed sequence mid-buffer (bare ESC,
/// oversized params) resyncs and keeps scanning — it must not suppress later
/// legitimate marks in the same read; only a *plausible* split at the buffer
/// end is reported as `incomplete`.
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
                if let Some(ev) = parse_params(&bytes[start..t]) {
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

fn parse_params(p: &[u8]) -> Option<Osc133> {
    let s = std::str::from_utf8(p).ok()?;
    let mut parts = s.split(';');
    match parts.next()? {
        "A" => Some(Osc133::PromptStart),
        "B" => Some(Osc133::CommandStart),
        "C" => Some(Osc133::OutputStart),
        "D" => Some(Osc133::CommandEnd(
            parts.next().and_then(|c| c.parse::<i32>().ok()),
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bel_terminated() {
        assert_eq!(scan(b"\x1b]133;A\x07"), vec![Osc133::PromptStart]);
        assert_eq!(scan(b"\x1b]133;B\x07"), vec![Osc133::CommandStart]);
        assert_eq!(scan(b"\x1b]133;C\x07"), vec![Osc133::OutputStart]);
    }

    #[test]
    fn st_terminated() {
        assert_eq!(scan(b"\x1b]133;A\x1b\\"), vec![Osc133::PromptStart]);
    }

    #[test]
    fn command_end_exit_codes() {
        assert_eq!(scan(b"\x1b]133;D;0\x07"), vec![Osc133::CommandEnd(Some(0))]);
        assert_eq!(scan(b"\x1b]133;D;1\x07"), vec![Osc133::CommandEnd(Some(1))]);
        assert_eq!(scan(b"\x1b]133;D\x07"), vec![Osc133::CommandEnd(None)]);
        // Extra params after the exit code (e.g. aid=…) are ignored.
        assert_eq!(
            scan(b"\x1b]133;D;130;aid=7\x07"),
            vec![Osc133::CommandEnd(Some(130))]
        );
    }

    #[test]
    fn embedded_in_surrounding_output() {
        let s =
            b"user@host \x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07file\r\n\x1b]133;D;0\x07";
        assert_eq!(
            scan(s),
            vec![
                Osc133::PromptStart,
                Osc133::CommandStart,
                Osc133::OutputStart,
                Osc133::CommandEnd(Some(0)),
            ]
        );
    }

    #[test]
    fn extra_prompt_params_ok() {
        // Some shells emit `133;A;cl=m` etc.
        assert_eq!(scan(b"\x1b]133;A;cl=m\x07"), vec![Osc133::PromptStart]);
    }

    #[test]
    fn incomplete_sequence_skipped() {
        assert_eq!(scan(b"prompt \x1b]133;A"), vec![]); // no terminator yet
        assert_eq!(scan(b"no marks here"), vec![]);
    }

    #[test]
    fn malformed_mid_buffer_does_not_suppress_later_marks() {
        // A bare ESC right after the prefix (6 bytes of binary noise) used to
        // abort scanning the entire rest of the read.
        assert_eq!(
            scan(b"\x1b]133;\x1bZ noise \x1b]133;A\x07"),
            vec![Osc133::PromptStart]
        );
        // The offending ESC may itself start a valid sequence.
        assert_eq!(scan(b"\x1b]133;\x1b]133;B\x07"), vec![Osc133::CommandStart]);
    }

    #[test]
    fn oversized_unterminated_is_garbage_not_a_split() {
        let mut buf = b"\x1b]133;".to_vec();
        buf.extend(std::iter::repeat_n(b'x', 400));
        buf.extend_from_slice(b"\x1b]133;A\x07");
        let r = scan_split(&buf);
        assert_eq!(r.marks.len(), 1);
        assert_eq!(r.incomplete, None);
    }

    #[test]
    fn split_points_are_reported_as_incomplete() {
        // Mid-prefix.
        assert_eq!(scan_split(b"out\x1b]13").incomplete, Some(3));
        // Mid-params.
        assert_eq!(scan_split(b"x\x1b]133;D;1").incomplete, Some(1));
        // Trailing lone ESC of a split ST.
        assert_eq!(scan_split(b"\x1b]133;A\x1b").incomplete, Some(0));
    }

    #[test]
    fn ignores_unknown_and_other_osc() {
        assert_eq!(scan(b"\x1b]0;title\x07"), vec![]); // OSC 0, not 133
        assert_eq!(scan(b"\x1b]133;Z\x07"), vec![]); // unknown letter
    }
}
