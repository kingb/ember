//! iTerm2 OSC 1337 scanner — the subset named in design §8.1: `CurrentDir`
//! (cwd-inheriting new splits), `RemoteHost`, and `SetMark`.
//!
//! Mirrors [`crate::osc133`]'s scan-and-resync approach (split-across-reads
//! carry, malformed-mid-buffer resync) rather than sharing code with it —
//! the two scanners are short, independently tested, and diverging here
//! can't regress the already-hardened OSC 133 path. A generic "scan one
//! `ESC ] <code> ;` family" helper would be a reasonable follow-up refactor
//! if a third OSC code needs this same treatment.
//!
//! Sequence shape: `ESC ] 1337 ; <key>[=<value>] (BEL | ESC \)`. Real iTerm2
//! integration scripts emit many other keys (`File=`, `ShellIntegrationVersion=`,
//! …) — [`parse_params`] returns `None` for anything but the three tracked
//! here, so those bytes are simply not turned into an event (never a
//! regression: unrecognized keys already fall through, same as an unknown
//! OSC 133 letter).

/// A parsed OSC 1337 sequence (the tracked subset).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Osc1337 {
    /// `CurrentDir=<path>` — the shell's cwd as of this prompt.
    CurrentDir(String),
    /// `RemoteHost=<user>@<host>` — set when the shell is on a remote box (ssh).
    RemoteHost(String),
    /// `SetMark` — a user-placed navigable mark, independent of command
    /// boundaries (iTerm2's Cmd+Shift+M).
    SetMark,
}

const PREFIX: &[u8] = b"\x1b]1337;";

/// Longest sequence we'll treat as "possibly split across reads" — same
/// reasoning as `osc133::MAX_SEQ`: a `CurrentDir` path is realistically a few
/// hundred bytes at most; anything longer unterminated is garbage, not a split.
const MAX_SEQ: usize = 4096;

/// One scan pass: the complete sequences found, plus where a **possibly
/// split** sequence starts at the end of the buffer, so the caller can carry
/// those bytes into the next read.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Each mark, paired with the byte index just past its terminator — so a
    /// caller merging this with another OSC scan (e.g. `osc133`) can sort
    /// the combined stream back into buffer order (needed for `SetMark`,
    /// which anchors to the engine's cursor position at that exact point).
    pub marks: Vec<(usize, Osc1337)>,
    /// Start of an incomplete suffix to carry into the next read, if any.
    pub incomplete: Option<usize>,
}

/// Scan `bytes` for complete OSC 1337 sequences (the tracked subset), in order.
pub fn scan(bytes: &[u8]) -> Vec<Osc1337> {
    scan_split(bytes)
        .marks
        .into_iter()
        .map(|(_, m)| m)
        .collect()
}

/// Full scan with split-detection — see the module doc for the shape this
/// mirrors ([`crate::osc133::scan_split`]).
pub fn scan_split(bytes: &[u8]) -> ScanResult {
    let mut out = ScanResult::default();
    let len = bytes.len();
    let mut i = 0usize;
    while i < len {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        let n = PREFIX.len().min(len - i);
        if bytes[i..i + n] != PREFIX[..n] {
            i += 1;
            continue;
        }
        if n < PREFIX.len() {
            out.incomplete = Some(i);
            break;
        }
        let start = i + PREFIX.len();
        let mut j = start;
        let mut term: Option<usize> = None;
        let mut malformed = false;
        while j < len {
            if j - start > MAX_SEQ {
                malformed = true;
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
                i = j.max(i + 1);
            }
            (None, false) => {
                out.incomplete = Some(i);
                break;
            }
        }
    }
    out
}

fn parse_params(p: &[u8]) -> Option<Osc1337> {
    let s = std::str::from_utf8(p).ok()?;
    match s.split_once('=') {
        Some(("CurrentDir", path)) => Some(Osc1337::CurrentDir(path.to_string())),
        Some(("RemoteHost", host)) => Some(Osc1337::RemoteHost(host.to_string())),
        _ if s == "SetMark" => Some(Osc1337::SetMark),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_dir_bel_and_st_terminated() {
        assert_eq!(
            scan(b"\x1b]1337;CurrentDir=/home/user/projects\x07"),
            vec![Osc1337::CurrentDir("/home/user/projects".to_string())]
        );
        assert_eq!(
            scan(b"\x1b]1337;CurrentDir=/tmp\x1b\\"),
            vec![Osc1337::CurrentDir("/tmp".to_string())]
        );
    }

    #[test]
    fn remote_host() {
        assert_eq!(
            scan(b"\x1b]1337;RemoteHost=user@host\x07"),
            vec![Osc1337::RemoteHost("user@host".to_string())]
        );
    }

    #[test]
    fn set_mark_has_no_value() {
        assert_eq!(scan(b"\x1b]1337;SetMark\x07"), vec![Osc1337::SetMark]);
    }

    #[test]
    fn unrecognized_keys_are_ignored_not_a_regression() {
        assert_eq!(
            scan(b"\x1b]1337;ShellIntegrationVersion=15;shell=bash\x07"),
            vec![]
        );
        assert_eq!(
            scan(b"\x1b]1337;File=name=x.png;size=10:aGVsbG8=\x07"),
            vec![]
        );
    }

    #[test]
    fn embedded_in_surrounding_output() {
        let s = b"$ \x1b]1337;CurrentDir=/tmp\x07\x1b]1337;SetMark\x07ls\r\n";
        assert_eq!(
            scan(s),
            vec![Osc1337::CurrentDir("/tmp".to_string()), Osc1337::SetMark,]
        );
    }

    #[test]
    fn incomplete_sequence_skipped() {
        assert_eq!(scan(b"prompt \x1b]1337;CurrentDir=/tm"), vec![]);
        assert_eq!(scan(b"no marks here"), vec![]);
    }

    #[test]
    fn malformed_mid_buffer_does_not_suppress_later_marks() {
        assert_eq!(
            scan(b"\x1b]1337;\x1bZ noise \x1b]1337;SetMark\x07"),
            vec![Osc1337::SetMark]
        );
    }

    #[test]
    fn oversized_unterminated_is_garbage_not_a_split() {
        let mut buf = b"\x1b]1337;CurrentDir=".to_vec();
        buf.extend(std::iter::repeat_n(b'x', 5000));
        buf.extend_from_slice(b"\x1b]1337;SetMark\x07");
        let r = scan_split(&buf);
        assert_eq!(
            r.marks.into_iter().map(|(_, m)| m).collect::<Vec<_>>(),
            vec![Osc1337::SetMark]
        );
        assert_eq!(r.incomplete, None);
    }

    #[test]
    fn split_points_are_reported_as_incomplete() {
        assert_eq!(scan_split(b"out\x1b]133").incomplete, Some(3));
        assert_eq!(scan_split(b"x\x1b]1337;CurrentDir=/tm").incomplete, Some(1));
        assert_eq!(scan_split(b"\x1b]1337;SetMark\x1b").incomplete, Some(0));
    }

    #[test]
    fn ignores_other_osc() {
        assert_eq!(scan(b"\x1b]0;title\x07"), vec![]); // OSC 0
        assert_eq!(scan(b"\x1b]133;A\x07"), vec![]); // OSC 133, not 1337
    }
}
