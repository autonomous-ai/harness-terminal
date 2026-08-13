//! URL detection for the terminal grid.
//!
//! Given a row of cell text and a column, find the URL span (start, end) covering that column, if
//! any. Used by the renderer (to underline/tint the URL cells) and by the native window's
//! Cmd/Ctrl+click handler (to open the URL). Detection runs on a single row at a time and only on
//! rows about to be drawn, so it stays cheap.

/// A detected URL span within one row. `start` and `end` are **byte** indices into the row's
/// `&str` (matching `str::find` semantics and the grid's column indexing for ASCII); `end` is
/// exclusive. Every URL character the terminal sheet cares about is ASCII, so byte == column for
/// the span itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UrlSpan {
    /// Byte offset of the URL's first character.
    pub start: usize,
    /// Byte offset one past the URL's last character.
    pub end: usize,
}

impl UrlSpan {
    /// The URL text for the span.
    pub fn as_str<'a>(&self, line: &'a str) -> &'a str {
        &line[self.start..self.end]
    }
}

/// True if `s` looks like a bare host (e.g. `foo.com`, `foo.com/path`): a `.` with an ASCII letter
/// on both sides. Rejects `2+2`, `a.b`-sized noise and empty strings.
fn hostish(s: &str) -> bool {
    let bytes = s.as_bytes();
    let Some(dot) = s.find('.') else {
        return false;
    };
    if dot == 0 || dot + 1 >= bytes.len() {
        return false;
    }
    bytes[..dot].iter().any(|b| b.is_ascii_alphabetic())
        && bytes[dot + 1..].iter().any(|b| b.is_ascii_alphabetic())
}

/// Does `cell` have the byte value we treat as trailing URL-closing punctuation?
fn is_closer(b: u8) -> bool {
    matches!(
        b,
        b'.' | b',' | b')' | b']' | b'"' | b'\'' | b';' | b'!' | b'?' | b':'
    )
}

/// Grow a URL region forward from `start` across URI-safe characters. Deliberately does NOT consume
/// closing punctuation (`.`, `,`, `)`, `"`, `'`, `;`), so `https://x.com/foo).` spans as
/// `https://x.com/foo`.
fn url_at(line: &str, start: usize) -> UrlSpan {
    let bytes = line.as_bytes();
    let n = bytes.len();
    let mut end = start;
    while end < n
        && (bytes[end].is_ascii_alphanumeric()
            || matches!(
                bytes[end],
                b'-' | b'_'
                    | b'.'
                    | b'/'
                    | b'~'
                    | b'?'
                    | b'#'
                    | b'&'
                    | b'%'
                    | b'='
                    | b'+'
                    | b'*'
                    | b'@'
                    | b':'
            ))
    {
        end += 1;
    }
    // A trailing close-paren/tick is already excluded (not in the allowed set). A single trailing
    // period, though it IS a URL char, is almost always sentence punctuation (`.../foo.`), so trim
    // it to keep the opened URL clean; a path-internal dot (`foo.bar/baz`) or a relative `..` is
    // untouched.
    if end > start && bytes[end - 1] == b'.' && !(end >= 2 && bytes[end - 2] == b'.') {
        end -= 1;
    }
    UrlSpan { start, end }
}

/// Return the URL span covering byte column `col` in `line`, or `None`.
///
/// Recognises `scheme://…` (http, https, file, ftp), single-colon `mailto:` / `tel:` links, and a
/// bare host like `foo.com/path`. The span excludes trailing closing punctuation (`.`, `,`, `)`,
/// …), so `…/foo.` reads as `…/foo`.
pub fn url_span(line: &str, col: usize) -> Option<UrlSpan> {
    let bytes = line.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let n = bytes.len();
    let col = col.min(n);

    // 1) Scheme URLs (`scheme://…`). Scan the whole line for `://` and check the scheme name.
    let mut search = 0usize;
    while let Some(p) = line[search..].find("://") {
        let colon = search + p;
        // Scheme name = the run of alphanumerics immediately before the colon.
        let mut s = colon;
        while s > 0 && bytes[s - 1].is_ascii_alphanumeric() {
            s -= 1;
        }
        let scheme = &line[s..colon];
        if matches!(scheme, "http" | "https" | "file" | "ftp") {
            let sp = url_at(line, s);
            if sp.start <= col && col < sp.end {
                return Some(sp);
            }
            // This URL didn't cover the column; skip past it and keep searching.
            let after = sp.end.max(colon + 3);
            if after >= n {
                break;
            }
            search = after;
        } else {
            search = colon + 3;
        }
    }

    // 1b) Single-colon schemes (`mailto:` and `tel:`) have no `//`, so the loop above misses them.
    // Scan for each marker and require a real value after the colon and a word boundary before the
    // scheme (`amailto:x` must not mis-detect). Runs before the bare-host branch so `user@host` in
    // `mailto:user@host` resolves as a mail link, not a web host.
    for marker in ["mailto:", "tel:"] {
        let mut search = 0usize;
        while let Some(p) = line[search..].find(marker) {
            let start = search + p;
            // Scheme must be word-boundary: line start or whitespace immediately before it.
            let preceded_ok = start == 0 || bytes[start - 1].is_ascii_whitespace();
            if preceded_ok {
                let sp = url_at(line, start);
                // Require at least one character of value ("mailto:" alone is not a link).
                if sp.end > start + marker.len() && sp.start <= col && col < sp.end {
                    return Some(sp);
                }
            }
            search = start + marker.len();
        }
    }

    // 2) Bare host: expand the whitespace/bracket-delimited token containing the column and test it.
    let mut start = col;
    let wb = |b: u8| {
        b.is_ascii_whitespace()
            || matches!(
                b,
                b'(' | b')' | b'"' | b'\'' | b'[' | b']' | b'<' | b'>' | b','
            )
    };
    while start > 0 && !wb(bytes[start - 1]) {
        start -= 1;
    }
    let mut end = start;
    while end < n && !wb(bytes[end]) {
        end += 1;
    }
    // Trim one run of trailing closing punctuation that commonly follows a URL.
    while end > start && is_closer(bytes[end - 1]) {
        end -= 1;
    }
    // Trim a trailing `/`, `.`, `:` or `?` left by the token (e.g. `example.com/` → `example.com`)
    // so the clickable core is what actually resolves.
    while end > start + 1 && matches!(bytes[end - 1], b'/' | b'.' | b':' | b'?') {
        end -= 1;
    }
    let tok = &line[start..end];
    // A bare-host URL must not itself contain a scheme (that was handled above and can read more
    // than the host); also require at least one `/` or `.` so plain words like `version` skip.
    if tok.contains("://") || (!tok.contains('.') && !tok.contains('/')) {
        return None;
    }
    if hostish(tok) {
        return Some(UrlSpan { start, end });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(line: &str, col: usize) -> Option<(usize, usize)> {
        url_span(line, col).map(|s| (s.start, s.end))
    }

    #[test]
    fn url_mid_line() {
        let line = "see https://example.com/foo in the log";
        // "https://example.com/foo" = bytes 4..27.
        assert_eq!(span(line, 4), Some((4, 27)));
        assert_eq!(span(line, 10), Some((4, 27)));
        assert_eq!(span(line, 20), Some((4, 27)));
        assert_eq!(span(line, 26), Some((4, 27)));
    }

    #[test]
    fn url_starts_at_column_zero() {
        let line = "https://example.com";
        assert_eq!(span(line, 0), Some((0, 19)));
        assert_eq!(span(line, 10), Some((0, 19)));
        assert_eq!(span(line, 18), Some((0, 19)));
    }

    #[test]
    fn url_ends_at_last_column() {
        let line = "go to https://x.com/foo";
        // "https://x.com/foo" = bytes 6..23; the URL ends at the last column (col 22).
        assert_eq!(span(line, 22), Some((6, 23)));
        assert_eq!(span(line, 15), Some((6, 23)));
    }

    #[test]
    fn no_url() {
        assert_eq!(span("just some ordinary text here", 5), None);
        assert_eq!(span("", 0), None);
        assert_eq!(span("   ", 1), None);
        assert_eq!(span("a", 0), None);
        assert_eq!(span("calc 2+2 is four", 6), None);
        assert_eq!(span("no host here", 4), None);
        // A plain word with a slash but no dot/host is not a URL.
        assert_eq!(span("grep -r foo/bar src", 10), None);
    }

    #[test]
    fn url_adjacent_to_trailing_punctuation() {
        // Trailing `.` trimmed: `…/foo.` clicks as `…/foo`.
        assert_eq!(span("www.x.com/foo. then", 5), Some((0, 13)));
        // Trailing `),` trimmed.
        assert_eq!(span("see www.x.com/foo), next", 8), Some((4, 17)));
        // URL inside parens excludes the parens.
        assert_eq!(span("(https://x.com/foo)", 10), Some((1, 18)));
        // file:// URL (with the extra slash) spans the whole clickable path.
        let line = "path file:///tmp/a.log here";
        assert_eq!(span(line, 10), Some((5, 22)));
    }

    #[test]
    fn bare_host_detection() {
        assert_eq!(span("visit foo.com/path now", 9), Some((6, 18)));
        assert_eq!(span("ping example.com", 8), Some((5, 16)));
        // Trailing slash trimmed.
        assert_eq!(span("try example.com/ !", 8), Some((4, 15)));
        // Inside parens.
        assert_eq!(span("see (foo.com/bar) ok", 8), Some((5, 16)));
    }

    #[test]
    fn mailto_and_tel_links() {
        // mailto: spans the full address; a col inside it clicks it. "email me at " is 12 chars.
        let line = "email me at mailto:alice@example.com thanks";
        assert_eq!(span(line, 15), Some((12, 36)));
        assert_eq!(span(line, 34), Some((12, 36)));
        let s = url_span(line, 20).unwrap();
        assert_eq!(s.as_str(line), "mailto:alice@example.com");
        // tel: link — "call tel:" is 5 chars, "tel:+1-555-0100" is 15 chars.
        let line3 = "call tel:+1-555-0100 now";
        assert_eq!(span(line3, 7), Some((5, 20)));
        assert_eq!(
            url_span(line3, 12).unwrap().as_str(line3),
            "tel:+1-555-0100"
        );
        // A bare email in prose still resolves as a host link (no mailto: needed).
        assert_eq!(span("write to alice@example.com", 10), Some((9, 26)));
    }

    #[test]
    fn mailto_requires_word_boundary_and_value() {
        // Colon alone, or nothing after it, is not a link.
        assert_eq!(span("a mailto: b", 4), None);
        // No space before the scheme: `amailto:x` is a word, not a scheme — so it reads as a host
        // only if there's a dotted TLD, and as nothing at all without one.
        assert_eq!(span("amailto:xyz", 5), None);
        // Dotted TLD after a glued scheme still resolves as a host link, not a mailto.
        let s = url_span("amailto:alice@example.com", 15).unwrap();
        assert_eq!(
            s.as_str("amailto:alice@example.com"),
            "amailto:alice@example.com"
        );
    }

    #[test]
    fn url_span_as_str_roundtrip() {
        let line = "see https://example.com/foo in the log";
        let s = url_span(line, 10).unwrap();
        assert_eq!(s.as_str(line), "https://example.com/foo");
        let line2 = "www.x.com/foo. done";
        let s2 = url_span(line2, 5).unwrap();
        assert_eq!(s2.as_str(line2), "www.x.com/foo");
        let line3 = "path file:///tmp/a.log here";
        let s3 = url_span(line3, 10).unwrap();
        assert_eq!(s3.as_str(line3), "file:///tmp/a.log");
    }

    /// A scheme URL ending in sentence punctuation opens clean: the single trailing period after a
    /// URL char is dropped, while an internal dot (`pkg.v2`) and a relative `..` are preserved.
    #[test]
    fn url_span_trims_trailing_sentence_period_only() {
        // Sentence-final period is punctuation, not part of the URL.
        let line = "read https://example.com/foo. ok";
        let s = url_span(line, 10).unwrap();
        assert_eq!(s.as_str(line), "https://example.com/foo");
        // An internal dot is a real path character.
        let line2 = "see https://example.com/pkg.v2/index here";
        let s2 = url_span(line2, 8).unwrap();
        assert_eq!(s2.as_str(line2), "https://example.com/pkg.v2/index");
        // A relative parent `..` is a real path, not punctuation — keep both dots.
        let line3 = "cd https://example.com/dir/.. now";
        let s3 = url_span(line3, 8).unwrap();
        assert_eq!(s3.as_str(line3), "https://example.com/dir/..");
    }
}
