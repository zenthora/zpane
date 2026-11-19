//! Shell-dialect-aware tokenization, pipeline-prefix splitting, literal quoting, and
//! initial-buffer sanitization.

use crate::discovery::ShellDialect;
use crate::sanitize::{has_disallowed_control, is_disallowed_control, strip_disallowed_controls};
use std::borrow::Cow;

#[derive(Default)]
struct QuoteState {
    in_single: bool,
    in_double: bool,
    escaped: bool,
}

enum QuoteStep {
    EscapedChar,
    EscapePrefix,
    QuoteToggle,
    Plain,
}

impl QuoteState {
    fn in_quotes(&self) -> bool {
        self.in_single || self.in_double
    }

    fn step(&mut self, ch: char, dialect: Option<ShellDialect>) -> QuoteStep {
        if self.escaped {
            self.escaped = false;
            return QuoteStep::EscapedChar;
        }
        let ps_mode = dialect.is_some_and(ShellDialect::is_powershell);
        let fish_mode = dialect == Some(ShellDialect::Fish);
        let nu_mode = dialect == Some(ShellDialect::Nushell);
        match ch {
            '`' if ps_mode && !self.in_single => {
                self.escaped = true;
                QuoteStep::EscapePrefix
            }
            '\\' if !ps_mode
                && !self.in_single
                && (self.in_double || (dialect.is_some() && !nu_mode)) =>
            {
                self.escaped = true;
                QuoteStep::EscapePrefix
            }
            '\\' if fish_mode && self.in_single => {
                self.escaped = true;
                QuoteStep::EscapePrefix
            }
            '"' if !self.in_single => {
                self.in_double = !self.in_double;
                QuoteStep::QuoteToggle
            }
            '\'' if !self.in_double => {
                self.in_single = !self.in_single;
                QuoteStep::QuoteToggle
            }
            _ => QuoteStep::Plain,
        }
    }
}

/// Splits a seeded shell command buffer at the last top-level unquoted pipeline/chain
/// separator (`||`, `&&`, `|`, `;`), returning `(pipeline_prefix, active_segment)`.
pub fn split_pipeline_prefix(input: &str) -> (Option<String>, &str) {
    split_pipeline_prefix_impl(input, None)
}

/// Splits a seeded shell command buffer at the last top-level unquoted pipeline/chain
/// separator using `dialect`-specific quoting and escaping rules.
pub fn split_pipeline_prefix_for_dialect(
    input: &str,
    dialect: ShellDialect,
) -> (Option<String>, &str) {
    split_pipeline_prefix_impl(input, Some(dialect))
}

fn split_pipeline_prefix_impl(
    input: &str,
    dialect: Option<ShellDialect>,
) -> (Option<String>, &str) {
    let bytes = input.as_bytes();
    let no_pipe_amp = matches!(
        dialect,
        Some(ShellDialect::PowerShell | ShellDialect::Nushell)
    );
    let mut state = QuoteState::default();
    let mut paren_depth = 0usize;
    let mut brace_depth = 0usize;
    let mut last_sep_end: Option<usize> = None;
    let mut i = 0;

    while let Some(ch) = input.get(i..).and_then(|rest| rest.chars().next()) {
        let ch_len = ch.len_utf8();
        if matches!(state.step(ch, dialect), QuoteStep::Plain) && !state.in_quotes() {
            match ch {
                '(' => paren_depth += 1,
                ')' => paren_depth = paren_depth.saturating_sub(1),
                '{' => brace_depth += 1,
                '}' => brace_depth = brace_depth.saturating_sub(1),
                '|' if paren_depth == 0 && brace_depth == 0 => {
                    if bytes.get(i + 1) == Some(&b'|')
                        || (!no_pipe_amp && bytes.get(i + 1) == Some(&b'&'))
                    {
                        last_sep_end = Some(i + 2);
                        i += 2;
                        continue;
                    } else {
                        last_sep_end = Some(i + 1);
                    }
                }
                '&' if paren_depth == 0 && brace_depth == 0 && bytes.get(i + 1) == Some(&b'&') => {
                    last_sep_end = Some(i + 2);
                    i += 2;
                    continue;
                }
                ';' if paren_depth == 0 && brace_depth == 0 => {
                    last_sep_end = Some(i + 1);
                }
                _ => {}
            }
        }
        i += ch_len;
    }

    if let Some(sep_end) = last_sep_end {
        let prefix = input[..sep_end].trim();
        let tail = input[sep_end..].trim();
        if prefix.is_empty() {
            (None, tail)
        } else {
            (Some(prefix.to_string()), tail)
        }
    } else {
        (None, input.trim())
    }
}

/// Tokenizes a single shell command segment on unquoted whitespace while keeping quoted
/// multi-word arguments intact as single tokens.
pub fn tokenize_shell_words(segment: &str) -> Vec<String> {
    tokenize_shell_words_impl(segment, None)
}

/// Tokenizes a single shell command segment using `dialect`-specific quoting and escaping rules.
pub fn tokenize_shell_words_for_dialect(segment: &str, dialect: ShellDialect) -> Vec<String> {
    tokenize_shell_words_impl(segment, Some(dialect))
}

fn tokenize_shell_words_impl(segment: &str, dialect: Option<ShellDialect>) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut state = QuoteState::default();

    for ch in segment.chars() {
        match state.step(ch, dialect) {
            QuoteStep::Plain if ch.is_whitespace() && !state.in_quotes() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Strips disallowed control and BiDi characters from a seeded command buffer while folding
/// unquoted `\n` and `\t` separators into spaces.
pub fn sanitize_initial_buffer(raw: &str, dialect: ShellDialect) -> Cow<'_, str> {
    if !has_disallowed_control(raw) {
        return Cow::Borrowed(raw);
    }
    let mut state = QuoteState::default();
    let mut out = String::with_capacity(raw.len());

    for ch in raw.chars() {
        match state.step(ch, Some(dialect)) {
            QuoteStep::EscapedChar => {
                if !is_disallowed_control(ch) {
                    out.push(ch);
                }
            }
            QuoteStep::EscapePrefix | QuoteStep::QuoteToggle => out.push(ch),
            QuoteStep::Plain => {
                if matches!(ch, '\n' | '\t') && !state.in_quotes() {
                    out.push(' ');
                } else if !is_disallowed_control(ch) {
                    out.push(ch);
                }
            }
        }
    }
    Cow::Owned(out)
}

#[inline]
pub(crate) fn is_safe_unquoted_char(ch: char, allow_ps_backslash: bool) -> bool {
    if ch.is_ascii() {
        matches!(
            ch,
            'a'..='z' | 'A'..='Z' | '0'..='9' | '_' | '-' | '.' | '/' | '+' | ':' | '='
        ) || (allow_ps_backslash && ch == '\\')
    } else {
        !ch.is_whitespace()
            && !is_disallowed_control(ch)
            && !matches!(
                ch,
                '\u{00AB}'
                    | '\u{00BB}'
                    | '\u{2013}'..='\u{2015}'
                    | '\u{2018}'..='\u{201F}'
                    | '\u{2039}'
                    | '\u{203A}'
            )
    }
}

#[inline]
pub(crate) fn is_safe_unquoted_token(s: &str, dialect: Option<ShellDialect>) -> bool {
    if s.is_empty() || s.starts_with('=') {
        return false;
    }
    let allow_ps_backslash = dialect == Some(ShellDialect::PowerShell);
    s.chars()
        .all(|ch| is_safe_unquoted_char(ch, allow_ps_backslash))
}

#[inline]
pub(crate) fn is_benign_double_quotable(s: &str, dialect: Option<ShellDialect>) -> bool {
    let allow_ps_backslash = dialect == Some(ShellDialect::PowerShell);
    s.chars().all(|ch| {
        ch == '"'
            || (ch.is_whitespace() && !is_disallowed_control(ch))
            || is_safe_unquoted_char(ch, allow_ps_backslash)
    })
}

/// Quotes `raw` in single quotes using `dialect`'s single-quote escaping rules.
pub fn quote_single_for_dialect(raw: &str, dialect: Option<ShellDialect>) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    out.push('\'');
    match dialect {
        Some(ShellDialect::PowerShell) => {
            for ch in raw.chars() {
                if ch == '\'' {
                    out.push_str("''");
                } else {
                    out.push(ch);
                }
            }
        }
        Some(ShellDialect::Fish) => {
            for ch in raw.chars() {
                match ch {
                    '\\' => out.push_str("\\\\"),
                    '\'' => out.push_str("\\'"),
                    _ => out.push(ch),
                }
            }
        }
        Some(ShellDialect::Nushell) => {
            if !raw.contains('\'') {
                out.push_str(raw);
            } else {
                // Nushell single quotes are raw and cannot embed `'`; double quotes do not
                // interpolate `$` or backticks (only `$"..."` interpolates), so escaping `\` and `"`
                // yields a safe literal string across all Nushell versions.
                out.clear();
                out.push('"');
                for ch in raw.chars() {
                    match ch {
                        '\\' => out.push_str("\\\\"),
                        '"' => out.push_str("\\\""),
                        _ => out.push(ch),
                    }
                }
                out.push('"');
                return out;
            }
        }
        Some(ShellDialect::Posix) | None => {
            for ch in raw.chars() {
                if ch == '\'' {
                    out.push_str("'\\''");
                } else {
                    out.push(ch);
                }
            }
        }
    }
    out.push('\'');
    out
}

/// Formats `raw` as a shell-safe literal for `dialect`, leaving safe tokens unquoted,
/// optionally double-quoting benign space-containing strings, and otherwise single-quoting.
pub fn quote_literal_for_dialect(
    raw: &str,
    dialect: Option<ShellDialect>,
    allow_double_for_spaces_only: bool,
) -> String {
    if is_safe_unquoted_token(raw, dialect) {
        return raw.to_string();
    }
    if allow_double_for_spaces_only
        && !raw.contains('"')
        && !raw.contains('\\')
        && raw.chars().all(|ch| {
            (ch.is_whitespace() && !is_disallowed_control(ch)) || is_safe_unquoted_char(ch, false)
        })
    {
        return format!("\"{raw}\"");
    }
    quote_single_for_dialect(raw, dialect)
}

pub(crate) fn escape_benign_double_quoted(raw: &str, dialect: ShellDialect) -> String {
    let mut out = String::with_capacity(raw.len() + 4);
    out.push('"');
    for ch in raw.chars() {
        if ch == '"' {
            if dialect.is_powershell() {
                out.push_str("`\"");
            } else {
                out.push_str("\\\"");
            }
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

/// Formats a user-typed custom Pane 3 value into a shell-safe Token for `dialect`.
pub fn format_custom_value_token(raw: &str, dialect: ShellDialect) -> String {
    let cleaned = strip_disallowed_controls(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    if trimmed.starts_with('"') {
        let inner = trimmed
            .strip_prefix('"')
            .map(|rest| rest.strip_suffix('"').unwrap_or(rest))
            .unwrap_or("");
        if !inner.is_empty()
            && !inner.contains('"')
            && !inner.contains('\\')
            && inner
                .chars()
                .all(|ch| ch.is_whitespace() || is_safe_unquoted_char(ch, false))
        {
            return format!("\"{inner}\"");
        }
    } else if trimmed.starts_with('\'') {
        let inner = trimmed
            .strip_prefix('\'')
            .map(|rest| rest.strip_suffix('\'').unwrap_or(rest))
            .unwrap_or("");
        if !inner.is_empty()
            && !inner.contains('\'')
            && !inner.contains('\\')
            && inner
                .chars()
                .all(|ch| ch.is_whitespace() || is_safe_unquoted_char(ch, false))
        {
            return format!("'{inner}'");
        }
    }

    if is_safe_unquoted_token(trimmed, Some(dialect)) {
        return trimmed.to_string();
    }

    if is_benign_double_quotable(trimmed, Some(dialect))
        && !(dialect.is_powershell() && trimmed.contains('\\'))
    {
        return escape_benign_double_quoted(trimmed, dialect);
    }

    quote_single_for_dialect(trimmed, Some(dialect))
}

pub(crate) fn sanitize_literal_value_text(raw: &str, dialect: ShellDialect) -> Option<String> {
    if has_disallowed_control(raw) {
        return None;
    }
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        let inner = &raw[1..raw.len() - 1];
        if !inner.contains('"')
            && is_benign_double_quotable(inner, Some(dialect))
            && (dialect.is_powershell() || !inner.contains('\\'))
        {
            return Some(raw.to_string());
        }
        return Some(quote_single_for_dialect(inner, Some(dialect)));
    }
    Some(quote_literal_for_dialect(raw, Some(dialect), true))
}

pub(crate) fn is_valid_single_quoted(token: &str, dialect: ShellDialect) -> bool {
    if token.len() < 2
        || !token.starts_with('\'')
        || !token.ends_with('\'')
        || has_disallowed_control(token)
    {
        return false;
    }
    let inner = &token[1..token.len() - 1];
    match dialect {
        ShellDialect::PowerShell => {
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                if ch == '\'' && chars.next() != Some('\'') {
                    return false;
                }
            }
            true
        }
        ShellDialect::Posix => {
            let mut rest = inner;
            while let Some(pos) = rest.find('\'') {
                if !rest[pos..].starts_with("'\\''") {
                    return false;
                }
                rest = &rest[pos + 4..];
            }
            true
        }
        ShellDialect::Fish => {
            let mut chars = inner.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\'' => return false,
                    '\\' if !matches!(chars.next(), Some('\\' | '\'')) => return false,
                    _ => {}
                }
            }
            true
        }
        ShellDialect::Nushell => !inner.contains('\''),
    }
}

/// Safely splits a prompt buffer at a 0-based character cursor offset, clamping out-of-bounds
/// offsets to the end of `input` without ever panicking on UTF-8 char boundaries.
/// When a non-empty trailing suffix exists, any trailing whitespace immediately preceding the
/// cursor is shifted onto the suffix so re-joining an edited prefix with the verbatim suffix
/// preserves the separator spacing.
pub fn split_buffer_at_cursor(input: &str, cursor_chars: usize) -> (&str, &str) {
    let byte_idx = input
        .char_indices()
        .nth(cursor_chars)
        .map_or(input.len(), |(idx, _)| idx);
    if byte_idx == input.len() {
        return (input, "");
    }
    let prefix = &input[..byte_idx];
    let trimmed_prefix_len = prefix.trim_end().len();
    (&input[..trimmed_prefix_len], &input[trimmed_prefix_len..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_pipeline_prefix_and_tokenize_shell_words() {
        assert_eq!(
            split_pipeline_prefix(r#"Get-Process | Where-Object -Property "CPU""#),
            (
                Some("Get-Process |".to_string()),
                r#"Where-Object -Property "CPU""#
            )
        );
        assert_eq!(
            tokenize_shell_words(r#"git commit -m "fix: bug" --date='2026-01-01'"#),
            vec!["git", "commit", "-m", "\"fix: bug\"", "--date='2026-01-01'"]
        );
        assert_eq!(
            split_pipeline_prefix_for_dialect(
                r#"Get-ChildItem "C:\temp\" | Select-Object Name"#,
                ShellDialect::PowerShell
            ),
            (
                Some(r#"Get-ChildItem "C:\temp\" |"#.to_string()),
                "Select-Object Name"
            )
        );
        assert_eq!(
            split_pipeline_prefix(r#"Get-Process | ForEach-Object { $_.Name; $_.Id }"#),
            (
                Some("Get-Process |".to_string()),
                r#"ForEach-Object { $_.Name; $_.Id }"#
            )
        );
        assert_eq!(
            split_pipeline_prefix(r#"(git status; git diff) | head"#),
            (Some("(git status; git diff) |".to_string()), "head")
        );
    }

    #[test]
    fn test_pentest_split_pipeline_prefix_adversarial_and_unicode_boundaries() {
        assert_eq!(
            split_pipeline_prefix("echo 🦀|🚀; grep ☃"),
            (Some("echo 🦀|🚀;".to_string()), "grep ☃")
        );
        assert_eq!(
            split_pipeline_prefix("echo café&&ls --all"),
            (Some("echo café&&".to_string()), "ls --all")
        );
        assert_eq!(
            split_pipeline_prefix("((a | b) && {c; d}) | tail"),
            (Some("((a | b) && {c; d}) |".to_string()), "tail")
        );
        assert_eq!(
            split_pipeline_prefix("echo 'a|b' \"c;d\""),
            (None, "echo 'a|b' \"c;d\"")
        );
        assert_eq!(split_pipeline_prefix("echo \\"), (None, "echo \\"));
        assert_eq!(
            split_pipeline_prefix("echo 'unclosed"),
            (None, "echo 'unclosed")
        );
        assert_eq!(
            split_pipeline_prefix("echo \"unclosed"),
            (None, "echo \"unclosed")
        );
        assert_eq!(
            split_pipeline_prefix("| cmd"),
            (Some("|".to_string()), "cmd")
        );
        assert_eq!(split_pipeline_prefix("a |"), (Some("a |".to_string()), ""));
        assert_eq!(split_pipeline_prefix(";;;"), (Some(";;;".to_string()), ""));

        let ps_pipe_amp =
            split_pipeline_prefix_for_dialect("echo ok |& grep err", ShellDialect::PowerShell);
        assert_eq!(ps_pipe_amp, (Some("echo ok |".to_string()), "& grep err"));

        let posix_pipe_amp =
            split_pipeline_prefix_for_dialect("echo ok |& grep err", ShellDialect::Posix);
        assert_eq!(posix_pipe_amp, (Some("echo ok |&".to_string()), "grep err"));
    }

    #[test]
    fn test_sanitize_initial_buffer_and_dialect_quoting() {
        assert_eq!(
            sanitize_initial_buffer("echo\nok\t'keep\n'\u{202e}\0", ShellDialect::Posix),
            "echo ok 'keep'"
        );
        assert_eq!(
            tokenize_shell_words_for_dialect(r#"echo 'a\'b' "c d""#, ShellDialect::Fish),
            vec!["echo", r"'a\'b'", "\"c d\""]
        );
        assert_eq!(
            format_custom_value_token("hello world", ShellDialect::Posix),
            "\"hello world\""
        );
        assert_eq!(
            format_custom_value_token("$(id)", ShellDialect::Posix),
            "'$(id)'"
        );
        assert_eq!(
            format_custom_value_token("a'b", ShellDialect::PowerShell),
            "'a''b'"
        );
        assert_eq!(
            format_custom_value_token("a'b", ShellDialect::Posix),
            r"'a'\''b'"
        );
        assert_eq!(
            format_custom_value_token("a'b", ShellDialect::Fish),
            r"'a\'b'"
        );
        assert_eq!(
            format_custom_value_token("a'b", ShellDialect::Nushell),
            "\"a'b\""
        );
        assert_eq!(
            format_custom_value_token("$(id)", ShellDialect::Nushell),
            "'$(id)'"
        );
        assert!(is_valid_single_quoted("'a''b'", ShellDialect::PowerShell));
        assert!(!is_valid_single_quoted("'a'b'", ShellDialect::PowerShell));
        assert!(is_valid_single_quoted(r"'a'\''b'", ShellDialect::Posix));
        assert!(is_valid_single_quoted(r"'a\'b'", ShellDialect::Fish));
        assert!(is_valid_single_quoted(r"'C:\temp'", ShellDialect::Nushell));
        assert!(!is_valid_single_quoted("'a'b'", ShellDialect::Nushell));
    }

    #[test]
    fn test_split_buffer_at_cursor_handles_ascii_spacing_unicode_and_bounds() {
        assert_eq!(
            split_buffer_at_cursor("git status && cargo test", 10),
            ("git status", " && cargo test")
        );
        assert_eq!(
            split_buffer_at_cursor("git status && cargo test", 11),
            ("git status", " && cargo test")
        );
        assert_eq!(
            split_buffer_at_cursor("git status|rg foo", 10),
            ("git status", "|rg foo")
        );
        assert_eq!(
            split_buffer_at_cursor("git status && cargo test", 0),
            ("", "git status && cargo test")
        );
        assert_eq!(
            split_buffer_at_cursor("git status && cargo test", 999),
            ("git status && cargo test", "")
        );
        assert_eq!(split_buffer_at_cursor("", 0), ("", ""));
        assert_eq!(split_buffer_at_cursor("", 42), ("", ""));
        assert_eq!(
            split_buffer_at_cursor("echo 🦀 && ls", 6),
            ("echo 🦀", " && ls")
        );
        assert_eq!(
            split_buffer_at_cursor("echo café && ls", 9),
            ("echo café", " && ls")
        );
        assert_eq!(
            split_buffer_at_cursor("git status ", 11),
            ("git status ", "")
        );
    }
}
