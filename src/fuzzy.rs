use crate::model::MillerItem;

#[derive(Debug, Clone)]
pub struct FilteredItem {
    pub item_idx: usize,
    pub score: i32,
    pub match_positions: Vec<usize>,
}

/// Computes fuzzy subsequence match score and matched char indices on `label`,
/// with fallback to substring match on `desc`.
pub fn fuzzy_match(label: &str, desc: &str, query: &str) -> Option<(i32, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }

    let q_chars: Vec<char> = query.chars().map(|c| c.to_ascii_lowercase()).collect();
    let l_chars: Vec<char> = label.chars().collect();
    let l_lower: Vec<char> = l_chars.iter().map(|c| c.to_ascii_lowercase()).collect();

    // 1. Try subsequence match on label
    let mut positions = Vec::with_capacity(q_chars.len());
    let mut q_idx = 0;
    let mut score: i32 = 0;
    let mut prev_match_idx: Option<usize> = None;

    for (i, &lc) in l_lower.iter().enumerate() {
        if q_idx < q_chars.len() && lc == q_chars[q_idx] {
            positions.push(i);

            // Base match score
            score += 10;

            // Prefix bonus
            if i == 0 {
                score += 45;
            } else if i == 1 && (l_chars[0] == '-' || l_chars[0] == '$') {
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
                let prev_orig = l_chars[i - 1];
                let cur_orig = l_chars[i];
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

    if q_idx == q_chars.len() {
        // Shorter labels that match completely get a slight tie-break bonus
        score += (40_i32 - (l_chars.len() as i32)).max(0);
        return Some((score, positions));
    }

    // 2. Fallback: case-insensitive substring match in description
    let q_lower = query.to_ascii_lowercase();
    if desc.to_ascii_lowercase().contains(&q_lower) {
        return Some((5, Vec::new()));
    }

    None
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

    let mut matched: Vec<FilteredItem> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, item)| {
            fuzzy_match(&item.label, &item.desc, q).map(|(score, match_positions)| FilteredItem {
                item_idx: idx,
                score,
                match_positions,
            })
        })
        .collect();

    matched.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.item_idx.cmp(&b.item_idx)));
    matched
}
