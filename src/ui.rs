use crate::discovery::ShellDialect;
use crate::fuzzy::PaneList;
use crate::model::{Entry, EntryKind, Pane};
use std::fmt::Write as FmtWrite;
use unicode_width::UnicodeWidthChar;

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
        EntryKind::Env | EntryKind::Var => "\x1b[38;5;216m", // Peach
        EntryKind::Cli => "\x1b[38;5;114m",                  // Green
        EntryKind::Func | EntryKind::Alias => "\x1b[38;5;81m", // Cyan
        EntryKind::Cmdlet => "\x1b[38;5;75m",                // Blue
        EntryKind::Subcmd => "\x1b[38;5;141m",               // Purple
        EntryKind::Flag => "\x1b[38;5;183m",                 // Lavender
        EntryKind::Param => "\x1b[38;5;223m",                // Soft yellow
        EntryKind::Enum | EntryKind::Value => "\x1b[38;5;150m", // Mint green
        EntryKind::Arg => "\x1b[38;5;250m",                  // Light gray
        EntryKind::Loading => "\x1b[38;5;244m",              // Dim gray
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
fn sanitize_char(c: char) -> char {
    if matches!(c, '\r' | '\n' | '\t') {
        ' '
    } else {
        c
    }
}

/// Appends plain text truncated to at most `max_width` display columns, returning the written width.
fn push_truncated_text(out: &mut String, text: &str, max_width: usize) -> usize {
    if max_width == 0 {
        return 0;
    }
    let mut cur_w = 0usize;
    let mut last_w = 0usize;

    for raw_ch in text.chars() {
        let ch = sanitize_char(raw_ch);
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > max_width {
            if max_width > 1 {
                if cur_w == max_width {
                    out.pop();
                    cur_w = cur_w.saturating_sub(last_w);
                }
                if cur_w < max_width {
                    out.push('…');
                    cur_w += 1;
                }
            }
            break;
        }
        out.push(ch);
        cur_w += w;
        last_w = w;
    }

    cur_w
}

/// Appends plain text truncated or space-padded to exact terminal display columns (`max_width`).
fn push_padded_text(out: &mut String, text: &str, max_width: usize) {
    let cur_w = push_truncated_text(out, text, max_width);
    for _ in cur_w..max_width {
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
    let badge_w = badge.len();
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

    let mut match_cursor = 0usize;
    let mut cur_w = 0usize;
    for (idx, raw_ch) in entry.label.chars().enumerate() {
        let ch = sanitize_char(raw_ch);
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > avail_label_w {
            if avail_label_w > 1 && cur_w < avail_label_w {
                out.push('…');
                cur_w += 1;
            }
            break;
        }
        while match_cursor < match_positions.len() && match_positions[match_cursor] < idx {
            match_cursor += 1;
        }
        if match_cursor < match_positions.len() && match_positions[match_cursor] == idx {
            out.push_str(C_MATCH_HL);
            out.push(ch);
            out.push_str(base_style);
            match_cursor += 1;
        } else {
            out.push(ch);
        }
        cur_w += w;
    }

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
    let win_w = st.viewport.width.max(Viewport::MIN_WIDTH);
    let list_rows = st.viewport.rows(Pane::First);
    let (info_title, info_desc) = inspector_header(st);

    let inner_w = win_w.saturating_sub(4);
    let (w1, w2, w3) = split_pane_widths(inner_w);

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
    let h3_txt = format!(" 3. Flags / Values / Info ({}) ", st.panes[2].visible_len());

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
    sb.push_str("\r\n");

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

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        sb.push_str("\r\n");
    }

    // Divider above status bars
    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}├{}┴{}┴{}┤{RST}\r",
        "─".repeat(w1),
        "─".repeat(w2),
        "─".repeat(w3)
    );

    let full_inner = win_w.saturating_sub(2);

    let filter_line = format!(
        " Filter (Pane {}): {}█   [P1: '{}' | P2: '{}' | P3: '{}']",
        st.active_pane.index() + 1,
        st.panes[st.active_pane.index()].filter(),
        st.panes[0].filter(),
        st.panes[1].filter(),
        st.panes[2].filter()
    );
    push_status_row(&mut sb, C_FILTER, &filter_line, full_inner);

    let cmd_line = format!(" Command Line  : {}", st.command_line);
    push_status_row(&mut sb, C_CMD_BAR, &cmd_line, full_inner);

    let reload_action = if st.dialect.is_powershell() {
        "Sync PS"
    } else {
        "Reload"
    };
    let footer = format!(
        " [↑/↓] Move  [←/→] Pane  [Type] Fuzzy Filter  [Tab] Append Token  [Ctrl+U] Clear Filter  [Ctrl+R] {reload_action}  [Enter] Accept  [Esc] Exit"
    );
    push_status_row(&mut sb, C_DIM, &footer, full_inner);

    let _ = write!(
        sb,
        "{C_BORDER_DIM}╰{}╯{RST}\x1b[?2026l",
        "─".repeat(full_inner)
    );

    sb
}

fn push_status_row(out: &mut String, color: &str, text: &str, inner_width: usize) {
    out.push_str(C_BORDER_DIM);
    out.push('│');
    out.push_str(RST);
    out.push_str(color);
    push_padded_text(out, text, inner_width);
    out.push_str(RST);
    out.push_str(C_BORDER_DIM);
    out.push('│');
    out.push_str(RST);
    out.push_str("\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;
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
            dialect: ShellDialect::Posix,
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
        }));
        assert!(ps_frame.contains("[Ctrl+R] Sync PS"));

        let posix_frame = strip_ansi(&render_frame(&RenderState {
            viewport: Viewport::new(150, 14),
            active_pane: Pane::First,
            panes: [&empty, &empty, &empty],
            command_line: String::new(),
            active_param_set: None,
            dialect: ShellDialect::Posix,
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
}
