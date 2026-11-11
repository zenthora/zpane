use crate::fuzzy::FilteredItem;
use crate::model::MillerItem;
use std::collections::HashSet;
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

pub fn clean_single_line(s: &str) -> String {
    s.chars()
        .map(|c| if matches!(c, '\r' | '\n' | '\t') { ' ' } else { c })
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

    let match_set: HashSet<usize> = filtered.match_positions.iter().copied().collect();
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
    let chars: Vec<char> = clean_label.chars().collect();
    for (idx, &ch) in chars.iter().enumerate() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if cur_w + w > avail_label_w {
            if avail_label_w > 1 && cur_w < avail_label_w {
                out.push('…');
                cur_w += 1;
            }
            break;
        }
        if match_set.contains(&idx) {
            out.push_str(C_MATCH_HL);
            out.push(ch);
            out.push_str(base_style);
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
    pub info_title: &'a str,
    pub info_desc: &'a str,
    pub active_param_set: Option<&'a str>,
}

pub fn render_frame(st: &RenderState<'_>) -> String {
    let win_w = st.term_w.max(76);
    let win_h = st.term_h.max(16);
    let list_rows = win_h.saturating_sub(6); // 1 top border + list_rows + 1 sep + 3 status/footer + 1 bottom border

    let inner_w = win_w.saturating_sub(4);
    let w1 = (inner_w * 28 / 100).max(20);
    let w2 = (inner_w * 32 / 100).max(22);
    let w3 = inner_w.saturating_sub(w1 + w2).max(22);

    let mut sb = String::with_capacity(32768);
    sb.push_str("\x1b[H"); // Cursor to home (0,0) for flicker-free overwrite

    // Top Border with Column Headers
    let h1_txt = format!(" 1. Command / Scope ({}) ", st.col1_filtered.len());
    let h2_txt = if let Some(set_name) = st.active_param_set {
        format!(" 2. Params [Set: {set_name}] ({}) ", st.col2_filtered.len())
    } else {
        format!(" 2. Subcommands / Params ({}) ", st.col2_filtered.len())
    };
    let h3_txt = format!(" 3. Flags / Values / Info ({}) ", st.col3_filtered.len());

    let h1_c = if st.active_col == 0 { C_HEADER_ACT } else { C_HEADER_DIM };
    let h2_c = if st.active_col == 1 { C_HEADER_ACT } else { C_HEADER_DIM };
    let h3_c = if st.active_col == 2 { C_HEADER_ACT } else { C_HEADER_DIM };

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
            let t = pad_or_truncate(st.info_title, w3);
            format!("{C_HEADER_ACT}{t}{RST}")
        } else if r == 1 {
            let t = pad_or_truncate(st.info_desc, w3);
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
        "{C_BORDER_DIM}╰{}╯{RST}",
        "─".repeat(full_inner)
    );

    sb
}
