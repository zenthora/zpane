use crate::discovery::{CliIntrospector, Introspect, ShellDialect};
use crate::fuzzy::PaneList;
use crate::model::{
    is_safe_variable_token, sanitize_variable_token, Entry, EntryKind, Pane, RuntimeContext,
};
use crate::sanitize::{has_disallowed_control, is_disallowed_control, strip_disallowed_controls};
use crate::shell::{
    format_custom_value_token, is_safe_unquoted_char, is_safe_unquoted_token,
    is_valid_single_quoted, quote_literal_for_dialect, sanitize_initial_buffer,
    sanitize_literal_value_text, split_pipeline_prefix_for_dialect,
    tokenize_shell_words_for_dialect,
};
pub use crate::shell::{split_pipeline_prefix, tokenize_shell_words};
use crate::ui::{render_frame, RenderState, Viewport};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::borrow::Cow;
use std::collections::{HashSet, VecDeque};

/// Semantic user actions accepted by [`MillerSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserAction {
    MoveUp,
    MoveDown,
    PageUp,
    PageDown,
    MoveHome,
    MoveEnd,
    StepRight,
    StepLeft,
    AcceptToken,
    Backspace,
    InsertChar(char),
    ClearFilter,
    ReloadCatalog,
    ToggleInspector,
    Resize(usize, usize),
    Confirm,
    Cancel,
}

impl UserAction {
    /// Translates a terminal key event into a semantic [`UserAction`].
    pub fn from_key_event(key: KeyEvent) -> Option<Self> {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return None;
        }

        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => Some(UserAction::Cancel),
            (KeyCode::Enter, _) => Some(UserAction::Confirm),
            (KeyCode::F(1), _) => Some(UserAction::ToggleInspector),
            (KeyCode::Char('?'), mods) if mods.contains(KeyModifiers::ALT) => {
                Some(UserAction::ToggleInspector)
            }
            (KeyCode::Char(ch), mods) if mods == KeyModifiers::CONTROL => {
                match ch.to_ascii_lowercase() {
                    'c' => Some(UserAction::Cancel),
                    'r' => Some(UserAction::ReloadCatalog),
                    'u' => Some(UserAction::ClearFilter),
                    'p' | 'k' => Some(UserAction::MoveUp),
                    'n' | 'j' => Some(UserAction::MoveDown),
                    'f' => Some(UserAction::StepRight),
                    'b' => Some(UserAction::StepLeft),
                    _ => None,
                }
            }
            (KeyCode::Char(ch), mods) if mods == KeyModifiers::ALT => {
                match ch.to_ascii_lowercase() {
                    'i' => Some(UserAction::ToggleInspector),
                    'k' => Some(UserAction::MoveUp),
                    'j' => Some(UserAction::MoveDown),
                    'l' => Some(UserAction::StepRight),
                    'h' => Some(UserAction::StepLeft),
                    _ => None,
                }
            }
            (KeyCode::Up, _) => Some(UserAction::MoveUp),
            (KeyCode::Down, _) => Some(UserAction::MoveDown),
            (KeyCode::PageUp, _) => Some(UserAction::PageUp),
            (KeyCode::PageDown, _) => Some(UserAction::PageDown),
            (KeyCode::Home, _) => Some(UserAction::MoveHome),
            (KeyCode::End, _) => Some(UserAction::MoveEnd),
            (KeyCode::Right, _) => Some(UserAction::StepRight),
            (KeyCode::Left, _) => Some(UserAction::StepLeft),
            (KeyCode::Tab, _) => Some(UserAction::AcceptToken),
            (KeyCode::Backspace, _) => Some(UserAction::Backspace),
            (KeyCode::Char(ch), mods)
                if !mods.contains(KeyModifiers::CONTROL)
                    && !mods.contains(KeyModifiers::ALT)
                    && !is_disallowed_control(ch) =>
            {
                Some(UserAction::InsertChar(ch))
            }
            _ => None,
        }
    }
}

/// Observable result of applying a [`UserAction`] to a [`MillerSession`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutcome {
    Continue,
    Confirmed(String),
    Canceled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane2CacheState {
    p1_entry_idx: Option<usize>,
    active_param_set: Option<String>,
    subcmd_chain: Vec<String>,
}

/// Cache-key equality for Pane 2: only the same Pane 1 source index and
/// subcommand depth can reuse the cached entries.
fn pane2_cache_key_matches(
    prev: &Pane2CacheState,
    cur_p1_idx: Option<usize>,
    cur_subcmd_chain: &[String],
) -> bool {
    prev.p1_entry_idx == cur_p1_idx && prev.subcmd_chain == cur_subcmd_chain
}

/// Navigation-context equality for Pane 2: the selected Pane 1 entry identity
/// and subcommand depth are unchanged, even when a catalog refresh shifted the
/// entry's source index. This decides whether the user's Pane 2 cursor can be
/// restored after entries are recomputed.
fn pane2_navigation_context_matches(
    prev_p1_identity: Option<&(String, EntryKind)>,
    cur_p1_identity: Option<&(String, EntryKind)>,
    prev_subcmd_chain: &[String],
    cur_subcmd_chain: &[String],
) -> bool {
    prev_p1_identity == cur_p1_identity
        && prev_p1_identity.is_some()
        && prev_subcmd_chain == cur_subcmd_chain
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane3CacheState {
    p1_entry_idx: Option<usize>,
    p2_entry_idx: Option<usize>,
    pane3_filter: String,
    subcmd_chain: Vec<String>,
}

type CatalogLoader = Box<dyn FnMut(ShellDialect) -> Vec<Entry>>;

#[inline]
fn is_safe_entry_for_emit(entry: &Entry, dialect: ShellDialect) -> bool {
    entry.is_emittable()
        && !has_disallowed_control(&entry.text)
        && (!matches!(entry.kind, EntryKind::Env | EntryKind::Var)
            || is_safe_variable_token(&entry.text, dialect))
}

fn join_command_tokens_cow(pipeline_prefix: Option<&str>, tokens: &[Cow<'_, str>]) -> String {
    let prefix_clean = pipeline_prefix
        .map(strip_disallowed_controls)
        .filter(|s| !s.is_empty());
    let mut cap = prefix_clean.as_ref().map_or(0, |p| p.len() + 1);
    for tok in tokens {
        cap += tok.len() + 1;
    }
    let mut out = String::with_capacity(cap);
    if let Some(ref prefix) = prefix_clean {
        out.push_str(prefix);
    }

    let mut prev_tok: Option<&str> = None;
    for tok in tokens {
        let clean = strip_disallowed_controls(tok.as_ref());
        let tok_str = clean.as_ref();
        if tok_str.is_empty() {
            continue;
        }
        if prev_tok.is_none() {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(tok_str);
            prev_tok = Some(tok.as_ref());
            continue;
        }
        let prev = prev_tok.unwrap_or("");
        let prev_is_param_eq = prev.starts_with('-') && prev.ends_with('=');
        let prev_is_param_colon = prev.starts_with('-') && prev.ends_with(':');
        let is_ps_switch_val = prev.starts_with('-') && tok_str.starts_with(":$");

        if prev_is_param_eq && !tok_str.starts_with('-') {
            out.push_str(tok_str);
        } else if prev_is_param_colon && !tok_str.starts_with('-') {
            if let Some(rest) = tok_str.strip_prefix(':') {
                out.push_str(rest);
            } else {
                out.push_str(tok_str);
            }
        } else if is_ps_switch_val {
            out.push_str(tok_str);
        } else {
            out.push(' ');
            out.push_str(tok_str);
        }
        prev_tok = Some(tok.as_ref());
    }
    out
}

fn split_inline_param_value(token: &str) -> Option<(&str, &str)> {
    if !token.starts_with('-') {
        return None;
    }
    let idx = token.find(['=', ':'])?;
    Some(token.split_at(idx + 1))
}

fn replace_inline_param_value(token: &str, new_val: &str) -> Option<String> {
    let (head, rhs) = split_inline_param_value(token)?;
    if rhs.is_empty() {
        return None;
    }
    let clean_val = if head.ends_with(':') {
        new_val.strip_prefix(':').unwrap_or(new_val)
    } else {
        new_val
    };
    Some(format!("{head}{clean_val}"))
}

fn token_matches_entry(token: &str, entry: &Entry) -> bool {
    token.eq_ignore_ascii_case(&entry.text)
        || entry.matches_parameter_token(token)
        || (entry.kind == EntryKind::Subcmd
            && entry.aliases.iter().any(|a| a.eq_ignore_ascii_case(token)))
}

fn select_best_matching(pane: &mut PaneList, token: &str) -> bool {
    pane.select_first_matching(|it| it.text == token)
        || pane.select_first_matching(|it| it.text.eq_ignore_ascii_case(token))
        || pane.select_first_matching(|it| {
            it.kind == EntryKind::Subcmd && it.aliases.iter().any(|a| a.eq_ignore_ascii_case(token))
        })
        || pane.select_first_matching(|it| it.matches_parameter_token(token))
}

/// Cohesive Command Line assembler owning committed tokens, multi-token initial seeding,
/// multi-level subcommand window chains, pipeline prefixes, and live-cursor preview suppression.
#[derive(Debug, Default, Clone)]
struct CommandLine {
    pipeline_prefix: Option<String>,
    tokens: Vec<String>,
    tokens_rev: u64,
    subcmd_chain: Vec<String>,
    subcmd_token_indices: Vec<usize>,
    pending_pane2_seed: Option<String>,
    pending_pane2_reselect: Option<String>,
    pending_inline_pane3_seed: Option<String>,
    pending_pane3_seeds: VecDeque<(usize, String)>,
    suppress_pane1_preview: bool,
    suppress_pane2_preview: bool,
    suppress_pane3_preview: bool,
}

impl CommandLine {
    fn from_initial_buffer(
        initial_buffer: &str,
        pane1: &mut PaneList,
        dialect: ShellDialect,
    ) -> (Self, Pane) {
        let mut active_pane = Pane::First;
        let sanitized = sanitize_initial_buffer(initial_buffer, dialect);
        let (pipeline_prefix, active_segment) =
            split_pipeline_prefix_for_dialect(sanitized.as_ref(), dialect);
        let tokens = tokenize_shell_words_for_dialect(active_segment, dialect);

        if let Some(first_tok) = tokens.first() {
            let matched = pane1.select_first_matching(|it| {
                it.is_emittable()
                    && it.kind != EntryKind::History
                    && it.text.eq_ignore_ascii_case(first_tok)
            }) || pane1.select_first_matching(|it| {
                it.is_emittable() && it.text.eq_ignore_ascii_case(first_tok)
            });
            if matched {
                active_pane = Pane::Second;
            }
        }

        let (pending_pane2_seed, pending_pane3_seeds) = if active_pane == Pane::Second {
            (
                tokens.get(1).cloned(),
                tokens
                    .iter()
                    .enumerate()
                    .skip(2)
                    .map(|(idx, tok)| (idx, tok.clone()))
                    .collect(),
            )
        } else {
            (None, VecDeque::new())
        };

        let suppress_pane1_preview = active_pane == Pane::First && tokens.len() == 1;

        (
            Self {
                pipeline_prefix,
                tokens,
                tokens_rev: 0,
                subcmd_chain: Vec::new(),
                subcmd_token_indices: Vec::new(),
                pending_pane2_seed,
                pending_pane2_reselect: None,
                pending_inline_pane3_seed: None,
                pending_pane3_seeds,
                suppress_pane1_preview,
                suppress_pane2_preview: false,
                suppress_pane3_preview: false,
            },
            active_pane,
        )
    }

    fn tokens(&self) -> &[String] {
        &self.tokens
    }

    fn push_token_dedup(&mut self, token: String) {
        let clean = strip_disallowed_controls(&token);
        if clean.is_empty() {
            return;
        }
        if self
            .tokens
            .last()
            .is_some_and(|last| last.eq_ignore_ascii_case(clean.as_ref()))
        {
            return;
        }
        self.tokens.push(clean.into_owned());
        self.tokens_rev += 1;
    }

    fn push_pane2_token_dedup(&mut self, p2: &Entry, dialect: ShellDialect) {
        if !is_safe_entry_for_emit(p2, dialect) {
            return;
        }
        if self
            .tokens
            .last()
            .is_some_and(|last| token_matches_entry(last, p2))
        {
            return;
        }
        self.tokens.push(p2.text.clone());
        self.tokens_rev += 1;
    }

    fn replace_last_token(&mut self, token: String) {
        let clean = strip_disallowed_controls(&token);
        if clean.is_empty() {
            return;
        }
        if let Some(last) = self.tokens.last_mut() {
            *last = clean.into_owned();
        } else {
            self.tokens.push(clean.into_owned());
        }
        self.tokens_rev += 1;
    }

    fn ensure_pane1_token(&mut self, p1: Option<&Entry>, dialect: ShellDialect) {
        if let Some(p1) = p1 {
            if is_safe_entry_for_emit(p1, dialect) && self.tokens.is_empty() {
                self.tokens.push(p1.text.clone());
                self.tokens_rev += 1;
            }
        }
    }

    fn commit_pane1_entry(&mut self, p1: &Entry, dialect: ShellDialect) {
        if !is_safe_entry_for_emit(p1, dialect) {
            return;
        }
        if self.tokens.is_empty() {
            self.tokens.push(p1.text.clone());
            self.tokens_rev += 1;
        } else if !self.tokens[0].eq_ignore_ascii_case(&p1.text) {
            self.tokens[0] = p1.text.clone();
            self.tokens.truncate(1);
            self.subcmd_chain.clear();
            self.subcmd_token_indices.clear();
            self.tokens_rev += 1;
        }
    }

    fn commit_pane2_entry(&mut self, p1: Option<&Entry>, p2: &Entry, dialect: ShellDialect) {
        self.ensure_pane1_token(p1, dialect);
        let is_scope_leaf = p1.is_some_and(Entry::is_scope_root) && p2.children.is_empty();
        let allow_commit = !p1.is_some_and(Entry::is_scope_root) || is_scope_leaf;
        if allow_commit && !p2.is_loading() {
            self.push_pane2_token_dedup(p2, dialect);
        }
    }

    fn push_subcmd(&mut self, parent_sub: String, token_idx: usize) {
        self.subcmd_chain.push(parent_sub);
        self.subcmd_token_indices.push(token_idx);
    }

    fn pop_subcmd(&mut self) -> Option<String> {
        self.subcmd_token_indices.pop();
        self.subcmd_chain.pop()
    }

    fn find_subcmd_token_idx(&self, p2: &Entry) -> usize {
        self.tokens
            .iter()
            .rposition(|tok| token_matches_entry(tok, p2))
            .unwrap_or_else(|| self.tokens.len().saturating_sub(1))
    }

    fn min_pane3_tokens(&self) -> usize {
        1 + self.subcmd_token_indices.last().copied().unwrap_or(0)
    }

    fn pop_token(&mut self) -> Option<(String, Option<String>)> {
        self.pending_pane2_seed = None;
        self.pending_pane2_reselect = None;
        self.pending_inline_pane3_seed = None;
        self.pending_pane3_seeds.clear();
        let popped_idx = self.tokens.len().checked_sub(1)?;
        let popped = self.tokens.pop()?;
        self.tokens_rev += 1;
        let popped_sub = if self
            .subcmd_token_indices
            .last()
            .is_some_and(|&idx| popped_idx <= idx)
        {
            self.subcmd_token_indices.pop();
            self.subcmd_chain.pop()
        } else {
            None
        };
        Some((popped, popped_sub))
    }

    fn resume_preview(&mut self, pane: Pane) {
        match pane {
            Pane::First => self.suppress_pane1_preview = false,
            Pane::Second => self.suppress_pane2_preview = false,
            Pane::Third => self.suppress_pane3_preview = false,
        }
    }

    fn assemble(
        &self,
        active_pane: Pane,
        p1: Option<&Entry>,
        p2: Option<&Entry>,
        p3: Option<&Entry>,
        pane3_filter: &str,
        dialect: ShellDialect,
    ) -> String {
        let mut out: Vec<Cow<'_, str>> = Vec::with_capacity(self.tokens.len() + 3);
        for tok in &self.tokens {
            out.push(Cow::Borrowed(tok.as_str()));
        }

        if out.is_empty() {
            if let Some(p1) = p1.filter(|e| is_safe_entry_for_emit(e, dialect)) {
                out.push(Cow::Borrowed(p1.text.as_str()));
            }
        } else if active_pane == Pane::First && out.len() == 1 && !self.suppress_pane1_preview {
            if let Some(p1) = p1.filter(|e| is_safe_entry_for_emit(e, dialect)) {
                out[0] = Cow::Borrowed(p1.text.as_str());
            }
        }

        match active_pane {
            Pane::First => {}
            Pane::Second => {
                if !self.suppress_pane2_preview {
                    if let Some(p2) =
                        p2.filter(|e| !e.is_loading() && is_safe_entry_for_emit(e, dialect))
                    {
                        if !out
                            .last()
                            .is_some_and(|last| token_matches_entry(last.as_ref(), p2))
                        {
                            out.push(Cow::Borrowed(p2.text.as_str()));
                        }
                    }
                }
            }
            Pane::Third => {
                if let (Some(p1), Some(p2)) = (p1, p2) {
                    if !p1.is_scope_root()
                        && !p2.is_loading()
                        && is_safe_entry_for_emit(p2, dialect)
                        && out.len() <= self.min_pane3_tokens()
                        && !out
                            .last()
                            .is_some_and(|last| token_matches_entry(last.as_ref(), p2))
                    {
                        out.push(Cow::Borrowed(p2.text.as_str()));
                    }
                }
                if !self.suppress_pane3_preview {
                    if let Some(v) = resolve_pane3_token_cow(p3, pane3_filter, dialect) {
                        let replaced_inline = p2
                            .filter(|e| matches!(e.kind, EntryKind::Param | EntryKind::Flag))
                            .and_then(|e| {
                                let last = out.last()?;
                                if e.matches_parameter_token(last.as_ref()) {
                                    replace_inline_param_value(last.as_ref(), v.as_ref())
                                } else {
                                    None
                                }
                            });
                        if let Some(updated) = replaced_inline {
                            if let Some(last) = out.last_mut() {
                                *last = Cow::Owned(updated);
                            }
                        } else if !out.last().is_some_and(|last| {
                            last.as_ref().eq_ignore_ascii_case(v.as_ref())
                                || p3.is_some_and(|e| token_matches_entry(last.as_ref(), e))
                        }) {
                            out.push(v);
                        }
                    }
                }
            }
        }

        join_command_tokens_cow(self.pipeline_prefix.as_deref(), &out)
    }
}

#[inline]
fn same_dedup_key(a: &str, b: &str) -> bool {
    if a.starts_with('-') || b.starts_with('-') {
        a == b
    } else {
        a.eq_ignore_ascii_case(b)
    }
}

#[inline]
fn entry_dedup_cow(text: &str) -> Cow<'_, str> {
    if text.starts_with('-') || !text.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Borrowed(text)
    } else {
        Cow::Owned(text.to_ascii_lowercase())
    }
}

fn dedup_entries_in_place(entries: &mut Vec<Entry>) {
    if entries.len() <= 1 {
        return;
    }
    let mut seen: HashSet<String> = HashSet::with_capacity(entries.len());
    entries.retain(|entry| seen.insert(entry_dedup_cow(&entry.text).into_owned()));
}

fn extend_dedup_slice(target: &mut Vec<Entry>, additions: &[Entry], additions_pre_deduped: bool) {
    if additions.is_empty() {
        return;
    }
    if additions_pre_deduped && target.iter().all(|e| e.text.is_empty()) {
        target.extend(additions.iter().cloned());
        return;
    }
    if target.len() <= 24 && additions.len() <= 24 {
        for entry in additions {
            if !target.iter().any(|e| same_dedup_key(&e.text, &entry.text)) {
                target.push(entry.clone());
            }
        }
        return;
    }
    let mut existing: HashSet<String> = target
        .iter()
        .map(|e| entry_dedup_cow(&e.text).into_owned())
        .collect();
    for entry in additions {
        if existing.insert(entry_dedup_cow(&entry.text).into_owned()) {
            target.push(entry.clone());
        }
    }
}

fn extend_dedup_owned(target: &mut Vec<Entry>, additions: Vec<Entry>) {
    if additions.is_empty() {
        return;
    }
    if target.len() <= 24 && additions.len() <= 24 {
        for entry in additions {
            if !target.iter().any(|e| same_dedup_key(&e.text, &entry.text)) {
                target.push(entry);
            }
        }
        return;
    }
    let mut existing: HashSet<String> = target
        .iter()
        .map(|e| entry_dedup_cow(&e.text).into_owned())
        .collect();
    for entry in additions {
        if existing.insert(entry_dedup_cow(&entry.text).into_owned()) {
            target.push(entry);
        }
    }
}

fn expand_runtime_entries_cached(
    children: &[Entry],
    cwd_all: &[Entry],
    cwd_dirs: &[Entry],
) -> Vec<Entry> {
    let mut out = Vec::with_capacity(children.len());
    for child in children {
        if child.is_placeholder() && child.label == "$files" {
            if cwd_all.is_empty() {
                out.push(Entry::placeholder(
                    "(No files in current directory)",
                    EntryKind::Path,
                    "Expanded from spec $files macro",
                ));
            } else {
                out.extend(cwd_all.iter().cloned());
            }
        } else if child.is_placeholder() && child.label == "$directories" {
            if cwd_dirs.is_empty() {
                out.push(Entry::placeholder(
                    "(No directories in current directory)",
                    EntryKind::Path,
                    "Expanded from spec $directories macro",
                ));
            } else {
                out.extend(cwd_dirs.iter().cloned());
            }
        } else {
            out.push(child.clone());
        }
    }
    out
}

fn history_command_chain(cmd: &str) -> Vec<&str> {
    let (_, segment) = split_pipeline_prefix(cmd);
    segment
        .split_whitespace()
        .take_while(|tok| {
            !tok.starts_with('-') && !matches!(*tok, "|" | "||" | "&&" | ";" | ">" | "<" | ">>")
        })
        .take(2)
        .collect()
}

fn primary_exe_for_entry(entry: &Entry) -> &str {
    if entry.kind == EntryKind::History {
        let (_, segment) = split_pipeline_prefix(entry.text.as_str());
        segment
            .split_whitespace()
            .next()
            .unwrap_or(entry.text.as_str())
    } else {
        entry.text.as_str()
    }
}

fn is_git_branch_target(exe: &str, p2: &Entry) -> bool {
    if !exe.eq_ignore_ascii_case("git") {
        return false;
    }
    let t = p2.text.as_str();
    t == "-B"
        || [
            "checkout",
            "switch",
            "branch",
            "merge",
            "rebase",
            "diff",
            "log",
            "push",
            "pull",
            "reset",
            "cherry-pick",
            "show",
            "tag",
            "-b",
            "--track",
        ]
        .iter()
        .any(|kw| t.eq_ignore_ascii_case(kw))
}

fn resolve_pane2_entries<I: Introspect>(
    pane1_entry: Option<&Entry>,
    tokens: &[String],
    subcmd_chain: &[String],
    introspector: &mut I,
) -> (Vec<Entry>, Option<String>) {
    let Some(p1) = pane1_entry else {
        return (Vec::new(), None);
    };

    if p1.is_loading() {
        return (Vec::new(), None);
    }

    if !subcmd_chain.is_empty() {
        if let Some(nested) = p1.walk_chain(subcmd_chain) {
            if !nested.children.is_empty() {
                return (nested.children.clone(), None);
            }
        }
        let p1_exe = primary_exe_for_entry(p1);
        let mut chain: Vec<&str> = Vec::with_capacity(1 + subcmd_chain.len());
        chain.push(p1_exe);
        chain.extend(subcmd_chain.iter().map(String::as_str));
        return (introspector.query(&chain), None);
    }

    if !p1.children.is_empty() {
        return p1.children_for_tokens(tokens);
    }

    if p1.is_scope_root() {
        return (Vec::new(), None);
    }

    if p1.kind == EntryKind::History {
        let chain = history_command_chain(&p1.text);
        if chain.is_empty() {
            return (Vec::new(), None);
        }
        return (introspector.query(&chain), None);
    }

    (introspector.query(&[p1.text.as_str()]), None)
}

struct SessionRuntimeCache<'a> {
    dialect: ShellDialect,
    common_vars: &'a [Entry],
    cwd_all: &'a [Entry],
    cwd_dirs: &'a [Entry],
    git_branches: &'a [Entry],
}

fn sanitize_scope_children(entries: &mut Vec<Entry>, dialect: ShellDialect, in_var_scope: bool) {
    let mut kept = Vec::with_capacity(entries.len());
    for mut entry in std::mem::take(entries) {
        if has_disallowed_control(&entry.text) {
            continue;
        }
        match entry.kind {
            EntryKind::Env | EntryKind::Var if !entry.is_scope_root() => {
                let Some(safe_tok) = sanitize_variable_token(&entry.text, dialect) else {
                    continue;
                };
                entry.text = safe_tok;
                sanitize_scope_children(&mut entry.children, dialect, true);
                kept.push(entry);
            }
            EntryKind::Value if in_var_scope => {
                let Some(safe_val) = sanitize_literal_value_text(&entry.text, dialect) else {
                    continue;
                };
                entry.text = safe_val;
                kept.push(entry);
            }
            EntryKind::Path if !entry.is_scope_root() => {
                if entry.text.starts_with('-') {
                    let prefixed = format!("./{}", entry.text);
                    entry.text = quote_literal_for_dialect(&prefixed, Some(dialect), true);
                } else {
                    let already_safe_double = entry.text.len() >= 2
                        && entry.text.starts_with('"')
                        && entry.text.ends_with('"')
                        && {
                            let inner = &entry.text[1..entry.text.len() - 1];
                            !inner.is_empty()
                                && !inner.starts_with('-')
                                && !inner.contains('"')
                                && !inner.contains('\\')
                                && inner.chars().all(|ch| {
                                    (ch.is_whitespace() && !is_disallowed_control(ch))
                                        || is_safe_unquoted_char(ch, false)
                                })
                        };
                    if !already_safe_double
                        && !is_valid_single_quoted(&entry.text, dialect)
                        && !is_safe_unquoted_token(&entry.text, Some(dialect))
                    {
                        entry.text = quote_literal_for_dialect(&entry.text, Some(dialect), true);
                    }
                }
                kept.push(entry);
            }
            _ => {
                sanitize_scope_children(&mut entry.children, dialect, in_var_scope);
                kept.push(entry);
            }
        }
    }
    *entries = kept;
}

fn sanitize_catalog_entries(
    entries: &mut Vec<Entry>,
    dialect: ShellDialect,
    runtime_ctx: &RuntimeContext,
) {
    let mut i = 0;
    while i < entries.len() {
        let entry = &mut entries[i];
        if entry.is_scope_root() {
            if entry.text == "./" && !runtime_ctx.cwd_entries.is_empty() {
                entry.children = runtime_ctx.cwd_path_entries_for_dialect(false, dialect);
            } else {
                let in_var_scope = matches!(entry.kind, EntryKind::Env | EntryKind::Var);
                sanitize_scope_children(&mut entry.children, dialect, in_var_scope);
            }
            i += 1;
            continue;
        }
        if matches!(entry.kind, EntryKind::Env | EntryKind::Var) {
            if let Some(safe_tok) = sanitize_variable_token(&entry.text, dialect) {
                entry.text = safe_tok;
                sanitize_scope_children(&mut entry.children, dialect, true);
                i += 1;
            } else {
                entries.swap_remove(i);
            }
            continue;
        }
        if has_disallowed_control(&entry.text) {
            let cleaned = strip_disallowed_controls(&entry.text);
            if cleaned.trim().is_empty() {
                entries.swap_remove(i);
                continue;
            }
            entry.text = cleaned.into_owned();
        }
        i += 1;
    }
}

fn resolve_pane3_entries<I: Introspect>(
    pane1_entry: Option<&Entry>,
    pane2_entry: Option<&Entry>,
    subcmd_chain: &[String],
    introspector: &mut I,
    pane3_filter: &str,
    cache: &SessionRuntimeCache<'_>,
) -> Vec<Entry> {
    let (Some(p1), Some(p2)) = (pane1_entry, pane2_entry) else {
        return Vec::new();
    };

    if p2.is_loading() {
        return Vec::new();
    }

    let p1_exe = primary_exe_for_entry(p1);
    let mut entries = Vec::new();
    let cleaned_filter = strip_disallowed_controls(pane3_filter);
    let trimmed_input = cleaned_filter.trim();

    if !trimmed_input.is_empty() {
        let token = format_custom_value_token(trimmed_input, cache.dialect);
        if !token.is_empty() {
            entries.push(Entry::new(
                token,
                format!("✏ Use \"{trimmed_input}\""),
                EntryKind::Value,
                format!("Insert custom value for {}", p2.label),
            ));
        }
    } else if matches!(p2.kind, EntryKind::Param | EntryKind::Flag) && p2.children.is_empty() {
        entries.push(Entry::placeholder(
            "✏ (Type custom value / path)",
            EntryKind::Value,
            format!(
                "Type directly in this pane to specify an argument for {}",
                p2.label
            ),
        ));
    }

    if !p2.children.is_empty() {
        if is_git_branch_target(p1_exe, p2) {
            extend_dedup_slice(&mut entries, cache.git_branches, true);
        }
        extend_dedup_owned(
            &mut entries,
            expand_runtime_entries_cached(&p2.children, cache.cwd_all, cache.cwd_dirs),
        );
        if matches!(p2.kind, EntryKind::Param | EntryKind::Flag) {
            extend_dedup_slice(&mut entries, cache.common_vars, true);
        }
        return entries;
    }

    if p1.is_scope_root() && p2.children.is_empty() {
        if entries.is_empty() && is_safe_entry_for_emit(p2, cache.dialect) {
            entries.push(Entry::new(
                p2.text.clone(),
                format!("Insert {}", p2.text),
                p2.kind,
                p2.desc.clone(),
            ));
        }
        return entries;
    }

    if p2.kind == EntryKind::Subcmd {
        let mut chain: Vec<&str> = Vec::with_capacity(2 + subcmd_chain.len());
        chain.push(p1_exe);
        chain.extend(subcmd_chain.iter().map(String::as_str));
        chain.push(p2.text.as_str());
        let sub_entries = introspector.query(&chain);
        if sub_entries.iter().any(Entry::is_loading) {
            entries.extend(sub_entries);
            return entries;
        }
        if is_git_branch_target(p1_exe, p2) {
            extend_dedup_slice(&mut entries, cache.git_branches, true);
        }
        extend_dedup_owned(&mut entries, sub_entries);
        extend_dedup_slice(&mut entries, cache.cwd_all, true);
        return entries;
    }

    if !p1.is_scope_root() {
        if is_git_branch_target(p1_exe, p2) {
            extend_dedup_slice(&mut entries, cache.git_branches, true);
        }
        if p2.kind == EntryKind::Param {
            extend_dedup_slice(&mut entries, cache.cwd_all, true);
        }
    }

    extend_dedup_slice(&mut entries, cache.common_vars, true);
    entries
}

fn subcommand_has_pane2_children<I: Introspect>(
    p1_opt: Option<&Entry>,
    p2: &Entry,
    subcmd_chain: &[String],
    introspector: &mut I,
) -> bool {
    if !p2.children.is_empty() {
        return true;
    }
    let Some(p1) = p1_opt else {
        return false;
    };
    if p1
        .walk_chain(subcmd_chain)
        .and_then(|parent| {
            parent
                .children
                .iter()
                .find(|c| c.text.eq_ignore_ascii_case(&p2.text))
        })
        .is_some_and(|e| !e.children.is_empty())
    {
        return true;
    }
    let p1_exe = primary_exe_for_entry(p1);
    let mut exe_chain: Vec<&str> = Vec::with_capacity(2 + subcmd_chain.len());
    exe_chain.push(p1_exe);
    exe_chain.extend(subcmd_chain.iter().map(String::as_str));
    exe_chain.push(p2.text.as_str());
    let queried = introspector.query(&exe_chain);
    !queried.is_empty() && !queried.iter().any(Entry::is_loading)
}

fn resolve_pane3_token_cow<'a>(
    cur_p3: Option<&'a Entry>,
    pane3_filter: &str,
    dialect: ShellDialect,
) -> Option<Cow<'a, str>> {
    let cleaned_filter = strip_disallowed_controls(pane3_filter);
    let trimmed_filter = cleaned_filter.trim();
    if let Some(p3) = cur_p3 {
        if p3.is_loading() {
            None
        } else if is_safe_entry_for_emit(p3, dialect) {
            Some(Cow::Borrowed(p3.text.as_str()))
        } else if !trimmed_filter.is_empty() {
            let tok = format_custom_value_token(trimmed_filter, dialect);
            (!tok.is_empty()).then_some(Cow::Owned(tok))
        } else {
            None
        }
    } else if !trimmed_filter.is_empty() {
        let tok = format_custom_value_token(trimmed_filter, dialect);
        (!tok.is_empty()).then_some(Cow::Owned(tok))
    } else {
        None
    }
}

fn resolve_pane3_token(
    cur_p3: Option<&Entry>,
    pane3_filter: &str,
    dialect: ShellDialect,
) -> Option<String> {
    resolve_pane3_token_cow(cur_p3, pane3_filter, dialect).map(Cow::into_owned)
}

/// Deep interactive session state machine owning the 3-pane Miller catalogs,
/// per-pane fuzzy filters, selection/scroll cursors, unified Command Line
/// assembly/seeding state, and active PowerShell Parameter Set metadata.
pub struct MillerSession<I: Introspect = CliIntrospector> {
    panes: [PaneList; 3],
    pane2_cache_state: Option<Pane2CacheState>,
    /// Pane 2 entries need recomputation even though its navigation context
    /// (Pane 1 entry and subcommand depth) is unchanged.
    pane2_cache_stale: bool,
    pane3_cache_state: Option<Pane3CacheState>,
    pane2_tokens_rev: u64,
    last_p1_identity: Option<(String, EntryKind)>,
    last_p2_text: Option<String>,
    skip_child_filter_clear_once: bool,
    introspector: I,
    catalog_loader: Option<CatalogLoader>,
    runtime_ctx: RuntimeContext,
    cached_common_vars: Option<Vec<Entry>>,
    cached_cwd_all: Vec<Entry>,
    cached_cwd_dirs: Vec<Entry>,
    cached_git_branches: Vec<Entry>,
    initial_buffer: String,
    active_pane: Pane,
    cmd: CommandLine,
    active_param_set_label: Option<String>,
    viewport: Viewport,
    dialect: ShellDialect,
    inspector_expanded: bool,
    dirty: bool,
}

impl<I: Introspect> MillerSession<I> {
    pub fn new(
        mut pane1_entries: Vec<Entry>,
        introspector: I,
        initial_buffer: &str,
        viewport: Viewport,
        dialect: ShellDialect,
    ) -> Self {
        let runtime_ctx = RuntimeContext::default();
        sanitize_catalog_entries(&mut pane1_entries, dialect, &runtime_ctx);
        let mut pane1 = PaneList::new(pane1_entries);
        let (cmd, active_pane) =
            CommandLine::from_initial_buffer(initial_buffer, &mut pane1, dialect);

        let mut session = Self {
            panes: [pane1, PaneList::default(), PaneList::default()],
            pane2_cache_state: None,
            pane2_cache_stale: false,
            pane3_cache_state: None,
            pane2_tokens_rev: u64::MAX,
            last_p1_identity: None,
            last_p2_text: None,
            skip_child_filter_clear_once: false,
            introspector,
            catalog_loader: None,
            runtime_ctx,
            cached_common_vars: None,
            cached_cwd_all: Vec::new(),
            cached_cwd_dirs: Vec::new(),
            cached_git_branches: Vec::new(),
            initial_buffer: initial_buffer.to_string(),
            active_pane,
            cmd,
            active_param_set_label: None,
            viewport,
            dialect,
            inspector_expanded: false,
            dirty: true,
        };
        session.sync_panes();
        session
    }

    fn ensure_common_vars_cached(&mut self) {
        if self.cached_common_vars.is_none() {
            let mut vars = self.dialect.common_var_entries();
            dedup_entries_in_place(&mut vars);
            self.cached_common_vars = Some(vars);
        }
    }

    fn rebuild_runtime_caches(&mut self) {
        let mut branches = self
            .runtime_ctx
            .git_branch_entries_for_dialect(self.dialect);
        dedup_entries_in_place(&mut branches);
        self.cached_git_branches = branches;

        let mut cwd_all = self
            .runtime_ctx
            .cwd_path_entries_for_dialect(false, self.dialect);
        dedup_entries_in_place(&mut cwd_all);
        self.cached_cwd_all = cwd_all;

        let mut cwd_dirs = self
            .runtime_ctx
            .cwd_path_entries_for_dialect(true, self.dialect);
        dedup_entries_in_place(&mut cwd_dirs);
        self.cached_cwd_dirs = cwd_dirs;
    }

    /// Attaches dynamic runtime context (Git branch, CWD files, shell history)
    /// used to enrich Pane 2 and Pane 3 completions.
    pub fn with_runtime_context(mut self, runtime_ctx: RuntimeContext) -> Self {
        self.runtime_ctx = runtime_ctx;
        self.rebuild_runtime_caches();

        if !self.runtime_ctx.cwd_entries.is_empty()
            && self.panes[0]
                .entries()
                .iter()
                .any(|e| e.is_scope_root() && e.text == "./")
        {
            let mut pane1_entries = self.panes[0].entries().to_vec();
            let prev_p1 = self.selected_entry(Pane::First).map(|e| e.text.clone());
            for entry in &mut pane1_entries {
                if entry.is_scope_root() && entry.text == "./" {
                    entry.children = self.cached_cwd_all.clone();
                }
            }
            self.panes[0].set_entries(pane1_entries);
            if let Some(txt) = prev_p1 {
                self.panes[0].select_first_matching(|e| e.text == txt);
            }
        }

        if !self.initial_buffer.is_empty() {
            self.panes[0].clear_filter();
            self.panes[0].reset_cursor();
            self.panes[1].clear_filter();
            self.panes[2].clear_filter();
            let (cmd, active_pane) = CommandLine::from_initial_buffer(
                &self.initial_buffer,
                &mut self.panes[0],
                self.dialect,
            );
            self.cmd = cmd;
            self.active_pane = active_pane;
        }
        self.pane2_cache_state = None;
        self.pane3_cache_state = None;
        self.last_p1_identity = None;
        self.last_p2_text = None;
        self.sync_panes();
        self
    }

    /// Configures a catalog loader invoked on [`UserAction::ReloadCatalog`].
    pub fn with_catalog_loader<F>(mut self, loader: F) -> Self
    where
        F: FnMut(ShellDialect) -> Vec<Entry> + 'static,
    {
        self.catalog_loader = Some(Box::new(loader));
        self
    }

    /// Polls asynchronous introspection and background metadata updates, refreshing
    /// affected panes live while preserving user cursor/filter state where possible.
    pub fn poll_discovery(&mut self) -> bool {
        let poll = self.introspector.poll_updates();
        if !poll.any() {
            return false;
        }
        if let Some(entries) = poll.catalog_reloaded {
            self.apply_catalog_update(entries);
        }
        if poll.introspected {
            self.invalidate_loading_panes();
        }
        self.sync_panes();
        self.dirty = true;
        true
    }

    fn apply_catalog_update(&mut self, mut entries: Vec<Entry>) {
        sanitize_catalog_entries(&mut entries, self.dialect, &self.runtime_ctx);
        let prev_p1 = self
            .selected_entry(Pane::First)
            .map(|e| (e.text.clone(), e.kind));
        self.panes[0].set_entries(entries);
        let matched_prev = prev_p1.as_ref().is_some_and(|(txt, kind)| {
            self.panes[0].select_first_matching(|e| e.text == *txt && e.kind == *kind)
        });
        if !matched_prev {
            let matched_seed = self.cmd.tokens().first().is_some_and(|first_tok| {
                self.panes[0].select_first_matching(|e| e.text.eq_ignore_ascii_case(first_tok))
            });
            if !matched_seed {
                self.panes[0].reset_cursor();
            }
        }
        let next_p1 = self
            .selected_entry(Pane::First)
            .map(|e| (e.text.clone(), e.kind));
        if prev_p1 == next_p1 && prev_p1.is_some() {
            self.skip_child_filter_clear_once = true;
        } else {
            self.cmd.subcmd_chain.clear();
            self.cmd.subcmd_token_indices.clear();
            self.skip_child_filter_clear_once = false;
        }
        // The Pane 1 selection identity drives whether Pane 2's navigation
        // context changed. Mark the cached entries stale instead of dropping
        // `pane2_cache_state`: the cache is also the record of that context,
        // and losing it made every live refresh reset the Pane 2 cursor.
        self.pane2_cache_stale = true;
        self.pane3_cache_state = None;
    }

    fn invalidate_loading_panes(&mut self) {
        let pane2_entries = self.panes[1].entries();
        if pane2_entries.is_empty() || pane2_entries.iter().any(Entry::is_loading) {
            self.pane2_cache_stale = true;
        }
        // Only refresh Pane 3 when it is still waiting on background data.
        // An unrelated cache completion must not reset the user's Pane 3 cursor.
        let pane3_entries = self.panes[2].entries();
        if pane3_entries.is_empty() || pane3_entries.iter().any(Entry::is_loading) {
            self.pane3_cache_state = None;
        }
    }

    /// Flushes the underlying [`Introspect`] cache to disk if dirty.
    pub fn flush_introspector(&mut self) -> bool {
        self.introspector.flush()
    }

    /// Translates and applies a raw `crossterm` event.
    pub fn handle_terminal_event(&mut self, ev: Event) -> SessionOutcome {
        match ev {
            Event::Resize(w, h) => {
                let next = self.viewport.resized(w, h);
                self.apply(UserAction::Resize(next.width, next.height))
            }
            Event::Key(key) => {
                if let Some(action) = UserAction::from_key_event(key) {
                    self.apply(action)
                } else {
                    SessionOutcome::Continue
                }
            }
            _ => SessionOutcome::Continue,
        }
    }

    /// Applies a semantic [`UserAction`] to the session state machine.
    pub fn apply(&mut self, action: UserAction) -> SessionOutcome {
        self.sync_panes();

        match action {
            UserAction::Cancel => {
                if self.inspector_expanded {
                    self.inspector_expanded = false;
                    self.dirty = true;
                    return SessionOutcome::Continue;
                }
                return SessionOutcome::Canceled;
            }
            UserAction::Confirm => return SessionOutcome::Confirmed(self.command_line()),
            UserAction::ToggleInspector => {
                self.inspector_expanded = !self.inspector_expanded;
                self.dirty = true;
                return SessionOutcome::Continue;
            }
            UserAction::Resize(w, h) => {
                self.viewport = Viewport::new(w, h);
                self.clamp_scroll();
                self.dirty = true;
                return SessionOutcome::Continue;
            }
            UserAction::ReloadCatalog => {
                if let Some(p1) = self.selected_entry(Pane::First) {
                    let exe = primary_exe_for_entry(p1).to_string();
                    self.introspector.invalidate_command(&exe);
                }
                self.cached_common_vars = None;
                if let Some(loader) = self.catalog_loader.as_mut() {
                    let entries = loader(self.dialect);
                    self.apply_catalog_update(entries);
                } else {
                    self.pane2_cache_state = None;
                    self.pane3_cache_state = None;
                }
                self.sync_panes();
                self.dirty = true;
                return SessionOutcome::Continue;
            }
            UserAction::MoveUp
            | UserAction::MoveDown
            | UserAction::PageUp
            | UserAction::PageDown
            | UserAction::MoveHome
            | UserAction::MoveEnd => self.apply_navigation(&action),
            UserAction::StepRight => self.step_right(),
            UserAction::StepLeft => self.step_left(),
            UserAction::AcceptToken => self.accept_token(),
            UserAction::Backspace => self.handle_backspace(),
            UserAction::InsertChar(ch) => {
                if !is_disallowed_control(ch) {
                    self.active_pane_mut().push_filter_char(ch);
                    self.finish_filter_edit();
                }
            }
            UserAction::ClearFilter => {
                if self.active_pane_mut().clear_filter() {
                    self.finish_filter_edit();
                }
            }
        }

        if self.dirty {
            self.sync_panes();
        }
        SessionOutcome::Continue
    }

    fn apply_navigation(&mut self, action: &UserAction) {
        self.cmd.resume_preview(self.active_pane);
        let page_rows = self.viewport.rows(self.active_pane);
        let pane = self.active_pane_mut();
        let moved = match action {
            UserAction::MoveHome => {
                pane.move_home();
                true
            }
            UserAction::MoveUp => pane.move_up(),
            UserAction::MoveDown => pane.move_down(),
            UserAction::PageUp => pane.page_up(page_rows),
            UserAction::PageDown => pane.page_down(page_rows),
            UserAction::MoveEnd => pane.move_end(),
            _ => false,
        };
        if moved {
            self.dirty = true;
        }
    }

    fn slide_window_into_subcmd(
        &mut self,
        parent_sub: String,
        parent_idx: usize,
        next_sub: String,
        suppress_pane3: bool,
    ) {
        self.cmd.push_subcmd(parent_sub, parent_idx);
        self.panes[1].clear_filter();
        self.panes[2].clear_filter();
        self.cmd.pending_pane2_seed = Some(next_sub);
        self.cmd.suppress_pane2_preview = false;
        self.cmd.suppress_pane3_preview = suppress_pane3;
        self.active_pane = Pane::Second;
        self.pane2_cache_state = None;
        self.pane3_cache_state = None;
        self.dirty = true;
    }

    fn step_right(&mut self) {
        let dialect = self.dialect;
        match self.active_pane {
            Pane::First => {
                if let Some(p1) = self.panes[0].selected() {
                    if !p1.is_loading() {
                        self.cmd.commit_pane1_entry(p1, dialect);
                        self.active_pane = Pane::Second;
                        self.cmd.resume_preview(Pane::Second);
                        self.dirty = true;
                    }
                }
            }
            Pane::Second => {
                let p1_opt = self.panes[0].selected();
                if let Some(p2) = self.panes[1].selected() {
                    if !p2.is_loading() {
                        self.cmd.commit_pane2_entry(p1_opt, p2, dialect);
                        self.active_pane = Pane::Third;
                        self.cmd.resume_preview(Pane::Second);
                        self.cmd.resume_preview(Pane::Third);
                        self.dirty = true;
                    }
                }
            }
            Pane::Third => {
                if let (Some(p1), Some(p2), Some(p3)) = (
                    self.panes[0].selected(),
                    self.panes[1].selected(),
                    self.panes[2].selected(),
                ) {
                    let can_slide_p3 = p3.kind == EntryKind::Subcmd
                        || p3.kind == EntryKind::Param
                        || (p3.kind == EntryKind::Flag && !p3.commits_without_value());
                    if p2.kind == EntryKind::Subcmd
                        && can_slide_p3
                        && p3.is_emittable()
                        && is_safe_entry_for_emit(p3, dialect)
                        && !p3.is_loading()
                    {
                        self.cmd.commit_pane2_entry(Some(p1), p2, dialect);
                        let parent_idx = self.cmd.find_subcmd_token_idx(p2);
                        self.cmd.push_token_dedup(p3.text.clone());
                        self.cmd.pending_pane3_seeds.clear();
                        let p2_text = p2.text.clone();
                        let p3_text = p3.text.clone();
                        self.slide_window_into_subcmd(p2_text, parent_idx, p3_text, false);
                    }
                }
            }
        }
    }

    fn step_left(&mut self) {
        match self.active_pane {
            Pane::Third => {
                self.active_pane = Pane::Second;
                self.cmd.resume_preview(Pane::Second);
                self.cmd.resume_preview(Pane::Third);
                self.dirty = true;
            }
            Pane::Second => {
                if let Some(prev_sub) = self.cmd.pop_subcmd() {
                    self.panes[1].clear_filter();
                    self.panes[2].clear_filter();
                    self.cmd.pending_pane2_seed = None;
                    self.cmd.pending_pane2_reselect = Some(prev_sub.clone());
                    self.cmd.suppress_pane2_preview = true;
                    self.pane2_cache_state = None;
                    self.pane3_cache_state = None;
                    self.sync_panes();
                    if self.panes[1]
                        .select_first_matching(|it| it.text.eq_ignore_ascii_case(&prev_sub))
                    {
                        self.cmd.pending_pane2_reselect = None;
                    }
                    self.pane3_cache_state = None;
                    self.sync_panes();
                } else {
                    self.active_pane = Pane::First;
                    self.cmd.resume_preview(Pane::Second);
                }
                self.dirty = true;
            }
            Pane::First => {}
        }
    }

    fn accept_token(&mut self) {
        match self.active_pane {
            Pane::First => {
                let dialect = self.dialect;
                if let Some(p1) = self.panes[0].selected() {
                    if !p1.is_loading() {
                        self.cmd.commit_pane1_entry(p1, dialect);
                        if !self.panes[0].filter().is_empty() {
                            let p1_text = p1.text.clone();
                            let p1_kind = p1.kind;
                            if self.panes[0].clear_filter() {
                                self.panes[0].select_first_matching(|it| {
                                    it.text == p1_text && it.kind == p1_kind
                                });
                            }
                        }
                        self.active_pane = Pane::Second;
                        self.cmd.resume_preview(Pane::Second);
                        self.dirty = true;
                    }
                }
            }
            Pane::Second => {
                // A suppressed Pane 2 preview means the reset index-0 entry was
                // never shown; Tab is a no-op until navigation or typing clears it.
                if self.cmd.suppress_pane2_preview {
                    return;
                }
                let dialect = self.dialect;
                let p1_opt = self.panes[0].selected();
                let Some(p2) = self.panes[1].selected() else {
                    return;
                };
                if p2.is_loading() || !p2.is_emittable() {
                    return;
                }

                let commit_without_value = p2.commits_without_value()
                    || p1_opt.is_some_and(|p1| p1.is_scope_root() && p2.children.is_empty());
                self.cmd.commit_pane2_entry(p1_opt, p2, dialect);
                if commit_without_value {
                    self.panes[1].clear_filter();
                    self.panes[1].reset_cursor();
                    self.cmd.suppress_pane2_preview = true;
                } else {
                    self.active_pane = Pane::Third;
                    self.panes[2].reset_cursor();
                    self.cmd.resume_preview(Pane::Second);
                    self.cmd.resume_preview(Pane::Third);
                }
                self.dirty = true;
            }
            Pane::Third => {
                let p1_opt = self.panes[0].selected();
                let p2_opt = self.panes[1].selected();
                let p3_opt = self.panes[2].selected();

                if let Some(p2) = p2_opt.filter(|e| e.kind == EntryKind::Subcmd) {
                    let slide_into_p3 = p3_opt.is_some_and(|p3| {
                        !p3.is_loading()
                            && p3.is_emittable()
                            && (p3.kind == EntryKind::Subcmd
                                || (matches!(p3.kind, EntryKind::Param | EntryKind::Flag)
                                    && !p3.commits_without_value()
                                    && !self.cmd.tokens().last().is_some_and(|tok| {
                                        token_matches_entry(tok, p3)
                                            && split_inline_param_value(tok)
                                                .is_some_and(|(_, rhs)| !rhs.is_empty())
                                    })))
                    });
                    if slide_into_p3 {
                        if let Some(p3) = p3_opt {
                            let dialect = self.dialect;
                            if is_safe_entry_for_emit(p3, dialect) {
                                self.cmd.commit_pane2_entry(p1_opt, p2, dialect);
                                let parent_idx = self.cmd.find_subcmd_token_idx(p2);
                                self.cmd.push_token_dedup(p3.text.clone());
                                self.cmd.pending_pane3_seeds.clear();
                                let p2_text = p2.text.clone();
                                let p3_text = p3.text.clone();
                                self.slide_window_into_subcmd(p2_text, parent_idx, p3_text, false);
                            }
                            return;
                        }
                    }
                }

                let pane3_filter = self.panes[2].filter();
                let val_to_add = resolve_pane3_token(p3_opt, pane3_filter, self.dialect);
                let is_flag_placeholder = p3_opt
                    .is_some_and(|p3| p3.is_placeholder() && p3.kind == EntryKind::Flag)
                    && pane3_filter.trim().is_empty();

                if val_to_add.is_none() && !is_flag_placeholder {
                    return;
                }

                if let (Some(p2), Some(val)) = (p2_opt, val_to_add.as_ref()) {
                    let inline_updated = matches!(p2.kind, EntryKind::Param | EntryKind::Flag)
                        .then(|| {
                            let last = self.cmd.tokens().last()?;
                            if p2.matches_parameter_token(last) {
                                replace_inline_param_value(last, val)
                            } else {
                                None
                            }
                        })
                        .flatten();
                    if let Some(updated) = inline_updated {
                        self.cmd.replace_last_token(updated);
                        self.panes[2].clear_filter();
                        self.panes[1].clear_filter();
                        self.active_pane = Pane::Second;
                        self.panes[1].reset_cursor();
                        self.cmd.suppress_pane2_preview = true;
                        self.cmd.resume_preview(Pane::Third);
                        self.dirty = true;
                        return;
                    }
                }

                let dialect = self.dialect;
                if let Some(p2) = p2_opt {
                    let should_slide_subcmd = p2.kind == EntryKind::Subcmd
                        && subcommand_has_pane2_children(
                            p1_opt,
                            p2,
                            &self.cmd.subcmd_chain,
                            &mut self.introspector,
                        );
                    self.cmd.commit_pane2_entry(p1_opt, p2, dialect);
                    if let Some(val) = val_to_add {
                        self.cmd.push_token_dedup(val);
                    }
                    if p2.kind == EntryKind::Subcmd {
                        let p2_slide = if should_slide_subcmd {
                            let parent_idx = self.cmd.find_subcmd_token_idx(p2);
                            Some((p2.text.clone(), parent_idx))
                        } else {
                            None
                        };
                        self.panes[2].clear_filter();
                        self.cmd.resume_preview(Pane::Third);
                        if let Some((p2_text, parent_idx)) = p2_slide {
                            self.cmd.push_subcmd(p2_text, parent_idx);
                            self.panes[1].clear_filter();
                            self.panes[1].reset_cursor();
                            self.active_pane = Pane::Second;
                            self.cmd.suppress_pane2_preview = true;
                            self.pane2_cache_state = None;
                            self.pane3_cache_state = None;
                        } else {
                            self.panes[2].reset_cursor();
                            self.cmd.suppress_pane3_preview = true;
                        }
                        self.dirty = true;
                        return;
                    }
                } else {
                    self.cmd.ensure_pane1_token(p1_opt, dialect);
                    if let Some(val) = val_to_add {
                        self.cmd.push_token_dedup(val);
                    }
                }
                self.panes[2].clear_filter();
                self.panes[1].clear_filter();
                self.active_pane = Pane::Second;
                self.panes[1].reset_cursor();
                self.cmd.suppress_pane2_preview = true;
                self.cmd.resume_preview(Pane::Third);
                self.dirty = true;
            }
        }
    }

    fn handle_backspace(&mut self) {
        if self.active_pane_mut().pop_filter_char() {
            self.finish_filter_edit();
        } else if let Some((_popped, depth_popped)) = self.cmd.pop_token() {
            if let Some(prev_sub) = depth_popped {
                self.panes[1].clear_filter();
                self.panes[2].clear_filter();
                self.pane2_cache_state = None;
                self.pane3_cache_state = None;
                self.active_pane = Pane::Second;
                self.cmd.pending_pane2_reselect = Some(prev_sub.clone());
                self.cmd.suppress_pane2_preview = true;
                self.sync_panes();
                if self.panes[1].select_first_matching(|it| it.text.eq_ignore_ascii_case(&prev_sub))
                {
                    self.cmd.pending_pane2_reselect = None;
                }
                self.pane3_cache_state = None;
            } else {
                let min_pane3_tokens = self.cmd.min_pane3_tokens();
                if self.active_pane == Pane::Third && self.cmd.tokens().len() <= min_pane3_tokens {
                    self.active_pane = Pane::Second;
                    self.cmd.suppress_pane2_preview = true;
                    self.cmd.resume_preview(Pane::Third);
                } else if self.active_pane == Pane::Third {
                    self.cmd.suppress_pane3_preview = true;
                } else if self.active_pane == Pane::Second {
                    if self.cmd.tokens().is_empty() {
                        self.active_pane = Pane::First;
                        self.cmd.resume_preview(Pane::Second);
                    } else {
                        self.cmd.suppress_pane2_preview = true;
                    }
                }
            }
            self.dirty = true;
        } else if self.cmd.pipeline_prefix.take().is_some() {
            self.active_pane = Pane::First;
            self.cmd.resume_preview(Pane::Second);
            self.dirty = true;
        } else if self.active_pane != Pane::First {
            self.active_pane = match self.active_pane {
                Pane::Third => Pane::Second,
                Pane::Second | Pane::First => Pane::First,
            };
            self.cmd.resume_preview(Pane::Second);
            self.cmd.resume_preview(Pane::Third);
            self.dirty = true;
        }
    }

    /// Assembles the current live Command Line string from Committed Tokens and cursor preview.
    pub fn command_line(&self) -> String {
        self.cmd.assemble(
            self.active_pane,
            self.selected_entry(Pane::First),
            self.selected_entry(Pane::Second),
            self.selected_entry(Pane::Third),
            self.panes[2].filter(),
            self.dialect,
        )
    }

    /// Returns a read-only projection of session state for rendering and UI-decoupled assertions.
    pub fn view(&self) -> RenderState<'_> {
        RenderState {
            viewport: self.viewport,
            active_pane: self.active_pane,
            panes: [&self.panes[0], &self.panes[1], &self.panes[2]],
            command_line: self.command_line(),
            active_param_set: self.active_param_set_label.as_deref(),
            dialect: self.dialect,
            inspector_expanded: self.inspector_expanded,
        }
    }

    /// Renders a complete terminal frame for the current session state and clears the dirty flag.
    pub fn render(&mut self) -> String {
        self.sync_panes();
        let frame = render_frame(&self.view());
        self.dirty = false;
        frame
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    fn selected_entry(&self, pane: Pane) -> Option<&Entry> {
        self.panes[pane.index()].selected()
    }

    fn active_pane_mut(&mut self) -> &mut PaneList {
        &mut self.panes[self.active_pane.index()]
    }

    fn finish_filter_edit(&mut self) {
        self.active_pane_mut().set_sel_idx(0);
        self.cmd.resume_preview(self.active_pane);
        self.dirty = true;
    }

    fn sync_panes(&mut self) {
        for _ in 0..16 {
            let cur_p1_idx = self.panes[0].selected_source_idx();
            let cur_p2_idx = self.sync_pane2(cur_p1_idx);
            let slid = self.sync_pane3(cur_p1_idx, cur_p2_idx);
            if !slid {
                break;
            }
        }
        self.clamp_scroll();
    }

    fn sync_pane2(&mut self, cur_p1_idx: Option<usize>) -> Option<usize> {
        // A stale refresh recomputes Pane 2 entries while keeping the cached
        // navigation context so the user's cursor can be restored by text.
        let force_refresh = std::mem::take(&mut self.pane2_cache_stale);
        let cur_p1 = cur_p1_idx.map(|idx| &self.panes[0].entries()[idx]);
        let mut p1_or_depth_changed = true;
        let mut tokens_changed_only = false;

        if let Some(prev) = self.pane2_cache_state.as_ref() {
            if !force_refresh && pane2_cache_key_matches(prev, cur_p1_idx, &self.cmd.subcmd_chain) {
                p1_or_depth_changed = false;
                if self.pane2_tokens_rev == self.cmd.tokens_rev {
                    tokens_changed_only = false;
                } else {
                    // `tokens` only affects Pane 2 when `p1` has parameter-set children at depth 0.
                    let uses_param_sets = self.cmd.subcmd_chain.is_empty()
                        && cur_p1.is_some_and(|p1| !p1.is_loading() && !p1.children.is_empty());
                    if uses_param_sets {
                        let next_label =
                            cur_p1.and_then(|p1| p1.active_parameter_set_label(self.cmd.tokens()));
                        tokens_changed_only = next_label != prev.active_param_set;
                    }
                    if !tokens_changed_only {
                        self.pane2_tokens_rev = self.cmd.tokens_rev;
                    }
                }
            }
        }

        if p1_or_depth_changed || tokens_changed_only {
            let (entries, active_param_set) = resolve_pane2_entries(
                cur_p1,
                self.cmd.tokens(),
                &self.cmd.subcmd_chain,
                &mut self.introspector,
            );
            self.pane2_tokens_rev = self.cmd.tokens_rev;

            let next_state = Pane2CacheState {
                p1_entry_idx: cur_p1_idx,
                active_param_set: active_param_set.clone(),
                subcmd_chain: self.cmd.subcmd_chain.clone(),
            };

            if force_refresh || self.pane2_cache_state.as_ref() != Some(&next_state) {
                let cur_p1_identity = cur_p1.map(|e| (e.text.clone(), e.kind));
                let same_p1_depth = self.pane2_cache_state.as_ref().is_some_and(|prev| {
                    pane2_navigation_context_matches(
                        self.last_p1_identity.as_ref(),
                        cur_p1_identity.as_ref(),
                        &prev.subcmd_chain,
                        &self.cmd.subcmd_chain,
                    )
                });
                let p1_changed = self.last_p1_identity != cur_p1_identity;
                if p1_changed {
                    self.last_p1_identity = cur_p1_identity;
                }
                let prev_p2_text = if same_p1_depth {
                    self.selected_entry(Pane::Second).map(|it| it.text.clone())
                } else {
                    None
                };

                if p1_changed && !self.skip_child_filter_clear_once {
                    self.panes[1].clear_filter();
                    self.panes[2].clear_filter();
                }
                self.skip_child_filter_clear_once = false;

                self.panes[1].set_entries(entries);
                if same_p1_depth && !self.cmd.suppress_pane2_preview {
                    let matched = prev_p2_text.as_deref().is_some_and(|prev_txt| {
                        self.panes[1].select_first_matching(|it| it.text == prev_txt)
                    });
                    if !matched {
                        self.panes[1].reset_cursor();
                    }
                } else {
                    self.panes[1].reset_cursor();
                }

                self.active_param_set_label = active_param_set;
                self.pane2_cache_state = Some(next_state);
                self.pane3_cache_state = None;
            }
        }

        if let Some(reselect) = self.cmd.pending_pane2_reselect.take() {
            let pane2_entries = self.panes[1].entries();
            if !pane2_entries.iter().any(Entry::is_loading) {
                self.panes[1].select_first_matching(|it| it.text.eq_ignore_ascii_case(&reselect));
            } else {
                self.cmd.pending_pane2_reselect = Some(reselect);
            }
        }

        if let Some(tok2) = self.cmd.pending_pane2_seed.take() {
            let pane2_entries = self.panes[1].entries();
            if !pane2_entries.iter().any(Entry::is_loading) {
                if select_best_matching(&mut self.panes[1], &tok2) {
                    let inline_rhs = self.panes[1]
                        .selected()
                        .filter(|e| matches!(e.kind, EntryKind::Param | EntryKind::Flag))
                        .and_then(|_| split_inline_param_value(&tok2))
                        .map(|(_, rhs)| rhs)
                        .filter(|rhs| !rhs.is_empty())
                        .map(str::to_string);

                    self.active_pane = Pane::Third;
                    if self.cmd.pending_pane3_seeds.is_empty() {
                        if let Some(rhs) = inline_rhs {
                            self.cmd.suppress_pane3_preview = true;
                            self.cmd.pending_inline_pane3_seed = Some(rhs);
                        }
                    }
                } else {
                    self.cmd.pending_pane3_seeds.clear();
                    self.cmd.suppress_pane2_preview = true;
                }
            } else {
                self.cmd.pending_pane2_seed = Some(tok2);
            }
        }

        self.panes[1].selected_source_idx()
    }

    fn sync_pane3(&mut self, cur_p1_idx: Option<usize>, cur_p2_idx: Option<usize>) -> bool {
        let p2_changed = {
            let cur_p2_str = cur_p2_idx.map(|idx| self.panes[1].entries()[idx].text.as_str());
            if self.last_p2_text.as_deref() != cur_p2_str {
                self.last_p2_text = cur_p2_str.map(str::to_string);
                true
            } else {
                false
            }
        };

        if p2_changed {
            self.panes[2].clear_filter();
        }

        let cur_filter = self.panes[2].filter();
        let cache_hit = self.pane3_cache_state.as_ref().is_some_and(|prev| {
            prev.p1_entry_idx == cur_p1_idx
                && prev.p2_entry_idx == cur_p2_idx
                && prev.pane3_filter == cur_filter
                && prev.subcmd_chain == self.cmd.subcmd_chain
        });

        if !cache_hit {
            self.ensure_common_vars_cached();
            let cur_p1 = cur_p1_idx.map(|idx| &self.panes[0].entries()[idx]);
            let cur_p2 = cur_p2_idx.map(|idx| &self.panes[1].entries()[idx]);
            let cache = SessionRuntimeCache {
                dialect: self.dialect,
                common_vars: self.cached_common_vars.as_deref().unwrap_or(&[]),
                cwd_all: &self.cached_cwd_all,
                cwd_dirs: &self.cached_cwd_dirs,
                git_branches: &self.cached_git_branches,
            };
            let entries = resolve_pane3_entries(
                cur_p1,
                cur_p2,
                &self.cmd.subcmd_chain,
                &mut self.introspector,
                self.panes[2].filter(),
                &cache,
            );
            self.panes[2].set_entries(entries);
            self.panes[2].reset_cursor();
            self.pane3_cache_state = Some(Pane3CacheState {
                p1_entry_idx: cur_p1_idx,
                p2_entry_idx: cur_p2_idx,
                pane3_filter: self.panes[2].filter().to_string(),
                subcmd_chain: self.cmd.subcmd_chain.clone(),
            });
        }

        if let Some(inline_rhs) = self.cmd.pending_inline_pane3_seed.take() {
            let pane3_entries = self.panes[2].entries();
            if !pane3_entries.iter().any(Entry::is_loading) {
                let colon_rhs = format!(":{inline_rhs}");
                let _ = select_best_matching(&mut self.panes[2], &inline_rhs)
                    || select_best_matching(&mut self.panes[2], &colon_rhs);
            } else {
                self.cmd.pending_inline_pane3_seed = Some(inline_rhs);
            }
        }

        if self.active_pane == Pane::Third && !self.cmd.pending_pane3_seeds.is_empty() {
            let pane3_entries = self.panes[2].entries();
            if !pane3_entries.iter().any(Entry::is_loading) {
                let Some((tok3_idx, tok3)) = self.cmd.pending_pane3_seeds.pop_front() else {
                    return false;
                };
                let has_more = !self.cmd.pending_pane3_seeds.is_empty();
                let matched = select_best_matching(&mut self.panes[2], &tok3);
                let matched_navigable_child = matched
                    && self.panes[2].selected().is_some_and(|e| {
                        e.kind == EntryKind::Subcmd
                            || (matches!(e.kind, EntryKind::Param | EntryKind::Flag)
                                && !e.commits_without_value())
                    });
                let exact_match = matched
                    && self.panes[2]
                        .selected()
                        .is_some_and(|e| e.text.eq_ignore_ascii_case(&tok3));
                let has_inline_rhs =
                    split_inline_param_value(&tok3).is_some_and(|(_, rhs)| !rhs.is_empty());

                if has_more && !has_inline_rhs && matched_navigable_child {
                    let parent_sub = cur_p2_idx
                        .and_then(|idx| self.panes[1].entries().get(idx))
                        .filter(|e| e.kind == EntryKind::Subcmd)
                        .map(|e| e.text.clone());
                    if let Some(parent_sub) = parent_sub {
                        self.slide_window_into_subcmd(
                            parent_sub,
                            tok3_idx.saturating_sub(1),
                            tok3,
                            false,
                        );
                        return true;
                    }
                }
                self.cmd.pending_pane3_seeds.clear();
                if !matched || has_more || has_inline_rhs || !exact_match {
                    self.cmd.suppress_pane3_preview = true;
                }
            }
        }
        false
    }

    fn clamp_scroll(&mut self) {
        for pane in Pane::ALL {
            self.panes[pane.index()].clamp_scroll(self.viewport.rows(pane));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::PollResult;
    use std::cell::RefCell;
    use std::collections::{HashMap, VecDeque};
    use std::rc::Rc;

    #[derive(Default)]
    struct FakeIntrospectorState {
        responses: HashMap<String, Vec<Entry>>,
        cache: HashMap<String, Vec<Entry>>,
        pending: VecDeque<String>,
        ready: VecDeque<(String, Vec<Entry>)>,
        catalog_ready: VecDeque<Vec<Entry>>,
    }

    #[derive(Clone, Default)]
    struct FakeIntrospector {
        state: Rc<RefCell<FakeIntrospectorState>>,
    }

    impl FakeIntrospector {
        fn empty() -> Self {
            Self::default()
        }

        fn with_chains<I, K>(chains: I) -> Self
        where
            I: IntoIterator<Item = (K, Vec<Entry>)>,
            K: Into<String>,
        {
            let responses = chains
                .into_iter()
                .map(|(k, v)| (k.into().to_ascii_lowercase(), v))
                .collect();
            Self {
                state: Rc::new(RefCell::new(FakeIntrospectorState {
                    responses,
                    ..Default::default()
                })),
            }
        }

        fn complete_next(&self) {
            let mut st = self.state.borrow_mut();
            if let Some(key) = st.pending.pop_front() {
                let entries = st.responses.get(&key).cloned().unwrap_or_default();
                st.ready.push_back((key, entries));
            }
        }

        fn push_ready(&self, key: &str, entries: Vec<Entry>) {
            self.state
                .borrow_mut()
                .ready
                .push_back((key.to_ascii_lowercase(), entries));
        }

        fn push_catalog_reload(&self, entries: Vec<Entry>) {
            self.state.borrow_mut().catalog_ready.push_back(entries);
        }
    }

    impl Introspect for FakeIntrospector {
        fn query(&mut self, chain: &[&str]) -> Vec<Entry> {
            if chain.is_empty() {
                return Vec::new();
            }
            let key = chain
                .iter()
                .map(|s| s.trim().to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join("::");
            let mut st = self.state.borrow_mut();
            if let Some(cached) = st.cache.get(&key) {
                return cached.clone();
            }
            if !st.pending.contains(&key) {
                st.pending.push_back(key);
            }
            vec![Entry::placeholder(
                format!("⟳ Loading {}...", chain.join(" ")),
                EntryKind::Loading,
                "",
            )]
        }

        fn poll_updates(&mut self) -> PollResult {
            let mut st = self.state.borrow_mut();
            let mut introspected = false;
            while let Some((key, entries)) = st.ready.pop_front() {
                st.cache.insert(key, entries);
                introspected = true;
            }
            let catalog_reloaded = st.catalog_ready.pop_front();
            PollResult {
                introspected,
                catalog_reloaded,
            }
        }

        fn flush(&mut self) -> bool {
            false
        }
    }

    fn sample_ps_cmdlet() -> Entry {
        let mut get_process = Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "Gets the processes that are running on the local computer",
        );

        let info_action = Entry::new(
            "-InformationAction",
            "-InformationAction (-ia)",
            EntryKind::Param,
            "[ActionPreference] Parameter -InformationAction (-ia)",
        )
        .with_aliases(vec!["ia".to_string()])
        .with_parameter_sets(vec!["InfoSet".to_string()]);

        let identity = Entry::new(
            "-Identity",
            "-Identity",
            EntryKind::Param,
            "[String] Parameter -Identity",
        )
        .with_parameter_sets(vec!["IdentitySet".to_string()]);

        let id_param = Entry::new(
            "-Id",
            "-Id (-PID, -ProcessId)",
            EntryKind::Param,
            "[Int32[]] Parameter -Id (-PID, -ProcessId)",
        )
        .with_aliases(vec!["PID".to_string(), "ProcessId".to_string()])
        .with_parameter_sets(vec!["Id".to_string(), "IdWithUserName".to_string()]);

        let name_param = Entry::new(
            "-Name",
            "-Name (-ProcessName)",
            EntryKind::Param,
            "[String[]] Parameter -Name (-ProcessName)",
        )
        .with_aliases(vec!["ProcessName".to_string()])
        .with_parameter_sets(vec!["Name".to_string(), "NameWithUserName".to_string()]);

        let mut verbose_flag = Entry::new(
            "-Verbose",
            "-Verbose (-vb)",
            EntryKind::Flag,
            "[SwitchParameter] Parameter -Verbose (-vb)",
        )
        .with_aliases(vec!["vb".to_string()])
        .with_parameter_sets(vec!["__AllParameterSets".to_string()]);
        verbose_flag.children = vec![
            Entry::placeholder(
                "(Flag - No Value Needed)",
                EntryKind::Flag,
                "Press Enter or Tab to include -Verbose",
            ),
            Entry::new(":$true", ":$true", EntryKind::Value, "Enable flag"),
            Entry::new(":$false", ":$false", EntryKind::Value, "Disable flag"),
        ];

        get_process.children = vec![info_action, identity, id_param, name_param, verbose_flag];
        get_process
    }

    #[test]
    fn test_exact_parameter_alias_matching_and_exclusivity() {
        let mut session = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Step into Pane 2: all 5 parameters should be visible initially
        session.apply(UserAction::StepRight);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Second);
            assert_eq!(v.panes[1].visible_len(), 5);
            assert_eq!(v.active_param_set, None);
        }

        // Move down to `-Id` (index 2: after -InformationAction and -Identity)
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "Get-Process -Id");

        // Step right into Pane 3 to supply an ID: commits `-Id` and activates {"Id", "IdWithUserName"}
        session.apply(UserAction::StepRight);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert_eq!(v.active_param_set, Some("Id, IdWithUserName"));
            assert_eq!(v.panes[1].visible_len(), 2);
            assert_eq!(v.panes[1].selected().map(|e| e.text.as_str()), Some("-Id"));
            assert_eq!(
                v.panes[1].row_at(1).map(|(e, _)| e.text.as_str()),
                Some("-Verbose")
            );
        }

        // Type custom process ID "4242" in Pane 3 and accept via Tab
        for ch in "4242".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(session.command_line(), "Get-Process -Id 4242");
        session.apply(UserAction::AcceptToken);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Second);
            assert_eq!(v.active_param_set, Some("Id, IdWithUserName"));
            assert_eq!(v.command_line, "Get-Process -Id 4242");
        }

        // Pop "4242" then pop "-Id" via Backspace -> parameter set exclusivity clears
        session.apply(UserAction::Backspace);
        assert_eq!(session.command_line(), "Get-Process -Id");
        assert_eq!(session.view().active_param_set, Some("Id, IdWithUserName"));

        session.apply(UserAction::Backspace);
        assert_eq!(session.command_line(), "Get-Process");
        {
            let v = session.view();
            assert_eq!(v.active_param_set, None);
            assert_eq!(v.panes[1].visible_len(), 5);
        }

        // Verify structured alias matching via initial buffer (`-pid` and `-ia`)
        let session_alias = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "Get-Process -pid",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        {
            let v = session_alias.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert_eq!(v.active_param_set, Some("Id, IdWithUserName"));
            assert_eq!(v.panes[1].selected().map(|e| e.text.as_str()), Some("-Id"));
        }

        let session_ia = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "Get-Process -ia",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(session_ia.view().active_param_set, Some("InfoSet"));
    }

    #[test]
    fn test_accept_token_on_pane2_flag_does_not_pollute_preview() {
        let mut session = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Step into Pane 2 and move to `-Verbose` (index 4, a Flag parameter)
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        assert_eq!(session.command_line(), "Get-Process -Verbose");

        // Press Tab (`AcceptToken`) on `-Verbose`:
        // Selection resets to index 0 (`-InformationAction`), but Command Line MUST NOT append it!
        session.apply(UserAction::AcceptToken);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Second);
            assert_eq!(
                v.panes[1].selected().map(|e| e.text.as_str()),
                Some("-InformationAction")
            );
            assert_eq!(v.command_line, "Get-Process -Verbose");
        }

        // Confirming right now emits only `Get-Process -Verbose`
        assert_eq!(
            session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("Get-Process -Verbose".to_string())
        );

        // Moving down in Pane 2 re-enables live preview for the newly highlighted entry
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "Get-Process -Verbose -Identity");
    }

    #[test]
    fn test_second_tab_after_flag_accept_does_not_commit_unpreviewed_entry() {
        let mut session = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.command_line(), "Get-Process -Verbose");

        // Second Tab without navigation or typing must not commit the un-previewed index-0 entry.
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(session.command_line(), "Get-Process -Verbose");

        // Navigation re-arms normal acceptance of the highlighted entry.
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_pane, Pane::Third);
        assert_eq!(session.command_line(), "Get-Process -Verbose -Identity");
    }

    #[test]
    fn test_unrelated_background_completion_preserves_pane3_cursor() {
        let git_entry = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        let fake = FakeIntrospector::with_chains([
            (
                "git",
                vec![
                    Entry::new("clone", "clone", EntryKind::Subcmd, "Clone"),
                    Entry::new("commit", "commit", EntryKind::Subcmd, "Commit"),
                ],
            ),
            (
                "git::commit",
                vec![
                    Entry::new("--all", "--all", EntryKind::Flag, "All"),
                    Entry::new("--message", "--message", EntryKind::Param, "Msg"),
                    Entry::new("--amend", "--amend", EntryKind::Flag, "Amend"),
                ],
            ),
        ]);
        let mut session = MillerSession::new(
            vec![git_entry],
            fake.clone(),
            "git commit",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        fake.complete_next();
        assert!(session.poll_discovery());
        fake.complete_next();
        assert!(session.poll_discovery());
        assert_eq!(session.view().active_pane, Pane::Third);

        session.apply(UserAction::MoveDown);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "git commit --amend");
        assert_eq!(
            session.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("--amend")
        );

        // Deliver an unrelated background job completion.
        fake.push_ready(
            "other",
            vec![Entry::new(
                "--unrelated",
                "--unrelated",
                EntryKind::Flag,
                "Unrelated",
            )],
        );
        assert!(session.poll_discovery());

        assert_eq!(session.command_line(), "git commit --amend");
        assert_eq!(
            session.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("--amend")
        );
    }

    #[test]
    fn test_flag_placeholder_in_pane3_never_emits_blank_token() {
        let mut session = MillerSession::new(
            vec![sample_ps_cmdlet()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Select `-Verbose` in Pane 2 and press Right (`StepRight`) to enter Pane 3
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        session.apply(UserAction::StepRight);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert!(v.panes[2].selected().is_some_and(Entry::is_placeholder));
            assert_eq!(v.command_line, "Get-Process -Verbose");
        }

        // Pressing Tab on the placeholder returns to Pane 2 without adding a blank token
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(session.command_line(), "Get-Process -Verbose");
        assert_eq!(
            session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("Get-Process -Verbose".to_string())
        );
    }

    #[test]
    fn test_initial_buffer_seeding_three_plus_tokens_and_async_poll() {
        let git_entry = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        let fake = FakeIntrospector::with_chains([
            (
                "git",
                vec![
                    Entry::new("clone", "clone", EntryKind::Subcmd, "Clone"),
                    Entry::new("commit", "commit", EntryKind::Subcmd, "Commit"),
                ],
            ),
            (
                "git::commit",
                vec![
                    Entry::new("--all", "--all", EntryKind::Flag, "All"),
                    Entry::new("--message", "--message", EntryKind::Param, "Message"),
                    Entry::new("--amend", "--amend", EntryKind::Flag, "Amend"),
                ],
            ),
        ]);

        // 1. 3-token seeding where 3rd token matches Pane 3 entry (`--amend`)
        let mut session = MillerSession::new(
            vec![git_entry.clone()],
            fake.clone(),
            "git commit --amend",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        assert_eq!(session.view().active_pane, Pane::Second);
        assert!(session.view().panes[1]
            .selected()
            .is_some_and(Entry::is_loading));

        fake.complete_next();
        assert!(session.poll_discovery());
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert_eq!(
                v.panes[1].selected().map(|e| e.text.as_str()),
                Some("commit")
            );
            assert_eq!(v.command_line, "git commit --amend");
        }

        fake.complete_next();
        assert!(session.poll_discovery());
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert_eq!(
                v.panes[2].selected().map(|e| e.text.as_str()),
                Some("--amend")
            );
            assert_eq!(
                v.command_line, "git commit --amend",
                "3rd seeded token must match Pane 3 entry without appending extra token"
            );
        }

        // 2. 3+ token seeding with extra/unmatched trailing tokens suppresses index-0 preview
        let fake_multi = FakeIntrospector::with_chains([
            (
                "git",
                vec![Entry::new("commit", "commit", EntryKind::Subcmd, "Commit")],
            ),
            (
                "git::commit",
                vec![
                    Entry::new("--all", "--all", EntryKind::Flag, "All"),
                    Entry::new("--amend", "--amend", EntryKind::Flag, "Amend"),
                ],
            ),
        ]);
        let mut session_multi = MillerSession::new(
            vec![git_entry],
            fake_multi.clone(),
            "git commit --amend --no-edit",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        fake_multi.complete_next();
        assert!(session_multi.poll_discovery());
        fake_multi.complete_next();
        assert!(session_multi.poll_discovery());
        assert_eq!(session_multi.view().active_pane, Pane::Third);
        assert_eq!(
            session_multi.command_line(),
            "git commit --amend --no-edit",
            "Extra seeded tokens must not suffer Pane 3 index-0 preview pollution"
        );
    }

    #[test]
    fn test_live_background_catalog_reload_preserves_selection_and_filter() {
        let fake = FakeIntrospector::empty();
        let initial = vec![
            Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo"),
            Entry::new("git", "git", EntryKind::Cli, "Git"),
        ];
        let mut session = MillerSession::new(
            initial,
            fake.clone(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        session.apply(UserAction::InsertChar('g'));
        assert_eq!(
            session.view().panes[0].selected().map(|e| e.text.as_str()),
            Some("git")
        );

        // Deliver refreshed catalog containing PowerShell cmdlets + git
        fake.push_catalog_reload(vec![
            Entry::new("Get-Process", "Get-Process", EntryKind::Cmdlet, "PS Cmdlet"),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo"),
            Entry::new("git", "git", EntryKind::Cli, "Git"),
        ]);
        assert!(session.poll_discovery());

        {
            let v = session.view();
            assert_eq!(v.panes[0].filter(), "g");
            assert_eq!(v.panes[0].selected().map(|e| e.text.as_str()), Some("git"));
            assert_eq!(v.panes[0].entries().len(), 3);
        }

        // Clearing filter now reveals Get-Process live without Ctrl+R
        session.apply(UserAction::ClearFilter);
        {
            let v = session.view();
            assert_eq!(v.panes[0].filter(), "");
            assert_eq!(v.panes[0].visible_len(), 3);
            assert_eq!(
                v.panes[0].selected().map(|e| e.text.as_str()),
                Some("Get-Process")
            );
        }
    }

    #[test]
    fn test_variable_scopes_across_all_three_dialects() {
        for (dialect, expected_user_tok, expected_special_var) in [
            (ShellDialect::PowerShell, "$env:USER", "$_"),
            (ShellDialect::Posix, "$USER", "$?"),
            (ShellDialect::Fish, "$USER", "$status"),
        ] {
            let mut env_root = Entry::scope_root(
                if dialect.is_powershell() {
                    "$env:"
                } else {
                    "$"
                },
                "$ENV",
                EntryKind::Env,
                "Env vars",
            );
            let mut user_entry = Entry::new(expected_user_tok, "USER", EntryKind::Env, "alice");
            user_entry.children = vec![
                Entry::new(
                    expected_user_tok,
                    format!("Insert {expected_user_tok}"),
                    EntryKind::Var,
                    "alice",
                ),
                Entry::new("\"alice\"", "Literal: alice", EntryKind::Value, "Literal"),
            ];
            env_root.children = vec![user_entry];

            let mut var_root = Entry::scope_root("$", "$Vars", EntryKind::Var, "Shell vars");
            var_root.children = dialect.common_var_entries();

            let mut session = MillerSession::new(
                vec![env_root, var_root],
                FakeIntrospector::empty(),
                "",
                Viewport::new(120, 30),
                dialect,
            );

            // ScopeRoot in Pane 1 does not emit "$env:" or "$"
            assert_eq!(session.command_line(), "");

            // Step into Pane 2 (USER)
            session.apply(UserAction::StepRight);
            assert_eq!(session.command_line(), expected_user_tok);

            // Step into Pane 3 on USER: index 0 is variable ref, index 1 is literal
            session.apply(UserAction::StepRight);
            assert_eq!(session.command_line(), expected_user_tok);

            session.apply(UserAction::MoveDown);
            assert_eq!(session.command_line(), "\"alice\"");

            // Step back to Pane 1 and inspect the dialect's common variable scope
            session.apply(UserAction::Backspace);
            session.apply(UserAction::Backspace);
            assert_eq!(session.view().active_pane, Pane::First);

            session.apply(UserAction::MoveDown);
            session.apply(UserAction::StepRight);
            let p2_texts: Vec<&str> = session.view().panes[1]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert!(
                p2_texts.contains(&expected_special_var),
                "Dialect {dialect:?} should expose {expected_special_var}"
            );
        }
    }

    #[test]
    fn test_small_terminal_paging_and_reload_catalog() {
        let initial_entries: Vec<Entry> = (0..15)
            .map(|i| {
                let mut cmd = Entry::new(
                    format!("cmd{i:02}"),
                    format!("cmd{i:02}"),
                    EntryKind::Cli,
                    "",
                );
                cmd.children = (0..10)
                    .map(|f| {
                        let mut sub =
                            Entry::new(format!("sub{f}"), format!("sub{f}"), EntryKind::Subcmd, "");
                        sub.children = (0..10)
                            .map(|v| {
                                Entry::new(
                                    format!("--opt{v}"),
                                    format!("--opt{v}"),
                                    EntryKind::Flag,
                                    "",
                                )
                            })
                            .collect();
                        sub
                    })
                    .collect();
                cmd
            })
            .collect();

        // Small terminal: height = 12 (< 16)
        // Pane 1 & Pane 2 rows = 6; Pane 3 rows = 3
        let mut session = MillerSession::new(
            initial_entries,
            FakeIntrospector::empty(),
            "",
            Viewport::new(90, 12),
            ShellDialect::Posix,
        )
        .with_catalog_loader(|_| {
            vec![Entry::new(
                "reloaded-tool",
                "reloaded-tool",
                EntryKind::Cli,
                "Freshly synced tool",
            )]
        });

        // PageDown in Pane 1 jumps by 6 rows -> cmd06 selected
        session.apply(UserAction::PageDown);
        assert_eq!(
            session.view().panes[0].selected().map(|e| e.text.as_str()),
            Some("cmd06")
        );
        assert_eq!(session.view().panes[0].scroll_idx(), 1);

        // Step right into Pane 2, then page by 6 rows -> sub6 selected
        session.apply(UserAction::StepRight);
        session.apply(UserAction::PageDown);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("sub6")
        );

        // Step right into Pane 3, then page by 3 rows -> --opt3 selected
        session.apply(UserAction::StepRight);
        session.apply(UserAction::PageDown);
        {
            let v = session.view();
            assert_eq!(v.active_pane, Pane::Third);
            assert_eq!(
                v.panes[2].selected().map(|e| e.text.as_str()),
                Some("--opt3")
            );
        }

        // Left navigation alone must not mutate committed tokens.
        session.apply(UserAction::StepLeft);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(session.command_line(), "cmd06 sub6");
        session.apply(UserAction::StepLeft);
        assert_eq!(session.view().active_pane, Pane::First);
        assert_eq!(session.command_line(), "cmd06 sub6");

        // Backspace pops committed tokens cleanly.
        session.apply(UserAction::Backspace);
        assert_eq!(session.command_line(), "cmd06");
        session.apply(UserAction::Backspace);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "cmd07");

        // ReloadCatalog replaces catalog cleanly and updates Command Line
        session.apply(UserAction::ReloadCatalog);
        {
            let v = session.view();
            assert_eq!(v.panes[0].visible_len(), 1);
            assert_eq!(
                v.panes[0].selected().map(|e| e.text.as_str()),
                Some("reloaded-tool")
            );
            assert_eq!(v.command_line, "reloaded-tool");
        }
    }

    #[test]
    fn test_switching_pane1_command_truncates_stale_child_tokens() {
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![Entry::new("commit", "commit", EntryKind::Subcmd, "Commit")];

        let mut cargo = Entry::new("cargo", "cargo", EntryKind::Cli, "Rust Cargo");
        cargo.children = vec![Entry::new("build", "build", EntryKind::Subcmd, "Build")];

        let mut session = MillerSession::new(
            vec![git, cargo],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        session.apply(UserAction::StepRight);
        session.apply(UserAction::StepRight);
        assert_eq!(session.command_line(), "git commit");

        session.apply(UserAction::StepLeft);
        session.apply(UserAction::StepLeft);
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::StepRight);
        assert_eq!(session.command_line(), "cargo build");
    }

    #[test]
    fn test_git_branch_entries_appear_in_pane3_for_git_checkout() {
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![Entry::new(
            "checkout",
            "checkout",
            EntryKind::Subcmd,
            "Switch branches",
        )];

        let fake = FakeIntrospector::with_chains([(
            "git::checkout",
            vec![Entry::new("--force", "--force", EntryKind::Flag, "Force")],
        )]);
        let ctx = RuntimeContext {
            git_branch: Some("main".to_string()),
            git_branches: vec!["main".to_string(), "feat/branches".to_string()],
            cwd_entries: vec![crate::model::CwdEntry {
                name: "README.md".to_string(),
                is_dir: false,
            }],
            history: vec![],
        };

        let mut session = MillerSession::new(
            vec![git],
            fake.clone(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
        .with_runtime_context(ctx);

        // Step into checkout -> Pane 3 initially waits for git::checkout introspection
        session.apply(UserAction::StepRight);
        fake.complete_next();
        assert!(session.poll_discovery());

        let p3_texts: Vec<&str> = session.view().panes[2]
            .entries()
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(
            p3_texts,
            vec!["main", "feat/branches", "--force", "README.md"]
        );

        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "git checkout feat/branches");
    }

    #[test]
    fn test_spec_files_and_directories_macros_expand_to_cwd_entries_in_pane3() {
        let mut tool = Entry::new("tool", "tool", EntryKind::Cli, "Spec tool");
        let mut file_param = Entry::new("--input", "--input", EntryKind::Param, "Input file");
        file_param.children = vec![Entry::placeholder("$files", EntryKind::Path, "Spec macro")];
        let mut dir_param = Entry::new("--dir", "--dir", EntryKind::Param, "Target directory");
        dir_param.children = vec![Entry::placeholder(
            "$directories",
            EntryKind::Path,
            "Spec macro",
        )];
        tool.children = vec![file_param, dir_param];

        let ctx = RuntimeContext {
            cwd_entries: vec![
                crate::model::CwdEntry {
                    name: "src".to_string(),
                    is_dir: true,
                },
                crate::model::CwdEntry {
                    name: "Cargo.toml".to_string(),
                    is_dir: false,
                },
            ],
            ..Default::default()
        };

        let mut session = MillerSession::new(
            vec![tool],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
        .with_runtime_context(ctx);

        // --input expands $files (both dirs and files)
        let p3_files: Vec<&str> = session.view().panes[2]
            .entries()
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert!(p3_files.contains(&"src/"));
        assert!(p3_files.contains(&"Cargo.toml"));
        assert!(!p3_files.contains(&"$files"));

        // Moving to --dir expands $directories (only dirs)
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveDown);
        let p3_dirs: Vec<&str> = session.view().panes[2]
            .entries()
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert!(p3_dirs.contains(&"src/"));
        assert!(!p3_dirs.contains(&"Cargo.toml"));
        assert!(!p3_dirs.contains(&"$directories"));
    }

    #[test]
    fn test_history_entry_in_pane1_introspects_leading_command_chain() {
        let hist = Entry::new(
            "cargo test --release",
            "cargo test --release",
            EntryKind::History,
            "Recent shell history",
        );
        let fake = FakeIntrospector::with_chains([(
            "cargo::test",
            vec![Entry::new(
                "--locked",
                "--locked",
                EntryKind::Flag,
                "Locked",
            )],
        )]);

        let mut session = MillerSession::new(
            vec![hist],
            fake.clone(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        fake.complete_next();
        assert!(session.poll_discovery());

        assert!(session.view().panes[1]
            .entries()
            .iter()
            .any(|e| e.text == "--locked"));
    }

    #[test]
    fn test_interactive_subcommand_sliding_honors_subcommand_aliases() {
        let mut add = Entry::new("add", "add", EntryKind::Subcmd, "Add a remote")
            .with_aliases(vec!["new".to_string()]);
        add.children = vec![Entry::new(
            "--tags",
            "--tags",
            EntryKind::Flag,
            "Import tags",
        )];
        let mut remote = Entry::new("remote", "remote", EntryKind::Subcmd, "Manage remotes")
            .with_aliases(vec!["rm".to_string()]);
        remote.children = vec![
            add,
            Entry::new("remove", "remove", EntryKind::Subcmd, "Remove a remote"),
        ];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![remote];

        // Seeding with Subcommand aliases (`rm`, `new`) slides through both nested levels
        // without falling back to Introspection (FakeIntrospector::empty has no responses).
        let seeded = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "git RM new --tags",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(seeded.view().active_pane, Pane::Third);
        assert_eq!(
            seeded.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("add")
        );
        assert_eq!(
            seeded.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("--tags")
        );

        // Interactive AcceptToken / StepRight sliding starting from an aliased subcommand (`git rm`)
        // matches `remote` and traverses nested children via `Entry::walk_chain`.
        let mut interactive = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "git rm",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(
            interactive.view().panes[1]
                .selected()
                .map(|e| e.text.as_str()),
            Some("remote")
        );
        interactive.apply(UserAction::StepRight);
        assert_eq!(interactive.view().active_pane, Pane::Third);
        assert_eq!(
            interactive.view().panes[1]
                .selected()
                .map(|e| e.text.as_str()),
            Some("add")
        );
        assert_eq!(
            interactive.view().panes[2]
                .selected()
                .map(|e| e.text.as_str()),
            Some("--tags")
        );

        let inspect = crate::discovery::inspect_command_chain_with(
            ShellDialect::Posix,
            &[git],
            &["git".to_string(), "rm".to_string(), "new".to_string()],
            None,
            false,
            &|_, _| None,
        )
        .expect("inspect should resolve subcommand aliases via Entry::walk_chain");
        assert!(inspect.entries.iter().any(|e| e.text == "--tags"));
    }

    #[test]
    fn test_multi_level_subcommand_sliding_navigation_backspace_and_seeding() {
        let mut remote = Entry::new("remote", "remote", EntryKind::Subcmd, "Manage remotes");
        let mut add = Entry::new("add", "add", EntryKind::Subcmd, "Add a remote");
        add.children = vec![
            Entry::new("--tags", "--tags", EntryKind::Flag, "Import tags"),
            Entry::new("-f", "-f", EntryKind::Flag, "Fetch immediately"),
        ];
        remote.children = vec![
            add,
            Entry::new("remove", "remove", EntryKind::Subcmd, "Remove a remote"),
        ];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![remote];

        let mut session = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        // Pane 1 (`git`) -> Pane 2 (`remote`) -> Pane 3 (`add`) -> Slide into (`git`, `remote`, `add`)
        session.apply(UserAction::StepRight);
        session.apply(UserAction::StepRight);
        assert_eq!(session.view().active_pane, Pane::Third);
        assert_eq!(
            session.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("add")
        );

        session.apply(UserAction::StepRight);
        assert_eq!(session.view().active_pane, Pane::Third);
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("add")
        );
        assert_eq!(
            session.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("--tags")
        );
        assert_eq!(session.command_line(), "git remote add --tags");

        // StepLeft into Pane 2 (`add`), then StepLeft again slides back to root `remote`
        session.apply(UserAction::StepLeft);
        assert_eq!(session.view().active_pane, Pane::Second);
        session.apply(UserAction::StepLeft);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("remote")
        );
        assert_eq!(session.command_line(), "git remote add");

        // Initial buffer seeding across 3+ levels (`git remote add --tags`)
        let seeded = MillerSession::new(
            vec![git],
            FakeIntrospector::empty(),
            "git remote add --tags",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(seeded.view().active_pane, Pane::Third);
        assert_eq!(
            seeded.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("add")
        );
        assert_eq!(
            seeded.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("--tags")
        );
        assert_eq!(seeded.command_line(), "git remote add --tags");
    }

    #[test]
    fn test_quote_aware_seeding_pipeline_prefix_and_equals_colon_attachment() {
        assert_eq!(
            split_pipeline_prefix(r#"Get-Process | Where-Object -Property "CPU""#),
            (
                Some("Get-Process |".to_string()),
                r#"Where-Object -Property "CPU""#
            )
        );
        assert_eq!(
            tokenize_shell_words(r#"git commit -m "fix: bug" --date='2026-01-01'"#),
            vec!["git", "commit", "-m", "\"fix: bug\"", "--date='2026-01-01'"]
        );

        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        let mut commit = Entry::new("commit", "commit", EntryKind::Subcmd, "Commit");
        commit.children = vec![
            Entry::new("-m", "-m", EntryKind::Param, "Message"),
            Entry::new("--format=", "--format=", EntryKind::Param, "Format"),
        ];
        git.children = vec![commit];

        let session = MillerSession::new(
            vec![git],
            FakeIntrospector::empty(),
            r#"git status && git commit -m "fix: bug""#,
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(
            session.command_line(),
            r#"git status && git commit -m "fix: bug""#
        );

        // Typing custom value with spaces wraps in quotes automatically
        let mut fd = Entry::new("fd", "fd", EntryKind::Cli, "Find");
        let ext = Entry::new("--extension=", "-e, --extension=", EntryKind::Param, "Ext");
        fd.children = vec![
            ext,
            Entry::new("--glob", "--glob", EntryKind::Param, "Glob"),
        ];
        let mut fd_sess = MillerSession::new(
            vec![fd],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        fd_sess.apply(UserAction::StepRight);
        fd_sess.apply(UserAction::StepRight);
        for ch in "rs".chars() {
            fd_sess.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(fd_sess.command_line(), "fd --extension=rs");

        fd_sess.apply(UserAction::AcceptToken);
        fd_sess.apply(UserAction::MoveDown);
        fd_sess.apply(UserAction::StepRight);
        for ch in "my file.rs".chars() {
            fd_sess.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(
            fd_sess.command_line(),
            "fd --extension=rs --glob \"my file.rs\""
        );
    }

    #[test]
    fn test_home_row_keys_and_inspector_toggle_overlay() {
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL)),
            Some(UserAction::MoveDown)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(UserAction::MoveUp)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            Some(UserAction::MoveDown)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::ALT)),
            Some(UserAction::MoveUp)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)),
            Some(UserAction::ToggleInspector)
        );

        let mut session = MillerSession::new(
            vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")],
            FakeIntrospector::empty(),
            "",
            Viewport::new(100, 20),
            ShellDialect::Posix,
        );
        assert!(!session.view().inspector_expanded);

        assert_eq!(
            session.apply(UserAction::ToggleInspector),
            SessionOutcome::Continue
        );
        assert!(session.view().inspector_expanded);

        // First Esc closes the expanded inspector instead of canceling the session.
        assert_eq!(session.apply(UserAction::Cancel), SessionOutcome::Continue);
        assert!(!session.view().inspector_expanded);

        // Second Esc cancels normally.
        assert_eq!(session.apply(UserAction::Cancel), SessionOutcome::Canceled);
    }

    #[test]
    fn test_equals_and_colon_seeding_prevents_duplication_and_supports_replacement() {
        let mut ext = Entry::new("--extension=", "-e, --extension=", EntryKind::Param, "Ext")
            .with_aliases(vec!["e".to_string()]);
        ext.children = vec![
            Entry::new("md", "md", EntryKind::Value, "Markdown"),
            Entry::new("rs", "rs", EntryKind::Value, "Rust"),
        ];
        let mut fd = Entry::new("fd", "fd", EntryKind::Cli, "Find");
        fd.children = vec![ext];

        let mut fd_seeded = MillerSession::new(
            vec![fd],
            FakeIntrospector::empty(),
            "fd --extension=rs",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(fd_seeded.view().active_pane, Pane::Third);
        assert_eq!(
            fd_seeded.view().panes[2]
                .selected()
                .map(|e| e.text.as_str()),
            Some("rs")
        );
        assert_eq!(fd_seeded.command_line(), "fd --extension=rs");

        // Stepping back to Pane 2 must not re-emit `--extension=` after `--extension=rs`.
        fd_seeded.apply(UserAction::StepLeft);
        assert_eq!(fd_seeded.view().active_pane, Pane::Second);
        assert_eq!(fd_seeded.command_line(), "fd --extension=rs");

        // Stepping into Pane 3 and picking another value replaces the inline RHS cleanly.
        fd_seeded.apply(UserAction::StepRight);
        fd_seeded.apply(UserAction::MoveUp);
        assert_eq!(
            fd_seeded.view().panes[2]
                .selected()
                .map(|e| e.text.as_str()),
            Some("md")
        );
        assert_eq!(fd_seeded.command_line(), "fd --extension=md");
        fd_seeded.apply(UserAction::AcceptToken);
        assert_eq!(fd_seeded.command_line(), "fd --extension=md");

        // PowerShell colon parameter alias (`-pid:42`) + switch colon (`-Verbose:$true`)
        let mut get_proc = Entry::new("Get-Process", "Get-Process", EntryKind::Cmdlet, "Processes");
        get_proc.children = vec![
            Entry::new("-Id", "-Id (-PID)", EntryKind::Param, "PID")
                .with_aliases(vec!["PID".to_string()]),
            Entry::new("-Verbose", "-Verbose", EntryKind::Flag, "Verbose"),
        ];
        let mut ps_seeded = MillerSession::new(
            vec![get_proc.clone()],
            FakeIntrospector::empty(),
            "Get-Process -pid:42",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(ps_seeded.view().active_pane, Pane::Third);
        assert_eq!(
            ps_seeded.view().panes[1]
                .selected()
                .map(|e| e.text.as_str()),
            Some("-Id")
        );
        assert_eq!(ps_seeded.command_line(), "Get-Process -pid:42");
        ps_seeded.apply(UserAction::StepLeft);
        assert_eq!(ps_seeded.command_line(), "Get-Process -pid:42");
        ps_seeded.apply(UserAction::StepRight);
        for ch in "99".chars() {
            ps_seeded.apply(UserAction::InsertChar(ch));
        }
        ps_seeded.apply(UserAction::AcceptToken);
        assert_eq!(ps_seeded.command_line(), "Get-Process -pid:99");

        let ps_switch = MillerSession::new(
            vec![get_proc],
            FakeIntrospector::empty(),
            "Get-Process -Verbose:$true",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(ps_switch.command_line(), "Get-Process -Verbose:$true");

        // Subcommand inline `--format=oneline` + exact case priority (`-B` over `-b`)
        let mut log = Entry::new("log", "log", EntryKind::Subcmd, "Log");
        log.children = vec![
            Entry::new("-b", "-b", EntryKind::Flag, "Lower b"),
            Entry::new("-B", "-B", EntryKind::Flag, "Upper B"),
            Entry::new("--format=", "--format=", EntryKind::Param, "Format"),
        ];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![
            log,
            Entry::new("-b", "-b", EntryKind::Flag, "Root lower b"),
            Entry::new("-B", "-B", EntryKind::Flag, "Root upper B"),
        ];

        let log_seeded = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "git log --format=oneline",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(log_seeded.command_line(), "git log --format=oneline");

        let pane2_case_seeded = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "git -B",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(
            pane2_case_seeded.view().panes[1]
                .selected()
                .map(|e| e.text.as_str()),
            Some("-B")
        );

        let pane3_case_seeded = MillerSession::new(
            vec![git],
            FakeIntrospector::empty(),
            "git log -B",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(
            pane3_case_seeded.view().panes[2]
                .selected()
                .map(|e| e.text.as_str()),
            Some("-B")
        );
    }

    #[test]
    fn test_dialect_aware_pipeline_splitting_escaping_and_custom_value_quoting() {
        // PowerShell Windows paths ending in backslash must not escape the closing quote.
        assert_eq!(
            split_pipeline_prefix_for_dialect(
                r#"Get-ChildItem "C:\temp\" | Select-Object Name"#,
                ShellDialect::PowerShell
            ),
            (
                Some(r#"Get-ChildItem "C:\temp\" |"#.to_string()),
                "Select-Object Name"
            )
        );
        let ps_pipe_seeded = MillerSession::new(
            vec![Entry::new(
                "Select-Object",
                "Select-Object",
                EntryKind::Cmdlet,
                "Select properties",
            )],
            FakeIntrospector::empty(),
            r#"Get-ChildItem "C:\temp\" | Select-Object"#,
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(
            ps_pipe_seeded.command_line(),
            r#"Get-ChildItem "C:\temp\" | Select-Object"#
        );
        // Scriptblocks and subshell grouping ignore inner separators (`;`, `|`).
        assert_eq!(
            split_pipeline_prefix(r#"Get-Process | ForEach-Object { $_.Name; $_.Id }"#),
            (
                Some("Get-Process |".to_string()),
                r#"ForEach-Object { $_.Name; $_.Id }"#
            )
        );
        assert_eq!(
            split_pipeline_prefix(r#"(git status; git diff) | head"#),
            (Some("(git status; git diff) |".to_string()), "head")
        );

        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![Entry::new("-m", "-m", EntryKind::Param, "Message")];

        // Unclosed leading quote closes cleanly without double-wrapping in POSIX & PowerShell.
        let mut posix_sess = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        posix_sess.apply(UserAction::StepRight);
        posix_sess.apply(UserAction::StepRight);
        for ch in r#""hello world"#.chars() {
            posix_sess.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(posix_sess.command_line(), r#"git -m "hello world""#);

        // Internal quotes escape according to dialect (`\"` vs `` `" ``).
        posix_sess.apply(UserAction::ClearFilter);
        for ch in r#"say "hi" world"#.chars() {
            posix_sess.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(posix_sess.command_line(), r#"git -m "say \"hi\" world""#);

        let mut ps_sess = MillerSession::new(
            vec![git],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        ps_sess.apply(UserAction::StepRight);
        ps_sess.apply(UserAction::StepRight);
        for ch in r#"say "hi" world"#.chars() {
            ps_sess.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(ps_sess.command_line(), "git -m \"say `\"hi`\" world\"");
    }

    #[test]
    fn test_initial_seeding_with_runtime_context_unmatched_single_token_and_empty_async() {
        let mut checkout = Entry::new("checkout", "checkout", EntryKind::Subcmd, "Checkout");
        checkout.children = vec![Entry::new("-b", "-b", EntryKind::Param, "New branch")];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![checkout];

        let ctx = RuntimeContext {
            git_branch: Some("main".to_string()),
            git_branches: vec!["main".to_string(), "feat/ui".to_string()],
            ..RuntimeContext::default()
        };

        let session = MillerSession::new(
            vec![
                Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo"),
                git.clone(),
            ],
            FakeIntrospector::empty(),
            "git checkout feat/ui",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
        .with_runtime_context(ctx);
        assert_eq!(session.view().active_pane, Pane::Third);
        assert_eq!(
            session.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("feat/ui")
        );
        assert_eq!(session.command_line(), "git checkout feat/ui");

        // Unmatched single token in --initial is preserved instead of replaced by index 0.
        let mut unmatched = MillerSession::new(
            vec![Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo")],
            FakeIntrospector::empty(),
            "customtool",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(unmatched.view().active_pane, Pane::First);
        assert_eq!(unmatched.command_line(), "customtool");
        unmatched.apply(UserAction::MoveDown);
        assert_eq!(unmatched.command_line(), "cargo");

        // Empty async introspection clears queued seeds once loaded.
        let bare_git = Entry::new("git", "git", EntryKind::Cli, "Git");
        let introspector = FakeIntrospector::with_chains([("git", Vec::new())]);
        let mut async_empty = MillerSession::new(
            vec![bare_git],
            introspector.clone(),
            "git mystery arg",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(async_empty.command_line(), "git mystery arg");
        introspector.complete_next();
        assert!(async_empty.poll_discovery());
        assert_eq!(async_empty.command_line(), "git mystery arg");
    }

    #[test]
    fn test_pane1_filter_accept_preserves_command_and_parent_change_clears_child_filters() {
        let mut cargo = Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo");
        cargo.children = vec![
            Entry::new("build", "build", EntryKind::Subcmd, "Compile"),
            Entry::new("test", "test", EntryKind::Subcmd, "Run tests"),
        ];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![
            Entry::new("status", "status", EntryKind::Subcmd, "Status"),
            Entry::new("-m", "-m", EntryKind::Param, "Message"),
            Entry::new("--amend", "--amend", EntryKind::Flag, "Amend"),
        ];

        let mut session = MillerSession::new(
            vec![cargo, git],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        for ch in "gi".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(
            session.view().panes[0].selected().map(|e| e.text.as_str()),
            Some("git")
        );

        // Accepting filtered Pane 1 entry clears Pane 1 filter while staying on `git`.
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(session.view().panes[0].filter(), "");
        assert_eq!(
            session.view().panes[0].selected().map(|e| e.text.as_str()),
            Some("git")
        );

        // Type custom filter in Pane 3 under `-m`, step left to `--amend`: Pane 3 filter clears.
        session.apply(UserAction::MoveDown);
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("-m")
        );
        session.apply(UserAction::StepRight);
        for ch in "fix".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(session.view().panes[2].filter(), "fix");
        session.apply(UserAction::StepLeft);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.view().panes[2].filter(), "");

        // Type filter in Pane 2 on `git`, step left and move to `cargo`: Pane 2 filter clears.
        for ch in "st".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(session.view().panes[1].filter(), "st");
        session.apply(UserAction::Backspace); // clears filter char 't'
        session.apply(UserAction::Backspace); // clears filter char 's'
        for ch in "st".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        session.apply(UserAction::StepLeft);
        session.apply(UserAction::MoveUp);
        assert_eq!(
            session.view().panes[0].selected().map(|e| e.text.as_str()),
            Some("cargo")
        );
        assert_eq!(session.view().panes[1].filter(), "");
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("build")
        );
    }

    #[test]
    fn test_scope_root_leaf_commit_and_interactive_subcommand_flag_sliding_and_backspace() {
        let ctx = RuntimeContext {
            git_branches: vec!["main".to_string(), "dev".to_string()],
            cwd_entries: vec![crate::model::CwdEntry {
                name: "src".to_string(),
                is_dir: true,
            }],
            ..RuntimeContext::default()
        };
        let mut scope = Entry::scope_root(
            "./",
            "📁 ./ (Current Dir)",
            EntryKind::Path,
            "Current working directory",
        );
        scope.children = ctx.cwd_path_entries(false);

        let mut scope_session = MillerSession::new(
            vec![scope],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
        .with_runtime_context(ctx.clone());
        scope_session.apply(UserAction::StepRight);
        assert_eq!(scope_session.view().active_pane, Pane::Second);
        assert_eq!(
            scope_session.view().panes[1]
                .selected()
                .map(|e| e.text.as_str()),
            Some("src/")
        );
        assert_eq!(
            scope_session.view().panes[2]
                .selected()
                .map(|e| e.text.as_str()),
            Some("src/")
        );
        scope_session.apply(UserAction::AcceptToken);
        assert_eq!(scope_session.view().active_pane, Pane::Second);
        assert_eq!(
            scope_session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("src/".to_string())
        );

        // Multi-level interactive sliding onto `Param` child (`git checkout -b <branch>`)
        let mut checkout = Entry::new("checkout", "checkout", EntryKind::Subcmd, "Checkout branch");
        checkout.children = vec![
            Entry::new("-b", "-b", EntryKind::Param, "Create branch"),
            Entry::new("--track", "--track", EntryKind::Flag, "Track remote"),
        ];
        let mut remote = Entry::new("remote", "remote", EntryKind::Subcmd, "Remotes");
        remote.children = vec![Entry::new("add", "add", EntryKind::Subcmd, "Add remote")];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![
            Entry::new("-C", "-C", EntryKind::Param, "Working dir"),
            checkout,
            remote,
        ];

        let mut session = MillerSession::new(
            vec![git.clone()],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
        .with_runtime_context(ctx);
        session.apply(UserAction::StepRight); // Pane 2 on -C
        session.apply(UserAction::MoveDown); // Pane 2 on checkout
        session.apply(UserAction::StepRight); // Pane 3 of checkout (shows -b at index 2 after branches or first sub)
        while session.view().panes[2]
            .selected()
            .is_some_and(|e| e.text != "-b")
        {
            session.apply(UserAction::MoveDown);
        }
        session.apply(UserAction::StepRight);
        assert_eq!(session.view().active_pane, Pane::Third);
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("-b")
        );
        // Pane 3 starts on custom value placeholder for `-b`; moving down previews `main`.
        assert_eq!(session.command_line(), "git checkout -b");
        session.apply(UserAction::MoveDown);
        assert_eq!(session.command_line(), "git checkout -b main");

        // Backspace in Pane 3 pops `-b` back to Pane 2 WITHOUT ghost re-previewing `-b`.
        session.apply(UserAction::Backspace);
        assert_eq!(session.view().active_pane, Pane::Second);
        assert_eq!(session.command_line(), "git checkout");

        // Leading flag before sliding subcommand (`git -C . remote add`) backspaces cleanly.
        let mut leading = MillerSession::new(
            vec![git],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        leading.apply(UserAction::StepRight); // Pane 2 on `-C`
        leading.apply(UserAction::StepRight); // Pane 3 of `-C`
        leading.apply(UserAction::InsertChar('.'));
        leading.apply(UserAction::AcceptToken); // committed `git -C .`
        assert_eq!(leading.command_line(), "git -C .");
        leading.apply(UserAction::MoveDown); // checkout
        leading.apply(UserAction::MoveDown); // remote
        leading.apply(UserAction::StepRight); // Pane 3 of remote -> `add`
        assert_eq!(
            leading.view().panes[2].selected().map(|e| e.text.as_str()),
            Some("add")
        );
        leading.apply(UserAction::StepRight); // slides `remote`, commits `add`
        assert_eq!(leading.command_line(), "git -C . remote add");

        leading.apply(UserAction::Backspace); // pops `add`, stays in remote Pane 2
        assert_eq!(leading.view().active_pane, Pane::Second);
        assert_eq!(leading.command_line(), "git -C . remote");
        leading.apply(UserAction::Backspace); // pops `remote`, returns to root Pane 2 on `remote`
        assert_eq!(leading.view().active_pane, Pane::Second);
        assert_eq!(leading.command_line(), "git -C .");
        assert_eq!(
            leading.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("remote")
        );
    }

    #[test]
    fn test_caps_lock_ctrl_alt_bindings_ignore_control_chars() {
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::CONTROL)),
            Some(UserAction::MoveDown)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(KeyCode::Char('H'), KeyModifiers::ALT)),
            Some(UserAction::StepLeft)
        );
        assert_eq!(
            UserAction::from_key_event(KeyEvent::new(
                KeyCode::Char('\u{0007}'),
                KeyModifiers::NONE
            )),
            None
        );
    }

    #[test]
    fn test_pentest_pane3_custom_values_and_initial_buffer_across_dialects() {
        let mut echo = Entry::new("echo", "echo", EntryKind::Cli, "Print");
        echo.children = vec![Entry::new("--msg", "--msg", EntryKind::Param, "Message")];

        let dialects = [
            ShellDialect::PowerShell,
            ShellDialect::Posix,
            ShellDialect::Fish,
        ];

        for dialect in dialects {
            let mut session = MillerSession::new(
                vec![echo.clone()],
                FakeIntrospector::empty(),
                "",
                Viewport::new(120, 30),
                dialect,
            );
            session.apply(UserAction::StepRight);
            session.apply(UserAction::StepRight);

            let cases: [(&str, &str); 8] = [
                ("$(id)", "echo --msg '$(id)'"),
                ("`id`", "echo --msg '`id`'"),
                (
                    "'; evil; '",
                    match dialect {
                        ShellDialect::PowerShell => "echo --msg '''; evil; '''",
                        ShellDialect::Posix => r"echo --msg ''\''; evil; '\'''",
                        ShellDialect::Fish => r"echo --msg '\'; evil; \''",
                    },
                ),
                (r#""$(evil)""#, r#"echo --msg '"$(evil)"'"#),
                (
                    r#""a\"; evil #""#,
                    match dialect {
                        ShellDialect::Fish => r#"echo --msg '"a\\"; evil #"'"#,
                        _ => r#"echo --msg '"a\"; evil #"'"#,
                    },
                ),
                ("$env:x; evil", "echo --msg '$env:x; evil'"),
                ("; | & > <", "echo --msg '; | & > <'"),
                ("(id)", "echo --msg '(id)'"),
            ];

            for (payload, expected_cmd) in cases {
                session.apply(UserAction::ClearFilter);
                for ch in payload.chars() {
                    session.apply(UserAction::InsertChar(ch));
                }
                let cmd = session.command_line();
                assert_eq!(cmd, expected_cmd, "dialect={dialect:?}, payload={payload}");
            }

            // Control, newline, NUL, and BiDi override/mark chars injected via typing must be dropped.
            session.apply(UserAction::ClearFilter);
            for ch in "safe\n;\rid\0\x1b\u{061C}\u{200E}\u{200F}\u{202e}".chars() {
                session.apply(UserAction::InsertChar(ch));
            }
            assert_eq!(session.command_line(), "echo --msg 'safe;id'");
            assert!(
                !session.command_line().chars().any(is_disallowed_control),
                "control or BiDi characters leaked into command line: {:?}",
                session.command_line()
            );

            // Hostile `--initial` buffer with embedded CR/LF/NUL/ESC/BiDi must be sanitized.
            let seeded = MillerSession::new(
                vec![echo.clone()],
                FakeIntrospector::empty(),
                "echo --msg ok\n; id\r\n\0\x1b[31m\u{061C}\u{200E}\u{200F}\u{202e}",
                Viewport::new(120, 30),
                dialect,
            );
            let seeded_cmd = seeded.command_line();
            assert!(
                !seeded_cmd.chars().any(is_disallowed_control),
                "hostile initial buffer retained control/BiDi characters: {seeded_cmd:?}"
            );
        }
    }

    #[test]
    fn test_pentest_hostile_cwd_git_branch_and_env_var_selection_in_session() {
        let ctx = RuntimeContext {
            cwd_entries: vec![
                crate::model::CwdEntry {
                    name: "$(id)".to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: "`id`".to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: "'; evil; '".to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: r#""$(evil)""#.to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: "-rf; $(id)".to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: "bad\nfile".to_string(),
                    is_dir: false,
                },
                crate::model::CwdEntry {
                    name: "safe.txt".to_string(),
                    is_dir: false,
                },
            ],
            git_branches: vec![
                "$(whoami)".to_string(),
                "'; evil; '".to_string(),
                "-leading".to_string(),
                "bad\rbranch".to_string(),
                "main".to_string(),
            ],
            ..RuntimeContext::default()
        };

        let mut var_root = Entry::scope_root("$", "Shell variables", EntryKind::Var, "");
        var_root.children = vec![
            Entry::new("$GIT_BRANCH", "GIT_BRANCH", EntryKind::Var, ""),
            Entry::new("$x; evil", "bad3", EntryKind::Var, ""),
            Entry::new("$x\nevil", "bad4", EntryKind::Var, ""),
        ];

        let cwd_root = Entry::scope_root("./", "Current directory", EntryKind::Subcmd, "");

        let mut checkout = Entry::new("checkout", "checkout", EntryKind::Subcmd, "Checkout branch");
        checkout.children = vec![Entry::new("-b", "-b", EntryKind::Param, "New branch")];
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
        git.children = vec![checkout];

        for dialect in [
            ShellDialect::PowerShell,
            ShellDialect::Posix,
            ShellDialect::Fish,
        ] {
            let safe_env_name = if dialect == ShellDialect::PowerShell {
                "$env:USER"
            } else {
                "$USER"
            };
            let mut env_root =
                Entry::scope_root("$env:", "Environment variables", EntryKind::Env, "");
            let mut safe_env = Entry::new(safe_env_name, "USER", EntryKind::Env, "");
            safe_env.children = vec![
                Entry::new("alice", "alice", EntryKind::Value, "Safe"),
                Entry::new(r#""$(evil)""#, "hostile", EntryKind::Value, "Hostile"),
            ];
            env_root.children = vec![
                safe_env,
                Entry::new("$env:x; evil", "bad1", EntryKind::Env, ""),
                Entry::new("$env:$(id)", "bad2", EntryKind::Env, ""),
                Entry::new("$env:ProgramFiles(x86)", "pf", EntryKind::Env, ""),
            ];

            let mut session = MillerSession::new(
                vec![env_root, var_root.clone(), cwd_root.clone(), git.clone()],
                FakeIntrospector::empty(),
                "",
                Viewport::new(120, 30),
                dialect,
            )
            .with_runtime_context(ctx.clone());

            // 1. `$env:` scope drops hostile var names and sanitizes literal `"$(evil)"` child.
            session.apply(UserAction::StepRight);
            let env_texts: Vec<&str> = session.view().panes[1]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert!(!env_texts.contains(&"$env:x; evil"));
            assert!(!env_texts.contains(&"$env:$(id)"));
            assert!(env_texts.contains(&safe_env_name));
            if dialect == ShellDialect::PowerShell {
                assert!(env_texts.contains(&"${env:ProgramFiles(x86)}"));
            } else {
                assert!(!env_texts.contains(&"$env:ProgramFiles(x86)"));
            }

            session.apply(UserAction::StepRight);
            let env_vals: Vec<&str> = session.view().panes[2]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert_eq!(env_vals, vec!["alice", "'$(evil)'"]);
            session.apply(UserAction::MoveDown);
            session.apply(UserAction::AcceptToken);
            assert_eq!(session.command_line(), "'$(evil)'");

            // 2. `$` variable scope drops hostile names while keeping `$GIT_BRANCH`.
            session.apply(UserAction::StepLeft);
            session.apply(UserAction::StepLeft);
            session.apply(UserAction::MoveDown);
            session.apply(UserAction::StepRight);
            let var_texts: Vec<&str> = session.view().panes[1]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert_eq!(var_texts, vec!["$GIT_BRANCH"]);

            // 3. `./` CWD scope drops newline filenames, guards leading `-`, and quotes metacharacters.
            session.apply(UserAction::StepLeft);
            session.apply(UserAction::MoveDown);
            session.apply(UserAction::StepRight);
            let cwd_texts: Vec<&str> = session.view().panes[1]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert_eq!(
                cwd_texts,
                vec![
                    "'$(id)'",
                    "'`id`'",
                    match dialect {
                        ShellDialect::PowerShell => "'''; evil; '''",
                        ShellDialect::Posix => r"''\''; evil; '\'''",
                        ShellDialect::Fish => r"'\'; evil; \''",
                    },
                    r#"'"$(evil)"'"#,
                    "'./-rf; $(id)'",
                    "safe.txt",
                ]
            );
            session.apply(UserAction::AcceptToken);
            assert_eq!(
                session.apply(UserAction::Confirm),
                SessionOutcome::Confirmed("'$(evil)' '$(id)'".to_string())
            );

            // 4. `git checkout` rejects leading `-` and `\r` branches and quotes hostile names.
            let mut git_sess = MillerSession::new(
                vec![git.clone()],
                FakeIntrospector::empty(),
                "",
                Viewport::new(120, 30),
                dialect,
            )
            .with_runtime_context(ctx.clone());
            git_sess.apply(UserAction::StepRight);
            git_sess.apply(UserAction::StepRight);
            let branch_texts: Vec<&str> = git_sess.view().panes[2]
                .entries()
                .iter()
                .filter(|e| e.kind == EntryKind::Arg)
                .map(|e| e.text.as_str())
                .collect();
            assert_eq!(
                branch_texts,
                vec![
                    "'$(whoami)'",
                    match dialect {
                        ShellDialect::PowerShell => "'''; evil; '''",
                        ShellDialect::Posix => r"''\''; evil; '\'''",
                        ShellDialect::Fish => r"'\'; evil; \''",
                    },
                    "main",
                ]
            );
            assert_eq!(
                git_sess.apply(UserAction::Confirm),
                SessionOutcome::Confirmed("git checkout '$(whoami)'".to_string())
            );

            // 5. Pre-populated `EntryKind::Path` children neutralize interior quotes/backslashes,
            // leading `-` option injection, and BiDi marks.
            let mut static_paths_root =
                Entry::scope_root("./", "Pre-populated paths", EntryKind::Subcmd, "");
            static_paths_root.children = vec![
                Entry::new(r#""a" --evil=1""#, "breakout", EntryKind::Path, ""),
                Entry::new("--upload-pack=evil", "opt", EntryKind::Path, ""),
                Entry::new("bad\u{200E}mark.txt", "bidi", EntryKind::Path, ""),
                Entry::new(r#""C:\temp\dir""#, "win", EntryKind::Path, ""),
                Entry::new(r#""my file.txt""#, "benign", EntryKind::Path, ""),
            ];
            let mut static_sess = MillerSession::new(
                vec![static_paths_root],
                FakeIntrospector::empty(),
                "",
                Viewport::new(120, 30),
                dialect,
            );
            static_sess.apply(UserAction::StepRight);
            let static_texts: Vec<&str> = static_sess.view().panes[1]
                .entries()
                .iter()
                .map(|e| e.text.as_str())
                .collect();
            assert_eq!(
                static_texts,
                vec![
                    r#"'"a" --evil=1"'"#,
                    "./--upload-pack=evil",
                    match dialect {
                        ShellDialect::Fish => r#"'"C:\\temp\\dir"'"#,
                        _ => r#"'"C:\temp\dir"'"#,
                    },
                    r#""my file.txt""#,
                ]
            );
        }
    }

    #[test]
    fn test_perf_rapid_navigation_and_parameter_set_cache_stability() {
        let mut catalog = Vec::with_capacity(5_000);
        for cmd_idx in 0..5_000 {
            let mut children = Vec::with_capacity(15);
            children.push(
                Entry::new("-Alpha", "-Alpha", EntryKind::Param, "Set A")
                    .with_parameter_sets(vec!["SetA".to_string()]),
            );
            children.push(
                Entry::new("-AlphaExtra", "-AlphaExtra", EntryKind::Flag, "Set A flag")
                    .with_parameter_sets(vec!["SetA".to_string()]),
            );
            children.push(
                Entry::new("-Beta", "-Beta", EntryKind::Param, "Set B")
                    .with_parameter_sets(vec!["SetB".to_string()]),
            );
            children.push(Entry::new(
                "-Verbose",
                "-Verbose",
                EntryKind::Flag,
                "Common",
            ));
            for p_idx in 4..15 {
                let name = format!("-Param{p_idx}");
                children.push(
                    Entry::new(name.clone(), name, EntryKind::Param, "Desc")
                        .with_parameter_sets(vec!["SetA".to_string(), "SetB".to_string()]),
                );
            }
            let cmd_name = format!("Invoke-Task{cmd_idx:04}");
            let mut cmd = Entry::new(cmd_name.clone(), cmd_name, EntryKind::Cmdlet, "Task");
            cmd.children = children;
            catalog.push(cmd);
        }

        let ctx = RuntimeContext {
            cwd_entries: (0..64)
                .map(|i| crate::model::CwdEntry {
                    name: format!("file_{i:02}.rs"),
                    is_dir: false,
                })
                .collect(),
            git_branches: (0..16).map(|i| format!("feature/{i:02}")).collect(),
            ..RuntimeContext::default()
        };

        let mut session = MillerSession::new(
            catalog,
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        )
        .with_runtime_context(ctx);

        // Rapid Pane 1 navigation should keep runtime context cached and stay responsive.
        for step in 1..=200 {
            session.apply(UserAction::MoveDown);
            let cmd = session.command_line();
            assert_eq!(cmd, format!("Invoke-Task{step:04}"));
        }

        // Verify Parameter Set cache stability: committing universal `-Verbose` preserves all 15
        // params; committing `-Alpha` narrows to SetA (`14` entries, excluding `-Beta`), and then
        // accepting `-AlphaExtra` within the same SetA preserves the SetA view.
        session.apply(UserAction::StepRight);
        assert_eq!(session.view().panes[1].entries().len(), 15);
        assert_eq!(session.view().active_param_set, None);

        while session.view().panes[1]
            .selected()
            .is_some_and(|e| e.text != "-Verbose")
        {
            session.apply(UserAction::MoveDown);
        }
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().panes[1].entries().len(), 15);
        assert_eq!(session.view().active_param_set, None);

        session.apply(UserAction::MoveHome);
        session.apply(UserAction::StepRight); // Pane 3 of `-Alpha`
        session.apply(UserAction::InsertChar('v'));
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_param_set, Some("SetA"));
        let set_a_names: Vec<&str> = session.view().panes[1]
            .entries()
            .iter()
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(set_a_names.len(), 14);
        assert!(!set_a_names.contains(&"-Beta"));

        session.apply(UserAction::MoveDown); // `-AlphaExtra` (Switch/Flag in SetA)
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.view().active_param_set, Some("SetA"));
        assert_eq!(
            session.command_line(),
            "Invoke-Task0200 -Verbose -Alpha v -AlphaExtra"
        );
    }

    #[test]
    fn test_perf_step_right_and_accept_without_subtree_clone() {
        let leaf_template: Vec<Entry> = (0..20)
            .map(|k| {
                let name = format!("--arg{k:02}");
                Entry::new(name.clone(), name, EntryKind::Param, "Leaf parameter")
            })
            .collect();

        let mut root_cmd = Entry::new("az", "az", EntryKind::Cli, "Azure CLI");
        let mut svc_children = Vec::with_capacity(20);
        for i in 0..20 {
            let svc_name = format!("svc{i:02}");
            let mut svc = Entry::new(svc_name.clone(), svc_name, EntryKind::Subcmd, "Service");
            let mut op_children = Vec::with_capacity(26);
            for j in 0..25 {
                let op_name = format!("op{j:02}");
                let mut op = Entry::new(op_name.clone(), op_name, EntryKind::Subcmd, "Operation");
                op.children = leaf_template.clone();
                op_children.push(op);
            }
            let mut dry_run = Entry::new("--dry-run", "--dry-run", EntryKind::Flag, "Dry run");
            dry_run.children = vec![Entry::placeholder(
                "(Flag - No Value Needed)",
                EntryKind::Flag,
                "Valueless flag",
            )];
            op_children.push(dry_run);
            svc.children = op_children;
            svc_children.push(svc);
        }
        root_cmd.children = svc_children;

        let mut session = MillerSession::new(
            vec![root_cmd],
            FakeIntrospector::empty(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        assert_eq!(session.command_line(), "az");

        const ITERS: usize = 500;
        let step_start = std::time::Instant::now();
        for _ in 0..ITERS {
            session.apply(UserAction::StepRight); // Pane 1 -> Pane 2 (`svc00`)
            session.apply(UserAction::StepRight); // Pane 2 -> Pane 3 (`op00`)
            session.apply(UserAction::StepLeft); // Pane 3 -> Pane 2
            session.apply(UserAction::StepLeft); // Pane 2 -> Pane 1
        }
        let step_elapsed = step_start.elapsed();
        assert_eq!(session.command_line(), "az svc00");

        session.apply(UserAction::Backspace); // pop `svc00`
        session.apply(UserAction::Backspace); // pop `az`
        assert_eq!(session.view().active_pane, Pane::First);

        let accept_start = std::time::Instant::now();
        for _ in 0..ITERS {
            session.apply(UserAction::InsertChar('a'));
            session.apply(UserAction::AcceptToken); // Pane 1 with filter -> Pane 2
            session.apply(UserAction::AcceptToken); // Pane 2 (`svc00`) -> Pane 3
            session.apply(UserAction::StepLeft); // Pane 3 -> Pane 2
            session.apply(UserAction::StepLeft); // Pane 2 -> Pane 1
        }
        let accept_elapsed = accept_start.elapsed();

        // Also verify Pane 3 subcommand sliding and Pane 2 valueless flag accept over deep tree.
        session.apply(UserAction::StepRight); // Pane 1 -> Pane 2 (`svc00`)
        session.apply(UserAction::StepRight); // Pane 2 -> Pane 3 (`op00`)
        session.apply(UserAction::AcceptToken); // slides into `svc00` (Pane 3 of `op00`)
        assert_eq!(session.command_line(), "az svc00 op00 --arg00");
        session.apply(UserAction::StepLeft); // `svc00` Pane 2
        session.apply(UserAction::MoveEnd); // `--dry-run`
        session.apply(UserAction::AcceptToken); // commits valueless Flag
        assert_eq!(session.command_line(), "az svc00 op00 --dry-run");

        eprintln!(
            "[perf] test_perf_step_right_and_accept_without_subtree_clone: iters={ITERS} step_right={:.2}ms ({:.1}us/iter) accept_token={:.2}ms ({:.1}us/iter)",
            step_elapsed.as_secs_f64() * 1000.0,
            step_elapsed.as_secs_f64() * 1_000_000.0 / ITERS as f64,
            accept_elapsed.as_secs_f64() * 1000.0,
            accept_elapsed.as_secs_f64() * 1_000_000.0 / ITERS as f64,
        );

        assert!(
            step_elapsed < std::time::Duration::from_secs(2),
            "StepRight over deep Entry tree took too long: {step_elapsed:?}"
        );
        assert!(
            accept_elapsed < std::time::Duration::from_secs(2),
            "AcceptToken over deep Entry tree took too long: {accept_elapsed:?}"
        );
        assert_eq!(
            session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("az svc00 op00 --dry-run".to_string())
        );
    }

    #[test]
    fn test_pentest_pane3_seeding_and_hostile_introspector_entries() {
        let mut kubectl = Entry::new("kubectl", "kubectl", EntryKind::Cli, "Kubernetes");
        let mut get = Entry::new("get", "get", EntryKind::Subcmd, "Get resources");
        get.children = vec![
            Entry::new("pods", "pods", EntryKind::Subcmd, "Pods"),
            Entry::new("bad\n;id", "bad", EntryKind::Subcmd, "Control injection"),
            Entry::new(
                "evil\u{202E}txt",
                "bidi",
                EntryKind::Param,
                "BiDi injection",
            ),
            Entry::new("$BAD;id", "badvar", EntryKind::Var, "Unsafe variable"),
        ];
        kubectl.children = vec![get];

        // Multi-token seeded initial buffer with embedded escapes/controls/BiDi must never panic
        // when draining `pending_pane3_seeds` and must emit zero disallowed controls.
        let mut seeded = MillerSession::new(
            vec![kubectl.clone()],
            FakeIntrospector::empty(),
            "kubectl get pods \x1b[31m--all\r\n;id\0\u{202E}\u{2028}",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        let seeded_cmd = seeded.command_line();
        assert!(!seeded_cmd.chars().any(is_disallowed_control));
        assert!(seeded_cmd.starts_with("kubectl get pods"));
        seeded.apply(UserAction::StepRight);
        seeded.apply(UserAction::AcceptToken);
        seeded.apply(UserAction::Backspace);
        assert!(!seeded.command_line().chars().any(is_disallowed_control));

        // Hostile introspector output containing control/BiDi chars and unsafe variables must
        // never leak into the assembled Command Line on StepRight or AcceptToken.
        let mut target_param = Entry::new("--target", "--target", EntryKind::Param, "Target");
        target_param.children = vec![
            Entry::new("evil\nval", "evil_nl", EntryKind::Value, "Bad value"),
            Entry::new("$EVIL;id", "evil_var", EntryKind::Var, "Bad var"),
            Entry::new("ok.txt", "ok.txt", EntryKind::Value, "Safe value"),
        ];
        let introspector = FakeIntrospector::empty();
        introspector.push_ready(
            "mycli",
            vec![
                Entry::new("bad\n;id", "bad_sub", EntryKind::Subcmd, "Newline"),
                Entry::new("$EVIL;id", "bad_var", EntryKind::Var, "Unsafe var"),
                Entry::new("safe\x1b[31m", "ansi_sub", EntryKind::Subcmd, "ANSI"),
                target_param,
                Entry::new("clean", "clean", EntryKind::Subcmd, "Clean"),
            ],
        );

        let mut sess = MillerSession::new(
            vec![Entry::new("mycli", "mycli", EntryKind::Cli, "CLI")],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );
        sess.poll_discovery();
        sess.apply(UserAction::StepRight); // Pane 2 on `bad\n;id` (unsafe)
        sess.apply(UserAction::StepRight); // Pane 3 of unsafe entry
        assert!(!sess.command_line().contains("bad"));
        assert!(!sess.command_line().chars().any(is_disallowed_control));
        sess.apply(UserAction::StepLeft);
        sess.apply(UserAction::MoveDown); // `$EVIL;id`
        sess.apply(UserAction::AcceptToken);
        assert!(!sess.command_line().contains("$EVIL"));
        sess.apply(UserAction::StepLeft);
        sess.apply(UserAction::MoveDown); // `safe\x1b[31m`
        sess.apply(UserAction::AcceptToken);
        assert!(!sess.command_line().chars().any(is_disallowed_control));
        sess.apply(UserAction::StepLeft);
        sess.apply(UserAction::MoveDown); // `--target`
        sess.apply(UserAction::StepRight); // Pane 3 on `evil\nval`
        sess.apply(UserAction::AcceptToken); // ignored unsafe Value
        assert!(!sess.command_line().contains("evil"));
        sess.apply(UserAction::MoveDown); // `$EVIL;id` in Pane 3
        sess.apply(UserAction::AcceptToken); // ignored unsafe Var
        assert!(!sess.command_line().contains("$EVIL"));
        sess.apply(UserAction::MoveDown); // `ok.txt`
        sess.apply(UserAction::AcceptToken);
        assert_eq!(sess.command_line(), "mycli --target ok.txt");
        sess.apply(UserAction::MoveEnd); // `clean`
        sess.apply(UserAction::AcceptToken);
        assert_eq!(
            sess.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("mycli --target ok.txt clean".to_string())
        );
    }

    fn ps_catalog_with_duplicate_git(with_cmdlets: bool) -> Vec<Entry> {
        // Mirrors the PowerShell catalog: shell history first, then scope roots,
        // then the PATH executable that shares its text with a history entry.
        let mut catalog = vec![
            Entry::new("git", "git", EntryKind::History, "Recent shell history"),
            Entry::scope_root("$env:", "$env:Variables", EntryKind::Env, ""),
            Entry::scope_root("$", "$global:Variables", EntryKind::Var, ""),
            Entry::scope_root("./", "./ Current Dir", EntryKind::Path, ""),
            Entry::new("git", "git", EntryKind::Cli, "Tool"),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Tool"),
        ];
        if with_cmdlets {
            for i in 0..50 {
                let name = format!("Invoke-Task{i:03}");
                catalog.push(Entry::new(&name, &name, EntryKind::Cmdlet, "Task"));
            }
        }
        catalog
    }

    #[test]
    fn test_catalog_refresh_keeps_duplicate_text_selection_in_powershell() {
        let fake = FakeIntrospector::empty();
        let mut sess = MillerSession::new(
            ps_catalog_with_duplicate_git(false),
            fake.clone(),
            "",
            Viewport::new(100, 24),
            ShellDialect::PowerShell,
        );

        // Walk down to the PATH `git` entry. The history `git` at index 0 shares
        // its text, so a refresh must not reselect the wrong duplicate.
        for _ in 0..4 {
            sess.apply(UserAction::MoveDown);
        }
        let selected = sess.view().panes[0].selected().cloned().expect("selection");
        assert_eq!(selected.text, "git");
        assert_eq!(selected.kind, EntryKind::Cli);
        assert_eq!(sess.view().panes[0].sel_idx(), 4);

        // The first-run PowerShell metadata sync (or Ctrl+R "Sync PS") swaps the
        // catalog in place while the user is browsing.
        fake.push_catalog_reload(ps_catalog_with_duplicate_git(true));
        assert!(sess.poll_discovery());

        let view = sess.view();
        let selected = view.panes[0].selected().cloned().expect("selection");
        assert_eq!(
            selected.kind,
            EntryKind::Cli,
            "refresh jumped from the PATH `git` to the history `git` at the top"
        );
        assert_eq!(selected.text, "git");
        assert_eq!(
            view.panes[0].sel_idx(),
            4,
            "refresh moved the marker to the top"
        );
    }

    fn git_chain_fake() -> FakeIntrospector {
        FakeIntrospector::with_chains([
            (
                "git",
                vec![
                    Entry::new("clone", "clone", EntryKind::Subcmd, "Clone"),
                    Entry::new("commit", "commit", EntryKind::Subcmd, "Commit"),
                    Entry::new("rebase", "rebase", EntryKind::Subcmd, "Rebase"),
                ],
            ),
            (
                "git::commit",
                vec![
                    Entry::new("--all", "--all", EntryKind::Flag, "All"),
                    Entry::new("--message", "--message", EntryKind::Param, "Msg"),
                    Entry::new("--amend", "--amend", EntryKind::Flag, "Amend"),
                ],
            ),
        ])
    }

    fn git_session(fake: &FakeIntrospector) -> MillerSession<FakeIntrospector> {
        MillerSession::new(
            vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")],
            fake.clone(),
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        )
    }

    #[test]
    fn test_pane3_background_load_preserves_pane2_selection() {
        let fake = git_chain_fake();
        let mut session = git_session(&fake);

        // Pane 2 starts as a loading placeholder; deliver the subcommands.
        fake.complete_next();
        assert!(session.poll_discovery());

        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveDown); // commit
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("commit")
        );

        // Selecting `commit` starts a Pane 3 introspection. Its completion must
        // not move the Pane 2 marker back to the first subcommand.
        assert!(session.view().panes[2]
            .entries()
            .iter()
            .any(Entry::is_loading));
        fake.complete_next();
        assert!(session.poll_discovery());

        let view = session.view();
        assert_eq!(
            view.panes[1].selected().map(|e| e.text.as_str()),
            Some("commit"),
            "Pane 3 load reset the Pane 2 selection"
        );
        assert_eq!(view.panes[1].sel_idx(), 1);
    }

    #[test]
    fn test_catalog_reload_preserves_pane2_selection_when_command_unchanged() {
        let fake = git_chain_fake();
        let mut session = git_session(&fake);

        fake.complete_next();
        assert!(session.poll_discovery());

        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::MoveDown); // rebase
        assert_eq!(
            session.view().panes[1].selected().map(|e| e.text.as_str()),
            Some("rebase")
        );

        // Background metadata sync replaces the command catalog while the same
        // command stays selected in Pane 1. Cmdlets are inserted before it, so
        // its source index shifts.
        fake.push_catalog_reload(vec![
            Entry::new("Invoke-One", "Invoke-One", EntryKind::Cmdlet, "Cmdlet"),
            Entry::new("Invoke-Two", "Invoke-Two", EntryKind::Cmdlet, "Cmdlet"),
            Entry::new("git", "git", EntryKind::Cli, "Git VCS"),
            Entry::new("cargo", "cargo", EntryKind::Cli, "Cargo"),
        ]);
        assert!(session.poll_discovery());

        let view = session.view();
        assert_eq!(
            view.panes[0].selected().map(|e| e.text.as_str()),
            Some("git")
        );
        assert_eq!(
            view.panes[0].sel_idx(),
            2,
            "catalog reload should re-find `git`"
        );
        assert_eq!(
            view.panes[1].selected().map(|e| e.text.as_str()),
            Some("rebase"),
            "catalog reload reset the Pane 2 selection"
        );
        assert_eq!(view.panes[1].sel_idx(), 2);
    }
}
