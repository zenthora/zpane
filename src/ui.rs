use crate::fuzzy::{selected_item, FilteredItem};
use crate::model::{Column, ItemKind, MillerItem};
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

impl ItemKind {
    pub fn ansi_color(self) -> &'static str {
        match self {
            ItemKind::Env | ItemKind::Var => "\x1b[38;5;216m", // Peach
            ItemKind::Cli => "\x1b[38;5;114m",                 // Green
            ItemKind::Func | ItemKind::Alias => "\x1b[38;5;81m", // Cyan
            ItemKind::Cmdlet => "\x1b[38;5;75m",               // Blue
            ItemKind::Subcmd => "\x1b[38;5;141m",              // Purple
            ItemKind::Flag | ItemKind::Param => "\x1b[38;5;223m", // Soft yellow
            ItemKind::Switch => "\x1b[38;5;183m",              // Lavender
            ItemKind::Enum | ItemKind::Value => "\x1b[38;5;150m", // Mint green
            ItemKind::Arg => "\x1b[38;5;250m",                 // Light gray
            ItemKind::Loading => "\x1b[38;5;244m",             // Dim gray
        }
    }
}

/// Rows consumed by Column 3's inspector header (title, description, divider).
const COL3_INSPECTOR_ROWS: usize = 3;

/// Rows consumed by the outer frame: top border, divider, 3 status/footer bars, bottom border.
const FRAME_CHROME_ROWS: usize = 6;

/// Terminal dimensions shared by viewport geometry, scrolling, and rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub width: usize,
    pub height: usize,
}

impl Viewport {
    pub const fn new(width: usize, height: usize) -> Self {
        Self { width, height }
    }

    /// Clamps raw terminal dimensions `(w, h)` to usable frame bounds,
    /// falling back to `(fallback_w, fallback_h)` when either axis is below minimum.
    pub const fn from_terminal_size(w: u16, h: u16, fallback_w: usize, fallback_h: usize) -> Self {
        let width = if w >= 40 { w as usize } else { fallback_w }.saturating_sub(1);
        let height = if h >= 12 { h as usize } else { fallback_h }.saturating_sub(1);
        Self { width, height }
    }

    /// Returns a resized viewport, preserving the previous axis if the new dimension is below minimum.
    pub const fn resized(self, w: u16, h: u16) -> Self {
        let width = if w >= 40 {
            (w as usize).saturating_sub(1)
        } else {
            self.width
        };
        let height = if h >= 12 {
            (h as usize).saturating_sub(1)
        } else {
            self.height
        };
        Self { width, height }
    }

    /// Authoritative visible list-row count for `col`, accounting for frame chrome
    /// and Column 3's inspector header. Never exceeds the terminal height.
    pub fn rows(self, col: Column) -> usize {
        let list_rows = self.height.saturating_sub(FRAME_CHROME_ROWS);
        match col {
            Column::Third => list_rows.saturating_sub(COL3_INSPECTOR_ROWS),
            Column::First | Column::Second => list_rows,
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

/// Appends plain text truncated or space-padded to exact terminal display columns (`max_width`).
fn push_padded_text(out: &mut String, text: &str, max_width: usize) {
    if max_width == 0 {
        return;
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

    while cur_w < max_width {
        out.push(' ');
        cur_w += 1;
    }
}

/// Renders a Miller cell directly into `out` with fuzzy-match highlighting, selection styling,
/// and right-aligned `[Kind]` badge.
fn push_miller_cell(
    out: &mut String,
    item: &MillerItem,
    filtered: &FilteredItem,
    col_width: usize,
    is_selected: bool,
    is_active_col: bool,
) {
    let badge = item.kind.badge();
    let badge_w = badge.len();
    let ptr = if is_selected { "❯ " } else { "  " };
    let ptr_w = 2;

    // Reserve space for: ptr (2) + label + space (1) + badge
    let avail_label_w = col_width.saturating_sub(ptr_w + badge_w + 1).max(2);
    let match_positions = &filtered.match_positions;
    let mut match_cursor = 0usize;
    let base_style = if is_selected {
        if is_active_col {
            C_SEL_ACT_BG
        } else {
            C_SEL_INACT_BG
        }
    } else {
        C_TEXT
    };

    out.push_str(base_style);
    out.push_str(ptr);

    let mut cur_w = 0usize;
    for (idx, raw_ch) in item.label.chars().enumerate() {
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

    while cur_w < avail_label_w {
        out.push(' ');
        cur_w += 1;
    }

    out.push(' ');
    if !is_selected {
        out.push_str(item.kind.ansi_color());
    }
    out.push_str(badge);
    out.push_str(RST);
}

#[allow(clippy::too_many_arguments)]
fn push_column_cell(
    out: &mut String,
    items: &[MillerItem],
    filtered: &[FilteredItem],
    idx: usize,
    sel_idx: usize,
    col_width: usize,
    is_active_col: bool,
) {
    if let Some(f) = filtered.get(idx) {
        push_miller_cell(
            out,
            &items[f.item_idx],
            f,
            col_width,
            idx == sel_idx,
            is_active_col,
        );
    } else {
        for _ in 0..col_width {
            out.push(' ');
        }
    }
}

pub struct RenderState<'a> {
    pub viewport: Viewport,
    pub active_col: Column,
    pub filters: &'a [String; 3],
    pub sel_idx: &'a [usize; 3],
    pub scroll_idx: &'a [usize; 3],
    pub col1_items: &'a [MillerItem],
    pub col1_filtered: &'a [FilteredItem],
    pub col2_items: &'a [MillerItem],
    pub col2_filtered: &'a [FilteredItem],
    pub col3_items: &'a [MillerItem],
    pub col3_filtered: &'a [FilteredItem],
    pub cmd_preview: &'a str,
    pub active_param_set: Option<&'a str>,
}

/// Resolves the currently inspected item from the active column, falling back to parent columns
/// when the active column has no selection.
pub fn selected_info_item<'a>(st: &RenderState<'a>) -> Option<&'a MillerItem> {
    let cur_c1 = selected_item(st.col1_filtered, st.col1_items, st.sel_idx[0]);
    let cur_c2 = selected_item(st.col2_filtered, st.col2_items, st.sel_idx[1]);
    let cur_c3 = selected_item(st.col3_filtered, st.col3_items, st.sel_idx[2]);

    match st.active_col {
        Column::Third => cur_c3.or(cur_c2).or(cur_c1),
        Column::Second => cur_c2.or(cur_c1),
        Column::First => cur_c1,
    }
}

/// Derives the Column 3 inspector header `(info_title, info_desc)` from `RenderState`.
pub fn inspector_header(st: &RenderState<'_>) -> (String, String) {
    let info_item = selected_info_item(st);
    let info_title = info_item
        .map(|it| format!(" ℹ {} {}", it.label, it.kind.badge()))
        .unwrap_or_else(|| " ℹ No selection".to_string());
    let info_desc = info_item
        .map(|it| format!("   {}", it.desc))
        .unwrap_or_default();
    (info_title, info_desc)
}

pub fn render_frame(st: &RenderState<'_>) -> String {
    let win_w = st.viewport.width.max(76);
    let list_rows = st.viewport.rows(Column::First); // 1 top border + list_rows + 1 sep + 3 status/footer + 1 bottom border
    let (info_title, info_desc) = inspector_header(st);

    let inner_w = win_w.saturating_sub(4);
    let w1 = (inner_w * 28 / 100).max(20);
    let w2 = (inner_w * 32 / 100).max(22);
    let w3 = inner_w.saturating_sub(w1 + w2).max(22);

    let mut sb = String::with_capacity(32768);
    sb.push_str("\x1b[?2026h\x1b[H"); // Begin synchronized update + Cursor to home (0,0) for flicker-free overwrite

    // Top Border with Column Headers
    let h1_txt = format!(" 1. Command / Scope ({}) ", st.col1_filtered.len());
    let h2_txt = if let Some(set_name) = st.active_param_set {
        format!(" 2. Params [Set: {set_name}] ({}) ", st.col2_filtered.len())
    } else {
        format!(" 2. Subcommands / Params ({}) ", st.col2_filtered.len())
    };
    let h3_txt = format!(" 3. Flags / Values / Info ({}) ", st.col3_filtered.len());

    let h1_c = if st.active_col == Column::First {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };
    let h2_c = if st.active_col == Column::Second {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };
    let h3_c = if st.active_col == Column::Third {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };

    let b1 = "─".repeat(w1.saturating_sub(UnicodeWidthStr::width(h1_txt.as_str())));
    let b2 = "─".repeat(w2.saturating_sub(UnicodeWidthStr::width(h2_txt.as_str())));
    let b3 = "─".repeat(w3.saturating_sub(UnicodeWidthStr::width(h3_txt.as_str())));

    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}╭{h1_c}{h1_txt}{C_BORDER_DIM}{b1}┬{h2_c}{h2_txt}{C_BORDER_DIM}{b2}┬{h3_c}{h3_txt}{C_BORDER_DIM}{b3}╮{RST}\r"
    );

    for r in 0..list_rows {
        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        push_column_cell(
            &mut sb,
            st.col1_items,
            st.col1_filtered,
            st.scroll_idx[0] + r,
            st.sel_idx[0],
            w1,
            st.active_col == Column::First,
        );

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        push_column_cell(
            &mut sb,
            st.col2_items,
            st.col2_filtered,
            st.scroll_idx[1] + r,
            st.sel_idx[1],
            w2,
            st.active_col == Column::Second,
        );

        sb.push_str(C_BORDER_DIM);
        sb.push('│');
        sb.push_str(RST);
        // Column 3: Top 3 rows = Inspector Header, Rows 3.. = Interactive List
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
            _ => push_column_cell(
                &mut sb,
                st.col3_items,
                st.col3_filtered,
                st.scroll_idx[2] + (r - COL3_INSPECTOR_ROWS),
                st.sel_idx[2],
                w3,
                st.active_col == Column::Third,
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
        " Filter (Col {}): {}█   [Col1: '{}' | Col2: '{}' | Col3: '{}']",
        st.active_col.index() + 1,
        st.filters[st.active_col.index()],
        st.filters[0],
        st.filters[1],
        st.filters[2]
    );
    push_status_row(&mut sb, C_FILTER, &filter_line, full_inner);

    let cmd_line = format!(" Command Line  : {}", st.cmd_preview);
    push_status_row(&mut sb, C_CMD_BAR, &cmd_line, full_inner);

    let footer = " [↑/↓] Move  [←/→] Column  [Type] Fuzzy Filter  [Tab] Append Token  [Ctrl+U] Clear Filter  [Ctrl+R] Sync PS  [Enter] Accept  [Esc] Exit";
    push_status_row(&mut sb, C_DIM, footer, full_inner);

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
    use crate::fuzzy::filter_items;

    #[test]
    fn test_viewport_rows_standard_and_small_terminals() {
        let standard = Viewport::new(120, 30);
        // Standard 30-row terminal: 30 - 6 = 24 list rows in Col 1/2, 21 in Col 3
        assert_eq!(standard.rows(Column::First), 24);
        assert_eq!(standard.rows(Column::Second), 24);
        assert_eq!(standard.rows(Column::Third), 21);

        // Compact terminal (term_h < 16): e.g. 12 rows -> 6 list rows in Col 1/2, 3 in Col 3
        let compact = Viewport::new(90, 12);
        assert_eq!(compact.rows(Column::First), 6);
        assert_eq!(compact.rows(Column::Second), 6);
        assert_eq!(compact.rows(Column::Third), 3);

        // Below 10 rows the viewport must shrink rather than floor at 4 list rows.
        let tiny = Viewport::new(80, 8);
        assert_eq!(tiny.rows(Column::First), 2);
        assert_eq!(tiny.rows(Column::Second), 2);
        assert_eq!(tiny.rows(Column::Third), 0);
        assert_eq!(tiny.height - FRAME_CHROME_ROWS, tiny.rows(Column::First));
    }

    #[test]
    fn test_tiny_terminal_frame_never_exceeds_height() {
        let filters = [String::new(), String::new(), String::new()];
        let sel_idx = [0, 0, 0];
        let scroll_idx = [0, 0, 0];
        let st = RenderState {
            viewport: Viewport::new(80, 8),
            active_col: Column::First,
            filters: &filters,
            sel_idx: &sel_idx,
            scroll_idx: &scroll_idx,
            col1_items: &[],
            col1_filtered: &[],
            col2_items: &[],
            col2_filtered: &[],
            col3_items: &[],
            col3_filtered: &[],
            cmd_preview: "",
            active_param_set: None,
        };

        assert_eq!(render_frame(&st).lines().count(), 8);
    }

    #[test]
    fn test_small_terminal_scroll_alignment() {
        let col1_items: Vec<MillerItem> = (0..10)
            .map(|i| MillerItem::new(format!("cmd{i}"), format!("cmd{i}"), ItemKind::Cli, ""))
            .collect();
        let col1_filtered = filter_items(&col1_items, "");

        let col3_items: Vec<MillerItem> = (0..10)
            .map(|i| {
                MillerItem::new(
                    format!("--flag{i}"),
                    format!("--flag{i}"),
                    ItemKind::Flag,
                    "",
                )
            })
            .collect();
        let col3_filtered = filter_items(&col3_items, "");

        let term_h = 12; // < 16 rows
        let viewport = Viewport::new(100, term_h);
        let max_r_c1 = viewport.rows(Column::First); // 6
        let max_r_c3 = viewport.rows(Column::Third); // 3

        let sel_c1 = 7;
        let scroll_c1 = sel_c1 + 1 - max_r_c1; // 2 (visible indices 2..=7)
        let sel_c3 = 4;
        let scroll_c3 = sel_c3 + 1 - max_r_c3; // 2 (visible indices 2..=4)

        let filters = [String::new(), String::new(), String::new()];
        let sel_idx = [sel_c1, 0, sel_c3];
        let scroll_idx = [scroll_c1, 0, scroll_c3];

        let frame = render_frame(&RenderState {
            viewport,
            active_col: Column::Third,
            filters: &filters,
            sel_idx: &sel_idx,
            scroll_idx: &scroll_idx,
            col1_items: &col1_items,
            col1_filtered: &col1_filtered,
            col2_items: &[],
            col2_filtered: &[],
            col3_items: &col3_items,
            col3_filtered: &col3_filtered,
            cmd_preview: "cmd7 --flag4",
            active_param_set: None,
        });

        // Total rendered lines must equal term_h (6 list rows + 6 chrome lines = 12)
        assert_eq!(frame.lines().count(), term_h);
        // Bottom-most visible selections in both Col 1 and Col 3 must appear in the rendered frame
        assert!(frame.contains("cmd7"));
        assert!(!frame.contains("cmd1"));
        assert!(frame.contains("--flag4"));
        assert!(!frame.contains("--flag1"));
    }

    #[test]
    fn test_inspector_header_derivation() {
        let col1_items = vec![MillerItem::new(
            "git",
            "git",
            ItemKind::Cli,
            "Distributed version control",
        )];
        let col1_filtered = filter_items(&col1_items, "");

        let col2_items = vec![MillerItem::new(
            "commit",
            "commit",
            ItemKind::Subcmd,
            "Record changes to the repository",
        )];
        let col2_filtered = filter_items(&col2_items, "");

        let filters = [String::new(), String::new(), String::new()];
        let sel_idx = [0, 0, 0];
        let scroll_idx = [0, 0, 0];

        // Active Col 0 -> inspects col1 item
        let st_col0 = RenderState {
            viewport: Viewport::new(100, 20),
            active_col: Column::First,
            filters: &filters,
            sel_idx: &sel_idx,
            scroll_idx: &scroll_idx,
            col1_items: &col1_items,
            col1_filtered: &col1_filtered,
            col2_items: &col2_items,
            col2_filtered: &col2_filtered,
            col3_items: &[],
            col3_filtered: &[],
            cmd_preview: "git",
            active_param_set: None,
        };
        let (title0, desc0) = inspector_header(&st_col0);
        assert_eq!(title0, " ℹ git [CLI]");
        assert_eq!(desc0, "   Distributed version control");

        // Active Col 2 with empty Col 3 -> falls back to Col 2 selected item
        let st_col2_fallback = RenderState {
            active_col: Column::Third,
            ..st_col0
        };
        let (title2, desc2) = inspector_header(&st_col2_fallback);
        assert_eq!(title2, " ℹ commit [Subcmd]");
        assert_eq!(desc2, "   Record changes to the repository");
        let frame = render_frame(&st_col2_fallback);
        assert!(frame.contains("ℹ commit [Subcmd]"));
        assert!(frame.contains("Record changes to the repository"));

        // Empty selection across all columns -> " ℹ No selection"
        let st_empty = RenderState {
            col1_items: &[],
            col1_filtered: &[],
            col2_items: &[],
            col2_filtered: &[],
            ..st_col0
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
