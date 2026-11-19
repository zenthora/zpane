use super::config::{load_user_config, UserConfig};
use super::powershell::{
    parse_ps_metadata_entries, ps_metadata_path, sync_powershell_metadata, MAX_PS_METADATA_BYTES,
};
use super::project_tasks::{discover_project_task_specs, merge_task_entries_front};
use super::shell_rc::{collect_shell_rc_files, parse_posix_fish_definitions};
use super::specs::{is_spec_filename, parse_spec_str, spec_search_dirs};
use crate::model::{Entry, EntryKind, RuntimeContext};
use crate::sanitize::{contains_ignore_ascii_case, is_disallowed_control, sanitize_single_line};
use crate::shell::quote_single_for_dialect;
use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

pub(super) const MAX_SPEC_BYTES: usize = 2 * 1024 * 1024;
pub(super) const MAX_SPEC_FILES: usize = 256;

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
    "touch",
    "which",
];

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
        if let Some(zs) = zpane_shell.map(str::trim).filter(|s| !s.is_empty()) {
            if zs.eq_ignore_ascii_case("pwsh") || zs.eq_ignore_ascii_case("powershell") {
                return ShellDialect::PowerShell;
            }
            if zs.eq_ignore_ascii_case("fish") {
                return ShellDialect::Fish;
            }
            if zs.eq_ignore_ascii_case("posix")
                || zs.eq_ignore_ascii_case("bash")
                || zs.eq_ignore_ascii_case("zsh")
                || zs.eq_ignore_ascii_case("sh")
                || zs.eq_ignore_ascii_case("dash")
                || zs.eq_ignore_ascii_case("ash")
                || zs.eq_ignore_ascii_case("ksh")
                || zs.eq_ignore_ascii_case("mksh")
            {
                return ShellDialect::Posix;
            }
        }
        if is_windows || has_ps_module_path {
            ShellDialect::PowerShell
        } else if has_fish_version
            || shell
                .map(str::trim)
                .map(|s| s == "fish" || s.ends_with("/fish"))
                .unwrap_or(false)
        {
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
        let mut out = String::with_capacity(name.len() + 5);
        if self.is_powershell() {
            out.push_str("$env:");
        } else {
            out.push('$');
        }
        out.push_str(name);
        out
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
            value.contains(':')
                && (contains_ignore_ascii_case(name, "PATH")
                    || ends_with_ignore_ascii_case(name, "DIRS"))
        }
    }

    /// Returns common built-in and special variable entries for this shell dialect.
    pub fn common_var_entries(self) -> Vec<Entry> {
        let pwd = sanitize_single_line(
            &env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
        .into_owned();

        match self {
            ShellDialect::PowerShell => {
                let home = sanitize_single_line(
                    &env::var("USERPROFILE")
                        .or_else(|_| env::var("HOME"))
                        .unwrap_or_default(),
                )
                .into_owned();
                let temp =
                    sanitize_single_line(&env::temp_dir().display().to_string()).into_owned();
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
                let var =
                    |k: &str| sanitize_single_line(&env::var(k).unwrap_or_default()).into_owned();
                let tmp = sanitize_single_line(
                    &env::var("TMPDIR")
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| {
                            if let Some(prefix) = non_empty_abs_env_path("PREFIX") {
                                prefix.join("tmp").display().to_string()
                            } else {
                                "/tmp".to_string()
                            }
                        }),
                )
                .into_owned();
                let mut vars = Vec::with_capacity(8);
                vars.extend([
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
                ]);
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

#[inline]
pub(super) fn cmp_ascii_case_insensitive(a: &str, b: &str) -> Ordering {
    a.bytes()
        .map(|x| x.to_ascii_lowercase())
        .cmp(b.bytes().map(|y| y.to_ascii_lowercase()))
}

#[inline]
pub(super) fn starts_with_ignore_ascii_case(haystack: &str, prefix: &str) -> bool {
    haystack
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

#[inline]
pub(super) fn ends_with_ignore_ascii_case(haystack: &str, suffix: &str) -> bool {
    haystack
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| haystack.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

pub(super) fn is_safe_command_token(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with('-')
        && name != "."
        && name != ".."
        && !name
            .chars()
            .any(|c| is_disallowed_control(c) || c.is_whitespace())
}

fn is_valid_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && name.bytes().enumerate().all(|(idx, b)| {
            if idx == 0 {
                b.is_ascii_alphabetic() || b == b'_'
            } else {
                b.is_ascii_alphanumeric() || b == b'_'
            }
        })
}

/// Quotes a literal string safely for `dialect` using single-quoting rules after stripping
/// NUL/CR/LF and control characters so command substitution (`$(...)`, backticks) and quote
/// breakouts are neutralized.
fn quote_shell_literal(dialect: ShellDialect, raw: &str) -> Option<String> {
    let clean = sanitize_single_line(raw);
    if clean.is_empty() {
        return None;
    }
    Some(quote_single_for_dialect(clean.as_ref(), Some(dialect)))
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

pub(super) fn non_empty_abs_env_path(var: &str) -> Option<PathBuf> {
    let val = env::var_os(var)?;
    if val.is_empty() {
        return None;
    }
    let path = PathBuf::from(val);
    path.is_absolute().then_some(path)
}

/// Per-user cache directory: `%LOCALAPPDATA%\zpane` on Windows,
/// `$XDG_CACHE_HOME/zpane` or `~/.cache/zpane` on Linux / Termux.
pub(super) fn cache_dir() -> PathBuf {
    let base = if cfg!(windows) {
        non_empty_abs_env_path("LOCALAPPDATA").or_else(|| {
            non_empty_abs_env_path("USERPROFILE").map(|p| p.join("AppData").join("Local"))
        })
    } else {
        non_empty_abs_env_path("XDG_CACHE_HOME")
            .or_else(|| non_empty_abs_env_path("HOME").map(|h| h.join(".cache")))
    }
    .unwrap_or_else(env::temp_dir);
    base.join("zpane")
}

#[cfg(unix)]
fn is_zpane_owned_dir(dir: &Path) -> bool {
    dir.file_name().and_then(|n| n.to_str()) == Some("zpane")
        || dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            == Some("zpane")
}

pub(super) fn ensure_secure_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(dir)?;
    #[cfg(unix)]
    {
        if is_zpane_owned_dir(dir) {
            if let Ok(meta) = fs::symlink_metadata(dir) {
                if meta.is_dir() && !meta.file_type().is_symlink() {
                    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
                }
            }
        }
    }
    Ok(())
}

/// Atomically writes `data` to `path` with `0600` file permissions and `0700` zpane cache directory
/// permissions on Unix, avoiding symlink-following and world-readable race windows.
pub(super) fn write_secure_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing parent directory")
    })?;
    ensure_secure_dir(parent)?;

    static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);
    let pid = std::process::id();
    let seq = TEMP_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("cache");

    let mut tmp_path = None;
    let mut file = None;
    for attempt in 0..8u64 {
        let candidate = parent.join(format!(".{stem}.{pid}.{seq}.{attempt}.tmp"));
        let mut opts = fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        opts.mode(0o600);
        match opts.open(&candidate) {
            Ok(f) => {
                tmp_path = Some(candidate);
                file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }

    let (Some(tmp_path), Some(mut file)) = (tmp_path, file) else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate unique temp file",
        ));
    };

    let res = (|| -> std::io::Result<()> {
        file.write_all(data)?;
        file.flush()?;
        #[cfg(unix)]
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if res.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    res
}

/// Reads a regular file up to `max_bytes`, returning `None` for FIFOs, device nodes,
/// missing files, or files larger than `max_bytes`.
pub(super) fn read_bounded_bytes(path: &Path, max_bytes: u64) -> Option<Vec<u8>> {
    // Check `metadata` before `open` so Unix FIFOs cannot block on open().
    let pre = fs::metadata(path).ok()?;
    if !pre.file_type().is_file() || pre.len() > max_bytes {
        return None;
    }
    let file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_file() || meta.len() > max_bytes {
        return None;
    }
    let mut buf = Vec::with_capacity(meta.len().min(65_536) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .ok()?;
    if (buf.len() as u64) > max_bytes {
        return None;
    }
    Some(buf)
}

pub(super) fn read_bounded_string(path: &Path, max_bytes: u64) -> Option<String> {
    let bytes = read_bounded_bytes(path, max_bytes)?;
    String::from_utf8(bytes).ok()
}

pub(super) fn read_bounded_string_lossy(path: &Path, max_bytes: u64) -> Option<String> {
    let bytes = read_bounded_bytes(path, max_bytes)?;
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
    })
}

pub(super) fn home_dir() -> Option<PathBuf> {
    non_empty_abs_env_path("HOME").or_else(|| non_empty_abs_env_path("USERPROFILE"))
}

pub(super) fn read_tail_string(path: &Path, max_bytes: u64) -> Option<String> {
    let pre = fs::metadata(path).ok()?;
    if !pre.file_type().is_file() {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.file_type().is_file() {
        return None;
    }
    let len = meta.len();
    let drop_first_line = if len > max_bytes {
        file.seek(SeekFrom::Start(len - max_bytes)).ok()?;
        true
    } else {
        false
    };
    let mut buf = Vec::with_capacity(len.min(max_bytes).min(65_536) as usize);
    file.take(max_bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf);
    if drop_first_line {
        if let Some((_, rest)) = text.split_once('\n') {
            return Some(rest.to_string());
        }
    }
    Some(text.into_owned())
}

pub(super) fn collect_path_dirs_with_prefix(
    path_os: Option<&OsStr>,
    prefix_os: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut seen_canon = HashSet::new();

    let mut push_if_unique_dir = |candidate: PathBuf| {
        if candidate.as_os_str().is_empty() || !candidate.is_absolute() {
            return;
        }
        let Ok(canon) = fs::canonicalize(&candidate) else {
            return;
        };
        if canon.is_dir() && seen_canon.insert(canon) {
            dirs.push(candidate);
        }
    };

    if let Some(os) = path_os {
        for p in env::split_paths(os) {
            push_if_unique_dir(p);
        }
    }
    if let Some(prefix) = prefix_os.filter(|p| !p.is_empty()) {
        let prefix_path = Path::new(prefix);
        if prefix_path.is_absolute() {
            push_if_unique_dir(prefix_path.join("bin"));
            push_if_unique_dir(prefix_path.join("bin").join("applets"));
        }
    }
    dirs
}

fn load_specs_from_dir_bounded(
    dir: &Path,
    max_files: usize,
    seen_canon_dirs: &mut HashSet<PathBuf>,
) -> Vec<(String, String)> {
    if max_files == 0 {
        return Vec::new();
    }
    let Ok(canon_dir) = fs::canonicalize(dir) else {
        return Vec::new();
    };
    if !seen_canon_dirs.insert(canon_dir.clone()) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(&canon_dir) else {
        return Vec::new();
    };
    let mut files: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_string();
            if !is_spec_filename(&name) {
                return None;
            }
            if let Ok(sym_meta) = fs::symlink_metadata(&path) {
                if sym_meta.file_type().is_symlink() {
                    let canon_file = fs::canonicalize(&path).ok()?;
                    if !canon_file.starts_with(&canon_dir) {
                        return None;
                    }
                }
            }
            if !path.is_file() {
                return None;
            }
            Some((name, path))
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files.truncate(max_files);

    files
        .into_iter()
        .filter_map(|(name, path)| {
            read_bounded_string(&path, MAX_SPEC_BYTES as u64).map(|body| (name, body))
        })
        .collect()
}

pub(super) fn load_specs_from_dirs(dirs: &[PathBuf]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut seen_canon_dirs = HashSet::new();
    for dir in dirs {
        let remaining = MAX_SPEC_FILES.saturating_sub(out.len());
        if remaining == 0 {
            break;
        }
        out.extend(load_specs_from_dir_bounded(
            dir,
            remaining,
            &mut seen_canon_dirs,
        ));
    }
    out
}

/// Injectable environment/filesystem source for building the Pane 1 Command Catalog.
pub(super) trait EnvSource {
    fn env_vars(&self) -> Vec<(String, String)>;
    fn path_dirs(&self) -> Vec<PathBuf>;
    fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)>;
    fn read_ps_metadata(&self) -> Option<Vec<u8>>;
    fn spec_files(&self) -> Vec<(String, String)>;
    fn user_config(&self) -> UserConfig {
        UserConfig::default()
    }
    fn shell_rc_files(&self, _dialect: ShellDialect) -> Vec<(String, String)> {
        Vec::new()
    }
    fn project_task_specs(&self) -> Vec<Entry> {
        Vec::new()
    }
}

struct OsEnv;

impl EnvSource for OsEnv {
    fn env_vars(&self) -> Vec<(String, String)> {
        env::vars().collect()
    }

    fn path_dirs(&self) -> Vec<PathBuf> {
        collect_path_dirs_with_prefix(
            env::var_os("PATH").as_deref(),
            env::var_os("PREFIX").as_deref(),
        )
    }

    fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(128);
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = executable_name(&entry, &path) {
                if is_safe_command_token(name) {
                    let name = name.to_string();
                    out.push((name, path));
                }
            }
        }
        out
    }

    fn read_ps_metadata(&self) -> Option<Vec<u8>> {
        read_bounded_bytes(&ps_metadata_path(), MAX_PS_METADATA_BYTES)
    }

    fn spec_files(&self) -> Vec<(String, String)> {
        load_specs_from_dirs(&spec_search_dirs())
    }

    fn user_config(&self) -> UserConfig {
        load_user_config()
    }

    fn shell_rc_files(&self, dialect: ShellDialect) -> Vec<(String, String)> {
        collect_shell_rc_files(dialect)
    }

    fn project_task_specs(&self) -> Vec<Entry> {
        env::current_dir()
            .ok()
            .map(|cwd| discover_project_task_specs(&cwd))
            .unwrap_or_default()
    }
}

/// Returns the command name for a PATH entry if it is a launchable executable on this platform.
fn executable_name<'a>(entry: &fs::DirEntry, path: &'a Path) -> Option<&'a str> {
    #[cfg(windows)]
    {
        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            return None;
        }
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
        if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
            return None;
        }
        // `metadata()` follows symlinks (most of /usr/bin on Termux & Debian are links).
        let meta = fs::metadata(path).ok()?;
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
    vars.sort_by(|a, b| cmp_ascii_case_insensitive(&a.0, &b.0));
    let list_sep = dialect.path_sep();

    let mut children = Vec::with_capacity(vars.len());
    for (k, v) in vars {
        // Windows exposes hidden per-drive CWD entries like `=C:` and `=ExitCode` in env blocks;
        // also skip hostile env names that cannot be safely emitted as shell variable tokens.
        if k.starts_with('=') || !is_valid_env_var_name(&k) {
            continue;
        }
        let clean_v = sanitize_single_line(&v);
        let token = dialect.env_token(&k);
        let mut entry = Entry::new(&token, &k, EntryKind::Env, clean_v.as_ref());

        if dialect.is_path_list(&k, clean_v.as_ref()) {
            for part in clean_v.split(list_sep).filter(|s| !s.is_empty()) {
                if let Some(quoted) = quote_shell_literal(dialect, part) {
                    entry.children.push(Entry::new(
                        quoted,
                        part,
                        EntryKind::Value,
                        format!("Path entry in {token}"),
                    ));
                }
            }
        } else {
            entry.children.push(Entry::new(
                &token,
                format!("Insert {token}"),
                EntryKind::Var,
                format!("Current value: {clean_v}"),
            ));
            if let Some(quoted) = quote_shell_literal(dialect, clean_v.as_ref()) {
                entry.children.push(Entry::new(
                    quoted,
                    format!("Literal: {clean_v}"),
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
        let sanitized = sanitize_single_line(branch.trim());
        let trimmed = sanitized.trim();
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
        let val = sanitize_single_line(&child.desc).into_owned();
        child.children.push(Entry::new(
            &tok,
            format!("Insert {tok}"),
            EntryKind::Var,
            format!("Value: {val}"),
        ));
        if child.has_literal {
            if let Some(quoted) = quote_shell_literal(dialect, &val) {
                child.children.push(Entry::new(
                    quoted,
                    format!("Literal: {val}"),
                    EntryKind::Value,
                    format!("Insert literal path/value of {tok}"),
                ));
            }
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

fn child_dedup_key(c: &Entry) -> (bool, String) {
    if c.is_placeholder() {
        (true, c.label.to_ascii_lowercase())
    } else {
        (false, c.text.to_ascii_lowercase())
    }
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
            let mut existing: HashSet<(bool, String)> =
                target.children.iter().map(child_dedup_key).collect();
            for child in spec.children {
                if existing.insert(child_dedup_key(&child)) {
                    target.children.push(child);
                }
            }
        }
    }
}

fn merge_project_task_spec_into_entry(target: &mut Entry, task_spec: Entry) {
    if target.desc.is_empty() && !task_spec.desc.is_empty() {
        target.desc = task_spec.desc;
    }
    merge_task_entries_front(&mut target.children, task_spec.children);
}

pub(super) fn load_catalog_with_context(
    dialect: ShellDialect,
    env_src: &dyn EnvSource,
    ctx: &RuntimeContext,
) -> Vec<Entry> {
    let mut list = Vec::with_capacity(5000);
    let mut seen = HashSet::with_capacity(2048);

    // 0. Active shell session history at the top of Pane 1
    let mut hist_seen = HashSet::with_capacity(ctx.history.len());
    for cmd in &ctx.history {
        let clean = sanitize_single_line(cmd.trim());
        let trimmed = clean.trim();
        if trimmed.is_empty() || !hist_seen.insert(trimmed.to_ascii_lowercase()) {
            continue;
        }
        list.push(Entry::new(
            trimmed,
            trimmed,
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

    // 2. Load exported PowerShell metadata (or POSIX/Fish aliases & functions)
    let user_cfg = env_src.user_config();
    let disabled: HashSet<String> = user_cfg
        .disabled_tools
        .iter()
        .map(|s| s.to_ascii_lowercase())
        .collect();

    let mut ps_map: HashMap<String, Entry> =
        HashMap::with_capacity(if dialect.is_powershell() { 1024 } else { 64 });
    let mut ps_order: Vec<String> =
        Vec::with_capacity(if dialect.is_powershell() { 1024 } else { 64 });
    let defs = if dialect.is_powershell() {
        env_src
            .read_ps_metadata()
            .map(|bytes| parse_ps_metadata_entries(&bytes))
            .unwrap_or_default()
    } else {
        parse_posix_fish_definitions(dialect, &env_src.shell_rc_files(dialect))
    };
    for def in defs {
        let key = def.text.to_ascii_lowercase();
        if disabled.contains(&key) {
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(e) = ps_map.entry(key.clone()) {
            ps_order.push(key);
            e.insert(def);
        }
    }

    // 3. Scan PATH directories for CLI executables
    //    (Windows: .exe / .cmd / .bat; Unix: any file with an execute bit)
    let mut cli_map: HashMap<String, Entry> = HashMap::with_capacity(2048);
    let mut cli_order: Vec<String> = Vec::with_capacity(2048);
    let mut seen_path_dirs = HashSet::new();
    for dir in env_src.path_dirs() {
        let dir_key: Cow<'_, str> = if cfg!(windows) {
            let s = dir.to_string_lossy();
            if s.bytes().any(|b| b.is_ascii_uppercase()) {
                Cow::Owned(s.to_ascii_lowercase())
            } else {
                s
            }
        } else {
            dir.to_string_lossy()
        };
        if seen_path_dirs.contains(dir_key.as_ref()) {
            continue;
        }
        seen_path_dirs.insert(dir_key.into_owned());
        for (name, path) in env_src.dir_executables(&dir) {
            if !is_safe_command_token(&name) {
                continue;
            }
            let has_upper = name.bytes().any(|b| b.is_ascii_uppercase());
            let key: Cow<'_, str> = if has_upper {
                Cow::Owned(name.to_ascii_lowercase())
            } else {
                Cow::Borrowed(name.as_str())
            };
            if disabled.contains(key.as_ref()) || cli_map.contains_key(key.as_ref()) {
                continue;
            }
            let owned_key = key.into_owned();
            cli_order.push(owned_key.clone());
            cli_map.insert(
                owned_key,
                Entry::new(
                    name.clone(),
                    name,
                    EntryKind::Cli,
                    path.display().to_string(),
                ),
            );
        }
    }

    // 3b. Load declarative specs (carapace-spec / Fig) in strict precedence order
    //     so higher-precedence specs win cleanly without duplicate conflicts.
    let mut seen_spec_cmds: HashSet<String> = HashSet::new();
    for (file_name, content) in env_src.spec_files() {
        let Ok(spec_entry) = parse_spec_str(&file_name, &content) else {
            continue;
        };
        let key = spec_entry.text.to_ascii_lowercase();
        if disabled.contains(&key) || !seen_spec_cmds.insert(key.clone()) {
            continue;
        }
        if let Some(cli_entry) = cli_map.get_mut(&key) {
            merge_spec_into_entry(cli_entry, spec_entry);
        } else if let Some(ps_entry) = ps_map.get_mut(&key) {
            merge_spec_into_entry(ps_entry, spec_entry);
        } else {
            cli_order.push(key.clone());
            cli_map.insert(key, spec_entry);
        }
    }

    // 3c. Merge project-local task targets (justfile, Makefile, package.json, mise.toml)
    //     from the active working directory so Pane 2/3 immediately surfaces them.
    for task_spec in env_src.project_task_specs() {
        let key = task_spec.text.to_ascii_lowercase();
        if disabled.contains(&key) {
            continue;
        }
        if let Some(cli_entry) = cli_map.get_mut(&key) {
            merge_project_task_spec_into_entry(cli_entry, task_spec);
        } else if let Some(ps_entry) = ps_map.get_mut(&key) {
            merge_project_task_spec_into_entry(ps_entry, task_spec);
        } else {
            cli_order.push(key.clone());
            cli_map.insert(key, task_spec);
        }
    }

    // Enrich `git` description with detected current branch if available
    if let Some(branch) = ctx
        .git_branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty())
    {
        let clean_branch = sanitize_single_line(branch);
        if !clean_branch.is_empty() {
            if let Some(git_entry) = cli_map.get_mut("git") {
                git_entry.desc = format!("[branch: {clean_branch}] {}", git_entry.desc);
            }
        }
    }

    // 4. Priority Favorites at the top (user-configured first, then built-in list)
    for p in user_cfg
        .priority_tools
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .chain(PRIORITY_CLI_TOOLS.iter().copied())
    {
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
    use super::super::config::parse_user_config_toml;
    use super::super::powershell::PsCommandExport;
    use super::*;
    use crate::model::{CwdEntry, EntryRole};

    struct FakeEnv {
        vars: Vec<(String, String)>,
        path_dirs: Vec<PathBuf>,
        bins: HashMap<PathBuf, Vec<(String, PathBuf)>>,
        ps_metadata: Option<Vec<u8>>,
        specs: Vec<(String, String)>,
        user_config: UserConfig,
        shell_rc: Vec<(String, String)>,
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

        fn user_config(&self) -> UserConfig {
            self.user_config.clone()
        }

        fn shell_rc_files(&self, _dialect: ShellDialect) -> Vec<(String, String)> {
            self.shell_rc.clone()
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
            user_config: UserConfig::default(),
            shell_rc: Vec::new(),
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
            user_config: UserConfig::default(),
            shell_rc: Vec::new(),
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
    fn test_catalog_priority_and_disabled_overrides_from_user_config() {
        let cfg = UserConfig {
            priority_tools: vec!["mycli".to_string(), "gs".to_string()],
            disabled_tools: vec!["blocked".to_string()],
            history_limit: Some(25),
            ..UserConfig::default()
        };

        let bin_dir = PathBuf::from("/fake/bin");
        let fake = FakeEnv {
            vars: vec![],
            path_dirs: vec![bin_dir.clone()],
            bins: HashMap::from([(
                bin_dir,
                vec![
                    ("git".to_string(), PathBuf::from("/fake/bin/git")),
                    ("blocked".to_string(), PathBuf::from("/fake/bin/blocked")),
                    ("mycli".to_string(), PathBuf::from("/fake/bin/mycli")),
                ],
            )]),
            ps_metadata: None,
            specs: Vec::new(),
            user_config: cfg,
            shell_rc: vec![(".zshrc".to_string(), "alias gs='git status'".to_string())],
        };

        let catalog = load_catalog_with(ShellDialect::Posix, &fake);
        let texts: Vec<&str> = catalog.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, vec!["$", "$", "mycli", "gs", "git"]);
        assert!(!texts.contains(&"blocked"));
    }

    #[test]
    fn test_merge_spec_into_entry_preserves_distinct_placeholders_and_filters_windows_drive_env() {
        let mut target = Entry::new("tool", "tool", EntryKind::Cli, "/bin/tool");
        target.children = vec![
            Entry::placeholder("File", EntryKind::Arg, "Path"),
            Entry::new("--help", "--help", EntryKind::Flag, "Help"),
        ];

        let mut spec = Entry::new("tool", "tool", EntryKind::Cli, "Spec desc");
        spec.children = vec![
            Entry::placeholder("File", EntryKind::Arg, "Duplicate file"),
            Entry::placeholder("Dir", EntryKind::Arg, "Directory"),
            Entry::new("--help", "--help", EntryKind::Flag, "Dup help"),
            Entry::new("--verbose", "--verbose", EntryKind::Flag, "Verbose"),
        ];

        merge_spec_into_entry(&mut target, spec);
        let child_summary: Vec<(EntryRole, &str, &str)> = target
            .children
            .iter()
            .map(|c| (c.role, c.text.as_str(), c.label.as_str()))
            .collect();
        assert_eq!(
            child_summary,
            vec![
                (EntryRole::Placeholder, "", "File"),
                (EntryRole::Token, "--help", "--help"),
                (EntryRole::Placeholder, "", "Dir"),
                (EntryRole::Token, "--verbose", "--verbose"),
            ]
        );

        let env_root = build_env_scope_entry_from_vars(
            ShellDialect::PowerShell,
            vec![
                ("=C:".to_string(), "C:\\work".to_string()),
                ("=ExitCode".to_string(), "00000000".to_string()),
                ("PATH".to_string(), "C:\\bin".to_string()),
            ],
        );
        let var_labels: Vec<&str> = env_root.children.iter().map(|c| c.label.as_str()).collect();
        assert_eq!(var_labels, vec!["PATH"]);
    }

    #[test]
    fn test_shell_dialect_and_config_toml_edge_cases() {
        assert_eq!(
            ShellDialect::from_env(Some("  dash  "), false, false, None, false),
            ShellDialect::Posix
        );
        assert_eq!(
            ShellDialect::from_env(Some("ksh"), false, false, None, false),
            ShellDialect::Posix
        );
        assert_eq!(
            ShellDialect::from_env(None, false, false, Some("fish"), false),
            ShellDialect::Fish
        );

        let cfg = parse_user_config_toml(
            "\u{feff}history_limit = 1_500\npriority_tools = [\" mycli \"]\n",
        );
        assert_eq!(cfg.history_limit, Some(1500));

        let dir = PathBuf::from("/fake/bin");
        let fake = FakeEnv {
            vars: vec![],
            path_dirs: vec![dir.clone(), dir.clone()],
            bins: HashMap::from([(
                dir,
                vec![
                    ("git".to_string(), PathBuf::from("/fake/bin/git")),
                    ("mycli".to_string(), PathBuf::from("/fake/bin/mycli")),
                ],
            )]),
            ps_metadata: None,
            specs: Vec::new(),
            user_config: cfg,
            shell_rc: Vec::new(),
        };
        let ctx = RuntimeContext {
            git_branch: Some("   ".to_string()),
            ..RuntimeContext::default()
        };
        let catalog = load_catalog_with_context(ShellDialect::Posix, &fake, &ctx);
        let git_entry = catalog.iter().find(|e| e.text == "git").unwrap();
        assert_eq!(git_entry.desc, "/fake/bin/git");
        let cli_count = catalog.iter().filter(|e| e.text == "mycli").count();
        assert_eq!(cli_count, 1);
    }

    #[test]
    fn test_env_scope_filters_unsafe_names_and_quotes_literal_values_safely() {
        let hostile_vars = vec![
            ("FOO;id".to_string(), "1".to_string()),
            ("$(whoami)".to_string(), "2".to_string()),
            ("1BAD".to_string(), "3".to_string()),
            ("GOOD\nBAD".to_string(), "4".to_string()),
            (
                "SAFE_TOKEN".to_string(),
                "a'b\\c$(id)`id`\r\n\0\u{202e}".to_string(),
            ),
        ];

        let ps_root =
            build_env_scope_entry_from_vars(ShellDialect::PowerShell, hostile_vars.clone());
        assert_eq!(ps_root.children.len(), 1);
        assert_eq!(ps_root.children[0].label, "SAFE_TOKEN");
        assert_eq!(ps_root.children[0].text, "$env:SAFE_TOKEN");
        assert_eq!(ps_root.children[0].desc, "a'b\\c$(id)`id`");
        assert_eq!(ps_root.children[0].children[1].text, "'a''b\\c$(id)`id`'");

        let posix_root = build_env_scope_entry_from_vars(ShellDialect::Posix, hostile_vars.clone());
        assert_eq!(posix_root.children.len(), 1);
        assert_eq!(posix_root.children[0].text, "$SAFE_TOKEN");
        assert_eq!(
            posix_root.children[0].children[1].text,
            "'a'\\''b\\c$(id)`id`'"
        );

        let fish_root = build_env_scope_entry_from_vars(ShellDialect::Fish, hostile_vars);
        assert_eq!(fish_root.children.len(), 1);
        assert_eq!(fish_root.children[0].text, "$SAFE_TOKEN");
        assert_eq!(
            fish_root.children[0].children[1].text,
            "'a\\'b\\\\c$(id)`id`'"
        );
    }

    #[test]
    fn test_cross_distro_prefix_path_dedup_and_skips_relative_entries() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("zpane_test_path_{}_{nonce}", std::process::id()));
        let prefix = root.join("termux_usr");
        let prefix_bin = prefix.join("bin");
        let prefix_applets = prefix_bin.join("applets");
        let extra_bin = root.join("extra_bin");
        fs::create_dir_all(&prefix_applets).unwrap();
        fs::create_dir_all(&extra_bin).unwrap();

        let joined_path =
            env::join_paths([Path::new("."), Path::new("rel/bin"), &extra_bin, &extra_bin])
                .unwrap();
        let dirs = collect_path_dirs_with_prefix(Some(&joined_path), Some(prefix.as_os_str()));

        assert_eq!(dirs, vec![extra_bin.clone(), prefix_bin, prefix_applets]);

        #[cfg(unix)]
        {
            let usr_merge_link = root.join("bin_link");
            std::os::unix::fs::symlink(&extra_bin, &usr_merge_link).unwrap();
            let linked_path = env::join_paths([&extra_bin, &usr_merge_link]).unwrap();
            let merged = collect_path_dirs_with_prefix(Some(&linked_path), None);
            assert_eq!(merged, vec![extra_bin.clone()]);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_bounded_reads_and_secure_cache_permissions() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!("zpane_test_sec_{}_{nonce}", std::process::id()));
        let zpane_cache = root.join("zpane");
        let cache_file = zpane_cache.join("ps_metadata.json");
        let small_file = root.join("small.txt");
        let oversized_file = root.join("huge.txt");

        write_secure_file(&cache_file, b"{\"ok\":true}").unwrap();
        assert_eq!(
            read_bounded_bytes(&cache_file, 1024).unwrap(),
            b"{\"ok\":true}"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&zpane_cache).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(&cache_file).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        fs::write(&small_file, b"hello").unwrap();
        fs::write(&oversized_file, vec![b'x'; 64]).unwrap();
        assert!(read_bounded_bytes(&small_file, 16).is_some());
        assert!(read_bounded_bytes(&oversized_file, 16).is_none());
        assert!(read_bounded_bytes(&root, 1024).is_none());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_pentest_path_catalog_filters_hostile_entries() {
        // PATH catalog scan: deduplicates mixed/lowercase tools, obeys case-insensitive
        // disabled_tools, and rejects option/control/BiDi/oversized command tokens.
        let dir1 = PathBuf::from("/fake/bin1");
        let dir2 = PathBuf::from("/fake/bin2");
        let oversized = "x".repeat(300);
        let fake = FakeEnv {
            vars: vec![],
            path_dirs: vec![dir1.clone(), dir2.clone(), dir1.clone()],
            bins: HashMap::from([
                (
                    dir1,
                    vec![
                        ("git".to_string(), PathBuf::from("/fake/bin1/git")),
                        ("MyTool".to_string(), PathBuf::from("/fake/bin1/MyTool")),
                        ("-evil".to_string(), PathBuf::from("/fake/bin1/-evil")),
                        (".".to_string(), PathBuf::from("/fake/bin1/.")),
                        ("..".to_string(), PathBuf::from("/fake/bin1/..")),
                        (
                            "cmd\ninject".to_string(),
                            PathBuf::from("/fake/bin1/cmd_nl"),
                        ),
                        ("cmd\0nul".to_string(), PathBuf::from("/fake/bin1/cmd_nul")),
                        (
                            "cmd\u{202E}bidi".to_string(),
                            PathBuf::from("/fake/bin1/cmd_bidi"),
                        ),
                        ("two words".to_string(), PathBuf::from("/fake/bin1/spaces")),
                        (oversized, PathBuf::from("/fake/bin1/oversized")),
                        (
                            "BlockedTool".to_string(),
                            PathBuf::from("/fake/bin1/BlockedTool"),
                        ),
                    ],
                ),
                (
                    dir2,
                    vec![
                        ("Git".to_string(), PathBuf::from("/fake/bin2/Git")),
                        ("mytool".to_string(), PathBuf::from("/fake/bin2/mytool")),
                        (
                            "blockedtool".to_string(),
                            PathBuf::from("/fake/bin2/blockedtool"),
                        ),
                        (
                            "lowercase_only".to_string(),
                            PathBuf::from("/fake/bin2/lowercase_only"),
                        ),
                    ],
                ),
            ]),
            ps_metadata: None,
            specs: Vec::new(),
            user_config: UserConfig {
                disabled_tools: vec!["blockedtool".to_string()],
                ..UserConfig::default()
            },
            shell_rc: Vec::new(),
        };

        let catalog = load_catalog_with(ShellDialect::Posix, &fake);
        let cli_names: Vec<&str> = catalog
            .iter()
            .filter(|e| e.kind == EntryKind::Cli)
            .map(|e| e.text.as_str())
            .collect();
        assert_eq!(cli_names, vec!["git", "MyTool", "lowercase_only"]);
    }

    #[test]
    fn test_declarative_spec_precedence_zpane_over_carapace_over_legacy_cache() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "zpane_spec_prec_test_{}_{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);

        let zpane_specs = root.join("config").join("zpane").join("specs");
        let carapace_specs = root.join("config").join("carapace").join("specs");
        let cache_specs = root.join("cache").join("zpane").join("specs");
        fs::create_dir_all(&zpane_specs).unwrap();
        fs::create_dir_all(&carapace_specs).unwrap();
        fs::create_dir_all(&cache_specs).unwrap();

        // Tier 1 (highest): overrides `kubectl`, has invalid YAML for `helm` (should fall back to Tier 2)
        fs::write(
            zpane_specs.join("kubectl.yaml"),
            "name: kubectl\ndescription: Zpane kubectl\nflags:\n  --context=: Zpane context\ncommands:\n  - name: apply\n    description: Apply manifests\n",
        )
        .unwrap();
        fs::write(zpane_specs.join("helm.yaml"), "name: helm\n\tbroken: tab\n").unwrap();

        // Tier 2 (carapace): conflicting `kubectl` (must lose), valid `helm` (must win over Tier 3)
        fs::write(
            carapace_specs.join("kubectl.json"),
            r#"{"name":"kubectl","description":"Carapace kubectl","flags":{"--context":"Lower flag","--carapace-only":"Should not leak"}}"#,
        )
        .unwrap();
        fs::write(
            carapace_specs.join("helm.yaml"),
            "name: helm\ndescription: Carapace helm\ncommands:\n  - name: upgrade\n    description: Upgrade release\n",
        )
        .unwrap();

        // Tier 3 (legacy cache): conflicting `kubectl`/`helm` (must lose), unique `legacytool` (must load)
        fs::write(
            cache_specs.join("kubectl.yaml"),
            "name: kubectl\ndescription: Legacy kubectl\nflags:\n  --legacy: Stale flag\n",
        )
        .unwrap();
        fs::write(
            cache_specs.join("helm.yaml"),
            "name: helm\ndescription: Legacy helm\n",
        )
        .unwrap();
        fs::write(
            cache_specs.join("legacytool.yaml"),
            "name: legacytool\ndescription: Legacy cache tool\nflags:\n  --old: Legacy option\n",
        )
        .unwrap();

        // Passing duplicate dir path must not scan it twice
        let loaded_specs = load_specs_from_dirs(&[
            zpane_specs.clone(),
            zpane_specs,
            carapace_specs,
            cache_specs,
        ]);

        let bin_dir = PathBuf::from("/fake/bin");
        let fake = FakeEnv {
            vars: vec![],
            path_dirs: vec![bin_dir.clone()],
            bins: HashMap::from([(
                bin_dir,
                vec![("kubectl".to_string(), PathBuf::from("/fake/bin/kubectl"))],
            )]),
            ps_metadata: None,
            specs: loaded_specs,
            user_config: UserConfig::default(),
            shell_rc: Vec::new(),
        };

        let catalog = load_catalog_with(ShellDialect::Posix, &fake);

        let kubectl = catalog
            .iter()
            .find(|e| e.text == "kubectl")
            .expect("kubectl in catalog");
        assert!(kubectl.desc.contains("Zpane kubectl"));
        assert!(!kubectl.desc.contains("Carapace kubectl"));
        assert!(!kubectl.desc.contains("Legacy kubectl"));
        assert_eq!(
            kubectl
                .children
                .iter()
                .filter(|c| c.text == "--context")
                .count(),
            1
        );
        let ctx_param = kubectl
            .children
            .iter()
            .find(|c| c.text == "--context")
            .unwrap();
        assert_eq!(ctx_param.kind, EntryKind::Param);
        assert_eq!(ctx_param.desc, "Zpane context");
        assert!(kubectl.children.iter().any(|c| c.text == "apply"));
        assert!(!kubectl.children.iter().any(|c| c.text == "--carapace-only"));
        assert!(!kubectl.children.iter().any(|c| c.text == "--legacy"));

        let helm = catalog
            .iter()
            .find(|e| e.text == "helm")
            .expect("helm falls back to Tier 2 when Tier 1 spec is corrupt");
        assert_eq!(helm.desc, "Carapace helm");
        assert!(helm.children.iter().any(|c| c.text == "upgrade"));

        let legacy = catalog
            .iter()
            .find(|e| e.text == "legacytool")
            .expect("legacytool loads from Tier 3 cache fallback");
        assert_eq!(legacy.desc, "Legacy cache tool");
        assert!(legacy.children.iter().any(|c| c.text == "--old"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_catalog_merges_project_tasks_with_declarative_specs_and_path_commands() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let proj_dir = env::temp_dir().join(format!(
            "zpane_cat_proj_tasks_{}_{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&proj_dir);
        fs::create_dir_all(&proj_dir).unwrap();
        fs::write(
            proj_dir.join("package.json"),
            r#"{"scripts":{"dev":"vite","test":"vitest"}}"#,
        )
        .unwrap();
        fs::write(proj_dir.join("justfile"), "build:\n    cargo build\n").unwrap();

        struct ProjectFakeEnv {
            inner: FakeEnv,
            project_dir: PathBuf,
        }
        impl EnvSource for ProjectFakeEnv {
            fn env_vars(&self) -> Vec<(String, String)> {
                self.inner.env_vars()
            }
            fn path_dirs(&self) -> Vec<PathBuf> {
                self.inner.path_dirs()
            }
            fn dir_executables(&self, dir: &Path) -> Vec<(String, PathBuf)> {
                self.inner.dir_executables(dir)
            }
            fn read_ps_metadata(&self) -> Option<Vec<u8>> {
                self.inner.read_ps_metadata()
            }
            fn spec_files(&self) -> Vec<(String, String)> {
                self.inner.spec_files()
            }
            fn user_config(&self) -> UserConfig {
                self.inner.user_config()
            }
            fn shell_rc_files(&self, dialect: ShellDialect) -> Vec<(String, String)> {
                self.inner.shell_rc_files(dialect)
            }
            fn project_task_specs(&self) -> Vec<Entry> {
                discover_project_task_specs(&self.project_dir)
            }
        }

        let npm_spec = r#"
name: npm
description: Node package manager
commands:
  - name: install
    description: Install dependencies
  - name: run
    description: Run script
    flags:
      --if-present: Skip missing script
"#;
        let bin_dir = PathBuf::from("/fake/bin");
        let fake = ProjectFakeEnv {
            inner: FakeEnv {
                vars: vec![],
                path_dirs: vec![bin_dir.clone()],
                bins: HashMap::from([(
                    bin_dir,
                    vec![
                        ("npm".to_string(), PathBuf::from("/fake/bin/npm")),
                        ("just".to_string(), PathBuf::from("/fake/bin/just")),
                    ],
                )]),
                ps_metadata: None,
                specs: vec![("npm.yaml".to_string(), npm_spec.to_string())],
                user_config: UserConfig::default(),
                shell_rc: Vec::new(),
            },
            project_dir: proj_dir.clone(),
        };

        let catalog = load_catalog_with(ShellDialect::Posix, &fake);

        let npm = catalog.iter().find(|e| e.text == "npm").expect("npm");
        assert!(npm.children.iter().any(|c| c.text == "install"));
        assert_eq!(npm.children.iter().filter(|c| c.text == "run").count(), 1);
        let npm_run = npm.walk_chain(&["run"]).expect("npm run");
        let run_texts: Vec<&str> = npm_run.children.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(run_texts, vec!["dev", "test", "--if-present"]);

        let just = catalog.iter().find(|e| e.text == "just").expect("just");
        assert!(just.children.iter().any(|c| c.text == "build"));

        let _ = fs::remove_dir_all(&proj_dir);
    }
}
