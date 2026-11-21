use super::catalog::{cache_dir, non_empty_abs_env_path};
use super::history::MAX_HISTORY_ITEMS;
use crate::sanitize::sanitize_single_line;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub(super) const MAX_CONFIG_BYTES: u64 = 256 * 1024;
const MAX_CONFIG_ARRAY_ITEMS: usize = 1_024;
const MAX_DIAGNOSTICS: usize = 64;

/// Default CLI Introspection child-process timeout in milliseconds.
pub const DEFAULT_INTROSPECT_TIMEOUT_MS: u64 = 750;
/// Minimum allowed CLI Introspection timeout in milliseconds.
pub const MIN_INTROSPECT_TIMEOUT_MS: u64 = 100;
/// Maximum allowed CLI Introspection timeout in milliseconds.
pub const MAX_INTROSPECT_TIMEOUT_MS: u64 = 10_000;

/// Selectable terminal color palette preset for [`UiConfig`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemePreset {
    #[default]
    Dark,
    Light,
    Ansi16,
    None,
}

impl ThemePreset {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "ansi16" => Some(Self::Ansi16),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

/// Selectable glyph mode for UI indicators and status chrome.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum IconMode {
    #[default]
    Unicode,
    Ascii,
}

impl IconMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "unicode" => Some(Self::Unicode),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }
}

/// Selectable box-drawing style for Pane borders and dividers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BorderStyle {
    #[default]
    Rounded,
    Plain,
    Ascii,
}

impl BorderStyle {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "rounded" => Some(Self::Rounded),
            "plain" => Some(Self::Plain),
            "ascii" => Some(Self::Ascii),
            _ => None,
        }
    }
}

/// Default minimum terminal height (in rows) required for inline partial-height mode.
pub const DEFAULT_MIN_HEIGHT: usize = 15;
/// Maximum configurable `min_height` bound in rows.
pub const MAX_MIN_HEIGHT: usize = 100;

/// User- or CLI-specified UI height preference (`Rows(n)` or `Percent(1..=100)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightSpec {
    Rows(usize),
    Percent(u8),
}

impl HeightSpec {
    /// Validates an integer row count (`Viewport::MIN_HEIGHT..=u16::MAX`).
    pub fn from_rows(rows: usize) -> Result<Self, String> {
        let min = crate::ui::Viewport::MIN_HEIGHT;
        if rows < min {
            return Err(format!("height must be at least {min}, got {rows}"));
        }
        if rows > u16::MAX as usize {
            return Err(format!("height must be at most {}, got {rows}", u16::MAX));
        }
        Ok(Self::Rows(rows))
    }

    /// Parses a height specification from rows (`"18"`) or percentage (`"45%"`, `"100%"`).
    pub fn parse(raw: &str) -> Result<Self, String> {
        let trimmed = raw.trim();
        if let Some(pct_raw) = trimmed.strip_suffix('%') {
            if pct_raw.is_empty() || !pct_raw.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("Invalid integer or percentage: {raw}"));
            }
            let pct = pct_raw
                .parse::<u8>()
                .ok()
                .filter(|&v| (1..=100).contains(&v))
                .ok_or_else(|| format!("percentage must be between 1% and 100%, got {trimmed}"))?;
            return Ok(Self::Percent(pct));
        }
        if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("Invalid integer or percentage: {raw}"));
        }
        let rows = trimmed
            .parse::<usize>()
            .map_err(|_| format!("Invalid integer or percentage: {raw}"))?;
        Self::from_rows(rows)
    }
}

/// User-configured UI appearance and layout preferences loaded from `config.toml`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiConfig {
    pub theme: ThemePreset,
    pub icons: IconMode,
    pub borders: BorderStyle,
    pub height: Option<HeightSpec>,
    pub min_height: Option<usize>,
}

/// Resolved UI rendering style after applying environment overrides (`NO_COLOR`, `TERM=dumb`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UiStyle {
    pub theme: ThemePreset,
    pub icons: IconMode,
    pub borders: BorderStyle,
}

impl UiConfig {
    /// Returns the effective bounded `min_height` for inline mode, defaulting to
    /// [`DEFAULT_MIN_HEIGHT`] (`15`) and clamped to `Viewport::MIN_HEIGHT..=MAX_MIN_HEIGHT`.
    pub fn effective_min_height(self) -> usize {
        self.min_height
            .unwrap_or(DEFAULT_MIN_HEIGHT)
            .clamp(crate::ui::Viewport::MIN_HEIGHT, MAX_MIN_HEIGHT)
    }

    /// Resolves UI settings with explicit `NO_COLOR` and `TERM` values without mutating process env.
    pub fn resolve_env(self, no_color: Option<&str>, term: Option<&str>) -> UiStyle {
        let no_color_active = no_color.is_some_and(|v| !v.is_empty());
        let dumb_term = term.is_some_and(|v| v.trim().eq_ignore_ascii_case("dumb"));
        UiStyle {
            theme: if no_color_active || dumb_term {
                ThemePreset::None
            } else {
                self.theme
            },
            icons: self.icons,
            borders: self.borders,
        }
    }

    /// Resolves UI settings against the active process environment (`NO_COLOR` and `TERM`).
    pub fn resolve_process_env(self) -> UiStyle {
        let no_color_os = env::var_os("NO_COLOR");
        let no_color = no_color_os
            .as_deref()
            .and_then(|v| (!v.is_empty()).then_some("1"));
        let term = env::var("TERM").ok();
        self.resolve_env(no_color, term.as_deref())
    }
}

impl From<UiConfig> for UiStyle {
    fn from(ui: UiConfig) -> Self {
        Self {
            theme: ui.theme,
            icons: ui.icons,
            borders: ui.borders,
        }
    }
}

/// Session navigation/editing action configurable under `[keybindings]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindableAction {
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
    ClearFilter,
    ReloadCatalog,
    ToggleInspector,
    ChainPipeline,
    Confirm,
    Cancel,
}

impl BindableAction {
    fn parse_key_name(key: &str) -> Option<Self> {
        match key {
            "move_up" | "up" => Some(Self::MoveUp),
            "move_down" | "down" => Some(Self::MoveDown),
            "page_up" => Some(Self::PageUp),
            "page_down" => Some(Self::PageDown),
            "move_home" | "home" => Some(Self::MoveHome),
            "move_end" | "end" => Some(Self::MoveEnd),
            "step_right" | "right" => Some(Self::StepRight),
            "step_left" | "left" => Some(Self::StepLeft),
            "accept_token" | "accept" => Some(Self::AcceptToken),
            "backspace" => Some(Self::Backspace),
            "clear_filter" => Some(Self::ClearFilter),
            "reload_catalog" | "reload" => Some(Self::ReloadCatalog),
            "toggle_inspector" | "inspector" => Some(Self::ToggleInspector),
            "chain_pipeline" | "pipeline" => Some(Self::ChainPipeline),
            "confirm" => Some(Self::Confirm),
            "cancel" => Some(Self::Cancel),
            _ => None,
        }
    }
}

/// Parsed keyboard chord (`code` + `modifiers`) used in `[keybindings]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyChord {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyChord {
    /// Parses a human-readable key chord such as `"ctrl+n"`, `"alt+i"`, `"f1"`, or `"enter"`.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return None;
        }

        let (mods_part, key_part) = if trimmed == "+" {
            ("", "+")
        } else if let Some(prefix) = trimmed.strip_suffix("++") {
            (prefix, "+")
        } else if let Some((left, right)) = trimmed.rsplit_once('+') {
            if left.is_empty() || right.is_empty() {
                return None;
            }
            (left, right)
        } else {
            ("", trimmed)
        };

        let mut modifiers = KeyModifiers::NONE;
        if !mods_part.is_empty() {
            for part in mods_part.split('+') {
                let flag = match part.trim().to_ascii_lowercase().as_str() {
                    "ctrl" | "control" => KeyModifiers::CONTROL,
                    "alt" | "meta" => KeyModifiers::ALT,
                    "shift" => KeyModifiers::SHIFT,
                    _ => return None,
                };
                if modifiers.contains(flag) {
                    return None;
                }
                modifiers |= flag;
            }
        }

        let key_trimmed = key_part.trim();
        let lower = key_trimmed.to_ascii_lowercase();
        let code = match lower.as_str() {
            "enter" | "return" => KeyCode::Enter,
            "esc" | "escape" => KeyCode::Esc,
            "tab" => KeyCode::Tab,
            "backtab" => KeyCode::BackTab,
            "backspace" | "bs" => KeyCode::Backspace,
            "space" => KeyCode::Char(' '),
            "up" => KeyCode::Up,
            "down" => KeyCode::Down,
            "left" => KeyCode::Left,
            "right" => KeyCode::Right,
            "home" => KeyCode::Home,
            "end" => KeyCode::End,
            "pageup" | "pgup" => KeyCode::PageUp,
            "pagedown" | "pgdn" => KeyCode::PageDown,
            "delete" | "del" => KeyCode::Delete,
            "insert" | "ins" => KeyCode::Insert,
            _ if lower.starts_with('f') && lower.len() >= 2 => {
                let num = lower[1..].parse::<u8>().ok()?;
                if !(1..=24).contains(&num) {
                    return None;
                }
                KeyCode::F(num)
            }
            _ => {
                let mut chars = key_trimmed.chars();
                let ch = chars.next()?;
                if chars.next().is_some() || ch.is_control() {
                    return None;
                }
                if modifiers.contains(KeyModifiers::CONTROL)
                    || modifiers.contains(KeyModifiers::ALT)
                {
                    KeyCode::Char(ch.to_ascii_lowercase())
                } else if modifiers.contains(KeyModifiers::SHIFT) && ch.is_ascii_alphabetic() {
                    KeyCode::Char(ch.to_ascii_uppercase())
                } else {
                    KeyCode::Char(ch)
                }
            }
        };

        Some(Self { code, modifiers })
    }
}

/// Custom `[keybindings]` overrides loaded from `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingsConfig {
    pub bindings: Vec<(KeyChord, BindableAction)>,
}

impl KeybindingsConfig {
    /// Resolves a terminal `KeyEvent` against custom `[keybindings]` overrides (last match wins).
    pub fn resolve(&self, key: KeyEvent) -> Option<BindableAction> {
        if self.bindings.is_empty()
            || !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            return None;
        }
        let norm_code = match key.code {
            KeyCode::Char(c)
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                KeyCode::Char(c.to_ascii_lowercase())
            }
            other => other,
        };
        self.bindings
            .iter()
            .rev()
            .find(|(chord, _)| chord.code == norm_code && chord.modifiers == key.modifiers)
            .map(|(_, action)| *action)
    }
}

/// Non-fatal diagnostic emitted while reading or parsing `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDiagnostic {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for ConfigDiagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line > 0 {
            write!(f, "line {}: {}", self.line, self.message)
        } else {
            write!(f, "{}", self.message)
        }
    }
}

/// Optional user overrides loaded from `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserConfig {
    pub priority_tools: Vec<String>,
    pub disabled_tools: Vec<String>,
    pub history_limit: Option<usize>,
    pub introspect_timeout_ms: Option<u64>,
    pub sync_profile: bool,
    pub ui: UiConfig,
    pub keybindings: KeybindingsConfig,
}

impl UserConfig {
    /// Returns the effective bounded CLI Introspection timeout, defaulting to
    /// [`DEFAULT_INTROSPECT_TIMEOUT_MS`] (`750ms`) and clamped to
    /// [`MIN_INTROSPECT_TIMEOUT_MS`]..=[`MAX_INTROSPECT_TIMEOUT_MS`] (`100..=10_000` ms).
    pub fn effective_introspect_timeout(&self) -> Duration {
        let ms = self
            .introspect_timeout_ms
            .unwrap_or(DEFAULT_INTROSPECT_TIMEOUT_MS)
            .clamp(MIN_INTROSPECT_TIMEOUT_MS, MAX_INTROSPECT_TIMEOUT_MS);
        Duration::from_millis(ms)
    }
}

/// Location of `zpane`'s user configuration file (`ZPANE_CONFIG`, `%LOCALAPPDATA%\zpane\config.toml`,
/// `$XDG_CONFIG_HOME/zpane/config.toml`, or `~/.config/zpane/config.toml`).
pub fn config_path() -> PathBuf {
    if let Ok(explicit) = env::var("ZPANE_CONFIG") {
        let trimmed = explicit.trim();
        if !trimmed.is_empty() && !trimmed.contains('\0') {
            return PathBuf::from(trimmed);
        }
    }
    if cfg!(windows) {
        non_empty_abs_env_path("LOCALAPPDATA")
            .or_else(|| {
                non_empty_abs_env_path("USERPROFILE").map(|p| p.join("AppData").join("Local"))
            })
            .unwrap_or_else(env::temp_dir)
            .join("zpane")
            .join("config.toml")
    } else if let Some(xdg) = non_empty_abs_env_path("XDG_CONFIG_HOME") {
        xdg.join("zpane").join("config.toml")
    } else if let Some(home) = non_empty_abs_env_path("HOME") {
        home.join(".config").join("zpane").join("config.toml")
    } else {
        cache_dir().join("config.toml")
    }
}

fn strip_toml_comment(line: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for (idx, ch) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return &line[..idx],
            _ => {}
        }
    }
    line
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigSection {
    Root,
    Discovery,
    Ui,
    Keybindings,
    Unknown,
}

impl ConfigSection {
    fn label(self) -> &'static str {
        match self {
            Self::Root => "top-level",
            Self::Discovery => "[discovery]",
            Self::Ui => "[ui]",
            Self::Keybindings => "[keybindings]",
            Self::Unknown => "unknown section",
        }
    }
}

fn push_diag(diags: &mut Vec<ConfigDiagnostic>, line: usize, message: impl Into<String>) {
    if diags.len() >= MAX_DIAGNOSTICS {
        return;
    }
    let clean = sanitize_single_line(&message.into()).into_owned();
    diags.push(ConfigDiagnostic {
        line,
        message: clean,
    });
}

fn has_unquoted_equals(line: &str) -> bool {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in line.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_double => escaped = true,
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '=' if !in_single && !in_double => return true,
            _ => {}
        }
    }
    false
}

fn looks_like_section_header(line: &str) -> bool {
    line.starts_with('[')
        && !line.starts_with("[\"")
        && !line.starts_with("['")
        && !line.starts_with("[]")
        && line
            .chars()
            .nth(1)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '[')
}

fn parse_toml_string_literal(raw: &str) -> Result<String, &'static str> {
    let trimmed = raw.trim();
    let mut chars = trimmed.chars();
    let Some(quote @ ('"' | '\'')) = chars.next() else {
        return Err("expected quoted string literal");
    };
    let mut val = String::new();
    let mut escaped = false;
    let mut closed = false;
    for ch in chars.by_ref() {
        if escaped {
            let decoded = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                other => other,
            };
            val.push(decoded);
            escaped = false;
        } else if ch == '\\' && quote == '"' {
            escaped = true;
        } else if ch == quote {
            closed = true;
            break;
        } else {
            val.push(ch);
        }
    }
    if !closed {
        return Err("unclosed string literal");
    }
    if !chars.as_str().trim().is_empty() {
        return Err("unexpected trailing characters after string literal");
    }
    Ok(sanitize_single_line(val.trim()).into_owned())
}

fn parse_toml_string_array_checked(raw: &str) -> Result<Vec<String>, &'static str> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Err("expected array enclosed in `[...]`");
    };
    let mut out = Vec::new();
    let mut rest = inner.trim();
    while !rest.is_empty() {
        let Some(quote @ ('"' | '\'')) = rest.chars().next() else {
            return Err("expected quoted string element inside array");
        };
        rest = &rest[1..];
        let mut val = String::new();
        let mut escaped = false;
        let mut end_idx = None;
        for (idx, ch) in rest.char_indices() {
            if escaped {
                let decoded = match ch {
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    other => other,
                };
                val.push(decoded);
                escaped = false;
            } else if ch == '\\' && quote == '"' {
                escaped = true;
            } else if ch == quote {
                end_idx = Some(idx);
                break;
            } else {
                val.push(ch);
            }
        }
        let Some(close_idx) = end_idx else {
            return Err("unclosed string inside array");
        };
        let sanitized = sanitize_single_line(val.trim());
        if !sanitized.is_empty() && out.len() < MAX_CONFIG_ARRAY_ITEMS {
            out.push(sanitized.into_owned());
        }
        rest = rest[close_idx + 1..].trim();
        if rest.is_empty() {
            break;
        }
        let Some(after_comma) = rest.strip_prefix(',') else {
            return Err("expected `,` between array items");
        };
        rest = after_comma.trim();
    }
    Ok(out)
}

fn parse_toml_usize(raw: &str) -> Option<usize> {
    let raw_num = raw.trim();
    if raw_num.is_empty() {
        return None;
    }
    if !raw_num.starts_with('_') && !raw_num.ends_with('_') && !raw_num.contains("__") {
        if raw_num.contains('_') {
            raw_num.replace('_', "").parse::<usize>().ok()
        } else {
            raw_num.parse::<usize>().ok()
        }
    } else {
        None
    }
}

fn parse_toml_u64(raw: &str) -> Option<u64> {
    let raw_num = raw.trim();
    if raw_num.is_empty() {
        return None;
    }
    if !raw_num.starts_with('_') && !raw_num.ends_with('_') && !raw_num.contains("__") {
        if raw_num.contains('_') {
            raw_num.replace('_', "").parse::<u64>().ok()
        } else {
            raw_num.parse::<u64>().ok()
        }
    } else {
        None
    }
}

fn parse_toml_bool(raw: &str) -> Option<bool> {
    match raw.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

fn parse_keybinding_chords(raw: &str) -> Result<Vec<KeyChord>, String> {
    let trimmed = raw.trim();
    let items = if trimmed.starts_with('[') {
        parse_toml_string_array_checked(trimmed).map_err(str::to_string)?
    } else {
        let single = parse_toml_string_literal(trimmed).map_err(str::to_string)?;
        if single.is_empty() {
            return Err("keybinding chord must not be empty".to_string());
        }
        vec![single]
    };
    let mut chords = Vec::with_capacity(items.len());
    for item in items {
        let Some(chord) = KeyChord::parse(&item) else {
            return Err(format!("invalid key chord '{item}'"));
        };
        chords.push(chord);
    }
    Ok(chords)
}

fn apply_config_entry(
    cfg: &mut UserConfig,
    section: ConfigSection,
    key: &str,
    val: &str,
    line_no: usize,
    diags: &mut Vec<ConfigDiagnostic>,
) {
    match section {
        ConfigSection::Unknown => {}
        ConfigSection::Keybindings => {
            let Some(action) = BindableAction::parse_key_name(key) else {
                push_diag(
                    diags,
                    line_no,
                    format!("Unknown key '{key}' in [keybindings]"),
                );
                return;
            };
            match parse_keybinding_chords(val) {
                Ok(chords) => {
                    for chord in chords {
                        cfg.keybindings.bindings.push((chord, action));
                    }
                }
                Err(err) => {
                    push_diag(diags, line_no, format!("Invalid keybinding '{key}': {err}"));
                }
            }
        }
        ConfigSection::Discovery | ConfigSection::Ui | ConfigSection::Root => {
            let allow_discovery = matches!(section, ConfigSection::Root | ConfigSection::Discovery);
            let allow_ui = matches!(section, ConfigSection::Root | ConfigSection::Ui);
            match key {
                "priority_tools" if allow_discovery => match parse_toml_string_array_checked(val) {
                    Ok(items) => cfg.priority_tools = items,
                    Err(err) => push_diag(
                        diags,
                        line_no,
                        format!("Malformed array for 'priority_tools': {err}"),
                    ),
                },
                "disabled_tools" if allow_discovery => match parse_toml_string_array_checked(val) {
                    Ok(items) => cfg.disabled_tools = items,
                    Err(err) => push_diag(
                        diags,
                        line_no,
                        format!("Malformed array for 'disabled_tools': {err}"),
                    ),
                },
                "history_limit" if allow_discovery => match parse_toml_usize(val) {
                    Some(limit) => cfg.history_limit = Some(limit.min(MAX_HISTORY_ITEMS)),
                    None => push_diag(
                        diags,
                        line_no,
                        format!("Invalid integer for 'history_limit': {}", val.trim()),
                    ),
                },
                "introspect_timeout_ms" if allow_discovery => match parse_toml_u64(val) {
                    Some(ms) => {
                        let clamped =
                            ms.clamp(MIN_INTROSPECT_TIMEOUT_MS, MAX_INTROSPECT_TIMEOUT_MS);
                        cfg.introspect_timeout_ms = Some(clamped);
                        if clamped != ms {
                            push_diag(
                                diags,
                                line_no,
                                format!(
                                    "Value for 'introspect_timeout_ms' ({ms}) out of range ({MIN_INTROSPECT_TIMEOUT_MS}..={MAX_INTROSPECT_TIMEOUT_MS}); clamped to {clamped}"
                                ),
                            );
                        }
                    }
                    None => push_diag(
                        diags,
                        line_no,
                        format!(
                            "Invalid integer for 'introspect_timeout_ms': {}",
                            val.trim()
                        ),
                    ),
                },
                "sync_profile" if allow_discovery => match parse_toml_bool(val) {
                    Some(b) => cfg.sync_profile = b,
                    None => push_diag(
                        diags,
                        line_no,
                        format!("Invalid boolean for 'sync_profile': {}", val.trim()),
                    ),
                },
                "theme" if allow_ui => match parse_toml_string_literal(val) {
                    Ok(s) => match ThemePreset::parse(&s) {
                        Some(preset) => cfg.ui.theme = preset,
                        None => push_diag(
                            diags,
                            line_no,
                            format!("Unknown theme '{s}' (expected: dark, light, ansi16, none)"),
                        ),
                    },
                    Err(err) => {
                        push_diag(diags, line_no, format!("Invalid value for 'theme': {err}"))
                    }
                },
                "icons" if allow_ui => match parse_toml_string_literal(val) {
                    Ok(s) => match IconMode::parse(&s) {
                        Some(mode) => cfg.ui.icons = mode,
                        None => push_diag(
                            diags,
                            line_no,
                            format!("Unknown icons mode '{s}' (expected: unicode, ascii)"),
                        ),
                    },
                    Err(err) => {
                        push_diag(diags, line_no, format!("Invalid value for 'icons': {err}"))
                    }
                },
                "borders" if allow_ui => match parse_toml_string_literal(val) {
                    Ok(s) => match BorderStyle::parse(&s) {
                        Some(style) => cfg.ui.borders = style,
                        None => push_diag(
                            diags,
                            line_no,
                            format!(
                                "Unknown borders style '{s}' (expected: rounded, plain, ascii)"
                            ),
                        ),
                    },
                    Err(err) => push_diag(
                        diags,
                        line_no,
                        format!("Invalid value for 'borders': {err}"),
                    ),
                },
                "height" if allow_ui => {
                    if let Some(rows) = parse_toml_usize(val) {
                        match HeightSpec::from_rows(rows) {
                            Ok(spec) => cfg.ui.height = Some(spec),
                            Err(msg) => push_diag(
                                diags,
                                line_no,
                                format!("Invalid value for 'height': {msg}"),
                            ),
                        }
                    } else {
                        match parse_toml_string_literal(val) {
                            Ok(s) => match HeightSpec::parse(&s) {
                                Ok(spec) => cfg.ui.height = Some(spec),
                                Err(msg) => push_diag(
                                    diags,
                                    line_no,
                                    format!("Invalid value for 'height': {msg}"),
                                ),
                            },
                            Err(err) => push_diag(
                                diags,
                                line_no,
                                format!("Invalid value for 'height': {err}"),
                            ),
                        }
                    }
                }
                "min_height" if allow_ui => match parse_toml_usize(val) {
                    Some(h) => {
                        let min_bound = crate::ui::Viewport::MIN_HEIGHT;
                        let clamped = h.clamp(min_bound, MAX_MIN_HEIGHT);
                        cfg.ui.min_height = Some(clamped);
                        if clamped != h {
                            push_diag(
                                diags,
                                line_no,
                                format!(
                                    "Value for 'min_height' ({h}) out of range ({min_bound}..={MAX_MIN_HEIGHT}); clamped to {clamped}"
                                ),
                            );
                        }
                    }
                    None => push_diag(
                        diags,
                        line_no,
                        format!("Invalid integer for 'min_height': {}", val.trim()),
                    ),
                },
                _ => push_diag(
                    diags,
                    line_no,
                    format!("Unknown key '{key}' in {}", section.label()),
                ),
            }
        }
    }
}

/// Parses `config.toml` supporting `[discovery]`, `[ui]`, `[keybindings]`, and top-level
/// backward-compatible keys, returning non-fatal [`ConfigDiagnostic`] warnings for issues.
pub fn parse_user_config_toml_with_diagnostics(raw: &str) -> (UserConfig, Vec<ConfigDiagnostic>) {
    let mut cfg = UserConfig::default();
    let mut diags = Vec::new();
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        push_diag(
            &mut diags,
            0,
            format!("Config exceeds {MAX_CONFIG_BYTES} byte limit; using defaults"),
        );
        return (cfg, diags);
    }

    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let mut lines = raw
        .lines()
        .enumerate()
        .map(|(idx, line)| (idx + 1, strip_toml_comment(line).trim()))
        .filter(|(_, line)| !line.is_empty())
        .peekable();

    let mut section = ConfigSection::Root;
    let mut array_buf = String::new();

    while let Some((line_no, line)) = lines.next() {
        if line.starts_with('[') {
            if let Some(section_name) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let trimmed_name = section_name.trim();
                section = match trimmed_name {
                    "discovery" => ConfigSection::Discovery,
                    "ui" => ConfigSection::Ui,
                    "keybindings" => ConfigSection::Keybindings,
                    _ => {
                        push_diag(
                            &mut diags,
                            line_no,
                            format!("Unknown section '[{trimmed_name}]'"),
                        );
                        ConfigSection::Unknown
                    }
                };
            } else {
                push_diag(
                    &mut diags,
                    line_no,
                    format!("Malformed section header '{line}'"),
                );
                section = ConfigSection::Unknown;
            }
            continue;
        }

        let Some((key_raw, val_raw)) = line.split_once('=') else {
            if section != ConfigSection::Unknown {
                push_diag(
                    &mut diags,
                    line_no,
                    format!("Expected `key = value` in {}", section.label()),
                );
            }
            continue;
        };

        let key = key_raw.trim();
        if key.is_empty() {
            push_diag(&mut diags, line_no, "Missing key before `=`");
            continue;
        }
        let val_start = val_raw.trim();

        if val_start.starts_with('[') && !val_start.ends_with(']') {
            array_buf.clear();
            array_buf.push_str(val_start);
            let mut closed = false;
            while let Some(&(_, next_line)) = lines.peek() {
                if looks_like_section_header(next_line) || has_unquoted_equals(next_line) {
                    break;
                }
                let (_, consumed) = lines.next().expect("peeked line exists");
                array_buf.push(' ');
                array_buf.push_str(consumed);
                if consumed.ends_with(']') {
                    closed = true;
                    break;
                }
            }
            if !closed {
                push_diag(
                    &mut diags,
                    line_no,
                    format!("Unclosed array for key '{key}'"),
                );
                continue;
            }
            apply_config_entry(&mut cfg, section, key, &array_buf, line_no, &mut diags);
        } else {
            apply_config_entry(&mut cfg, section, key, val_start, line_no, &mut diags);
        }
    }

    (cfg, diags)
}

/// Parses `config.toml` supporting `[discovery]`, `[ui]`, `[keybindings]`, and top-level keys.
pub fn parse_user_config_toml(raw: &str) -> UserConfig {
    parse_user_config_toml_with_diagnostics(raw).0
}

fn load_user_config_from_path_with_diagnostics(path: &Path) -> (UserConfig, Vec<ConfigDiagnostic>) {
    let mut diags = Vec::new();
    let pre = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return (UserConfig::default(), diags),
        Err(e) => {
            push_diag(
                &mut diags,
                0,
                format!("Failed to inspect config '{}': {e}", path.display()),
            );
            return (UserConfig::default(), diags);
        }
    };
    if !pre.file_type().is_file() {
        push_diag(
            &mut diags,
            0,
            format!("Config path '{}' is not a regular file", path.display()),
        );
        return (UserConfig::default(), diags);
    }
    if pre.len() > MAX_CONFIG_BYTES {
        push_diag(
            &mut diags,
            0,
            format!(
                "Config '{}' exceeds {MAX_CONFIG_BYTES} byte limit; using defaults",
                path.display()
            ),
        );
        return (UserConfig::default(), diags);
    }
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            push_diag(
                &mut diags,
                0,
                format!("Failed to open config '{}': {e}", path.display()),
            );
            return (UserConfig::default(), diags);
        }
    };
    let meta = match file.metadata() {
        Ok(m) => m,
        Err(e) => {
            push_diag(
                &mut diags,
                0,
                format!("Failed to inspect open config '{}': {e}", path.display()),
            );
            return (UserConfig::default(), diags);
        }
    };
    if !meta.file_type().is_file() || meta.len() > MAX_CONFIG_BYTES {
        push_diag(
            &mut diags,
            0,
            format!(
                "Config '{}' is invalid or oversized; using defaults",
                path.display()
            ),
        );
        return (UserConfig::default(), diags);
    }
    let mut buf = Vec::with_capacity(meta.len().min(65_536) as usize);
    if let Err(e) = file
        .take(MAX_CONFIG_BYTES.saturating_add(1))
        .read_to_end(&mut buf)
    {
        push_diag(
            &mut diags,
            0,
            format!("Failed to read config '{}': {e}", path.display()),
        );
        return (UserConfig::default(), diags);
    }
    if (buf.len() as u64) > MAX_CONFIG_BYTES {
        push_diag(
            &mut diags,
            0,
            format!(
                "Config '{}' exceeds {MAX_CONFIG_BYTES} byte limit; using defaults",
                path.display()
            ),
        );
        return (UserConfig::default(), diags);
    }
    let raw = match String::from_utf8(buf) {
        Ok(s) => s,
        Err(e) => {
            push_diag(
                &mut diags,
                0,
                format!("Config '{}' contains invalid UTF-8: {e}", path.display()),
            );
            return (UserConfig::default(), diags);
        }
    };
    parse_user_config_toml_with_diagnostics(&raw)
}

/// Loads and parses the user configuration file from [`config_path`], returning defaults plus
/// non-fatal [`ConfigDiagnostic`] warnings on missing, oversized, or malformed files.
pub fn load_user_config_with_diagnostics() -> (UserConfig, Vec<ConfigDiagnostic>) {
    load_user_config_from_path_with_diagnostics(&config_path())
}

/// Loads and parses the user configuration file from [`config_path`], returning defaults on
/// missing, oversized, or unreadable files.
pub fn load_user_config() -> UserConfig {
    load_user_config_with_diagnostics().0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_user_config_toml_supports_comments_multiline_arrays_and_underscores() {
        let raw = r#"
# Custom zpane preferences
history_limit = 25
priority_tools = [
  "mycli",
  "gs",
]
disabled_tools = ["blocked"] # hide unwanted binaries
"#;
        assert_eq!(
            parse_user_config_toml(raw),
            UserConfig {
                priority_tools: vec!["mycli".to_string(), "gs".to_string()],
                disabled_tools: vec!["blocked".to_string()],
                history_limit: Some(25),
                ..UserConfig::default()
            }
        );

        let with_bom = "\u{feff}history_limit = 1_500\npriority_tools = [\" mycli \"]\n";
        assert_eq!(
            parse_user_config_toml(with_bom),
            UserConfig {
                priority_tools: vec!["mycli".to_string()],
                disabled_tools: Vec::new(),
                history_limit: Some(1500),
                ..UserConfig::default()
            }
        );

        let clamped = parse_user_config_toml("history_limit = 999999\n");
        assert_eq!(clamped.history_limit, Some(MAX_HISTORY_ITEMS));
    }

    #[test]
    fn test_parse_user_config_toml_supports_sections_and_top_level_ui_compat() {
        let sectioned = r#"
[discovery]
priority_tools = ["git", "cargo"]
disabled_tools = ["legacy"]
history_limit = 40

[ui]
theme = "light"
icons = "ascii"
borders = "plain"

[keybindings]
toggle_inspector = "ctrl+t"
move_down = ["ctrl+n", "alt+j"]
confirm = "ctrl+y"
"#;
        let (cfg, diags) = parse_user_config_toml_with_diagnostics(sectioned);
        assert!(diags.is_empty(), "expected zero diagnostics, got {diags:?}");
        assert_eq!(cfg.priority_tools, vec!["git", "cargo"]);
        assert_eq!(cfg.disabled_tools, vec!["legacy"]);
        assert_eq!(cfg.history_limit, Some(40));
        assert_eq!(
            cfg.ui,
            UiConfig {
                theme: ThemePreset::Light,
                icons: IconMode::Ascii,
                borders: BorderStyle::Plain,
                ..UiConfig::default()
            }
        );
        assert_eq!(
            cfg.keybindings
                .resolve(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(BindableAction::ToggleInspector)
        );
        assert_eq!(
            cfg.keybindings
                .resolve(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT)),
            Some(BindableAction::MoveDown)
        );
        assert_eq!(
            cfg.keybindings
                .resolve(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
            Some(BindableAction::Confirm)
        );

        let top_level_ui = r#"
theme = "ansi16"
icons = "ascii"
borders = "ascii"
history_limit = 10
"#;
        let (top_cfg, top_diags) = parse_user_config_toml_with_diagnostics(top_level_ui);
        assert!(
            top_diags.is_empty(),
            "unexpected diagnostics: {top_diags:?}"
        );
        assert_eq!(
            top_cfg.ui,
            UiConfig {
                theme: ThemePreset::Ansi16,
                icons: IconMode::Ascii,
                borders: BorderStyle::Ascii,
                ..UiConfig::default()
            }
        );
        assert_eq!(top_cfg.history_limit, Some(10));
    }

    #[test]
    fn test_parse_chain_pipeline_keybinding() {
        let raw = r#"
[keybindings]
chain_pipeline = "ctrl+o"
pipeline = "alt+|"
"#;
        let (cfg, diags) = parse_user_config_toml_with_diagnostics(raw);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(
            cfg.keybindings
                .resolve(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(BindableAction::ChainPipeline)
        );
        assert_eq!(
            cfg.keybindings
                .resolve(KeyEvent::new(KeyCode::Char('|'), KeyModifiers::ALT)),
            Some(BindableAction::ChainPipeline)
        );
    }

    #[test]
    fn test_parse_user_config_toml_with_diagnostics_reports_non_fatal_errors() {
        let raw = r#"
theme = "solarized"
unknown_top = 123
priority_tools = [
  "git"

[ui]
icons = "emoji"
borders = unquoted
priority_tools = ["wrong_section"]

[mystery]
foo = "bar"

[discovery]
history_limit = -5
disabled_tools = ["ok", 42]

[keybindings]
bogus_action = "ctrl+x"
toggle_inspector = "ctrl+badkey"
"#;
        let (cfg, diags) = parse_user_config_toml_with_diagnostics(raw);
        // Defaults preserved for invalid fields, startup never crashes
        assert_eq!(cfg.ui, UiConfig::default());
        assert_eq!(cfg.history_limit, None);

        let msgs: Vec<String> = diags.iter().map(ToString::to_string).collect();
        assert!(
            msgs.iter().any(|m| m.contains("Unknown theme")),
            "missing unknown theme diag: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Unknown key 'unknown_top'")),
            "missing unknown top key diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Unclosed array for key 'priority_tools'")),
            "missing unclosed array diag: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("Unknown icons mode")),
            "missing unknown icons diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Invalid value for 'borders'")),
            "missing unquoted borders diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Unknown key 'priority_tools' in [ui]")),
            "missing wrong-section key diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Unknown section '[mystery]'")),
            "missing unknown section diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Invalid integer for 'history_limit'")),
            "missing bad history_limit diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Malformed array for 'disabled_tools'")),
            "missing bad array element diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Unknown key 'bogus_action' in [keybindings]")),
            "missing unknown keybinding diag: {msgs:?}"
        );
        assert!(
            msgs.iter()
                .any(|m| m.contains("Invalid keybinding 'toggle_inspector'")),
            "missing bad key chord diag: {msgs:?}"
        );
    }

    #[test]
    fn test_ui_config_resolve_env_honors_non_empty_no_color_and_dumb_term() {
        let dark_unicode = UiConfig {
            theme: ThemePreset::Dark,
            icons: IconMode::Unicode,
            borders: BorderStyle::Rounded,
            ..UiConfig::default()
        };
        assert_eq!(
            dark_unicode.resolve_env(None, Some("xterm-256color")).theme,
            ThemePreset::Dark
        );
        // Empty NO_COLOR must not disable color per no-color.org
        assert_eq!(
            dark_unicode
                .resolve_env(Some(""), Some("xterm-256color"))
                .theme,
            ThemePreset::Dark
        );
        // Any non-empty NO_COLOR forces ThemePreset::None
        assert_eq!(
            dark_unicode
                .resolve_env(Some("1"), Some("xterm-256color"))
                .theme,
            ThemePreset::None
        );
        assert_eq!(
            dark_unicode.resolve_env(Some("0"), None).theme,
            ThemePreset::None
        );
        // TERM=dumb forces ThemePreset::None
        assert_eq!(
            dark_unicode.resolve_env(None, Some("dumb")).theme,
            ThemePreset::None
        );
        assert_eq!(
            dark_unicode.resolve_env(None, Some(" DUMB ")).theme,
            ThemePreset::None
        );
    }

    #[test]
    fn test_load_user_config_from_path_with_diagnostics_missing_and_malformed_file() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let missing = env::temp_dir().join(format!(
            "zpane_cfg_missing_{}_{uniq}.toml",
            std::process::id()
        ));
        let (cfg_missing, diags_missing) = load_user_config_from_path_with_diagnostics(&missing);
        assert_eq!(cfg_missing, UserConfig::default());
        assert!(diags_missing.is_empty());

        let bad = env::temp_dir().join(format!("zpane_cfg_bad_{}_{uniq}.toml", std::process::id()));
        fs::write(&bad, "[ui]\ntheme = \"light\"\nbogus = 1\n").unwrap();
        let (cfg_bad, diags_bad) = load_user_config_from_path_with_diagnostics(&bad);
        let _ = fs::remove_file(&bad);
        assert_eq!(cfg_bad.ui.theme, ThemePreset::Light);
        assert_eq!(diags_bad.len(), 1);
        assert_eq!(diags_bad[0].line, 3);
        assert!(diags_bad[0].message.contains("Unknown key 'bogus'"));
    }

    #[test]
    fn test_introspect_timeout_ms_parsing_bounds_and_diagnostics() {
        use std::time::Duration;

        // Default timeout is 750ms
        assert_eq!(
            UserConfig::default().effective_introspect_timeout(),
            Duration::from_millis(750)
        );
        assert_eq!(UserConfig::default().introspect_timeout_ms, None);

        // Top-level and [discovery] parsing with underscores
        let (top_cfg, top_diags) =
            parse_user_config_toml_with_diagnostics("introspect_timeout_ms = 1_500\n");
        assert!(top_diags.is_empty(), "unexpected diags: {top_diags:?}");
        assert_eq!(top_cfg.introspect_timeout_ms, Some(1500));
        assert_eq!(
            top_cfg.effective_introspect_timeout(),
            Duration::from_millis(1500)
        );

        let (disc_cfg, disc_diags) =
            parse_user_config_toml_with_diagnostics("[discovery]\nintrospect_timeout_ms = 250\n");
        assert!(disc_diags.is_empty(), "unexpected diags: {disc_diags:?}");
        assert_eq!(disc_cfg.introspect_timeout_ms, Some(250));
        assert_eq!(
            disc_cfg.effective_introspect_timeout(),
            Duration::from_millis(250)
        );

        // Out-of-range values clamp safely to 100..=10_000 ms with non-fatal diagnostics
        let (low_cfg, low_diags) =
            parse_user_config_toml_with_diagnostics("[discovery]\nintrospect_timeout_ms = 25\n");
        assert_eq!(low_cfg.introspect_timeout_ms, Some(100));
        assert_eq!(
            low_cfg.effective_introspect_timeout(),
            Duration::from_millis(100)
        );
        assert_eq!(low_diags.len(), 1);
        assert!(low_diags[0].message.contains("introspect_timeout_ms"));

        let (high_cfg, high_diags) =
            parse_user_config_toml_with_diagnostics("introspect_timeout_ms = 99_999\n");
        assert_eq!(high_cfg.introspect_timeout_ms, Some(10_000));
        assert_eq!(
            high_cfg.effective_introspect_timeout(),
            Duration::from_millis(10_000)
        );
        assert_eq!(high_diags.len(), 1);
        assert!(high_diags[0].message.contains("introspect_timeout_ms"));

        // Invalid syntax and wrong section emit non-fatal diagnostics and keep defaults
        let (bad_cfg, bad_diags) = parse_user_config_toml_with_diagnostics(
            "introspect_timeout_ms = -50\n[ui]\nintrospect_timeout_ms = 500\n[discovery]\nintrospect_timeout_ms = \"fast\"\n",
        );
        assert_eq!(bad_cfg.introspect_timeout_ms, None);
        assert_eq!(
            bad_cfg.effective_introspect_timeout(),
            Duration::from_millis(750)
        );
        assert_eq!(bad_diags.len(), 3);

        // Programmatically constructed out-of-range values are still safely bounded
        let manual_low = UserConfig {
            introspect_timeout_ms: Some(1),
            ..UserConfig::default()
        };
        assert_eq!(
            manual_low.effective_introspect_timeout(),
            Duration::from_millis(100)
        );
        let manual_high = UserConfig {
            introspect_timeout_ms: Some(60_000),
            ..UserConfig::default()
        };
        assert_eq!(
            manual_high.effective_introspect_timeout(),
            Duration::from_millis(10_000)
        );
    }

    #[test]
    fn test_sync_profile_parsing_and_diagnostics() {
        assert!(!UserConfig::default().sync_profile);

        let (top_true, top_diags) =
            parse_user_config_toml_with_diagnostics("sync_profile = true\n");
        assert!(top_diags.is_empty(), "unexpected diags: {top_diags:?}");
        assert!(top_true.sync_profile);

        let (disc_false, disc_diags) =
            parse_user_config_toml_with_diagnostics("[discovery]\nsync_profile = false\n");
        assert!(disc_diags.is_empty(), "unexpected diags: {disc_diags:?}");
        assert!(!disc_false.sync_profile);

        let (bad_cfg, bad_diags) = parse_user_config_toml_with_diagnostics(
            "sync_profile = \"true\"\n[ui]\nsync_profile = true\n[discovery]\nsync_profile = yes\n",
        );
        assert!(!bad_cfg.sync_profile);
        assert_eq!(bad_diags.len(), 3);
    }

    #[test]
    fn test_ui_height_and_min_height_parsing_bounds_and_diagnostics() {
        assert_eq!(UiConfig::default().height, None);
        assert_eq!(UiConfig::default().min_height, None);
        assert_eq!(UiConfig::default().effective_min_height(), 15);

        // HeightSpec::parse unit cases
        assert_eq!(HeightSpec::parse("18"), Ok(HeightSpec::Rows(18)));
        assert_eq!(HeightSpec::parse(" 45% "), Ok(HeightSpec::Percent(45)));
        assert_eq!(HeightSpec::parse("100%"), Ok(HeightSpec::Percent(100)));
        assert!(HeightSpec::parse("11").is_err());
        assert!(HeightSpec::parse("70000").is_err());
        assert!(HeightSpec::parse("0%").is_err());
        assert!(HeightSpec::parse("101%").is_err());
        assert!(HeightSpec::parse("abc").is_err());

        // [ui] section with percentage height and valid min_height
        let (pct_cfg, pct_diags) =
            parse_user_config_toml_with_diagnostics("[ui]\nheight = \"45%\"\nmin_height = 18\n");
        assert!(pct_diags.is_empty(), "unexpected diags: {pct_diags:?}");
        assert_eq!(pct_cfg.ui.height, Some(HeightSpec::Percent(45)));
        assert_eq!(pct_cfg.ui.min_height, Some(18));
        assert_eq!(pct_cfg.ui.effective_min_height(), 18);

        // Top-level compat with integer height
        let (rows_cfg, rows_diags) =
            parse_user_config_toml_with_diagnostics("height = 18\nmin_height = 12\n");
        assert!(rows_diags.is_empty(), "unexpected diags: {rows_diags:?}");
        assert_eq!(rows_cfg.ui.height, Some(HeightSpec::Rows(18)));
        assert_eq!(rows_cfg.ui.effective_min_height(), 12);

        // Out-of-range min_height clamps to 12..=100 with non-fatal diagnostics
        let (clamped_cfg, clamped_diags) =
            parse_user_config_toml_with_diagnostics("[ui]\nmin_height = 5\n");
        assert_eq!(clamped_cfg.ui.min_height, Some(12));
        assert_eq!(clamped_cfg.ui.effective_min_height(), 12);
        assert_eq!(clamped_diags.len(), 1);
        assert!(clamped_diags[0].message.contains("min_height"));

        let (clamped_hi, clamped_hi_diags) =
            parse_user_config_toml_with_diagnostics("min_height = 250\n");
        assert_eq!(clamped_hi.ui.effective_min_height(), 100);
        assert_eq!(clamped_hi_diags.len(), 1);

        // Invalid height/min_height or wrong section emit non-fatal diagnostics and keep defaults
        let (bad_cfg, bad_diags) = parse_user_config_toml_with_diagnostics(
            "height = \"0%\"\n[ui]\nheight = 5\nmin_height = -3\n[discovery]\nheight = 18\nmin_height = 20\n",
        );
        assert_eq!(bad_cfg.ui.height, None);
        assert_eq!(bad_cfg.ui.min_height, None);
        assert_eq!(bad_cfg.ui.effective_min_height(), 15);
        assert_eq!(bad_diags.len(), 5);
    }
}
