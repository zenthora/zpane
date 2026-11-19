use crate::discovery::ShellDialect;
pub use crate::discovery::{BorderStyle, IconMode, ThemePreset, UiConfig, UiStyle};
use crate::fuzzy::PaneList;
use crate::model::{Entry, EntryKind, Pane};
use crate::sanitize::{
    is_printable_ascii, next_escape_seq_len, sanitize_display_char, strip_escapes_and_controls_into,
};
use std::borrow::Cow;
use std::fmt::Write as FmtWrite;
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy)]
struct ThemePalette {
    border_dim: &'static str,
    header_act: &'static str,
    header_dim: &'static str,
    sel_act_bg: &'static str,
    sel_inact_bg: &'static str,
    match_hl: &'static str,
    text: &'static str,
    dim: &'static str,
    cmd_bar: &'static str,
    filter: &'static str,
    rst: &'static str,
}

const DARK_PALETTE: ThemePalette = ThemePalette {
    border_dim: "\x1b[38;5;240m",
    header_act: "\x1b[38;5;75;1m",
    header_dim: "\x1b[38;5;246m",
    sel_act_bg: "\x1b[48;5;236;38;5;255;1m",
    sel_inact_bg: "\x1b[48;5;234;38;5;250m",
    match_hl: "\x1b[38;5;226;1m",
    text: "\x1b[38;5;252m",
    dim: "\x1b[38;5;244m",
    cmd_bar: "\x1b[38;5;114;1m",
    filter: "\x1b[38;5;223;1m",
    rst: "\x1b[0m",
};

const LIGHT_PALETTE: ThemePalette = ThemePalette {
    border_dim: "\x1b[38;5;245m",
    header_act: "\x1b[38;5;25;1m",
    header_dim: "\x1b[38;5;242m",
    sel_act_bg: "\x1b[48;5;253;38;5;16;1m",
    sel_inact_bg: "\x1b[48;5;254;38;5;237m",
    match_hl: "\x1b[38;5;130;1m",
    text: "\x1b[38;5;235m",
    dim: "\x1b[38;5;243m",
    cmd_bar: "\x1b[38;5;28;1m",
    filter: "\x1b[38;5;94;1m",
    rst: "\x1b[0m",
};

const ANSI16_PALETTE: ThemePalette = ThemePalette {
    border_dim: "\x1b[90m",
    header_act: "\x1b[94;1m",
    header_dim: "\x1b[37m",
    sel_act_bg: "\x1b[44;97;1m",
    sel_inact_bg: "\x1b[100;97m",
    match_hl: "\x1b[93;1m",
    text: "\x1b[0m",
    dim: "\x1b[90m",
    cmd_bar: "\x1b[32;1m",
    filter: "\x1b[33;1m",
    rst: "\x1b[0m",
};

// Zero foreground/background color codes (`3x`/`4x`/`9x`/`10x`); uses bold + reverse video only.
const NONE_PALETTE: ThemePalette = ThemePalette {
    border_dim: "",
    header_act: "\x1b[1m",
    header_dim: "",
    sel_act_bg: "\x1b[0;7;1m",
    sel_inact_bg: "\x1b[0;7m",
    match_hl: "\x1b[1m",
    text: "",
    dim: "",
    cmd_bar: "\x1b[1m",
    filter: "\x1b[1m",
    rst: "\x1b[0m",
};

fn palette_for(theme: ThemePreset) -> &'static ThemePalette {
    match theme {
        ThemePreset::Dark => &DARK_PALETTE,
        ThemePreset::Light => &LIGHT_PALETTE,
        ThemePreset::Ansi16 => &ANSI16_PALETTE,
        ThemePreset::None => &NONE_PALETTE,
    }
}

fn kind_color(theme: ThemePreset, kind: EntryKind) -> &'static str {
    match theme {
        ThemePreset::Dark => match kind {
            EntryKind::History => "\x1b[38;5;221m",
            EntryKind::Path => "\x1b[38;5;116m",
            EntryKind::Env | EntryKind::Var => "\x1b[38;5;216m",
            EntryKind::Cli => "\x1b[38;5;114m",
            EntryKind::Func | EntryKind::Alias => "\x1b[38;5;81m",
            EntryKind::Cmdlet => "\x1b[38;5;75m",
            EntryKind::Subcmd => "\x1b[38;5;141m",
            EntryKind::Flag => "\x1b[38;5;183m",
            EntryKind::Param => "\x1b[38;5;223m",
            EntryKind::Enum | EntryKind::Value => "\x1b[38;5;150m",
            EntryKind::Arg => "\x1b[38;5;250m",
            EntryKind::Loading => "\x1b[38;5;244m",
        },
        ThemePreset::Light => match kind {
            EntryKind::History => "\x1b[38;5;136m",
            EntryKind::Path => "\x1b[38;5;30m",
            EntryKind::Env | EntryKind::Var => "\x1b[38;5;166m",
            EntryKind::Cli => "\x1b[38;5;28m",
            EntryKind::Func | EntryKind::Alias => "\x1b[38;5;31m",
            EntryKind::Cmdlet => "\x1b[38;5;25m",
            EntryKind::Subcmd => "\x1b[38;5;91m",
            EntryKind::Flag => "\x1b[38;5;98m",
            EntryKind::Param => "\x1b[38;5;94m",
            EntryKind::Enum | EntryKind::Value => "\x1b[38;5;29m",
            EntryKind::Arg => "\x1b[38;5;238m",
            EntryKind::Loading => "\x1b[38;5;243m",
        },
        ThemePreset::Ansi16 => match kind {
            EntryKind::History => "\x1b[33m",
            EntryKind::Path => "\x1b[36m",
            EntryKind::Env | EntryKind::Var => "\x1b[93m",
            EntryKind::Cli => "\x1b[32m",
            EntryKind::Func | EntryKind::Alias => "\x1b[96m",
            EntryKind::Cmdlet => "\x1b[34m",
            EntryKind::Subcmd => "\x1b[35m",
            EntryKind::Flag => "\x1b[95m",
            EntryKind::Param => "\x1b[33m",
            EntryKind::Enum | EntryKind::Value => "\x1b[92m",
            EntryKind::Arg => "\x1b[37m",
            EntryKind::Loading => "\x1b[90m",
        },
        ThemePreset::None => "",
    }
}

#[derive(Debug, Clone, Copy)]
struct BorderChars {
    top_left: char,
    top_right: char,
    bottom_left: char,
    bottom_right: char,
    vertical: char,
    t_down: char,
    t_up: char,
    t_right: char,
    t_left: char,
    is_ascii: bool,
}

fn border_chars_for(style: BorderStyle) -> BorderChars {
    match style {
        BorderStyle::Rounded => BorderChars {
            top_left: '╭',
            top_right: '╮',
            bottom_left: '╰',
            bottom_right: '╯',
            vertical: '│',
            t_down: '┬',
            t_up: '┴',
            t_right: '├',
            t_left: '┤',
            is_ascii: false,
        },
        BorderStyle::Plain => BorderChars {
            top_left: '┌',
            top_right: '┐',
            bottom_left: '└',
            bottom_right: '┘',
            vertical: '│',
            t_down: '┬',
            t_up: '┴',
            t_right: '├',
            t_left: '┤',
            is_ascii: false,
        },
        BorderStyle::Ascii => BorderChars {
            top_left: '+',
            top_right: '+',
            bottom_left: '+',
            bottom_right: '+',
            vertical: '|',
            t_down: '+',
            t_up: '+',
            t_right: '+',
            t_left: '+',
            is_ascii: true,
        },
    }
}

#[inline]
fn ascii_icon_equiv(ch: char) -> Option<&'static str> {
    match ch {
        '❯' => Some(">"),
        '›' => Some(">"),
        '✓' => Some("*"),
        'ℹ' => Some("i"),
        '✏' => Some("*"),
        '🔀' => Some("<>"),
        '⎇' => Some("@"),
        '⟳' => Some("~"),
        '📁' => Some("[]"),
        '…' => Some("~"),
        '█' => Some("_"),
        '↑' => Some("^"),
        '↓' => Some("v"),
        '←' => Some("<"),
        '→' => Some(">"),
        '—' => Some("-"),
        _ => None,
    }
}

#[inline]
fn icon_pointer(icons: IconMode, is_selected: bool) -> &'static str {
    if !is_selected {
        "  "
    } else if icons == IconMode::Ascii {
        "> "
    } else {
        "❯ "
    }
}

#[inline]
fn icon_ellipsis(icons: IconMode) -> char {
    if icons == IconMode::Ascii {
        '~'
    } else {
        '…'
    }
}

#[inline]
fn icon_cursor(icons: IconMode) -> char {
    if icons == IconMode::Ascii {
        '_'
    } else {
        '█'
    }
}

#[inline]
fn icon_info(icons: IconMode) -> &'static str {
    if icons == IconMode::Ascii {
        "i"
    } else {
        "ℹ"
    }
}

#[inline]
fn icon_committed(icons: IconMode) -> char {
    if icons == IconMode::Ascii {
        '*'
    } else {
        '✓'
    }
}

#[inline]
fn push_styled_end(out: &mut String, style: &str, rst: &str) {
    if !style.is_empty() {
        out.push_str(rst);
    }
}

/// Rows consumed by Pane 3's inspector header (title, description, divider).
const PANE3_INSPECTOR_ROWS: usize = 3;

/// Rows consumed by the outer frame: top border, divider, 3 status/footer bars, bottom border.
const FRAME_CHROME_ROWS: usize = 6;

/// Terminal dimensions shared by viewport geometry, scrolling, and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: usize,
    pub height: usize,
}

impl Viewport {
    pub const MIN_WIDTH: usize = 40;
    pub const MIN_HEIGHT: usize = 12;

    pub const fn new(width: usize, height: usize) -> Self {
        Self { width, height }
    }

    const fn clamp_axis(raw: u16, min: usize, fallback: usize) -> usize {
        if (raw as usize) >= min {
            (raw as usize).saturating_sub(1)
        } else {
            fallback
        }
    }

    /// Clamps raw terminal dimensions `(w, h)` to usable frame bounds,
    /// falling back to `(fallback_w, fallback_h)` when either axis is below minimum.
    pub const fn from_terminal_size(w: u16, h: u16, fallback_w: usize, fallback_h: usize) -> Self {
        Self {
            width: Self::clamp_axis(w, Self::MIN_WIDTH, fallback_w.saturating_sub(1)),
            height: Self::clamp_axis(h, Self::MIN_HEIGHT, fallback_h.saturating_sub(1)),
        }
    }

    /// Returns a resized viewport, preserving the previous axis if the new dimension is below minimum.
    pub const fn resized(self, w: u16, h: u16) -> Self {
        Self {
            width: Self::clamp_axis(w, Self::MIN_WIDTH, self.width),
            height: Self::clamp_axis(h, Self::MIN_HEIGHT, self.height),
        }
    }

    /// Authoritative visible list-row count for `pane`, accounting for frame chrome
    /// and Pane 3's inspector header. Never exceeds the terminal height.
    pub fn rows(self, pane: Pane) -> usize {
        let list_rows = self.height.saturating_sub(FRAME_CHROME_ROWS);
        match pane {
            Pane::Third => list_rows.saturating_sub(PANE3_INSPECTOR_ROWS),
            Pane::First | Pane::Second => list_rows,
        }
    }
}

fn display_width_sanitized(s: &str) -> usize {
    if is_printable_ascii(s) {
        return s.len();
    }
    let mut w = 0usize;
    let mut byte_idx = 0usize;
    while byte_idx < s.len() {
        let Some(ch) = s[byte_idx..].chars().next() else {
            break;
        };
        if ch == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&ch) {
            if let Some((esc_bytes, _)) = next_escape_seq_len(&s[byte_idx..]) {
                byte_idx += esc_bytes;
                continue;
            }
        }
        byte_idx += ch.len_utf8();
        if let Some(clean) = sanitize_display_char(ch) {
            w += UnicodeWidthChar::width(clean).unwrap_or(0);
        }
    }
    w
}

#[inline]
fn push_repeated_space(out: &mut String, mut count: usize) {
    const SPACES: &str = "                                                                ";
    while count > 0 {
        let chunk = count.min(SPACES.len());
        out.push_str(&SPACES[..chunk]);
        count -= chunk;
    }
}

const BAR_LINE: &str = "────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────";
const ASCII_BAR_LINE: &str = "--------------------------------------------------------------------------------------------------------------------------------";
const BAR_LINE_CHARS: usize = 128;

#[inline]
fn push_repeated_bar_for(out: &mut String, mut count: usize, ascii: bool) {
    if ascii {
        while count > 0 {
            let chunk = count.min(BAR_LINE_CHARS);
            out.push_str(&ASCII_BAR_LINE[..chunk]);
            count -= chunk;
        }
    } else {
        while count > 0 {
            let chunk = count.min(BAR_LINE_CHARS);
            out.push_str(&BAR_LINE[..chunk * 3]);
            count -= chunk;
        }
    }
}

#[inline]
fn bar_cow_for(width: usize, ascii: bool) -> Cow<'static, str> {
    if width <= BAR_LINE_CHARS {
        if ascii {
            Cow::Borrowed(&ASCII_BAR_LINE[..width])
        } else {
            Cow::Borrowed(&BAR_LINE[..width * 3])
        }
    } else {
        let mut s = String::with_capacity(if ascii { width } else { width * 3 });
        push_repeated_bar_for(&mut s, width, ascii);
        Cow::Owned(s)
    }
}

#[cfg(test)]
#[inline]
fn bar_cow(width: usize) -> Cow<'static, str> {
    bar_cow_for(width, false)
}

/// Appends sanitized text truncated to at most `max_width` display columns, optionally
/// highlighting `match_positions` on top of `base_style`, and returns the written width.
fn push_sanitized_truncated_styled(
    out: &mut String,
    text: &str,
    max_width: usize,
    match_positions: &[usize],
    base_style: Option<&str>,
    palette: &ThemePalette,
    icons: IconMode,
) -> usize {
    if max_width == 0 {
        return 0;
    }
    let ellipsis = icon_ellipsis(icons);
    if is_printable_ascii(text) {
        let truncated = text.len() > max_width;
        let keep = if !truncated {
            text.len()
        } else if max_width > 1 {
            max_width - 1
        } else {
            1
        };
        let slice = &text[..keep];
        if let Some(base) = base_style.filter(|_| !match_positions.is_empty()) {
            let restore = if base.is_empty() { palette.rst } else { base };
            let mut cursor = 0usize;
            let mut in_hl = false;
            for (idx, ch_byte) in slice.bytes().enumerate() {
                while cursor < match_positions.len() && match_positions[cursor] < idx {
                    cursor += 1;
                }
                let is_match = cursor < match_positions.len() && match_positions[cursor] == idx;
                if is_match {
                    cursor += 1;
                    if !in_hl {
                        out.push_str(palette.match_hl);
                        in_hl = true;
                    }
                } else if in_hl {
                    out.push_str(restore);
                    in_hl = false;
                }
                out.push(ch_byte as char);
            }
            if in_hl {
                out.push_str(restore);
            }
        } else {
            out.push_str(slice);
        }
        if truncated && max_width > 1 {
            out.push(ellipsis);
            return max_width;
        }
        return keep;
    }

    let mut cur_w = 0usize;
    let mut last_non_zero_start = out.len();
    let mut last_non_zero_base_w = 0usize;
    let mut last_non_zero_in_hl = false;
    let mut match_cursor = 0usize;
    let mut in_hl = false;

    let mut byte_idx = 0usize;
    let mut char_idx = 0usize;
    while byte_idx < text.len() {
        let Some(raw_ch) = text[byte_idx..].chars().next() else {
            break;
        };
        if raw_ch == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&raw_ch) {
            if let Some((esc_bytes, esc_chars)) = next_escape_seq_len(&text[byte_idx..]) {
                byte_idx += esc_bytes;
                char_idx += esc_chars;
                continue;
            }
        }
        let idx = char_idx;
        byte_idx += raw_ch.len_utf8();
        char_idx += 1;

        let Some(ch) = sanitize_display_char(raw_ch) else {
            continue;
        };
        let ascii_rep = if icons == IconMode::Ascii {
            ascii_icon_equiv(ch)
        } else {
            None
        };
        let w = ascii_rep
            .map(str::len)
            .unwrap_or_else(|| UnicodeWidthChar::width(ch).unwrap_or(0));
        if w == 0 && cur_w == 0 {
            continue;
        }
        if cur_w + w > max_width {
            if max_width > 1 {
                if cur_w == max_width {
                    out.truncate(last_non_zero_start);
                    cur_w = last_non_zero_base_w;
                    in_hl = last_non_zero_in_hl;
                }
                if cur_w < max_width {
                    if in_hl {
                        if let Some(base) = base_style {
                            out.push_str(if base.is_empty() { palette.rst } else { base });
                        }
                        in_hl = false;
                    }
                    out.push(ellipsis);
                    cur_w += 1;
                }
            }
            break;
        }
        if w > 0 {
            last_non_zero_start = out.len();
            last_non_zero_base_w = cur_w;
            last_non_zero_in_hl = in_hl;
        }
        if let Some(base) = base_style {
            let restore = if base.is_empty() { palette.rst } else { base };
            while match_cursor < match_positions.len() && match_positions[match_cursor] < idx {
                match_cursor += 1;
            }
            if match_cursor < match_positions.len() && match_positions[match_cursor] == idx {
                if !in_hl {
                    out.push_str(palette.match_hl);
                    in_hl = true;
                }
                if let Some(rep) = ascii_rep {
                    out.push_str(rep);
                } else {
                    out.push(ch);
                }
                match_cursor += 1;
            } else {
                if in_hl {
                    out.push_str(restore);
                    in_hl = false;
                }
                if let Some(rep) = ascii_rep {
                    out.push_str(rep);
                } else {
                    out.push(ch);
                }
            }
        } else if let Some(rep) = ascii_rep {
            out.push_str(rep);
        } else {
            out.push(ch);
        }
        cur_w += w;
    }

    if in_hl {
        if let Some(base) = base_style {
            out.push_str(if base.is_empty() { palette.rst } else { base });
        }
    }

    cur_w
}

/// Appends plain text truncated to at most `max_width` display columns, returning the written width.
fn push_truncated_text(out: &mut String, text: &str, max_width: usize, icons: IconMode) -> usize {
    push_sanitized_truncated_styled(out, text, max_width, &[], None, &DARK_PALETTE, icons)
}

/// Appends plain text truncated or space-padded to exact terminal display columns (`max_width`).
fn push_padded_text_with(out: &mut String, text: &str, max_width: usize, icons: IconMode) {
    let cur_w = push_truncated_text(out, text, max_width, icons);
    if max_width > cur_w {
        push_repeated_space(out, max_width - cur_w);
    }
}

#[cfg(test)]
fn push_padded_text(out: &mut String, text: &str, max_width: usize) {
    push_padded_text_with(out, text, max_width, IconMode::Unicode);
}

/// Appends `prefix` + `body` padded to `max_width`, tail-scrolling `body` with a leading `'…'`/`'~'`
/// when it overflows so the active tail remains visible.
fn push_tail_truncated_padded(
    out: &mut String,
    prefix: &str,
    body: &str,
    max_width: usize,
    icons: IconMode,
    scratch: &mut String,
) {
    let prefix_w = display_width_sanitized(prefix);
    let ellipsis = icon_ellipsis(icons);
    if is_printable_ascii(body) {
        let body_w = body.len();
        if prefix_w + body_w <= max_width || max_width <= prefix_w + 1 {
            let written = push_truncated_text(out, prefix, max_width, icons);
            let rem = max_width.saturating_sub(written);
            let body_written = push_truncated_text(out, body, rem, icons);
            let total = written + body_written;
            if max_width > total {
                push_repeated_space(out, max_width - total);
            }
            return;
        }
        let target_w = max_width - prefix_w - 1;
        let suffix_start = body_w - target_w;
        let mut written = push_truncated_text(out, prefix, max_width, icons);
        if written < max_width {
            out.push(ellipsis);
            written += 1;
        }
        written += push_truncated_text(
            out,
            &body[suffix_start..],
            max_width.saturating_sub(written),
            icons,
        );
        if max_width > written {
            push_repeated_space(out, max_width - written);
        }
        return;
    }

    strip_escapes_and_controls_into(body, scratch);
    let body_w = display_width_sanitized(scratch);

    if prefix_w + body_w <= max_width || max_width <= prefix_w + 1 {
        let written = push_truncated_text(out, prefix, max_width, icons);
        let rem = max_width.saturating_sub(written);
        let body_written = push_truncated_text(out, scratch, rem, icons);
        let total = written + body_written;
        if max_width > total {
            push_repeated_space(out, max_width - total);
        }
        return;
    }

    let target_w = max_width - prefix_w - 1;
    let mut w_so_far = 0usize;
    let mut suffix_start = scratch.len();
    for (idx, ch) in scratch.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w > 0 && body_w - w_so_far <= target_w {
            suffix_start = idx;
            break;
        }
        w_so_far += w;
    }

    let mut written = push_truncated_text(out, prefix, max_width, icons);
    if written < max_width {
        out.push(ellipsis);
        written += 1;
    }
    written += push_truncated_text(
        out,
        &scratch[suffix_start..],
        max_width.saturating_sub(written),
        icons,
    );
    if max_width > written {
        push_repeated_space(out, max_width - written);
    }
}

/// Appends a pane header title bounded to `col_width`, filling remaining width with border bar chars.
fn push_pane_header(
    out: &mut String,
    color: &str,
    title: &str,
    col_width: usize,
    palette: &ThemePalette,
    icons: IconMode,
    ascii_borders: bool,
) {
    out.push_str(color);
    let written = push_truncated_text(out, title, col_width, icons);
    push_styled_end(out, color, palette.rst);
    out.push_str(palette.border_dim);
    if col_width > written {
        push_repeated_bar_for(out, col_width - written, ascii_borders);
    }
}

/// Renders a Miller cell directly into `out` strictly bounded to `col_width` display columns,
/// with fuzzy-match highlighting, selection styling, optional committed marker, and right-aligned `[Kind]` badge.
#[allow(clippy::too_many_arguments)]
fn push_miller_cell_styled(
    out: &mut String,
    entry: &Entry,
    match_positions: &[usize],
    col_width: usize,
    is_selected: bool,
    is_active_pane: bool,
    is_committed: bool,
    style: UiStyle,
) {
    if col_width == 0 {
        return;
    }
    let theme = style.theme;
    let icons = style.icons;
    let palette = palette_for(theme);
    let badge = entry.kind.badge();
    let badge_w = badge.len();
    let ptr = icon_pointer(icons, is_selected);
    let ptr_w = if col_width >= 2 { 2 } else { 0 };

    let base_style = if is_selected {
        if is_active_pane {
            palette.sel_act_bg
        } else {
            palette.sel_inact_bg
        }
    } else {
        palette.text
    };

    out.push_str(base_style);
    if ptr_w == 2 {
        out.push_str(ptr);
    }

    let rem = col_width.saturating_sub(ptr_w);
    let marker_w = if is_committed && rem >= 4 { 2 } else { 0 };
    let show_badge = rem >= badge_w + marker_w + 3;
    let badge_reserved = if show_badge { badge_w + 1 } else { 0 };
    let avail_label_w = rem - marker_w - badge_reserved;

    let cur_w = push_sanitized_truncated_styled(
        out,
        &entry.label,
        avail_label_w,
        match_positions,
        Some(base_style),
        palette,
        icons,
    );

    if avail_label_w > cur_w {
        push_repeated_space(out, avail_label_w - cur_w);
    }

    if marker_w == 2 {
        out.push(' ');
        if !is_selected && !palette.cmd_bar.is_empty() {
            out.push_str(palette.cmd_bar);
        }
        out.push(icon_committed(icons));
    }

    if show_badge {
        if !is_selected && marker_w == 2 && !palette.cmd_bar.is_empty() {
            out.push_str(palette.rst);
            out.push_str(base_style);
        }
        out.push(' ');
        if !is_selected {
            out.push_str(kind_color(theme, entry.kind));
        }
        out.push_str(badge);
    }
    if !base_style.is_empty()
        || (!is_selected && show_badge && !kind_color(theme, entry.kind).is_empty())
        || (!is_selected && marker_w == 2 && !palette.cmd_bar.is_empty())
        || (!match_positions.is_empty() && !palette.match_hl.is_empty())
    {
        out.push_str(palette.rst);
    }
}

#[cfg(test)]
fn push_miller_cell(
    out: &mut String,
    entry: &Entry,
    match_positions: &[usize],
    col_width: usize,
    is_selected: bool,
    is_active_pane: bool,
) {
    push_miller_cell_styled(
        out,
        entry,
        match_positions,
        col_width,
        is_selected,
        is_active_pane,
        false,
        UiStyle::default(),
    );
}

fn push_pane_cell(
    out: &mut String,
    pane: &PaneList,
    row_offset: usize,
    col_width: usize,
    is_active_pane: bool,
    committed_tokens: &[String],
    style: UiStyle,
) {
    let idx = pane.scroll_idx() + row_offset;
    if let Some((entry, match_positions)) = pane.row_at(idx) {
        let is_committed = !committed_tokens.is_empty()
            && matches!(entry.kind, EntryKind::Flag | EntryKind::Param)
            && entry.is_emittable()
            && committed_tokens
                .iter()
                .any(|tok| entry.matches_parameter_token(tok));
        push_miller_cell_styled(
            out,
            entry,
            match_positions,
            col_width,
            idx == pane.sel_idx(),
            is_active_pane,
            is_committed,
            style,
        );
    } else {
        push_repeated_space(out, col_width);
    }
}

/// Read-only view of session state passed into [`render_frame`] and test assertions.
pub struct RenderState<'a> {
    pub viewport: Viewport,
    pub active_pane: Pane,
    pub panes: [&'a PaneList; 3],
    pub command_line: String,
    pub active_param_set: Option<&'a str>,
    pub subcmd_chain: &'a [String],
    pub committed_tokens: &'a [String],
    pub dialect: ShellDialect,
    pub inspector_expanded: bool,
    pub inspector_scroll: usize,
    pub style: UiStyle,
}

/// Resolves the currently inspected entry from the active pane, falling back to parent panes
/// when the active pane has no selection.
fn selected_info_entry<'a>(st: &RenderState<'a>) -> Option<&'a Entry> {
    let cur_p1 = st.panes[0].selected();
    let cur_p2 = st.panes[1].selected();
    let cur_p3 = st.panes[2].selected();

    match st.active_pane {
        Pane::Third => cur_p3.or(cur_p2).or(cur_p1),
        Pane::Second => cur_p2.or(cur_p1),
        Pane::First => cur_p1,
    }
}

/// Derives the Pane 3 inspector header `(info_title, info_desc)` from `RenderState`.
#[cfg(test)]
fn inspector_header(st: &RenderState<'_>) -> (String, String) {
    let info_entry = selected_info_entry(st);
    let info_glyph = icon_info(st.style.icons);
    let info_title = info_entry
        .map(|it| format!(" {info_glyph} {} {}", it.label, it.kind.badge()))
        .unwrap_or_else(|| format!(" {info_glyph} No selection"));
    let info_desc = info_entry
        .map(|it| format!("   {}", it.desc))
        .unwrap_or_default();
    (info_title, info_desc)
}

struct UnicodeLines<'a> {
    rest: &'a str,
}

impl<'a> Iterator for UnicodeLines<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let bytes = self.rest.as_bytes();
        let len = bytes.len();
        let mut i = 0usize;
        while i < len {
            match bytes[i] {
                b'\n' => {
                    let end = if i > 0 && bytes[i - 1] == b'\r' {
                        i - 1
                    } else {
                        i
                    };
                    let line = &self.rest[..end];
                    self.rest = &self.rest[i + 1..];
                    return Some(line);
                }
                0xc2 if i + 1 < len && bytes[i + 1] == 0x85 => {
                    let line = &self.rest[..i];
                    self.rest = &self.rest[i + 2..];
                    return Some(line);
                }
                0xe2 if i + 2 < len
                    && bytes[i + 1] == 0x80
                    && matches!(bytes[i + 2], 0xa8 | 0xa9) =>
                {
                    let line = &self.rest[..i];
                    self.rest = &self.rest[i + 3..];
                    return Some(line);
                }
                _ => i += 1,
            }
        }
        let tail = self.rest;
        self.rest = "";
        Some(tail)
    }
}

fn split_unicode_lines(text: &str) -> UnicodeLines<'_> {
    UnicodeLines { rest: text }
}

fn wrap_display_lines_bounded(text: &str, max_width: usize, max_lines: usize) -> Vec<String> {
    if max_width == 0 || max_lines == 0 {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(max_lines.min(16));
    let mut clean_line = String::new();
    for raw_line in split_unicode_lines(text) {
        if lines.len() >= max_lines {
            break;
        }
        strip_escapes_and_controls_into(raw_line, &mut clean_line);
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in clean_line.split_whitespace() {
            let word_w: usize = if is_printable_ascii(word) {
                word.len()
            } else {
                word.chars()
                    .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum()
            };
            let space_w = usize::from(!cur.is_empty());
            if cur_w + space_w + word_w <= max_width {
                if space_w == 1 {
                    cur.push(' ');
                    cur_w += 1;
                }
                cur.push_str(word);
                cur_w += word_w;
            } else {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    if lines.len() >= max_lines {
                        break;
                    }
                    cur_w = 0;
                }
                if word_w <= max_width {
                    cur.push_str(word);
                    cur_w = word_w;
                } else {
                    for ch in word.chars() {
                        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if cw > max_width {
                            continue;
                        }
                        if cur_w + cw > max_width && !cur.is_empty() {
                            lines.push(std::mem::take(&mut cur));
                            if lines.len() >= max_lines {
                                break;
                            }
                            cur_w = 0;
                        }
                        if cw == 0 && cur.is_empty() {
                            continue;
                        }
                        cur.push(ch);
                        cur_w += cw;
                    }
                    if lines.len() >= max_lines {
                        break;
                    }
                }
            }
        }
        if lines.len() >= max_lines {
            break;
        }
        if !cur.is_empty() {
            lines.push(cur);
        } else {
            lines.push(String::new());
        }
    }
    lines
}

#[cfg(test)]
fn wrap_display_lines(text: &str, max_width: usize) -> Vec<String> {
    wrap_display_lines_bounded(text, max_width, usize::MAX)
}

fn push_inspector_field(
    rows: &mut Vec<(&'static str, Cow<'static, str>)>,
    color: &'static str,
    raw: &str,
    w3: usize,
    wrap_w: usize,
    max_rows: usize,
) {
    if rows.len() >= max_rows {
        return;
    }
    let single_w = 1 + display_width_sanitized(raw);
    if single_w <= w3 {
        rows.push((color, Cow::Owned(format!(" {raw}"))));
    } else {
        let rem = max_rows - rows.len();
        for line in wrap_display_lines_bounded(raw, wrap_w, rem) {
            rows.push((color, Cow::Owned(format!(" {line}"))));
        }
    }
}

fn build_expanded_inspector_rows_styled(
    entry: Option<&Entry>,
    w3: usize,
    max_rows: usize,
    style: UiStyle,
) -> Vec<(&'static str, Cow<'static, str>)> {
    let mut rows = Vec::new();
    if max_rows == 0 {
        return rows;
    }
    let palette = palette_for(style.theme);
    let info_glyph = icon_info(style.icons);
    let ascii_borders = style.borders == BorderStyle::Ascii;
    let Some(it) = entry else {
        let msg = if w3 >= 26 {
            format!(" {info_glyph} No selection [F1 Close]")
        } else {
            format!(" {info_glyph} No selection")
        };
        rows.push((palette.header_act, Cow::Owned(msg)));
        return rows;
    };

    let full_hdr = format!(" {info_glyph} {} {} [F1 Close]", it.label, it.kind.badge());
    let hdr = if display_width_sanitized(&full_hdr) <= w3 {
        full_hdr
    } else {
        format!(" {info_glyph} {} {}", it.label, it.kind.badge())
    };
    rows.push((palette.header_act, Cow::Owned(hdr)));

    let wrap_w = w3.saturating_sub(2).max(1);
    let tok_val = if it.text.is_empty() {
        "(placeholder)"
    } else {
        it.text.as_str()
    };
    if it.aliases.is_empty() {
        push_inspector_field(
            &mut rows,
            palette.filter,
            &format!("Token: {tok_val}"),
            w3,
            wrap_w,
            max_rows,
        );
    } else {
        let aliases = it.aliases.join(", ");
        let single_raw = format!("Token: {tok_val}  Aliases: {aliases}");
        if display_width_sanitized(&single_raw) < w3 {
            if rows.len() < max_rows {
                rows.push((palette.filter, Cow::Owned(format!(" {single_raw}"))));
            }
        } else {
            push_inspector_field(
                &mut rows,
                palette.filter,
                &format!("Token: {tok_val}"),
                w3,
                wrap_w,
                max_rows,
            );
            push_inspector_field(
                &mut rows,
                palette.filter,
                &format!("Aliases: {aliases}"),
                w3,
                wrap_w,
                max_rows,
            );
        }
    }

    let sets_raw = if !it.parameter_sets.is_empty() {
        format!("Sets : {}", it.parameter_sets.join(", "))
    } else {
        format!("Kind : {}", it.kind.badge())
    };
    push_inspector_field(&mut rows, palette.dim, &sets_raw, w3, wrap_w, max_rows);
    if rows.len() < max_rows {
        rows.push((palette.border_dim, bar_cow_for(w3, ascii_borders)));
    }

    if rows.len() < max_rows {
        let rem = max_rows - rows.len();
        for line in wrap_display_lines_bounded(&it.desc, wrap_w, rem) {
            rows.push((palette.text, Cow::Owned(format!(" {line}"))));
        }
    }

    if rows.len() < max_rows && !it.children.is_empty() {
        let mut shown = [""; 8];
        let mut total_children = 0usize;
        for c in &it.children {
            if !c.is_placeholder() && !c.text.is_empty() {
                if total_children < 8 {
                    shown[total_children] = c.text.as_str();
                }
                total_children += 1;
            }
        }
        if total_children > 0 {
            rows.push((palette.border_dim, bar_cow_for(w3, ascii_borders)));
            if rows.len() < max_rows {
                let shown_count = total_children.min(8);
                let mut summary = String::from("Children: ");
                for (i, child_text) in shown[..shown_count].iter().enumerate() {
                    if i > 0 {
                        summary.push_str(", ");
                    }
                    summary.push_str(child_text);
                }
                if total_children > shown_count {
                    let _ = write!(
                        summary,
                        ", {} (+{} more)",
                        icon_ellipsis(style.icons),
                        total_children - shown_count
                    );
                }
                let rem = max_rows - rows.len();
                for line in wrap_display_lines_bounded(&summary, wrap_w, rem) {
                    rows.push((palette.dim, Cow::Owned(format!(" {line}"))));
                }
            }
        }
    }

    rows
}

#[cfg(test)]
fn build_expanded_inspector_rows(
    entry: Option<&Entry>,
    w3: usize,
    max_rows: usize,
) -> Vec<(&'static str, Cow<'static, str>)> {
    build_expanded_inspector_rows_styled(entry, w3, max_rows, UiStyle::default())
}

/// Computes `(w1, w2, w3)` inside `inner_w` summing strictly to `inner_w`.
fn split_pane_widths(inner_w: usize) -> (usize, usize, usize) {
    let w1 = (inner_w * 28 / 100).max(10).min(inner_w / 3);
    let w2 = (inner_w * 32 / 100)
        .max(12)
        .min((inner_w.saturating_sub(w1)) / 2);
    let w3 = inner_w.saturating_sub(w1 + w2);
    (w1, w2, w3)
}

/// Formats Pane 2's subcommand breadcrumb header into `out`, shortening leading segments
/// when `w2` is narrow so the deepest active subcommand and count remain legible.
fn write_pane2_breadcrumb_header(
    out: &mut String,
    p1_entry: Option<&Entry>,
    subcmd_chain: &[String],
    visible_len: usize,
    w2: usize,
    icons: IconMode,
) {
    let sep = if icons == IconMode::Ascii {
        " > "
    } else {
        " › "
    };
    let root = p1_entry
        .map(|e| {
            if e.kind == EntryKind::History {
                e.text.split_whitespace().next().unwrap_or(e.text.as_str())
            } else {
                e.text.as_str()
            }
        })
        .filter(|s| !s.is_empty());

    out.clear();
    out.push_str(" 2. ");
    let mut has_prev = false;
    if let Some(root_name) = root {
        out.push_str(root_name);
        has_prev = true;
    }
    for seg in subcmd_chain {
        if has_prev {
            out.push_str(sep);
        }
        out.push_str(seg);
        has_prev = true;
    }
    let _ = write!(out, " ({visible_len}) ");

    if display_width_sanitized(out) <= w2 {
        return;
    }

    if let Some(last_seg) = subcmd_chain.last() {
        let mut compact = String::new();
        let ellipsis = icon_ellipsis(icons);
        if subcmd_chain.len() > 1 || root.is_some() {
            let _ = write!(compact, " 2. {ellipsis}{sep}{last_seg} ({visible_len}) ");
        } else {
            let _ = write!(compact, " 2. {last_seg} ({visible_len}) ");
        }
        if display_width_sanitized(&compact) <= w2 {
            *out = compact;
        }
    }
}

/// Maximum wrapped rows retained by the scrollable `F1` Inspector drawer.
const MAX_INSPECTOR_SCROLLABLE_ROWS: usize = 512;

/// Returns the maximum valid scroll offset for the expanded `F1` Inspector on `entry`.
pub(crate) fn expanded_inspector_max_scroll(
    entry: Option<&Entry>,
    viewport: Viewport,
    style: UiStyle,
) -> usize {
    let win_w = viewport.width.max(Viewport::MIN_WIDTH.saturating_sub(1));
    let list_rows = viewport.rows(Pane::First);
    let inner_w = win_w.saturating_sub(4);
    let (_w1, _w2, w3) = split_pane_widths(inner_w);
    let total_rows =
        build_expanded_inspector_rows_styled(entry, w3, MAX_INSPECTOR_SCROLLABLE_ROWS, style).len();
    total_rows.saturating_sub(list_rows)
}

fn write_inspector_header(
    out: &mut String,
    scroll: usize,
    list_rows: usize,
    total_rows: usize,
    w3: usize,
    icons: IconMode,
) {
    out.clear();
    if total_rows <= list_rows || list_rows == 0 {
        out.push_str(" 3. Inspector [F1 Close] ");
        return;
    }
    let start_row = scroll + 1;
    let end_row = (scroll + list_rows).min(total_rows);
    let can_up = scroll > 0;
    let can_down = scroll + list_rows < total_rows;
    let arrows = match (can_up, can_down, icons) {
        (true, true, IconMode::Unicode) => "↑↓ ",
        (true, false, IconMode::Unicode) => "↑ ",
        (false, true, IconMode::Unicode) => "↓ ",
        (true, true, IconMode::Ascii) => "^v ",
        (true, false, IconMode::Ascii) => "^ ",
        (false, true, IconMode::Ascii) => "v ",
        _ => "",
    };
    let _ = write!(
        out,
        " 3. Inspector [{arrows}{start_row}-{end_row}/{total_rows}] [F1 Close] "
    );
    if display_width_sanitized(out) <= w3 {
        return;
    }
    out.clear();
    let _ = write!(
        out,
        " 3. Inspector [{arrows}{start_row}-{end_row}/{total_rows}] "
    );
    if display_width_sanitized(out) <= w3 {
        return;
    }
    out.clear();
    let _ = write!(out, " 3. [{arrows}{start_row}-{end_row}/{total_rows}] ");
}

const FOOTER_PS: &str = " [↑/↓|Ctrl+N/P] Move  [←/→] Pane  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [F1] Inspect  [Ctrl+R] Sync PS  [Ctrl+O] Pipe  [Enter] Accept  [Esc]";
const FOOTER_RELOAD: &str = " [↑/↓|Ctrl+N/P] Move  [←/→] Pane  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [F1] Inspect  [Ctrl+R] Reload  [Ctrl+O] Pipe  [Enter] Accept  [Esc]";
const FOOTER_PS_ASCII: &str = " [^/v|Ctrl+N/P] Move  [</>] Pane  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [F1] Inspect  [Ctrl+R] Sync PS  [Ctrl+O] Pipe  [Enter] Accept  [Esc]";
const FOOTER_RELOAD_ASCII: &str = " [^/v|Ctrl+N/P] Move  [</>] Pane  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [F1] Inspect  [Ctrl+R] Reload  [Ctrl+O] Pipe  [Enter] Accept  [Esc]";

pub fn render_frame(st: &RenderState<'_>) -> String {
    let palette = palette_for(st.style.theme);
    let borders = border_chars_for(st.style.borders);
    let icons = st.style.icons;
    let info_glyph = icon_info(icons);
    let cursor_ch = icon_cursor(icons);

    let win_w = st.viewport.width.max(Viewport::MIN_WIDTH.saturating_sub(1));
    let list_rows = st.viewport.rows(Pane::First);
    let inner_w = win_w.saturating_sub(4);
    let (w1, w2, w3) = split_pane_widths(inner_w);
    let info_entry = if st.inspector_expanded {
        None
    } else {
        selected_info_entry(st)
    };
    let expanded_rows = if st.inspector_expanded {
        build_expanded_inspector_rows_styled(
            selected_info_entry(st),
            w3,
            MAX_INSPECTOR_SCROLLABLE_ROWS,
            st.style,
        )
    } else {
        Vec::new()
    };
    let inspector_total_rows = expanded_rows.len();
    let inspector_scroll = st
        .inspector_scroll
        .min(inspector_total_rows.saturating_sub(list_rows));

    let estimated_cap = ((list_rows + 6) * (win_w * 2 + 128)).clamp(4096, 65536);
    let mut sb = String::with_capacity(estimated_cap);
    let mut scratch = String::with_capacity(256);
    let mut sanitize_scratch = String::with_capacity(256);
    sb.push_str("\x1b[?2026h\x1b[H"); // Begin synchronized update + Cursor to home (0,0) for flicker-free overwrite

    let header_color = |pane: Pane| {
        if st.active_pane == pane {
            palette.header_act
        } else {
            palette.header_dim
        }
    };

    sb.push_str(palette.border_dim);
    sb.push(borders.top_left);
    scratch.clear();
    let _ = write!(
        scratch,
        " 1. Command / Scope ({}) ",
        st.panes[0].visible_len()
    );
    push_pane_header(
        &mut sb,
        header_color(Pane::First),
        &scratch,
        w1,
        palette,
        icons,
        borders.is_ascii,
    );

    sb.push(borders.t_down);
    scratch.clear();
    if !st.subcmd_chain.is_empty() {
        write_pane2_breadcrumb_header(
            &mut scratch,
            st.panes[0].selected(),
            st.subcmd_chain,
            st.panes[1].visible_len(),
            w2,
            icons,
        );
    } else if let Some(set_name) = st.active_param_set {
        let _ = write!(
            scratch,
            " 2. Params [Set: {set_name}] ({}) ",
            st.panes[1].visible_len()
        );
    } else {
        let _ = write!(
            scratch,
            " 2. Subcommands / Params ({}) ",
            st.panes[1].visible_len()
        );
    }
    push_pane_header(
        &mut sb,
        header_color(Pane::Second),
        &scratch,
        w2,
        palette,
        icons,
        borders.is_ascii,
    );

    sb.push(borders.t_down);
    if st.inspector_expanded {
        write_inspector_header(
            &mut scratch,
            inspector_scroll,
            list_rows,
            inspector_total_rows,
            w3,
            icons,
        );
        push_pane_header(
            &mut sb,
            header_color(Pane::Third),
            &scratch,
            w3,
            palette,
            icons,
            borders.is_ascii,
        );
    } else {
        scratch.clear();
        let _ = write!(
            scratch,
            " 3. Flags / Values / Info ({}) ",
            st.panes[2].visible_len()
        );
        push_pane_header(
            &mut sb,
            header_color(Pane::Third),
            &scratch,
            w3,
            palette,
            icons,
            borders.is_ascii,
        );
    }
    sb.push(borders.top_right);
    push_styled_end(&mut sb, palette.border_dim, palette.rst);
    sb.push_str("\x1b[K\r\n");

    for r in 0..list_rows {
        sb.push_str(palette.border_dim);
        sb.push(borders.vertical);
        push_styled_end(&mut sb, palette.border_dim, palette.rst);
        push_pane_cell(
            &mut sb,
            st.panes[0],
            r,
            w1,
            st.active_pane == Pane::First,
            &[],
            st.style,
        );

        sb.push_str(palette.border_dim);
        sb.push(borders.vertical);
        push_styled_end(&mut sb, palette.border_dim, palette.rst);
        push_pane_cell(
            &mut sb,
            st.panes[1],
            r,
            w2,
            st.active_pane == Pane::Second,
            st.committed_tokens,
            st.style,
        );

        sb.push_str(palette.border_dim);
        sb.push(borders.vertical);
        push_styled_end(&mut sb, palette.border_dim, palette.rst);
        if st.inspector_expanded {
            if let Some((color, text)) = expanded_rows.get(inspector_scroll + r) {
                sb.push_str(color);
                push_padded_text_with(&mut sb, text, w3, icons);
                push_styled_end(&mut sb, color, palette.rst);
            } else {
                push_repeated_space(&mut sb, w3);
            }
        } else {
            // Pane 3: Top 3 rows = Inspector Header, Rows 3.. = Interactive List
            match r {
                0 => {
                    sb.push_str(palette.header_act);
                    if let Some(it) = info_entry {
                        scratch.clear();
                        let _ = write!(scratch, " {info_glyph} {} {}", it.label, it.kind.badge());
                        push_padded_text_with(&mut sb, &scratch, w3, icons);
                    } else {
                        scratch.clear();
                        let _ = write!(scratch, " {info_glyph} No selection");
                        push_padded_text_with(&mut sb, &scratch, w3, icons);
                    }
                    push_styled_end(&mut sb, palette.header_act, palette.rst);
                }
                1 => {
                    sb.push_str(palette.dim);
                    if let Some(it) = info_entry {
                        scratch.clear();
                        scratch.push_str("   ");
                        scratch.push_str(&it.desc);
                        push_padded_text_with(&mut sb, &scratch, w3, icons);
                    } else {
                        push_repeated_space(&mut sb, w3);
                    }
                    push_styled_end(&mut sb, palette.dim, palette.rst);
                }
                2 => {
                    sb.push_str(palette.border_dim);
                    push_repeated_bar_for(&mut sb, w3, borders.is_ascii);
                    push_styled_end(&mut sb, palette.border_dim, palette.rst);
                }
                _ => push_pane_cell(
                    &mut sb,
                    st.panes[2],
                    r - PANE3_INSPECTOR_ROWS,
                    w3,
                    st.active_pane == Pane::Third,
                    &[],
                    st.style,
                ),
            }
        }

        sb.push_str(palette.border_dim);
        sb.push(borders.vertical);
        push_styled_end(&mut sb, palette.border_dim, palette.rst);
        sb.push_str("\x1b[K\r\n");
    }

    // Divider above status bars
    sb.push_str(palette.border_dim);
    sb.push(borders.t_right);
    push_repeated_bar_for(&mut sb, w1, borders.is_ascii);
    sb.push(borders.t_up);
    push_repeated_bar_for(&mut sb, w2, borders.is_ascii);
    sb.push(borders.t_up);
    push_repeated_bar_for(&mut sb, w3, borders.is_ascii);
    sb.push(borders.t_left);
    push_styled_end(&mut sb, palette.border_dim, palette.rst);
    sb.push_str("\x1b[K\r\n");

    let full_inner = win_w.saturating_sub(2);

    let active_filter = st.panes[st.active_pane.index()].filter();
    let filter_prefix = match st.active_pane {
        Pane::First => " Filter (Pane 1): ",
        Pane::Second => " Filter (Pane 2): ",
        Pane::Third => " Filter (Pane 3): ",
    };
    let active_filter_w = display_width_sanitized(active_filter) + 1; // + cursor_ch
    scratch.clear();
    let _ = write!(
        scratch,
        "{filter_prefix}{active_filter}{cursor_ch}   [P1: '{}' | P2: '{}' | P3: '{}']",
        st.panes[0].filter(),
        st.panes[1].filter(),
        st.panes[2].filter()
    );
    if display_width_sanitized(&scratch) <= full_inner
        || filter_prefix.len() + active_filter_w <= full_inner
    {
        push_status_row(
            &mut sb,
            palette.filter,
            &scratch,
            full_inner,
            palette,
            borders,
            icons,
        );
    } else {
        scratch.clear();
        scratch.push_str(active_filter);
        scratch.push(cursor_ch);
        push_tail_status_row(
            &mut sb,
            palette.filter,
            filter_prefix,
            &scratch,
            full_inner,
            palette,
            borders,
            icons,
            &mut sanitize_scratch,
        );
    }

    push_tail_status_row(
        &mut sb,
        palette.cmd_bar,
        " Command Line  : ",
        &st.command_line,
        full_inner,
        palette,
        borders,
        icons,
        &mut sanitize_scratch,
    );

    let footer = match (st.dialect.is_powershell(), icons) {
        (true, IconMode::Unicode) => FOOTER_PS,
        (false, IconMode::Unicode) => FOOTER_RELOAD,
        (true, IconMode::Ascii) => FOOTER_PS_ASCII,
        (false, IconMode::Ascii) => FOOTER_RELOAD_ASCII,
    };
    push_status_row(
        &mut sb,
        palette.dim,
        footer,
        full_inner,
        palette,
        borders,
        icons,
    );

    sb.push_str(palette.border_dim);
    sb.push(borders.bottom_left);
    push_repeated_bar_for(&mut sb, full_inner, borders.is_ascii);
    sb.push(borders.bottom_right);
    push_styled_end(&mut sb, palette.border_dim, palette.rst);
    sb.push_str("\x1b[K\x1b[J\x1b[?2026l");

    sb
}

fn push_framed_status_row(
    out: &mut String,
    color: &str,
    palette: &ThemePalette,
    borders: BorderChars,
    write_inner: impl FnOnce(&mut String),
) {
    out.push_str(palette.border_dim);
    out.push(borders.vertical);
    push_styled_end(out, palette.border_dim, palette.rst);
    out.push_str(color);
    write_inner(out);
    push_styled_end(out, color, palette.rst);
    out.push_str(palette.border_dim);
    out.push(borders.vertical);
    push_styled_end(out, palette.border_dim, palette.rst);
    out.push_str("\x1b[K\r\n");
}

fn push_status_row(
    out: &mut String,
    color: &str,
    text: &str,
    inner_width: usize,
    palette: &ThemePalette,
    borders: BorderChars,
    icons: IconMode,
) {
    push_framed_status_row(out, color, palette, borders, |buf| {
        push_padded_text_with(buf, text, inner_width, icons)
    });
}

#[allow(clippy::too_many_arguments)]
fn push_tail_status_row(
    out: &mut String,
    color: &str,
    prefix: &str,
    body: &str,
    inner_width: usize,
    palette: &ThemePalette,
    borders: BorderChars,
    icons: IconMode,
    scratch: &mut String,
) {
    push_framed_status_row(out, color, palette, borders, |buf| {
        push_tail_truncated_padded(buf, prefix, body, inner_width, icons, scratch);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sanitize::is_bidi_control;
    use unicode_width::UnicodeWidthStr;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&'[') {
                chars.next();
                for inner in chars.by_ref() {
                    if ('@'..='~').contains(&inner) {
                        break;
                    }
                }
            } else if c != '\r' {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn test_viewport_rows_standard_and_small_terminals() {
        let standard = Viewport::new(120, 30);
        assert_eq!(standard.rows(Pane::First), 24);
        assert_eq!(standard.rows(Pane::Second), 24);
        assert_eq!(standard.rows(Pane::Third), 21);

        let compact = Viewport::new(90, 12);
        assert_eq!(compact.rows(Pane::First), 6);
        assert_eq!(compact.rows(Pane::Second), 6);
        assert_eq!(compact.rows(Pane::Third), 3);

        let tiny = Viewport::new(80, 8);
        assert_eq!(tiny.rows(Pane::First), 2);
        assert_eq!(tiny.rows(Pane::Second), 2);
        assert_eq!(tiny.rows(Pane::Third), 0);
        assert_eq!(tiny.height - FRAME_CHROME_ROWS, tiny.rows(Pane::First));
    }

    #[test]
    fn test_viewport_from_terminal_size_and_resized_clamp_below_minimum() {
        let vp = Viewport::from_terminal_size(100, 30, 120, 34);
        assert_eq!(vp, Viewport::new(99, 29));

        let fallback = Viewport::from_terminal_size(20, 5, 120, 34);
        assert_eq!(fallback, Viewport::new(119, 33));

        // Below-minimum resize keeps prior axis
        assert_eq!(vp.resized(25, 18), Viewport::new(99, 17));
        assert_eq!(vp.resized(50, 8), Viewport::new(49, 29));
    }

    #[test]
    fn test_tiny_terminal_frame_never_exceeds_height() {
        let empty = PaneList::default();
        let st = RenderState {
            viewport: Viewport::new(80, 8),
            active_pane: Pane::First,
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        };

        assert_eq!(render_frame(&st).lines().count(), 8);
    }

    #[test]
    fn test_narrow_terminal_scales_proportionally_without_wrapping() {
        let pane1 = PaneList::new(vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")]);
        let pane2 = PaneList::new(vec![Entry::new(
            "commit",
            "commit",
            EntryKind::Subcmd,
            "Record changes",
        )]);
        let pane3 = PaneList::new(vec![Entry::new(
            "--amend",
            "--amend",
            EntryKind::Flag,
            "Amend commit",
        )]);

        for width in [Viewport::MIN_WIDTH, 55, 75] {
            let st = RenderState {
                viewport: Viewport::new(width, 16),
                active_pane: Pane::First,
                panes: [&pane1, &pane2, &pane3],
                command_line: "git commit".to_string(),
                active_param_set: None,
                subcmd_chain: &[],
                committed_tokens: &[],
                dialect: ShellDialect::Posix,
                inspector_expanded: false,
                inspector_scroll: 0,
                style: UiStyle::default(),
            };
            let plain = strip_ansi(&render_frame(&st));
            let lines: Vec<&str> = plain.lines().collect();
            assert_eq!(lines.len(), 16);
            for (idx, line) in lines.iter().enumerate() {
                assert_eq!(
                    UnicodeWidthStr::width(*line),
                    width,
                    "Line {idx} at width {width} must fit exact frame width without wrapping: {line:?}"
                );
            }
        }
    }

    #[test]
    fn test_footer_ctrl_r_hint_is_dialect_aware() {
        let empty = PaneList::default();
        let ps_frame = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(150, 14),
            active_pane: Pane::First,
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::PowerShell,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        assert!(ps_frame.contains("[Ctrl+R] Sync PS"));

        let posix_frame = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(150, 14),
            active_pane: Pane::First,
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        assert!(posix_frame.contains("[Ctrl+R] Reload"));
        assert!(!posix_frame.contains("Sync PS"));
    }

    #[test]
    fn test_small_terminal_scroll_alignment() {
        let col1_entries: Vec<Entry> = (0..10)
            .map(|i| Entry::new(format!("cmd{i}"), format!("cmd{i}"), EntryKind::Cli, ""))
            .collect();
        let mut pane1 = PaneList::new(col1_entries);

        let col3_entries: Vec<Entry> = (0..10)
            .map(|i| {
                Entry::new(
                    format!("--flag{i}"),
                    format!("--flag{i}"),
                    EntryKind::Flag,
                    "",
                )
            })
            .collect();
        let mut pane3 = PaneList::new(col3_entries);

        let term_h = 12;
        let viewport = Viewport::new(100, term_h);

        pane1.set_sel_idx(7);
        pane1.clamp_scroll(viewport.rows(Pane::First));
        pane3.set_sel_idx(4);
        pane3.clamp_scroll(viewport.rows(Pane::Third));

        let empty = PaneList::default();
        let frame = render_frame(&RenderState {
            viewport,
            active_pane: Pane::Third,
            panes: [&pane1, &empty, &pane3],
            command_line: "cmd7 --flag4".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        });

        assert_eq!(frame.lines().count(), term_h);
        assert!(frame.contains("cmd7"));
        assert!(!frame.contains("cmd1"));
        assert!(frame.contains("--flag4"));
        assert!(!frame.contains("--flag1"));
    }

    #[test]
    fn test_inspector_header_derivation() {
        let pane1 = PaneList::new(vec![Entry::new(
            "git",
            "git",
            EntryKind::Cli,
            "Distributed version control",
        )]);
        let pane2 = PaneList::new(vec![Entry::new(
            "commit",
            "commit",
            EntryKind::Subcmd,
            "Record changes to the repository",
        )]);
        let empty = PaneList::default();

        let st_p1 = RenderState {
            viewport: Viewport::new(100, 20),
            active_pane: Pane::First,
            panes: [&pane1, &pane2, &empty],
            command_line: "git".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        };
        let (title0, desc0) = inspector_header(&st_p1);
        assert_eq!(title0, " ℹ git [CLI]");
        assert_eq!(desc0, "   Distributed version control");

        let st_p3_fallback = RenderState {
            active_pane: Pane::Third,
            command_line: "git".to_string(),
            ..st_p1
        };
        let (title2, desc2) = inspector_header(&st_p3_fallback);
        assert_eq!(title2, " ℹ commit [Subcmd]");
        assert_eq!(desc2, "   Record changes to the repository");
        let frame = render_frame(&st_p3_fallback);
        assert!(frame.contains("ℹ commit [Subcmd]"));
        assert!(frame.contains("Record changes to the repository"));

        let st_empty = RenderState {
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            ..st_p1
        };
        let (title_empty, desc_empty) = inspector_header(&st_empty);
        assert_eq!(title_empty, " ℹ No selection");
        assert_eq!(desc_empty, "");
    }

    #[test]
    fn test_push_padded_text_preserves_width_on_wide_chars() {
        let mut buf = String::new();
        // '中' and '文' are 2 display columns each (total 4), plus 'a' overflows width 4.
        push_padded_text(&mut buf, "中文a", 4);
        assert_eq!(UnicodeWidthStr::width(buf.as_str()), 4);
        assert_eq!(buf, "中… ");

        buf.clear();
        push_padded_text(&mut buf, "a\nb\tc", 6);
        assert_eq!(buf, "a b c ");
    }

    #[test]
    fn test_expanded_inspector_drawer_wraps_full_description_and_metadata() {
        let param = Entry::new(
            "-Id",
            "-Id (-PID)",
            EntryKind::Param,
            "Gets processes that are identified by one or more process IDs on the local host.",
        )
        .with_aliases(vec!["PID".to_string()])
        .with_parameter_sets(vec!["Id".to_string(), "IdWithUserName".to_string()]);
        let pane1 = PaneList::new(vec![Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "",
        )]);
        let pane2 = PaneList::new(vec![param]);
        let empty = PaneList::default();

        let plain = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 18),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "Get-Process -Id".to_string(),
            active_param_set: Some("Id"),
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::PowerShell,
            inspector_expanded: true,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));

        assert_eq!(plain.lines().count(), 18);
        assert!(plain.contains("3. Inspector [F1 Close]"));
        assert!(plain.contains("Token: -Id  Aliases: PID"));
        assert!(plain.contains("Sets : Id, IdWithUserName"));
        assert!(plain.contains("Gets processes that are identified by"));
        assert!(plain.contains("one or more process IDs on the local"));
        assert!(plain.contains("host."));
    }

    #[test]
    fn test_combining_marks_and_control_chars_sanitize_and_truncate_cleanly() {
        let mut buf = String::new();
        // 'ab' + combining acute + 'c' in width 2 must roll back the 'b\u{0301}' cluster to 'a…'
        push_padded_text(&mut buf, "ab\u{0301}c", 2);
        assert_eq!(UnicodeWidthStr::width(buf.as_str()), 2);
        assert_eq!(buf, "a…");

        buf.clear();
        // Control chars such as ESC/BEL must be stripped so terminal escape injection cannot occur
        push_padded_text(&mut buf, "\x1bfoo\x07bar", 8);
        assert_eq!(buf, "foobar  ");
        assert!(!buf.contains('\x1b'));
    }

    #[test]
    fn test_miller_cell_exact_overflow_emits_ellipsis_with_match_highlights() {
        let mut pane = PaneList::new(vec![Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "",
        )]);
        pane.push_filter_char('g');
        pane.push_filter_char('p');

        // Width 20 -> ptr(2) + label(9) + space(1) + [Cmdlet](8) = 20
        let mut raw = String::new();
        let (entry, matches) = pane.row_at(0).unwrap();
        push_miller_cell(&mut raw, entry, matches, 20, true, true);
        let plain = strip_ansi(&raw);
        assert_eq!(UnicodeWidthStr::width(plain.as_str()), 20);
        assert_eq!(plain, "❯ Get-Proc… [Cmdlet]");
        assert!(raw.contains(DARK_PALETTE.match_hl));
        assert!(raw.ends_with(DARK_PALETTE.rst));

        // Exact-width wide-char label truncation keeps valid ANSI reset and width
        let wide_entry = Entry::new("中文abc", "中文abc", EntryKind::Arg, "");
        raw.clear();
        push_miller_cell(&mut raw, &wide_entry, &[1], 6, false, false);
        let wide_plain = strip_ansi(&raw);
        assert_eq!(UnicodeWidthStr::width(wide_plain.as_str()), 6);
        assert_eq!(wide_plain, "  中… ");
        assert!(raw.ends_with(DARK_PALETTE.rst));
    }

    #[test]
    fn test_wrap_display_lines_preserves_blank_lines_and_handles_narrow_wide_chars() {
        let wrapped = wrap_display_lines("First line.\n\nSecond line.", 20);
        assert_eq!(wrapped, vec!["First line.", "", "Second line."]);

        let narrow_wide = wrap_display_lines("中文a", 1);
        assert_eq!(narrow_wide, vec!["a"]);
    }

    #[test]
    fn test_narrow_expanded_inspector_wraps_metadata_and_tail_scrolls_status_bars() {
        let mut param = Entry::new(
            "-VeryLongParameterName",
            "-VeryLongParameterName",
            EntryKind::Param,
            "First paragraph.\n\nSecond paragraph with detail.",
        )
        .with_aliases(vec!["VLP".to_string(), "AliasTwo".to_string()])
        .with_parameter_sets(vec!["SetAlpha".to_string(), "SetBeta".to_string()]);
        param.children = (0..10)
            .map(|i| Entry::new(format!("val{i}"), format!("val{i}"), EntryKind::Value, ""))
            .collect();

        let pane1 = PaneList::new(vec![Entry::new("cmd", "cmd", EntryKind::Cli, "")]);
        let mut pane2 = PaneList::new(vec![param]);
        for ch in "very".chars() {
            pane2.push_filter_char(ch);
        }
        let empty = PaneList::default();

        // Actual 40x18 terminal clamps to Viewport(39, 17) and must render exact width 39
        let vp = Viewport::from_terminal_size(40, 18, 120, 30);
        assert_eq!(vp, Viewport::new(39, 17));

        let raw = render_frame(&RenderState {
            viewport: vp,
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "cmd -VeryLongParameterName value-at-the-very-end".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
            inspector_scroll: 0,
            style: UiStyle::default(),
        });
        assert!(raw.contains("\x1b[K\r\n"));
        assert!(raw.contains("\x1b[K\x1b[J\x1b[?2026l"));

        let plain = strip_ansi(&raw);
        let lines: Vec<&str> = plain.lines().collect();
        assert_eq!(lines.len(), 17);
        for line in &lines {
            assert_eq!(UnicodeWidthStr::width(*line), 39);
        }

        // Aliases & Parameter Sets wrap instead of silently disappearing on narrow Pane 3
        assert!(plain.contains("Aliases:"));
        assert!(plain.contains("VLP,"));
        assert!(plain.contains("AliasTwo"));
        assert!(plain.contains("Sets :"));
        assert!(plain.contains("SetBeta"));
        // Long Command Line and active Filter preserve their trailing active content
        assert!(plain.contains("…") && plain.contains("very-end"));
        assert!(plain.contains("very█"));
    }

    #[test]
    fn test_pentest_escape_osc_csi_c0_c1_bidi_injection_sanitized_across_all_surfaces() {
        let mut hostile_cmd = Entry::new(
            "git",
            "\x1b]52;c;SGVsbG8=\x07\x1b[31mgit\x1b[0m\u{202e}",
            EntryKind::Cli,
            "\x1b]0;root@prod:~#\x1b\\Read \u{009b}2Jfiles\u{0085}safely\x00\x7f",
        )
        .with_aliases(vec!["\x1b[32mg\x1b[0m".to_string()])
        .with_parameter_sets(vec!["\x1b]2;spoof\x07DefaultSet".to_string()]);
        hostile_cmd.children = vec![Entry::new(
            "child\x1b]52;c;YQ==\x07",
            "child",
            EntryKind::Subcmd,
            "",
        )];

        let mut pane1 = PaneList::new(vec![hostile_cmd]);
        pane1.push_filter_char('\x1b');
        pane1.push_filter_char('\x07');
        pane1.push_filter_char('\u{009b}');
        pane1.push_filter_char('g');
        pane1.push_filter_char('i');
        pane1.push_filter_char('t');
        assert_eq!(pane1.filter(), "git");

        let pane2 = PaneList::new(vec![Entry::new(
            "commit",
            "\u{009d}52;c;SGVsbG8=\u{009c}commit\x1bP1$r\x1b\\",
            EntryKind::Subcmd,
            "\u{2028}Line1\u{2029}Line2\u{2066}bidi",
        )]);
        let empty = PaneList::default();

        for expanded in [false, true] {
            let raw = render_frame(&RenderState {
                viewport: Viewport::new(110, 18),
                active_pane: Pane::First,
                panes: [&pane1, &pane2, &empty],
                command_line: "git \x1b]52;c;cHduZWQ=\x07\x1b[6nstatus\u{009b}31m\x00\x7f"
                    .to_string(),
                active_param_set: Some("\x1b]0;title\x07IdSet\u{202e}"),
                subcmd_chain: &[],
                committed_tokens: &[],
                dialect: ShellDialect::Posix,
                inspector_expanded: expanded,
                inspector_scroll: 0,
                style: UiStyle::default(),
            });

            // Raw rendered frame must contain zero OSC, BEL, NUL, DEL, C1, BiDi, or injected CSI
            assert!(
                !raw.contains("\x1b]"),
                "OSC leaked in expanded={expanded}: {raw:?}"
            );
            assert!(
                !raw.contains("\x1bP"),
                "DCS leaked in expanded={expanded}: {raw:?}"
            );
            assert!(!raw.contains("\x1b[6n"), "DSR CSI leaked");
            assert!(!raw.contains("SGVsbG8="), "OSC 52 payload debris leaked");
            assert!(
                !raw.contains("cHduZWQ="),
                "OSC 52 command-line payload debris leaked"
            );
            assert!(!raw.contains("root@prod"), "OSC title spoof debris leaked");
            assert!(!raw.contains('\x07'));
            assert!(!raw.contains('\x00'));
            assert!(!raw.contains('\x7f'));
            assert!(
                !raw.chars().any(|c| ('\u{0080}'..='\u{009f}').contains(&c)
                    || is_bidi_control(c)
                    || matches!(c, '\u{2028}' | '\u{2029}')),
                "C1, BiDi, or Unicode line-sep leaked in frame"
            );

            let plain = strip_ansi(&raw);
            let lines: Vec<&str> = plain.lines().collect();
            assert_eq!(lines.len(), 18);
            for line in &lines {
                assert_eq!(UnicodeWidthStr::width(*line), 110);
            }
            assert!(plain.contains("git"));
            assert!(plain.contains("commit"));
            assert!(plain.contains("Params [Set: IdSet]"));
            assert!(plain.contains("git status"));
            if expanded {
                assert!(plain.contains("Read files"));
                assert!(plain.contains("safely"));
            }
        }
    }

    #[test]
    fn test_pentest_unterminated_osc_and_huge_inspector_desc_bounded() {
        let huge_desc = (0..5000)
            .map(|i| format!("Line {i}: \x1b]52;c;unterminated_without_terminator"))
            .collect::<Vec<_>>()
            .join("\n");
        let entry = Entry::new("cmd", "cmd\x1b]0;unclosed", EntryKind::Cli, huge_desc);
        let pane1 = PaneList::new(vec![entry]);
        let empty = PaneList::default();

        let vp = Viewport::new(80, 14);
        let rows = build_expanded_inspector_rows(pane1.selected(), 30, vp.rows(Pane::First));
        assert!(rows.len() <= vp.rows(Pane::First));

        let raw = render_frame(&RenderState {
            viewport: vp,
            active_pane: Pane::First,
            panes: [&pane1, &empty, &empty],
            command_line: "cmd".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
            inspector_scroll: 0,
            style: UiStyle::default(),
        });
        assert!(!raw.contains("\x1b]"));
        let plain = strip_ansi(&raw);
        assert_eq!(plain.lines().count(), 14);
        for line in plain.lines() {
            assert_eq!(UnicodeWidthStr::width(line), 80);
        }
    }

    #[test]
    fn test_unicode_lines_iterator_and_bar_cow_preserve_boundaries() {
        assert_eq!(BAR_LINE.chars().count(), BAR_LINE_CHARS);
        assert_eq!(BAR_LINE.len(), BAR_LINE_CHARS * 3);
        assert!(matches!(bar_cow(0), Cow::Borrowed("")));
        let small = bar_cow(5);
        assert!(matches!(small, Cow::Borrowed(_)));
        assert_eq!(small.as_ref(), "─────");
        let max_static = bar_cow(BAR_LINE_CHARS);
        assert!(matches!(max_static, Cow::Borrowed(_)));
        assert_eq!(max_static.chars().count(), BAR_LINE_CHARS);
        let wide = bar_cow(BAR_LINE_CHARS + 22);
        assert!(matches!(wide, Cow::Owned(_)));
        assert_eq!(wide.chars().count(), BAR_LINE_CHARS + 22);

        let empty: Vec<&str> = split_unicode_lines("").collect();
        assert!(empty.is_empty());

        let trailing: Vec<&str> = split_unicode_lines("a\r\nb\nc\n").collect();
        assert_eq!(trailing, vec!["a", "b", "c"]);

        let leading: Vec<&str> = split_unicode_lines("\n\r\n").collect();
        assert_eq!(leading, vec!["", ""]);

        let unicode_seps: Vec<&str> = split_unicode_lines("a\u{0085}b\u{2028}c\u{2029}d").collect();
        assert_eq!(unicode_seps, vec!["a", "b", "c", "d"]);

        let cr_before_nel: Vec<&str> = split_unicode_lines("a\r\u{0085}b").collect();
        assert_eq!(cr_before_nel, vec!["a\r", "b"]);

        let cjk: Vec<&str> = split_unicode_lines("中文\n日本語\r\n").collect();
        assert_eq!(cjk, vec!["中文", "日本語"]);
    }

    #[test]
    fn test_pentest_ecma48_bidi_wide_chars_and_edge_widths() {
        let mut clean = String::new();
        // 7-bit SOS, PM, APC, DCS and 8-bit C1 control strings must all be stripped cleanly.
        let hostile_ctrl_strings = concat!(
            "\x1bXsecret_sos\x1b\\",
            "\x1b^secret_pm\x1b\\",
            "\x1b_secret_apc\x1b\\",
            "\x1bP1$rsecret_dcs\x1b\\",
            "\u{0098}c1_sos\u{009c}",
            "\u{009e}c1_pm\x1b\\",
            "\u{009f}c1_apc\u{009c}",
            "\u{0090}c1_dcs\u{009c}",
            "safe",
            "\u{061c}\u{200e}\u{200f}\u{202a}\u{202e}\u{2066}\u{2069}",
            "\u{0085}\u{2028}\u{2029}",
            "text"
        );
        strip_escapes_and_controls_into(hostile_ctrl_strings, &mut clean);
        assert_eq!(clean, "safe   text");
        for hidden in [
            "secret_sos",
            "secret_pm",
            "secret_apc",
            "secret_dcs",
            "c1_sos",
            "c1_pm",
            "c1_apc",
            "c1_dcs",
        ] {
            assert!(!clean.contains(hidden), "leaked control payload {hidden}");
        }

        // Oversized (> MAX_STR_SCAN) OSC payload must stop scanning cleanly without DoS or panic.
        let oversized_osc = format!("\x1b]52;c;{}\x07after", "A".repeat(5000));
        strip_escapes_and_controls_into(&oversized_osc, &mut clean);
        assert!(!clean.contains('\x1b'));
        assert!(!clean.contains('\x07'));

        // Edge widths on wide CJK, combining marks, and cells must never panic or exceed width.
        let wide_entry = Entry::new(
            "中文cmd",
            "中文コマンド",
            EntryKind::Cmdlet,
            "e\u{0301}説明",
        )
        .with_aliases(vec!["alias1".to_string()])
        .with_parameter_sets(vec!["SetOne".to_string()]);
        let mut cell_buf = String::new();
        for w in [0usize, 1, 2, 3, 12] {
            cell_buf.clear();
            push_padded_text(&mut cell_buf, "中文a", w);
            assert_eq!(UnicodeWidthStr::width(cell_buf.as_str()), w);

            cell_buf.clear();
            push_miller_cell(&mut cell_buf, &wide_entry, &[0, 2], w, true, true);
            let plain_cell = strip_ansi(&cell_buf);
            assert_eq!(UnicodeWidthStr::width(plain_cell.as_str()), w);
        }

        // Degenerate inspector bounds and extra-wide viewport (> 128-char bar_cow owned path).
        let mut many_children_entry = Entry::new(
            "Get-Item",
            "Get-Item\x1b_apc_payload\x1b\\\u{202e}",
            EntryKind::Cmdlet,
            "日本語の説明文。\n\nSecond\u{2028}line\u{0085}here.",
        )
        .with_aliases(vec!["gi".to_string()]);
        many_children_entry.children = (0..15)
            .map(|i| {
                Entry::new(
                    format!("child{i}"),
                    format!("child{i}"),
                    EntryKind::Param,
                    "",
                )
            })
            .collect();

        for w3 in [0usize, 1, 2, 200] {
            for max_rows in [0usize, 1, 2, 20] {
                let rows = build_expanded_inspector_rows(Some(&many_children_entry), w3, max_rows);
                assert!(rows.len() <= max_rows);
            }
        }

        let pane1 = PaneList::new(vec![many_children_entry]);
        let empty = PaneList::default();
        for (vp, expect_exact_width) in [
            (Viewport::new(0, 0), false),
            (Viewport::new(1, 1), false),
            (
                Viewport::new(Viewport::MIN_WIDTH, Viewport::MIN_HEIGHT),
                true,
            ),
            (Viewport::new(280, 24), true),
        ] {
            for expanded in [false, true] {
                let raw = render_frame(&RenderState {
                    viewport: vp,
                    active_pane: Pane::First,
                    panes: [&pane1, &empty, &empty],
                    command_line: "Get-Item \u{202e}--evil\x1bPbad\x1b\\".to_string(),
                    active_param_set: Some("Default\x1b^pm\x1b\\"),
                    subcmd_chain: &[],
                    committed_tokens: &[],
                    dialect: ShellDialect::PowerShell,
                    inspector_expanded: expanded,
                    inspector_scroll: 0,
                    style: UiStyle::default(),
                });
                assert!(!raw.contains("apc_payload"));
                assert!(!raw.contains("\x1bPbad"));
                assert!(!raw.contains("\x1b^pm"));
                assert!(!raw.chars().any(is_bidi_control));
                if expect_exact_width {
                    let plain = strip_ansi(&raw);
                    let lines: Vec<&str> = plain.lines().collect();
                    assert_eq!(lines.len(), vp.height);
                    for line in lines {
                        assert_eq!(UnicodeWidthStr::width(line), vp.width);
                    }
                }
            }
        }
    }

    #[test]
    fn test_perf_render_frame_and_inspector_throughput() {
        let mut p1_entries: Vec<Entry> = (0..2000)
            .map(|i| {
                Entry::new(
                    format!("Get-CmdletItem{i:04}"),
                    format!("Get-CmdletItem{i:04}"),
                    EntryKind::Cmdlet,
                    "Synthetic Command Catalog entry for frame rendering throughput benchmark.",
                )
            })
            .collect();
        p1_entries[123] = Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "Gets the processes that are running on the local computer.",
        );
        let mut pane1 = PaneList::new(p1_entries);
        pane1.push_filter_char('g');
        pane1.push_filter_char('p');

        let mut p2_entries: Vec<Entry> = (0..200)
            .map(|i| {
                let mut e = Entry::new(
                    format!("-ParameterName{i:03}"),
                    format!("-ParameterName{i:03} (-P{i})"),
                    EntryKind::Param,
                    "Specifies a target configuration parameter.\n\nSupports multiple Parameter Sets and pipeline input.",
                )
                .with_aliases(vec![format!("P{i}"), format!("Alt{i}")])
                .with_parameter_sets(vec!["DefaultSet".to_string(), "ExtendedSet".to_string()]);
                e.children = (0..12)
                    .map(|j| Entry::new(format!("val{j}"), format!("val{j}"), EntryKind::Value, ""))
                    .collect();
                e
            })
            .collect();
        p2_entries.insert(
            0,
            Entry::new(
                "-Name",
                "-Name (-ProcessName)",
                EntryKind::Param,
                "Specifies one or more processes by process name.\n\nWildcard characters are permitted across local sessions.",
            )
            .with_aliases(vec!["ProcessName".to_string(), "PN".to_string()])
            .with_parameter_sets(vec!["Name".to_string(), "NameWithUserName".to_string()]),
        );
        let mut pane2 = PaneList::new(p2_entries);
        pane2.push_filter_char('n');

        let pane3 = PaneList::new(
            (0..50)
                .map(|i| {
                    Entry::new(
                        format!("value-{i}"),
                        format!("value-{i}"),
                        EntryKind::Value,
                        "Sample value",
                    )
                })
                .collect(),
        );

        let vp = Viewport::new(120, 34);
        const NORMAL_FRAMES: usize = 400;
        const EXPANDED_FRAMES: usize = 400;
        let total_frames = NORMAL_FRAMES + EXPANDED_FRAMES;

        let start = std::time::Instant::now();
        let mut total_bytes = 0usize;

        for expanded in [false, true] {
            let count = if expanded {
                EXPANDED_FRAMES
            } else {
                NORMAL_FRAMES
            };
            let st = RenderState {
                viewport: vp,
                active_pane: Pane::Second,
                panes: [&pane1, &pane2, &pane3],
                command_line: "Get-Process -Name explorer".to_string(),
                active_param_set: Some("Name"),
                subcmd_chain: &[],
                committed_tokens: &[],
                dialect: ShellDialect::PowerShell,
                inspector_expanded: expanded,
                inspector_scroll: 0,
                style: UiStyle::default(),
            };
            for i in 0..count {
                let frame = render_frame(std::hint::black_box(&st));
                total_bytes += std::hint::black_box(frame.len());
                if i == 0 {
                    assert_eq!(frame.lines().count(), vp.height);
                }
            }
        }

        let elapsed = start.elapsed();
        let secs = elapsed.as_secs_f64().max(1e-9);
        let fps = (total_frames as f64) / secs;
        let us_per_frame = (secs * 1_000_000.0) / (total_frames as f64);
        println!(
            "test_perf_render_frame_and_inspector_throughput: {total_frames} frames ({total_bytes} bytes) in {:.2} ms ({fps:.0} frames/s, {us_per_frame:.1} us/frame)",
            secs * 1000.0
        );
        assert!(total_bytes > 0);
        assert!(
            secs < 10.0,
            "render_frame throughput regressed severely: {secs:.2}s for {total_frames} frames"
        );
    }

    fn has_ansi_color_escape(frame: &str) -> bool {
        let bytes = frame.as_bytes();
        let mut i = 0usize;
        while i + 2 < bytes.len() {
            if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
                let start = i + 2;
                let mut end = start;
                while end < bytes.len() && !(b'@'..=b'~').contains(&bytes[end]) {
                    end += 1;
                }
                if end < bytes.len() && bytes[end] == b'm' {
                    let params = &frame[start..end];
                    for part in params.split(';') {
                        if let Ok(code) = part.parse::<u16>() {
                            if (30..=49).contains(&code) || (90..=107).contains(&code) {
                                return true;
                            }
                        }
                    }
                }
                i = end + 1;
                continue;
            }
            i += 1;
        }
        false
    }

    #[test]
    fn test_no_color_and_dumb_term_strip_ansi_colors_while_keeping_reverse_bold_selection() {
        let mut pane1 = PaneList::new(vec![
            Entry::new("git", "git", EntryKind::Cli, "Git VCS"),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Rust Cargo"),
        ]);
        pane1.push_filter_char('g');
        let pane2 = PaneList::new(vec![Entry::new(
            "commit",
            "commit",
            EntryKind::Subcmd,
            "Commit",
        )]);
        let empty = PaneList::default();

        let configured = UiConfig {
            theme: ThemePreset::Dark,
            icons: IconMode::Unicode,
            borders: BorderStyle::Rounded,
        };

        // Empty NO_COLOR keeps configured Dark theme colors
        let with_empty_no_color = configured.resolve_env(Some(""), Some("xterm-256color"));
        let colored_frame = render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::First,
            panes: [&pane1, &pane2, &empty],
            command_line: "git commit".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: with_empty_no_color,
        });
        assert!(has_ansi_color_escape(&colored_frame));

        for style in [
            configured.resolve_env(Some("1"), None),
            configured.resolve_env(None, Some("dumb")),
            UiStyle {
                theme: ThemePreset::None,
                ..UiStyle::default()
            },
        ] {
            for expanded in [false, true] {
                let raw = render_frame(&RenderState {
                    viewport: Viewport::new(100, 14),
                    active_pane: Pane::First,
                    panes: [&pane1, &pane2, &empty],
                    command_line: "git commit".to_string(),
                    active_param_set: None,
                    subcmd_chain: &[],
                    committed_tokens: &[],
                    dialect: ShellDialect::Posix,
                    inspector_expanded: expanded,
                    inspector_scroll: 0,
                    style,
                });
                assert!(
                    !has_ansi_color_escape(&raw),
                    "NO_COLOR/TERM=dumb/none emitted color SGR in expanded={expanded}: {raw:?}"
                );
                assert!(
                    !raw.contains("\x1b[3")
                        && !raw.contains("\x1b[4")
                        && !raw.contains("\x1b[9")
                        && !raw.contains("\x1b[10"),
                    "unexpected color-prefixed CSI in monochrome frame: {raw:?}"
                );
                assert!(
                    raw.contains("\x1b[0;7;1m") && raw.contains("\x1b[1m"),
                    "active selection must stay visually distinct via reverse/bold: {raw:?}"
                );

                let plain = strip_ansi(&raw);
                let lines: Vec<&str> = plain.lines().collect();
                assert_eq!(lines.len(), 14);
                for line in lines {
                    assert_eq!(UnicodeWidthStr::width(line), 100);
                }
            }
        }
    }

    #[test]
    fn test_all_theme_presets_and_border_styles_render_exact_frame_dimensions() {
        let pane1 = PaneList::new(vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")]);
        let pane2 = PaneList::new(vec![Entry::new(
            "branch",
            "branch",
            EntryKind::Subcmd,
            "List branches",
        )]);
        let pane3 = PaneList::new(vec![Entry::new(
            "main",
            "⎇ main (current branch)",
            EntryKind::Arg,
            "Branch",
        )]);

        let dark_frame = render_frame(&RenderState {
            viewport: Viewport::new(90, 14),
            active_pane: Pane::First,
            panes: [&pane1, &pane2, &pane3],
            command_line: "git branch".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle {
                theme: ThemePreset::Dark,
                ..UiStyle::default()
            },
        });
        let light_frame = render_frame(&RenderState {
            viewport: Viewport::new(90, 14),
            active_pane: Pane::First,
            panes: [&pane1, &pane2, &pane3],
            command_line: "git branch".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle {
                theme: ThemePreset::Light,
                ..UiStyle::default()
            },
        });
        let ansi16_frame = render_frame(&RenderState {
            viewport: Viewport::new(90, 14),
            active_pane: Pane::First,
            panes: [&pane1, &pane2, &pane3],
            command_line: "git branch".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle {
                theme: ThemePreset::Ansi16,
                ..UiStyle::default()
            },
        });

        assert_ne!(dark_frame, light_frame);
        assert_ne!(dark_frame, ansi16_frame);
        assert_ne!(light_frame, ansi16_frame);
        assert!(ansi16_frame.contains("\x1b[94;1m"));
        assert!(!ansi16_frame.contains("\x1b[38;5;"));
        assert!(!ansi16_frame.contains("\x1b[48;5;"));

        for borders in [BorderStyle::Rounded, BorderStyle::Plain, BorderStyle::Ascii] {
            for width in [Viewport::MIN_WIDTH, 110] {
                for expanded in [false, true] {
                    let raw = render_frame(&RenderState {
                        viewport: Viewport::new(width, 15),
                        active_pane: Pane::Third,
                        panes: [&pane1, &pane2, &pane3],
                        command_line: "git branch main".to_string(),
                        active_param_set: Some("Default"),
                        subcmd_chain: &[],
                        committed_tokens: &[],
                        dialect: ShellDialect::Posix,
                        inspector_expanded: expanded,
                        inspector_scroll: 0,
                        style: UiStyle {
                            theme: ThemePreset::Light,
                            icons: IconMode::Unicode,
                            borders,
                        },
                    });
                    let plain = strip_ansi(&raw);
                    let lines: Vec<&str> = plain.lines().collect();
                    assert_eq!(lines.len(), 15);
                    for line in &lines {
                        assert_eq!(UnicodeWidthStr::width(*line), width);
                    }
                    match borders {
                        BorderStyle::Rounded => {
                            assert!(lines[0].starts_with('╭') && lines[0].ends_with('╮'));
                            assert!(lines[14].starts_with('╰') && lines[14].ends_with('╯'));
                        }
                        BorderStyle::Plain => {
                            assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'));
                            assert!(lines[14].starts_with('└') && lines[14].ends_with('┘'));
                        }
                        BorderStyle::Ascii => {
                            assert!(lines[0].starts_with('+') && lines[0].ends_with('+'));
                            assert!(lines[14].starts_with('+') && lines[14].ends_with('+'));
                            assert!(lines[1].starts_with('|') && lines[1].ends_with('|'));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_ascii_icons_and_borders_replace_all_unicode_glyphs_with_exact_width() {
        let pane1 = PaneList::new(vec![
            Entry::new("./", "📁 ./ (Current Dir)", EntryKind::Path, ""),
            Entry::new("git", "git", EntryKind::Cli, "Git"),
        ]);
        let pane2 = PaneList::new(vec![Entry::new(
            "checkout",
            "🔀 checkout",
            EntryKind::Subcmd,
            "Switch branches",
        )]);
        let pane3 = PaneList::new(vec![
            Entry::new("val", "✏ Use \"main\"", EntryKind::Value, "Custom"),
            Entry::new("main", "⎇ main", EntryKind::Arg, "Branch"),
            Entry::placeholder("⟳ Loading git...", EntryKind::Loading, ""),
        ]);

        for width in [Viewport::MIN_WIDTH, 120] {
            for expanded in [false, true] {
                let raw = render_frame(&RenderState {
                    viewport: Viewport::new(width, 14),
                    active_pane: Pane::First,
                    panes: [&pane1, &pane2, &pane3],
                    command_line: "git checkout very-long-branch-name-that-overflows-narrow-view"
                        .to_string(),
                    active_param_set: None,
                    subcmd_chain: &[],
                    committed_tokens: &[],
                    dialect: ShellDialect::Posix,
                    inspector_expanded: expanded,
                    inspector_scroll: 0,
                    style: UiStyle {
                        theme: ThemePreset::None,
                        icons: IconMode::Ascii,
                        borders: BorderStyle::Ascii,
                    },
                });
                let plain = strip_ansi(&raw);
                assert!(
                    plain.chars().all(|c| c == '\n' || c.is_ascii()),
                    "ASCII icons+borders leaked non-ASCII char in width={width} expanded={expanded}:\n{plain}"
                );
                for forbidden in ['❯', '✏', '🔀', '⎇', 'ℹ', '⟳', '📁', '╭', '─', '│', '…', '█']
                {
                    assert!(!plain.contains(forbidden), "leaked glyph {forbidden:?}");
                }
                let lines: Vec<&str> = plain.lines().collect();
                assert_eq!(lines.len(), 14);
                for line in &lines {
                    assert_eq!(UnicodeWidthStr::width(*line), width);
                }
                if width == 120 {
                    assert!(plain.contains("> [] ./ (Current Dir)"));
                    assert!(plain.contains("<> checkout"));
                    assert!(plain.contains("i [] ./ (Current Dir)"));
                    assert!(plain.contains("[^/v|Ctrl+N/P] Move  [</>] Pane"));
                    if !expanded {
                        assert!(plain.contains("* Use \"main\""));
                        assert!(plain.contains("@ main"));
                        assert!(plain.contains("~ Loading git..."));
                    }
                }
            }
        }
    }

    #[test]
    fn test_pane2_header_renders_subcommand_breadcrumb_unicode_ascii_and_min_width() {
        let pane1 = PaneList::new(vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")]);
        let pane2 = PaneList::new(vec![
            Entry::new("add", "add", EntryKind::Subcmd, "Add remote"),
            Entry::new("--verbose", "--verbose", EntryKind::Flag, "Verbose"),
        ]);
        let empty = PaneList::default();
        let chain_one = vec!["remote".to_string()];
        let chain_two = vec!["remote".to_string(), "add".to_string()];

        let unicode_one = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "git remote add".to_string(),
            active_param_set: None,
            subcmd_chain: &chain_one,
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        assert!(unicode_one.contains("2. git › remote (2)"));

        let unicode_two = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(110, 14),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "git remote add".to_string(),
            active_param_set: None,
            subcmd_chain: &chain_two,
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        assert!(unicode_two.contains("2. git › remote › add (2)"));

        let ascii_one = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "git remote add".to_string(),
            active_param_set: None,
            subcmd_chain: &chain_one,
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle {
                theme: ThemePreset::None,
                icons: IconMode::Ascii,
                borders: BorderStyle::Ascii,
            },
        }));
        assert!(ascii_one.contains("2. git > remote (2)"));
        assert!(ascii_one.chars().all(|c| c == '\n' || c.is_ascii()));

        let min_frame = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(Viewport::MIN_WIDTH, 14),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "git remote add".to_string(),
            active_param_set: None,
            subcmd_chain: &chain_two,
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        let lines: Vec<&str> = min_frame.lines().collect();
        assert_eq!(lines.len(), 14);
        for line in lines {
            assert_eq!(UnicodeWidthStr::width(line), Viewport::MIN_WIDTH);
        }
    }

    #[test]
    fn test_committed_flag_and_param_markers_in_pane2_preserve_width_across_modes() {
        let pane1 = PaneList::new(vec![Entry::new("cmd", "cmd", EntryKind::Cli, "CLI")]);
        let pane2 = PaneList::new(vec![
            Entry::new("-v", "-v", EntryKind::Flag, "Verbose")
                .with_aliases(vec!["verbose".to_string()]),
            Entry::new("-Id", "-Id (-PID)", EntryKind::Param, "Id")
                .with_aliases(vec!["PID".to_string()]),
            Entry::new("--dry-run", "--dry-run", EntryKind::Flag, "Dry run"),
            Entry::new("status", "status", EntryKind::Subcmd, "Status"),
        ]);
        let empty = PaneList::default();
        let committed = vec![
            "--verbose".to_string(),
            "-PID:42".to_string(),
            "status".to_string(),
        ];

        let unicode_plain = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::Second,
            panes: [&pane1, &pane2, &empty],
            command_line: "cmd --verbose -PID:42".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &committed,
            dialect: ShellDialect::PowerShell,
            inspector_expanded: false,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        let v_line = unicode_plain.lines().find(|l| l.contains("-v")).unwrap();
        let id_line = unicode_plain.lines().find(|l| l.contains("-Id")).unwrap();
        let dry_line = unicode_plain
            .lines()
            .find(|l| l.contains("--dry-run"))
            .unwrap();
        let sub_line = unicode_plain
            .lines()
            .find(|l| l.contains("status"))
            .unwrap();
        assert!(
            v_line.contains('✓'),
            "committed Flag must show ✓: {v_line:?}"
        );
        assert!(
            id_line.contains('✓'),
            "committed Param via alias must show ✓: {id_line:?}"
        );
        assert!(
            !dry_line.contains('✓'),
            "uncommitted Flag must not show ✓: {dry_line:?}"
        );
        assert!(
            !sub_line.contains('✓'),
            "Subcmd must not show ✓: {sub_line:?}"
        );

        for width in [Viewport::MIN_WIDTH, 100] {
            let ascii_raw = render_frame(&RenderState {
                viewport: Viewport::new(width, 14),
                active_pane: Pane::Second,
                panes: [&pane1, &pane2, &empty],
                command_line: "cmd --verbose -PID:42".to_string(),
                active_param_set: None,
                subcmd_chain: &[],
                committed_tokens: &committed,
                dialect: ShellDialect::PowerShell,
                inspector_expanded: false,
                inspector_scroll: 0,
                style: UiStyle {
                    theme: ThemePreset::None,
                    icons: IconMode::Ascii,
                    borders: BorderStyle::Ascii,
                },
            });
            assert!(!has_ansi_color_escape(&ascii_raw));
            let ascii_plain = strip_ansi(&ascii_raw);
            assert!(ascii_plain.chars().all(|c| c == '\n' || c.is_ascii()));
            assert!(!ascii_plain.contains('✓'));
            let ascii_v_line = ascii_plain.lines().find(|l| l.contains("-v")).unwrap();
            assert!(
                ascii_v_line.contains('*'),
                "ASCII mode must show * marker on committed Flag: {ascii_v_line:?}"
            );
            for line in ascii_plain.lines() {
                assert_eq!(UnicodeWidthStr::width(line), width);
            }
        }
    }

    #[test]
    fn test_expanded_inspector_scroll_offset_clamps_and_renders_indicator() {
        let long_desc = (1..=25)
            .map(|i| format!("Detail line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        let pane1 = PaneList::new(vec![Entry::new("cmd", "cmd", EntryKind::Cli, long_desc)]);
        let empty = PaneList::default();

        // Height 14 -> list_rows = 8. Top of inspector shows lines 1-8 and down indicator.
        let top_plain = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::First,
            panes: [&pane1, &empty, &empty],
            command_line: "cmd".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
            inspector_scroll: 0,
            style: UiStyle::default(),
        }));
        assert_eq!(top_plain.lines().count(), 14);
        assert!(top_plain.contains("Detail line 01"));
        assert!(!top_plain.contains("Detail line 25"));
        assert!(top_plain.contains("↓ 1-8/"));

        // Mid scroll shows middle lines and both up/down indicator.
        let mid_plain = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::First,
            panes: [&pane1, &empty, &empty],
            command_line: "cmd".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
            inspector_scroll: 10,
            style: UiStyle::default(),
        }));
        assert_eq!(mid_plain.lines().count(), 14);
        assert!(!mid_plain.contains("Detail line 01"));
        assert!(mid_plain.contains("Detail line 10"));
        assert!(mid_plain.contains("↑↓ 11-18/"));

        // Oversized offset clamps cleanly to bottom and ASCII mode uses ASCII arrows.
        let bot_ascii = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(100, 14),
            active_pane: Pane::First,
            panes: [&pane1, &empty, &empty],
            command_line: "cmd".to_string(),
            active_param_set: None,
            subcmd_chain: &[],
            committed_tokens: &[],
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
            inspector_scroll: 999,
            style: UiStyle {
                theme: ThemePreset::None,
                icons: IconMode::Ascii,
                borders: BorderStyle::Ascii,
            },
        }));
        assert_eq!(bot_ascii.lines().count(), 14);
        assert!(bot_ascii.chars().all(|c| c == '\n' || c.is_ascii()));
        assert!(bot_ascii.contains("Detail line 25"));
        assert!(bot_ascii.contains("^ "));
    }
}
