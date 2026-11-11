mod discovery;
mod fuzzy;
mod model;
mod ui;

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use discovery::{
    common_var_items, ensure_ps_metadata_async, introspect_cli_chain, load_cli_disk_cache,
    load_col1_commands, save_cli_disk_cache, spawn_background_worker, sync_powershell_metadata,
};
use fuzzy::{filter_items, FilteredItem};
use model::{BgRequest, BgResponse, ItemKind, MillerItem};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;
use ui::{render_frame, RenderState};

#[link(name = "kernel32")]
extern "system" {
    fn GetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, lpMode: *mut u32) -> i32;
    fn SetConsoleMode(hConsoleHandle: *mut std::ffi::c_void, dwMode: u32) -> i32;
    fn WriteConsoleW(
        hConsoleOutput: *mut std::ffi::c_void,
        lpBuffer: *const u16,
        nNumberOfCharsToWrite: u32,
        lpNumberOfCharsWritten: *mut u32,
        lpReserved: *mut std::ffi::c_void,
    ) -> i32;
}

const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
const ENABLE_WRAP_AT_EOL_OUTPUT: u32 = 0x0002;
const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

struct ConOut {
    file: fs::File,
}

impl ConOut {
    fn open() -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
        let s = Self { file };
        s.enable_vt();
        Ok(s)
    }

    fn handle(&self) -> *mut std::ffi::c_void {
        self.file.as_raw_handle() as *mut std::ffi::c_void
    }

    fn enable_vt(&self) {
        unsafe {
            let h = self.handle();
            let mut mode: u32 = 0;
            if GetConsoleMode(h, &mut mode) != 0 {
                let new_mode = mode
                    | ENABLE_PROCESSED_OUTPUT
                    | ENABLE_WRAP_AT_EOL_OUTPUT
                    | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
                SetConsoleMode(h, new_mode);
            }
        }
    }

    fn write_str(&self, s: &str) {
        self.enable_vt();
        let wide: Vec<u16> = s.encode_utf16().collect();
        let mut written: u32 = 0;
        unsafe {
            WriteConsoleW(
                self.handle(),
                wide.as_ptr(),
                wide.len() as u32,
                &mut written,
                std::ptr::null_mut(),
            );
        }
    }
}

struct TerminalGuard {
    conout: ConOut,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let conout = ConOut::open()?;
        enable_raw_mode()?;
        conout.write_str("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H");
        Ok(Self { conout })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.conout.write_str("\x1b[?25h\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}

fn compute_active_parameter_sets(
    c1: Option<&MillerItem>,
    tokens: &[String],
) -> Option<HashSet<String>> {
    let c1 = c1?;
    if !matches!(c1.kind, ItemKind::Cmdlet | ItemKind::Func | ItemKind::Alias) {
        return None;
    }
    let mut active_sets: Option<HashSet<String>> = None;

    for tok in tokens.iter().skip(1) {
        let clean = tok.trim_start_matches('-').to_ascii_lowercase();
        if clean.is_empty() {
            continue;
        }

        for param in &c1.children {
            let p_name = param.text.trim_start_matches('-').to_ascii_lowercase();
            let matches_name = p_name == clean;
            let matches_alias = param
                .label
                .to_ascii_lowercase()
                .contains(&format!("-{clean}"));

            if matches_name || matches_alias {
                let non_all: HashSet<String> = param
                    .parameter_sets
                    .iter()
                    .filter(|s| s.as_str() != "__AllParameterSets")
                    .cloned()
                    .collect();

                if !non_all.is_empty() {
                    active_sets = match active_sets {
                        None => Some(non_all),
                        Some(cur) => {
                            let inter: HashSet<String> =
                                cur.intersection(&non_all).cloned().collect();
                            if inter.is_empty() {
                                Some(cur)
                            } else {
                                Some(inter)
                            }
                        }
                    };
                }
                break;
            }
        }
    }

    active_sets
}

fn resolve_col2_items(
    col1: Option<&MillerItem>,
    active_param_sets: Option<&HashSet<String>>,
    cli_cache: &HashMap<String, Vec<MillerItem>>,
    bg_tx: &mpsc::Sender<BgRequest>,
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

    let key = c1.text.to_ascii_lowercase();
    if let Some(cached) = cli_cache.get(&key) {
        return cached.clone();
    }

    let _ = bg_tx.send(BgRequest {
        cache_key: key,
        cmd_chain: vec![c1.text.clone()],
    });

    vec![MillerItem::new(
        "",
        format!("⟳ Loading subcommands & flags for '{}'...", c1.label),
        ItemKind::Loading,
        format!("Introspecting {} --help in background thread", c1.label),
    )]
}

fn resolve_col3_items(
    col1: Option<&MillerItem>,
    col2: Option<&MillerItem>,
    cli_cache: &HashMap<String, Vec<MillerItem>>,
    bg_tx: &mpsc::Sender<BgRequest>,
    custom_input: &str,
) -> Vec<MillerItem> {
    let (Some(c1), Some(c2)) = (col1, col2) else {
        return Vec::new();
    };

    if c2.kind == ItemKind::Loading {
        return Vec::new();
    }

    let mut items = Vec::new();
    let trimmed_input = custom_input.trim();

    // Interactive custom argument prompt (Navi inspired)
    if !trimmed_input.is_empty() {
        items.push(MillerItem::new(
            trimmed_input,
            format!("✏ Use \"{}\"", trimmed_input),
            ItemKind::Value,
            format!("Insert custom value for {}", c2.label),
        ));
    } else if matches!(c2.kind, ItemKind::Param | ItemKind::Flag) && c2.children.is_empty() {
        items.push(MillerItem::new(
            "",
            "✏ (Type custom value / path)",
            ItemKind::Value,
            format!("Type directly in this column to specify an argument for {}", c2.label),
        ));
    }

    if !c2.children.is_empty() {
        items.extend(c2.children.clone());
        return items;
    }

    if c2.kind == ItemKind::Subcmd {
        let key = format!(
            "{}::{}",
            c1.text.to_ascii_lowercase(),
            c2.text.to_ascii_lowercase()
        );
        if let Some(cached) = cli_cache.get(&key) {
            items.extend(cached.clone());
            return items;
        }

        let _ = bg_tx.send(BgRequest {
            cache_key: key,
            cmd_chain: vec![c1.text.clone(), c2.text.clone()],
        });

        items.push(MillerItem::new(
            "",
            format!("⟳ Loading options for '{} {}'...", c1.label, c2.label),
            ItemKind::Loading,
            c2.desc.clone(),
        ));
        return items;
    }

    items.extend(common_var_items());
    items
}

fn build_preview_command(
    tokens: &[String],
    active_col: usize,
    cur_c1: Option<&MillerItem>,
    cur_c2: Option<&MillerItem>,
    cur_c3: Option<&MillerItem>,
    col3_filter: &str,
) -> String {
    let mut out: Vec<String> = tokens.to_vec();

    if out.is_empty() {
        if let Some(c1) = cur_c1 {
            if !matches!(c1.kind, ItemKind::Env | ItemKind::Var | ItemKind::Loading)
                && !c1.text.is_empty()
            {
                out.push(c1.text.clone());
            }
        }
    }

    if active_col == 1 {
        if let Some(c2) = cur_c2 {
            if c2.kind != ItemKind::Loading && !c2.text.is_empty() {
                if out.last().map(|s| s.as_str()) != Some(c2.text.as_str()) {
                    out.push(c2.text.clone());
                }
            }
        }
    } else if active_col == 2 {
        if let (Some(c1), Some(c2)) = (cur_c1, cur_c2) {
            if !matches!(c1.kind, ItemKind::Env | ItemKind::Var)
                && c2.kind != ItemKind::Loading
                && !c2.text.is_empty()
                && out.len() <= 1
            {
                if out.last().map(|s| s.as_str()) != Some(c2.text.as_str()) {
                    out.push(c2.text.clone());
                }
            }
        }
        let val = if let Some(c3) = cur_c3 {
            if !c3.text.is_empty() {
                Some(c3.text.clone())
            } else if !col3_filter.trim().is_empty() {
                Some(col3_filter.trim().to_string())
            } else {
                None
            }
        } else if !col3_filter.trim().is_empty() {
            Some(col3_filter.trim().to_string())
        } else {
            None
        };

        if let Some(v) = val {
            if out.last().map(|s| s.as_str()) != Some(v.as_str()) {
                out.push(v);
            }
        }
    }

    out.join(" ")
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == "--sync-ps" || a == "--warm") {
        let count = sync_powershell_metadata()?;
        eprintln!("Synced {count} PowerShell commands & parameter definitions.");
        if args.iter().any(|a| a == "--warm") {
            let mut cache = load_cli_disk_cache();
            let top_tools = [
                "git", "prek", "zoxide", "winget", "uv", "uvx", "cargo", "rustc", "adb", "fd",
                "rg", "lsd", "fzf", "starship", "scoop", "tailscale", "caddy",
            ];
            let results: Vec<(String, Vec<MillerItem>)> = top_tools
                .par_iter()
                .map(|&t| {
                    let items = introspect_cli_chain(&[t.to_string()]);
                    (t.to_string(), items)
                })
                .collect();
            for (k, v) in results {
                if !v.is_empty() {
                    cache.insert(k, v);
                }
            }
            save_cli_disk_cache(&cache);
            eprintln!("Pre-warmed CLI cache for {} priority tools.", top_tools.len());
        }
        return Ok(());
    }

    let mut initial_buffer = String::new();
    let mut out_file: Option<PathBuf> = None;
    let mut fallback_w: usize = 120;
    let mut fallback_h: usize = 30;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initial" if i + 1 < args.len() => {
                initial_buffer = args[i + 1].clone();
                i += 2;
            }
            "--out-file" if i + 1 < args.len() => {
                out_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--width" if i + 1 < args.len() => {
                if let Ok(w) = args[i + 1].parse::<usize>() {
                    if w >= 40 {
                        fallback_w = w;
                    }
                }
                i += 2;
            }
            "--height" if i + 1 < args.len() => {
                if let Ok(h) = args[i + 1].parse::<usize>() {
                    if h >= 12 {
                        fallback_h = h;
                    }
                }
                i += 2;
            }
            other => {
                if !initial_buffer.is_empty() {
                    initial_buffer.push(' ');
                }
                initial_buffer.push_str(other);
                i += 1;
            }
        }
    }

    if let Some(ref f) = out_file {
        let _ = fs::remove_file(f);
    }

    ensure_ps_metadata_async();

    let mut col1_items = load_col1_commands();
    let mut cli_cache = load_cli_disk_cache();
    let (req_tx, req_rx) = mpsc::channel::<BgRequest>();
    let (res_tx, res_rx) = mpsc::channel::<BgResponse>();
    spawn_background_worker(req_rx, res_tx);

    let mut active_col: usize = 0;
    let mut filters: [String; 3] = [String::new(), String::new(), String::new()];
    let mut sel_idx: [usize; 3] = [0, 0, 0];
    let mut scroll_idx: [usize; 3] = [0, 0, 0];

    let mut tokens: Vec<String> = initial_buffer
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
            active_col = 1;
        }
    }

    let mut pending_second_token = if tokens.len() >= 2 {
        Some(tokens[1].clone())
    } else {
        None
    };

    let guard = TerminalGuard::enter()?;
    let mut accepted: Option<String> = None;

    let mut last_c1_key = String::new();
    let mut last_c2_key = String::new();
    let mut last_active_param_key = String::new();
    let mut col2_items: Vec<MillerItem> = Vec::new();
    let mut col3_items: Vec<MillerItem> = Vec::new();
    let mut dirty = true;

    loop {
        // 1. Drain any completed background CLI introspection responses
        let mut cache_updated = false;
        while let Ok(resp) = res_rx.try_recv() {
            cli_cache.insert(resp.cache_key, resp.items);
            cache_updated = true;
            dirty = true;
        }
        if cache_updated {
            save_cli_disk_cache(&cli_cache);
            last_c1_key.clear();
            last_c2_key.clear();
            last_active_param_key.clear();
        }

        // 2. Recompute filtered views and draw frame when state changed
        if dirty {
            let (tw, th) = size().unwrap_or((fallback_w as u16, fallback_h as u16));
            let term_w = if tw >= 40 { tw as usize } else { fallback_w }.saturating_sub(1);
            let term_h = if th >= 12 { th as usize } else { fallback_h }.saturating_sub(1);
            let list_rows = term_h.saturating_sub(6).max(4);

            let col1_filtered: Vec<FilteredItem> = filter_items(&col1_items, &filters[0]);
            if sel_idx[0] >= col1_filtered.len() {
                sel_idx[0] = col1_filtered.len().saturating_sub(1);
            }
            let cur_c1 = col1_filtered
                .get(sel_idx[0])
                .map(|f| &col1_items[f.item_idx]);

            let cur_c1_key = cur_c1.map(|c| c.label.clone()).unwrap_or_default();

            // Compute active parameter sets from tokens and cur_c1
            let active_param_sets = compute_active_parameter_sets(cur_c1, &tokens);
            let active_param_key = active_param_sets.as_ref().map(|s| {
                let mut v: Vec<_> = s.iter().cloned().collect();
                v.sort();
                v.join(",")
            }).unwrap_or_default();

            if cur_c1_key != last_c1_key || active_param_key != last_active_param_key {
                col2_items = resolve_col2_items(cur_c1, active_param_sets.as_ref(), &cli_cache, &req_tx);
                sel_idx[1] = 0;
                scroll_idx[1] = 0;
                last_c1_key = cur_c1_key;
                last_active_param_key = active_param_key;
                last_c2_key.clear();
            }

            if let Some(ref tok2) = pending_second_token {
                if !col2_items.is_empty() && col2_items[0].kind != ItemKind::Loading {
                    if let Some(pos) = col2_items
                        .iter()
                        .position(|it| it.text.eq_ignore_ascii_case(tok2))
                    {
                        sel_idx[1] = pos;
                        active_col = 2;
                    }
                    pending_second_token = None;
                }
            }

            let col2_filtered: Vec<FilteredItem> = filter_items(&col2_items, &filters[1]);
            if sel_idx[1] >= col2_filtered.len() {
                sel_idx[1] = col2_filtered.len().saturating_sub(1);
            }
            let cur_c2 = col2_filtered
                .get(sel_idx[1])
                .map(|f| &col2_items[f.item_idx]);

            let cur_c2_key = format!(
                "{}::{}::{}",
                last_c1_key,
                cur_c2.map(|c| c.label.as_str()).unwrap_or(""),
                filters[2]
            );
            if cur_c2_key != last_c2_key {
                col3_items = resolve_col3_items(cur_c1, cur_c2, &cli_cache, &req_tx, &filters[2]);
                sel_idx[2] = 0;
                scroll_idx[2] = 0;
                last_c2_key = cur_c2_key;
            }

            let col3_filtered: Vec<FilteredItem> = filter_items(&col3_items, &filters[2]);
            if sel_idx[2] >= col3_filtered.len() {
                sel_idx[2] = col3_filtered.len().saturating_sub(1);
            }
            let cur_c3 = col3_filtered
                .get(sel_idx[2])
                .map(|f| &col3_items[f.item_idx]);

            for c in 0..3 {
                let max_r = if c == 2 {
                    list_rows.saturating_sub(3).max(1)
                } else {
                    list_rows
                };
                if sel_idx[c] < scroll_idx[c] {
                    scroll_idx[c] = sel_idx[c];
                }
                if sel_idx[c] >= scroll_idx[c] + max_r {
                    scroll_idx[c] = sel_idx[c] + 1 - max_r;
                }
            }

            let cmd_preview = build_preview_command(&tokens, active_col, cur_c1, cur_c2, cur_c3, &filters[2]);

            let active_param_set_label = active_param_sets.as_ref().map(|s| {
                let mut v: Vec<_> = s.iter().cloned().collect();
                v.sort();
                v.join(", ")
            });

            let info_item = if active_col == 2 {
                cur_c3.or(cur_c2).or(cur_c1)
            } else if active_col == 1 {
                cur_c2.or(cur_c1)
            } else {
                cur_c1
            };

            let info_title = info_item
                .map(|it| format!(" ℹ {} {}", it.label, it.kind.badge()))
                .unwrap_or_else(|| " ℹ No selection".to_string());
            let info_desc = info_item
                .map(|it| format!("   {}", it.desc))
                .unwrap_or_default();

            let frame = render_frame(&RenderState {
                term_w,
                term_h,
                active_col,
                filters: &filters,
                sel_idx: &sel_idx,
                scroll_idx: &scroll_idx,
                col1_items: &col1_items,
                col1_filtered: &col1_filtered,
                col2_items: &col2_items,
                col2_filtered: &col2_filtered,
                col3_items: &col3_items,
                col3_filtered: &col3_filtered,
                cmd_preview: &cmd_preview,
                info_title: &info_title,
                info_desc: &info_desc,
                active_param_set: active_param_set_label.as_deref(),
            });

            guard.conout.write_str(&frame);
            dirty = false;
        }

        // 3. Poll for keyboard / resize events
        if !event::poll(Duration::from_millis(25))? {
            continue;
        }

        match event::read()? {
            Event::Resize(w, h) => {
                if w >= 40 {
                    fallback_w = w as usize;
                }
                if h >= 12 {
                    fallback_h = h as usize;
                }
                dirty = true;
            }
            Event::Key(key) => {
                if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                    continue;
                }

                let col1_filtered = filter_items(&col1_items, &filters[0]);
                let col2_filtered = filter_items(&col2_items, &filters[1]);
                let col3_filtered = filter_items(&col3_items, &filters[2]);

                let cur_c1 = col1_filtered
                    .get(sel_idx[0])
                    .map(|f| &col1_items[f.item_idx]);
                let cur_c2 = col2_filtered
                    .get(sel_idx[1])
                    .map(|f| &col2_items[f.item_idx]);
                let cur_c3 = col3_filtered
                    .get(sel_idx[2])
                    .map(|f| &col3_items[f.item_idx]);

                let cur_len = match active_col {
                    0 => col1_filtered.len(),
                    1 => col2_filtered.len(),
                    _ => col3_filtered.len(),
                };

                match (key.code, key.modifiers) {
                    (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        break;
                    }
                    (KeyCode::Enter, _) => {
                        accepted = Some(build_preview_command(
                            &tokens, active_col, cur_c1, cur_c2, cur_c3, &filters[2],
                        ));
                        break;
                    }
                    (KeyCode::Char('r'), KeyModifiers::CONTROL) => {
                        let _ = sync_powershell_metadata();
                        col1_items = load_col1_commands();
                        last_c1_key.clear();
                        last_c2_key.clear();
                        last_active_param_key.clear();
                        dirty = true;
                    }
                    (KeyCode::Up, _) => {
                        if cur_len > 0 {
                            sel_idx[active_col] = if sel_idx[active_col] > 0 {
                                sel_idx[active_col] - 1
                            } else {
                                cur_len - 1
                            };
                            dirty = true;
                        }
                    }
                    (KeyCode::Down, _) => {
                        if cur_len > 0 {
                            sel_idx[active_col] = if sel_idx[active_col] + 1 < cur_len {
                                sel_idx[active_col] + 1
                            } else {
                                0
                            };
                            dirty = true;
                        }
                    }
                    (KeyCode::PageUp, _) => {
                        if cur_len > 0 {
                            sel_idx[active_col] = sel_idx[active_col].saturating_sub(12);
                            dirty = true;
                        }
                    }
                    (KeyCode::PageDown, _) => {
                        if cur_len > 0 {
                            sel_idx[active_col] = (sel_idx[active_col] + 12).min(cur_len - 1);
                            dirty = true;
                        }
                    }
                    (KeyCode::Home, _) => {
                        sel_idx[active_col] = 0;
                        dirty = true;
                    }
                    (KeyCode::End, _) => {
                        if cur_len > 0 {
                            sel_idx[active_col] = cur_len - 1;
                            dirty = true;
                        }
                    }
                    (KeyCode::Right, _) => {
                        if active_col == 0 {
                            if let Some(c1) = cur_c1 {
                                if !matches!(c1.kind, ItemKind::Env | ItemKind::Var) {
                                    if tokens.is_empty() {
                                        tokens.push(c1.text.clone());
                                    } else {
                                        tokens[0] = c1.text.clone();
                                    }
                                }
                                active_col = 1;
                                dirty = true;
                            }
                        } else if active_col == 1 {
                            if let Some(c2) = cur_c2 {
                                if let Some(c1) = cur_c1 {
                                    if !matches!(c1.kind, ItemKind::Env | ItemKind::Var)
                                        && tokens.is_empty()
                                    {
                                        tokens.push(c1.text.clone());
                                    }
                                }
                                if c2.kind != ItemKind::Loading
                                    && !c2.text.is_empty()
                                    && tokens.last().map(|s| s.as_str()) != Some(c2.text.as_str())
                                {
                                    tokens.push(c2.text.clone());
                                }
                                active_col = 2;
                                dirty = true;
                            }
                        }
                    }
                    (KeyCode::Left, _) => {
                        if active_col > 0 {
                            active_col -= 1;
                            dirty = true;
                        }
                    }
                    (KeyCode::Tab, _) => {
                        if active_col == 0 {
                            if let Some(c1) = cur_c1 {
                                if !matches!(c1.kind, ItemKind::Env | ItemKind::Var) {
                                    if tokens.is_empty() {
                                        tokens.push(c1.text.clone());
                                    } else {
                                        tokens[0] = c1.text.clone();
                                    }
                                }
                                filters[0].clear();
                                active_col = 1;
                                dirty = true;
                            }
                        } else if active_col == 1 {
                            if let Some(c2) = cur_c2 {
                                if let Some(c1) = cur_c1 {
                                    if !matches!(c1.kind, ItemKind::Env | ItemKind::Var)
                                        && tokens.is_empty()
                                    {
                                        tokens.push(c1.text.clone());
                                    }
                                }
                                if c2.kind != ItemKind::Loading && !c2.text.is_empty() {
                                    if c2.kind == ItemKind::Switch
                                        || (c2.kind == ItemKind::Flag && c2.children.is_empty())
                                    {
                                        tokens.push(c2.text.clone());
                                        filters[1].clear();
                                        sel_idx[1] = 0;
                                        dirty = true;
                                    } else {
                                        if tokens.last().map(|s| s.as_str()) != Some(c2.text.as_str()) {
                                            tokens.push(c2.text.clone());
                                        }
                                        active_col = 2;
                                        sel_idx[2] = 0;
                                        dirty = true;
                                    }
                                }
                            }
                        } else if active_col == 2 {
                            let val_to_add = if let Some(c3) = cur_c3 {
                                if !c3.text.is_empty() {
                                    Some(c3.text.clone())
                                } else if !filters[2].trim().is_empty() {
                                    Some(filters[2].trim().to_string())
                                } else {
                                    None
                                }
                            } else if !filters[2].trim().is_empty() {
                                Some(filters[2].trim().to_string())
                            } else {
                                None
                            };

                            if let Some(val) = val_to_add {
                                if let Some(c1) = cur_c1 {
                                    if !matches!(c1.kind, ItemKind::Env | ItemKind::Var)
                                        && tokens.is_empty()
                                    {
                                        tokens.push(c1.text.clone());
                                    }
                                }
                                if let Some(c2) = cur_c2 {
                                    if c2.kind != ItemKind::Loading && !c2.text.is_empty() {
                                        if tokens.last().map(|s| s.as_str()) != Some(c2.text.as_str()) {
                                            tokens.push(c2.text.clone());
                                        }
                                    }
                                }
                                tokens.push(val);
                                filters[2].clear();
                                filters[1].clear();
                                active_col = 1;
                                sel_idx[1] = 0;
                                dirty = true;
                            }
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        if !filters[active_col].is_empty() {
                            filters[active_col].pop();
                            sel_idx[active_col] = 0;
                            dirty = true;
                        } else if !tokens.is_empty() {
                            tokens.pop();
                            dirty = true;
                        }
                    }
                    (KeyCode::Char(ch), mods) => {
                        if !mods.contains(KeyModifiers::CONTROL)
                            && !mods.contains(KeyModifiers::ALT)
                        {
                            filters[active_col].push(ch);
                            sel_idx[active_col] = 0;
                            dirty = true;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    drop(guard);

    if let Some(cmd) = accepted {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            if let Some(ref f) = out_file {
                let _ = fs::write(f, trimmed.as_bytes());
            } else {
                println!("{trimmed}");
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parameter_set_exclusivity_filtering() {
        let mut get_process = MillerItem::new("Get-Process", "Get-Process", ItemKind::Cmdlet, "Gets running processes");
        
        let mut id_param = MillerItem::new("-Id", "-Id", ItemKind::Param, "Process IDs");
        id_param.parameter_sets = vec!["Id".to_string(), "IdWithUserName".to_string()];

        let mut name_param = MillerItem::new("-Name", "-Name", ItemKind::Param, "Process Name");
        name_param.parameter_sets = vec!["Name".to_string(), "NameWithUserName".to_string()];

        let mut verbose_param = MillerItem::new("-Verbose", "-Verbose", ItemKind::Switch, "Verbose output");
        verbose_param.parameter_sets = vec!["__AllParameterSets".to_string()];

        get_process.children = vec![id_param, name_param, verbose_param];

        // Case 1: No parameter selected -> all 3 parameters allowed
        let tokens_empty = vec!["Get-Process".to_string()];
        let sets_none = compute_active_parameter_sets(Some(&get_process), &tokens_empty);
        assert_eq!(sets_none, None);

        let (req_tx, _req_rx) = mpsc::channel();
        let cache = HashMap::new();
        let res_all = resolve_col2_items(Some(&get_process), sets_none.as_ref(), &cache, &req_tx);
        assert_eq!(res_all.len(), 3);

        // Case 2: User picked "-Id" -> active sets should be {"Id", "IdWithUserName"}
        let tokens_with_id = vec!["Get-Process".to_string(), "-Id".to_string(), "1234".to_string()];
        let sets_id = compute_active_parameter_sets(Some(&get_process), &tokens_with_id).expect("Should compute active sets");
        assert!(sets_id.contains("Id"));
        assert!(sets_id.contains("IdWithUserName"));
        assert!(!sets_id.contains("Name"));

        // When resolving col2 with sets_id, -Name MUST be pruned/filtered out!
        let res_filtered = resolve_col2_items(Some(&get_process), Some(&sets_id), &cache, &req_tx);
        let param_names: Vec<&str> = res_filtered.iter().map(|p| p.text.as_str()).collect();
        assert!(param_names.contains(&"-Id"));
        assert!(param_names.contains(&"-Verbose"));
        assert!(!param_names.contains(&"-Name"), "-Name is incompatible with ParameterSet Id and must be filtered out");
    }

    #[test]
    fn test_interactive_value_prompt() {
        let (req_tx, _req_rx) = mpsc::channel();
        let cache = HashMap::new();
        let col1 = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
        let col2 = MillerItem::new("clone", "clone", ItemKind::Subcmd, "Clone repository");

        // When user types custom URL in Column 3
        let custom_url = "https://github.com/user/my-repo";
        let col3_items = resolve_col3_items(Some(&col1), Some(&col2), &cache, &req_tx, custom_url);
        
        assert!(!col3_items.is_empty());
        assert_eq!(col3_items[0].text, custom_url);
        assert!(col3_items[0].label.contains(custom_url));
        assert_eq!(col3_items[0].kind, ItemKind::Value);

        // When building preview command, the custom value is interpolated
        let tokens = vec!["git".to_string(), "clone".to_string()];
        let preview = build_preview_command(&tokens, 2, Some(&col1), Some(&col2), Some(&col3_items[0]), custom_url);
        assert_eq!(preview, "git clone https://github.com/user/my-repo");
    }
}
