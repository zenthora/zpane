use super::catalog::{
    cache_dir, ensure_parent_dir, hide_window, load_catalog, ps_metadata_path,
    sync_powershell_metadata, ShellDialect, PRIORITY_CLI_TOOLS,
};
use super::help_parser::parse_cli_help;
use crate::model::{Entry, EntryKind};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

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
}

fn cli_cache_path() -> PathBuf {
    cache_dir().join("cli_cache.json")
}

fn load_cli_disk_cache_from(path: &Path) -> HashMap<String, Vec<Entry>> {
    if let Ok(bytes) = fs::read(path) {
        if let Ok(map) = serde_json::from_slice::<HashMap<String, Vec<Entry>>>(&bytes) {
            return map;
        }
    }
    HashMap::new()
}

fn save_cli_disk_cache_to(path: &Path, cache: &HashMap<String, Vec<Entry>>) {
    if let Ok(json) = serde_json::to_vec(cache) {
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

fn introspect_cli_chain_with<F>(chain: &[&str], dialect: ShellDialect, run_cmd: &F) -> Vec<Entry>
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
    pub fn new(dialect: ShellDialect) -> Self {
        let path = cli_cache_path();
        let cache = load_cli_disk_cache_from(&path);
        let runner = default_command_runner();
        let catalog_rx = maybe_spawn_ps_sync(dialect);
        Self::build(dialect, cache, Some(path), runner, true, catalog_rx)
    }

    /// Creates a `CliIntrospector` tailored for synchronous batch cache warming (no worker threads).
    fn new_for_warm(dialect: ShellDialect) -> Self {
        let path = cli_cache_path();
        let cache = load_cli_disk_cache_from(&path);
        let runner = default_command_runner();
        Self::build(dialect, cache, Some(path), runner, false, None)
    }

    /// Creates a `CliIntrospector` with an injected command-output adapter and optional disk cache path.
    #[cfg(test)]
    fn with_runner<F>(dialect: ShellDialect, cache_path: Option<PathBuf>, runner: F) -> Self
    where
        F: Fn(&str, &[&str]) -> Option<String> + Send + Sync + 'static,
    {
        let cache = cache_path
            .as_deref()
            .map(load_cli_disk_cache_from)
            .unwrap_or_default();
        Self::build(dialect, cache, cache_path, Arc::new(runner), true, None)
    }

    fn build(
        dialect: ShellDialect,
        cache: HashMap<String, Vec<Entry>>,
        cache_path: Option<PathBuf>,
        runner: Arc<CommandRunner>,
        spawn_worker: bool,
        catalog_rx: Option<Receiver<Vec<Entry>>>,
    ) -> Self {
        let (req_tx, res_rx) = if spawn_worker {
            let (req_tx, req_rx) = mpsc::channel::<BgRequest>();
            let (res_tx, res_rx) = mpsc::channel::<BgResponse>();
            let worker_runner = Arc::clone(&runner);
            spawn_background_worker(req_rx, res_tx, dialect, worker_runner);
            (Some(req_tx), Some(res_rx))
        } else {
            (None, None)
        };

        Self {
            cache,
            pending_keys: HashSet::new(),
            dirty: false,
            dialect,
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
        let runner = Arc::clone(&self.runner);
        let results: Vec<(String, Vec<Entry>)> = available
            .par_iter()
            .map(|&tool| {
                let chain = [tool];
                let entries = introspect_cli_chain_with(&chain, dialect, &*runner);
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
}

fn maybe_spawn_ps_sync(dialect: ShellDialect) -> Option<Receiver<Vec<Entry>>> {
    if !dialect.is_powershell() || ps_metadata_path().exists() {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        if sync_powershell_metadata().is_ok() {
            let _ = tx.send(load_catalog(dialect));
        }
    });
    Some(rx)
}

fn spawn_background_worker(
    rx: Receiver<BgRequest>,
    tx: Sender<BgResponse>,
    dialect: ShellDialect,
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
                let entries = introspect_cli_chain_with(&chain_refs, dialect, &*runner);
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
    let col1_entries = load_catalog(dialect);
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
}
