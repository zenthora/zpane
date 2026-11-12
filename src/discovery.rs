use crate::model::{BgRequest, BgResponse, ItemKind, MillerItem, PsCommandExport};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn cache_dir() -> PathBuf {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("miller-cmd");
    let _ = fs::create_dir_all(&dir);
    dir
}

pub fn ps_metadata_path() -> PathBuf {
    cache_dir().join("ps_metadata.json")
}

pub fn cli_cache_path() -> PathBuf {
    cache_dir().join("cli_cache.json")
}

pub fn load_cli_disk_cache() -> HashMap<String, Vec<MillerItem>> {
    let path = cli_cache_path();
    if let Ok(bytes) = fs::read(&path) {
        if let Ok(map) = serde_json::from_slice::<HashMap<String, Vec<MillerItem>>>(&bytes) {
            return map;
        }
    }
    HashMap::new()
}

pub fn save_cli_disk_cache(cache: &HashMap<String, Vec<MillerItem>>) {
    let path = cli_cache_path();
    if let Ok(json) = serde_json::to_vec(cache) {
        let _ = fs::write(path, json);
    }
}

pub fn common_var_items() -> &'static [MillerItem] {
    static COMMON_VARS: OnceLock<Vec<MillerItem>> = OnceLock::new();
    COMMON_VARS.get_or_init(|| {
        let pwd = env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let home = env::var("USERPROFILE").unwrap_or_default();
        let temp = env::var("TEMP").unwrap_or_default();
        let profile = format!(
            "{}\\Documents\\PowerShell\\Microsoft.PowerShell_profile.ps1",
            home
        );

        vec![
            MillerItem::new("$PWD", "$PWD (Current Dir)", ItemKind::Var, pwd),
            MillerItem::new("$HOME", "$HOME (User Home)", ItemKind::Var, home),
            MillerItem::new("$PROFILE", "$PROFILE (PS Profile)", ItemKind::Var, profile),
            MillerItem::new("$env:TEMP", "$env:TEMP (Temp Dir)", ItemKind::Var, temp),
            MillerItem::new(
                "$_",
                "$_ (Pipeline Item)",
                ItemKind::Var,
                "Current object in pipeline",
            ),
            MillerItem::new("$true", "$true", ItemKind::Var, "Boolean True"),
            MillerItem::new("$false", "$false", ItemKind::Var, "Boolean False"),
            MillerItem::new("$null", "$null", ItemKind::Var, "Null reference"),
        ]
    })
}

fn build_env_scope_item() -> MillerItem {
    let mut vars: Vec<(String, String)> = env::vars().collect();
    vars.sort_by_key(|a| a.0.to_ascii_lowercase());

    let mut children = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        let token = format!("$env:{k}");
        let mut item = MillerItem::new(&token, &k, ItemKind::Env, &v);

        if v.contains(';') {
            for part in v.split(';').filter(|s| !s.is_empty()) {
                item.children.push(MillerItem::new(
                    format!("\"{part}\""),
                    part,
                    ItemKind::Value,
                    format!("Path entry in $env:{k}"),
                ));
            }
        } else {
            item.children.push(MillerItem::new(
                &token,
                format!("Insert {token}"),
                ItemKind::Var,
                format!("Current value: {v}"),
            ));
            if !v.is_empty() {
                item.children.push(MillerItem::new(
                    format!("\"{v}\""),
                    format!("Literal: {v}"),
                    ItemKind::Value,
                    format!("Insert literal value of {token}"),
                ));
            }
        }
        children.push(item);
    }

    let mut root = MillerItem::new(
        "$env:",
        "$env:Variables",
        ItemKind::Env,
        "Browse and insert Windows & Shell Environment Variables ($env:*)",
    );
    root.children = children;
    root
}

fn build_ps_var_scope_item() -> MillerItem {
    let mut root = MillerItem::new(
        "$",
        "$global:Variables",
        ItemKind::Var,
        "Browse and insert PowerShell Session & Automatic Variables ($*)",
    );
    root.children = common_var_items().to_vec();
    for child in &mut root.children {
        let tok = child.text.clone();
        let val = child.desc.clone();
        child.children.push(MillerItem::new(
            &tok,
            format!("Insert {tok}"),
            ItemKind::Var,
            format!("Value: {val}"),
        ));
        if !val.is_empty() && !val.starts_with("Boolean") && !val.starts_with("Current ") {
            child.children.push(MillerItem::new(
                format!("\"{val}\""),
                format!("Literal: {val}"),
                ItemKind::Value,
                format!("Insert literal path/value of {tok}"),
            ));
        }
    }
    root
}

fn convert_ps_export_to_item(exp: PsCommandExport) -> MillerItem {
    let kind = match exp.kind.as_str() {
        "Func" => ItemKind::Func,
        "Alias" => ItemKind::Alias,
        "CLI" => ItemKind::Cli,
        _ => ItemKind::Cmdlet,
    };

    let mut cmd_item = MillerItem::new(&exp.name, &exp.name, kind, &exp.desc);
    let mut primary = Vec::new();
    let mut common = Vec::new();

    for p in exp.params {
        let p_flag = format!("-{}", p.name);
        let label = if p.aliases.is_empty() {
            p_flag.clone()
        } else {
            let al: Vec<String> = p.aliases.iter().map(|a| format!("-{a}")).collect();
            format!("{p_flag} ({})", al.join(", "))
        };
        let p_kind = if p.is_switch {
            ItemKind::Switch
        } else {
            ItemKind::Param
        };
        let desc = format!("[{}] Parameter {}", p.type_name, label);

        let mut param_item = MillerItem::new(&p_flag, label, p_kind, desc);
        param_item.parameter_sets = p.parameter_sets;

        for val in p.valid_values {
            param_item.children.push(MillerItem::new(
                &val,
                &val,
                ItemKind::Enum,
                format!("Allowed [{}] value for {p_flag}", p.type_name),
            ));
        }
        if p.is_switch {
            param_item.children.push(MillerItem::new(
                "",
                "(Switch Flag - No Value Needed)",
                ItemKind::Switch,
                format!("Press Enter or Tab to include {p_flag}"),
            ));
            param_item.children.push(MillerItem::new(
                ":$true",
                ":$true",
                ItemKind::Value,
                "Explicitly enable switch",
            ));
            param_item.children.push(MillerItem::new(
                ":$false",
                ":$false",
                ItemKind::Value,
                "Explicitly disable switch",
            ));
        }

        if p.is_common {
            common.push(param_item);
        } else {
            primary.push(param_item);
        }
    }

    primary.extend(common);
    cmd_item.children = primary;
    cmd_item
}

pub fn load_col1_commands() -> Vec<MillerItem> {
    let mut list = Vec::with_capacity(5000);
    let mut seen = HashSet::new();

    // 1. Variable scopes first
    list.push(build_env_scope_item());
    seen.insert("$env:variables".to_string());
    list.push(build_ps_var_scope_item());
    seen.insert("$global:variables".to_string());

    // 2. Load exported PowerShell metadata if available
    let mut ps_map: HashMap<String, MillerItem> = HashMap::new();
    let mut ps_order: Vec<String> = Vec::new();
    if let Ok(bytes) = fs::read(ps_metadata_path()) {
        if let Ok(exports) = serde_json::from_slice::<Vec<PsCommandExport>>(&bytes) {
            for exp in exports {
                let key = exp.name.to_ascii_lowercase();
                if let std::collections::hash_map::Entry::Vacant(e) = ps_map.entry(key.clone()) {
                    ps_order.push(key);
                    e.insert(convert_ps_export_to_item(exp));
                }
            }
        }
    }

    // 3. Scan PATH directories for CLI executables (.exe, .cmd, .bat)
    let mut cli_map: HashMap<String, MillerItem> = HashMap::new();
    let mut cli_order: Vec<String> = Vec::new();
    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if ext.eq_ignore_ascii_case("exe")
                            || ext.eq_ignore_ascii_case("cmd")
                            || ext.eq_ignore_ascii_case("bat")
                        {
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                let key = stem.to_ascii_lowercase();
                                if let std::collections::hash_map::Entry::Vacant(e) =
                                    cli_map.entry(key.clone())
                                {
                                    cli_order.push(key);
                                    e.insert(MillerItem::new(
                                        stem,
                                        stem,
                                        ItemKind::Cli,
                                        path.display().to_string(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // 4. Priority Favorites at the top
    let priority = [
        "git",
        "prek",
        "zoxide",
        "winget",
        "uv",
        "uvx",
        "cargo",
        "rustc",
        "adb",
        "fd",
        "rg",
        "lsd",
        "fzf",
        "starship",
        "scoop",
        "tailscale",
        "caddy",
        "python",
        "pip",
        "node",
        "npm",
        "pwsh",
        "docker",
        "z",
        "zi",
        "mkcd",
        "ports",
        "myip",
        "fkill",
        "fgs",
        "fscoop",
        "extract",
        "touch",
        "which",
        "reload",
        "ep",
        "pkr",
        "pki",
        "pku",
    ];

    for &p in &priority {
        let key = p.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        if let Some(item) = cli_map.remove(&key) {
            seen.insert(key);
            list.push(item);
        } else if let Some(item) = ps_map.remove(&key) {
            seen.insert(key);
            list.push(item);
        }
    }

    // 5. Profile Functions & Aliases from PS metadata
    for key in &ps_order {
        if seen.contains(key) {
            continue;
        }
        if let Some(item) = ps_map.get(key) {
            if matches!(item.kind, ItemKind::Func | ItemKind::Alias) {
                if let Some(owned) = ps_map.remove(key) {
                    seen.insert(key.clone());
                    list.push(owned);
                }
            }
        }
    }

    // 6. All remaining CLI executables
    for key in &cli_order {
        if seen.contains(key) {
            continue;
        }
        if let Some(owned) = cli_map.remove(key) {
            seen.insert(key.clone());
            list.push(owned);
        }
    }

    // 7. All PowerShell Cmdlets
    for key in &ps_order {
        if seen.contains(key) {
            continue;
        }
        if let Some(owned) = ps_map.remove(key) {
            seen.insert(key.clone());
            list.push(owned);
        }
    }

    list
}

fn run_command_with_timeout(exe: &str, args: &[&str], timeout_ms: u64) -> String {
    let mut cmd = Command::new(exe);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return String::new(),
    };

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let out_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stdout_pipe.take() {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let err_handle = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = stderr_pipe.take() {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    });

    let start = Instant::now();
    let limit = Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() >= limit {
                    let _ = child.kill();
                    break;
                }
                thread::sleep(Duration::from_millis(8));
            }
            Err(_) => break,
        }
    }

    let out_str = out_handle.join().unwrap_or_default();
    let err_str = err_handle.join().unwrap_or_default();
    format!("{out_str}\n{err_str}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HelpSection {
    None,
    Commands,
    Flags,
    Ignore,
}

fn classify_section_header(line: &str) -> Option<HelpSection> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();

    // Sections that should definitely be ignored
    if lower.starts_with("usage:")
        || lower.starts_with("usage")
        || lower.starts_with("examples:")
        || lower.starts_with("examples")
        || lower.starts_with("learn more")
        || lower.starts_with("see '")
        || lower.starts_with("help topics")
        || lower.starts_with("environment:")
        || lower.starts_with("topics")
    {
        return Some(HelpSection::Ignore);
    }

    // Git prose section headers
    if lower.starts_with("start a working area")
        || lower.starts_with("work on the current change")
        || lower.starts_with("examine the history")
        || lower.starts_with("grow, mark and tweak")
        || lower.starts_with("collaborate")
        || lower.starts_with("main porcelain commands")
        || lower.starts_with("ancillary commands")
        || lower.starts_with("the following commands are available")
    {
        return Some(HelpSection::Commands);
    }

    if lower.starts_with("the following options are available") {
        return Some(HelpSection::Flags);
    }

    // Standard headers ending with ':' or ALL-CAPS section headers (e.g. Cobra "CORE COMMANDS")
    let is_all_caps = trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| !c.is_alphabetic() || c.is_uppercase());
    let ends_with_colon = trimmed.ends_with(':');

    if ends_with_colon || is_all_caps {
        if lower.contains("command") || lower.contains("subcommand") {
            return Some(HelpSection::Commands);
        }
        if lower.contains("flag")
            || lower.contains("option")
            || lower.contains("argument")
            || lower.contains("selection")
        {
            return Some(HelpSection::Flags);
        }
    }

    None
}

pub fn parse_cli_help(help_text: &str) -> Vec<MillerItem> {
    static RE_SECTION_SUBCMD: OnceLock<Regex> = OnceLock::new();
    static RE_SECTION_FLAG: OnceLock<Regex> = OnceLock::new();
    static RE_POSSIBLE_VALS: OnceLock<Regex> = OnceLock::new();
    static RE_DEFAULT_VAL: OnceLock<Regex> = OnceLock::new();

    // Fallback regexes
    static RE_LONG_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SHORT_FLAG_FALLBACK: OnceLock<Regex> = OnceLock::new();
    static RE_SUBCMD_FALLBACK: OnceLock<Regex> = OnceLock::new();

    let re_sec_sub = RE_SECTION_SUBCMD.get_or_init(|| {
        // Matches: "  auth:          Authenticate" OR "  build, b    Compile" OR "  clone      Clone repo"
        Regex::new(r"^\s{2,8}([a-zA-Z0-9_-]+)(?::|,\s*([a-zA-Z0-9_-]+))?(?:\s{2,}|\t|\s*:\s+)(.+)$")
            .unwrap()
    });

    let re_sec_flag = RE_SECTION_FLAG.get_or_init(|| {
        // Matches: "  -v, --version", "  -v,--version", "  -?,--help", "  -h | --help", "      --color <WHEN>", "  -C <DIR>", "  -R, --repo [HOST/]OWNER/REPO"
        Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?]),?\s*(?:\||,)?\s*)?(--(?:\[no-\])?[a-zA-Z0-9_\.-]+|-[a-zA-Z0-9\?])(?:(?:\s+|=)([^\s].*?))?(?:\s{2,}|\t)(.+)$")
            .unwrap()
    });

    let re_vals = RE_POSSIBLE_VALS.get_or_init(|| {
        Regex::new(r"(?:\[possible values:\s*([^\]]+)\]|Allowed values:\s*([^.\n]+))").unwrap()
    });

    let re_def = RE_DEFAULT_VAL.get_or_init(|| Regex::new(r"\[default:\s*([^\]]+)\]").unwrap());

    let mut subcmds = Vec::new();
    let mut flags = Vec::new();
    let mut seen = HashSet::new();

    let mut current_sec = HelpSection::None;
    let mut detected_sections = false;

    for line in help_text.lines() {
        if let Some(sec) = classify_section_header(line) {
            current_sec = sec;
            detected_sections = true;
            continue;
        }

        match current_sec {
            HelpSection::Commands => {
                if let Some(caps) = re_sec_sub.captures(line) {
                    let cmd = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let alias = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");

                    if !cmd.is_empty() && seen.insert(cmd.to_string()) {
                        let label = if alias.is_empty() {
                            cmd.to_string()
                        } else {
                            format!("{cmd} ({alias})")
                        };
                        subcmds.push(MillerItem::new(cmd, label, ItemKind::Subcmd, desc));
                    }
                }
            }
            HelpSection::Flags => {
                if let Some(caps) = re_sec_flag.captures(line) {
                    let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                    let raw_flag = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                    let flag = raw_flag.replacen("--[no-]", "--", 1);
                    let arg_hint = caps.get(3).map(|m| m.as_str().trim()).unwrap_or("");
                    let desc = caps.get(4).map(|m| m.as_str().trim()).unwrap_or("");

                    if seen.insert(flag.clone()) {
                        let is_param = !arg_hint.is_empty();
                        let kind = if is_param {
                            ItemKind::Param
                        } else {
                            ItemKind::Flag
                        };

                        let label = if !short.is_empty() && short != flag {
                            if !arg_hint.is_empty() {
                                format!("{flag} {arg_hint} ({short})")
                            } else {
                                format!("{flag} ({short})")
                            }
                        } else if !arg_hint.is_empty() {
                            format!("{flag} {arg_hint}")
                        } else {
                            flag.clone()
                        };

                        let mut item = MillerItem::new(&flag, label, kind, desc);

                        // Extract enum values if present in description
                        if let Some(v_caps) = re_vals.captures(desc) {
                            let raw_vals = v_caps
                                .get(1)
                                .or_else(|| v_caps.get(2))
                                .map(|m| m.as_str())
                                .unwrap_or("");
                            for v in raw_vals
                                .split(',')
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty())
                            {
                                item.children.push(MillerItem::new(
                                    v,
                                    v,
                                    ItemKind::Enum,
                                    format!("Allowed value for {flag}"),
                                ));
                            }
                        }

                        // Extract default value if present
                        if let Some(d_caps) = re_def.captures(desc) {
                            let def_val = d_caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                            if !def_val.is_empty()
                                && !item.children.iter().any(|c| c.text == def_val)
                            {
                                item.children.push(MillerItem::new(
                                    def_val,
                                    format!("{def_val} [default]"),
                                    ItemKind::Value,
                                    format!("Default value for {flag}"),
                                ));
                            }
                        }

                        flags.push(item);
                    }
                }
            }
            HelpSection::Ignore | HelpSection::None => {}
        }
    }

    // Fallback if no sections were detected or no items parsed
    if !detected_sections || (subcmds.is_empty() && flags.is_empty()) {
        let re_long = RE_LONG_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(?:(-[a-zA-Z0-9\?]),?\s+(?:\|\s+)?)?(--(?:\[no-\])?[a-zA-Z0-9_-]+)(?:[=\s\[][^\s\]]*\]?)?(?:\s{2,}(.+))?$").unwrap()
        });
        let re_short = RE_SHORT_FLAG_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{1,10}(-[a-zA-Z0-9\?])(?:\s+<[^>]+>|\s+\[[^\]]+\])?\s{2,}(.+)$")
                .unwrap()
        });
        let re_sub = RE_SUBCMD_FALLBACK.get_or_init(|| {
            Regex::new(r"^\s{2,8}([a-z][a-z0-9_-]{1,24})(?:,\s*[a-z0-9_-]+)?\s{2,}([A-Za-z].+)$")
                .unwrap()
        });

        let ignored_words: HashSet<&'static str> = [
            "usage",
            "options",
            "commands",
            "arguments",
            "flags",
            "examples",
            "env",
            "note",
            "default",
            "or",
            "see",
            "start",
            "work",
            "examine",
            "grow",
            "collaborate",
        ]
        .into_iter()
        .collect();

        for line in help_text.lines() {
            if let Some(caps) = re_long.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let raw_long = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                let long = raw_long.replacen("--[no-]", "--", 1);
                let desc = caps
                    .get(3)
                    .map(|m| m.as_str().trim().to_string())
                    .unwrap_or_else(|| format!("CLI Flag {long}"));

                if seen.insert(long.clone()) {
                    let label = if short.is_empty() {
                        long.clone()
                    } else {
                        format!("{long} ({short})")
                    };
                    flags.push(MillerItem::new(long, label, ItemKind::Flag, desc));
                }
            } else if let Some(caps) = re_short.captures(line) {
                let short = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                if seen.insert(short.to_string()) {
                    flags.push(MillerItem::new(short, short, ItemKind::Flag, desc));
                }
            } else if let Some(caps) = re_sub.captures(line) {
                let sub = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
                let desc = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");
                if !ignored_words.contains(sub) && seen.insert(sub.to_string()) {
                    subcmds.push(MillerItem::new(sub, sub, ItemKind::Subcmd, desc));
                }
            }
        }
    }

    subcmds.extend(flags);
    subcmds
}

pub fn introspect_cli_chain(chain: &[String]) -> Vec<MillerItem> {
    if chain.is_empty() {
        return Vec::new();
    }
    let exe = &chain[0];
    let sub_args: Vec<&str> = chain[1..].iter().map(|s| s.as_str()).collect();

    // Note: For git, always prefer `-h` so git never spawns a web browser on Windows!
    let primary_flag = if exe.eq_ignore_ascii_case("git") {
        "-h"
    } else {
        "--help"
    };

    let mut args1 = sub_args.clone();
    args1.push(primary_flag);
    let mut text = run_command_with_timeout(exe, &args1, 450);

    if text.trim().len() < 25 && primary_flag != "-h" {
        let mut args2 = sub_args.clone();
        args2.push("-h");
        text = run_command_with_timeout(exe, &args2, 450);
    }

    let mut items = parse_cli_help(&text);

    // If this is a subcommand or leaf level, also append helpful shell variables
    if chain.len() >= 2 {
        let mut existing: HashSet<String> = items.iter().map(|i| i.text.clone()).collect();
        for cv in common_var_items() {
            if existing.insert(cv.text.clone()) {
                items.push(cv.clone());
            }
        }
    } else if items.is_empty() {
        items.push(MillerItem::new(
            "--help",
            "--help (-h)",
            ItemKind::Flag,
            format!("Display help for {exe}"),
        ));
        items.extend_from_slice(common_var_items());
    }

    items
}

pub fn spawn_background_worker(rx: Receiver<BgRequest>, tx: Sender<BgResponse>) {
    thread::spawn(move || {
        let mut handled = HashSet::new();
        let mut stack: Vec<BgRequest> = Vec::new();
        loop {
            if stack.is_empty() {
                match rx.recv() {
                    Ok(req) => stack.push(req),
                    Err(_) => break,
                }
            }
            // Drain any additional queued requests so we always process the latest (LIFO) first
            while let Ok(newer) = rx.try_recv() {
                stack.push(newer);
            }
            if let Some(req) = stack.pop() {
                if !handled.insert(req.cache_key.clone()) {
                    continue;
                }
                let items = introspect_cli_chain(&req.cmd_chain);
                let _ = tx.send(BgResponse {
                    cache_key: req.cache_key,
                    items,
                });
            }
        }
    });
}

/// Synchronizes PowerShell Cmdlets, Functions, Aliases, and Parameter Metadata to disk.
pub fn sync_powershell_metadata() -> anyhow::Result<usize> {
    let ps_script = r#"
$ErrorActionPreference = 'SilentlyContinue'
$commonSet = [System.Collections.Generic.HashSet[string]]::new(
    [string[]]@('Verbose','Debug','ErrorAction','WarningAction','InformationAction','ProgressAction','ErrorVariable','WarningVariable','InformationVariable','OutVariable','OutBuffer','PipelineVariable','WhatIf','Confirm'),
    [System.StringComparer]::OrdinalIgnoreCase
)
$cmds = Get-Command -CommandType Function, Alias, Cmdlet
$out = [System.Collections.Generic.List[object]]::new($cmds.Count)
foreach ($c in $cmds) {
    if ($c.Name -match '^(__|TabExpansion|Prompt|Clear-Host|more|oss|help)$') { continue }
    $kind = switch ($c.CommandType) {
        'Function' { 'Func' }
        'Alias'    { 'Alias' }
        default    { 'Cmdlet' }
    }
    $target = if ($c.CommandType -eq 'Alias' -and $c.ResolvedCommand) { $c.ResolvedCommand } else { $c }
    $desc = if ($c.CommandType -eq 'Alias') { "Alias -> $($target.Name)" } elseif ($c.Source) { "Module: $($c.Source)" } else { "PowerShell $kind" }
    $pList = [System.Collections.Generic.List[object]]::new()
    if ($target.Parameters) {
        foreach ($p in $target.Parameters.Values) {
            $vals = [System.Collections.Generic.List[string]]::new()
            if ($p.ParameterType.IsEnum) {
                foreach ($n in [System.Enum]::GetNames($p.ParameterType)) { $vals.Add($n) }
            }
            foreach ($attr in $p.Attributes) {
                if ($attr -is [System.Management.Automation.ValidateSetAttribute]) {
                    foreach ($v in $attr.ValidValues) { if (-not $vals.Contains($v)) { $vals.Add($v) } }
                }
            }
            $paramSets = [string[]]@($p.ParameterSets.Keys)
            $pList.Add(@{
                name           = $p.Name
                aliases        = @($p.Aliases)
                type_name      = ($p.ParameterType.Name -replace '`1$', '')
                is_switch      = [bool]$p.SwitchParameter
                is_common      = [bool]$commonSet.Contains($p.Name)
                valid_values   = $vals
                parameter_sets = $paramSets
            })
        }
    }
    $out.Add(@{
        name   = $c.Name
        kind   = $kind
        desc   = $desc
        params = $pList
    })
}
$out | ConvertTo-Json -Depth 4 -Compress
"#;

    let output = Command::new("pwsh")
        .args(["-NoLogo", "-Command", ps_script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .output()?;

    let exports: Vec<PsCommandExport> = serde_json::from_slice(&output.stdout)?;
    let count = exports.len();
    let dest = ps_metadata_path();
    fs::write(dest, &output.stdout)?;
    Ok(count)
}

pub fn ensure_ps_metadata_async() {
    let path: &Path = &ps_metadata_path();
    if !path.exists() {
        thread::spawn(|| {
            let _ = sync_powershell_metadata();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_cobra_gh_help() {
        let help = r#"
USAGE
  gh <command> <subcommand> [flags]

CORE COMMANDS
  auth:          Authenticate gh and git with GitHub
  issue:         Manage issues
  pr:            Manage pull requests
  repo:          Manage repositories

FLAGS
  -R, --repo [HOST/]OWNER/REPO   Select another repository using the [HOST/]OWNER/REPO format
      --help                     Show help for command
      --version                  Show gh version
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(
            subcmds.contains(&"auth"),
            "Cobra auth command should be parsed"
        );
        assert!(
            subcmds.contains(&"issue"),
            "Cobra issue command should be parsed"
        );
        assert!(subcmds.contains(&"pr"), "Cobra pr command should be parsed");
        assert!(
            subcmds.contains(&"repo"),
            "Cobra repo command should be parsed"
        );

        let flags: Vec<&str> = items
            .iter()
            .filter(|i| matches!(i.kind, ItemKind::Flag | ItemKind::Param))
            .map(|i| i.text.as_str())
            .collect();
        assert!(
            flags.contains(&"--repo"),
            "Cobra --repo flag should be parsed"
        );
        assert!(
            flags.contains(&"--help"),
            "Cobra --help flag should be parsed"
        );
        assert!(
            flags.contains(&"--version"),
            "Cobra --version flag should be parsed"
        );
    }

    #[test]
    fn test_parse_clap_prek_help() {
        let help = r#"
Usage: prek.exe [OPTIONS] [COMMAND]

Commands:
  init               Create a prek configuration and install Git hook shims
  install            Install prek Git hook shims
  run                Run configured hooks

Hook selection:
  --stage <STAGE>          The stage during which the hook is fired [possible values: manual, commit-msg, pre-commit, pre-push]
  -a, --all-files            Run hooks on all tracked files in the repository
  --color <COLOR>        Whether to use color in output [default: auto] [possible values: auto, always, never]
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"init"));
        assert!(subcmds.contains(&"install"));
        assert!(subcmds.contains(&"run"));

        let stage_flag = items
            .iter()
            .find(|i| i.text == "--stage")
            .expect("stage flag");
        assert_eq!(stage_flag.kind, ItemKind::Param);
        let enum_vals: Vec<&str> = stage_flag
            .children
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(
            enum_vals.contains(&"manual"),
            "Enum values extracted from [possible values]"
        );
        assert!(enum_vals.contains(&"pre-commit"));

        let color_flag = items
            .iter()
            .find(|i| i.text == "--color")
            .expect("color flag");
        let color_enums: Vec<&str> = color_flag
            .children
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert!(color_enums.contains(&"auto"));
        assert!(color_enums.contains(&"always"));
    }

    #[test]
    fn test_parse_winget_help() {
        let help = r#"
usage: winget [<command>] [<options>]

The following commands are available:
  install    Installs the given package
  show       Shows information about a package
  source     Manage sources of packages

The following options are available:
  -v,--version                Display the version of the tool
  --info                      Display general info of the tool
  -?,--help                   Shows help about the selected command
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"install"));
        assert!(subcmds.contains(&"show"));
        assert!(subcmds.contains(&"source"));

        let flags: Vec<&str> = items
            .iter()
            .filter(|i| matches!(i.kind, ItemKind::Flag | ItemKind::Param))
            .map(|i| i.text.as_str())
            .collect();
        assert!(flags.contains(&"--version"));
        assert!(flags.contains(&"--info"));
        assert!(flags.contains(&"--help"));
    }

    #[test]
    fn test_parse_git_help() {
        let help = r#"
start a working area (see also: git help tutorial)
   clone      Clone a repository into a new directory
   init       Create an empty Git repository or reinitialize an existing one

work on the current change (see also: git help everyday)
   add        Add file contents to the index
   mv         Move or rename a file, a directory, or a symlink
"#;
        let items = parse_cli_help(help);
        let subcmds: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == ItemKind::Subcmd)
            .map(|i| i.text.as_str())
            .collect();
        assert!(subcmds.contains(&"clone"));
        assert!(subcmds.contains(&"init"));
        assert!(subcmds.contains(&"add"));
        assert!(subcmds.contains(&"mv"));
    }
}
