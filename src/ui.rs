use crate::discovery::ShellDialect;
use crate::fuzzy::PaneList;
use crate::model::{Entry, EntryKind, Pane};
use std::fmt::Write as FmtWrite;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const C_BORDER_DIM: &str = "\x1b[38;5;240m";
const C_HEADER_ACT: &str = "\x1b[38;5;75;1m";
const C_HEADER_DIM: &str = "\x1b[38;5;246m";
const C_SEL_ACT_BG: &str = "\x1b[48;5;236;38;5;255;1m";
const C_SEL_INACT_BG: &str = "\x1b[48;5;234;38;5;250m";
const C_MATCH_HL: &str = "\x1b[38;5;226;1m";
const C_TEXT: &str = "\x1b[38;5;252m";
const C_DIM: &str = "\x1b[38;5;244m";
const C_CMD_BAR: &str = "\x1b[38;5;114;1m";
const C_FILTER: &str = "\x1b[38;5;223;1m";
const RST: &str = "\x1b[0m";

fn kind_color(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::History => "\x1b[38;5;221m", // Warm gold
        EntryKind::Path => "\x1b[38;5;116m",    // Soft teal
        EntryKind::Env | EntryKind::Var => "\x1b[38;5;216m", // Peach
        EntryKind::Cli => "\x1b[38;5;114m",     // Green
        EntryKind::Func | EntryKind::Alias => "\x1b[38;5;81m", // Cyan
        EntryKind::Cmdlet => "\x1b[38;5;75m",   // Blue
        EntryKind::Subcmd => "\x1b[38;5;141m",  // Purple
        EntryKind::Flag => "\x1b[38;5;183m",    // Lavender
        EntryKind::Param => "\x1b[38;5;223m",   // Soft yellow
        EntryKind::Enum | EntryKind::Value => "\x1b[38;5;150m", // Mint green
        EntryKind::Arg => "\x1b[38;5;250m",     // Light gray
        EntryKind::Loading => "\x1b[38;5;244m", // Dim gray
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

#[inline]
fn sanitize_char(c: char) -> Option<char> {
    if matches!(c, '\r' | '\n' | '\t') {
        Some(' ')
    } else if c.is_control() {
        None
    } else {
        Some(c)
    }
}

fn display_width_sanitized(s: &str) -> usize {
    s.chars()
        .filter_map(sanitize_char)
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

/// Appends sanitized text truncated to at most `max_width` display columns, optionally
/// highlighting `match_positions` on top of `base_style`, and returns the written width.
fn push_sanitized_truncated(
    out: &mut String,
    text: &str,
    max_width: usize,
    match_positions: &[usize],
    base_style: Option<&str>,
) -> usize {
    if max_width == 0 {
        return 0;
    }
    let mut cur_w = 0usize;
    let mut last_non_zero_start = out.len();
    let mut last_non_zero_base_w = 0usize;
    let mut match_cursor = 0usize;

    for (idx, raw_ch) in text.chars().enumerate() {
        let Some(ch) = sanitize_char(raw_ch) else {
            continue;
        };
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w == 0 && cur_w == 0 {
            continue;
        }
        if cur_w + w > max_width {
            if max_width > 1 {
                if cur_w == max_width {
                    out.truncate(last_non_zero_start);
                    cur_w = last_non_zero_base_w;
                }
                if cur_w < max_width {
                    out.push('…');
                    cur_w += 1;
                }
            }
            break;
        }
        if w > 0 {
            last_non_zero_start = out.len();
            last_non_zero_base_w = cur_w;
        }
        if let Some(base) = base_style {
            while match_cursor < match_positions.len() && match_positions[match_cursor] < idx {
                match_cursor += 1;
            }
            if match_cursor < match_positions.len() && match_positions[match_cursor] == idx {
                out.push_str(C_MATCH_HL);
                out.push(ch);
                out.push_str(base);
                match_cursor += 1;
            } else {
                out.push(ch);
            }
        } else {
            out.push(ch);
        }
        cur_w += w;
    }

    cur_w
}

/// Appends plain text truncated to at most `max_width` display columns, returning the written width.
fn push_truncated_text(out: &mut String, text: &str, max_width: usize) -> usize {
    push_sanitized_truncated(out, text, max_width, &[], None)
}

/// Appends plain text truncated or space-padded to exact terminal display columns (`max_width`).
fn push_padded_text(out: &mut String, text: &str, max_width: usize) {
    let cur_w = push_truncated_text(out, text, max_width);
    for _ in cur_w..max_width {
        out.push(' ');
    }
}

/// Appends `prefix` + `body` padded to `max_width`, tail-scrolling `body` with a leading `'…'`
/// when it overflows so the active tail remains visible.
fn push_tail_truncated_padded(out: &mut String, prefix: &str, body: &str, max_width: usize) {
    let prefix_w = display_width_sanitized(prefix);
    let clean_body: String = body.chars().filter_map(sanitize_char).collect();
    let body_w = display_width_sanitized(&clean_body);

    if prefix_w + body_w <= max_width || max_width <= prefix_w + 1 {
        let mut combined = String::with_capacity(prefix.len() + clean_body.len());
        combined.push_str(prefix);
        combined.push_str(&clean_body);
        push_padded_text(out, &combined, max_width);
        return;
    }

    let target_w = max_width - prefix_w - 1;
    let mut w_so_far = 0usize;
    let mut suffix_start = clean_body.len();
    for (idx, ch) in clean_body.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if w > 0 && body_w - w_so_far <= target_w {
            suffix_start = idx;
            break;
        }
        w_so_far += w;
    }

    let mut written = push_truncated_text(out, prefix, max_width);
    if written < max_width {
        out.push('…');
        written += 1;
    }
    written += push_truncated_text(
        out,
        &clean_body[suffix_start..],
        max_width.saturating_sub(written),
    );
    for _ in written..max_width {
        out.push(' ');
    }
}

/// Appends a pane header title bounded to `col_width`, filling remaining width with `'─'`.
fn push_pane_header(out: &mut String, color: &str, title: &str, col_width: usize) {
    out.push_str(color);
    let written = push_truncated_text(out, title, col_width);
    out.push_str(C_BORDER_DIM);
    for _ in written..col_width {
        out.push('─');
    }
}

/// Renders a Miller cell directly into `out` strictly bounded to `col_width` display columns,
/// with fuzzy-match highlighting, selection styling, and right-aligned `[Kind]` badge when space permits.
fn push_miller_cell(
    out: &mut String,
    entry: &Entry,
    match_positions: &[usize],
    col_width: usize,
    is_selected: bool,
    is_active_pane: bool,
) {
    if col_width == 0 {
        return;
    }
    let badge = entry.kind.badge();
    let badge_w = UnicodeWidthStr::width(badge);
    let ptr = if is_selected { "❯ " } else { "  " };
    let ptr_w = if col_width >= 2 { 2 } else { 0 };

    let base_style = if is_selected {
        if is_active_pane {
            C_SEL_ACT_BG
        } else {
            C_SEL_INACT_BG
        }
    } else {
        C_TEXT
    };

    out.push_str(base_style);
    if ptr_w == 2 {
        out.push_str(ptr);
    }

    let rem = col_width.saturating_sub(ptr_w);
    let show_badge = rem >= badge_w + 3;
    let avail_label_w = if show_badge { rem - badge_w - 1 } else { rem };

    let cur_w = push_sanitized_truncated(
        out,
        &entry.label,
        avail_label_w,
        match_positions,
        Some(base_style),
    );

    for _ in cur_w..avail_label_w {
        out.push(' ');
    }

    if show_badge {
        out.push(' ');
        if !is_selected {
            out.push_str(kind_color(entry.kind));
        }
        out.push_str(badge);
    }
    out.push_str(RST);
}

fn push_pane_cell(
    out: &mut String,
    pane: &PaneList,
    row_offset: usize,
    col_width: usize,
    is_active_pane: bool,
) {
    let idx = pane.scroll_idx() + row_offset;
    if let Some((entry, match_positions)) = pane.row_at(idx) {
        push_miller_cell(
            out,
            entry,
            match_positions,
            col_width,
            idx == pane.sel_idx(),
            is_active_pane,
        );
    } else {
        for _ in 0..col_width {
            out.push(' ');
        }
    }
}

/// Read-only view of session state passed into [`render_frame`] and test assertions.
pub struct RenderState<'a> {
    pub viewport: Viewport,
    pub active_pane: Pane,
    pub panes: [&'a PaneList; 3],
    pub command_line: String,
    pub active_param_set: Option<&'a str>,
    pub dialect: ShellDialect,
    pub inspector_expanded: bool,
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
fn inspector_header(st: &RenderState<'_>) -> (String, String) {
    let info_entry = selected_info_entry(st);
    let info_title = info_entry
        .map(|it| format!(" ℹ {} {}", it.label, it.kind.badge()))
        .unwrap_or_else(|| " ℹ No selection".to_string());
    let info_desc = info_entry
        .map(|it| format!("   {}", it.desc))
        .unwrap_or_default();
    (info_title, info_desc)
}

fn wrap_display_lines(text: &str, max_width: usize) -> Vec<String> {
    if max_width == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for raw_line in text.lines() {
        let mut cur = String::new();
        let mut cur_w = 0usize;
        for word in raw_line.split_whitespace() {
            let clean_word: String = word.chars().filter_map(sanitize_char).collect();
            if clean_word.is_empty() {
                continue;
            }
            let word_w: usize = clean_word
                .chars()
                .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
                .sum();
            let space_w = usize::from(!cur.is_empty());
            if cur_w + space_w + word_w <= max_width {
                if space_w == 1 {
                    cur.push(' ');
                    cur_w += 1;
                }
                cur.push_str(&clean_word);
                cur_w += word_w;
            } else {
                if !cur.is_empty() {
                    lines.push(std::mem::take(&mut cur));
                    cur_w = 0;
                }
                if word_w <= max_width {
                    cur.push_str(&clean_word);
                    cur_w = word_w;
                } else {
                    for ch in clean_word.chars() {
                        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if cw > max_width {
                            continue;
                        }
                        if cur_w + cw > max_width && !cur.is_empty() {
                            lines.push(std::mem::take(&mut cur));
                            cur_w = 0;
                        }
                        if cw == 0 && cur.is_empty() {
                            continue;
                        }
                        cur.push(ch);
                        cur_w += cw;
                    }
                }
            }
        }
        if !cur.is_empty() {
            lines.push(cur);
        } else {
            lines.push(String::new());
        }
    }
    lines
}

fn push_inspector_field(
    rows: &mut Vec<(&'static str, String)>,
    color: &'static str,
    raw: &str,
    w3: usize,
    wrap_w: usize,
) {
    let single = format!(" {raw}");
    if display_width_sanitized(&single) <= w3 {
        rows.push((color, single));
    } else {
        for line in wrap_display_lines(raw, wrap_w) {
            rows.push((color, format!(" {line}")));
        }
    }
}

fn build_expanded_inspector_rows(entry: Option<&Entry>, w3: usize) -> Vec<(&'static str, String)> {
    let mut rows = Vec::new();
    let Some(it) = entry else {
        let msg = if w3 >= 26 {
            " ℹ No selection [F1 Close]"
        } else {
            " ℹ No selection"
        };
        rows.push((C_HEADER_ACT, msg.to_string()));
        return rows;
    };

    let full_hdr = format!(" ℹ {} {} [F1 Close]", it.label, it.kind.badge());
    let hdr = if display_width_sanitized(&full_hdr) <= w3 {
        full_hdr
    } else {
        format!(" ℹ {} {}", it.label, it.kind.badge())
    };
    rows.push((C_HEADER_ACT, hdr));

    let wrap_w = w3.saturating_sub(2).max(1);
    let tok_val = if it.text.is_empty() {
        "(placeholder)"
    } else {
        it.text.as_str()
    };
    if it.aliases.is_empty() {
        push_inspector_field(
            &mut rows,
            C_FILTER,
            &format!("Token: {tok_val}"),
            w3,
            wrap_w,
        );
    } else {
        let aliases = it.aliases.join(", ");
        let single = format!(" Token: {tok_val}  Aliases: {aliases}");
        if display_width_sanitized(&single) <= w3 {
            rows.push((C_FILTER, single));
        } else {
            push_inspector_field(
                &mut rows,
                C_FILTER,
                &format!("Token: {tok_val}"),
                w3,
                wrap_w,
            );
            push_inspector_field(
                &mut rows,
                C_FILTER,
                &format!("Aliases: {aliases}"),
                w3,
                wrap_w,
            );
        }
    }

    let sets_raw = if !it.parameter_sets.is_empty() {
        format!("Sets : {}", it.parameter_sets.join(", "))
    } else {
        format!("Kind : {}", it.kind.badge())
    };
    push_inspector_field(&mut rows, C_DIM, &sets_raw, w3, wrap_w);
    rows.push((C_BORDER_DIM, "─".repeat(w3)));

    for line in wrap_display_lines(&it.desc, wrap_w) {
        rows.push((C_TEXT, format!(" {line}")));
    }

    if !it.children.is_empty() {
        let all_children: Vec<&str> = it
            .children
            .iter()
            .filter(|c| !c.is_placeholder() && !c.text.is_empty())
            .map(|c| c.text.as_str())
            .collect();
        if !all_children.is_empty() {
            rows.push((C_BORDER_DIM, "─".repeat(w3)));
            let shown = all_children.len().min(8);
            let mut summary = format!("Children: {}", all_children[..shown].join(", "));
            if all_children.len() > shown {
                summary.push_str(&format!(", … (+{} more)", all_children.len() - shown));
            }
            for line in wrap_display_lines(&summary, wrap_w) {
                rows.push((C_DIM, format!(" {line}")));
            }
        }
    }

    rows
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

pub fn render_frame(st: &RenderState<'_>) -> String {
    let win_w = st.viewport.width.max(Viewport::MIN_WIDTH.saturating_sub(1));
    let list_rows = st.viewport.rows(Pane::First);
    let inner_w = win_w.saturating_sub(4);
    let (w1, w2, w3) = split_pane_widths(inner_w);
    let (info_title, info_desc) = inspector_header(st);
    let expanded_rows = if st.inspector_expanded {
        build_expanded_inspector_rows(selected_info_entry(st), w3)
    } else {
        Vec::new()
    };

    let mut sb = String::with_capacity(32768);
    sb.push_str("\x1b[?2026h\x1b[H"); // Begin synchronized update + Cursor to home (0,0) for flicker-free overwrite

    let h1_txt = format!(" 1. Command / Scope ({}) ", st.panes[0].visible_len());
    let h2_txt = if let Some(set_name) = st.active_param_set {
        format!(
            " 2. Params [Set: {set_name}] ({}) ",
            st.panes[1].visible_len()
        )
    } else {
        format!(" 2. Subcommands / Params ({}) ", st.panes[1].visible_len())
    };
    let h3_txt = if st.inspector_expanded {
        " 3. Inspector [F1 Close] ".to_string()
    } else {
        format!(" 3. Flags / Values / Info ({}) ", st.panes[2].visible_len())
    };

    let header_color = |pane: Pane| {
        if st.active_pane == pane {
            C_HEADER_ACT
        } else {
            C_HEADER_DIM
        }
    };

    sb.push_str(C_BORDER_DIM);
    sb.push('╭');
    push_pane_header(&mut sb, header_color(Pane::First), &h1_txt, w1);
    sb.push('┬');
    push_pane_header(&mut sb, header_color(Pane::Second), &h2_txt, w2);
    sb.push('┬');
    push_pane_header(&mut sb, header_color(Pane::Third), &h3_txt, w3);
    sb.push('╮');
    sb.push_str(RST);
    sb.push_str("\x1b[K\r\n");

    for r in 0..list_rows {
        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        push_pane_cell(&mut sb, st.panes[0], r, w1, st.active_pane == Pane::First);

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        push_pane_cell(&mut sb, st.panes[1], r, w2, st.active_pane == Pane::Second);

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        if st.inspector_expanded {
            if let Some((color, text)) = expanded_rows.get(r) {
                sb.push_str(color);
                push_padded_text(&mut sb, text, w3);
                sb.push_str(RST);
            } else {
                for _ in 0..w3 {
                    sb.push(' ');
                }
            }
        } else {
            // Pane 3: Top 3 rows = Inspector Header, Rows 3.. = Interactive List
            match r {
                0 => {
                    sb.push_str(C_HEADER_ACT);
                    push_padded_text(&mut sb, &info_title, w3);
                    sb.push_str(RST);
                }
                1 => {
                    sb.push_str(C_DIM);
                    push_padded_text(&mut sb, &info_desc, w3);
                    sb.push_str(RST);
                }
                2 => {
                    sb.push_str(C_BORDER_DIM);
                    for _ in 0..w3 {
                        sb.push('─');
                    }
                    sb.push_str(RST);
                }
                _ => push_pane_cell(
                    &mut sb,
                    st.panes[2],
                    r - PANE3_INSPECTOR_ROWS,
                    w3,
                    st.active_pane == Pane::Third,
                ),
            }
        }

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        sb.push_str("\x1b[K\r\n");
    }

    // Divider above status bars
    let _ = write!(
        sb,
        "{C_BORDER_DIM}├{}┴{}┴{}┤{RST}\x1b[K\r\n",
        "─".repeat(w1),
        "─".repeat(w2),
        "─".repeat(w3)
    );

    let full_inner = win_w.saturating_sub(2);

    let active_filter = st.panes[st.active_pane.index()].filter();
    let filter_prefix = format!(" Filter (Pane {}): ", st.active_pane.index() + 1);
    let filter_active = format!("{active_filter}█");
    let filter_summary = format!(
        "   [P1: '{}' | P2: '{}' | P3: '{}']",
        st.panes[0].filter(),
        st.panes[1].filter(),
        st.panes[2].filter()
    );
    let filter_full = format!("{filter_prefix}{filter_active}{filter_summary}");
    if display_width_sanitized(&filter_full) <= full_inner
        || display_width_sanitized(&filter_prefix) + display_width_sanitized(&filter_active)
            <= full_inner
    {
        push_status_row(&mut sb, C_FILTER, &filter_full, full_inner);
    } else {
        push_tail_status_row(
            &mut sb,
            C_FILTER,
            &filter_prefix,
            &filter_active,
            full_inner,
        );
    }

    push_tail_status_row(
        &mut sb,
        C_CMD_BAR,
        " Command Line  : ",
        &st.command_line,
        full_inner,
    );

    let reload_action = if st.dialect.is_powershell() {
        "Sync PS"
    } else {
        "Reload"
    };
    let footer = format!(
        " [↑/↓|Ctrl+N/P] Move  [←/→] Pane  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [F1] Inspect  [Ctrl+R] {reload_action}  [Enter] Accept  [Esc]"
    );
    push_status_row(&mut sb, C_DIM, &footer, full_inner);

    let _ = write!(
        sb,
        "{C_BORDER_DIM}╰{}╯{RST}\x1b[K\x1b[J\x1b[?2026l",
        "─".repeat(full_inner)
    );

    sb
}

fn push_framed_status_row(out: &mut String, color: &str, write_inner: impl FnOnce(&mut String)) {
    out.push_str(C_BORDER_DIM);
    out.push('│');
    out.push_str(RST);
    out.push_str(color);
    write_inner(out);
    out.push_str(RST);
    out.push_str(C_BORDER_DIM);
    out.push('│');
    out.push_str(RST);
    out.push_str("\x1b[K\r\n");
}

fn push_status_row(out: &mut String, color: &str, text: &str, inner_width: usize) {
    push_framed_status_row(out, color, |buf| push_padded_text(buf, text, inner_width));
}

fn push_tail_status_row(
    out: &mut String,
    color: &str,
    prefix: &str,
    body: &str,
    inner_width: usize,
) {
    push_framed_status_row(out, color, |buf| {
        push_tail_truncated_padded(buf, prefix, body, inner_width);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
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
                dialect: ShellDialect::Posix,
                inspector_expanded: false,
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
            dialect: ShellDialect::PowerShell,
            inspector_expanded: false,
        }));
        assert!(ps_frame.contains("[Ctrl+R] Sync PS"));

        let posix_frame = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(150, 14),
            active_pane: Pane::First,
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            active_param_set: None,
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
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
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
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
            dialect: ShellDialect::Posix,
            inspector_expanded: false,
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
            dialect: ShellDialect::PowerShell,
            inspector_expanded: true,
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
        assert!(raw.contains(C_MATCH_HL));
        assert!(raw.ends_with(RST));

        // Exact-width wide-char label truncation keeps valid ANSI reset and width
        let wide_entry = Entry::new("中文abc", "中文abc", EntryKind::Arg, "");
        raw.clear();
        push_miller_cell(&mut raw, &wide_entry, &[1], 6, false, false);
        let wide_plain = strip_ansi(&raw);
        assert_eq!(UnicodeWidthStr::width(wide_plain.as_str()), 6);
        assert_eq!(wide_plain, "  中… ");
        assert!(raw.ends_with(RST));
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
            dialect: ShellDialect::Posix,
            inspector_expanded: true,
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
}
