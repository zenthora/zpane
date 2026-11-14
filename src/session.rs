use crate::discovery::{CliIntrospector, Introspect, ShellDialect};
use crate::fuzzy::PaneList;
use crate::model::{Entry, EntryKind, Pane, RuntimeContext};
use crate::ui::{render_frame, RenderState, Viewport};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::VecDeque;

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
            (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                Some(UserAction::Cancel)
            }
            (KeyCode::Enter, _) => Some(UserAction::Confirm),
            (KeyCode::Char('r'), KeyModifiers::CONTROL) => Some(UserAction::ReloadCatalog),
            (KeyCode::Char('u'), KeyModifiers::CONTROL) => Some(UserAction::ClearFilter),
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
                if !mods.contains(KeyModifiers::CONTROL) && !mods.contains(KeyModifiers::ALT) =>
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Pane3CacheState {
    p1_entry_idx: Option<usize>,
    p2_entry_idx: Option<usize>,
    pane3_filter: String,
}

type CatalogLoader = Box<dyn FnMut(ShellDialect) -> Vec<Entry>>;

/// Cohesive Command Line assembler owning committed tokens, multi-token initial seeding,
/// and live-cursor preview suppression rules across all three Miller Panes.
#[derive(Debug, Default, Clone)]
struct CommandLine {
    tokens: Vec<String>,
    pending_pane2_seed: Option<String>,
    pending_pane3_seeds: VecDeque<String>,
    suppress_pane2_preview: bool,
    suppress_pane3_preview: bool,
}

impl CommandLine {
    fn from_initial_buffer(initial_buffer: &str, pane1: &mut PaneList) -> (Self, Pane) {
        let mut active_pane = Pane::First;
        let tokens: Vec<String> = initial_buffer
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        if let Some(first_tok) = tokens.first() {
            if pane1.select_first_matching(|it| it.text.eq_ignore_ascii_case(first_tok)) {
                active_pane = Pane::Second;
            }
        }

        let (pending_pane2_seed, pending_pane3_seeds) = if active_pane == Pane::Second {
            (
                tokens.get(1).cloned(),
                tokens
                    .get(2..)
                    .map(|s| s.iter().cloned().collect())
                    .unwrap_or_default(),
            )
        } else {
            (None, VecDeque::new())
        };

        (
            Self {
                tokens,
                pending_pane2_seed,
                pending_pane3_seeds,
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
        if self.tokens.last().map(String::as_str) != Some(token.as_str()) {
            self.tokens.push(token);
        }
    }

    fn ensure_pane1_token(&mut self, p1: Option<&Entry>) {
        if let Some(p1) = p1 {
            if p1.is_emittable() && self.tokens.is_empty() {
                self.tokens.push(p1.text.clone());
            }
        }
    }

    fn commit_pane1_entry(&mut self, p1: &Entry) {
        if !p1.is_emittable() {
            return;
        }
        if self.tokens.is_empty() {
            self.tokens.push(p1.text.clone());
        } else if !self.tokens[0].eq_ignore_ascii_case(&p1.text) {
            self.tokens[0] = p1.text.clone();
            self.tokens.truncate(1);
        }
    }

    fn commit_pane2_entry(&mut self, p1: Option<&Entry>, p2: &Entry) {
        self.ensure_pane1_token(p1);
        if !p1.is_some_and(Entry::is_scope_root) && p2.is_emittable() && !p2.is_loading() {
            self.push_token_dedup(p2.text.clone());
        }
    }

    fn pop_token(&mut self) -> Option<String> {
        self.pending_pane2_seed = None;
        self.pending_pane3_seeds.clear();
        self.tokens.pop()
    }

    fn resume_preview(&mut self, pane: Pane) {
        match pane {
            Pane::First => {}
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
    ) -> String {
        let mut out = self.tokens.clone();

        if out.is_empty() {
            if let Some(p1) = p1.filter(|e| e.is_emittable()) {
                out.push(p1.text.clone());
            }
        } else if active_pane == Pane::First && out.len() == 1 {
            if let Some(p1) = p1.filter(|e| e.is_emittable()) {
                out[0] = p1.text.clone();
            }
        }

        match active_pane {
            Pane::First => {}
            Pane::Second => {
                if !self.suppress_pane2_preview {
                    if let Some(p2) = p2.filter(|e| e.is_emittable() && !e.is_loading()) {
                        if out.last().map(String::as_str) != Some(p2.text.as_str()) {
                            out.push(p2.text.clone());
                        }
                    }
                }
            }
            Pane::Third => {
                if let (Some(p1), Some(p2)) = (p1, p2) {
                    if !p1.is_scope_root()
                        && p2.is_emittable()
                        && !p2.is_loading()
                        && out.len() <= 1
                        && out.last().map(String::as_str) != Some(p2.text.as_str())
                    {
                        out.push(p2.text.clone());
                    }
                }
                if !self.suppress_pane3_preview {
                    if let Some(v) = resolve_pane3_token(p3, pane3_filter) {
                        if out.last().map(String::as_str) != Some(v.as_str()) {
                            out.push(v);
                        }
                    }
                }
            }
        }

        out.join(" ")
    }
}

fn extend_dedup(target: &mut Vec<Entry>, additions: Vec<Entry>) {
    let mut existing: std::collections::HashSet<String> =
        target.iter().map(|e| e.text.to_ascii_lowercase()).collect();
    for entry in additions {
        if existing.insert(entry.text.to_ascii_lowercase()) {
            target.push(entry);
        }
    }
}

fn expand_runtime_entries(children: &[Entry], ctx: &RuntimeContext) -> Vec<Entry> {
    let mut out = Vec::with_capacity(children.len());
    for child in children {
        if child.is_placeholder() && child.label == "$files" {
            let cwd = ctx.cwd_path_entries(false);
            if cwd.is_empty() {
                out.push(Entry::placeholder(
                    "(No files in current directory)",
                    EntryKind::Path,
                    "Expanded from spec $files macro",
                ));
            } else {
                out.extend(cwd);
            }
        } else if child.is_placeholder() && child.label == "$directories" {
            let dirs = ctx.cwd_path_entries(true);
            if dirs.is_empty() {
                out.push(Entry::placeholder(
                    "(No directories in current directory)",
                    EntryKind::Path,
                    "Expanded from spec $directories macro",
                ));
            } else {
                out.extend(dirs);
            }
        } else {
            out.push(child.clone());
        }
    }
    out
}

fn history_command_chain(cmd: &str) -> Vec<&str> {
    cmd.split_whitespace()
        .take_while(|tok| !tok.starts_with('-'))
        .take(2)
        .collect()
}

fn primary_exe_for_entry(entry: &Entry) -> &str {
    if entry.kind == EntryKind::History {
        entry
            .text
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
    matches!(
        p2.text.to_ascii_lowercase().as_str(),
        "checkout"
            | "switch"
            | "branch"
            | "merge"
            | "rebase"
            | "diff"
            | "log"
            | "push"
            | "pull"
            | "reset"
            | "cherry-pick"
            | "show"
            | "tag"
            | "-b"
            | "--track"
    ) || p2.text == "-B"
}

fn resolve_pane2_entries<I: Introspect>(
    pane1_entry: Option<&Entry>,
    tokens: &[String],
    introspector: &mut I,
) -> (Vec<Entry>, Option<String>) {
    let Some(p1) = pane1_entry else {
        return (Vec::new(), None);
    };

    if p1.is_loading() {
        return (Vec::new(), None);
    }

    if !p1.children.is_empty() {
        return p1.children_for_tokens(tokens);
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

fn resolve_pane3_entries<I: Introspect>(
    pane1_entry: Option<&Entry>,
    pane2_entry: Option<&Entry>,
    introspector: &mut I,
    pane3_filter: &str,
    dialect: ShellDialect,
    runtime_ctx: &RuntimeContext,
) -> Vec<Entry> {
    let (Some(p1), Some(p2)) = (pane1_entry, pane2_entry) else {
        return Vec::new();
    };

    if p2.is_loading() {
        return Vec::new();
    }

    let p1_exe = primary_exe_for_entry(p1);
    let mut entries = Vec::new();
    let trimmed_input = pane3_filter.trim();

    if !trimmed_input.is_empty() {
        entries.push(Entry::new(
            trimmed_input,
            format!("✏ Use \"{trimmed_input}\""),
            EntryKind::Value,
            format!("Insert custom value for {}", p2.label),
        ));
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
            extend_dedup(&mut entries, runtime_ctx.git_branch_entries());
        }
        extend_dedup(
            &mut entries,
            expand_runtime_entries(&p2.children, runtime_ctx),
        );
        if matches!(p2.kind, EntryKind::Param | EntryKind::Flag) {
            extend_dedup(&mut entries, dialect.common_var_entries());
        }
        return entries;
    }

    if p2.kind == EntryKind::Subcmd {
        let sub_entries = introspector.query(&[p1_exe, p2.text.as_str()]);
        if sub_entries.iter().any(Entry::is_loading) {
            entries.extend(sub_entries);
            return entries;
        }
        if is_git_branch_target(p1_exe, p2) {
            extend_dedup(&mut entries, runtime_ctx.git_branch_entries());
        }
        extend_dedup(&mut entries, sub_entries);
        extend_dedup(&mut entries, runtime_ctx.cwd_path_entries(false));
        return entries;
    }

    if !p1.is_scope_root() {
        if is_git_branch_target(p1_exe, p2) {
            extend_dedup(&mut entries, runtime_ctx.git_branch_entries());
        }
        if p2.kind == EntryKind::Param {
            extend_dedup(&mut entries, runtime_ctx.cwd_path_entries(false));
        }
    }

    extend_dedup(&mut entries, dialect.common_var_entries());
    entries
}

fn resolve_pane3_token(cur_p3: Option<&Entry>, pane3_filter: &str) -> Option<String> {
    let trimmed_filter = pane3_filter.trim();
    if let Some(p3) = cur_p3 {
        if p3.is_loading() {
            None
        } else if p3.is_emittable() {
            Some(p3.text.clone())
        } else if !trimmed_filter.is_empty() {
            Some(trimmed_filter.to_string())
        } else {
            None
        }
    } else if !trimmed_filter.is_empty() {
        Some(trimmed_filter.to_string())
    } else {
        None
    }
}

/// Deep interactive session state machine owning the 3-pane Miller catalogs,
/// per-pane fuzzy filters, selection/scroll cursors, unified Command Line
/// assembly/seeding state, and active PowerShell Parameter Set metadata.
pub struct MillerSession<I: Introspect = CliIntrospector> {
    panes: [PaneList; 3],
    pane2_cache_state: Option<Pane2CacheState>,
    pane3_cache_state: Option<Pane3CacheState>,
    introspector: I,
    catalog_loader: Option<CatalogLoader>,
    runtime_ctx: RuntimeContext,
    active_pane: Pane,
    cmd: CommandLine,
    active_param_set_label: Option<String>,
    viewport: Viewport,
    dialect: ShellDialect,
    dirty: bool,
}

impl<I: Introspect> MillerSession<I> {
    pub fn new(
        pane1_entries: Vec<Entry>,
        introspector: I,
        initial_buffer: &str,
        viewport: Viewport,
        dialect: ShellDialect,
    ) -> Self {
        let mut pane1 = PaneList::new(pane1_entries);
        let (cmd, active_pane) = CommandLine::from_initial_buffer(initial_buffer, &mut pane1);

        let mut session = Self {
            panes: [pane1, PaneList::default(), PaneList::default()],
            pane2_cache_state: None,
            pane3_cache_state: None,
            introspector,
            catalog_loader: None,
            runtime_ctx: RuntimeContext::default(),
            active_pane,
            cmd,
            active_param_set_label: None,
            viewport,
            dialect,
            dirty: true,
        };
        session.sync_panes();
        session
    }

    /// Attaches dynamic runtime context (Git branch, CWD files, shell history)
    /// used to enrich Pane 2 and Pane 3 completions.
    pub fn with_runtime_context(mut self, runtime_ctx: RuntimeContext) -> Self {
        self.runtime_ctx = runtime_ctx;
        self.pane2_cache_state = None;
        self.pane3_cache_state = None;
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

    fn apply_catalog_update(&mut self, entries: Vec<Entry>) {
        let prev_p1_text = self.selected_entry(Pane::First).map(|e| e.text.clone());
        self.panes[0].set_entries(entries);
        let matched_prev = prev_p1_text
            .as_deref()
            .is_some_and(|txt| self.panes[0].select_first_matching(|e| e.text == txt));
        if !matched_prev {
            let matched_seed = self.cmd.tokens().first().is_some_and(|first_tok| {
                self.panes[0].select_first_matching(|e| e.text.eq_ignore_ascii_case(first_tok))
            });
            if !matched_seed {
                self.panes[0].reset_cursor();
            }
        }
        self.pane2_cache_state = None;
        self.pane3_cache_state = None;
    }

    fn invalidate_loading_panes(&mut self) {
        let pane2_entries = self.panes[1].entries();
        if pane2_entries.is_empty() || pane2_entries.iter().any(Entry::is_loading) {
            self.pane2_cache_state = None;
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
            UserAction::Cancel => return SessionOutcome::Canceled,
            UserAction::Confirm => return SessionOutcome::Confirmed(self.command_line()),
            UserAction::Resize(w, h) => {
                self.viewport = Viewport::new(w, h);
                self.clamp_scroll();
                self.dirty = true;
                return SessionOutcome::Continue;
            }
            UserAction::ReloadCatalog => {
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
                self.active_pane_mut().push_filter_char(ch);
                self.finish_filter_edit();
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

    fn step_right(&mut self) {
        match self.active_pane {
            Pane::First => {
                if let Some(p1) = self.selected_entry(Pane::First).cloned() {
                    if !p1.is_loading() {
                        self.cmd.commit_pane1_entry(&p1);
                        self.active_pane = Pane::Second;
                        self.cmd.resume_preview(Pane::Second);
                        self.dirty = true;
                    }
                }
            }
            Pane::Second => {
                let p1_opt = self.selected_entry(Pane::First).cloned();
                if let Some(p2) = self.selected_entry(Pane::Second).cloned() {
                    if !p2.is_loading() {
                        self.cmd.commit_pane2_entry(p1_opt.as_ref(), &p2);
                        self.active_pane = Pane::Third;
                        self.cmd.resume_preview(Pane::Second);
                        self.cmd.resume_preview(Pane::Third);
                        self.dirty = true;
                    }
                }
            }
            Pane::Third => {}
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
                self.active_pane = Pane::First;
                self.cmd.resume_preview(Pane::Second);
                self.dirty = true;
            }
            Pane::First => {}
        }
    }

    fn accept_token(&mut self) {
        match self.active_pane {
            Pane::First => {
                if let Some(p1) = self.selected_entry(Pane::First).cloned() {
                    if !p1.is_loading() {
                        self.cmd.commit_pane1_entry(&p1);
                        self.panes[0].clear_filter();
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
                let p1_opt = self.selected_entry(Pane::First).cloned();
                let Some(p2) = self.selected_entry(Pane::Second).cloned() else {
                    return;
                };
                if p2.is_loading() || !p2.is_emittable() {
                    return;
                }

                self.cmd.commit_pane2_entry(p1_opt.as_ref(), &p2);
                if p2.commits_without_value() {
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
                let p1_opt = self.selected_entry(Pane::First).cloned();
                let p2_opt = self.selected_entry(Pane::Second).cloned();
                let p3_opt = self.selected_entry(Pane::Third).cloned();
                let pane3_filter = self.panes[2].filter();
                let val_to_add = resolve_pane3_token(p3_opt.as_ref(), pane3_filter);
                let is_flag_placeholder = p3_opt
                    .as_ref()
                    .is_some_and(|p3| p3.is_placeholder() && p3.kind == EntryKind::Flag)
                    && pane3_filter.trim().is_empty();

                if val_to_add.is_none() && !is_flag_placeholder {
                    return;
                }

                if let Some(ref p2) = p2_opt {
                    self.cmd.commit_pane2_entry(p1_opt.as_ref(), p2);
                } else {
                    self.cmd.ensure_pane1_token(p1_opt.as_ref());
                }
                if let Some(val) = val_to_add {
                    self.cmd.push_token_dedup(val);
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
        } else if self.cmd.pop_token().is_some() {
            if self.active_pane == Pane::Third && self.cmd.tokens().len() <= 1 {
                self.active_pane = Pane::Second;
                self.cmd.resume_preview(Pane::Second);
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
        let cur_p1_idx = self.panes[0].selected_source_idx();
        let cur_p2_idx = self.sync_pane2(cur_p1_idx);
        self.sync_pane3(cur_p1_idx, cur_p2_idx);
        self.clamp_scroll();
    }

    fn sync_pane2(&mut self, cur_p1_idx: Option<usize>) -> Option<usize> {
        let cur_p1 = cur_p1_idx.map(|idx| &self.panes[0].entries()[idx]);
        let (entries, active_param_set) =
            resolve_pane2_entries(cur_p1, self.cmd.tokens(), &mut self.introspector);

        let next_state = Pane2CacheState {
            p1_entry_idx: cur_p1_idx,
            active_param_set: active_param_set.clone(),
        };

        if self.pane2_cache_state.as_ref() != Some(&next_state) {
            let same_p1 = self
                .pane2_cache_state
                .as_ref()
                .is_some_and(|prev| prev.p1_entry_idx == cur_p1_idx);
            let prev_p2_text = if same_p1 {
                self.selected_entry(Pane::Second).map(|it| it.text.clone())
            } else {
                None
            };

            self.panes[1].set_entries(entries);
            if same_p1 && !self.cmd.suppress_pane2_preview {
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

        if let Some(tok2) = self.cmd.pending_pane2_seed.take() {
            let pane2_entries = self.panes[1].entries();
            if !pane2_entries.is_empty() && !pane2_entries[0].is_loading() {
                if self.panes[1].select_first_matching(|it| {
                    it.text.eq_ignore_ascii_case(&tok2) || it.matches_parameter_token(&tok2)
                }) {
                    self.active_pane = Pane::Third;
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

    fn sync_pane3(&mut self, cur_p1_idx: Option<usize>, cur_p2_idx: Option<usize>) {
        let cur_p1 = cur_p1_idx.map(|idx| &self.panes[0].entries()[idx]);
        let cur_p2 = cur_p2_idx.map(|idx| &self.panes[1].entries()[idx]);

        let next_state = Pane3CacheState {
            p1_entry_idx: cur_p1_idx,
            p2_entry_idx: cur_p2_idx,
            pane3_filter: self.panes[2].filter().to_string(),
        };

        if self.pane3_cache_state.as_ref() != Some(&next_state) {
            let entries = resolve_pane3_entries(
                cur_p1,
                cur_p2,
                &mut self.introspector,
                self.panes[2].filter(),
                self.dialect,
                &self.runtime_ctx,
            );
            self.panes[2].set_entries(entries);
            self.panes[2].reset_cursor();
            self.pane3_cache_state = Some(next_state);
        }

        if self.active_pane == Pane::Third && !self.cmd.pending_pane3_seeds.is_empty() {
            let pane3_entries = self.panes[2].entries();
            if !pane3_entries.is_empty() && !pane3_entries[0].is_loading() {
                let tok3 = self.cmd.pending_pane3_seeds.pop_front().unwrap();
                let has_more = !self.cmd.pending_pane3_seeds.is_empty();
                self.cmd.pending_pane3_seeds.clear();
                let matched = self.panes[2].select_first_matching(|it| {
                    it.text.eq_ignore_ascii_case(&tok3) || it.matches_parameter_token(&tok3)
                });
                if !matched || has_more {
                    self.cmd.suppress_pane3_preview = true;
                }
            }
        }
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
}
