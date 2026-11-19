use crate::model::Entry;
use crate::sanitize::contains_ignore_ascii_case;

#[derive(Debug, Clone, Copy)]
struct FilteredEntry {
    entry_idx: u32,
    score: i32,
    pos_start: u32,
    pos_len: u32,
}

/// Deep fuzzy-filtered list state for a single [`crate::model::Pane`]: maintains
/// filtered matches in a flat reusable arena and clamps cursor bounds eagerly on every mutation.
#[derive(Debug, Clone, Default)]
pub struct PaneList {
    entries: Vec<Entry>,
    filter: String,
    has_active_filter: bool,
    last_query_lower: String,
    filtered: Vec<FilteredEntry>,
    filtered_scratch: Vec<FilteredEntry>,
    positions_arena: Vec<usize>,
    sel_idx: usize,
    scroll_idx: usize,
}

impl PaneList {
    pub fn new(entries: Vec<Entry>) -> Self {
        Self {
            entries,
            filter: String::new(),
            has_active_filter: false,
            last_query_lower: String::new(),
            filtered: Vec::new(),
            filtered_scratch: Vec::new(),
            positions_arena: Vec::new(),
            sel_idx: 0,
            scroll_idx: 0,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn sel_idx(&self) -> usize {
        self.sel_idx
    }

    pub fn scroll_idx(&self) -> usize {
        self.scroll_idx
    }

    pub fn visible_len(&self) -> usize {
        if self.has_active_filter {
            self.filtered.len()
        } else {
            self.entries.len()
        }
    }

    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.recompute_full();
    }

    pub fn push_filter_char(&mut self, ch: char) {
        if crate::sanitize::is_disallowed_control(ch) {
            return;
        }
        self.filter.push(ch);
        self.sel_idx = 0;
        self.scroll_idx = 0;
        self.recompute_after_push();
    }

    pub fn pop_filter_char(&mut self) -> bool {
        if self.filter.pop().is_some() {
            self.sel_idx = 0;
            self.scroll_idx = 0;
            self.recompute_full();
            true
        } else {
            false
        }
    }

    pub fn clear_filter(&mut self) -> bool {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.has_active_filter = false;
            self.last_query_lower.clear();
            self.filtered.clear();
            self.positions_arena.clear();
            self.sel_idx = 0;
            self.scroll_idx = 0;
            true
        } else {
            false
        }
    }

    fn reset_unfiltered(&mut self) {
        self.has_active_filter = false;
        self.last_query_lower.clear();
        self.filtered.clear();
        self.positions_arena.clear();
        self.clamp_sel();
    }

    fn recompute_after_push(&mut self) {
        let q = self.filter.trim();
        if q.is_empty() {
            self.reset_unfiltered();
            return;
        }

        let q_is_ascii = q.is_ascii();
        let q_lower = if q_is_ascii {
            q.to_ascii_lowercase()
        } else {
            q.to_lowercase()
        };

        if self.has_active_filter && !self.last_query_lower.is_empty() {
            if q_lower == self.last_query_lower {
                self.clamp_sel();
                return;
            }
            if q_lower.starts_with(&self.last_query_lower) {
                self.recompute_incremental(q_lower, q_is_ascii);
                return;
            }
        }

        self.recompute_full_with_lower(q_lower, q_is_ascii);
    }

    fn recompute_full(&mut self) {
        let q = self.filter.trim();
        if q.is_empty() {
            self.reset_unfiltered();
            return;
        }
        let q_is_ascii = q.is_ascii();
        let q_lower = if q_is_ascii {
            q.to_ascii_lowercase()
        } else {
            q.to_lowercase()
        };
        self.recompute_full_with_lower(q_lower, q_is_ascii);
    }

    fn recompute_full_with_lower(&mut self, q_lower: String, q_is_ascii: bool) {
        self.has_active_filter = true;
        self.filtered.clear();
        self.positions_arena.clear();

        let (q_chars_buf, q_chars_overflow, q_len) = collect_query_chars(&q_lower);
        let q_chars = match q_chars_overflow {
            Some(ref vec) => vec.as_slice(),
            None => &q_chars_buf[..q_len],
        };

        self.filtered.reserve(self.entries.len().min(4096));
        self.positions_arena
            .reserve(self.entries.len().saturating_mul(q_len.min(16)).min(65_536));

        let mut desc_scratch = String::new();
        for (idx, entry) in self.entries.iter().enumerate() {
            if let Some((score, pos_start, pos_len)) = fuzzy_match_precomputed(
                &entry.label,
                &entry.desc,
                q_chars,
                &q_lower,
                q_is_ascii,
                &mut self.positions_arena,
                &mut desc_scratch,
            ) {
                self.filtered.push(FilteredEntry {
                    entry_idx: idx as u32,
                    score,
                    pos_start: pos_start as u32,
                    pos_len: pos_len as u32,
                });
            }
        }

        self.filtered.sort_unstable_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry_idx.cmp(&b.entry_idx))
        });
        self.last_query_lower = q_lower;
        self.clamp_sel();
    }

    fn recompute_incremental(&mut self, q_lower: String, q_is_ascii: bool) {
        self.positions_arena.clear();
        self.filtered_scratch.clear();

        let (q_chars_buf, q_chars_overflow, q_len) = collect_query_chars(&q_lower);
        let q_chars = match q_chars_overflow {
            Some(ref vec) => vec.as_slice(),
            None => &q_chars_buf[..q_len],
        };

        self.filtered_scratch.reserve(self.filtered.len());
        self.positions_arena.reserve(
            self.filtered
                .len()
                .saturating_mul(q_len.min(16))
                .min(65_536),
        );

        let mut desc_scratch = String::new();
        for prev in &self.filtered {
            let entry = &self.entries[prev.entry_idx as usize];
            if let Some((score, pos_start, pos_len)) = fuzzy_match_precomputed(
                &entry.label,
                &entry.desc,
                q_chars,
                &q_lower,
                q_is_ascii,
                &mut self.positions_arena,
                &mut desc_scratch,
            ) {
                self.filtered_scratch.push(FilteredEntry {
                    entry_idx: prev.entry_idx,
                    score,
                    pos_start: pos_start as u32,
                    pos_len: pos_len as u32,
                });
            }
        }

        std::mem::swap(&mut self.filtered, &mut self.filtered_scratch);
        self.filtered.sort_unstable_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.entry_idx.cmp(&b.entry_idx))
        });
        self.last_query_lower = q_lower;
        self.clamp_sel();
    }

    fn clamp_sel(&mut self) {
        let len = self.visible_len();
        if self.sel_idx >= len {
            self.sel_idx = len.saturating_sub(1);
        }
        if self.scroll_idx > self.sel_idx {
            self.scroll_idx = self.sel_idx;
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        if !self.has_active_filter {
            self.entries.get(self.sel_idx)
        } else {
            self.filtered
                .get(self.sel_idx)
                .and_then(|f| self.entries.get(f.entry_idx as usize))
        }
    }

    pub fn selected_source_idx(&self) -> Option<usize> {
        if !self.has_active_filter {
            (self.sel_idx < self.entries.len()).then_some(self.sel_idx)
        } else {
            self.filtered
                .get(self.sel_idx)
                .map(|f| f.entry_idx as usize)
        }
    }

    pub fn row_at(&self, visible_idx: usize) -> Option<(&Entry, &[usize])> {
        if !self.has_active_filter {
            return self.entries.get(visible_idx).map(|entry| (entry, &[][..]));
        }
        let f = self.filtered.get(visible_idx)?;
        let entry = self.entries.get(f.entry_idx as usize)?;
        let start = f.pos_start as usize;
        let len = f.pos_len as usize;
        let positions = &self.positions_arena[start..start + len];
        Some((entry, positions))
    }

    pub fn set_sel_idx(&mut self, pos: usize) {
        self.sel_idx = pos;
        self.clamp_sel();
    }

    pub fn reset_cursor(&mut self) {
        self.sel_idx = 0;
        self.scroll_idx = 0;
    }

    pub fn select_first_matching(&mut self, mut pred: impl FnMut(&Entry) -> bool) -> bool {
        let pos = if !self.has_active_filter {
            self.entries.iter().position(pred)
        } else {
            self.filtered
                .iter()
                .position(|f| pred(&self.entries[f.entry_idx as usize]))
        };
        if let Some(pos) = pos {
            self.sel_idx = pos;
            true
        } else {
            false
        }
    }

    pub fn move_home(&mut self) -> bool {
        if self.visible_len() == 0 && self.sel_idx == 0 {
            return false;
        }
        self.sel_idx = 0;
        true
    }

    pub fn move_up(&mut self) -> bool {
        let len = self.visible_len();
        if len == 0 {
            return false;
        }
        self.sel_idx = if self.sel_idx > 0 {
            self.sel_idx - 1
        } else {
            len - 1
        };
        true
    }

    pub fn move_down(&mut self) -> bool {
        let len = self.visible_len();
        if len == 0 {
            return false;
        }
        self.sel_idx = if self.sel_idx + 1 < len {
            self.sel_idx + 1
        } else {
            0
        };
        true
    }

    pub fn page_up(&mut self, page_rows: usize) -> bool {
        if self.visible_len() == 0 || page_rows == 0 {
            return false;
        }
        self.sel_idx = self.sel_idx.saturating_sub(page_rows);
        true
    }

    pub fn page_down(&mut self, page_rows: usize) -> bool {
        let len = self.visible_len();
        if len == 0 || page_rows == 0 {
            return false;
        }
        self.sel_idx = self.sel_idx.saturating_add(page_rows).min(len - 1);
        true
    }

    pub fn move_end(&mut self) -> bool {
        let len = self.visible_len();
        if len == 0 {
            return false;
        }
        self.sel_idx = len - 1;
        true
    }

    pub fn clamp_scroll(&mut self, max_rows: usize) {
        if max_rows == 0 {
            self.scroll_idx = 0;
            return;
        }
        if self.sel_idx < self.scroll_idx {
            self.scroll_idx = self.sel_idx;
        }
        if self.sel_idx >= self.scroll_idx.saturating_add(max_rows) {
            self.scroll_idx = self.sel_idx + 1 - max_rows;
        }
        let max_scroll = self.visible_len().saturating_sub(max_rows);
        if self.scroll_idx > max_scroll {
            self.scroll_idx = max_scroll;
        }
    }
}

#[inline]
fn collect_query_chars(q_lower: &str) -> ([char; 64], Option<Vec<char>>, usize) {
    let mut buf = ['\0'; 64];
    let mut len = 0usize;
    for ch in q_lower.chars() {
        if len < 64 {
            buf[len] = ch;
            len += 1;
        } else {
            let vec: Vec<char> = q_lower.chars().collect();
            let vlen = vec.len();
            return (buf, Some(vec), vlen);
        }
    }
    (buf, None, len)
}

#[inline]
fn score_boundary(prev_orig: char, cur_orig: char) -> i32 {
    if matches!(prev_orig, '-' | '_' | ':' | '.' | ' ' | '/' | '\\' | '$') {
        32
    } else if (prev_orig.is_ascii_lowercase() && cur_orig.is_ascii_uppercase())
        || (!prev_orig.is_ascii() && prev_orig.is_lowercase() && cur_orig.is_uppercase())
    {
        30
    } else {
        0
    }
}

/// Computes fuzzy subsequence match score and appends matched char indices into `positions_arena`.
fn fuzzy_match_precomputed(
    label: &str,
    desc: &str,
    q_chars: &[char],
    q_lower_str: &str,
    q_is_ascii: bool,
    positions_arena: &mut Vec<usize>,
    desc_scratch: &mut String,
) -> Option<(i32, usize, usize)> {
    let q_len = q_chars.len();
    if q_len == 0 {
        return Some((0, 0, 0));
    }

    let pos_start = positions_arena.len();
    let mut ran_clean_ascii = false;

    // Fast path: ASCII query against ASCII label validated inline in a single byte scan.
    if q_is_ascii && label.is_ascii() {
        let l_bytes = label.as_bytes();
        let label_len = l_bytes.len();
        if label_len >= q_len {
            let q_bytes = q_lower_str.as_bytes();
            let mut q_idx = 0usize;
            let mut target_lower = q_bytes[0];
            let mut target_upper = target_lower.to_ascii_uppercase();
            let first_match = if target_lower == target_upper {
                l_bytes.iter().position(|&b| b == target_lower)
            } else {
                l_bytes
                    .iter()
                    .position(|&b| b == target_lower || b == target_upper)
            };

            if let Some(first_pos) = first_match {
                if !l_bytes[..first_pos].iter().any(|&b| b < 0x20 || b == 0x7f) {
                    if label_len - first_pos < q_len {
                        ran_clean_ascii = true;
                    } else {
                        let mut score: i32 = 0;
                        let mut prev_match_idx: Option<usize> = None;
                        let first_byte = l_bytes[0];
                        let mut ascii_clean = true;
                        let mut i = first_pos;

                        while i < label_len {
                            let cur_byte = l_bytes[i];
                            if cur_byte < 0x20 || cur_byte == 0x7f {
                                ascii_clean = false;
                                positions_arena.truncate(pos_start);
                                break;
                            }
                            if cur_byte == target_lower || cur_byte == target_upper {
                                positions_arena.push(i);

                                score += 10;
                                if i == 0 {
                                    score += 45;
                                } else if i == 1 && (first_byte == b'-' || first_byte == b'$') {
                                    score += 42;
                                }

                                if let Some(prev) = prev_match_idx {
                                    if i == prev + 1 {
                                        score += 28;
                                    } else {
                                        let gap = (i - prev - 1) as i32;
                                        score -= gap.min(12);
                                    }
                                }

                                if i > 0 {
                                    let prev = l_bytes[i - 1];
                                    if matches!(
                                        prev,
                                        b'-' | b'_' | b':' | b'.' | b' ' | b'/' | b'\\' | b'$'
                                    ) {
                                        score += 32;
                                    } else if prev.is_ascii_lowercase()
                                        && cur_byte.is_ascii_uppercase()
                                    {
                                        score += 30;
                                    }
                                }

                                prev_match_idx = Some(i);
                                q_idx += 1;
                                if q_idx == q_len {
                                    let mut j = i + 1;
                                    while j < label_len {
                                        let b = l_bytes[j];
                                        if b < 0x20 || b == 0x7f {
                                            ascii_clean = false;
                                            positions_arena.truncate(pos_start);
                                            break;
                                        }
                                        j += 1;
                                    }
                                    if ascii_clean {
                                        score += (40_i32.saturating_sub(label_len as i32)).max(0);
                                        return Some((score, pos_start, q_len));
                                    }
                                    break;
                                }
                                target_lower = q_bytes[q_idx];
                                target_upper = target_lower.to_ascii_uppercase();
                            }
                            i += 1;
                        }

                        if ascii_clean {
                            ran_clean_ascii = true;
                            positions_arena.truncate(pos_start);
                        }
                    }
                }
            } else if target_lower != b' ' {
                ran_clean_ascii = true;
            }
        } else {
            ran_clean_ascii = true;
        }
    }

    if !ran_clean_ascii {
        let mut q_idx = 0usize;
        let mut score: i32 = 0;
        let mut prev_match_vis_idx: Option<usize> = None;
        let mut first_vis_char: char = '\0';
        let mut prev_vis_orig: char = '\0';
        let mut vis_char_len: usize = 0;

        let has_ctrl = crate::sanitize::has_disallowed_control(label);
        let mut byte_idx = 0usize;
        let mut raw_char_idx = 0usize;

        while byte_idx < label.len() {
            let Some(raw_ch) = label[byte_idx..].chars().next() else {
                break;
            };
            if has_ctrl && (raw_ch == '\x1b' || ('\u{0080}'..='\u{009f}').contains(&raw_ch)) {
                if let Some((esc_bytes, esc_chars)) =
                    crate::sanitize::next_escape_seq_len(&label[byte_idx..])
                {
                    byte_idx += esc_bytes;
                    raw_char_idx += esc_chars;
                    continue;
                }
            }

            let cur_raw_idx = raw_char_idx;
            byte_idx += raw_ch.len_utf8();
            raw_char_idx += 1;

            let cur_orig = if has_ctrl {
                let Some(clean) = crate::sanitize::sanitize_display_char(raw_ch) else {
                    continue;
                };
                clean
            } else {
                raw_ch
            };

            let vis_i = vis_char_len;
            if vis_i == 0 {
                first_vis_char = cur_orig;
            }
            vis_char_len = vis_i + 1;

            if q_idx < q_len {
                let q_ch = q_chars[q_idx];
                let matched_char = if cur_orig.is_ascii() {
                    cur_orig.to_ascii_lowercase() == q_ch
                } else if q_ch.is_ascii() {
                    false
                } else {
                    cur_orig == q_ch || {
                        let mut lc = cur_orig.to_lowercase();
                        lc.next() == Some(q_ch) && lc.next().is_none()
                    }
                };
                if matched_char {
                    positions_arena.push(cur_raw_idx);

                    score += 10;
                    if vis_i == 0 {
                        score += 45;
                    } else if vis_i == 1 && (first_vis_char == '-' || first_vis_char == '$') {
                        score += 42;
                    }

                    if let Some(prev) = prev_match_vis_idx {
                        if vis_i == prev + 1 {
                            score += 28;
                        } else {
                            let gap = (vis_i - prev - 1) as i32;
                            score -= gap.min(12);
                        }
                    }

                    if vis_i > 0 {
                        score += score_boundary(prev_vis_orig, cur_orig);
                    }

                    prev_match_vis_idx = Some(vis_i);
                    q_idx += 1;
                }
            }
            prev_vis_orig = cur_orig;
        }

        if q_idx == q_len {
            score += (40_i32.saturating_sub(vis_char_len as i32)).max(0);
            return Some((score, pos_start, q_len));
        }
        positions_arena.truncate(pos_start);
    }

    // Fallback: case-insensitive substring match on sanitized description.
    let desc_target = if crate::sanitize::has_disallowed_control(desc) {
        crate::sanitize::strip_escapes_and_controls_into(desc, desc_scratch);
        desc_scratch.as_str()
    } else {
        desc
    };

    if q_is_ascii {
        if contains_ignore_ascii_case(desc_target, q_lower_str.as_bytes()) {
            return Some((5, 0, 0));
        }
    } else if desc_target.to_lowercase().contains(q_lower_str) {
        return Some((5, 0, 0));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryKind;

    fn sample_entries() -> Vec<Entry> {
        vec![
            Entry::new("git", "git", EntryKind::Cli, "Distributed version control"),
            Entry::new(
                "Get-Process",
                "Get-Process",
                EntryKind::Cmdlet,
                "List processes",
            ),
            Entry::new("Get-Content", "Get-Content", EntryKind::Cmdlet, "Read file"),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Rust package manager"),
        ]
    }

    #[test]
    fn test_eager_pane_filtering_ranking_and_desc_fallback() {
        let mut pane = PaneList::new(sample_entries());
        assert_eq!(pane.visible_len(), 4);
        assert_eq!(pane.selected().unwrap().text, "git");

        // Subsequence match on CamelCase prefers Get-Process over Get-Content for "gp"
        pane.push_filter_char('g');
        pane.push_filter_char('p');
        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.selected().unwrap().text, "Get-Process");
        let (_, positions) = pane.row_at(0).unwrap();
        assert_eq!(positions, &[0, 4]);

        // Clearing filter restores all entries eagerly and resets cursor
        assert!(pane.clear_filter());
        assert_eq!(pane.visible_len(), 4);
        assert_eq!(pane.sel_idx(), 0);

        // Description fallback when label does not match
        for ch in "rust".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.selected().unwrap().text, "cargo");
        assert!(pane.row_at(0).unwrap().1.is_empty());
    }

    #[test]
    fn test_pane_cursor_wrapping_paging_and_scroll_clamping() {
        let entries: Vec<Entry> = (0..10)
            .map(|i| Entry::new(format!("cmd{i}"), format!("cmd{i}"), EntryKind::Cli, ""))
            .collect();
        let mut pane = PaneList::new(entries);

        // Wrap up from 0 -> 9, down from 9 -> 0
        assert!(pane.move_up());
        assert_eq!(pane.sel_idx(), 9);
        assert!(pane.move_down());
        assert_eq!(pane.sel_idx(), 0);

        // Paging and scroll clamping in a 4-row viewport
        assert!(pane.page_down(4));
        assert_eq!(pane.sel_idx(), 4);
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 1);

        assert!(pane.move_end());
        assert_eq!(pane.sel_idx(), 9);
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 6);

        assert!(pane.page_up(4));
        assert_eq!(pane.sel_idx(), 5);
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 5);

        assert!(pane.move_home());
        assert_eq!(pane.sel_idx(), 0);
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 0);

        // Replacing entries clamps out-of-bounds selection eagerly
        pane.set_sel_idx(8);
        assert_eq!(pane.sel_idx(), 8);
        pane.set_entries(vec![Entry::new("only", "only", EntryKind::Cli, "")]);
        assert_eq!(pane.sel_idx(), 0);
        assert_eq!(pane.selected().unwrap().text, "only");
    }

    #[test]
    fn test_pane_scroll_clamps_on_viewport_growth_and_large_page_bounds() {
        let entries: Vec<Entry> = (0..10)
            .map(|i| Entry::new(format!("cmd{i}"), format!("cmd{i}"), EntryKind::Cli, ""))
            .collect();
        let mut pane = PaneList::new(entries);

        pane.move_end();
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 6);

        // Viewport grows taller: scroll_idx pulls back so all 8 visible rows fill cleanly
        pane.clamp_scroll(8);
        assert_eq!(pane.scroll_idx(), 2);

        // Viewport grows larger than entry count: scroll_idx resets to 0
        pane.clamp_scroll(12);
        assert_eq!(pane.scroll_idx(), 0);

        // Huge page_rows / max_rows must not overflow
        pane.move_home();
        assert!(pane.page_down(usize::MAX));
        assert_eq!(pane.sel_idx(), 9);
        pane.clamp_scroll(usize::MAX);
        assert_eq!(pane.scroll_idx(), 0);

        // Shrinking entries clamps scroll_idx eagerly
        pane.set_entries(
            (0..6)
                .map(|i| Entry::new(format!("x{i}"), format!("x{i}"), EntryKind::Cli, ""))
                .collect(),
        );
        pane.set_sel_idx(5);
        pane.clamp_scroll(4);
        assert_eq!(pane.scroll_idx(), 2);
        pane.push_filter_char('x');
        assert_eq!(pane.scroll_idx(), 0);
        assert_eq!(pane.sel_idx(), 0);
    }

    #[test]
    fn test_unicode_case_insensitive_pane_filtering() {
        let entries = vec![
            Entry::new("café", "Café", EntryKind::Arg, "Bistro menu"),
            Entry::new("über", "Über", EntryKind::Arg, "Übersicht helper"),
            Entry::new("plain", "plain", EntryKind::Cli, "Plain ASCII"),
        ];
        let mut pane = PaneList::new(entries);

        for ch in "CAFÉ".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.selected().unwrap().text, "café");
        assert_eq!(pane.row_at(0).unwrap().1, &[0, 1, 2, 3]);

        assert!(pane.clear_filter());
        for ch in "übersicht".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.selected().unwrap().text, "über");
    }

    #[test]
    fn test_pentest_fuzzy_ignores_escape_payloads_and_blocks_control_filter_chars() {
        let entries = vec![
            Entry::new(
                "git",
                "\x1b]52;c;SGVsbG8=\x07\x1b[31mgit\x1b[0m",
                EntryKind::Cli,
                "\x1b]0;secret_title\x1b\\Version control",
            ),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Build tool"),
        ];
        let mut pane = PaneList::new(entries);

        // Control / escape / BiDi chars pushed into filter must be rejected
        for bad in ['\x1b', '\x07', '\x00', '\x7f', '\u{009b}', '\u{202e}', '\n'] {
            pane.push_filter_char(bad);
        }
        assert_eq!(pane.filter(), "");
        assert_eq!(pane.visible_len(), 2);

        // Hidden OSC/CSI/title payloads must not be searchable
        for ch in "52".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 0);
        assert!(pane.clear_filter());

        for ch in "secret_title".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 0);
        assert!(pane.clear_filter());

        // Visible text after stripped escape sequence matches and aligns raw char indices
        for ch in "git".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        let (entry, positions) = pane.row_at(0).unwrap();
        assert_eq!(entry.text, "git");
        let label_chars: Vec<char> = entry.label.chars().collect();
        let matched_chars: String = positions.iter().map(|&idx| label_chars[idx]).collect();
        assert_eq!(matched_chars, "git");
    }

    #[test]
    fn test_incremental_pane_filtering_matches_full_recompute_on_large_catalog() {
        let entries: Vec<Entry> = (0..5000)
            .map(|i| {
                let name = if i == 3456 {
                    "git-absorb".to_string()
                } else {
                    format!("Get-SyntheticResourceItem{i:04}")
                };
                Entry::new(&name, &name, EntryKind::Cmdlet, "Synthetic provider entry")
            })
            .collect();

        let mut incremental = PaneList::new(entries.clone());
        for ch in ['g', 'i', 't'] {
            incremental.push_filter_char(ch);
        }
        // Trailing space keeps active filter query identical; backspace triggers full recompute
        incremental.push_filter_char(' ');
        assert_eq!(incremental.selected().unwrap().text, "git-absorb");

        let mut fresh = PaneList::new(entries);
        for ch in ['g', 'i', 't', ' ', 'x'] {
            fresh.push_filter_char(ch);
        }
        assert!(fresh.pop_filter_char());

        assert_eq!(incremental.visible_len(), fresh.visible_len());
        for i in 0..incremental.visible_len().min(20) {
            let (a_entry, a_pos) = incremental.row_at(i).unwrap();
            let (b_entry, b_pos) = fresh.row_at(i).unwrap();
            assert_eq!(a_entry.text, b_entry.text);
            assert_eq!(a_pos, b_pos);
        }
    }

    #[test]
    fn test_pentest_fuzzy_fast_ascii_precheck_preserves_escape_and_control_fallback() {
        let entries = vec![
            Entry::new(
                "osc-prefix",
                "\x1b]52;c;abc\x07xyz",
                EntryKind::Cli,
                "Desc with \x1b]0;secret_desc\x07visible_note",
            ),
            Entry::new("csi-mid", "pre\x1b[31mfix", EntryKind::Cli, ""),
            Entry::new("csi-tail", "git\x1b[31m", EntryKind::Cli, ""),
            Entry::new("ctrl-nul", "m\x00n\x7fp", EntryKind::Cli, ""),
            Entry::new(
                "short-tail",
                "zzza",
                EntryKind::Cli,
                "Fallback match target",
            ),
            Entry::new(
                "bidi-unicode",
                "\u{202e}café\u{2069}",
                EntryKind::Arg,
                "Bistro",
            ),
        ];

        // Hidden escape bytes inside OSC/CSI payloads must not match even if ASCII bytes exist.
        let mut pane = PaneList::new(entries.clone());
        for hidden in ["abc", "31m", "secret_desc"] {
            for ch in hidden.chars() {
                pane.push_filter_char(ch);
            }
            assert_eq!(pane.visible_len(), 0, "hidden payload {hidden:?} matched");
            pane.clear_filter();
        }

        // Visible text after OSC/CSI must still match with exact raw character positions.
        for ch in "xyz".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        let (entry, pos) = pane.row_at(0).unwrap();
        assert_eq!(entry.text, "osc-prefix");
        assert_eq!(pos, &[11, 12, 13]);
        pane.clear_filter();

        for ch in "prefix".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        let (entry, pos) = pane.row_at(0).unwrap();
        assert_eq!(entry.text, "csi-mid");
        assert_eq!(pos, &[0, 1, 2, 8, 9, 10]);
        pane.clear_filter();

        // Trailing CSI must not inflate label length penalty vs clean "git".
        let mut clean_vs_csi = PaneList::new(vec![
            Entry::new("csi-git", "git\x1b[31m", EntryKind::Cli, ""),
            Entry::new("clean-git", "git", EntryKind::Cli, ""),
        ]);
        for ch in "git".chars() {
            clean_vs_csi.push_filter_char(ch);
        }
        assert_eq!(clean_vs_csi.visible_len(), 2);
        // Same score -> tie-break preserves original index 0 ("csi-git") before 1 ("clean-git")
        assert_eq!(clean_vs_csi.row_at(0).unwrap().0.text, "csi-git");
        assert_eq!(clean_vs_csi.row_at(0).unwrap().1, &[0, 1, 2]);

        // Control chars between ASCII characters fall back cleanly and track raw char positions.
        for ch in "mnp".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        let (entry, pos) = pane.row_at(0).unwrap();
        assert_eq!(entry.text, "ctrl-nul");
        assert_eq!(pos, &[0, 2, 4]);
        pane.clear_filter();

        // Suffix shorter than query after first_pos ("zzza" vs "ab") skips label and matches desc.
        for ch in "fallback".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        assert_eq!(pane.row_at(0).unwrap().0.text, "short-tail");
        pane.clear_filter();

        // BiDi controls around Unicode text must be ignored while case-folding still matches.
        for ch in "CAFÉ".chars() {
            pane.push_filter_char(ch);
        }
        assert_eq!(pane.visible_len(), 1);
        let (entry, pos) = pane.row_at(0).unwrap();
        assert_eq!(entry.text, "bidi-unicode");
        assert_eq!(pos, &[1, 2, 3, 4]);
        pane.clear_filter();

        // Oversized (> 64 char) query exercises collect_query_chars overflow and arena cap safely.
        for _ in 0..80 {
            pane.push_filter_char('z');
        }
        assert_eq!(pane.visible_len(), 0);
    }

    #[test]
    fn test_perf_fuzzy_pane_filtering_throughput() {
        let mut entries: Vec<Entry> = (0..5000)
            .map(|i| {
                Entry::new(
                    format!("Get-SyntheticResourceItem{i:04}"),
                    format!("Get-SyntheticResourceItem{i:04}"),
                    EntryKind::Cmdlet,
                    "Synthetic Command Catalog provider entry",
                )
            })
            .collect();
        entries[4321] = Entry::new(
            "Get-ItemProperty",
            "Get-ItemProperty",
            EntryKind::Cmdlet,
            "Gets the properties of a specified item.",
        );

        let mut pane = PaneList::new(entries);
        const CYCLES: usize = 20;
        let start = std::time::Instant::now();
        let mut matched_total = 0usize;

        for _ in 0..CYCLES {
            for ch in ['g', 'e', 't', '-', 'i', 't'] {
                pane.push_filter_char(ch);
            }
            matched_total += std::hint::black_box(pane.visible_len());
            assert!(pane.pop_filter_char());
            matched_total += std::hint::black_box(pane.visible_len());
            pane.clear_filter();

            // Negative ASCII pre-check path across all 5000 entries
            pane.push_filter_char('z');
            assert_eq!(pane.visible_len(), 0);
            pane.clear_filter();
        }

        let elapsed = start.elapsed();
        let secs = elapsed.as_secs_f64().max(1e-9);
        println!(
            "test_perf_fuzzy_pane_filtering_throughput: {CYCLES} cycles over 5000 entries in {:.2} ms ({:.1} cycles/s, matched_total={matched_total})",
            secs * 1000.0,
            (CYCLES as f64) / secs
        );
        assert_eq!(matched_total, CYCLES * (5000 + 5000));
        assert!(
            secs < 10.0,
            "PaneList filtering throughput regressed severely: {secs:.2}s"
        );
    }
}
