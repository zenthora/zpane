use crate::model::{BgRequest, BgResponse, ItemKind, MillerItem};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PsParamExport {
    name: String,
    #[serde(default)]
    aliases: Vec<String>,
    type_name: String,
    is_switch: bool,
    is_common: bool,
    #[serde(default)]
    valid_values: Vec<String>,
    #[serde(default)]
    parameter_sets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PsCommandExport {
    name: String,
    kind: String,
    desc: String,
    #[serde(default)]
    params: Vec<PsParamExport>,
}

/// Which shell syntax zpane emits for variable tokens and which command sources it indexes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellDialect {
    /// PowerShell (Windows, or `pwsh` on Linux): `$env:X`, cmdlets, parameter sets.
    PowerShell,
    /// POSIX shells (bash / zsh / sh / Termux): `$X`, `$?`, `$!`, PATH executables.
    Posix,
    /// Fish shell (`fish`): `$X`, `$status`, `$last_pid`, `$fish_pid`, PATH executables.
    Fish,
}

impl ShellDialect {
    /// Detects the active shell dialect from environment variables without process-global state:
    /// `ZPANE_SHELL=pwsh|posix|fish` overrides; otherwise Windows is always PowerShell,
    /// `PSModulePath` selects PowerShell on Unix, `FISH_VERSION` or `SHELL=*/fish` selects Fish,
    /// and all other Unix environments default to Posix.
    pub fn detect() -> Self {
        let zpane_shell = env::var("ZPANE_SHELL").ok();
        let ps_module_path = env::var_os("PSModulePath");
        let fish_version = env::var_os("FISH_VERSION");
        let shell = env::var("SHELL").ok();
        Self::from_env(
            zpane_shell.as_deref(),
            ps_module_path.is_some(),
            fish_version.is_some(),
            shell.as_deref(),
            cfg!(windows),
        )
    }

    pub fn from_env(
        zpane_shell: Option<&str>,
        has_ps_module_path: bool,
        has_fish_version: bool,
        shell: Option<&str>,
        is_windows: bool,
    ) -> Self {
        if let Some(zs) = zpane_shell {
            match zs.to_ascii_lowercase().as_str() {
                "pwsh" | "powershell" => return ShellDialect::PowerShell,
                "fish" => return ShellDialect::Fish,
                "posix" | "bash" | "zsh" | "sh" => return ShellDialect::Posix,
                _ => {}
            }
        }
        if is_windows || has_ps_module_path {
            ShellDialect::PowerShell
        } else if has_fish_version || shell.map(|s| s.ends_with("/fish")).unwrap_or(false) {
            ShellDialect::Fish
        } else {
            ShellDialect::Posix
        }
    }

    /// Formats an environment variable reference in this dialect's syntax.
    pub fn env_token(self, name: &str) -> String {
        match self {
            ShellDialect::PowerShell => format!("$env:{name}"),
            ShellDialect::Posix | ShellDialect::Fish => format!("${name}"),
        }
    }

    /// Returns the path-list separator for this dialect.
    pub fn path_sep(self) -> char {
        match self {
            ShellDialect::PowerShell => ';',
            ShellDialect::Posix | ShellDialect::Fish => ':',
        }
    }

    /// Whether an environment variable holds a list of paths that should be split into entries.
    pub fn is_path_list(self, name: &str, value: &str) -> bool {
        match self {
            ShellDialect::PowerShell => value.contains(';'),
            ShellDialect::Posix | ShellDialect::Fish => {
                let upper = name.to_ascii_uppercase();
                value.contains(':') && (upper.contains("PATH") || upper.ends_with("DIRS"))
            }
        }
    }

    /// Returns common built-in and special variable items for this shell dialect.
    pub fn common_var_items(self) -> Vec<MillerItem> {
        let pwd = env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default();

        match self {
            ShellDialect::PowerShell => {
                let home = env::var("USERPROFILE")
                    .or_else(|_| env::var("HOME"))
                    .unwrap_or_default();
                let temp = env::temp_dir().display().to_string();
                let profile = if cfg!(windows) {
                    format!("{home}\\Documents\\PowerShell\\Microsoft.PowerShell_profile.ps1")
                } else {
                    format!("{home}/.config/powershell/Microsoft.PowerShell_profile.ps1")
                };
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
            }
            ShellDialect::Posix => {
                let var = |k: &str| env::var(k).unwrap_or_default();
                let tmp = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
                vec![
                    MillerItem::new("$PWD", "$PWD (Current Dir)", ItemKind::Var, pwd),
                    MillerItem::new("$HOME", "$HOME (User Home)", ItemKind::Var, var("HOME")),
                    MillerItem::new(
                        "$OLDPWD",
                        "$OLDPWD (Previous Dir)",
                        ItemKind::Var,
                        var("OLDPWD"),
                    ),
                    MillerItem::new("$TMPDIR", "$TMPDIR (Temp Dir)", ItemKind::Var, tmp),
                    MillerItem::new("$USER", "$USER (User Name)", ItemKind::Var, var("USER")),
                    MillerItem::new(
                        "$SHELL",
                        "$SHELL (Login Shell)",
                        ItemKind::Var,
                        var("SHELL"),
                    ),
                    MillerItem::new(
                        "$?",
                        "$? (Last Exit Code)",
                        ItemKind::Var,
                        "Current exit status",
                    ),
                    MillerItem::new(
                        "$!",
                        "$! (Last Background PID)",
                        ItemKind::Var,
                        "Current last background job PID",
                    ),
                ]
            }
            ShellDialect::Fish => {
                let var = |k: &str| env::var(k).unwrap_or_default();
                let tmp = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
                vec![
                    MillerItem::new("$PWD", "$PWD (Current Dir)", ItemKind::Var, pwd),
                    MillerItem::new("$HOME", "$HOME (User Home)", ItemKind::Var, var("HOME")),
                    MillerItem::new(
                        "$status",
                        "$status (Last Exit Status)",
                        ItemKind::Var,
                        "Current exit status of last command",
                    ),
                    MillerItem::new(
                        "$last_pid",
                        "$last_pid (Last Background PID)",
                        ItemKind::Var,
                        "Current process ID of last background job",
                    ),
                    MillerItem::new(
                        "$fish_pid",
                        "$fish_pid (Current Fish PID)",
                        ItemKind::Var,
                        "Current process ID of this Fish session",
                    ),
                    MillerItem::new("$TMPDIR", "$TMPDIR (Temp Dir)", ItemKind::Var, tmp),
                    MillerItem::new("$USER", "$USER (User Name)", ItemKind::Var, var("USER")),
                    MillerItem::new(
                        "$SHELL",
                        "$SHELL (Login Shell)",
                        ItemKind::Var,
                        var("SHELL"),
                    ),
                ]
            }
        }
    }
}

/// Formats an environment variable reference in the given shell dialect's syntax.
pub fn env_token(name: &str, dialect: ShellDialect) -> String {
    dialect.env_token(name)
}

/// Prevents a console window from flashing up for child processes on Windows; no-op elsewhere.
fn hide_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Per-user cache directory: `%LOCALAPPDATA%\zpane` on Windows,
/// `$XDG_CACHE_HOME/zpane` or `~/.cache/zpane` on Linux / Termux.
pub fn cache_dir() -> PathBuf {
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }
    .unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("zpane");
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

pub fn common_var_items(dialect: ShellDialect) -> Vec<MillerItem> {
    dialect.common_var_items()
}

/// Whether an environment variable holds a list of paths that should be split into entries.
pub fn is_path_list(name: &str, value: &str, dialect: ShellDialect) -> bool {
    dialect.is_path_list(name, value)
}

pub fn build_env_scope_item_from_vars<I>(dialect: ShellDialect, vars_iter: I) -> MillerItem
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut vars: Vec<(String, String)> = vars_iter.into_iter().collect();
    vars.sort_by_key(|a| a.0.to_ascii_lowercase());
    let list_sep = dialect.path_sep();

    let mut children = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        let token = env_token(&k, dialect);
        let mut item = MillerItem::new(&token, &k, ItemKind::Env, &v);

        if is_path_list(&k, &v, dialect) {
            for part in v.split(list_sep).filter(|s| !s.is_empty()) {
                item.children.push(MillerItem::new(
                    format!("\"{part}\""),
                    part,
                    ItemKind::Value,
                    format!("Path entry in {token}"),
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

    let (text, label) = match dialect {
        ShellDialect::PowerShell => ("$env:", "$env:Variables"),
        ShellDialect::Posix | ShellDialect::Fish => ("$", "$ENV Variables"),
    };
    let mut root = MillerItem::new(
        text,
        label,
        ItemKind::Env,
        "Browse and insert environment variables",
    );
    root.children = children;
    root
}

pub fn build_env_scope_item(dialect: ShellDialect) -> MillerItem {
    build_env_scope_item_from_vars(dialect, env::vars())
}

pub fn build_ps_var_scope_item(dialect: ShellDialect) -> MillerItem {
    let (label, desc) = match dialect {
        ShellDialect::PowerShell => (
            "$global:Variables",
            "Browse and insert PowerShell Session & Automatic Variables ($*)",
        ),
        ShellDialect::Posix => (
            "$shell Variables",
            "Browse and insert common shell & special parameters ($PWD, $?, ...)",
        ),
        ShellDialect::Fish => (
            "$fish Variables",
            "Browse and insert Fish shell & special variables ($PWD, $status, $last_pid, ...)",
        ),
    };
    let mut root = MillerItem::new("$", label, ItemKind::Var, desc);
    root.children = common_var_items(dialect);
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
        param_item.aliases = p.aliases;

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

/// Returns the command name for a PATH entry if it is a launchable executable on this platform.
fn executable_name<'a>(entry: &fs::DirEntry, path: &'a Path) -> Option<&'a str> {
    #[cfg(windows)]
    {
        let _ = entry;
        let ext = path.extension()?.to_str()?;
        if ext.eq_ignore_ascii_case("exe")
            || ext.eq_ignore_ascii_case("cmd")
            || ext.eq_ignore_ascii_case("bat")
        {
            return path.file_stem()?.to_str();
        }
        None
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // `metadata()` follows symlinks (most of /usr/bin on Termux & Debian are links).
        let meta = fs::metadata(entry.path()).ok()?;
        if meta.is_file() && meta.permissions().mode() & 0o111 != 0 {
            return path.file_name()?.to_str();
        }
        None
    }
}

pub fn load_col1_commands(dialect: ShellDialect) -> Vec<MillerItem> {
    let mut list = Vec::with_capacity(5000);
    let mut seen = HashSet::new();

    // 1. Variable scopes first
    list.push(build_env_scope_item(dialect));
    seen.insert("$env:variables".to_string());
    list.push(build_ps_var_scope_item(dialect));
    seen.insert("$global:variables".to_string());

    // 2. Load exported PowerShell metadata if available (PowerShell dialect only)
    let mut ps_map: HashMap<String, MillerItem> = HashMap::new();
    let mut ps_order: Vec<String> = Vec::new();
    let ps_bytes = if dialect == ShellDialect::PowerShell {
        fs::read(ps_metadata_path()).ok()
    } else {
        None
    };
    if let Some(bytes) = ps_bytes {
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

    // 3. Scan PATH directories for CLI executables
    //    (Windows: .exe / .cmd / .bat; Unix: any file with an execute bit)
    let mut cli_map: HashMap<String, MillerItem> = HashMap::new();
    let mut cli_order: Vec<String> = Vec::new();
    if let Some(path_os) = env::var_os("PATH") {
        for dir in env::split_paths(&path_os) {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if let Some(name) = executable_name(&entry, &path) {
                        let key = name.to_ascii_lowercase();
                        if let std::collections::hash_map::Entry::Vacant(e) =
                            cli_map.entry(key.clone())
                        {
                            cli_order.push(key);
                            e.insert(MillerItem::new(
                                name,
                                name,
                                ItemKind::Cli,
                                path.display().to_string(),
                            ));
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
        "dnf",
        "rpm",
        "apt",
        "pacman",
        "pkg",
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
        .stderr(Stdio::piped());
    hide_window(&mut cmd);

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

pub fn introspect_cli_chain(chain: &[String], dialect: ShellDialect) -> Vec<MillerItem> {
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
    let vars = common_var_items(dialect);

    // If this is a subcommand or leaf level, also append helpful shell variables
    if chain.len() >= 2 {
        let mut existing: HashSet<String> = items.iter().map(|i| i.text.clone()).collect();
        for cv in vars {
            if existing.insert(cv.text.clone()) {
                items.push(cv);
            }
        }
    } else if items.is_empty() {
        items.push(MillerItem::new(
            "--help",
            "--help (-h)",
            ItemKind::Flag,
            format!("Display help for {exe}"),
        ));
        items.extend(vars);
    }

    items
}

pub fn spawn_background_worker(
    rx: Receiver<BgRequest>,
    tx: Sender<BgResponse>,
    dialect: ShellDialect,
) {
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
                let items = introspect_cli_chain(&req.cmd_chain, dialect);
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

    let mut cmd = Command::new("pwsh");
    cmd.args(["-NoLogo", "-NoProfile", "-Command", ps_script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let output = hide_window(&mut cmd).output()?;

    let exports: Vec<PsCommandExport> = serde_json::from_slice(&output.stdout)?;
    let count = exports.len();
    let dest = ps_metadata_path();
    fs::write(dest, &output.stdout)?;
    Ok(count)
}

pub fn ensure_ps_metadata_async(dialect: ShellDialect) {
    if dialect != ShellDialect::PowerShell {
        return;
    }
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

    #[test]
    fn test_shell_dialects_side_by_side_in_same_process() {
        // 1. Variable tokens across all three dialects in the same test run
        assert_eq!(env_token("PATH", ShellDialect::PowerShell), "$env:PATH");
        assert_eq!(env_token("PATH", ShellDialect::Posix), "$PATH");
        assert_eq!(env_token("PATH", ShellDialect::Fish), "$PATH");

        // 2. Built-in common variable items across all three dialects in the same test run
        let ps_vars = common_var_items(ShellDialect::PowerShell);
        let posix_vars = common_var_items(ShellDialect::Posix);
        let fish_vars = common_var_items(ShellDialect::Fish);

        let ps_texts: Vec<&str> = ps_vars.iter().map(|i| i.text.as_str()).collect();
        assert!(ps_texts.contains(&"$PWD"));
        assert!(ps_texts.contains(&"$PROFILE"));
        assert!(ps_texts.contains(&"$env:TEMP"));
        assert!(ps_texts.contains(&"$_"));
        assert!(ps_texts.contains(&"$true"));
        assert!(!ps_texts.contains(&"$?"));
        assert!(!ps_texts.contains(&"$status"));

        let posix_texts: Vec<&str> = posix_vars.iter().map(|i| i.text.as_str()).collect();
        assert!(posix_texts.contains(&"$PWD"));
        assert!(posix_texts.contains(&"$OLDPWD"));
        assert!(posix_texts.contains(&"$?"));
        assert!(posix_texts.contains(&"$!"));
        assert!(!posix_texts.contains(&"$status"));
        assert!(!posix_texts.contains(&"$PROFILE"));

        let fish_texts: Vec<&str> = fish_vars.iter().map(|i| i.text.as_str()).collect();
        assert!(fish_texts.contains(&"$PWD"));
        assert!(fish_texts.contains(&"$status"));
        assert!(fish_texts.contains(&"$last_pid"));
        assert!(fish_texts.contains(&"$fish_pid"));
        assert!(!fish_texts.contains(&"$?"));
        assert!(!fish_texts.contains(&"$PROFILE"));

        // 3. Path-list splitting and env scope construction across dialects
        let sample_vars = vec![
            (
                "PATH".to_string(),
                if cfg!(windows) {
                    "C:\\bin;D:\\tools".to_string()
                } else {
                    "/usr/bin:/usr/local/bin".to_string()
                },
            ),
            ("WIN_LIST".to_string(), "C:\\one;C:\\two".to_string()),
            (
                "XDG_DATA_DIRS".to_string(),
                "/usr/share:/var/lib".to_string(),
            ),
            ("EDITOR".to_string(), "nvim".to_string()),
        ];

        let ps_scope =
            build_env_scope_item_from_vars(ShellDialect::PowerShell, sample_vars.clone());
        assert_eq!(ps_scope.text, "$env:");
        assert_eq!(ps_scope.label, "$env:Variables");
        let ps_win_list = ps_scope
            .children
            .iter()
            .find(|c| c.label == "WIN_LIST")
            .expect("WIN_LIST in PowerShell scope");
        assert_eq!(ps_win_list.text, "$env:WIN_LIST");
        let ps_win_entries: Vec<&str> = ps_win_list
            .children
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(ps_win_entries, vec!["C:\\one", "C:\\two"]);

        let posix_scope = build_env_scope_item_from_vars(ShellDialect::Posix, sample_vars.clone());
        assert_eq!(posix_scope.text, "$");
        assert_eq!(posix_scope.label, "$ENV Variables");
        let posix_xdg = posix_scope
            .children
            .iter()
            .find(|c| c.label == "XDG_DATA_DIRS")
            .expect("XDG_DATA_DIRS in Posix scope");
        assert_eq!(posix_xdg.text, "$XDG_DATA_DIRS");
        let posix_xdg_entries: Vec<&str> = posix_xdg
            .children
            .iter()
            .map(|c| c.label.as_str())
            .collect();
        assert_eq!(posix_xdg_entries, vec!["/usr/share", "/var/lib"]);

        let fish_scope = build_env_scope_item_from_vars(ShellDialect::Fish, sample_vars);
        assert_eq!(fish_scope.text, "$");
        assert_eq!(fish_scope.label, "$ENV Variables");
        let fish_xdg = fish_scope
            .children
            .iter()
            .find(|c| c.label == "XDG_DATA_DIRS")
            .expect("XDG_DATA_DIRS in Fish scope");
        assert_eq!(fish_xdg.text, "$XDG_DATA_DIRS");
        let fish_xdg_entries: Vec<&str> =
            fish_xdg.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(fish_xdg_entries, vec!["/usr/share", "/var/lib"]);

        // 4. ShellDialect::from_env detection rules
        assert_eq!(
            ShellDialect::from_env(Some("fish"), true, false, None, true),
            ShellDialect::Fish
        );
        assert_eq!(
            ShellDialect::from_env(Some("posix"), true, false, None, true),
            ShellDialect::Posix
        );
        assert_eq!(
            ShellDialect::from_env(Some("pwsh"), false, true, Some("/usr/bin/fish"), false),
            ShellDialect::PowerShell
        );
        assert_eq!(
            ShellDialect::from_env(None, false, false, Some("/usr/bin/fish"), false),
            ShellDialect::Fish
        );
        assert_eq!(
            ShellDialect::from_env(None, false, false, Some("/bin/bash"), false),
            ShellDialect::Posix
        );
    }

    #[test]
    fn test_convert_ps_export_preserves_structured_aliases_and_label() {
        let export = PsCommandExport {
            name: "Get-Process".to_string(),
            kind: "Cmdlet".to_string(),
            desc: "Module: Microsoft.PowerShell.Management".to_string(),
            params: vec![
                PsParamExport {
                    name: "Id".to_string(),
                    aliases: vec!["PID".to_string(), "ProcessId".to_string()],
                    type_name: "Int32[]".to_string(),
                    is_switch: false,
                    is_common: false,
                    valid_values: Vec::new(),
                    parameter_sets: vec!["Id".to_string()],
                },
                PsParamExport {
                    name: "InformationAction".to_string(),
                    aliases: vec!["ia".to_string()],
                    type_name: "ActionPreference".to_string(),
                    is_switch: false,
                    is_common: true,
                    valid_values: vec!["SilentlyContinue".to_string(), "Continue".to_string()],
                    parameter_sets: vec!["__AllParameterSets".to_string()],
                },
            ],
        };

        let item = convert_ps_export_to_item(export);
        assert_eq!(item.text, "Get-Process");
        assert_eq!(item.children.len(), 2);

        let id_param = &item.children[0];
        assert_eq!(id_param.text, "-Id");
        assert_eq!(id_param.label, "-Id (-PID, -ProcessId)");
        assert_eq!(
            id_param.aliases,
            vec!["PID".to_string(), "ProcessId".to_string()]
        );
        assert_eq!(id_param.parameter_sets, vec!["Id".to_string()]);

        let ia_param = &item.children[1];
        assert_eq!(ia_param.text, "-InformationAction");
        assert_eq!(ia_param.label, "-InformationAction (-ia)");
        assert_eq!(ia_param.aliases, vec!["ia".to_string()]);
    }
}
