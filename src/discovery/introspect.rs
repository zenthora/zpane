use super::catalog::{
    cache_dir, collect_path_dirs_with_prefix, contains_ignore_ascii_case, hide_window,
    load_catalog, ps_metadata_path, read_bounded_bytes, sanitize_multiline_desc,
    sanitize_single_line, sync_powershell_metadata, write_secure_file, ShellDialect,
    PRIORITY_CLI_TOOLS,
};
use super::help_parser::parse_cli_help;
use crate::model::{Entry, EntryKind, RuntimeContext};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Debug, Clone)]
struct BgRequest {
    cache_key: String,
    cmd_chain: Vec<String>,
    generation: u64,
}

#[derive(Debug, Clone)]
struct BgResponse {
    cache_key: String,
    entries: Vec<Entry>,
    generation: u64,
}

/// Asynchronous updates surfaced by [`Introspect::poll_updates`].
#[derive(Debug, Default, Clone)]
pub struct PollResult {
    /// `true` when one or more command-chain introspection queries finished.
    pub introspected: bool,
    /// `Some(entries)` when a background catalog metadata refresh (e.g. first-run PowerShell sync) finished.
    pub catalog_reloaded: Option<Vec<Entry>>,
}

impl PollResult {
    /// Returns `true` when any background work produced new UI state.
    pub fn any(&self) -> bool {
        self.introspected || self.catalog_reloaded.is_some()
    }
}

/// Seam for CLI subcommand/flag introspection and background catalog refreshes used by [`MillerSession`].
pub trait Introspect {
    fn query(&mut self, chain: &[&str]) -> Vec<Entry>;
    fn poll_updates(&mut self) -> PollResult;
    fn flush(&mut self) -> bool;
    fn invalidate_command(&mut self, _exe: &str) {}
}

const CLI_CACHE_SCHEMA_VERSION: u32 = 3;
const MAX_CLI_CACHE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CMD_OUTPUT_BYTES: usize = 512 * 1024;
const MAX_CARAPACE_ENTRIES: usize = 1_024;
const MAX_CHAIN_DEPTH: usize = 8;
const MAX_INSPECT_INPUT_BYTES: usize = 4_096;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedChainRecord {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    dialect: Option<String>,
    #[serde(default)]
    exe_fingerprint: Option<(u64, u64)>,
    entries: Vec<Entry>,
}

#[derive(Serialize)]
struct CachedChainRecordRef<'a> {
    schema_version: u32,
    dialect: &'a str,
    exe_fingerprint: Option<(u64, u64)>,
    entries: &'a [Entry],
}

fn dialect_cache_tag(dialect: ShellDialect) -> &'static str {
    match dialect {
        ShellDialect::PowerShell => "pwsh",
        ShellDialect::Posix => "posix",
        ShellDialect::Fish => "fish",
    }
}

fn has_shell_metachar_or_control(s: &str) -> bool {
    s.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '\0' | '\u{061c}'
                    | '\u{200b}'..='\u{200f}'
                    | '\u{2028}'
                    | '\u{2029}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
                    | '&'
                    | '|'
                    | ';'
                    | '<'
                    | '>'
                    | '$'
                    | '`'
                    | '"'
                    | '\''
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '*'
                    | '?'
                    | '!'
                    | '%'
                    | '^'
            )
    })
}

fn is_safe_exe_token(exe: &str) -> bool {
    let trimmed = exe.trim();
    if trimmed.is_empty() || trimmed.len() > 260 || trimmed.starts_with('-') {
        return false;
    }
    if has_shell_metachar_or_control(trimmed) || trimmed.contains("..") {
        return false;
    }
    if trimmed.starts_with("\\\\?\\UNC\\")
        || trimmed.starts_with("//")
        || (trimmed.starts_with("\\\\") && !trimmed.starts_with("\\\\?\\"))
    {
        return false;
    }
    let path = Path::new(trimmed);
    if path.is_absolute() {
        return !path.components().any(|c| {
            matches!(c, Component::ParentDir)
                || matches!(c, Component::Prefix(p) if matches!(
                    p.kind(),
                    std::path::Prefix::UNC(..)
                        | std::path::Prefix::VerbatimUNC(..)
                        | std::path::Prefix::DeviceNS(..)
                ))
        });
    }
    if trimmed.starts_with('.')
        || trimmed
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, ':' | '/' | '\\'))
    {
        return false;
    }
    let mut comps = path.components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

fn is_safe_catalog_command_token(tok: &str) -> bool {
    let trimmed = tok.trim();
    let is_windows_drive_prefix = trimmed.as_bytes().get(1) == Some(&b':')
        && trimmed
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic);
    !trimmed.is_empty()
        && trimmed.len() <= 260
        && !is_windows_drive_prefix
        && !trimmed.starts_with(['-', '.', '/', '\\'])
        && !trimmed.contains("..")
        && !trimmed
            .chars()
            .any(|c| c.is_whitespace() || matches!(c, '/' | '\\'))
        && !has_shell_metachar_or_control(trimmed)
}

fn is_safe_sub_arg(arg: &str) -> bool {
    let trimmed = arg.trim();
    if trimmed.is_empty()
        || trimmed.len() > 128
        || trimmed.starts_with(['-', '/', '\\', '.'])
        || trimmed.contains("..")
        || trimmed.chars().any(|c| c.is_whitespace() || c == '=')
        || has_shell_metachar_or_control(trimmed)
    {
        return false;
    }
    true
}

fn normalize_and_validate_chain<'a>(
    chain: &[&'a str],
    out: &mut [&'a str; MAX_CHAIN_DEPTH],
) -> Option<usize> {
    let mut len = 0usize;
    for raw in chain {
        let s = raw.trim();
        if s.is_empty() {
            continue;
        }
        if len >= MAX_CHAIN_DEPTH {
            return None;
        }
        let valid = if len == 0 {
            is_safe_exe_token(s)
        } else {
            is_safe_sub_arg(s)
        };
        if !valid {
            return None;
        }
        out[len] = s;
        len += 1;
    }
    if len == 0 {
        None
    } else {
        Some(len)
    }
}

fn validate_cache_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 512 {
        return false;
    }
    let mut parts = key.split("::");
    let Some(root) = parts.next() else {
        return false;
    };
    if !is_safe_exe_token(root) {
        return false;
    }
    for (count, part) in (1usize..).zip(parts) {
        if count >= MAX_CHAIN_DEPTH || !is_safe_sub_arg(part) {
            return false;
        }
    }
    true
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (meta.permissions().mode() & 0o111) != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_executable_in_dirs(trimmed: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    if !is_safe_exe_token(trimmed) {
        return None;
    }
    let direct_path = Path::new(trimmed);
    let has_ext = direct_path.extension().is_some();
    if direct_path.is_absolute() {
        if is_executable_file(direct_path) {
            return Some(direct_path.to_path_buf());
        }
        if cfg!(windows) && !has_ext {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = PathBuf::from(format!("{trimmed}.{ext}"));
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
        return None;
    }

    for dir in dirs {
        let direct = dir.join(trimmed);
        if is_executable_file(&direct) {
            return Some(direct);
        }
        if cfg!(windows) && !has_ext {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{trimmed}.{ext}"));
                if is_executable_file(&candidate) {
                    return Some(candidate);
                }
            }
        }
    }
    None
}

pub(super) fn find_executable_in_path(exe: &str) -> Option<PathBuf> {
    let trimmed = exe.trim();
    let dirs = collect_path_dirs_with_prefix(
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("PREFIX").as_deref(),
    );
    resolve_executable_in_dirs(trimmed, &dirs)
}

fn executable_fingerprint(exe: &str) -> Option<(u64, u64)> {
    let path = find_executable_in_path(exe)?;
    let meta = fs::metadata(path).ok()?;
    let secs = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some((secs, meta.len()))
}

fn cli_cache_path_for(dialect: ShellDialect) -> PathBuf {
    cache_dir().join(format!("cli_cache_{}.json", dialect_cache_tag(dialect)))
}

fn legacy_cli_cache_path() -> PathBuf {
    cache_dir().join("cli_cache.json")
}

/// Removes persisted CLI help (`cli_cache_*.json`, legacy `cli_cache.json`) and PowerShell metadata (`ps_metadata.json`) caches.
pub fn clear_disk_cache() -> anyhow::Result<usize> {
    let mut removed = 0usize;
    for path in [
        cli_cache_path_for(ShellDialect::PowerShell),
        cli_cache_path_for(ShellDialect::Posix),
        cli_cache_path_for(ShellDialect::Fish),
        legacy_cli_cache_path(),
        ps_metadata_path(),
    ] {
        if path.exists() {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn load_cli_disk_cache_with_fingerprint<F>(
    path: &Path,
    expected_dialect: Option<ShellDialect>,
    fp_fn: F,
) -> HashMap<String, Vec<Entry>>
where
    F: Fn(&str) -> Option<(u64, u64)>,
{
    let Some(bytes) = read_bounded_bytes(path, MAX_CLI_CACHE_BYTES) else {
        return HashMap::new();
    };
    let Ok(records) = serde_json::from_slice::<HashMap<String, CachedChainRecord>>(&bytes) else {
        return HashMap::new();
    };

    let expected_tag = expected_dialect.map(dialect_cache_tag);
    let mut out = HashMap::with_capacity(records.len());
    let mut fp_cache: HashMap<String, Option<(u64, u64)>> = HashMap::new();
    for (key, rec) in records {
        if rec.schema_version != CLI_CACHE_SCHEMA_VERSION || !validate_cache_key(&key) {
            continue;
        }
        if let (Some(expected), Some(saved_dialect)) = (expected_tag, rec.dialect.as_deref()) {
            if saved_dialect != expected {
                continue;
            }
        }
        if let Some(saved_fp) = rec.exe_fingerprint {
            if let Some(root_exe) = key.split("::").next() {
                let current_fp = if let Some(&cached_fp) = fp_cache.get(root_exe) {
                    cached_fp
                } else {
                    let computed = fp_fn(root_exe);
                    fp_cache.insert(root_exe.to_string(), computed);
                    computed
                };
                if current_fp != Some(saved_fp) {
                    continue;
                }
            }
        }
        out.insert(key, rec.entries);
    }
    out
}

fn load_cli_disk_cache_from(path: &Path, dialect: ShellDialect) -> HashMap<String, Vec<Entry>> {
    load_cli_disk_cache_with_fingerprint(path, Some(dialect), executable_fingerprint)
}

fn save_cli_disk_cache_to(path: &Path, dialect: ShellDialect, cache: &HashMap<String, Vec<Entry>>) {
    let mut records: HashMap<&str, CachedChainRecordRef<'_>> = HashMap::with_capacity(cache.len());
    let mut fp_cache: HashMap<&str, Option<(u64, u64)>> = HashMap::new();
    let tag = dialect_cache_tag(dialect);
    for (key, entries) in cache {
        let root_exe = key.split("::").next().unwrap_or(key.as_str());
        let fp = *fp_cache
            .entry(root_exe)
            .or_insert_with(|| executable_fingerprint(root_exe));
        records.insert(
            key.as_str(),
            CachedChainRecordRef {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                dialect: tag,
                exe_fingerprint: fp,
                entries: entries.as_slice(),
            },
        );
    }
    if let Ok(json) = serde_json::to_vec(&records) {
        let _ = write_secure_file(path, &json);
    }
}

fn read_pipe_bounded<R: Read>(mut reader: R, max_bytes: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(max_bytes.min(8192));
    let mut limited = reader.by_ref().take((max_bytes + 1) as u64);
    let _ = limited.read_to_end(&mut buf);
    if buf.len() > max_bytes {
        buf.truncate(max_bytes);
    }
    buf
}

fn run_command_with_timeout(exe_path: &Path, args: &[&str], timeout_ms: u64) -> String {
    let mut cmd = Command::new(exe_path);
    cmd.args(args)
        .env_remove("FORCE_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env("NO_COLOR", "1")
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
        stdout_pipe
            .take()
            .map(|p| read_pipe_bounded(p, MAX_CMD_OUTPUT_BYTES))
            .unwrap_or_default()
    });

    let err_handle = thread::spawn(move || {
        stderr_pipe
            .take()
            .map(|p| read_pipe_bounded(p, MAX_CMD_OUTPUT_BYTES))
            .unwrap_or_default()
    });

    let start = Instant::now();
    let limit = Duration::from_millis(timeout_ms);
    let mut poll_ms = 1u64;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                let elapsed = start.elapsed();
                if elapsed >= limit {
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                let remaining = limit - elapsed;
                thread::sleep(Duration::from_millis(poll_ms).min(remaining));
                poll_ms = (poll_ms * 2).min(8);
            }
            Err(_) => break,
        }
    }

    let out_bytes = out_handle.join().unwrap_or_default();
    let err_bytes = err_handle.join().unwrap_or_default();
    let mut combined = String::from_utf8_lossy(&out_bytes).into_owned();
    if !err_bytes.is_empty() {
        if !combined.is_empty() {
            combined.push('\n');
        }
        combined.push_str(&String::from_utf8_lossy(&err_bytes));
    }
    combined
}

/// Adapter signature for running a CLI command (`exe`, `args`) and returning combined help text.
type CommandRunner = dyn Fn(&str, &[&str]) -> Option<String> + Send + Sync;

fn default_command_runner() -> Arc<CommandRunner> {
    Arc::new(|exe: &str, args: &[&str]| {
        if !is_safe_exe_token(exe) {
            return None;
        }
        let resolved = find_executable_in_path(exe)?;
        let out = run_command_with_timeout(&resolved, args, 450);
        if out.trim().is_empty() {
            None
        } else {
            Some(out)
        }
    })
}

#[derive(Debug, Deserialize)]
struct CarapaceExport {
    #[serde(default)]
    values: Vec<CarapaceValue>,
}

#[derive(Debug, Deserialize)]
struct CarapaceValue {
    value: String,
    #[serde(default)]
    display: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    tag: String,
}

fn is_carapace_installed() -> bool {
    find_executable_in_path("carapace").is_some()
}

fn parse_carapace_export(json_str: &str) -> Vec<Entry> {
    let trimmed = json_str.trim();
    if trimmed.len() > MAX_CMD_OUTPUT_BYTES {
        return Vec::new();
    }
    let Some(start) = trimmed.find('{') else {
        return Vec::new();
    };
    let Ok(export) = serde_json::from_str::<CarapaceExport>(&trimmed[start..]) else {
        return Vec::new();
    };

    let cap = export.values.len().min(MAX_CARAPACE_ENTRIES);
    let mut entries = Vec::with_capacity(cap);
    let mut seen = HashSet::with_capacity(cap);
    for item in export.values.into_iter().take(MAX_CARAPACE_ENTRIES) {
        let clean_text = sanitize_single_line(item.value.trim());
        let text_trimmed = clean_text.trim();
        if text_trimmed.is_empty() || !seen.insert(text_trimmed.to_ascii_lowercase()) {
            continue;
        }
        let text = text_trimmed.to_string();
        let clean_display = sanitize_single_line(item.display.trim());
        let label = if clean_display.trim().is_empty() {
            text.clone()
        } else {
            clean_display.trim().to_string()
        };
        let tag = item.tag.as_str();
        let kind = if text.starts_with('-') || contains_ignore_ascii_case(tag, "flag") {
            if text.ends_with('=') {
                EntryKind::Param
            } else {
                EntryKind::Flag
            }
        } else if contains_ignore_ascii_case(tag, "command")
            || contains_ignore_ascii_case(tag, "subcommand")
        {
            EntryKind::Subcmd
        } else if contains_ignore_ascii_case(tag, "directory")
            || contains_ignore_ascii_case(tag, "dir")
            || contains_ignore_ascii_case(tag, "file")
        {
            EntryKind::Path
        } else if tag.trim().is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            EntryKind::Subcmd
        } else {
            EntryKind::Arg
        };

        let clean_desc = sanitize_multiline_desc(item.description.trim());
        entries.push(Entry::new(text, label, kind, clean_desc.as_ref()));
    }
    entries
}

fn exe_stem_eq(exe: &str, expected: &str) -> bool {
    Path::new(exe)
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case(expected))
}

fn introspect_cli_chain_with<F>(
    chain: &[&str],
    dialect: ShellDialect,
    carapace_enabled: bool,
    run_cmd: &F,
) -> Vec<Entry>
where
    F: Fn(&str, &[&str]) -> Option<String> + ?Sized,
{
    let mut stack_chain = [""; MAX_CHAIN_DEPTH];
    let Some(chain_len) = normalize_and_validate_chain(chain, &mut stack_chain) else {
        return Vec::new();
    };
    let clean_chain = &stack_chain[..chain_len];
    let exe = clean_chain[0];
    let sub_args = &clean_chain[1..];

    // Note: For git, always prefer `-h` so git never spawns a web browser on Windows!
    let primary_flag = if exe_stem_eq(exe, "git") {
        "-h"
    } else {
        "--help"
    };

    let mut cmd_args = [""; MAX_CHAIN_DEPTH + 1];
    cmd_args[..sub_args.len()].copy_from_slice(sub_args);
    cmd_args[sub_args.len()] = primary_flag;
    let arg_len = sub_args.len() + 1;

    let text = run_cmd(exe, &cmd_args[..arg_len]).unwrap_or_default();
    let mut entries = parse_cli_help(&text);

    if primary_flag != "-h" && (text.trim().len() < 25 || entries.is_empty()) {
        cmd_args[sub_args.len()] = "-h";
        if let Some(fallback_text) = run_cmd(exe, &cmd_args[..arg_len]) {
            let fallback_entries = parse_cli_help(&fallback_text);
            if !fallback_entries.is_empty() || entries.is_empty() {
                entries = fallback_entries;
            }
        }
    }

    // Secondary Introspection Bridge: query `carapace <exe> export <chain...> ""`
    // when primary help parsing yields nothing, or on subcommand chains when `--help`
    // returned only flags/params and no subcommands or dynamic value candidates.
    let needs_carapace = carapace_enabled
        && !exe_stem_eq(exe, "carapace")
        && (entries.is_empty()
            || (clean_chain.len() >= 2
                && !entries.iter().any(|e| {
                    matches!(
                        e.kind,
                        EntryKind::Subcmd
                            | EntryKind::Arg
                            | EntryKind::Enum
                            | EntryKind::Value
                            | EntryKind::Path
                    )
                })));
    if needs_carapace {
        let mut c_args = [""; MAX_CHAIN_DEPTH + 3];
        c_args[0] = exe;
        c_args[1] = "export";
        c_args[2..2 + clean_chain.len()].copy_from_slice(clean_chain);
        let c_len = 2 + clean_chain.len() + 1; // trailing empty arg ""
        if let Some(carapace_out) = run_cmd("carapace", &c_args[..c_len]) {
            let carapace_entries = parse_carapace_export(&carapace_out);
            if entries.is_empty() {
                entries = carapace_entries;
            } else if !carapace_entries.is_empty() {
                let mut merged = Vec::with_capacity(entries.len() + carapace_entries.len());
                let mut seen: HashSet<String> =
                    HashSet::with_capacity(entries.len() + carapace_entries.len());
                let mut extra_flags = Vec::new();
                for item in carapace_entries {
                    let key = item.text.to_ascii_lowercase();
                    if matches!(item.kind, EntryKind::Flag | EntryKind::Param) {
                        extra_flags.push(item);
                    } else if seen.insert(key) {
                        merged.push(item);
                    }
                }
                for item in entries {
                    seen.insert(item.text.to_ascii_lowercase());
                    for alias in &item.aliases {
                        let lower = alias.to_ascii_lowercase();
                        if !lower.starts_with('-') {
                            seen.insert(format!("-{lower}"));
                        }
                        seen.insert(lower);
                    }
                    merged.push(item);
                }
                for item in extra_flags {
                    if seen.insert(item.text.to_ascii_lowercase()) {
                        merged.push(item);
                    }
                }
                entries = merged;
            }
        }
    }

    // If this is a subcommand or leaf level, also append helpful shell variables
    if clean_chain.len() >= 2 {
        let vars = dialect.common_var_entries();
        let orig_len = entries.len();
        entries.reserve(vars.len());
        for cv in vars {
            if !entries[..orig_len].iter().any(|e| e.text == cv.text) {
                entries.push(cv);
            }
        }
    } else if entries.is_empty() {
        entries.push(
            Entry::new(
                "--help",
                "--help (-h)",
                EntryKind::Flag,
                format!("Display help for {exe}"),
            )
            .with_aliases(vec!["h".to_string()]),
        );
        entries.extend(dialect.common_var_entries());
    }

    entries
}

/// Deep CLI introspection and caching module owning the in-memory CLI cache map,
/// dirty tracking, disk persistence, canonical command-chain cache keys, background
/// worker thread/channels, first-run PowerShell metadata sync notification, and
/// priority tool cache warming.
pub struct CliIntrospector {
    cache: HashMap<String, Vec<Entry>>,
    pending_keys: HashSet<String>,
    invalidated_gen: HashMap<String, u64>,
    dirty: bool,
    dialect: ShellDialect,
    carapace_enabled: bool,
    cache_path: Option<PathBuf>,
    runner: Arc<CommandRunner>,
    req_tx: Option<Sender<BgRequest>>,
    res_rx: Option<Receiver<BgResponse>>,
    catalog_rx: Option<Receiver<Vec<Entry>>>,
}

impl CliIntrospector {
    /// Creates a production `CliIntrospector` backed by a dialect-specific disk cache (`cli_cache_*.json`)
    /// and a background OS worker thread.
    /// When running on PowerShell without cached `ps_metadata.json`, starts first-run metadata generation
    /// in the background so `poll_updates()` delivers the refreshed catalog live.
    pub fn new(dialect: ShellDialect, runtime_ctx: &RuntimeContext) -> Self {
        let path = cli_cache_path_for(dialect);
        let cache = load_cli_disk_cache_from(&path, dialect);
        let runner = default_command_runner();
        let carapace_enabled = is_carapace_installed();
        let catalog_rx = maybe_spawn_ps_sync(dialect, runtime_ctx.clone());
        Self::build_with_carapace(
            dialect,
            cache,
            Some(path),
            runner,
            true,
            carapace_enabled,
            catalog_rx,
        )
    }

    /// Creates a `CliIntrospector` tailored for synchronous batch cache warming (no worker threads).
    fn new_for_warm(dialect: ShellDialect) -> Self {
        let path = cli_cache_path_for(dialect);
        let cache = load_cli_disk_cache_from(&path, dialect);
        let runner = default_command_runner();
        let carapace_enabled = is_carapace_installed();
        Self::build_with_carapace(
            dialect,
            cache,
            Some(path),
            runner,
            false,
            carapace_enabled,
            None,
        )
    }

    /// Creates a `CliIntrospector` with an injected command-output adapter and optional disk cache path.
    #[cfg(test)]
    fn with_runner<F>(dialect: ShellDialect, cache_path: Option<PathBuf>, runner: F) -> Self
    where
        F: Fn(&str, &[&str]) -> Option<String> + Send + Sync + 'static,
    {
        Self::with_runner_and_carapace(dialect, cache_path, false, runner)
    }

    /// Creates a `CliIntrospector` with an injected command-output adapter and configurable Carapace fallback.
    #[cfg(test)]
    fn with_runner_and_carapace<F>(
        dialect: ShellDialect,
        cache_path: Option<PathBuf>,
        carapace_enabled: bool,
        runner: F,
    ) -> Self
    where
        F: Fn(&str, &[&str]) -> Option<String> + Send + Sync + 'static,
    {
        let cache = cache_path
            .as_deref()
            .map(|p| load_cli_disk_cache_from(p, dialect))
            .unwrap_or_default();
        Self::build_with_carapace(
            dialect,
            cache,
            cache_path,
            Arc::new(runner),
            true,
            carapace_enabled,
            None,
        )
    }

    #[cfg(test)]
    fn build(
        dialect: ShellDialect,
        cache: HashMap<String, Vec<Entry>>,
        cache_path: Option<PathBuf>,
        runner: Arc<CommandRunner>,
        spawn_worker: bool,
        catalog_rx: Option<Receiver<Vec<Entry>>>,
    ) -> Self {
        Self::build_with_carapace(
            dialect,
            cache,
            cache_path,
            runner,
            spawn_worker,
            false,
            catalog_rx,
        )
    }

    fn build_with_carapace(
        dialect: ShellDialect,
        cache: HashMap<String, Vec<Entry>>,
        cache_path: Option<PathBuf>,
        runner: Arc<CommandRunner>,
        spawn_worker: bool,
        carapace_enabled: bool,
        catalog_rx: Option<Receiver<Vec<Entry>>>,
    ) -> Self {
        let (req_tx, res_rx) = if spawn_worker {
            let (req_tx, req_rx) = mpsc::channel::<BgRequest>();
            let (res_tx, res_rx) = mpsc::channel::<BgResponse>();
            let worker_runner = Arc::clone(&runner);
            spawn_background_worker(req_rx, res_tx, dialect, carapace_enabled, worker_runner);
            (Some(req_tx), Some(res_rx))
        } else {
            (None, None)
        };

        Self {
            cache,
            pending_keys: HashSet::new(),
            invalidated_gen: HashMap::new(),
            dirty: false,
            dialect,
            carapace_enabled,
            cache_path,
            runner,
            req_tx,
            res_rx,
            catalog_rx,
        }
    }

    /// Canonical cache key for a command chain (`&["git"]` -> `"git"`, `&["git", "commit"]` -> `"git::commit"`).
    fn chain_key(chain: &[&str]) -> String {
        let mut total_len = 0usize;
        let mut count = 0usize;
        for raw in chain {
            let s = raw.trim();
            if !s.is_empty() {
                total_len += s.len();
                count += 1;
            }
        }
        if count == 0 {
            return String::new();
        }
        let mut out = String::with_capacity(total_len + 2 * (count - 1));
        let mut first = true;
        for raw in chain {
            let s = raw.trim();
            if s.is_empty() {
                continue;
            }
            if !first {
                out.push_str("::");
            }
            first = false;
            let start = out.len();
            out.push_str(s);
            out[start..].make_ascii_lowercase();
        }
        out
    }

    fn root_exe(key: &str) -> &str {
        key.split("::").next().unwrap_or(key)
    }

    fn current_generation(&self, key: &str) -> u64 {
        self.invalidated_gen
            .get(Self::root_exe(key))
            .copied()
            .unwrap_or(0)
    }

    /// Queries the cache for `chain`. On cache hit, returns the cached entries.
    /// On cache miss, enqueues a deduplicated background introspection job and returns a loading placeholder entry.
    pub fn query(&mut self, chain: &[&str]) -> Vec<Entry> {
        let mut stack_chain = [""; MAX_CHAIN_DEPTH];
        let Some(chain_len) = normalize_and_validate_chain(chain, &mut stack_chain) else {
            return Vec::new();
        };
        let clean_chain = &stack_chain[..chain_len];

        if clean_chain.len() == 1 && !clean_chain[0].bytes().any(|b| b.is_ascii_uppercase()) {
            if let Some(cached) = self.cache.get(clean_chain[0]) {
                return cached.clone();
            }
        }

        let key = Self::chain_key(clean_chain);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }

        let generation = self.current_generation(&key);
        if let Some(ref tx) = self.req_tx {
            if self.pending_keys.insert(key.clone()) {
                let _ = tx.send(BgRequest {
                    cache_key: key,
                    cmd_chain: clean_chain.iter().map(|s| (*s).to_string()).collect(),
                    generation,
                });
            }
        }

        vec![Self::loading_entry(clean_chain)]
    }

    /// Drains completed background introspection responses and catalog refreshes.
    pub fn poll_updates(&mut self) -> PollResult {
        let introspected = self.drain_ready();
        let catalog_reloaded = match self.catalog_rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(entries)) => {
                self.catalog_rx = None;
                Some(entries)
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.catalog_rx = None;
                None
            }
            Some(Err(TryRecvError::Empty)) | None => None,
        };
        PollResult {
            introspected,
            catalog_reloaded,
        }
    }

    /// Test-only blocking poll that waits for every queued background job to finish.
    #[cfg(test)]
    fn poll_blocking(&mut self) -> bool {
        let mut updated = self.drain_ready();
        while !self.pending_keys.is_empty() {
            let Some(ref rx) = self.res_rx else {
                break;
            };
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(resp) => {
                    if self.record(resp) {
                        updated = true;
                    }
                }
                Err(_) => break,
            }
        }
        updated
    }

    fn drain_ready(&mut self) -> bool {
        let mut drained = Vec::new();
        if let Some(ref rx) = self.res_rx {
            while let Ok(resp) = rx.try_recv() {
                drained.push(resp);
            }
        }
        let mut updated = false;
        for resp in drained {
            if self.record(resp) {
                updated = true;
            }
        }
        updated
    }

    fn record(&mut self, resp: BgResponse) -> bool {
        if resp.generation < self.current_generation(&resp.cache_key) {
            return false;
        }
        self.pending_keys.remove(&resp.cache_key);
        self.cache.insert(resp.cache_key, resp.entries);
        self.dirty = true;
        true
    }

    /// Persists the cache to disk if and only if new entries were recorded.
    pub fn flush(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        if let Some(ref path) = self.cache_path {
            save_cli_disk_cache_to(path, self.dialect, &self.cache);
        }
        self.dirty = false;
        true
    }

    /// Invalidates all cached introspection chains rooted at `exe` so subsequent queries re-run `--help`.
    pub fn invalidate_command(&mut self, exe: &str) {
        let root_trimmed = exe.trim();
        if root_trimmed.is_empty() {
            return;
        }
        let root = root_trimmed.to_ascii_lowercase();
        *self.invalidated_gen.entry(root.clone()).or_insert(0) += 1;
        let before = self.cache.len();
        let matches_root = |k: &str| {
            k == root
                || (k.starts_with(&root)
                    && k.as_bytes().get(root.len()..root.len() + 2) == Some(b"::"))
        };
        self.cache.retain(|k, _| !matches_root(k));
        self.pending_keys.retain(|k| !matches_root(k));
        if self.cache.len() != before {
            self.dirty = true;
        }
    }

    /// Pre-warms the CLI cache in parallel for all installed tools in `PRIORITY_CLI_TOOLS`
    /// using the canonical cache key format. Returns the number of installed priority tools warmed.
    fn warm_priority_tools(&mut self, col1_entries: &[Entry]) -> usize {
        let mut present = [false; PRIORITY_CLI_TOOLS.len()];
        let mut remaining = PRIORITY_CLI_TOOLS.len();
        for it in col1_entries {
            if it.kind != EntryKind::Cli {
                continue;
            }
            for (idx, &tool) in PRIORITY_CLI_TOOLS.iter().enumerate() {
                if !present[idx]
                    && it.text.len() == tool.len()
                    && it.text.eq_ignore_ascii_case(tool)
                {
                    present[idx] = true;
                    remaining -= 1;
                    break;
                }
            }
            if remaining == 0 {
                break;
            }
        }

        let available: Vec<&str> = PRIORITY_CLI_TOOLS
            .iter()
            .copied()
            .enumerate()
            .filter_map(|(idx, tool)| present[idx].then_some(tool))
            .collect();

        let dialect = self.dialect;
        let carapace_enabled = self.carapace_enabled;
        let runner = Arc::clone(&self.runner);
        let results: Vec<(String, Vec<Entry>)> = available
            .par_iter()
            .map(|&tool| {
                let chain = [tool];
                let entries =
                    introspect_cli_chain_with(&chain, dialect, carapace_enabled, &*runner);
                (Self::chain_key(&chain), entries)
            })
            .collect();

        for (key, entries) in results {
            if !entries.is_empty() {
                self.pending_keys.remove(&key);
                self.cache.insert(key, entries);
                self.dirty = true;
            }
        }

        available.len()
    }

    fn loading_entry(chain: &[&str]) -> Entry {
        if chain.len() <= 1 {
            let cmd = chain.first().copied().unwrap_or("");
            Entry::placeholder(
                format!("⟳ Loading subcommands & flags for '{cmd}'..."),
                EntryKind::Loading,
                format!("Introspecting {cmd} --help in background thread"),
            )
        } else {
            let joined = chain.join(" ");
            Entry::placeholder(
                format!("⟳ Loading options for '{joined}'..."),
                EntryKind::Loading,
                format!("Introspecting {joined} --help in background thread"),
            )
        }
    }
}

impl Introspect for CliIntrospector {
    fn query(&mut self, chain: &[&str]) -> Vec<Entry> {
        CliIntrospector::query(self, chain)
    }

    fn poll_updates(&mut self) -> PollResult {
        CliIntrospector::poll_updates(self)
    }

    fn flush(&mut self) -> bool {
        CliIntrospector::flush(self)
    }

    fn invalidate_command(&mut self, exe: &str) {
        CliIntrospector::invalidate_command(self, exe)
    }
}

fn maybe_spawn_ps_sync(
    dialect: ShellDialect,
    runtime_ctx: RuntimeContext,
) -> Option<Receiver<Vec<Entry>>> {
    if !dialect.is_powershell() || ps_metadata_path().exists() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if sync_powershell_metadata().is_ok() {
            let _ = tx.send(load_catalog(dialect, &runtime_ctx));
        }
    });
    Some(rx)
}

fn spawn_background_worker(
    rx: Receiver<BgRequest>,
    tx: Sender<BgResponse>,
    dialect: ShellDialect,
    carapace_enabled: bool,
    runner: Arc<CommandRunner>,
) {
    thread::spawn(move || {
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
                let chain_refs: Vec<&str> = req.cmd_chain.iter().map(|s| s.as_str()).collect();
                let entries =
                    introspect_cli_chain_with(&chain_refs, dialect, carapace_enabled, &*runner);
                if tx
                    .send(BgResponse {
                        cache_key: req.cache_key,
                        entries,
                        generation: req.generation,
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    });
}

/// Synchronously pre-warms and persists the CLI introspection cache for installed
/// priority tools in `dialect` using Rayon without spawning an extra worker thread.
/// Returns the number of tools warmed.
pub fn warm_cli_cache(dialect: ShellDialect) -> usize {
    let col1_entries = load_catalog(dialect, &RuntimeContext::default());
    let mut introspector = CliIntrospector::new_for_warm(dialect);
    let warmed = introspector.warm_priority_tools(&col1_entries);
    introspector.flush();
    warmed
}

/// Machine- and human-readable snapshot produced by synchronous command inspection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectReport {
    pub dialect: String,
    pub chain: Vec<String>,
    pub command: Option<Entry>,
    pub entries: Vec<Entry>,
}

fn entries_eq(a: &Entry, b: &Entry) -> bool {
    a.text == b.text
        && a.label == b.label
        && a.kind == b.kind
        && a.role == b.role
        && a.desc == b.desc
        && a.parameter_sets == b.parameter_sets
        && a.aliases == b.aliases
        && a.has_literal == b.has_literal
        && a.children.len() == b.children.len()
        && a.children
            .iter()
            .zip(&b.children)
            .all(|(ca, cb)| entries_eq(ca, cb))
}

impl PartialEq for InspectReport {
    fn eq(&self, other: &Self) -> bool {
        self.dialect == other.dialect
            && self.chain == other.chain
            && match (&self.command, &other.command) {
                (Some(a), Some(b)) => entries_eq(a, b),
                (None, None) => true,
                _ => false,
            }
            && self.entries.len() == other.entries.len()
            && self
                .entries
                .iter()
                .zip(&other.entries)
                .all(|(a, b)| entries_eq(a, b))
    }
}

impl Eq for InspectReport {}

fn parse_inspect_chain(raw_chain: &[String]) -> anyhow::Result<Vec<String>> {
    if raw_chain.is_empty() {
        anyhow::bail!("Command chain must not be empty");
    }

    let mut total_bytes = 0usize;
    let mut tokens = Vec::new();
    for raw in raw_chain {
        total_bytes = total_bytes.saturating_add(raw.len());
        if total_bytes > MAX_INSPECT_INPUT_BYTES {
            anyhow::bail!(
                "Command chain exceeds maximum input length of {MAX_INSPECT_INPUT_BYTES} bytes"
            );
        }
        if has_shell_metachar_or_control(raw) {
            anyhow::bail!(
                "Unsafe command token '{}': contains disallowed shell metacharacters or control characters",
                sanitize_single_line(raw).trim()
            );
        }
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            anyhow::bail!("Command chain must not contain empty tokens");
        }
        for part in trimmed.split_whitespace() {
            if tokens.len() >= MAX_CHAIN_DEPTH {
                anyhow::bail!("Command chain exceeds maximum depth of {MAX_CHAIN_DEPTH} tokens");
            }
            tokens.push(part.to_string());
        }
    }

    if tokens.is_empty() {
        anyhow::bail!("Command chain must not be empty");
    }

    let exe = &tokens[0];
    if exe.len() > 260
        || exe.starts_with('-')
        || exe.starts_with('.')
        || exe.contains("..")
        || (!Path::new(exe).is_absolute() && exe.contains(['/', '\\']))
    {
        anyhow::bail!(
            "Unsafe command token '{}'",
            sanitize_single_line(exe).trim()
        );
    }

    for sub in &tokens[1..] {
        if !is_safe_sub_arg(sub) {
            anyhow::bail!(
                "Unsafe subcommand argument '{}'",
                sanitize_single_line(sub).trim()
            );
        }
    }

    Ok(tokens)
}

fn find_catalog_root<'a>(catalog: &'a [Entry], root: &str) -> Option<&'a Entry> {
    catalog
        .iter()
        .find(|e| {
            e.is_emittable() && e.kind != EntryKind::History && e.text.eq_ignore_ascii_case(root)
        })
        .or_else(|| {
            catalog
                .iter()
                .find(|e| e.is_emittable() && e.text.eq_ignore_ascii_case(root))
        })
}

fn walk_entry_chain<'a>(root: &'a Entry, sub_chain: &[String]) -> Option<&'a Entry> {
    let mut cur = root;
    for seg in sub_chain {
        cur = cur
            .children
            .iter()
            .find(|c| c.text.eq_ignore_ascii_case(seg))
            .or_else(|| {
                cur.children.iter().find(|c| {
                    c.kind == EntryKind::Subcmd
                        && c.aliases.iter().any(|a| a.eq_ignore_ascii_case(seg))
                })
            })?;
    }
    Some(cur)
}

fn has_discovered_cli_entries(chain_len: usize, exe: &str, entries: &[Entry]) -> bool {
    if entries.is_empty() {
        return false;
    }
    if chain_len >= 2 {
        return entries.iter().any(|e| e.kind != EntryKind::Var);
    }
    !(entries
        .first()
        .is_some_and(|e| e.text == "--help" && e.desc == format!("Display help for {exe}"))
        && entries[1..].iter().all(|e| e.kind == EntryKind::Var))
}

pub(crate) fn inspect_command_chain_with<F>(
    dialect: ShellDialect,
    catalog: &[Entry],
    raw_chain: &[String],
    cache_path: Option<&Path>,
    carapace_enabled: bool,
    runner: &F,
) -> anyhow::Result<InspectReport>
where
    F: Fn(&str, &[&str]) -> Option<String> + ?Sized,
{
    let chain = parse_inspect_chain(raw_chain)?;
    let root_tok = &chain[0];
    let matched_root = find_catalog_root(catalog, root_tok);

    let is_catalog_cmd = matched_root.is_some_and(|e| {
        matches!(
            e.kind,
            EntryKind::Cmdlet | EntryKind::Func | EntryKind::Alias
        )
    });
    if !is_safe_exe_token(root_tok) && !(is_catalog_cmd && is_safe_catalog_command_token(root_tok))
    {
        anyhow::bail!(
            "Unsafe command token '{}'",
            sanitize_single_line(root_tok).trim()
        );
    }

    let command = matched_root.map(|entry| {
        let mut clean = entry.clone();
        clean.children.clear();
        clean
    });

    if let Some(root_entry) = matched_root {
        let target = if chain.len() == 1 {
            Some(root_entry)
        } else {
            walk_entry_chain(root_entry, &chain[1..])
        };
        if let Some(node) = target {
            if !node.children.is_empty() {
                return Ok(InspectReport {
                    dialect: dialect_cache_tag(dialect).to_string(),
                    chain,
                    command,
                    entries: node.children.clone(),
                });
            }
        }
    }

    let chain_refs: Vec<&str> = chain.iter().map(String::as_str).collect();
    let key = CliIntrospector::chain_key(&chain_refs);
    let mut cache = cache_path
        .map(|p| load_cli_disk_cache_from(p, dialect))
        .unwrap_or_default();

    let entries = if let Some(cached) = cache.get(&key) {
        cached.clone()
    } else if is_safe_exe_token(root_tok) {
        let discovered = introspect_cli_chain_with(&chain_refs, dialect, carapace_enabled, runner);
        if has_discovered_cli_entries(chain_refs.len(), chain_refs[0], &discovered) {
            if let Some(path) = cache_path {
                cache.insert(key, discovered.clone());
                save_cli_disk_cache_to(path, dialect, &cache);
            }
        }
        discovered
    } else {
        Vec::new()
    };

    Ok(InspectReport {
        dialect: dialect_cache_tag(dialect).to_string(),
        chain,
        command,
        entries,
    })
}

/// Synchronously inspects a command or subcommand chain against the Command Catalog,
/// persisted CLI introspection cache, and live `--help`/`-h`/carapace introspection.
pub fn inspect_command_chain(
    dialect: ShellDialect,
    runtime_ctx: &RuntimeContext,
    raw_chain: &[String],
) -> anyhow::Result<InspectReport> {
    let _ = parse_inspect_chain(raw_chain)?;
    let catalog = load_catalog(dialect, runtime_ctx);
    let cache_path = cli_cache_path_for(dialect);
    let carapace_enabled = is_carapace_installed();
    let runner = default_command_runner();
    inspect_command_chain_with(
        dialect,
        &catalog,
        raw_chain,
        Some(&cache_path),
        carapace_enabled,
        &*runner,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_cli_introspector_cache_miss_background_poll_and_persistence() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp_path = env::temp_dir().join(format!(
            "zpane_test_cli_cache_{}_{}.json",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = fs::remove_file(&temp_path);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_runner = Arc::clone(&call_count);

        let mut introspector = CliIntrospector::with_runner(
            ShellDialect::Posix,
            Some(temp_path.clone()),
            move |exe, args| {
                call_count_runner.fetch_add(1, Ordering::SeqCst);
                match (exe.to_ascii_lowercase().as_str(), args) {
                    ("cargo", ["--help"]) => Some(
                        "\
Usage: cargo [OPTIONS] [COMMAND]

Commands:
  build    Compile the current package
  test     Execute all unit and integration tests

Options:
  -V, --version    Print version info and exit
  -h, --help       Print help
"
                        .to_string(),
                    ),
                    ("cargo", ["build", "--help"]) => Some(
                        "\
Usage: cargo build [OPTIONS]

Options:
  -r, --release          Build artifacts in release mode, with optimizations
      --target <TRIPLE>  Build for the target triple
"
                        .to_string(),
                    ),
                    _ => None,
                }
            },
        );

        assert!(!introspector.dirty);
        assert!(!introspector.flush(), "Clean cache must not write to disk");
        assert!(!temp_path.exists());

        // 1. First query for top-level command is a cache miss returning a Loading placeholder
        let miss = introspector.query(&["cargo"]);
        assert_eq!(miss.len(), 1);
        assert!(miss[0].is_loading());
        assert!(miss[0].is_placeholder());

        // Duplicate query while in-flight remains deduplicated
        let miss_dup = introspector.query(&["CARGO"]);
        assert_eq!(miss_dup.len(), 1);
        assert!(miss_dup[0].is_loading());

        // 2. Poll drains background worker completion and marks cache dirty
        assert!(introspector.poll_blocking());
        assert!(introspector.dirty);
        assert!(
            !introspector.poll_blocking(),
            "Second poll with no pending jobs returns false"
        );
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        // 3. Subsequent queries (regardless of casing) hit the populated cache
        let cached = introspector.query(&["Cargo"]);
        let texts: Vec<&str> = cached.iter().map(|i| i.text.as_str()).collect();
        assert!(texts.contains(&"build"));
        assert!(texts.contains(&"test"));
        assert!(texts.contains(&"--version"));

        // 4. Subcommand chain query (`cargo build`)
        let sub_miss = introspector.query(&["cargo", "build"]);
        assert_eq!(sub_miss.len(), 1);
        assert!(sub_miss[0].is_loading());

        assert!(introspector.poll_blocking());
        let sub_cached = introspector.query(&["CARGO", "BUILD"]);
        let sub_texts: Vec<&str> = sub_cached.iter().map(|i| i.text.as_str()).collect();
        assert!(sub_texts.contains(&"--release"));
        assert!(sub_texts.contains(&"--target"));
        assert!(
            sub_texts.contains(&"$PWD"),
            "Subcommand level appends dialect common variables"
        );

        // 5. Flush writes to disk once and clears dirty flag
        assert!(introspector.flush());
        assert!(!introspector.dirty);
        assert!(temp_path.exists());
        assert!(!introspector.flush());

        // 6. New introspector loads persisted cache from disk without invoking command runner
        let mut reloaded = CliIntrospector::with_runner(
            ShellDialect::Posix,
            Some(temp_path.clone()),
            |_exe, _args| {
                panic!("Reloaded cache should satisfy queries without invoking runner");
            },
        );
        let from_disk = reloaded.query(&["cargo", "build"]);
        assert!(from_disk.iter().any(|i| i.text == "--release"));

        let _ = fs::remove_file(&temp_path);
    }

    #[test]
    fn test_cli_introspector_help_to_short_h_fallback_and_git_short_h() {
        use std::sync::Mutex;
        type RecordedCalls = Arc<Mutex<Vec<(String, Vec<String>)>>>;

        let calls: RecordedCalls = Arc::new(Mutex::new(Vec::new()));
        let calls_runner = Arc::clone(&calls);

        let mut introspector =
            CliIntrospector::with_runner(ShellDialect::Posix, None, move |exe, args| {
                calls_runner.lock().unwrap().push((
                    exe.to_string(),
                    args.iter().map(|s| (*s).to_string()).collect(),
                ));

                match (exe, args) {
                    // Simulates a tool where `--help` fails with a short error and `-h` succeeds
                    ("legacytool", ["--help"]) => Some("unknown option".to_string()),
                    ("legacytool", ["-h"]) => Some(
                        "\
Options:
  -v, --verbose    Enable verbose output
  -f, --file <F>   Specify input file
"
                        .to_string(),
                    ),
                    // Git must query `-h` directly without trying `--help`
                    ("git", ["-h"]) => Some(
                        "\
start a working area
   clone      Clone a repository into a new directory
   init       Create an empty Git repository
"
                        .to_string(),
                    ),
                    _ => None,
                }
            });

        assert!(introspector.query(&["legacytool"])[0].is_loading());
        assert!(introspector.query(&["git"])[0].is_loading());
        assert!(introspector.poll_blocking());

        let legacy_items = introspector.query(&["legacytool"]);
        let legacy_texts: Vec<&str> = legacy_items.iter().map(|i| i.text.as_str()).collect();
        assert!(legacy_texts.contains(&"--verbose"));
        assert!(legacy_texts.contains(&"--file"));

        let git_items = introspector.query(&["git"]);
        let git_texts: Vec<&str> = git_items.iter().map(|i| i.text.as_str()).collect();
        assert!(git_texts.contains(&"clone"));
        assert!(git_texts.contains(&"init"));

        let recorded = calls.lock().unwrap();
        assert!(
            recorded.contains(&("legacytool".to_string(), vec!["--help".to_string()])),
            "legacytool should try --help first"
        );
        assert!(
            recorded.contains(&("legacytool".to_string(), vec!["-h".to_string()])),
            "legacytool should fall back to -h when --help output is too short"
        );
        assert!(
            recorded.contains(&("git".to_string(), vec!["-h".to_string()])),
            "git should use -h directly"
        );
        assert!(
            !recorded.contains(&("git".to_string(), vec!["--help".to_string()])),
            "git must never invoke --help"
        );
    }

    #[test]
    fn test_canonical_priority_tools_and_warm_cache_key_matching() {
        assert!(
            PRIORITY_CLI_TOOLS.contains(&"gh"),
            "Canonical priority tool list must include gh"
        );
        assert!(PRIORITY_CLI_TOOLS.contains(&"git"));
        assert!(PRIORITY_CLI_TOOLS.contains(&"cargo"));

        let col1_entries = vec![
            Entry::new("Git", "Git", EntryKind::Cli, "/usr/bin/git"),
            Entry::new("gh", "gh", EntryKind::Cli, "/usr/bin/gh"),
            Entry::new(
                "unlisted-cli",
                "unlisted-cli",
                EntryKind::Cli,
                "/usr/bin/unlisted-cli",
            ),
        ];

        let mut introspector = CliIntrospector::build(
            ShellDialect::Posix,
            HashMap::new(),
            None,
            Arc::new(
                |exe: &str, _args: &[&str]| match exe.to_ascii_lowercase().as_str() {
                    "git" => Some(
                        "\
start a working area
   clone      Clone a repository into a new directory
"
                        .to_string(),
                    ),
                    "gh" => Some(
                        "\
CORE COMMANDS
  pr:            Manage pull requests
  issue:         Manage issues
"
                        .to_string(),
                    ),
                    _ => panic!("Unexpected non-priority tool warmed: {exe}"),
                },
            ),
            false,
            None,
        );

        assert!(
            introspector.req_tx.is_none(),
            "Synchronous warm builder must not spawn a background worker thread"
        );

        let warmed = introspector.warm_priority_tools(&col1_entries);
        assert_eq!(warmed, 2);
        assert!(introspector.dirty);

        // Interactive queries using any casing hit the warmed cache immediately (no Loading state)
        let git_entries = introspector.query(&["git"]);
        assert!(git_entries.iter().all(|i| !i.is_loading()));
        assert!(git_entries.iter().any(|i| i.text == "clone"));

        let git_upper = introspector.query(&["GIT"]);
        assert!(git_upper.iter().any(|i| i.text == "clone"));

        let gh_entries = introspector.query(&["gh"]);
        assert!(gh_entries.iter().all(|i| !i.is_loading()));
        assert!(gh_entries.iter().any(|i| i.text == "pr"));
        assert!(gh_entries.iter().any(|i| i.text == "issue"));

        assert_eq!(
            CliIntrospector::chain_key(&["Git", "Commit"]),
            "git::commit"
        );
    }

    #[test]
    fn test_empty_chain_query_and_poll_updates_catalog_reload() {
        let (tx, rx) = mpsc::channel();
        let mut introspector = CliIntrospector::build(
            ShellDialect::PowerShell,
            HashMap::new(),
            None,
            Arc::new(|_, _| None),
            false,
            Some(rx),
        );
        assert!(introspector.query(&[]).is_empty());

        let empty_poll = introspector.poll_updates();
        assert!(!empty_poll.any());

        tx.send(vec![Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "Module",
        )])
        .unwrap();

        let loaded = introspector.poll_updates();
        assert!(!loaded.introspected);
        assert!(loaded.any());
        let catalog = loaded.catalog_reloaded.expect("reloaded catalog");
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].text, "Get-Process");

        // Second poll after channel consumption returns no update
        assert!(!introspector.poll_updates().any());
    }

    #[test]
    fn test_carapace_bridge_fallback_when_help_empty_and_skipped_when_help_succeeds() {
        use std::sync::Mutex;

        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let calls_ref = Arc::clone(&calls);

        let mut introspector = CliIntrospector::with_runner_and_carapace(
            ShellDialect::Posix,
            None,
            true,
            move |exe, args| {
                calls_ref.lock().unwrap().push((
                    exe.to_string(),
                    args.iter().map(|s| (*s).to_string()).collect(),
                ));
                match (exe, args) {
                    ("carapace", ["fd", "export", "fd", ""]) => Some(
                        r#"{
  "version": "1.0.0",
  "nospace": "=",
  "values": [
    { "value": "--extension=", "display": "-e, --extension=", "description": "Filter by file extension", "tag": "flags" },
    { "value": "--hidden", "description": "Search hidden files", "tag": "flags" },
    { "value": "src/", "description": "Source directory", "tag": "directories" },
    { "value": "build", "description": "Build command", "tag": "commands" }
  ]
}"#
                        .to_string(),
                    ),
                    ("git", ["-h"]) => Some(
                        "\
start a working area
   clone      Clone a repository
"
                        .to_string(),
                    ),
                    _ => None,
                }
            },
        );

        // 1. Tool with empty --help/-h falls back to carapace export
        let _ = introspector.query(&["fd"]);
        assert!(introspector.poll_blocking());
        let fd_entries = introspector.query(&["fd"]);
        let ext = fd_entries
            .iter()
            .find(|e| e.text == "--extension=")
            .expect("--extension= from carapace");
        assert_eq!(ext.kind, EntryKind::Param);
        assert_eq!(ext.label, "-e, --extension=");

        let hidden = fd_entries
            .iter()
            .find(|e| e.text == "--hidden")
            .expect("--hidden from carapace");
        assert_eq!(hidden.kind, EntryKind::Flag);

        let dir_entry = fd_entries
            .iter()
            .find(|e| e.text == "src/")
            .expect("src/ path from carapace");
        assert_eq!(dir_entry.kind, EntryKind::Path);

        let cmd_entry = fd_entries
            .iter()
            .find(|e| e.text == "build")
            .expect("build subcmd from carapace");
        assert_eq!(cmd_entry.kind, EntryKind::Subcmd);

        // 2. Tool with working primary help does NOT invoke carapace
        let _ = introspector.query(&["git"]);
        assert!(introspector.poll_blocking());
        let recorded = calls.lock().unwrap();
        assert!(recorded
            .iter()
            .any(|(exe, args)| exe == "carapace" && args == &["fd", "export", "fd", ""]));
        assert!(!recorded.iter().any(
            |(exe, args)| exe == "carapace" && args.first().map(String::as_str) == Some("git")
        ));
    }

    #[test]
    fn test_subcommand_merges_carapace_dynamic_values_when_help_has_only_flags() {
        let mut introspector = CliIntrospector::with_runner_and_carapace(
            ShellDialect::Posix,
            None,
            true,
            |exe, args| match (exe, args) {
                ("docker", ["exec", "--help"]) => Some(
                    "\
Options:
  -i, --interactive    Keep STDIN open
  -t, --tty            Allocate a pseudo-TTY
"
                    .to_string(),
                ),
                ("carapace", ["docker", "export", "docker", "exec", ""]) => Some(
                    r#"{
  "values": [
    { "value": "web-api", "description": "Running container", "tag": "containers" },
    { "value": "--detach", "description": "Detached mode", "tag": "flags" }
  ]
}"#
                    .to_string(),
                ),
                _ => None,
            },
        );

        let _ = introspector.query(&["docker", "exec"]);
        assert!(introspector.poll_blocking());
        let entries = introspector.query(&["docker", "exec"]);
        let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts[0], "web-api");
        assert!(texts.contains(&"--interactive"));
        assert!(texts.contains(&"--detach"));
    }

    #[test]
    fn test_invalidate_command_and_cache_freshness_rejects_stale_records() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp_path = env::temp_dir().join(format!(
            "zpane_test_fresh_cache_{}_{}.json",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let _ = fs::remove_file(&temp_path);

        // 1. Write stale schema_version, mismatched dialect, missing binary, and mismatched exe_fingerprint records
        let mut raw_records = HashMap::new();
        raw_records.insert(
            "stale_ver".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION - 1,
                dialect: Some("posix".to_string()),
                exe_fingerprint: Some((100, 200)),
                entries: vec![Entry::new("old", "old", EntryKind::Subcmd, "")],
            },
        );
        raw_records.insert(
            "wrong_dialect".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                dialect: Some("pwsh".to_string()),
                exe_fingerprint: Some((555, 999)),
                entries: vec![Entry::new("$env:TEMP", "$env:TEMP", EntryKind::Var, "")],
            },
        );
        raw_records.insert(
            "uninstalled_bin".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                dialect: Some("posix".to_string()),
                exe_fingerprint: Some((100, 200)),
                entries: vec![Entry::new("gone", "gone", EntryKind::Subcmd, "")],
            },
        );
        raw_records.insert(
            "upgraded_bin".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                dialect: Some("posix".to_string()),
                exe_fingerprint: Some((100, 200)),
                entries: vec![Entry::new("old-bin", "old-bin", EntryKind::Subcmd, "")],
            },
        );
        raw_records.insert(
            "fresh_bin::sub".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                dialect: Some("posix".to_string()),
                exe_fingerprint: Some((555, 999)),
                entries: vec![Entry::new("ok", "ok", EntryKind::Subcmd, "")],
            },
        );
        fs::write(&temp_path, serde_json::to_vec(&raw_records).unwrap()).unwrap();

        let loaded =
            load_cli_disk_cache_with_fingerprint(&temp_path, Some(ShellDialect::Posix), |exe| {
                match exe {
                    "upgraded_bin" => Some((101, 200)),
                    "fresh_bin" | "wrong_dialect" => Some((555, 999)),
                    _ => None,
                }
            });
        assert!(!loaded.contains_key("stale_ver"));
        assert!(!loaded.contains_key("wrong_dialect"));
        assert!(!loaded.contains_key("uninstalled_bin"));
        assert!(!loaded.contains_key("upgraded_bin"));
        assert!(loaded.contains_key("fresh_bin::sub"));
        let _ = fs::remove_file(&temp_path);

        // 2. invalidate_command drops root and subcommand chains and triggers re-introspection
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        let mut introspector =
            CliIntrospector::with_runner(ShellDialect::Posix, None, move |exe, args| {
                calls_ref.fetch_add(1, Ordering::SeqCst);
                match (exe, args) {
                    ("cargo", ["--help"]) => {
                        Some("Commands:\n  build    Compile package\n".to_string())
                    }
                    ("cargo", ["build", "--help"]) => {
                        Some("Options:\n  --release    Release build\n".to_string())
                    }
                    ("other", ["--help"]) => Some("Options:\n  --flag    Other\n".to_string()),
                    _ => None,
                }
            });

        let _ = introspector.query(&["cargo"]);
        let _ = introspector.query(&["cargo", "build"]);
        let _ = introspector.query(&["other"]);
        assert!(introspector.poll_blocking());
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        introspector.invalidate_command("cargo");
        assert!(introspector
            .query(&["other"])
            .iter()
            .any(|e| e.text == "--flag"));
        assert!(introspector.query(&["cargo"])[0].is_loading());
        assert!(introspector.query(&["cargo", "build"])[0].is_loading());
        assert!(introspector.poll_blocking());
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn test_invalidate_command_discards_inflight_stale_response() {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = std::sync::Mutex::new(release_rx);
        let call_num = std::sync::atomic::AtomicUsize::new(0);

        let mut introspector =
            CliIntrospector::with_runner(ShellDialect::Posix, None, move |_exe, _args| {
                let idx = call_num.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if idx == 0 {
                    let _ = entered_tx.send(());
                    let _ = release_rx.lock().unwrap().recv();
                    Some("Options:\n  --stale-flag    Old version\n".to_string())
                } else {
                    Some("Options:\n  --fresh-flag    New version\n".to_string())
                }
            });

        assert!(introspector.query(&["tool"])[0].is_loading());
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        introspector.invalidate_command("tool");
        let _ = release_tx.send(());

        // Allow worker to send the stale response and ensure poll rejects it
        for _ in 0..20 {
            let poll = introspector.poll_updates();
            assert!(!poll.introspected, "Stale response must not be recorded");
            thread::sleep(Duration::from_millis(5));
        }

        // Re-querying triggers generation 1 and returns fresh entries
        assert!(introspector.query(&["tool"])[0].is_loading());
        assert!(introspector.poll_blocking());
        let fresh = introspector.query(&["tool"]);
        assert!(fresh.iter().any(|e| e.text == "--fresh-flag"));
        assert!(!fresh.iter().any(|e| e.text == "--stale-flag"));
    }

    #[test]
    fn test_long_help_error_falls_back_to_short_h_and_carapace_dedups_aliases() {
        use std::sync::Mutex;

        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let calls_ref = Arc::clone(&calls);

        let mut introspector = CliIntrospector::with_runner_and_carapace(
            ShellDialect::Posix,
            None,
            true,
            move |exe, args| {
                calls_ref.lock().unwrap().push((
                    exe.to_string(),
                    args.iter().map(|s| (*s).to_string()).collect(),
                ));
                match (exe, args) {
                    ("errtool", ["--help"]) => Some(
                        "Error: unrecognized long option '--help'. Try 'errtool -h' for usage."
                            .to_string(),
                    ),
                    ("errtool", ["-h"]) => {
                        Some("Options:\n  -v, --verbose    Enable verbose logs\n".to_string())
                    }
                    ("git.exe", ["-h"]) => {
                        Some("start a working area\n   clone   Clone repo\n".to_string())
                    }
                    ("docker", ["exec", "--help"]) => {
                        Some("Options:\n  -i, --interactive    Keep STDIN open\n".to_string())
                    }
                    ("carapace", ["docker", "export", "docker", "exec", ""]) => Some(
                        r#"{
  "values": [
    { "value": "web", "description": "Container", "tag": "containers" },
    { "value": "-i", "description": "Duplicate short", "tag": "flags" },
    { "value": "-d", "description": "Detached", "tag": "flags" }
  ]
}"#
                        .to_string(),
                    ),
                    _ => None,
                }
            },
        );

        let _ = introspector.query(&["errtool"]);
        let _ = introspector.query(&["git.exe"]);
        let _ = introspector.query(&["docker", "exec"]);
        assert!(introspector.poll_blocking());

        assert!(introspector
            .query(&["errtool"])
            .iter()
            .any(|e| e.text == "--verbose"));
        assert!(introspector
            .query(&["git.exe"])
            .iter()
            .any(|e| e.text == "clone"));

        let docker_entries = introspector.query(&["docker", "exec"]);
        let docker_texts: Vec<&str> = docker_entries.iter().map(|e| e.text.as_str()).collect();
        assert!(docker_texts.contains(&"web"));
        assert!(docker_texts.contains(&"--interactive"));
        assert!(docker_texts.contains(&"-d"));
        assert!(!docker_texts.contains(&"-i"));

        let recorded = calls.lock().unwrap();
        assert!(!recorded
            .iter()
            .any(|(exe, args)| exe == "git.exe" && args == &["--help"]));
    }

    #[test]
    fn test_argument_option_and_path_traversal_injection_never_spawns_runner() {
        use std::sync::Mutex;

        let calls = Arc::new(Mutex::new(Vec::<(String, Vec<String>)>::new()));
        let calls_ref = Arc::clone(&calls);

        let mut introspector = CliIntrospector::with_runner_and_carapace(
            ShellDialect::Posix,
            None,
            true,
            move |exe, args| {
                calls_ref.lock().unwrap().push((
                    exe.to_string(),
                    args.iter().map(|s| (*s).to_string()).collect(),
                ));
                Some("Options:\n  --ok    Ok\n".to_string())
            },
        );

        let hostile_chains: &[&[&str]] = &[
            &["-rf"],
            &["--upload-pack=calc"],
            &["git", "--exec-path=/tmp/evil"],
            &["git", "-c"],
            &["cmd", "/c"],
            &["git", "--upload-pack=id"],
            &["./evil"],
            &["../evil"],
            &["sub/evil"],
            &["C:evil.exe"],
            &["git;id"],
            &["git$(whoami)"],
            &["git\n--help"],
            &["git\0evil"],
            &["tool", "../sub"],
            &["a", "b", "c", "d", "e", "f", "g", "h", "i"],
        ];

        for chain in hostile_chains {
            assert!(
                introspector.query(chain).is_empty(),
                "query should reject hostile chain {chain:?}"
            );
            assert!(
                introspect_cli_chain_with(chain, ShellDialect::Posix, true, &|exe, _| {
                    panic!("direct runner invoked for hostile exe: {exe}");
                })
                .is_empty(),
                "introspect_cli_chain_with should reject {chain:?}"
            );
        }

        assert!(!introspector.poll_blocking());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn test_resolve_executable_blocks_relative_paths_and_reads_bounded_output_and_cache() {
        use std::io::Cursor;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let nonce = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = env::temp_dir().join(format!(
            "zpane_test_introspect_sec_{}_{nonce}",
            std::process::id()
        ));
        let bin_dir = root.join("zpane");
        fs::create_dir_all(&bin_dir).unwrap();

        #[cfg(windows)]
        let fake_exe_name = "mybin.exe";
        #[cfg(not(windows))]
        let fake_exe_name = "mybin";

        let fake_exe = bin_dir.join(fake_exe_name);
        fs::write(&fake_exe, b"echo ok\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_exe, fs::Permissions::from_mode(0o755)).unwrap();
            let non_exec = bin_dir.join("readme");
            fs::write(&non_exec, b"not exec").unwrap();
            fs::set_permissions(&non_exec, fs::Permissions::from_mode(0o644)).unwrap();
            assert!(resolve_executable_in_dirs("readme", std::slice::from_ref(&bin_dir)).is_none());
        }

        assert!(resolve_executable_in_dirs("mybin", std::slice::from_ref(&bin_dir)).is_some());
        assert!(resolve_executable_in_dirs("./mybin", std::slice::from_ref(&bin_dir)).is_none());
        assert!(resolve_executable_in_dirs("-mybin", std::slice::from_ref(&bin_dir)).is_none());

        let huge_stream = vec![b'a'; 1_000_000];
        let bounded = read_pipe_bounded(Cursor::new(huge_stream), MAX_CMD_OUTPUT_BYTES);
        assert_eq!(bounded.len(), MAX_CMD_OUTPUT_BYTES);

        // Cache key sanitization + fp_cache single evaluation per root exe + 0600 save permissions
        let cache_path = bin_dir.join("cli_cache_posix.json");
        let valid_rec = CachedChainRecord {
            schema_version: CLI_CACHE_SCHEMA_VERSION,
            dialect: Some("posix".to_string()),
            exe_fingerprint: Some((10, 20)),
            entries: vec![Entry::new("--help", "--help", EntryKind::Flag, "Help")],
        };
        let poisoned = HashMap::from([
            ("cargo", valid_rec.clone()),
            ("cargo::build", valid_rec.clone()),
            ("-rf", valid_rec.clone()),
            ("../evil", valid_rec.clone()),
            ("git::--exec-path=sh", valid_rec),
        ]);
        fs::write(&cache_path, serde_json::to_vec(&poisoned).unwrap()).unwrap();

        let fp_calls = Arc::new(AtomicUsize::new(0));
        let fp_calls_ref = Arc::clone(&fp_calls);
        let loaded = load_cli_disk_cache_with_fingerprint(
            &cache_path,
            Some(ShellDialect::Posix),
            move |exe| {
                assert_eq!(exe, "cargo");
                fp_calls_ref.fetch_add(1, Ordering::SeqCst);
                Some((10, 20))
            },
        );
        assert_eq!(fp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.contains_key("cargo"));
        assert!(loaded.contains_key("cargo::build"));

        save_cli_disk_cache_to(&cache_path, ShellDialect::Posix, &loaded);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir_mode = fs::metadata(&bin_dir).unwrap().permissions().mode() & 0o777;
            let file_mode = fs::metadata(&cache_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(dir_mode, 0o700);
            assert_eq!(file_mode, 0o600);
        }

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn test_inspect_command_chain_prepopulated_catalog_and_nested_subcommands() {
        let mut ps_cmd = Entry::new(
            "Get-Process",
            "Get-Process",
            EntryKind::Cmdlet,
            "Module: Microsoft.PowerShell.Management",
        );
        ps_cmd.children = vec![
            Entry::new(
                "-Id",
                "-Id (-PID)",
                EntryKind::Param,
                "[Int32[]] Process ID",
            ),
            Entry::new(
                "-Name",
                "-Name",
                EntryKind::Param,
                "[String[]] Process name",
            ),
        ];

        let mut deploy_sub = Entry::new("deploy", "deploy", EntryKind::Subcmd, "Deploy app");
        deploy_sub.children = vec![Entry::new(
            "--dry-run",
            "--dry-run",
            EntryKind::Flag,
            "Preview changes",
        )];

        let mut mycli = Entry::new("mycli", "mycli", EntryKind::Cli, "My CLI");
        mycli.children = vec![
            deploy_sub,
            Entry::new("status", "status", EntryKind::Subcmd, "Show status"),
        ];

        let mut db_migrate = Entry::new(
            "db:migrate",
            "db:migrate",
            EntryKind::Func,
            "Run DB migrations",
        );
        db_migrate.children = vec![Entry::new(
            "--step",
            "--step",
            EntryKind::Param,
            "Migration step count",
        )];

        let catalog = vec![
            Entry::scope_root("$env:", "$env:Variables", EntryKind::Env, "Env scope"),
            Entry::placeholder("Loading...", EntryKind::Loading, "Loading"),
            Entry::new("git", "git", EntryKind::History, "Recent shell history"),
            Entry::new("git", "git", EntryKind::Cli, "/usr/bin/git"),
            ps_cmd,
            mycli,
            db_migrate,
        ];

        // 1. Pre-populated PowerShell cmdlet matches case-insensitively and skips runner
        let ps_report = inspect_command_chain_with(
            ShellDialect::PowerShell,
            &catalog,
            &["get-process".to_string()],
            None,
            false,
            &|exe, _| panic!("pre-populated cmdlet should not invoke runner: {exe}"),
        )
        .unwrap();
        assert_eq!(ps_report.dialect, "pwsh");
        assert_eq!(ps_report.chain, vec!["get-process"]);
        let cmd = ps_report.command.as_ref().expect("matched root command");
        assert_eq!(cmd.text, "Get-Process");
        assert!(
            cmd.children.is_empty(),
            "command.children must be cleared to avoid duplicating entries"
        );
        let entry_texts: Vec<&str> = ps_report.entries.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(entry_texts, vec!["-Id", "-Name"]);

        // 2. Nested pre-populated subcommand walks children without spawning runner
        let deploy_report = inspect_command_chain_with(
            ShellDialect::Posix,
            &catalog,
            &["mycli".to_string(), "deploy".to_string()],
            None,
            false,
            &|exe, _| panic!("pre-populated nested subcmd should not invoke runner: {exe}"),
        )
        .unwrap();
        assert_eq!(deploy_report.dialect, "posix");
        assert_eq!(deploy_report.chain, vec!["mycli", "deploy"]);
        assert_eq!(deploy_report.entries.len(), 1);
        assert_eq!(deploy_report.entries[0].text, "--dry-run");
        assert!(deploy_report
            .command
            .as_ref()
            .is_some_and(|c| c.children.is_empty() && c.text == "mycli"));

        // 3. Nested subcommand with empty children falls back to CLI introspection
        let status_report = inspect_command_chain_with(
            ShellDialect::Posix,
            &catalog,
            &["mycli".to_string(), "status".to_string()],
            None,
            false,
            &|exe, args| match (exe, args) {
                ("mycli", ["status", "--help"]) => {
                    Some("Options:\n  -v, --verbose    Verbose status\n".to_string())
                }
                _ => None,
            },
        )
        .unwrap();
        assert!(status_report.entries.iter().any(|e| e.text == "--verbose"));

        // 4. Catalog function with ':' (valid shell ident) resolves safely when present
        let fn_report = inspect_command_chain_with(
            ShellDialect::Posix,
            &catalog,
            &["db:migrate".to_string()],
            None,
            false,
            &|exe, _| panic!("catalog func with children should not invoke runner: {exe}"),
        )
        .unwrap();
        assert_eq!(fn_report.entries[0].text, "--step");

        // 5. History entry does not shadow Cli entry with same text
        let git_report = inspect_command_chain_with(
            ShellDialect::Posix,
            &catalog,
            &["git".to_string()],
            None,
            false,
            &|exe, args| match (exe, args) {
                ("git", ["-h"]) => {
                    Some("start a working area\n   clone    Clone repo\n".to_string())
                }
                _ => None,
            },
        )
        .unwrap();
        let git_cmd = git_report.command.as_ref().expect("git matched");
        assert_eq!(git_cmd.kind, EntryKind::Cli);
        assert_eq!(git_cmd.desc, "/usr/bin/git");
        assert!(git_report.entries.iter().any(|e| e.text == "clone"));

        // 6. InspectReport PartialEq / Eq and JSON serialization shape
        assert_eq!(ps_report, ps_report.clone());
        assert_ne!(ps_report, deploy_report);
        let json_val = serde_json::to_value(&ps_report).unwrap();
        assert_eq!(json_val["dialect"], "pwsh");
        assert_eq!(json_val["command"]["children"], serde_json::json!([]));
        assert_eq!(json_val["entries"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_inspect_command_chain_tokenization_cache_hit_and_persistence() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let nonce = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let cache_path = env::temp_dir().join(format!(
            "zpane_test_inspect_cache_{}_{nonce}.json",
            std::process::id()
        ));
        let _ = fs::remove_file(&cache_path);

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);

        // Single whitespace-delimited string (`--inspect "  cargo   build  "`)
        let r1 = inspect_command_chain_with(
            ShellDialect::Fish,
            &[],
            &["  cargo   build  ".to_string()],
            Some(&cache_path),
            false,
            &move |exe, args| {
                calls_ref.fetch_add(1, Ordering::SeqCst);
                match (exe, args) {
                    ("cargo", ["build", "--help"]) => {
                        Some("Options:\n  -r, --release    Build in release mode\n".to_string())
                    }
                    _ => None,
                }
            },
        )
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(r1.dialect, "fish");
        assert_eq!(r1.chain, vec!["cargo", "build"]);
        assert!(r1.command.is_none());
        assert!(r1.entries.iter().any(|e| e.text == "--release"));
        assert!(cache_path.exists());

        // Multi-token form (`zpane inspect cargo build`) hits disk cache without calling runner
        let r2 = inspect_command_chain_with(
            ShellDialect::Fish,
            &[],
            &["cargo".to_string(), "build".to_string()],
            Some(&cache_path),
            false,
            &|_exe, _args| panic!("disk cache hit should satisfy inspect without spawning runner"),
        )
        .unwrap();
        assert_eq!(r1, r2);

        // Failed / unknown command must not poison disk cache with fallback entries
        let r_unknown = inspect_command_chain_with(
            ShellDialect::Fish,
            &[],
            &["unknown_nonexistent_tool".to_string()],
            Some(&cache_path),
            false,
            &|_exe, _args| None,
        )
        .unwrap();
        assert!(r_unknown.command.is_none());
        assert!(r_unknown
            .entries
            .iter()
            .any(|e| e.text == "--help" && e.desc.contains("unknown_nonexistent_tool")));

        let persisted = load_cli_disk_cache_from(&cache_path, ShellDialect::Fish);
        assert!(persisted.contains_key("cargo::build"));
        assert!(!persisted.contains_key("unknown_nonexistent_tool"));

        let _ = fs::remove_file(&cache_path);
    }

    #[test]
    fn test_inspect_command_chain_pentest_rejects_injection_traversal_bidi_and_oversized() {
        let oversized = "a".repeat(MAX_INSPECT_INPUT_BYTES + 1);
        let oversized_tokens = "a ".repeat(2500);
        let oversized_exe = "b".repeat(261);
        let oversized_sub = "c".repeat(129);

        let hostile_inputs: Vec<Vec<String>> = vec![
            vec![],
            vec!["".to_string()],
            vec!["   ".to_string()],
            vec!["git".to_string(), "".to_string()],
            vec!["-rf".to_string()],
            vec!["--upload-pack=calc".to_string()],
            vec!["git".to_string(), "-c".to_string()],
            vec!["git".to_string(), "--exec-path=/tmp/evil".to_string()],
            vec!["git --upload-pack=calc".to_string()],
            vec!["cmd".to_string(), "/c".to_string()],
            vec!["../evil".to_string()],
            vec!["./evil".to_string()],
            vec!["sub/evil".to_string()],
            vec!["sub\\evil".to_string()],
            vec!["C:evil.exe".to_string()],
            vec!["/usr/../bin/evil".to_string()],
            vec!["\\\\attacker\\share\\evil.exe".to_string()],
            vec!["//attacker/share/evil".to_string()],
            vec!["git".to_string(), "../sub".to_string()],
            vec!["git;id".to_string()],
            vec!["git|sh".to_string()],
            vec!["git&calc".to_string()],
            vec!["git$(whoami)".to_string()],
            vec!["git`id`".to_string()],
            vec!["git>out".to_string()],
            vec!["git%COMSPEC%".to_string()],
            vec!["git\ncommit".to_string()],
            vec!["git\tcommit".to_string()],
            vec!["git\0evil".to_string()],
            vec!["git\x1b[31m".to_string()],
            vec!["git\u{202e}commit".to_string()],
            vec!["git\u{061c}commit".to_string()],
            vec!["git\u{2028}commit".to_string()],
            vec!["git".to_string(), "sub\u{2066}x".to_string()],
            vec![oversized],
            vec![oversized_tokens],
            vec![oversized_exe],
            vec!["git".to_string(), oversized_sub],
            vec![
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
                "f".into(),
                "g".into(),
                "h".into(),
                "i".into(),
            ],
        ];

        for raw_chain in &hostile_inputs {
            let err = inspect_command_chain_with(
                ShellDialect::Posix,
                &[],
                raw_chain,
                None,
                true,
                &|exe, _| panic!("hostile inspect input spawned runner for '{exe}'"),
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                !msg.contains('\x1b') && !msg.contains('\0') && !msg.contains('\u{202e}'),
                "error message leaked unsafe control chars for {raw_chain:?}: {msg:?}"
            );
        }
    }
}
