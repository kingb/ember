//! Plain-text URL detection (design doc: docs/design/2026-07-05-clickable-urls-design.md).
//!
//! A hand-rolled scanner, not a regex: the hard parts of URL detection —
//! trailing-punctuation trimming and balanced-bracket counting — are exactly
//! what regex can't express, and this code runs over untrusted program output
//! on every grid change, so linear-time-by-construction matters. Character
//! set from RFC 3986. Only `http://` and `https://` are recognized; the open
//! site re-checks the scheme (defense in depth).

use std::ops::Range;

/// A URL found in a line of text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UrlMatch {
    /// Byte range into the input (for slicing the URL text).
    pub bytes: Range<usize>,
    /// Char range into the input (for mapping to grid columns).
    pub chars: Range<usize>,
}

/// RFC 3986 unreserved + reserved + `%`. Terminators by omission: whitespace,
/// `<` `>` `"` `` ` `` `{` `}` `|` `\` `^` and all non-ASCII.
fn is_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '-' | '.'
                | '_'
                | '~'
                | ':'
                | '/'
                | '?'
                | '#'
                | '['
                | ']'
                | '@'
                | '!'
                | '$'
                | '&'
                | '\''
                | '('
                | ')'
                | '*'
                | '+'
                | ','
                | ';'
                | '='
                | '%'
        )
}

/// Sentence punctuation trimmed from a match tail (quotes included: `'` is a
/// legal URL char, but a trailing one is far more likely prose).
fn is_trim_char(c: char) -> bool {
    matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '"')
}

/// Find every http/https URL in `line`, left to right, non-overlapping.
pub fn find_urls(line: &str) -> Vec<UrlMatch> {
    // (char_index, byte_index, char) triple per char, for range bookkeeping.
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0; // char index

    let matches_ascii_ci = |start: usize, pat: &str| -> bool {
        pat.chars().enumerate().all(|(k, p)| {
            chars
                .get(start + k)
                .is_some_and(|&(_, c)| c.eq_ignore_ascii_case(&p))
        })
    };

    while i < n {
        // 1. Scheme.
        let scheme_len = if matches_ascii_ci(i, "https://") {
            8
        } else if matches_ascii_ci(i, "http://") {
            7
        } else {
            i += 1;
            continue;
        };
        let start = i;
        let mut j = i + scheme_len;

        // 2. Host must begin with an alphanumeric or an IPv6 bracket.
        match chars.get(j) {
            Some(&(_, '[')) => {
                // IPv6 literal: consume `[`, then hex/colon/dot, then `]`.
                let mut k = j + 1;
                while k < n && matches!(chars[k].1, '0'..='9' | 'a'..='f' | 'A'..='F' | ':' | '.') {
                    k += 1;
                }
                if k == j + 1 || chars.get(k).map(|&(_, c)| c) != Some(']') {
                    i += scheme_len; // malformed literal: resume after scheme
                    continue;
                }
                j = k + 1;
            }
            Some(&(_, c)) if c.is_ascii_alphanumeric() => {}
            _ => {
                i += scheme_len;
                continue;
            }
        }

        // 3. Extend over the URL charset.
        while j < n && is_url_char(chars[j].1) {
            j += 1;
        }

        // 4. Trim the tail: sentence punctuation, and closing brackets that
        //    close nothing (depth rule). Never trim into the scheme.
        let body_start = start + scheme_len;
        loop {
            if j <= body_start {
                break;
            }
            let last = chars[j - 1].1;
            if is_trim_char(last) {
                j -= 1;
                continue;
            }
            if last == ')' || last == ']' {
                let (open, close) = if last == ')' { ('(', ')') } else { ('[', ']') };
                let mut depth = 0i32;
                for &(_, c) in &chars[body_start..j] {
                    if c == open {
                        depth += 1;
                    } else if c == close {
                        depth -= 1;
                    }
                }
                if depth < 0 {
                    j -= 1;
                    continue;
                }
            }
            break;
        }

        // 5. Still need at least one host char.
        if j <= body_start {
            i = body_start;
            continue;
        }

        let byte_start = chars[start].0;
        let byte_end = chars.get(j).map_or(line.len(), |&(b, _)| b);
        out.push(UrlMatch {
            bytes: byte_start..byte_end,
            chars: start..j,
        });
        i = j;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The matched URL strings, in order.
    fn urls(line: &str) -> Vec<&str> {
        find_urls(line)
            .into_iter()
            .map(|m| &line[m.bytes])
            .collect()
    }

    // --- basics ---------------------------------------------------------
    #[test]
    fn plain_url_mid_line() {
        assert_eq!(
            urls("hello https://example.com world"),
            ["https://example.com"]
        );
    }
    #[test]
    fn http_scheme_too() {
        assert_eq!(urls("see http://example.com now"), ["http://example.com"]);
    }
    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(urls("HTTPS://EXAMPLE.COM"), ["HTTPS://EXAMPLE.COM"]);
        assert_eq!(urls("HtTp://example.com"), ["HtTp://example.com"]);
    }
    #[test]
    fn whole_line_is_the_url() {
        assert_eq!(
            urls("https://example.com/path"),
            ["https://example.com/path"]
        );
    }
    #[test]
    fn url_at_column_zero_and_at_eol() {
        assert_eq!(urls("https://a.io x"), ["https://a.io"]);
        assert_eq!(urls("x https://a.io"), ["https://a.io"]);
    }
    #[test]
    fn two_urls_one_line_in_order() {
        assert_eq!(
            urls("a https://one.example b http://two.example c"),
            ["https://one.example", "http://two.example"]
        );
    }
    #[test]
    fn scheme_without_host_is_not_a_match() {
        assert_eq!(urls("https:// and http://"), Vec::<&str>::new());
        assert_eq!(urls("https://."), Vec::<&str>::new());
    }
    #[test]
    fn broken_scheme_is_not_a_match() {
        assert_eq!(urls("http:/broken http:x https:"), Vec::<&str>::new());
    }
    #[test]
    fn preceding_text_does_not_block_the_match() {
        // Matches from `http`, whatever came before it.
        assert_eq!(urls("dot.http://example.com"), ["http://example.com"]);
        assert_eq!(urls("xhttps://example.com"), ["https://example.com"]);
    }

    // --- trailing punctuation --------------------------------------------
    #[test]
    fn trailing_sentence_punctuation_is_trimmed() {
        for p in [".", ",", ";", ":", "!", "?"] {
            let line = format!("go to https://example.com{p} now");
            assert_eq!(urls(&line), ["https://example.com"], "sep {p:?}");
        }
    }
    #[test]
    fn trailing_run_of_dots_all_trim() {
        assert_eq!(urls("https://example.com..."), ["https://example.com"]);
    }
    #[test]
    fn dots_inside_the_path_survive() {
        assert_eq!(
            urls("https://example.com/v1.2/docs.html more"),
            ["https://example.com/v1.2/docs.html"]
        );
    }

    // --- quotes and wrappers ----------------------------------------------
    #[test]
    fn double_and_single_quotes_wrap() {
        assert_eq!(urls("\"https://example.com\" x"), ["https://example.com"]);
        assert_eq!(urls("'https://example.com' x"), ["https://example.com"]);
    }
    #[test]
    fn bracket_wrappers_yield_the_inner_url() {
        assert_eq!(urls("(https://example.com)"), ["https://example.com"]);
        assert_eq!(urls("[https://example.com]"), ["https://example.com"]);
        assert_eq!(urls("<https://example.com>"), ["https://example.com"]);
    }
    #[test]
    fn markdown_link_yields_the_inner_url() {
        assert_eq!(
            urls("[mode 2027](https://github.com/contour/spec) for unicode"),
            ["https://github.com/contour/spec"]
        );
    }

    // --- bracket depth rule ------------------------------------------------
    #[test]
    fn wikipedia_parens_are_kept_whole() {
        assert_eq!(
            urls("see https://en.wikipedia.org/wiki/Rust_(video_game) now"),
            ["https://en.wikipedia.org/wiki/Rust_(video_game)"]
        );
    }
    #[test]
    fn balanced_parens_mid_url_survive() {
        assert_eq!(
            urls("https://example.com/foo(bar)baz more"),
            ["https://example.com/foo(bar)baz"]
        );
    }
    #[test]
    fn unbalanced_trailing_paren_trims_to_balance() {
        assert_eq!(
            urls("https://example.com/foo(bar))"),
            ["https://example.com/foo(bar)"]
        );
        assert_eq!(urls("https://example.com)"), ["https://example.com"]);
    }
    #[test]
    fn balanced_square_brackets_in_path_survive() {
        assert_eq!(
            urls("x https://example.com/[foo] y"),
            ["https://example.com/[foo]"]
        );
    }
    #[test]
    fn punctuation_after_balanced_paren_trims() {
        assert_eq!(
            urls("(https://example.com/a_(b)) done."),
            ["https://example.com/a_(b)"]
        );
    }

    // --- query / fragment / userinfo / port / encoding ----------------------
    #[test]
    fn query_and_fragment_survive() {
        assert_eq!(
            urls("q https://example.com/~user/?query=1&other=2#hash z"),
            ["https://example.com/~user/?query=1&other=2#hash"]
        );
    }
    #[test]
    fn userinfo_port_and_percent_encoding_survive() {
        assert_eq!(
            urls("https://user:pass@example.com:8443/a%20b+c=d end"),
            ["https://user:pass@example.com:8443/a%20b+c=d"]
        );
    }

    // --- IPv6 literals -------------------------------------------------------
    #[test]
    fn ipv6_dev_server_case() {
        assert_eq!(
            urls("Serving HTTP on :: port 8000 (http://[::]:8000/)"),
            ["http://[::]:8000/"]
        );
    }
    #[test]
    fn ipv6_with_port_path_query() {
        assert_eq!(
            urls("at https://[2001:db8::1]:8080/api?p=1 ok"),
            ["https://[2001:db8::1]:8080/api?p=1"]
        );
    }
    #[test]
    fn bare_ipv6_without_scheme_is_not_a_match() {
        assert_eq!(urls("listening on [::1]:8000"), Vec::<&str>::new());
    }
    #[test]
    fn unterminated_ipv6_bracket_is_not_a_match() {
        assert_eq!(urls("https://[::1 nope"), Vec::<&str>::new());
    }

    // --- char-range correctness (wide chars in the line) ---------------------
    #[test]
    fn char_range_is_correct_after_multibyte_chars() {
        // 你好 = 2 chars, 6 bytes. The URL starts at char 3 / byte 7.
        let line = "你好 https://a.io";
        let m = &find_urls(line)[0];
        assert_eq!(&line[m.bytes.clone()], "https://a.io");
        assert_eq!(m.chars, 3..15);
    }
}
