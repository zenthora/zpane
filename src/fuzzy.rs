use crate::model::Entry;

#[derive(Debug, Clone)]
struct FilteredEntry {
    entry_idx: usize,
    score: i32,
    match_positions: Vec<usize>,
}

/// Deep fuzzy-filtered list state for a single [`crate::model::Pane`]: maintains
/// filtered matches and clamped cursor bounds eagerly on every mutation.
#[derive(Debug, Clone, Default)]
pub struct PaneList {
    entries: Vec<Entry>,
    filter: String,
    filtered: Vec<FilteredEntry>,
    sel_idx: usize,
    scroll_idx: usize,
}

impl PaneList {
    pub fn new(entries: Vec<Entry>) -> Self {
        let mut list = Self {
            entries,
            filter: String::new(),
            filtered: Vec::new(),
            sel_idx: 0,
            scroll_idx: 0,
        };
        list.recompute_filtered();
        list
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
        self.filtered.len()
    }

    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        self.entries = entries;
        self.recompute_filtered();
    }

    pub fn push_filter_char(&mut self, ch: char) {
        self.filter.push(ch);
        self.sel_idx = 0;
        self.recompute_filtered();
    }

    pub fn pop_filter_char(&mut self) -> bool {
        if self.filter.pop().is_some() {
            self.sel_idx = 0;
            self.recompute_filtered();
            true
        } else {
            false
        }
    }

    pub fn clear_filter(&mut self) -> bool {
        if !self.filter.is_empty() {
            self.filter.clear();
            self.sel_idx = 0;
            self.recompute_filtered();
            true
        } else {
            false
        }
    }

    fn recompute_filtered(&mut self) {
        self.filtered = filter_entries(&self.entries, &self.filter);
        self.clamp_sel();
    }

    fn clamp_sel(&mut self) {
        if self.sel_idx >= self.filtered.len() {
            self.sel_idx = self.filtered.len().saturating_sub(1);
        }
    }

    pub fn selected(&self) -> Option<&Entry> {
        self.filtered
            .get(self.sel_idx)
            .and_then(|f| self.entries.get(f.entry_idx))
    }

    pub fn selected_source_idx(&self) -> Option<usize> {
        self.filtered.get(self.sel_idx).map(|f| f.entry_idx)
    }

    pub fn row_at(&self, visible_idx: usize) -> Option<(&Entry, &[usize])> {
        let f = self.filtered.get(visible_idx)?;
        let entry = self.entries.get(f.entry_idx)?;
        Some((entry, &f.match_positions))
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
        if let Some(pos) = self
            .filtered
            .iter()
            .position(|f| pred(&self.entries[f.entry_idx]))
        {
            self.sel_idx = pos;
            true
        } else {
            false
        }
    }

    pub fn move_home(&mut self) -> bool {
        if self.filtered.is_empty() && self.sel_idx == 0 {
            return false;
        }
        self.sel_idx = 0;
        true
    }

    pub fn move_up(&mut self) -> bool {
        let len = self.filtered.len();
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
        let len = self.filtered.len();
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
        if self.filtered.is_empty() || page_rows == 0 {
            return false;
        }
        self.sel_idx = self.sel_idx.saturating_sub(page_rows);
        true
    }

    pub fn page_down(&mut self, page_rows: usize) -> bool {
        let len = self.filtered.len();
        if len == 0 || page_rows == 0 {
            return false;
        }
        self.sel_idx = (self.sel_idx + page_rows).min(len - 1);
        true
    }

    pub fn move_end(&mut self) -> bool {
        let len = self.filtered.len();
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
        if self.sel_idx >= self.scroll_idx + max_rows {
            self.scroll_idx = self.sel_idx + 1 - max_rows;
        }
    }
}

/// Zero-allocation case-insensitive ASCII substring search.
#[inline]
fn contains_ignore_ascii_case(haystack: &str, needle_lower: &[u8]) -> bool {
    let n_len = needle_lower.len();
    if n_len == 0 {
        return true;
    }
    let h_bytes = haystack.as_bytes();
    if h_bytes.len() < n_len {
        return false;
    }
    let first = needle_lower[0];
    let max_start = h_bytes.len() - n_len;
    for i in 0..=max_start {
        if h_bytes[i].to_ascii_lowercase() == first
            && h_bytes[i + 1..i + n_len]
                .iter()
                .zip(&needle_lower[1..])
                .all(|(&h, &n)| h.to_ascii_lowercase() == n)
        {
            return true;
        }
    }
    false
}

/// Computes fuzzy subsequence match score and matched char indices on `label`,
/// with fallback to substring match on `desc`, using pre-lowercased query chars.
fn fuzzy_match_precomputed(
    label: &str,
    desc: &str,
    q_chars: &[char],
    q_lower_str: &str,
) -> Option<(i32, Vec<usize>)> {
    let q_len = q_chars.len();
    if q_len == 0 {
        return Some((0, Vec::new()));
    }

    // 1. Single-pass subsequence match on label with stack-buffered positions (zero heap allocs unless matched)
    let mut pos_buf = [0usize; 64];
    let mut pos_overflow: Option<Vec<usize>> = if q_len > 64 {
        Some(Vec::with_capacity(q_len))
    } else {
        None
    };

    let mut q_idx = 0;
    let mut score: i32 = 0;
    let mut prev_match_idx: Option<usize> = None;
    let mut first_char: char = '\0';
    let mut prev_orig: char = '\0';
    let mut label_char_len: usize = 0;

    for (i, cur_orig) in label.chars().enumerate() {
        if i == 0 {
            first_char = cur_orig;
        }
        label_char_len = i + 1;

        if q_idx < q_len {
            let lc = cur_orig.to_ascii_lowercase();
            if lc == q_chars[q_idx] {
                if let Some(ref mut vec) = pos_overflow {
                    vec.push(i);
                } else {
                    pos_buf[q_idx] = i;
                }

                // Base match score
                score += 10;

                // Prefix bonus
                if i == 0 {
                    score += 45;
                } else if i == 1 && (first_char == '-' || first_char == '$') {
                    score += 42;
                }

                // Consecutive match bonus
                if let Some(prev) = prev_match_idx {
                    if i == prev + 1 {
                        score += 28;
                    } else {
                        // Penalty for gap
                        let gap = (i - prev - 1) as i32;
                        score -= gap.min(12);
                    }
                }

                // Word boundary / CamelCase bonus
                if i > 0 {
                    if matches!(prev_orig, '-' | '_' | ':' | '.' | ' ' | '/' | '\\' | '$') {
                        score += 32;
                    } else if prev_orig.is_ascii_lowercase() && cur_orig.is_ascii_uppercase() {
                        score += 30;
                    }
                }

                prev_match_idx = Some(i);
                q_idx += 1;
            }
        }
        prev_orig = cur_orig;
    }

    if q_idx == q_len {
        // Shorter labels that match completely get a slight tie-break bonus
        score += (40_i32 - (label_char_len as i32)).max(0);
        let positions = match pos_overflow {
            Some(vec) => vec,
            None => pos_buf[..q_len].to_vec(),
        };
        return Some((score, positions));
    }

    // 2. Fallback: zero-allocation case-insensitive substring match in description
    if contains_ignore_ascii_case(desc, q_lower_str.as_bytes()) {
        return Some((5, Vec::new()));
    }

    None
}

fn filter_entries(entries: &[Entry], query: &str) -> Vec<FilteredEntry> {
    let q = query.trim();
    if q.is_empty() {
        return (0..entries.len())
            .map(|i| FilteredEntry {
                entry_idx: i,
                score: 0,
                match_positions: Vec::new(),
            })
            .collect();
    }

    let q_lower = q.to_ascii_lowercase();
    let q_chars: Vec<char> = q_lower.chars().collect();

    let mut matched: Vec<FilteredEntry> = entries
        .iter()
        .enumerate()
        .filter_map(|(idx, entry)| {
            fuzzy_match_precomputed(&entry.label, &entry.desc, &q_chars, &q_lower).map(
                |(score, match_positions)| FilteredEntry {
                    entry_idx: idx,
                    score,
                    match_positions,
                },
            )
        })
        .collect();

    matched.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.entry_idx.cmp(&b.entry_idx))
    });
    matched
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
}
