use super::specs::{is_spec_filename, parse_spec_str, specs_dir};
use crate::model::{CwdEntry, Entry, EntryKind, RuntimeContext};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Canonical priority CLI tools list shared by Pane 1 catalog ordering and `--warm` batch warming.
pub(super) const PRIORITY_CLI_TOOLS: &[&str] = &[
    "git",
    "gh",
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

    fn from_env(
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

    /// Whether this dialect uses PowerShell syntax and cmdlet/parameter-set sources.
    pub const fn is_powershell(self) -> bool {
        matches!(self, ShellDialect::PowerShell)
    }

    /// Formats an environment variable reference in this dialect's syntax.
    fn env_token(self, name: &str) -> String {
        if self.is_powershell() {
            format!("$env:{name}")
        } else {
            format!("${name}")
        }
    }

    /// Returns the path-list separator for this dialect.
    const fn path_sep(self) -> char {
        if self.is_powershell() {
            ';'
        } else {
            ':'
        }
    }

    /// Whether an environment variable holds a list of paths that should be split into entries.
    fn is_path_list(self, name: &str, value: &str) -> bool {
        if self.is_powershell() {
            value.contains(';')
        } else {
            let upper = name.to_ascii_uppercase();
            value.contains(':') && (upper.contains("PATH") || upper.ends_with("DIRS"))
        }
    }

    /// Returns common built-in and special variable entries for this shell dialect.
    pub fn common_var_entries(self) -> Vec<Entry> {
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
                    Entry::new("$PWD", "$PWD (Current Dir)", EntryKind::Var, pwd)
                        .with_literal(true),
                    Entry::new("$HOME", "$HOME (User Home)", EntryKind::Var, home)
                        .with_literal(true),
                    Entry::new("$PROFILE", "$PROFILE (PS Profile)", EntryKind::Var, profile)
                        .with_literal(true),
                    Entry::new("$env:TEMP", "$env:TEMP (Temp Dir)", EntryKind::Var, temp)
                        .with_literal(true),
                    Entry::new(
                        "$_",
                        "$_ (Pipeline Item)",
                        EntryKind::Var,
                        "Current object in pipeline",
                    ),
                    Entry::new("$true", "$true", EntryKind::Var, "Boolean True"),
                    Entry::new("$false", "$false", EntryKind::Var, "Boolean False"),
                    Entry::new("$null", "$null", EntryKind::Var, "Null reference"),
                ]
            }
            ShellDialect::Posix | ShellDialect::Fish => {
                let var = |k: &str| env::var(k).unwrap_or_default();
                let tmp = env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
                let mut vars = vec![
                    Entry::new("$PWD", "$PWD (Current Dir)", EntryKind::Var, pwd)
                        .with_literal(true),
                    Entry::new("$HOME", "$HOME (User Home)", EntryKind::Var, var("HOME"))
                        .with_literal(true),
                    Entry::new("$TMPDIR", "$TMPDIR (Temp Dir)", EntryKind::Var, tmp)
                        .with_literal(true),
                    Entry::new("$USER", "$USER (User Name)", EntryKind::Var, var("USER"))
                        .with_literal(true),
                    Entry::new(
                        "$SHELL",
                        "$SHELL (Login Shell)",
                        EntryKind::Var,
                        var("SHELL"),
                    )
                    .with_literal(true),
                ];
                if self == ShellDialect::Posix {
                    vars.extend([
                        Entry::new(
                            "$OLDPWD",
                            "$OLDPWD (Previous Dir)",
                            EntryKind::Var,
                            var("OLDPWD"),
                        )
                        .with_literal(true),
                        Entry::new(
                            "$?",
                            "$? (Last Exit Code)",
                            EntryKind::Var,
                            "Current exit status",
                        ),
                        Entry::new(
                            "$!",
                            "$! (Last Background PID)",
                            EntryKind::Var,
                            "Current last background job PID",
                        ),
                    ]);
                } else {
                    vars.extend([
                        Entry::new(
                            "$status",
                            "$status (Last Exit Status)",
                            EntryKind::Var,
                            "Current exit status of last command",
                        ),
                        Entry::new(
                            "$last_pid",
                            "$last_pid (Last Background PID)",
                            EntryKind::Var,
                            "Current process ID of last background job",
                        ),
                        Entry::new(
                            "$fish_pid",
                            "$fish_pid (Current Fish PID)",
                            EntryKind::Var,
                            "Current process ID of this Fish session",
                        ),
                    ]);
                }
                vars
            }
        }
    }
}

/// Prevents a console window from flashing up for child processes on Windows; no-op elsewhere.
pub(super) fn hide_window(cmd: &mut Command) -> &mut Command {
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
pub(super) fn cache_dir() -> PathBuf {
    let base = if cfg!(windows) {
        env::var_os("LOCALAPPDATA").map(PathBuf::from)
    } else {
        env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
    }
    .unwrap_or_else(|| PathBuf::from("."));
    base.join("zpane")
}

pub(super) fn ensure_parent_dir(path: &Path) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
}

pub(super) fn ps_metadata_path() -> PathBuf {
    cache_dir().join("ps_metadata.json")
}

/// Injectable environment/filesystem source for building the Pane 1 Command Catalog.
pub(super) trait EnvSource {
    fn env_vars(&self) -> Vec<(String, String)>;
    fn path_dirs(&self) -> Vec<PathBuf>;
    fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)>;
    fn read_ps_metadata(&self) -> Option<Vec<u8>>;
    fn spec_files(&self) -> Vec<(String, String)>;
}

struct OsEnv;

impl EnvSource for OsEnv {
    fn env_vars(&self) -> Vec<(String, String)> {
        env::vars().collect()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        env::var_os("PATH")
            .map(|os| env::split_paths(&os).collect())
            .unwrap_or_default()
    }

    fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = executable_name(&entry, &path) {
                let name = name.to_string();
                out.push((name, path));
            }
        }
        out
    }

    fn read_ps_metadata(&self) -> Option<Vec<u8>> {
        fs::read(ps_metadata_path()).ok()
    }

    fn spec_files(&self) -> Vec<(String, String)> {
        let dir = specs_dir();
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut files: Vec<(String, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                if !path.is_file() {
                    return None;
                }
                let name = path.file_name()?.to_str()?.to_string();
                is_spec_filename(&name).then_some((name, path))
            })
            .collect();
        files.sort_by(|a, b| a.0.cmp(&b.0));

        files
            .into_iter()
            .filter_map(|(name, path)| fs::read_to_string(path).ok().map(|body| (name, body)))
            .collect()
    }
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

fn build_env_scope_entry_from_vars<I>(dialect: ShellDialect, vars_iter: I) -> Entry
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut vars: Vec<(String, String)> = vars_iter.into_iter().collect();
    vars.sort_by_key(|a| a.0.to_ascii_lowercase());
    let list_sep = dialect.path_sep();

    let mut children = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        let token = dialect.env_token(&k);
        let mut entry = Entry::new(&token, &k, EntryKind::Env, &v);

        if dialect.is_path_list(&k, &v) {
            for part in v.split(list_sep).filter(|s| !s.is_empty()) {
                entry.children.push(Entry::new(
                    format!("\"{part}\""),
                    part,
                    EntryKind::Value,
                    format!("Path entry in {token}"),
                ));
            }
        } else {
            entry.children.push(Entry::new(
                &token,
                format!("Insert {token}"),
                EntryKind::Var,
                format!("Current value: {v}"),
            ));
            if !v.is_empty() {
                entry.children.push(Entry::new(
                    format!("\"{v}\""),
                    format!("Literal: {v}"),
                    EntryKind::Value,
                    format!("Insert literal value of {token}"),
                ));
            }
        }
        children.push(entry);
    }

    let (text, label) = if dialect.is_powershell() {
        ("$env:", "$env:Variables")
    } else {
        ("$", "$ENV Variables")
    };
    let mut root = Entry::scope_root(
        text,
        label,
        EntryKind::Env,
        "Browse and insert environment variables",
    );
    root.children = children;
    root
}

fn build_var_scope_entry(dialect: ShellDialect, ctx: &RuntimeContext) -> Entry {
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
    let mut root = Entry::scope_root("$", label, EntryKind::Var, desc);
    root.children = dialect.common_var_entries();
    if let Some(ref branch) = ctx.git_branch {
        let trimmed = branch.trim();
        if !trimmed.is_empty() {
            root.children.push(
                Entry::new(
                    "$GIT_BRANCH",
                    format!("$GIT_BRANCH ({trimmed})"),
                    EntryKind::Var,
                    trimmed,
                )
                .with_literal(true),
            );
        }
    }
    for child in &mut root.children {
        let tok = child.text.clone();
        let val = child.desc.clone();
        child.children.push(Entry::new(
            &tok,
            format!("Insert {tok}"),
            EntryKind::Var,
            format!("Value: {val}"),
        ));
        if child.has_literal && !val.is_empty() {
            child.children.push(Entry::new(
                format!("\"{val}\""),
                format!("Literal: {val}"),
                EntryKind::Value,
                format!("Insert literal path/value of {tok}"),
            ));
        }
    }
    root
}

fn build_cwd_scope_entry(ctx: &RuntimeContext) -> Option<Entry> {
    if ctx.cwd_entries.is_empty() {
        return None;
    }
    let mut root = Entry::scope_root(
        "./",
        "./ Current Dir",
        EntryKind::Path,
        "Browse and insert current directory files & folders",
    );
    root.children = ctx.cwd_path_entries(false);
    Some(root)
}

fn convert_ps_export_to_entry(exp: PsCommandExport) -> Entry {
    let kind = match exp.kind.as_str() {
        "Func" => EntryKind::Func,
        "Alias" => EntryKind::Alias,
        "CLI" => EntryKind::Cli,
        _ => EntryKind::Cmdlet,
    };

    let mut cmd_entry = Entry::new(&exp.name, &exp.name, kind, &exp.desc);
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
            EntryKind::Flag
        } else {
            EntryKind::Param
        };
        let desc = format!("[{}] Parameter {}", p.type_name, label);

        let mut param_entry = Entry::new(&p_flag, label, p_kind, desc)
            .with_parameter_sets(p.parameter_sets)
            .with_aliases(p.aliases);

        for val in p.valid_values {
            param_entry.children.push(Entry::new(
                &val,
                &val,
                EntryKind::Enum,
                format!("Allowed [{}] value for {p_flag}", p.type_name),
            ));
        }
        if p.is_switch {
            param_entry.children.push(Entry::placeholder(
                "(Flag - No Value Needed)",
                EntryKind::Flag,
                format!("Press Enter or Tab to include {p_flag}"),
            ));
            param_entry.children.push(Entry::new(
                ":$true",
                ":$true",
                EntryKind::Value,
                "Explicitly enable switch",
            ));
            param_entry.children.push(Entry::new(
                ":$false",
                ":$false",
                EntryKind::Value,
                "Explicitly disable switch",
            ));
        }

        if p.is_common {
            common.push(param_entry);
        } else {
            primary.push(param_entry);
        }
    }

    primary.extend(common);
    cmd_entry.children = primary;
    cmd_entry
}

fn drain_ordered(
    list: &mut Vec<Entry>,
    seen: &mut HashSet<String>,
    order: &[String],
    map: &mut HashMap<String, Entry>,
) {
    for key in order {
        if !seen.contains(key) {
            if let Some(owned) = map.remove(key) {
                seen.insert(key.clone());
                list.push(owned);
            }
        }
    }
}

/// Detects current Git branch/branches and shallow CWD entries to populate [`RuntimeContext`].
/// Any fields already provided in `explicit` are preserved.
pub fn detect_runtime_context(explicit: RuntimeContext) -> RuntimeContext {
    let mut ctx = explicit;
    let Ok(cwd) = std::env::current_dir() else {
        return ctx;
    };

    if ctx.git_branch.is_none() || ctx.git_branches.is_empty() {
        let (detected_branch, detected_branches) = detect_git_context_in(&cwd);
        if ctx.git_branch.is_none() {
            ctx.git_branch = detected_branch;
        }
        if ctx.git_branches.is_empty() {
            ctx.git_branches = detected_branches;
        }
    }

    if let Some(ref current) = ctx.git_branch {
        if !ctx.git_branches.iter().any(|b| b == current) {
            ctx.git_branches.insert(0, current.clone());
        }
    }

    if ctx.cwd_entries.is_empty() {
        ctx.cwd_entries = scan_cwd_entries(&cwd);
    }

    ctx
}

fn resolve_git_dir(start_dir: &Path) -> Option<PathBuf> {
    let mut cur = Some(start_dir);
    while let Some(dir) = cur {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            if let Ok(content) = fs::read_to_string(&candidate) {
                if let Some(rest) = content.trim().strip_prefix("gitdir:") {
                    let git_path = PathBuf::from(rest.trim());
                    return Some(if git_path.is_absolute() {
                        git_path
                    } else {
                        dir.join(git_path)
                    });
                }
            }
        }
        cur = dir.parent();
    }
    None
}

fn collect_git_refs(base: &Path, dir: &Path, out: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_git_refs(base, &path, out);
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(base) {
                let branch = rel.to_string_lossy().replace('\\', "/");
                if !branch.is_empty() {
                    out.push(branch);
                }
            }
        }
    }
}

fn detect_git_context_in(start_dir: &Path) -> (Option<String>, Vec<String>) {
    let Some(git_dir) = resolve_git_dir(start_dir) else {
        return (None, Vec::new());
    };

    let current = fs::read_to_string(git_dir.join("HEAD"))
        .ok()
        .and_then(|head| {
            let trimmed = head.trim();
            if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
                Some(branch.to_string())
            } else if let Some(other_ref) = trimmed.strip_prefix("ref: ") {
                Some(other_ref.to_string())
            } else if !trimmed.is_empty() {
                Some(trimmed.chars().take(8).collect())
            } else {
                None
            }
        });

    let mut branches = Vec::new();
    let heads_dir = git_dir.join("refs").join("heads");
    if heads_dir.is_dir() {
        collect_git_refs(&heads_dir, &heads_dir, &mut branches);
    }
    if let Ok(packed) = fs::read_to_string(git_dir.join("packed-refs")) {
        for line in packed.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('^') {
                continue;
            }
            if let Some((_, ref_name)) = line.split_once(' ') {
                if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
                    if !branches.iter().any(|b| b == branch) {
                        branches.push(branch.to_string());
                    }
                }
            }
        }
    }
    branches.sort();

    if let Some(ref cur) = current {
        branches.retain(|b| b != cur);
        branches.insert(0, cur.clone());
    }

    (current, branches)
}

fn scan_cwd_entries(dir: &Path) -> Vec<CwdEntry> {
    const MAX_CWD_ENTRIES: usize = 200;
    let Ok(rd) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<CwdEntry> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if name.is_empty() {
                return None;
            }
            let is_dir = e.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
            Some(CwdEntry { name, is_dir })
        })
        .collect();
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            a.name
                .to_ascii_lowercase()
                .cmp(&b.name.to_ascii_lowercase())
        })
    });
    items.truncate(MAX_CWD_ENTRIES);
    items
}

fn merge_spec_into_entry(target: &mut Entry, spec: Entry) {
    if !spec.desc.is_empty() {
        if target.desc.is_empty() {
            target.desc = spec.desc;
        } else if !target.desc.contains(&spec.desc) {
            target.desc = format!("{} — {}", spec.desc, target.desc);
        }
    }
    if target.aliases.is_empty() && !spec.aliases.is_empty() {
        target.aliases = spec.aliases;
    }
    if !spec.children.is_empty() {
        if target.children.is_empty() {
            target.children = spec.children;
        } else {
            let mut existing: HashSet<String> = target
                .children
                .iter()
                .map(|c| c.text.to_ascii_lowercase())
                .collect();
            for child in spec.children {
                if existing.insert(child.text.to_ascii_lowercase()) {
                    target.children.push(child);
                }
            }
        }
    }
}

pub(super) fn load_catalog_with_context(
    dialect: ShellDialect,
    env_src: &dyn EnvSource,
    ctx: &RuntimeContext,
) -> Vec<Entry> {
    let mut list = Vec::with_capacity(5000);
    let mut seen = HashSet::new();

    // 0. Active shell session history at the top of Pane 1
    let mut hist_seen = HashSet::new();
    for cmd in &ctx.history {
        let trimmed = cmd.trim();
        if trimmed.is_empty() || !hist_seen.insert(trimmed.to_ascii_lowercase()) {
            continue;
        }
        list.push(Entry::new(
            trimmed.to_string(),
            trimmed.to_string(),
            EntryKind::History,
            "Recent shell history",
        ));
    }

    // 1. Variable and CWD scope roots first
    let env_scope = build_env_scope_entry_from_vars(dialect, env_src.env_vars());
    seen.insert(env_scope.text.to_ascii_lowercase());
    list.push(env_scope);

    let var_scope = build_var_scope_entry(dialect, ctx);
    seen.insert(var_scope.text.to_ascii_lowercase());
    list.push(var_scope);

    if let Some(cwd_scope) = build_cwd_scope_entry(ctx) {
        seen.insert(cwd_scope.text.to_ascii_lowercase());
        list.push(cwd_scope);
    }

    // 2. Load exported PowerShell metadata if available (PowerShell dialect only)
    let mut ps_map: HashMap<String, Entry> = HashMap::new();
    let mut ps_order: Vec<String> = Vec::new();
    if dialect.is_powershell() {
        if let Some(bytes) = env_src.read_ps_metadata() {
            if let Ok(exports) = serde_json::from_slice::<Vec<PsCommandExport>>(&bytes) {
                for exp in exports {
                    let key = exp.name.to_ascii_lowercase();
                    if let std::collections::hash_map::Entry::Vacant(e) = ps_map.entry(key.clone())
                    {
                        ps_order.push(key);
                        e.insert(convert_ps_export_to_entry(exp));
                    }
                }
            }
        }
    }

    // 3. Scan PATH directories for CLI executables
    //    (Windows: .exe / .cmd / .bat; Unix: any file with an execute bit)
    let mut cli_map: HashMap<String, Entry> = HashMap::new();
    let mut cli_order: Vec<String> = Vec::new();
    for dir in env_src.path_dirs() {
        for (name, path) in env_src.dir_executables(&dir) {
            let key = name.to_ascii_lowercase();
            if let std::collections::hash_map::Entry::Vacant(e) = cli_map.entry(key.clone()) {
                cli_order.push(key);
                e.insert(Entry::new(
                    name.clone(),
                    name,
                    EntryKind::Cli,
                    path.display().to_string(),
                ));
            }
        }
    }

    // 3b. Load declarative specs (carapace-spec / Fig) and merge or register commands
    for (file_name, content) in env_src.spec_files() {
        let Ok(spec_entry) = parse_spec_str(&file_name, &content) else {
            continue;
        };
        let key = spec_entry.text.to_ascii_lowercase();
        if let Some(cli_entry) = cli_map.get_mut(&key) {
            merge_spec_into_entry(cli_entry, spec_entry);
        } else if let Some(ps_entry) = ps_map.get_mut(&key) {
            merge_spec_into_entry(ps_entry, spec_entry);
        } else {
            cli_order.push(key.clone());
            cli_map.insert(key, spec_entry);
        }
    }

    // Enrich `git` description with detected current branch if available
    if let Some(ref branch) = ctx.git_branch {
        if let Some(git_entry) = cli_map.get_mut("git") {
            git_entry.desc = format!("[branch: {branch}] {}", git_entry.desc);
        }
    }

    // 4. Priority Favorites at the top
    for &p in PRIORITY_CLI_TOOLS {
        let key = p.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        if let Some(entry) = cli_map.remove(&key) {
            seen.insert(key);
            list.push(entry);
        } else if let Some(entry) = ps_map.remove(&key) {
            seen.insert(key);
            list.push(entry);
        }
    }

    // 5. Profile Functions & Aliases from PS metadata
    for key in &ps_order {
        if seen.contains(key) {
            continue;
        }
        if ps_map
            .get(key)
            .is_some_and(|entry| matches!(entry.kind, EntryKind::Func | EntryKind::Alias))
        {
            if let Some(owned) = ps_map.remove(key) {
                seen.insert(key.clone());
                list.push(owned);
            }
        }
    }

    // 6. All remaining CLI executables, then PowerShell Cmdlets
    drain_ordered(&mut list, &mut seen, &cli_order, &mut cli_map);
    drain_ordered(&mut list, &mut seen, &ps_order, &mut ps_map);

    list
}

#[cfg(test)]
pub(super) fn load_catalog_with(dialect: ShellDialect, env_src: &dyn EnvSource) -> Vec<Entry> {
    load_catalog_with_context(dialect, env_src, &RuntimeContext::default())
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
    ensure_parent_dir(&dest);
    fs::write(dest, &output.stdout)?;
    Ok(count)
}

/// Loads the initial Pane 1 Command Catalog for `dialect` without spawning side-effect threads.
/// Background PowerShell metadata generation on first run is driven by [`CliIntrospector`]
/// and surfaced through [`Introspect::poll_updates`].
pub fn load_catalog(dialect: ShellDialect, ctx: &RuntimeContext) -> Vec<Entry> {
    load_catalog_with_context(dialect, &OsEnv, ctx)
}

/// Refreshes any dialect-specific metadata (e.g. PowerShell cmdlets) and reloads
/// the Pane 1 Command Catalog.
pub fn reload_catalog(dialect: ShellDialect, ctx: &RuntimeContext) -> Vec<Entry> {
    if dialect.is_powershell() {
        let _ = sync_powershell_metadata();
    }
    load_catalog_with_context(dialect, &OsEnv, ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::EntryRole;

    struct FakeEnv {
        vars: Vec<(String, String)>,
        path_dirs: Vec<PathBuf>,
        bins: HashMap<PathBuf, Vec<(String, PathBuf)>>,
        ps_metadata: Option<Vec<u8>>,
        specs: Vec<(String, String)>,
    }

    impl EnvSource for FakeEnv {
        fn env_vars(&self) -> Vec<(String, String)> {
            self.vars.clone()
        }

        fn path_dirs(&self) -> Vec<PathBuf> {
            self.path_dirs.clone()
        }

        fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)> {
            self.bins.get(dir).cloned().unwrap_or_default()
        }

        fn read_ps_metadata(&self) -> Option<Vec<u8>> {
            self.ps_metadata.clone()
        }

        fn spec_files(&self) -> Vec<(String, String)> {
            self.specs.clone()
        }
    }

    #[test]
    fn test_shell_dialects_side_by_side_in_same_process() {
        // 1. Variable tokens across all three dialects in the same test run
        assert_eq!(ShellDialect::PowerShell.env_token("PATH"), "$env:PATH");
        assert_eq!(ShellDialect::Posix.env_token("PATH"), "$PATH");
        assert_eq!(ShellDialect::Fish.env_token("PATH"), "$PATH");

        // 2. Built-in common variable entries across all three dialects in the same test run
        let ps_vars = ShellDialect::PowerShell.common_var_entries();
        let posix_vars = ShellDialect::Posix.common_var_entries();
        let fish_vars = ShellDialect::Fish.common_var_entries();

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
            build_env_scope_entry_from_vars(ShellDialect::PowerShell, sample_vars.clone());
        assert!(ps_scope.is_scope_root());
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

        let posix_scope = build_env_scope_entry_from_vars(ShellDialect::Posix, sample_vars.clone());
        assert!(posix_scope.is_scope_root());
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

        let fish_scope = build_env_scope_entry_from_vars(ShellDialect::Fish, sample_vars);
        assert!(fish_scope.is_scope_root());
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
    fn test_convert_ps_export_preserves_structured_aliases_and_flag_role() {
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
                    name: "Force".to_string(),
                    aliases: vec!["f".to_string()],
                    type_name: "SwitchParameter".to_string(),
                    is_switch: true,
                    is_common: false,
                    valid_values: Vec::new(),
                    parameter_sets: vec!["__AllParameterSets".to_string()],
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

        let entry = convert_ps_export_to_entry(export);
        assert_eq!(entry.text, "Get-Process");
        assert_eq!(entry.children.len(), 3);

        let id_param = &entry.children[0];
        assert_eq!(id_param.text, "-Id");
        assert_eq!(id_param.label, "-Id (-PID, -ProcessId)");
        assert_eq!(id_param.kind, EntryKind::Param);
        assert_eq!(
            id_param.aliases,
            vec!["PID".to_string(), "ProcessId".to_string()]
        );
        assert_eq!(id_param.parameter_sets, vec!["Id".to_string()]);

        let force_flag = &entry.children[1];
        assert_eq!(force_flag.text, "-Force");
        assert_eq!(force_flag.kind, EntryKind::Flag);
        assert!(force_flag.children[0].is_placeholder());
        assert_eq!(force_flag.children[0].role, EntryRole::Placeholder);

        let ia_param = &entry.children[2];
        assert_eq!(ia_param.text, "-InformationAction");
        assert_eq!(ia_param.label, "-InformationAction (-ia)");
        assert_eq!(ia_param.aliases, vec!["ia".to_string()]);
    }

    #[test]
    fn test_var_scope_uses_explicit_literal_flag_instead_of_text_sniffing() {
        let ctx = RuntimeContext {
            git_branch: Some("feat/roadmap".to_string()),
            ..Default::default()
        };
        let ps_vars = build_var_scope_entry(ShellDialect::PowerShell, &ctx);
        assert!(ps_vars.is_scope_root());

        let git_var = ps_vars
            .children
            .iter()
            .find(|c| c.text == "$GIT_BRANCH")
            .expect("$GIT_BRANCH in var scope when git branch detected");
        assert!(git_var.has_literal);
        assert!(git_var.desc.contains("feat/roadmap"));

        let null_entry = ps_vars
            .children
            .iter()
            .find(|c| c.text == "$null")
            .expect("$null in PowerShell var scope");
        assert_eq!(null_entry.children.len(), 1);
        assert_eq!(null_entry.children[0].text, "$null");

        let pwd_entry = ps_vars
            .children
            .iter()
            .find(|c| c.text == "$PWD")
            .expect("$PWD in PowerShell var scope");
        assert!(pwd_entry.has_literal);
        assert_eq!(pwd_entry.children.len(), 2);
        assert!(pwd_entry.children[1].label.starts_with("Literal: "));
    }

    #[test]
    fn test_load_catalog_with_orders_scopes_priority_funcs_cli_and_cmdlets() {
        let bin_dir = PathBuf::from("/fake/bin");
        let ps_exports = vec![
            PsCommandExport {
                name: "myfn".to_string(),
                kind: "Func".to_string(),
                desc: "Profile function".to_string(),
                params: vec![],
            },
            PsCommandExport {
                name: "Get-Item".to_string(),
                kind: "Cmdlet".to_string(),
                desc: "Module: Microsoft.PowerShell.Management".to_string(),
                params: vec![],
            },
            PsCommandExport {
                name: "git".to_string(),
                kind: "CLI".to_string(),
                desc: "Shadowed by PATH git".to_string(),
                params: vec![],
            },
        ];

        let fake = FakeEnv {
            vars: vec![("EDITOR".to_string(), "nvim".to_string())],
            path_dirs: vec![bin_dir.clone()],
            bins: HashMap::from([(
                bin_dir,
                vec![
                    ("zzztool".to_string(), PathBuf::from("/fake/bin/zzztool")),
                    ("git".to_string(), PathBuf::from("/fake/bin/git")),
                    ("cargo".to_string(), PathBuf::from("/fake/bin/cargo")),
                ],
            )]),
            ps_metadata: Some(serde_json::to_vec(&ps_exports).unwrap()),
            specs: Vec::new(),
        };

        let ps_catalog = load_catalog_with(ShellDialect::PowerShell, &fake);
        let texts: Vec<&str> = ps_catalog.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["$env:", "$", "git", "cargo", "myfn", "zzztool", "Get-Item"]
        );

        let posix_catalog = load_catalog_with(ShellDialect::Posix, &fake);
        let posix_texts: Vec<&str> = posix_catalog.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            posix_texts,
            vec!["$", "$", "git", "cargo", "zzztool"],
            "Non-PowerShell dialects must ignore PS metadata"
        );
    }

    #[test]
    fn test_load_catalog_with_context_places_history_first_merges_specs_and_enriches_git() {
        let bin_dir = PathBuf::from("/fake/bin");
        let kubectl_spec = r#"
name: kubectl
description: Kubernetes CLI
flags:
  -n, --namespace=: Target namespace
commands:
  - name: get
    description: Display resources
"#;
        let standalone_spec = r#"{
  "name": "mycustom",
  "description": "Standalone spec command",
  "options": [
    { "name": "--verbose", "description": "Verbose output" }
  ]
}"#;
        let fake = FakeEnv {
            vars: vec![],
            path_dirs: vec![bin_dir.clone()],
            bins: HashMap::from([(
                bin_dir,
                vec![
                    ("git".to_string(), PathBuf::from("/fake/bin/git")),
                    ("kubectl".to_string(), PathBuf::from("/fake/bin/kubectl")),
                ],
            )]),
            ps_metadata: None,
            specs: vec![
                ("kubectl.yaml".to_string(), kubectl_spec.to_string()),
                ("mycustom.json".to_string(), standalone_spec.to_string()),
            ],
        };
        let ctx = RuntimeContext {
            git_branch: Some("main".to_string()),
            git_branches: vec!["main".to_string(), "feat/ui".to_string()],
            cwd_entries: vec![
                CwdEntry {
                    name: "src".to_string(),
                    is_dir: true,
                },
                CwdEntry {
                    name: "Cargo.toml".to_string(),
                    is_dir: false,
                },
            ],
            history: vec![
                "cargo test --locked".to_string(),
                "git status".to_string(),
                "cargo test --locked".to_string(),
            ],
        };

        let catalog = load_catalog_with_context(ShellDialect::PowerShell, &fake, &ctx);
        let texts: Vec<&str> = catalog.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "cargo test --locked",
                "git status",
                "$env:",
                "$",
                "./",
                "git",
                "kubectl",
                "mycustom",
            ]
        );
        assert_eq!(catalog[0].kind, EntryKind::History);
        assert_eq!(catalog[4].children.len(), 2);
        assert!(catalog[5].desc.contains("[branch: main]"));
        assert!(catalog[6]
            .children
            .iter()
            .any(|c| c.text == "--namespace" && c.kind == EntryKind::Param));
        assert!(catalog[6]
            .children
            .iter()
            .any(|c| c.text == "get" && c.kind == EntryKind::Subcmd));
    }

    #[test]
    fn test_detect_git_context_and_cwd_scan_in_temp_dir() {
        let base = std::env::temp_dir().join(format!("zpane_ctx_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join(".git").join("refs").join("heads").join("feat")).unwrap();
        fs::create_dir_all(base.join("subdir").join("nested")).unwrap();
        fs::write(base.join("alpha.txt"), b"hello").unwrap();
        fs::write(
            base.join(".git").join("HEAD"),
            b"ref: refs/heads/feat/demo\n",
        )
        .unwrap();
        fs::write(
            base.join(".git").join("refs").join("heads").join("main"),
            b"0123456789abcdef\n",
        )
        .unwrap();
        fs::write(
            base.join(".git")
                .join("refs")
                .join("heads")
                .join("feat")
                .join("demo"),
            b"fedcba9876543210\n",
        )
        .unwrap();

        let (branch, branches) = detect_git_context_in(&base.join("subdir").join("nested"));
        assert_eq!(branch.as_deref(), Some("feat/demo"));
        assert_eq!(branches, vec!["feat/demo", "main"]);

        let cwd = scan_cwd_entries(&base);
        let names: Vec<(&str, bool)> = cwd.iter().map(|e| (e.name.as_str(), e.is_dir)).collect();
        assert_eq!(
            names,
            vec![(".git", true), ("subdir", true), ("alpha.txt", false)]
        );

        let _ = fs::remove_dir_all(&base);
    }
}
