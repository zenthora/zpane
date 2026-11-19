//! Single audited source of truth for stripping terminal escape sequences, C0/C1 control
//! characters, Unicode BiDi / line-separator controls, and for ASCII case-insensitive
//! substring matching.
//!
//! Every surface that renders, stores, or emits untrusted text (the Command Catalog,
//! Introspection help parsing, `Entry` normalisation, frame rendering, fuzzy filtering, and
//! CLI error output) goes through these helpers so the blocked character set is defined
//! exactly once.

use std::borrow::Cow;

/// Unicode BiDi format controls that can visually reorder or spoof terminal text.
#[inline]
pub const fn is_bidi_control(c: char) -> bool {
    matches!(
        c,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

/// Zero-width space / non-joiner / joiner characters that hide content from reviewers.
#[inline]
pub const fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200d}')
}

/// C0/DEL/C1 controls, BiDi format controls, and Unicode line/paragraph separators.
#[inline]
pub fn is_disallowed_control(c: char) -> bool {
    c.is_control() || is_bidi_control(c) || matches!(c, '\u{2028}' | '\u{2029}')
}

/// [`is_disallowed_control`] plus zero-width characters: everything that must never be
/// displayed verbatim in help text, cache entries, or catalog descriptions.
#[inline]
pub fn is_unsafe_display_char(c: char) -> bool {
    is_disallowed_control(c) || is_zero_width(c)
}

/// Returns `true` when `s` contains at least one [`is_disallowed_control`] character.
#[inline]
pub fn has_disallowed_control(s: &str) -> bool {
    s.bytes()
        .any(|b| b < 0x20 || b == 0x7f || matches!(b, 0xc2 | 0xd8 | 0xe2))
        && (s.is_ascii() || s.chars().any(is_disallowed_control))
}

/// Removes every [`is_disallowed_control`] character, borrowing when `s` is already clean.
pub fn strip_disallowed_controls(s: &str) -> Cow<'_, str> {
    if !has_disallowed_control(s) {
        Cow::Borrowed(s)
    } else {
        Cow::Owned(s.chars().filter(|&ch| !is_disallowed_control(ch)).collect())
    }
}

/// Strips C0/DEL/C1 control characters and Unicode BiDi / line-separator controls so tokens
/// and single-line descriptions cannot inject newlines, NULs, terminal escapes, or spoofed
/// text.
pub fn sanitize_single_line(raw: &str) -> Cow<'_, str> {
    strip_disallowed_controls(raw)
}

/// Sanitizes multi-line descriptions by stripping every [`is_disallowed_control`] character
/// except `\n`, which is preserved as the line separator.
pub fn sanitize_multiline_desc(raw: &str) -> Cow<'_, str> {
    if !raw.chars().any(|c| c != '\n' && is_disallowed_control(c)) {
        return Cow::Borrowed(raw);
    }
    Cow::Owned(
        raw.chars()
            .filter(|&c| c == '\n' || !is_disallowed_control(c))
            .collect(),
    )
}

/// Zero-allocation ASCII case-insensitive substring search. An empty needle always matches.
#[inline]
pub fn contains_ignore_ascii_case(haystack: &str, needle: impl AsRef<[u8]>) -> bool {
    let needle = needle.as_ref();
    let n_len = needle.len();
    if n_len == 0 {
        return true;
    }
    let h_bytes = haystack.as_bytes();
    if h_bytes.len() < n_len {
        return false;
    }
    let first_lower = needle[0].to_ascii_lowercase();
    let first_upper = first_lower.to_ascii_uppercase();
    if n_len == 1 {
        return h_bytes
            .iter()
            .any(|&b| b == first_lower || b == first_upper);
    }
    let max_start = h_bytes.len() - n_len;
    (0..=max_start).any(|i| {
        let hb = h_bytes[i];
        (hb == first_lower || hb == first_upper)
            && h_bytes[i + 1..i + n_len].eq_ignore_ascii_case(&needle[1..])
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EscKind {
    Csi,
    Osc,
    ControlString,
}

#[inline]
pub fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Returns `(byte_len, char_len)` when `rest` begins with a complete ANSI/ECMA-48
/// multi-character control sequence (CSI, OSC, DCS, SOS, PM, or APC in 7-bit or C1 form).
pub fn next_escape_seq_len(rest: &str) -> Option<(usize, usize)> {
    let bytes = rest.as_bytes();
    let (prefix_bytes, prefix_chars, kind) = if bytes.starts_with(b"\x1b[") {
        (2usize, 2usize, EscKind::Csi)
    } else if bytes.starts_with(b"\x1b]") {
        (2, 2, EscKind::Osc)
    } else if bytes.len() >= 2 && bytes[0] == 0x1b && matches!(bytes[1], b'P' | b'X' | b'^' | b'_')
    {
        (2, 2, EscKind::ControlString)
    } else if bytes.len() >= 2 && bytes[0] == 0xc2 {
        match bytes[1] {
            0x9b => (2, 1, EscKind::Csi),
            0x9d => (2, 1, EscKind::Osc),
            0x90 | 0x98 | 0x9e | 0x9f => (2, 1, EscKind::ControlString),
            _ => return None,
        }
    } else {
        return None;
    };

    if kind == EscKind::Csi {
        let mut i = prefix_bytes;
        while i < bytes.len() && (0x20..=0x3f).contains(&bytes[i]) {
            i += 1;
        }
        if i < bytes.len() && (0x40..=0x7e).contains(&bytes[i]) {
            let body_len = i + 1 - prefix_bytes;
            return Some((i + 1, prefix_chars + body_len));
        }
        return None;
    }

    const MAX_STR_SCAN: usize = 4096;
    let mut char_count = 0usize;
    let tail = &rest[prefix_bytes..];
    for (byte_off, ch) in tail.char_indices() {
        if byte_off >= MAX_STR_SCAN {
            return None;
        }
        char_count += 1;
        if kind == EscKind::Osc && ch == '\x07' {
            return Some((prefix_bytes + byte_off + 1, prefix_chars + char_count));
        }
        if ch == '\u{009c}' {
            return Some((
                prefix_bytes + byte_off + ch.len_utf8(),
                prefix_chars + char_count,
            ));
        }
        if ch == '\x1b' {
            if tail.as_bytes().get(byte_off + 1) == Some(&b'\\') {
                return Some((prefix_bytes + byte_off + 2, prefix_chars + char_count + 1));
            }
            return None;
        }
        if matches!(ch, '\r' | '\n' | '\x00') {
            return None;
        }
    }
    None
}

/// Maps one character to its display-safe form: whitespace-like separators become a space,
/// disallowed controls are dropped (`None`), everything else passes through.
#[inline]
pub fn sanitize_display_char(c: char) -> Option<char> {
    if matches!(c, '\r' | '\n' | '\t' | '\u{0085}' | '\u{2028}' | '\u{2029}') {
        Some(' ')
    } else if is_disallowed_control(c) {
        None
    } else {
        Some(c)
    }
}

/// Replaces `dst` with `src` stripped of every escape sequence and disallowed control
/// character, collapsing newlines and tabs into single spaces (single-line display text).
pub fn strip_escapes_and_controls_into(src: &str, dst: &mut String) {
    dst.clear();
    if is_printable_ascii(src) {
        dst.push_str(src);
        return;
    }
    dst.reserve(src.len());
    let mut byte_idx = 0usize;
    while byte_idx < src.len() {
        let Some(ch) = src[byte_idx..].chars().next() else {
            break;
        };
        if ch == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&ch) {
            if let Some((esc_bytes, _)) = next_escape_seq_len(&src[byte_idx..]) {
                byte_idx += esc_bytes;
                continue;
            }
        }
        byte_idx += ch.len_utf8();
        if let Some(clean) = sanitize_display_char(ch) {
            dst.push(clean);
        }
    }
}

/// Strips ANSI escape sequences, bare ESC/C0/DEL/C1 controls, zero-width and BiDi characters
/// from captured multi-line CLI help text while preserving `\n`, `\t`, and `\r\n` line
/// endings. Borrows when `text` is already clean.
pub fn strip_help_text_escapes(text: &str) -> Cow<'_, str> {
    #[inline]
    fn is_unsafe_help_char(c: char) -> bool {
        is_unsafe_display_char(c) && c != '\n' && c != '\t'
    }

    let bytes = text.as_bytes();
    let mut needs_clean = false;
    let mut non_ascii_start = None;
    for (idx, &b) in bytes.iter().enumerate() {
        if b >= 0x80 {
            non_ascii_start = Some(idx);
            break;
        }
        if b == 0x1b || b == 0x7f || (b < 0x20 && b != b'\n' && b != b'\t' && b != b'\r') {
            needs_clean = true;
            break;
        }
        if b == b'\r' && bytes.get(idx + 1) != Some(&b'\n') {
            needs_clean = true;
            break;
        }
    }
    if let Some(start) = non_ascii_start {
        for (rel_idx, ch) in text[start..].char_indices() {
            let idx = start + rel_idx;
            if ch == '\x1b'
                || (ch == '\r' && bytes.get(idx + 1) != Some(&b'\n'))
                || (ch != '\r' && is_unsafe_help_char(ch))
            {
                needs_clean = true;
                break;
            }
        }
    }
    if !needs_clean {
        return Cow::Borrowed(text);
    }

    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                Some('[') => {
                    chars.next();
                    let mut count = 0usize;
                    while let Some(&next) = chars.peek() {
                        if next == '\n' || next == '\r' || count >= 64 {
                            break;
                        }
                        chars.next();
                        count += 1;
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    chars.next();
                    let mut count = 0usize;
                    while let Some(&next) = chars.peek() {
                        if next == '\n' || next == '\r' || count >= 256 {
                            break;
                        }
                        let Some(c) = chars.next() else {
                            break;
                        };
                        count += 1;
                        if c == '\x07' {
                            break;
                        }
                        if c == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                Some(next) if next != '\n' && next != '\r' => {
                    chars.next();
                }
                _ => {}
            }
        } else if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                out.push('\r');
            }
        } else if !is_unsafe_help_char(ch) {
            out.push(ch);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_display(src: &str) -> String {
        let mut out = String::new();
        strip_escapes_and_controls_into(src, &mut out);
        out
    }

    #[test]
    fn disallowed_control_covers_c0_del_c1_bidi_and_line_separators() {
        for c in [
            '\0', '\x07', '\x1b', '\x7f', '\u{0085}', '\u{009b}', '\u{061c}', '\u{200e}',
            '\u{200f}', '\u{202a}', '\u{202e}', '\u{2066}', '\u{2069}', '\u{2028}', '\u{2029}',
        ] {
            assert!(is_disallowed_control(c), "{c:?} should be disallowed");
        }
        for c in [
            'a', ' ', 'é', '日', '😀', '\u{200b}', '\u{00a0}', '\u{2065}',
        ] {
            assert!(!is_disallowed_control(c), "{c:?} should be allowed");
        }
    }

    #[test]
    fn unsafe_display_char_additionally_rejects_zero_width_characters() {
        assert!(is_unsafe_display_char('\u{200b}'));
        assert!(is_unsafe_display_char('\u{200c}'));
        assert!(is_unsafe_display_char('\u{200d}'));
        assert!(!is_unsafe_display_char('\u{200a}'));
        assert!(!is_unsafe_display_char('x'));
    }

    #[test]
    fn has_disallowed_control_detects_ascii_and_unicode_payloads() {
        assert!(!has_disallowed_control("git commit -m 'héllo 日本'"));
        assert!(has_disallowed_control("a\nb"));
        assert!(has_disallowed_control("a\u{0000}b"));
        assert!(has_disallowed_control("a\u{009b}b"));
        assert!(has_disallowed_control("a\u{202e}b"));
        assert!(has_disallowed_control("a\u{061c}b"));
        assert!(has_disallowed_control("a\u{2029}b"));
    }

    #[test]
    fn strip_disallowed_controls_borrows_clean_text_and_removes_hostile_chars() {
        assert!(matches!(
            strip_disallowed_controls("clean text"),
            Cow::Borrowed("clean text")
        ));
        assert_eq!(
            strip_disallowed_controls("ev\u{202e}il\u{0}\r\n\u{2028}cmd\u{9b}"),
            "evilcmd"
        );
    }

    #[test]
    fn sanitize_single_line_drops_newlines_and_bidi() {
        assert_eq!(sanitize_single_line("a\nb\u{202e}c\u{7f}d"), "abcd");
        assert!(matches!(sanitize_single_line("plain"), Cow::Borrowed(_)));
    }

    #[test]
    fn sanitize_multiline_desc_keeps_newlines_but_drops_other_controls() {
        assert_eq!(
            sanitize_multiline_desc("line1\r\nline2\u{202e}\u{0}\u{2028}end"),
            "line1\nline2end"
        );
        assert!(matches!(
            sanitize_multiline_desc("a\nb"),
            Cow::Borrowed("a\nb")
        ));
    }

    #[test]
    fn contains_ignore_ascii_case_matches_worked_examples() {
        assert!(contains_ignore_ascii_case("Get-ChildItem", "childitem"));
        assert!(contains_ignore_ascii_case("Get-ChildItem", "CHILD"));
        assert!(contains_ignore_ascii_case(
            "Get-ChildItem",
            b"item".as_slice()
        ));
        assert!(contains_ignore_ascii_case("anything", ""));
        assert!(contains_ignore_ascii_case("x", "X"));
        assert!(!contains_ignore_ascii_case("abc", "abcd"));
        assert!(!contains_ignore_ascii_case("Get-ChildItem", "childitems"));
        assert!(!contains_ignore_ascii_case("straße", "STRASSE"));
        assert!(!contains_ignore_ascii_case("", "a"));
    }

    #[test]
    fn strip_escapes_removes_csi_osc_and_c1_sequences() {
        assert_eq!(strip_display("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_display("a\x1b]0;title\x07b"), "ab");
        assert_eq!(
            strip_display("a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b"),
            "alinkb"
        );
        assert_eq!(strip_display("a\u{9b}31mb\u{9d}0;t\u{9c}c"), "abc");
        assert_eq!(strip_display("a\x1bPdcs\x1b\\b"), "ab");
    }

    #[test]
    fn strip_escapes_collapses_whitespace_controls_to_spaces_and_drops_bidi() {
        assert_eq!(strip_display("a\nb\tc\r\nd"), "a b c  d");
        assert_eq!(strip_display("a\u{2028}b\u{85}c"), "a b c");
        assert_eq!(strip_display("a\u{202e}b\u{2066}c\x7fd\0e"), "abcde");
    }

    #[test]
    fn strip_escapes_leaves_unterminated_osc_payload_without_the_esc_byte() {
        assert_eq!(strip_display("x\x1b]0;never-ends"), "x]0;never-ends");
    }

    #[test]
    fn strip_help_text_preserves_newlines_tabs_and_crlf() {
        assert_eq!(
            strip_help_text_escapes("Usage:\n\tzpane [OPTIONS]\r\n"),
            "Usage:\n\tzpane [OPTIONS]\r\n"
        );
        assert!(matches!(
            strip_help_text_escapes("plain\nhelp"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn strip_help_text_removes_escapes_bare_cr_zero_width_and_bidi() {
        assert_eq!(
            strip_help_text_escapes("\x1b[1mBold\x1b[0m \x1b]0;t\x07text\rover\u{200b}\u{202e}x\0"),
            "Bold textoverx"
        );
    }
}
