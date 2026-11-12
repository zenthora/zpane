use crate::fuzzy::FilteredItem;
use crate::model::{ItemKind, MillerItem};
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

/// Authoritative visible row count for a given terminal height and Miller column (`0..=2`).
/// Accounts for the 6 rows of frame chrome (top border, divider, 3 status/footer bars, bottom border)
/// and Column 3's 3-row inspector header.
pub fn viewport_rows(term_h: usize, col: usize) -> usize {
    let list_rows = term_h.saturating_sub(6).max(4);
    if col == 2 {
        list_rows.saturating_sub(3).max(1)
    } else {
        list_rows
    }
}

pub fn clean_single_line(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\t') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Truncates or pads plain text to exact terminal display columns (`max_width`).
pub fn pad_or_truncate(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let clean = clean_single_line(text);
    let mut out = String::with_capacity(max_width + 8);
    let mut cur_w = 0;

    for ch in clean.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > max_width {
            if max_width > 1 && cur_w == max_width - 1 {
                out.push('…');
                cur_w += 1;
            } else if max_width > 1 && cur_w == max_width {
                out.pop();
                out.push('…');
            }
            break;
        }
        out.push(ch);
        cur_w += w;
    }

    while cur_w < max_width {
        out.push(' ');
        cur_w += 1;
    }
    out
}

/// Renders a Miller cell with fuzzy-match character highlighting, selection styling, and right-aligned `[Kind]` badge.
fn render_miller_cell(
    item: &MillerItem,
    filtered: &FilteredItem,
    col_width: usize,
    is_selected: bool,
    is_active_col: bool,
) -> String {
    let badge = item.kind.badge();
    let badge_w = badge.len();
    let ptr = if is_selected { "❯ " } else { "  " };
    let ptr_w = 2;

    // Reserve space for: ptr (2) + label + space (1) + badge
    let avail_label_w = col_width.saturating_sub(ptr_w + badge_w + 1).max(2);
    let clean_label = clean_single_line(&item.label);

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

    let mut out = String::with_capacity(col_width + 64);
    out.push_str(base_style);
    out.push_str(ptr);

    let mut cur_w = 0;
    for (idx, ch) in clean_label.chars().enumerate() {
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
    if is_selected {
        out.push_str(badge);
    } else {
        out.push_str(item.kind.ansi_color());
        out.push_str(badge);
    }
    out.push_str(RST);
    out
}

pub struct RenderState<'a> {
    pub term_w: usize,
    pub term_h: usize,
    pub active_col: usize,
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
    let cur_c1 = st
        .col1_filtered
        .get(st.sel_idx[0])
        .and_then(|f| st.col1_items.get(f.item_idx));
    let cur_c2 = st
        .col2_filtered
        .get(st.sel_idx[1])
        .and_then(|f| st.col2_items.get(f.item_idx));
    let cur_c3 = st
        .col3_filtered
        .get(st.sel_idx[2])
        .and_then(|f| st.col3_items.get(f.item_idx));

    match st.active_col {
        2 => cur_c3.or(cur_c2).or(cur_c1),
        1 => cur_c2.or(cur_c1),
        _ => cur_c1,
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
    let win_w = st.term_w.max(76);
    let list_rows = viewport_rows(st.term_h, 0); // 1 top border + list_rows + 1 sep + 3 status/footer + 1 bottom border
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

    let h1_c = if st.active_col == 0 {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };
    let h2_c = if st.active_col == 1 {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };
    let h3_c = if st.active_col == 2 {
        C_HEADER_ACT
    } else {
        C_HEADER_DIM
    };

    let b1 = "─".repeat(w1.saturating_sub(h1_txt.chars().count()));
    let b2 = "─".repeat(w2.saturating_sub(h2_txt.chars().count()));
    let b3 = "─".repeat(w3.saturating_sub(h3_txt.chars().count()));

    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}╭{h1_c}{h1_txt}{C_BORDER_DIM}{b1}┬{h2_c}{h2_txt}{C_BORDER_DIM}{b2}┬{h3_c}{h3_txt}{C_BORDER_DIM}{b3}╮{RST}\r"
    );

    let empty_c1 = " ".repeat(w1);
    let empty_c2 = " ".repeat(w2);
    let empty_c3 = " ".repeat(w3);

    for r in 0..list_rows {
        // --- Column 1 ---
        let idx1 = st.scroll_idx[0] + r;
        let cell1 = if let Some(f1) = st.col1_filtered.get(idx1) {
            let it1 = &st.col1_items[f1.item_idx];
            render_miller_cell(it1, f1, w1, idx1 == st.sel_idx[0], st.active_col == 0)
        } else {
            empty_c1.clone()
        };

        // --- Column 2 ---
        let idx2 = st.scroll_idx[1] + r;
        let cell2 = if let Some(f2) = st.col2_filtered.get(idx2) {
            let it2 = &st.col2_items[f2.item_idx];
            render_miller_cell(it2, f2, w2, idx2 == st.sel_idx[1], st.active_col == 1)
        } else {
            empty_c2.clone()
        };

        // --- Column 3 (Top 3 rows = Inspector Header, Rows 3.. = Interactive List) ---
        let cell3 = if r == 0 {
            let t = pad_or_truncate(&info_title, w3);
            format!("{C_HEADER_ACT}{t}{RST}")
        } else if r == 1 {
            let t = pad_or_truncate(&info_desc, w3);
            format!("{C_DIM}{t}{RST}")
        } else if r == 2 {
            format!("{C_BORDER_DIM}{}{RST}", "─".repeat(w3))
        } else {
            let idx3 = st.scroll_idx[2] + (r - 3);
            if let Some(f3) = st.col3_filtered.get(idx3) {
                let it3 = &st.col3_items[f3.item_idx];
                render_miller_cell(it3, f3, w3, idx3 == st.sel_idx[2], st.active_col == 2)
            } else {
                empty_c3.clone()
            }
        };

        let _ = writeln!(
            sb,
            "{C_BORDER_DIM}│{RST}{cell1}{C_BORDER_DIM}│{RST}{cell2}{C_BORDER_DIM}│{RST}{cell3}{C_BORDER_DIM}│{RST}\r"
        );
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

    // Filter Bar
    let filter_line = format!(
        " Filter (Col {}): {}█   [Col1: '{}' | Col2: '{}' | Col3: '{}']",
        st.active_col + 1,
        st.filters[st.active_col],
        st.filters[0],
        st.filters[1],
        st.filters[2]
    );
    let filter_pad = pad_or_truncate(&filter_line, full_inner);
    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}│{RST}{C_FILTER}{filter_pad}{RST}{C_BORDER_DIM}│{RST}\r"
    );

    // Command Builder Bar
    let cmd_line = format!(" Command Line  : {}", st.cmd_preview);
    let cmd_pad = pad_or_truncate(&cmd_line, full_inner);
    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}│{RST}{C_CMD_BAR}{cmd_pad}{RST}{C_BORDER_DIM}│{RST}\r"
    );

    // Footer Help Bar
    let footer = " [↑/↓] Move  [←/→] Column  [Type] Fuzzy Filter  [Tab] Append Token  [Ctrl+R] Sync PS  [Enter] Accept  [Esc] Exit";
    let footer_pad = pad_or_truncate(footer, full_inner);
    let _ = writeln!(
        sb,
        "{C_BORDER_DIM}│{RST}{C_DIM}{footer_pad}{RST}{C_BORDER_DIM}│{RST}\r"
    );

    let _ = write!(
        sb,
        "{C_BORDER_DIM}╰{}╯{RST}\x1b[?2026l",
        "─".repeat(full_inner)
    );

    sb
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuzzy::filter_items;

    #[test]
    fn test_viewport_rows_standard_and_small_terminals() {
        // Standard 30-row terminal: 30 - 6 = 24 list rows in Col 1/2, 21 in Col 3
        assert_eq!(viewport_rows(30, 0), 24);
        assert_eq!(viewport_rows(30, 1), 24);
        assert_eq!(viewport_rows(30, 2), 21);

        // Compact terminal (term_h < 16): e.g. 12 rows -> 6 list rows in Col 1/2, 3 in Col 3
        assert_eq!(viewport_rows(12, 0), 6);
        assert_eq!(viewport_rows(12, 1), 6);
        assert_eq!(viewport_rows(12, 2), 3);

        // Very small terminal clamping: minimum 4 list rows in Col 1/2, minimum 1 row in Col 3
        assert_eq!(viewport_rows(8, 0), 4);
        assert_eq!(viewport_rows(8, 1), 4);
        assert_eq!(viewport_rows(8, 2), 1);
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
        let max_r_c1 = viewport_rows(term_h, 0); // 6
        let max_r_c3 = viewport_rows(term_h, 2); // 3

        let sel_c1 = 7;
        let scroll_c1 = sel_c1 + 1 - max_r_c1; // 2 (visible indices 2..=7)
        let sel_c3 = 4;
        let scroll_c3 = sel_c3 + 1 - max_r_c3; // 2 (visible indices 2..=4)

        let filters = [String::new(), String::new(), String::new()];
        let sel_idx = [sel_c1, 0, sel_c3];
        let scroll_idx = [scroll_c1, 0, scroll_c3];

        let frame = render_frame(&RenderState {
            term_w: 100,
            term_h,
            active_col: 2,
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
            term_w: 100,
            term_h: 20,
            active_col: 0,
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
            active_col: 2,
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
}
