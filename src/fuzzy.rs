use crate::model::MillerItem;

#[derive(Debug, Clone)]
pub struct FilteredItem {
    pub item_idx: usize,
    pub score: i32,
    pub match_positions: Vec<usize>,
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

/// Resolves the selected item in a filtered view against its backing catalog.
#[inline]
pub fn selected_item<'a>(
    filtered: &[FilteredItem],
    items: &'a [MillerItem],
    sel_idx: usize,
) -> Option<&'a MillerItem> {
    filtered.get(sel_idx).and_then(|f| items.get(f.item_idx))
}

pub fn filter_items(items: &[MillerItem], query: &str) -> Vec<FilteredItem> {
    let q = query.trim();
    if q.is_empty() {
        return (0..items.len())
            .map(|i| FilteredItem {
                item_idx: i,
                score: 0,
                match_positions: Vec::new(),
            })
            .collect();
    }

    let q_lower = q.to_ascii_lowercase();
    let q_chars: Vec<char> = q_lower.chars().collect();

    let mut matched: Vec<FilteredItem> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            fuzzy_match_precomputed(&item.label, &item.desc, &q_chars, &q_lower).map(
                |(score, match_positions)| FilteredItem {
                    item_idx: idx,
                    score,
                    match_positions,
                },
            )
        })
        .collect();

    matched.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.item_idx.cmp(&b.item_idx))
    });
    matched
}
