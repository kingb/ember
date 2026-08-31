//! VS Code shell-integration OSC 633;E command-line scanner.
//!
//! Mirrors [`crate::osc133`]'s scan-and-resync approach (split-across-reads
//! carry, malformed-mid-buffer resync) rather than sharing code with it, for
//! the same reason `osc1337` does: the scanners are short, independently
//! tested, and diverging here can't regress an already-hardened path.
//!
//! Sequence shape: `ESC ] 633 ; <letter>[;params…] (BEL | ESC \)`. VS Code
//! emits several subcommands (`A`/`B`/`C`/`D`/`P`/…) for prompt/command
//! lifecycle and property reporting; only `E;<commandline>` — the shell
//! echoing the command it's about to run — is tracked here. The command line
//! is VS Code-escaped (`\\` for a literal backslash, `\xHH` for byte `HH`) so
//! that embedded semicolons, newlines, and control bytes survive the OSC
//! parameter boundary; [`decode`] reverses that escaping.

/// A parsed OSC 633 sequence (the tracked subset).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Osc633 {
    /// `E;<commandline>` — the command the shell is about to run, decoded.
    CommandLine(String),
}

const PREFIX: &[u8] = b"\x1b]633;";

/// Longest sequence we'll treat as "possibly split across reads". Command
/// lines run longer than OSC 133 marks; the 1 KiB storage cap on a captured
/// command line is applied later, at capture — this bound only distinguishes
/// a plausible split from binary noise that happens to contain the prefix.
const MAX_SEQ: usize = 4096;

/// One scan pass: the complete marks found, plus where a **possibly split**
/// sequence starts at the end of the buffer, so the caller can carry those
/// bytes into the next read.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Each mark, paired with the byte index just past its terminator.
    pub marks: Vec<(usize, Osc633)>,
    /// Start of an incomplete suffix to carry into the next read, if any.
    pub incomplete: Option<usize>,
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

/// Parse the bytes between the `633;` prefix and the terminator. Only the
/// `E;<commandline>` subcommand produces a mark; every other letter (`A`,
/// `B`, `C`, `D`, `P`, …) is part of the protocol but not tracked here.
fn parse_body(body: &[u8]) -> Option<Osc633> {
    if body.first() != Some(&b'E') || body.get(1) != Some(&b';') {
        return None;
    }
    Some(Osc633::CommandLine(decode(&body[2..])))
}

/// Reverse VS Code's command-line escaping: `\\` decodes to a literal `\`,
/// `\xHH` decodes to the single byte `HH`, and anything else following a
/// backslash passes through literally (including the backslash itself, so a
/// stray trailing `\` at the end of the payload is preserved rather than
/// dropped).
fn decode(escaped: &[u8]) -> String {
    let mut out = Vec::with_capacity(escaped.len());
    let mut i = 0;
    while i < escaped.len() {
        if escaped[i] == b'\\' && i + 1 < escaped.len() {
            match escaped[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                }
                b'x' if i + 3 < escaped.len() => {
                    match u8::from_str_radix(
                        std::str::from_utf8(&escaped[i + 2..i + 4]).unwrap_or(""),
                        16,
                    ) {
                        Ok(byte) => {
                            out.push(byte);
                            i += 4;
                        }
                        Err(_) => {
                            out.push(escaped[i]);
                            i += 1;
                        }
                    }
                }
                _ => {
                    out.push(escaped[i]);
                    i += 1;
                }
            }
        } else {
            out.push(escaped[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmds(bytes: &[u8]) -> Vec<String> {
        scan_split(bytes)
            .marks
            .into_iter()
            .map(|(_, Osc633::CommandLine(s))| s)
            .collect()
    }

    #[test]
    fn plain_command_line_bel_terminated() {
        assert_eq!(
            cmds(b"\x1b]633;E;gt crew at skippy\x07"),
            vec!["gt crew at skippy"]
        );
    }

    #[test]
    fn st_terminated_and_embedded_in_output() {
        assert_eq!(cmds(b"noise\x1b]633;E;ls -la\x1b\\more"), vec!["ls -la"]);
    }

    #[test]
    fn vscode_escapes_decode() {
        // `;` is \x3b, backslash is \\, newline is \x0a
        assert_eq!(
            cmds(b"\x1b]633;E;echo a\\x3b b\\\\c\\x0ad\x07"),
            vec!["echo a; b\\c\nd"]
        );
    }

    #[test]
    fn other_633_subcommands_ignored() {
        // A/B/C/D/P exist in the VS Code protocol; only E matters here.
        assert_eq!(
            cmds(b"\x1b]633;A\x07\x1b]633;P;Cwd=/x\x07"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn split_across_reads_reports_incomplete() {
        let r = scan_split(b"out\x1b]633;E;gt cr");
        assert!(r.marks.is_empty());
        assert_eq!(r.incomplete, Some(3));
    }

    #[test]
    fn oversized_sequence_resyncs() {
        let mut b = b"\x1b]633;E;".to_vec();
        b.extend(std::iter::repeat(b'a').take(MAX_SEQ + 1));
        b.extend_from_slice(b"\x1b]633;E;ok\x07");
        assert_eq!(cmds(&b), vec!["ok"]);
    }
}
