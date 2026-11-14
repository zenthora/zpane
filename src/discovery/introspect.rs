use super::catalog::{
    cache_dir, ensure_parent_dir, hide_window, load_catalog, ps_metadata_path,
    sync_powershell_metadata, ShellDialect, PRIORITY_CLI_TOOLS,
};
use super::help_parser::parse_cli_help;
use crate::model::{Entry, EntryKind, RuntimeContext};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

#[derive(Debug, Clone)]
struct BgRequest {
    cache_key: String,
    cmd_chain: Vec<String>,
}

#[derive(Debug, Clone)]
struct BgResponse {
    cache_key: String,
    entries: Vec<Entry>,
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

const CLI_CACHE_SCHEMA_VERSION: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedChainRecord {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    exe_fingerprint: Option<(u64, u64)>,
    entries: Vec<Entry>,
}

fn find_executable_in_path(exe: &str) -> Option<PathBuf> {
    let trimmed = exe.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path_val = std::env::var_os("PATH")?;
    let has_ext = Path::new(trimmed).extension().is_some();
    for dir in std::env::split_paths(&path_val) {
        let direct = dir.join(trimmed);
        if direct.is_file() {
            return Some(direct);
        }
        if cfg!(windows) && !has_ext {
            for ext in ["exe", "cmd", "bat"] {
                let candidate = dir.join(format!("{trimmed}.{ext}"));
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }
    None
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

fn cli_cache_path() -> PathBuf {
    cache_dir().join("cli_cache.json")
}

/// Removes persisted CLI help (`cli_cache.json`) and PowerShell metadata (`ps_metadata.json`) caches.
pub fn clear_disk_cache() -> anyhow::Result<usize> {
    let mut removed = 0usize;
    for path in [cli_cache_path(), ps_metadata_path()] {
        if path.exists() {
            fs::remove_file(&path)?;
            removed += 1;
        }
    }
    Ok(removed)
}

fn load_cli_disk_cache_with_fingerprint<F>(path: &Path, fp_fn: F) -> HashMap<String, Vec<Entry>>
where
    F: Fn(&str) -> Option<(u64, u64)>,
{
    let Ok(bytes) = fs::read(path) else {
        return HashMap::new();
    };
    let Ok(records) = serde_json::from_slice::<HashMap<String, CachedChainRecord>>(&bytes) else {
        return HashMap::new();
    };

    let mut out = HashMap::new();
    for (key, rec) in records {
        if rec.schema_version != CLI_CACHE_SCHEMA_VERSION {
            continue;
        }
        if let Some(saved_fp) = rec.exe_fingerprint {
            let root_exe = key.split("::").next().unwrap_or(key.as_str());
            if let Some(cur_fp) = fp_fn(root_exe) {
                if saved_fp != cur_fp {
                    continue;
                }
            }
        }
        out.insert(key, rec.entries);
    }
    out
}

fn load_cli_disk_cache_from(path: &Path) -> HashMap<String, Vec<Entry>> {
    load_cli_disk_cache_with_fingerprint(path, executable_fingerprint)
}

fn save_cli_disk_cache_to(path: &Path, cache: &HashMap<String, Vec<Entry>>) {
    let mut records: HashMap<String, CachedChainRecord> = HashMap::with_capacity(cache.len());
    let mut fp_cache: HashMap<String, Option<(u64, u64)>> = HashMap::new();
    for (key, entries) in cache {
        let root_exe = key.split("::").next().unwrap_or(key.as_str());
        let fp = *fp_cache
            .entry(root_exe.to_string())
            .or_insert_with(|| executable_fingerprint(root_exe));
        records.insert(
            key.clone(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                exe_fingerprint: fp,
                entries: entries.clone(),
            },
        );
    }
    if let Ok(json) = serde_json::to_vec(&records) {
        ensure_parent_dir(path);
        let _ = fs::write(path, json);
    }
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
                    let _ = child.wait();
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

/// Adapter signature for running a CLI command (`exe`, `args`) and returning combined help text.
type CommandRunner = dyn Fn(&str, &[&str]) -> Option<String> + Send + Sync;

fn default_command_runner() -> Arc<CommandRunner> {
    Arc::new(|exe: &str, args: &[&str]| {
        let out = run_command_with_timeout(exe, args, 450);
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
    let Some(path_val) = std::env::var_os("PATH") else {
        return false;
    };
    let candidates: &[&str] = if cfg!(windows) {
        &["carapace.exe", "carapace.cmd", "carapace.bat"]
    } else {
        &["carapace"]
    };
    std::env::split_paths(&path_val).any(|dir| candidates.iter().any(|bin| dir.join(bin).is_file()))
}

fn parse_carapace_export(json_str: &str) -> Vec<Entry> {
    let trimmed = json_str.trim();
    let Some(start) = trimmed.find('{') else {
        return Vec::new();
    };
    let Ok(export) = serde_json::from_str::<CarapaceExport>(&trimmed[start..]) else {
        return Vec::new();
    };

    let mut entries = Vec::with_capacity(export.values.len());
    let mut seen = HashSet::new();
    for item in export.values {
        let text = item.value.trim().to_string();
        if text.is_empty() || !seen.insert(text.to_ascii_lowercase()) {
            continue;
        }
        let label = if item.display.trim().is_empty() {
            text.clone()
        } else {
            item.display.trim().to_string()
        };
        let tag_lower = item.tag.to_ascii_lowercase();
        let kind = if text.starts_with('-') || tag_lower.contains("flag") {
            if text.ends_with('=') {
                EntryKind::Param
            } else {
                EntryKind::Flag
            }
        } else if tag_lower.contains("command") || tag_lower.contains("subcommand") {
            EntryKind::Subcmd
        } else if tag_lower.contains("directory")
            || tag_lower.contains("dir")
            || tag_lower.contains("file")
        {
            EntryKind::Path
        } else if tag_lower.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            EntryKind::Subcmd
        } else {
            EntryKind::Arg
        };

        entries.push(Entry::new(text, label, kind, item.description.trim()));
    }
    entries
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
    if chain.is_empty() {
        return Vec::new();
    }
    let exe = chain[0];
    let sub_args: Vec<&str> = chain[1..].to_vec();

    // Note: For git, always prefer `-h` so git never spawns a web browser on Windows!
    let primary_flag = if exe.eq_ignore_ascii_case("git") {
        "-h"
    } else {
        "--help"
    };

    let mut args1 = sub_args.clone();
    args1.push(primary_flag);
    let mut text = run_cmd(exe, &args1).unwrap_or_default();

    if text.trim().len() < 25 && primary_flag != "-h" {
        let mut args2 = sub_args;
        args2.push("-h");
        if let Some(fallback_text) = run_cmd(exe, &args2) {
            text = fallback_text;
        }
    }

    let mut entries = parse_cli_help(&text);

    // Secondary Introspection Bridge: query `carapace <exe> export <chain...> ""`
    // when primary help parsing yields nothing, or on subcommand chains when `--help`
    // returned only flags/params and no subcommands or dynamic value candidates.
    let needs_carapace = carapace_enabled
        && !exe.eq_ignore_ascii_case("carapace")
        && (entries.is_empty()
            || (chain.len() >= 2
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
        let mut c_args = Vec::with_capacity(chain.len() + 3);
        c_args.push(exe);
        c_args.push("export");
        c_args.extend_from_slice(chain);
        c_args.push("");
        if let Some(carapace_out) = run_cmd("carapace", &c_args) {
            let carapace_entries = parse_carapace_export(&carapace_out);
            if entries.is_empty() {
                entries = carapace_entries;
            } else if !carapace_entries.is_empty() {
                let mut merged = Vec::with_capacity(entries.len() + carapace_entries.len());
                let mut seen: HashSet<String> = HashSet::new();
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
                    if seen.insert(item.text.to_ascii_lowercase()) {
                        merged.push(item);
                    }
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

    let vars = dialect.common_var_entries();

    // If this is a subcommand or leaf level, also append helpful shell variables
    if chain.len() >= 2 {
        let mut existing: HashSet<String> = entries.iter().map(|i| i.text.clone()).collect();
        for cv in vars {
            if existing.insert(cv.text.clone()) {
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
        entries.extend(vars);
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
    /// Creates a production `CliIntrospector` backed by `cli_cache.json` and a background OS worker thread.
    /// When running on PowerShell without cached `ps_metadata.json`, starts first-run metadata generation
    /// in the background so `poll_updates()` delivers the refreshed catalog live.
    pub fn new(dialect: ShellDialect, runtime_ctx: &RuntimeContext) -> Self {
        let path = cli_cache_path();
        let cache = load_cli_disk_cache_from(&path);
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
        let path = cli_cache_path();
        let cache = load_cli_disk_cache_from(&path);
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
            .map(load_cli_disk_cache_from)
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
        chain
            .iter()
            .map(|s| s.trim().to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join("::")
    }

    /// Queries the cache for `chain`. On cache hit, returns the cached entries.
    /// On cache miss, enqueues a deduplicated background introspection job and returns a loading placeholder entry.
    pub fn query(&mut self, chain: &[&str]) -> Vec<Entry> {
        if chain.is_empty() {
            return Vec::new();
        }
        let key = Self::chain_key(chain);
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }

        if let Some(ref tx) = self.req_tx {
            if self.pending_keys.insert(key.clone()) {
                let _ = tx.send(BgRequest {
                    cache_key: key,
                    cmd_chain: chain.iter().map(|s| (*s).to_string()).collect(),
                });
            }
        }

        vec![Self::loading_entry(chain)]
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
                    self.record(resp);
                    updated = true;
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
        let updated = !drained.is_empty();
        for resp in drained {
            self.record(resp);
        }
        updated
    }

    fn record(&mut self, resp: BgResponse) {
        self.pending_keys.remove(&resp.cache_key);
        self.cache.insert(resp.cache_key, resp.entries);
        self.dirty = true;
    }

    /// Persists the cache to disk if and only if new entries were recorded.
    pub fn flush(&mut self) -> bool {
        if !self.dirty {
            return false;
        }
        if let Some(ref path) = self.cache_path {
            save_cli_disk_cache_to(path, &self.cache);
        }
        self.dirty = false;
        true
    }

    /// Invalidates all cached introspection chains rooted at `exe` so subsequent queries re-run `--help`.
    pub fn invalidate_command(&mut self, exe: &str) {
        let root = exe.trim().to_ascii_lowercase();
        if root.is_empty() {
            return;
        }
        let prefix = format!("{root}::");
        let before = self.cache.len();
        self.cache
            .retain(|k, _| k != &root && !k.starts_with(&prefix));
        self.pending_keys
            .retain(|k| k != &root && !k.starts_with(&prefix));
        if self.cache.len() != before {
            self.dirty = true;
        }
    }

    /// Pre-warms the CLI cache in parallel for all installed tools in `PRIORITY_CLI_TOOLS`
    /// using the canonical cache key format. Returns the number of installed priority tools warmed.
    fn warm_priority_tools(&mut self, col1_entries: &[Entry]) -> usize {
        let installed: HashSet<String> = col1_entries
            .iter()
            .filter(|it| it.kind == EntryKind::Cli)
            .map(|it| it.text.to_ascii_lowercase())
            .collect();

        let available: Vec<&str> = PRIORITY_CLI_TOOLS
            .iter()
            .copied()
            .filter(|t| installed.contains(&t.to_ascii_lowercase()))
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

        // 1. Write stale schema_version and mismatched exe_fingerprint records
        let mut raw_records = HashMap::new();
        raw_records.insert(
            "stale_ver".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION - 1,
                exe_fingerprint: Some((100, 200)),
                entries: vec![Entry::new("old", "old", EntryKind::Subcmd, "")],
            },
        );
        raw_records.insert(
            "upgraded_bin".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                exe_fingerprint: Some((100, 200)),
                entries: vec![Entry::new("old-bin", "old-bin", EntryKind::Subcmd, "")],
            },
        );
        raw_records.insert(
            "fresh_bin::sub".to_string(),
            CachedChainRecord {
                schema_version: CLI_CACHE_SCHEMA_VERSION,
                exe_fingerprint: Some((555, 999)),
                entries: vec![Entry::new("ok", "ok", EntryKind::Subcmd, "")],
            },
        );
        fs::write(&temp_path, serde_json::to_vec(&raw_records).unwrap()).unwrap();

        let loaded = load_cli_disk_cache_with_fingerprint(&temp_path, |exe| match exe {
            "upgraded_bin" => Some((101, 200)),
            "fresh_bin" => Some((555, 999)),
            _ => None,
        });
        assert!(!loaded.contains_key("stale_ver"));
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
}
