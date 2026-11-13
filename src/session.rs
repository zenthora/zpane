use crate::discovery::{CliIntrospector, ShellDialect};
use crate::fuzzy::{filter_items, selected_item, FilteredItem};
use crate::model::{Column, ItemKind, MillerItem};
use crate::ui::{render_frame, RenderState, Viewport};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::collections::HashSet;

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
struct Col2CacheState {
    c1_item_idx: Option<usize>,
    active_param_sets: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Col3CacheState {
    c1_item_idx: Option<usize>,
    c2_item_idx: Option<usize>,
    col3_filter: String,
}

type CatalogLoader = Box<dyn FnMut(ShellDialect) -> Vec<MillerItem>>;

/// Returns `true` when a Column 1 item represents a variable scope root or loading placeholder
/// whose own `.text` must never be emitted as a command token.
fn is_scope_root(item: &MillerItem) -> bool {
    matches!(item.kind, ItemKind::Env | ItemKind::Var | ItemKind::Loading) || item.text.is_empty()
}

/// Returns `true` when a Column 2 item can be selected or committed as a token.
fn is_selectable_col2_item(item: &MillerItem) -> bool {
    item.kind != ItemKind::Loading && !item.text.is_empty()
}

/// Returns `true` when a Column 2 item is a valueless switch or childless flag
/// that commits in place on `Tab` (`AcceptToken`) without advancing to Column 3.
fn is_childless_col2_flag_or_switch(item: &MillerItem) -> bool {
    item.kind == ItemKind::Switch || (item.kind == ItemKind::Flag && item.children.is_empty())
}

fn resolve_col2_items(
    col1: Option<&MillerItem>,
    active_param_sets: Option<&HashSet<String>>,
    introspector: &mut CliIntrospector,
) -> Vec<MillerItem> {
    let Some(c1) = col1 else {
        return Vec::new();
    };

    if !c1.children.is_empty() {
        if let Some(sets) = active_param_sets {
            let filtered: Vec<MillerItem> = c1
                .children
                .iter()
                .filter(|p| {
                    if p.parameter_sets.is_empty()
                        || p.parameter_sets.iter().any(|s| s == "__AllParameterSets")
                    {
                        true
                    } else {
                        p.parameter_sets.iter().any(|s| sets.contains(s))
                    }
                })
                .cloned()
                .collect();
            if !filtered.is_empty() {
                return filtered;
            }
        }
        return c1.children.clone();
    }

    introspector.query(&[c1.text.as_str()])
}

fn resolve_col3_items(
    col1: Option<&MillerItem>,
    col2: Option<&MillerItem>,
    introspector: &mut CliIntrospector,
    col3_filter: &str,
    dialect: ShellDialect,
) -> Vec<MillerItem> {
    let (Some(c1), Some(c2)) = (col1, col2) else {
        return Vec::new();
    };

    if c2.kind == ItemKind::Loading {
        return Vec::new();
    }

    let mut items = Vec::new();
    let trimmed_input = col3_filter.trim();

    if !trimmed_input.is_empty() {
        items.push(MillerItem::new(
            trimmed_input,
            format!("✏ Use \"{trimmed_input}\""),
            ItemKind::Value,
            format!("Insert custom value for {}", c2.label),
        ));
    } else if matches!(c2.kind, ItemKind::Param | ItemKind::Flag) && c2.children.is_empty() {
        items.push(MillerItem::new(
            "",
            "✏ (Type custom value / path)",
            ItemKind::Value,
            format!(
                "Type directly in this column to specify an argument for {}",
                c2.label
            ),
        ));
    }

    if !c2.children.is_empty() {
        items.extend_from_slice(&c2.children);
        if matches!(c2.kind, ItemKind::Param | ItemKind::Switch) {
            items.extend(dialect.common_var_items());
        }
        return items;
    }

    if c2.kind == ItemKind::Subcmd {
        items.extend(introspector.query(&[c1.text.as_str(), c2.text.as_str()]));
        return items;
    }

    items.extend(dialect.common_var_items());
    items
}

fn resolve_col3_token(cur_c3: Option<&MillerItem>, col3_filter: &str) -> Option<String> {
    let trimmed_filter = col3_filter.trim();
    if let Some(c3) = cur_c3 {
        if c3.kind == ItemKind::Loading {
            None
        } else if !c3.text.is_empty() {
            Some(c3.text.clone())
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

/// Deep interactive session state machine owning the 3-column Miller catalogs,
/// per-column fuzzy filters, cached filtered index views, selection/scroll cursors,
/// committed command tokens, initial buffer seeding, and active parameter-set state.
pub struct MillerSession {
    col1_items: Vec<MillerItem>,
    col2_items: Vec<MillerItem>,
    col3_items: Vec<MillerItem>,
    col1_filtered: Vec<FilteredItem>,
    col2_filtered: Vec<FilteredItem>,
    col3_filtered: Vec<FilteredItem>,
    col1_filter_dirty: bool,
    col2_filter_dirty: bool,
    col3_filter_dirty: bool,
    col2_cache_state: Option<Col2CacheState>,
    col3_cache_state: Option<Col3CacheState>,
    introspector: CliIntrospector,
    catalog_loader: Option<CatalogLoader>,
    active_col: Column,
    filters: [String; 3],
    sel_idx: [usize; 3],
    scroll_idx: [usize; 3],
    tokens: Vec<String>,
    pending_second_token: Option<String>,
    active_param_set_label: Option<String>,
    suppress_col2_preview: bool,
    viewport: Viewport,
    dialect: ShellDialect,
    dirty: bool,
}

impl MillerSession {
    pub fn new(
        col1_items: Vec<MillerItem>,
        introspector: CliIntrospector,
        initial_buffer: &str,
        viewport: Viewport,
        dialect: ShellDialect,
    ) -> Self {
        let mut active_col = Column::First;
        let mut sel_idx = [0, 0, 0];
        let tokens: Vec<String> = initial_buffer
            .split_whitespace()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        if let Some(first_tok) = tokens.first() {
            if let Some(pos) = col1_items
                .iter()
                .position(|it| it.text.eq_ignore_ascii_case(first_tok))
            {
                sel_idx[0] = pos;
                active_col = Column::Second;
            }
        }

        let pending_second_token = if tokens.len() >= 2 {
            Some(tokens[1].clone())
        } else {
            None
        };

        let mut session = Self {
            col1_items,
            col2_items: Vec::new(),
            col3_items: Vec::new(),
            col1_filtered: Vec::new(),
            col2_filtered: Vec::new(),
            col3_filtered: Vec::new(),
            col1_filter_dirty: true,
            col2_filter_dirty: true,
            col3_filter_dirty: true,
            col2_cache_state: None,
            col3_cache_state: None,
            introspector,
            catalog_loader: None,
            active_col,
            filters: [String::new(), String::new(), String::new()],
            sel_idx,
            scroll_idx: [0, 0, 0],
            tokens,
            pending_second_token,
            active_param_set_label: None,
            suppress_col2_preview: false,
            viewport,
            dialect,
            dirty: true,
        };
        session.sync_columns();
        session
    }

    /// Configures a catalog loader invoked on [`UserAction::ReloadCatalog`].
    pub fn with_catalog_loader<F>(mut self, loader: F) -> Self
    where
        F: FnMut(ShellDialect) -> Vec<MillerItem> + 'static,
    {
        self.catalog_loader = Some(Box::new(loader));
        self
    }

    /// Polls background CLI introspection jobs and refreshes dependent columns if new entries arrived.
    pub fn poll_introspector(&mut self) -> bool {
        if self.introspector.poll() {
            self.refresh_after_introspector_update();
            true
        } else {
            false
        }
    }

    /// Test-only blocking variant of [`Self::poll_introspector`].
    #[cfg(test)]
    pub fn poll_introspector_blocking(&mut self) -> bool {
        if self.introspector.poll_blocking() {
            self.refresh_after_introspector_update();
            true
        } else {
            false
        }
    }

    fn refresh_after_introspector_update(&mut self) {
        if self.col2_items.is_empty()
            || self.col2_items.first().map(|i| i.kind) == Some(ItemKind::Loading)
        {
            self.col2_cache_state = None;
        }
        // Only refresh Column 3 when it is still waiting on background data.
        // An unrelated cache completion must not reset the user's Column 3
        // cursor or shift the live preview.
        if self.col3_items.is_empty() || self.col3_items.iter().any(|i| i.kind == ItemKind::Loading)
        {
            self.col3_cache_state = None;
        }
        self.sync_columns();
        self.dirty = true;
    }

    /// Flushes the underlying [`CliIntrospector`] cache to disk if dirty.
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
        self.sync_columns();

        match action {
            UserAction::Cancel => return SessionOutcome::Canceled,
            UserAction::Confirm => return SessionOutcome::Confirmed(self.preview_command()),
            UserAction::Resize(w, h) => {
                self.viewport = Viewport::new(w, h);
                self.clamp_scroll();
                self.dirty = true;
                return SessionOutcome::Continue;
            }
            UserAction::ReloadCatalog => {
                if let Some(loader) = self.catalog_loader.as_mut() {
                    self.col1_items = loader(self.dialect);
                }
                self.col1_filter_dirty = true;
                self.col2_cache_state = None;
                self.col3_cache_state = None;
                self.sync_columns();
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
                self.filters[self.active_col.index()].push(ch);
                self.finish_filter_edit();
            }
            UserAction::ClearFilter => {
                if !self.filters[self.active_col.index()].is_empty() {
                    self.filters[self.active_col.index()].clear();
                    self.finish_filter_edit();
                }
            }
        }

        if self.dirty {
            self.sync_columns();
        }
        SessionOutcome::Continue
    }

    fn apply_navigation(&mut self, action: &UserAction) {
        if self.active_col == Column::Second {
            self.resume_col2_preview();
        }
        let col_idx = self.active_col.index();
        let cur_len = self.active_filtered_len();
        match action {
            UserAction::MoveHome => {
                self.sel_idx[col_idx] = 0;
                self.dirty = true;
            }
            UserAction::MoveUp if cur_len > 0 => {
                self.sel_idx[col_idx] = if self.sel_idx[col_idx] > 0 {
                    self.sel_idx[col_idx] - 1
                } else {
                    cur_len - 1
                };
                self.dirty = true;
            }
            UserAction::MoveDown if cur_len > 0 => {
                self.sel_idx[col_idx] = if self.sel_idx[col_idx] + 1 < cur_len {
                    self.sel_idx[col_idx] + 1
                } else {
                    0
                };
                self.dirty = true;
            }
            UserAction::PageUp if cur_len > 0 => {
                let page_rows = self.viewport.rows(self.active_col);
                if page_rows > 0 {
                    self.sel_idx[col_idx] = self.sel_idx[col_idx].saturating_sub(page_rows);
                    self.dirty = true;
                }
            }
            UserAction::PageDown if cur_len > 0 => {
                let page_rows = self.viewport.rows(self.active_col);
                if page_rows > 0 {
                    self.sel_idx[col_idx] = (self.sel_idx[col_idx] + page_rows).min(cur_len - 1);
                    self.dirty = true;
                }
            }
            UserAction::MoveEnd if cur_len > 0 => {
                self.sel_idx[col_idx] = cur_len - 1;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn step_right(&mut self) {
        match self.active_col {
            Column::First => {
                if let Some(c1) = self.selected_item(Column::First).cloned() {
                    if c1.kind != ItemKind::Loading {
                        self.commit_col1_item(&c1);
                        self.active_col = Column::Second;
                        self.resume_col2_preview();
                        self.dirty = true;
                    }
                }
            }
            Column::Second => {
                let c1_opt = self.selected_item(Column::First).cloned();
                if let Some(c2) = self.selected_item(Column::Second).cloned() {
                    if c2.kind != ItemKind::Loading {
                        self.commit_col2_item(c1_opt.as_ref(), &c2);
                        self.active_col = Column::Third;
                        self.resume_col2_preview();
                        self.dirty = true;
                    }
                }
            }
            Column::Third => {}
        }
    }

    fn step_left(&mut self) {
        match self.active_col {
            Column::Third => {
                self.active_col = Column::Second;
                self.resume_col2_preview();
                self.dirty = true;
            }
            Column::Second => {
                self.active_col = Column::First;
                self.resume_col2_preview();
                self.dirty = true;
            }
            Column::First => {}
        }
    }

    fn accept_token(&mut self) {
        match self.active_col {
            Column::First => {
                if let Some(c1) = self.selected_item(Column::First).cloned() {
                    if c1.kind != ItemKind::Loading {
                        self.commit_col1_item(&c1);
                        self.clear_filter(Column::First);
                        self.active_col = Column::Second;
                        self.resume_col2_preview();
                        self.dirty = true;
                    }
                }
            }
            Column::Second => {
                // A suppressed Column 2 preview means the reset index-0 item was
                // never shown; Tab is a no-op until navigation or typing clears it.
                if self.suppress_col2_preview {
                    return;
                }
                let c1_opt = self.selected_item(Column::First).cloned();
                let Some(c2) = self.selected_item(Column::Second).cloned() else {
                    return;
                };
                if !is_selectable_col2_item(&c2) {
                    return;
                }

                self.commit_col2_item(c1_opt.as_ref(), &c2);
                if is_childless_col2_flag_or_switch(&c2) {
                    self.clear_filter(Column::Second);
                    self.sel_idx[1] = 0;
                    self.scroll_idx[1] = 0;
                    self.suppress_col2_preview = true;
                } else {
                    self.active_col = Column::Third;
                    self.sel_idx[2] = 0;
                    self.scroll_idx[2] = 0;
                    self.resume_col2_preview();
                }
                self.dirty = true;
            }
            Column::Third => {
                let c1_opt = self.selected_item(Column::First).cloned();
                let c2_opt = self.selected_item(Column::Second).cloned();
                let c3_opt = self.selected_item(Column::Third).cloned();
                let val_to_add = resolve_col3_token(c3_opt.as_ref(), &self.filters[2]);
                let is_switch_placeholder = c3_opt
                    .as_ref()
                    .is_some_and(|c3| c3.kind == ItemKind::Switch && c3.text.is_empty())
                    && self.filters[2].trim().is_empty();

                if val_to_add.is_none() && !is_switch_placeholder {
                    return;
                }

                if let Some(ref c2) = c2_opt {
                    self.commit_col2_item(c1_opt.as_ref(), c2);
                } else {
                    self.ensure_col1_token(c1_opt.as_ref());
                }
                if let Some(val) = val_to_add {
                    self.push_token_dedup(val);
                }
                self.clear_filter(Column::Third);
                self.clear_filter(Column::Second);
                self.active_col = Column::Second;
                self.sel_idx[1] = 0;
                self.scroll_idx[1] = 0;
                self.suppress_col2_preview = true;
                self.dirty = true;
            }
        }
    }

    fn handle_backspace(&mut self) {
        if !self.filters[self.active_col.index()].is_empty() {
            self.filters[self.active_col.index()].pop();
            self.finish_filter_edit();
        } else if !self.tokens.is_empty() {
            self.tokens.pop();
            if self.active_col == Column::Third && self.tokens.len() <= 1 {
                self.active_col = Column::Second;
                self.resume_col2_preview();
            } else if self.active_col == Column::Second {
                if self.tokens.is_empty() {
                    self.active_col = Column::First;
                    self.resume_col2_preview();
                } else {
                    self.suppress_col2_preview = true;
                }
            }
            self.dirty = true;
        } else if self.active_col != Column::First {
            self.active_col = match self.active_col {
                Column::Third => Column::Second,
                Column::Second | Column::First => Column::First,
            };
            self.resume_col2_preview();
            self.dirty = true;
        }
    }

    /// Builds the live command preview string from the current session state.
    pub fn preview_command(&self) -> String {
        let cur_c1 = self.selected_item(Column::First);
        let cur_c2 = self.selected_item(Column::Second);
        let cur_c3 = self.selected_item(Column::Third);

        let mut out: Vec<String> = self.tokens.clone();

        if out.is_empty() {
            if let Some(c1) = cur_c1 {
                if !is_scope_root(c1) {
                    out.push(c1.text.clone());
                }
            }
        } else if self.active_col == Column::First && out.len() == 1 {
            if let Some(c1) = cur_c1 {
                if !is_scope_root(c1) {
                    out[0] = c1.text.clone();
                }
            }
        }

        if self.active_col == Column::Second {
            if !self.suppress_col2_preview {
                if let Some(c2) = cur_c2 {
                    if is_selectable_col2_item(c2)
                        && out.last().map(|s| s.as_str()) != Some(c2.text.as_str())
                    {
                        out.push(c2.text.clone());
                    }
                }
            }
        } else if self.active_col == Column::Third {
            if let (Some(c1), Some(c2)) = (cur_c1, cur_c2) {
                if !is_scope_root(c1)
                    && is_selectable_col2_item(c2)
                    && out.len() <= 1
                    && out.last().map(|s| s.as_str()) != Some(c2.text.as_str())
                {
                    out.push(c2.text.clone());
                }
            }
            if let Some(v) = resolve_col3_token(cur_c3, &self.filters[2]) {
                if out.last().map(|s| s.as_str()) != Some(v.as_str()) {
                    out.push(v);
                }
            }
        }

        out.join(" ")
    }

    /// Renders a complete terminal frame for the current session state and clears the dirty flag.
    pub fn render(&mut self) -> String {
        self.sync_columns();

        let cmd_preview = self.preview_command();

        let frame = render_frame(&RenderState {
            viewport: self.viewport,
            active_col: self.active_col,
            filters: &self.filters,
            sel_idx: &self.sel_idx,
            scroll_idx: &self.scroll_idx,
            col1_items: &self.col1_items,
            col1_filtered: &self.col1_filtered,
            col2_items: &self.col2_items,
            col2_filtered: &self.col2_filtered,
            col3_items: &self.col3_items,
            col3_filtered: &self.col3_filtered,
            cmd_preview: &cmd_preview,
            active_param_set: self.active_param_set_label.as_deref(),
        });

        self.dirty = false;
        frame
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[cfg(test)]
    pub fn active_col(&self) -> usize {
        self.active_col.index()
    }

    #[cfg(test)]
    pub fn sel_idx(&self) -> &[usize; 3] {
        &self.sel_idx
    }

    #[cfg(test)]
    pub fn scroll_idx(&self) -> &[usize; 3] {
        &self.scroll_idx
    }

    #[cfg(test)]
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    #[cfg(test)]
    pub fn active_param_set(&self) -> Option<&str> {
        self.active_param_set_label.as_deref()
    }

    /// Returns the currently visible (fuzzy-filtered) items in `col`.
    #[cfg(test)]
    pub fn visible_col_items(&self, col: Column) -> Vec<&MillerItem> {
        let (filtered, items) = match col {
            Column::First => (&self.col1_filtered, &self.col1_items),
            Column::Second => (&self.col2_filtered, &self.col2_items),
            Column::Third => (&self.col3_filtered, &self.col3_items),
        };
        filtered
            .iter()
            .filter_map(|f| items.get(f.item_idx))
            .collect()
    }

    /// Returns the currently highlighted item in `col`.
    pub fn selected_item(&self, col: Column) -> Option<&MillerItem> {
        match col {
            Column::First => selected_item(&self.col1_filtered, &self.col1_items, self.sel_idx[0]),
            Column::Second => selected_item(&self.col2_filtered, &self.col2_items, self.sel_idx[1]),
            Column::Third => selected_item(&self.col3_filtered, &self.col3_items, self.sel_idx[2]),
        }
    }

    fn active_filtered_len(&self) -> usize {
        match self.active_col {
            Column::First => self.col1_filtered.len(),
            Column::Second => self.col2_filtered.len(),
            Column::Third => self.col3_filtered.len(),
        }
    }

    fn finish_filter_edit(&mut self) {
        self.mark_active_filter_dirty();
        self.sel_idx[self.active_col.index()] = 0;
        if self.active_col == Column::Second {
            self.resume_col2_preview();
        }
        self.dirty = true;
    }

    fn clear_filter(&mut self, col: Column) {
        let idx = col.index();
        if !self.filters[idx].is_empty() {
            self.filters[idx].clear();
            match col {
                Column::First => self.col1_filter_dirty = true,
                Column::Second => self.col2_filter_dirty = true,
                Column::Third => self.col3_filter_dirty = true,
            }
        }
    }

    fn mark_active_filter_dirty(&mut self) {
        match self.active_col {
            Column::First => self.col1_filter_dirty = true,
            Column::Second => self.col2_filter_dirty = true,
            Column::Third => self.col3_filter_dirty = true,
        }
    }

    /// Re-arms Column 2 live preview after navigation or typing.
    fn resume_col2_preview(&mut self) {
        self.suppress_col2_preview = false;
    }

    fn push_token_dedup(&mut self, token: String) {
        if self.tokens.last().map(String::as_str) != Some(token.as_str()) {
            self.tokens.push(token);
        }
    }

    fn ensure_col1_token(&mut self, c1: Option<&MillerItem>) {
        if let Some(c1) = c1 {
            if !is_scope_root(c1) && self.tokens.is_empty() {
                self.tokens.push(c1.text.clone());
            }
        }
    }

    fn commit_col1_item(&mut self, c1: &MillerItem) {
        if is_scope_root(c1) {
            return;
        }
        if self.tokens.is_empty() {
            self.tokens.push(c1.text.clone());
        } else if !self.tokens[0].eq_ignore_ascii_case(&c1.text) {
            self.tokens[0] = c1.text.clone();
            self.tokens.truncate(1);
        }
    }

    fn commit_col2_item(&mut self, c1: Option<&MillerItem>, c2: &MillerItem) {
        self.ensure_col1_token(c1);
        if !c1.is_some_and(is_scope_root) && is_selectable_col2_item(c2) {
            self.push_token_dedup(c2.text.clone());
        }
    }

    fn sync_columns(&mut self) {
        let cur_c1_idx = self.sync_col1();
        let cur_c2_idx = self.sync_col2(cur_c1_idx);
        self.sync_col3(cur_c1_idx, cur_c2_idx);
        self.clamp_scroll();
    }

    fn sync_col1(&mut self) -> Option<usize> {
        if self.col1_filter_dirty {
            self.col1_filtered = filter_items(&self.col1_items, &self.filters[0]);
            self.col1_filter_dirty = false;
        }
        if self.sel_idx[0] >= self.col1_filtered.len() {
            self.sel_idx[0] = self.col1_filtered.len().saturating_sub(1);
        }
        self.col1_filtered.get(self.sel_idx[0]).map(|f| f.item_idx)
    }

    fn sync_col2(&mut self, cur_c1_idx: Option<usize>) -> Option<usize> {
        let cur_c1 = cur_c1_idx.map(|idx| &self.col1_items[idx]);
        let active_param_sets = cur_c1.and_then(|c1| c1.active_parameter_sets(&self.tokens));
        let sorted_param_sets: Option<Vec<String>> = active_param_sets.as_ref().map(|s| {
            let mut v: Vec<String> = s.iter().cloned().collect();
            v.sort();
            v
        });

        let next_col2_state = Col2CacheState {
            c1_item_idx: cur_c1_idx,
            active_param_sets: sorted_param_sets.clone(),
        };

        if self.col2_cache_state.as_ref() != Some(&next_col2_state) {
            let same_c1 = self
                .col2_cache_state
                .as_ref()
                .is_some_and(|prev| prev.c1_item_idx == cur_c1_idx);
            let prev_c2_text = if same_c1 {
                self.selected_item(Column::Second).map(|it| it.text.clone())
            } else {
                None
            };

            self.col2_items =
                resolve_col2_items(cur_c1, active_param_sets.as_ref(), &mut self.introspector);
            self.col2_filtered = filter_items(&self.col2_items, &self.filters[1]);
            self.col2_filter_dirty = false;

            if same_c1 && !self.suppress_col2_preview {
                if let Some(pos) = prev_c2_text.as_deref().and_then(|prev_txt| {
                    self.col2_filtered
                        .iter()
                        .position(|f| self.col2_items[f.item_idx].text == prev_txt)
                }) {
                    self.sel_idx[1] = pos;
                } else {
                    self.sel_idx[1] = 0;
                    self.scroll_idx[1] = 0;
                }
            } else {
                self.sel_idx[1] = 0;
                self.scroll_idx[1] = 0;
            }

            self.active_param_set_label = sorted_param_sets.map(|v| v.join(", "));
            self.col2_cache_state = Some(next_col2_state);
            self.col3_cache_state = None;
        }

        if self.col2_filter_dirty {
            self.col2_filtered = filter_items(&self.col2_items, &self.filters[1]);
            self.col2_filter_dirty = false;
        }

        if let Some(ref tok2) = self.pending_second_token {
            if !self.col2_items.is_empty() && self.col2_items[0].kind != ItemKind::Loading {
                if let Some(pos) = self.col2_filtered.iter().position(|f| {
                    let it = &self.col2_items[f.item_idx];
                    it.text.eq_ignore_ascii_case(tok2) || it.matches_parameter_token(tok2)
                }) {
                    self.sel_idx[1] = pos;
                    self.active_col = Column::Third;
                }
                self.pending_second_token = None;
            }
        }

        if self.sel_idx[1] >= self.col2_filtered.len() {
            self.sel_idx[1] = self.col2_filtered.len().saturating_sub(1);
        }
        self.col2_filtered.get(self.sel_idx[1]).map(|f| f.item_idx)
    }

    fn sync_col3(&mut self, cur_c1_idx: Option<usize>, cur_c2_idx: Option<usize>) {
        let cur_c1 = cur_c1_idx.map(|idx| &self.col1_items[idx]);
        let cur_c2 = cur_c2_idx.map(|idx| &self.col2_items[idx]);

        let next_col3_state = Col3CacheState {
            c1_item_idx: cur_c1_idx,
            c2_item_idx: cur_c2_idx,
            col3_filter: self.filters[2].clone(),
        };

        if self.col3_cache_state.as_ref() != Some(&next_col3_state) {
            self.col3_items = resolve_col3_items(
                cur_c1,
                cur_c2,
                &mut self.introspector,
                &self.filters[2],
                self.dialect,
            );
            self.col3_filter_dirty = true;
            self.sel_idx[2] = 0;
            self.scroll_idx[2] = 0;
            self.col3_cache_state = Some(next_col3_state);
        }

        if self.col3_filter_dirty {
            self.col3_filtered = filter_items(&self.col3_items, &self.filters[2]);
            self.col3_filter_dirty = false;
        }
        if self.sel_idx[2] >= self.col3_filtered.len() {
            self.sel_idx[2] = self.col3_filtered.len().saturating_sub(1);
        }
    }

    fn clamp_scroll(&mut self) {
        for col in Column::ALL {
            let c = col.index();
            let max_r = self.viewport.rows(col);
            if max_r == 0 {
                self.scroll_idx[c] = 0;
                continue;
            }
            if self.sel_idx[c] < self.scroll_idx[c] {
                self.scroll_idx[c] = self.sel_idx[c];
            }
            if self.sel_idx[c] >= self.scroll_idx[c] + max_r {
                self.scroll_idx[c] = self.sel_idx[c] + 1 - max_r;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{build_env_scope_item_from_vars, build_ps_var_scope_item};
    use std::collections::HashMap;

    fn sample_ps_cmdlet() -> MillerItem {
        let mut get_process = MillerItem::new(
            "Get-Process",
            "Get-Process",
            ItemKind::Cmdlet,
            "Gets the processes that are running on the local computer",
        );

        let info_action = MillerItem::new(
            "-InformationAction",
            "-InformationAction (-ia)",
            ItemKind::Param,
            "[ActionPreference] Parameter -InformationAction (-ia)",
        )
        .with_aliases(vec!["ia".to_string()])
        .with_parameter_sets(vec!["InfoSet".to_string()]);

        let identity = MillerItem::new(
            "-Identity",
            "-Identity",
            ItemKind::Param,
            "[String] Parameter -Identity",
        )
        .with_parameter_sets(vec!["IdentitySet".to_string()]);

        let id_param = MillerItem::new(
            "-Id",
            "-Id (-PID, -ProcessId)",
            ItemKind::Param,
            "[Int32[]] Parameter -Id (-PID, -ProcessId)",
        )
        .with_aliases(vec!["PID".to_string(), "ProcessId".to_string()])
        .with_parameter_sets(vec!["Id".to_string(), "IdWithUserName".to_string()]);

        let name_param = MillerItem::new(
            "-Name",
            "-Name (-ProcessName)",
            ItemKind::Param,
            "[String[]] Parameter -Name (-ProcessName)",
        )
        .with_aliases(vec!["ProcessName".to_string()])
        .with_parameter_sets(vec!["Name".to_string(), "NameWithUserName".to_string()]);

        let mut verbose_switch = MillerItem::new(
            "-Verbose",
            "-Verbose (-vb)",
            ItemKind::Switch,
            "[SwitchParameter] Parameter -Verbose (-vb)",
        )
        .with_aliases(vec!["vb".to_string()])
        .with_parameter_sets(vec!["__AllParameterSets".to_string()]);
        verbose_switch.children = vec![
            MillerItem::new(
                "",
                "(Switch Flag - No Value Needed)",
                ItemKind::Switch,
                "Press Enter or Tab to include -Verbose",
            ),
            MillerItem::new(":$true", ":$true", ItemKind::Value, "Enable switch"),
            MillerItem::new(":$false", ":$false", ItemKind::Value, "Disable switch"),
        ];

        get_process.children = vec![info_action, identity, id_param, name_param, verbose_switch];
        get_process
    }

    #[test]
    fn test_exact_parameter_alias_matching_and_exclusivity() {
        let get_process = sample_ps_cmdlet();
        let introspector =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let mut session = MillerSession::new(
            vec![get_process],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Step into Col 2: all 5 parameters should be visible initially
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.active_param_set(), None);
        assert_eq!(session.visible_col_items(Column::Second).len(), 5);

        // Move down to `-Id` (index 2: after -InformationAction and -Identity)
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::MoveDown);
        assert_eq!(session.selected_item(Column::Second).unwrap().text, "-Id");

        // Step right into Col 3 to supply an ID: commits `-Id` and activates {"Id", "IdWithUserName"}
        // Must NOT match `-InformationAction (-ia)` or `-Identity`!
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 2);
        assert_eq!(session.active_param_set(), Some("Id, IdWithUserName"));

        let visible_col2: Vec<&str> = session
            .visible_col_items(Column::Second)
            .iter()
            .map(|it| it.text.as_str())
            .collect();
        assert_eq!(visible_col2, vec!["-Id", "-Verbose"]);
        assert_eq!(session.selected_item(Column::Second).unwrap().text, "-Id");

        // Type custom process ID "4242" in Col 3 and accept via Tab
        for ch in "4242".chars() {
            session.apply(UserAction::InsertChar(ch));
        }
        assert_eq!(session.preview_command(), "Get-Process -Id 4242");
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.tokens(), &["Get-Process", "-Id", "4242"]);
        assert_eq!(session.active_param_set(), Some("Id, IdWithUserName"));
        assert_eq!(session.preview_command(), "Get-Process -Id 4242");

        // Pop "4242" then pop "-Id" via Backspace -> parameter set exclusivity immediately clears
        session.apply(UserAction::Backspace);
        assert_eq!(session.tokens(), &["Get-Process", "-Id"]);
        assert_eq!(session.active_param_set(), Some("Id, IdWithUserName"));

        session.apply(UserAction::Backspace);
        assert_eq!(session.tokens(), &["Get-Process"]);
        assert_eq!(session.active_param_set(), None);
        assert_eq!(session.visible_col_items(Column::Second).len(), 5);

        // Also verify structured alias matching via initial buffer (e.g., `-pid` and `-ia`)
        let introspector2 =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let session_alias = MillerSession::new(
            vec![sample_ps_cmdlet()],
            introspector2,
            "Get-Process -pid",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(session_alias.active_col(), 2);
        assert_eq!(session_alias.active_param_set(), Some("Id, IdWithUserName"));
        assert_eq!(
            session_alias.selected_item(Column::Second).unwrap().text,
            "-Id"
        );

        let introspector3 =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let session_ia = MillerSession::new(
            vec![sample_ps_cmdlet()],
            introspector3,
            "Get-Process -ia",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );
        assert_eq!(session_ia.active_param_set(), Some("InfoSet"));
    }

    #[test]
    fn test_accept_token_on_col2_switch_or_flag_does_not_pollute_preview() {
        let get_process = sample_ps_cmdlet();
        let introspector =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let mut session = MillerSession::new(
            vec![get_process],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Step into Col 2 and move to `-Verbose` (index 4, a Switch parameter)
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        assert_eq!(
            session.selected_item(Column::Second).unwrap().text,
            "-Verbose"
        );
        assert_eq!(session.preview_command(), "Get-Process -Verbose");

        // Press Tab (`AcceptToken`) on `-Verbose`:
        // Selection resets to index 0 (`-InformationAction`), but live preview MUST NOT append `-InformationAction`!
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.sel_idx()[1], 0);
        assert_eq!(session.tokens(), &["Get-Process", "-Verbose"]);
        assert_eq!(session.preview_command(), "Get-Process -Verbose");

        // Confirming right now emits only `Get-Process -Verbose`
        assert_eq!(
            session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("Get-Process -Verbose".to_string())
        );

        // Moving down in Col 2 re-enables live preview for the newly highlighted item
        session.apply(UserAction::MoveDown);
        assert_eq!(
            session.selected_item(Column::Second).unwrap().text,
            "-Identity"
        );
        assert_eq!(session.preview_command(), "Get-Process -Verbose -Identity");
    }

    #[test]
    fn test_second_tab_after_switch_accept_does_not_commit_unpreviewed_item() {
        let get_process = sample_ps_cmdlet();
        let introspector =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let mut session = MillerSession::new(
            vec![get_process],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Accept `-Verbose`; the selection resets to index 0 (`-InformationAction`),
        // which is deliberately excluded from the preview.
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.sel_idx()[1], 0);
        assert_eq!(session.tokens(), &["Get-Process", "-Verbose"]);
        assert_eq!(session.preview_command(), "Get-Process -Verbose");

        // A second Tab without navigation or typing must not commit the
        // un-previewed index-0 item.
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.tokens(), &["Get-Process", "-Verbose"]);
        assert_eq!(session.preview_command(), "Get-Process -Verbose");

        // Navigation re-arms normal acceptance of the highlighted item.
        session.apply(UserAction::MoveDown);
        assert_eq!(
            session.selected_item(Column::Second).unwrap().text,
            "-Identity"
        );
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.active_col(), 2);
        assert_eq!(session.tokens(), &["Get-Process", "-Verbose", "-Identity"]);
    }

    #[test]
    fn test_unrelated_background_completion_preserves_col3_cursor() {
        let git_item = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
        let introspector = CliIntrospector::with_runner(ShellDialect::Posix, None, |exe, args| {
            match (exe, args) {
                ("git", ["-h"]) => Some(
                    "Commands:\n  clone      Clone a repository\n  commit     Record changes\n"
                        .to_string(),
                ),
                ("git", ["commit", "-h"]) => Some(
                    "Options:\n  -a, --all       Commit all changed files\n  -m, --message <MSG>  Commit message\n"
                        .to_string(),
                ),
                ("other", ["--help"]) => Some("Options:\n  --unrelated   Unrelated flag\n".to_string()),
                _ => None,
            }
        });
        let mut session = MillerSession::new(
            vec![git_item],
            introspector,
            "git commit",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        // Drain `git --help` and `git commit --help` so Column 3 is fully loaded.
        assert!(session.poll_introspector_blocking());
        assert!(session.poll_introspector_blocking());
        assert!(session.active_col() == 2);
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::MoveDown);
        let selected_col3 = session.selected_item(Column::Third).unwrap().text.clone();
        assert_eq!(session.sel_idx()[2], 2);

        // Queue an unrelated background job, then let it complete.
        let _ = session.introspector.query(&["other"]);
        assert!(session.poll_introspector_blocking());

        // The unrelated completion must not reset Column 3 or shift the preview.
        assert_eq!(session.sel_idx()[2], 2);
        assert_eq!(
            session.selected_item(Column::Third).unwrap().text,
            selected_col3
        );
        assert!(session.preview_command().ends_with(&selected_col3));
    }

    #[test]
    fn test_switch_placeholder_in_col3_never_emits_blank_token() {
        let get_process = sample_ps_cmdlet();
        let introspector =
            CliIntrospector::with_initial_cache(ShellDialect::PowerShell, HashMap::new());
        let mut session = MillerSession::new(
            vec![get_process],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::PowerShell,
        );

        // Select `-Verbose` in Col 2 and press Right (`StepRight`) to enter Col 3
        session.apply(UserAction::StepRight);
        session.apply(UserAction::MoveEnd);
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 2);

        // Index 0 in Col 3 is `(Switch Flag - No Value Needed)` with empty `.text`
        let c3 = session.selected_item(Column::Third).unwrap();
        assert_eq!(c3.kind, ItemKind::Switch);
        assert_eq!(c3.text, "");
        assert_eq!(session.preview_command(), "Get-Process -Verbose");

        // Pressing Tab on the switch placeholder returns to Col 2 without adding a blank token
        session.apply(UserAction::AcceptToken);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.tokens(), &["Get-Process", "-Verbose"]);
        assert_eq!(session.preview_command(), "Get-Process -Verbose");
    }

    #[test]
    fn test_initial_buffer_seeding_and_async_introspection_poll() {
        let git_item = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
        let introspector = CliIntrospector::with_runner(ShellDialect::Posix, None, |exe, args| {
            match (exe, args) {
                ("git", ["-h"]) => Some(
                    "Commands:\n  clone      Clone a repository\n  commit     Record changes\n"
                        .to_string(),
                ),
                ("git", ["commit", "-h"]) => Some(
                    "Options:\n  -a, --all       Commit all changed files\n  -m, --message <MSG>  Commit message\n"
                        .to_string(),
                ),
                _ => None,
            }
        });

        // Launch with initial buffer "git commit" before cache is populated
        let mut session = MillerSession::new(
            vec![git_item],
            introspector,
            "git commit",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        // Initially Col 2 has the Loading placeholder because `git` hasn't finished introspecting yet
        assert_eq!(session.active_col(), 1);
        assert_eq!(
            session.selected_item(Column::Second).unwrap().kind,
            ItemKind::Loading
        );

        // Polling drains `git --help`, which resolves `pending_second_token` ("commit"),
        // selects `commit` in Col 2, advances `active_col` to 2, and enqueues `git commit --help`!
        assert!(session.poll_introspector_blocking());
        assert_eq!(session.active_col(), 2);
        assert_eq!(
            session.selected_item(Column::Second).unwrap().text,
            "commit"
        );

        // Polling again drains `git commit --help` into Col 3 while keeping `commit` selected in Col 2
        assert!(session.poll_introspector_blocking());
        assert_eq!(
            session.selected_item(Column::Second).unwrap().text,
            "commit"
        );
        let col3_texts: Vec<&str> = session
            .visible_col_items(Column::Third)
            .iter()
            .map(|it| it.text.as_str())
            .collect();
        assert!(col3_texts.contains(&"--all"));
        assert!(col3_texts.contains(&"--message"));
        assert!(col3_texts.contains(&"$?"));
    }

    #[test]
    fn test_variable_scopes_across_all_three_dialects() {
        for (dialect, expected_user_tok, expected_special_var) in [
            (ShellDialect::PowerShell, "$env:USER", "$_"),
            (ShellDialect::Posix, "$USER", "$?"),
            (ShellDialect::Fish, "$USER", "$status"),
        ] {
            let env_root = build_env_scope_item_from_vars(
                dialect,
                vec![
                    ("USER".to_string(), "alice".to_string()),
                    (
                        "PATH".to_string(),
                        format!("/usr/bin{}/bin", dialect.path_sep()),
                    ),
                ],
            );
            let var_root = build_ps_var_scope_item(dialect);
            let introspector = CliIntrospector::with_initial_cache(dialect, HashMap::new());

            let mut session = MillerSession::new(
                vec![env_root, var_root],
                introspector,
                "",
                Viewport::new(120, 30),
                dialect,
            );

            // Root scope item in Col 1 does not emit "$env:" or "$" into preview
            assert_eq!(session.preview_command(), "");

            // Step into Col 2 (Env variables: PATH, USER)
            session.apply(UserAction::StepRight);
            assert!(session.tokens().is_empty());
            session.apply(UserAction::MoveDown); // USER
            assert_eq!(
                session.selected_item(Column::Second).unwrap().text,
                expected_user_tok
            );
            assert_eq!(session.preview_command(), expected_user_tok);

            // Step into Col 3 on USER: index 0 is variable reference, index 1 is literal value
            session.apply(UserAction::StepRight);
            assert!(session.tokens().is_empty());
            assert_eq!(session.preview_command(), expected_user_tok);

            session.apply(UserAction::MoveDown);
            assert_eq!(session.preview_command(), "\"alice\"");

            // Step back to Col 1 and inspect the shell variable scope (`$global:Variables` / `$shell` / `$fish`)
            session.apply(UserAction::Backspace);
            session.apply(UserAction::Backspace);
            assert_eq!(session.active_col(), 0);

            session.apply(UserAction::MoveDown);
            session.apply(UserAction::StepRight);
            let var_texts: Vec<&str> = session
                .visible_col_items(Column::Second)
                .iter()
                .map(|it| it.text.as_str())
                .collect();
            assert!(
                var_texts.contains(&expected_special_var),
                "Dialect {dialect:?} should expose {expected_special_var}"
            );
        }
    }

    #[test]
    fn test_small_terminal_paging_and_reload_catalog() {
        let initial_items: Vec<MillerItem> = (0..15)
            .map(|i| {
                let mut cmd = MillerItem::new(
                    format!("cmd{i:02}"),
                    format!("cmd{i:02}"),
                    ItemKind::Cli,
                    "",
                );
                cmd.children = (0..10)
                    .map(|f| {
                        let mut sub = MillerItem::new(
                            format!("sub{f}"),
                            format!("sub{f}"),
                            ItemKind::Subcmd,
                            "",
                        );
                        sub.children = (0..10)
                            .map(|v| {
                                MillerItem::new(
                                    format!("--opt{v}"),
                                    format!("--opt{v}"),
                                    ItemKind::Flag,
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

        let introspector = CliIntrospector::with_initial_cache(ShellDialect::Posix, HashMap::new());
        // Small terminal: height = 12 (< 16)
        // Col 0 & Col 1 rows = 6; Col 2 rows = 3 (accounting for 3-row inspector)
        let mut session = MillerSession::new(
            initial_items,
            introspector,
            "",
            Viewport::new(90, 12),
            ShellDialect::Posix,
        )
        .with_catalog_loader(|_| {
            vec![MillerItem::new(
                "reloaded-tool",
                "reloaded-tool",
                ItemKind::Cli,
                "Freshly synced tool",
            )]
        });

        // PageDown in Col 0 jumps by 6 rows
        session.apply(UserAction::PageDown);
        assert_eq!(session.sel_idx()[0], 6);
        assert_eq!(session.scroll_idx()[0], 1); // visible rows 1..=6

        // Step right into Col 1, then page it by 6 rows (Viewport::rows(12, Col1) == 6)
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 1);
        session.apply(UserAction::PageDown);
        assert_eq!(session.sel_idx()[1], 6);
        assert_eq!(session.scroll_idx()[1], 1); // visible rows 1..=6

        // Step right into Col 2 (Column 3)
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 2);

        // PageDown in Col 2 jumps by 3 rows (Viewport::rows(12, Col3) == 3)
        session.apply(UserAction::PageDown);
        assert_eq!(session.sel_idx()[2], 3);
        assert_eq!(session.scroll_idx()[2], 1); // visible rows 1..=3

        let frame = session.render();
        assert_eq!(frame.lines().count(), 12);
        assert!(frame.contains("cmd06"));
        assert!(frame.contains("--opt3"));

        // Left navigation alone must not mutate committed tokens.
        session.apply(UserAction::StepLeft);
        assert_eq!(session.active_col(), 1);
        assert_eq!(session.tokens(), &["cmd06", "sub6"]);
        session.apply(UserAction::StepLeft);
        assert_eq!(session.active_col(), 0);
        assert_eq!(session.tokens(), &["cmd06", "sub6"]);

        // Backspace pops committed tokens (Right/Tab/Backspace/Enter unified).
        session.apply(UserAction::Backspace);
        assert_eq!(session.tokens(), &["cmd06"]);
        session.apply(UserAction::Backspace);
        assert!(session.tokens().is_empty());

        // ReloadCatalog replaces catalog cleanly and updates preview
        session.apply(UserAction::ReloadCatalog);
        assert_eq!(session.visible_col_items(Column::First).len(), 1);
        assert_eq!(session.preview_command(), "reloaded-tool");
    }

    #[test]
    fn test_switching_col1_command_truncates_stale_child_tokens() {
        let mut git = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
        git.children = vec![MillerItem::new(
            "commit",
            "commit",
            ItemKind::Subcmd,
            "Commit",
        )];

        let mut cargo = MillerItem::new("cargo", "cargo", ItemKind::Cli, "Rust Cargo");
        cargo.children = vec![MillerItem::new("build", "build", ItemKind::Subcmd, "Build")];

        let introspector = CliIntrospector::with_initial_cache(ShellDialect::Posix, HashMap::new());
        let mut session = MillerSession::new(
            vec![git, cargo],
            introspector,
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        // Commit `git commit`
        session.apply(UserAction::StepRight);
        session.apply(UserAction::StepRight);
        assert_eq!(session.tokens(), &["git", "commit"]);

        // Step back to Col 1, select `cargo`, and step right: stale `commit` must be dropped.
        session.apply(UserAction::StepLeft);
        session.apply(UserAction::StepLeft);
        session.apply(UserAction::MoveDown);
        session.apply(UserAction::StepRight);
        assert_eq!(session.tokens(), &["cargo"]);
        assert_eq!(session.preview_command(), "cargo build");
    }
}
