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

/// Scan `bytes` for complete OSC 133 sequences, in order.
pub fn scan(bytes: &[u8]) -> Vec<Osc133> {
    scan_indexed(bytes).into_iter().map(|(_, m)| m).collect()
}

/// Like [`scan`], but each mark is paired with the byte index **just past** its
/// terminator, so the caller can feed the engine incrementally and read the cursor
/// at the exact point a mark was emitted (needed to anchor a prompt to its line).
/// An incomplete sequence at the end (split across reads) is skipped.
pub fn scan_indexed(bytes: &[u8]) -> Vec<(usize, Osc133)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + PREFIX.len() <= bytes.len() {
        if &bytes[i..i + PREFIX.len()] != PREFIX {
            i += 1;
            continue;
        }
        let start = i + PREFIX.len();
        // Find the terminator: BEL (0x07) or ST (ESC \).
        let mut j = start;
        let mut term: Option<usize> = None;
        while j < bytes.len() {
            match bytes[j] {
                0x07 => {
                    term = Some(j);
                    break;
                }
                0x1b if j + 1 < bytes.len() && bytes[j + 1] == 0x5c => {
                    term = Some(j);
                    break;
                }
                0x1b => break, // a bare ESC (possibly a split ST) — stop, resync next read
                _ => j += 1,
            }
        }
        match term {
            Some(t) => {
                let past = if bytes[t] == 0x07 { t + 1 } else { t + 2 };
                if let Some(ev) = parse_params(&bytes[start..t]) {
                    out.push((past, ev));
                }
                i = past;
            }
            None => break, // incomplete sequence at end of buffer
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
        let s = b"user@host \x1b]133;A\x07$ \x1b]133;B\x07ls\r\n\x1b]133;C\x07file\r\n\x1b]133;D;0\x07";
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
    fn ignores_unknown_and_other_osc() {
        assert_eq!(scan(b"\x1b]0;title\x07"), vec![]); // OSC 0, not 133
        assert_eq!(scan(b"\x1b]133;Z\x07"), vec![]); // unknown letter
    }
}
