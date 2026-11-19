use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

struct TempWorkspace {
    root: PathBuf,
}

impl TempWorkspace {
    fn new(prefix: &str) -> Self {
        let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "zpane_it_{prefix}_{}_{}_{}",
            std::process::id(),
            nanos,
            seq
        ));
        fs::create_dir_all(&root).expect("create isolated temp workspace");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }

    fn cache_root(&self) -> PathBuf {
        if cfg!(windows) {
            self.root.join("localappdata").join("zpane")
        } else {
            self.root.join("xdg_cache").join("zpane")
        }
    }

    fn specs_dir(&self) -> PathBuf {
        self.cache_root().join("specs")
    }
}

impl Drop for TempWorkspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn zpane_bin() -> &'static str {
    env!("CARGO_BIN_EXE_zpane")
}

fn isolated_cmd(ws: &TempWorkspace) -> Command {
    let localappdata = ws.path().join("localappdata");
    let home = ws.path().join("home");
    let xdg_cache = ws.path().join("xdg_cache");
    let xdg_config = ws.path().join("xdg_config");
    let empty_bin = ws.path().join("empty_bin");
    for dir in [&localappdata, &home, &xdg_cache, &xdg_config, &empty_bin] {
        fs::create_dir_all(dir).expect("create isolated env dir");
    }

    let mut cmd = Command::new(zpane_bin());
    cmd.env("LOCALAPPDATA", &localappdata)
        .env("USERPROFILE", &home)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", &xdg_cache)
        .env("XDG_CONFIG_HOME", &xdg_config)
        .env("ZPANE_CONFIG", ws.path().join("config.toml"))
        .env("PATH", &empty_bin)
        .env_remove("ZPANE_SHELL")
        .env_remove("ZDOTDIR")
        .env_remove("PSModulePath")
        .env_remove("FISH_VERSION")
        .env_remove("SHELL");
    cmd
}

fn run_isolated(ws: &TempWorkspace, args: &[&str]) -> Output {
    isolated_cmd(ws)
        .args(args)
        .output()
        .expect("execute zpane binary")
}

fn write_demo_spec(ws: &TempWorkspace, name: &str) {
    let specs = ws.specs_dir();
    fs::create_dir_all(&specs).expect("create specs dir");
    let spec_json = serde_json::json!({
        "name": name,
        "description": "Integration test CLI spec",
        "commands": [
            {
                "name": "remote",
                "aliases": ["rm"],
                "description": "Manage remotes",
                "commands": [
                    {
                        "name": "add",
                        "aliases": ["new"],
                        "description": "Add a remote",
                        "flags": {
                            "--tags": "Import tags"
                        }
                    }
                ]
            }
        ]
    });
    fs::write(
        specs.join(format!("{name}.json")),
        serde_json::to_vec(&spec_json).expect("serialize demo spec"),
    )
    .expect("write demo spec");
}

#[test]
fn cli_version_prints_package_version_with_clean_stderr() {
    let ws = TempWorkspace::new("version");
    let expected = format!("zpane {}", env!("CARGO_PKG_VERSION"));

    for flag in ["--version", "-V"] {
        let out = run_isolated(&ws, &[flag]);
        assert!(out.status.success(), "{flag} failed: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{flag} wrote unexpected stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        assert_eq!(stdout.trim(), expected);
    }
}

#[test]
fn cli_help_prints_usage_and_env_vars_with_clean_stderr() {
    let ws = TempWorkspace::new("help");

    for args in [&["--help"][..], &["-h"][..], &["init", "--help"][..]] {
        let out = run_isolated(&ws, args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote unexpected stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        assert!(stdout.contains("Usage: zpane"), "missing usage: {stdout}");
        assert!(stdout.contains("zpane init <pwsh|zsh|bash|fish>"));
        assert!(stdout.contains("zpane inspect [--json] <CMD>..."));
        assert!(stdout.contains("ZPANE_SHELL"));
        assert!(stdout.contains("ZPANE_CONFIG"));
    }
}

#[test]
fn cli_init_emits_shell_hooks_with_clean_stderr_and_rejects_unknown_shell() {
    let ws = TempWorkspace::new("init");

    for (args, needle) in [
        (&["init", "pwsh"][..], "Set-PSReadLineKeyHandler"),
        (&["init", "zsh"][..], "_zpane_widget"),
        (&["init", "bash"][..], "_zpane_readline"),
        (&["init", "fish"][..], "function _zpane_fish"),
        (&["--init=bash"][..], "bind -x"),
    ] {
        let out = run_isolated(&ws, args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote unexpected stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        assert!(
            stdout.contains(needle) && stdout.contains("--out-file"),
            "unexpected init script for {args:?}: {stdout}"
        );
    }

    let bad = run_isolated(&ws, &["init", "nushell"]);
    assert!(!bad.status.success(), "unknown shell must fail");
    assert!(
        bad.stdout.is_empty(),
        "failed init must not write stdout: {:?}",
        String::from_utf8_lossy(&bad.stdout)
    );
    let stderr = String::from_utf8_lossy(&bad.stderr);
    assert!(
        stderr.contains("Unknown shell target"),
        "unexpected stderr: {stderr}"
    );
}

#[test]
fn cli_inspect_json_and_human_resolve_subcommand_aliases_with_stream_purity() {
    let ws = TempWorkspace::new("inspect");
    write_demo_spec(&ws, "zpanedemo");

    // 1. JSON inspect of root spec command under ZPANE_SHELL=posix
    let out_root = isolated_cmd(&ws)
        .env("ZPANE_SHELL", "posix")
        .args(["inspect", "--json", "zpanedemo"])
        .output()
        .expect("run inspect --json");
    assert!(out_root.status.success(), "inspect failed: {out_root:?}");
    assert!(
        out_root.stderr.is_empty(),
        "inspect --json wrote stderr: {:?}",
        String::from_utf8_lossy(&out_root.stderr)
    );
    let root_json: Value = serde_json::from_slice(&out_root.stdout).expect("valid inspect JSON");
    assert_eq!(root_json["dialect"], "posix");
    assert_eq!(root_json["chain"], serde_json::json!(["zpanedemo"]));
    assert_eq!(root_json["command"]["text"], "zpanedemo");
    assert!(root_json["entries"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|e| e["text"] == "remote")));

    // 2. JSON inspect walking subcommand aliases (`rm` -> `remote`, `new` -> `add`)
    let out_alias = isolated_cmd(&ws)
        .env("ZPANE_SHELL", "fish")
        .args(["inspect", "--json", "zpanedemo", "rm", "new"])
        .output()
        .expect("run inspect --json with aliases");
    assert!(out_alias.status.success(), "inspect alias failed");
    assert!(out_alias.stderr.is_empty());
    let alias_json: Value =
        serde_json::from_slice(&out_alias.stdout).expect("valid alias inspect JSON");
    assert_eq!(alias_json["dialect"], "fish");
    assert_eq!(
        alias_json["chain"],
        serde_json::json!(["zpanedemo", "rm", "new"])
    );
    assert!(alias_json["entries"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|e| e["text"] == "--tags")));

    // 3. Human inspect output writes cleanly to stdout with empty stderr
    let out_human = isolated_cmd(&ws)
        .env("ZPANE_SHELL", "posix")
        .args(["inspect", "zpanedemo", "remote"])
        .output()
        .expect("run human inspect");
    assert!(out_human.status.success());
    assert!(out_human.stderr.is_empty());
    let human_stdout = String::from_utf8(out_human.stdout).expect("utf8 stdout");
    assert!(human_stdout.starts_with("Command: zpanedemo remote (dialect: posix, entries:"));
    assert!(human_stdout.contains("add"));
}

#[test]
fn cli_clear_cache_json_and_human_remove_files_with_expected_streams() {
    let ws = TempWorkspace::new("clear_cache");
    let cache = ws.cache_root();
    fs::create_dir_all(&cache).expect("create cache dir");
    let f1 = cache.join("cli_cache_posix.json");
    let f2 = cache.join("cli_cache_pwsh.json");
    let f3 = cache.join("ps_metadata.json");
    for f in [&f1, &f2, &f3] {
        fs::write(f, b"{}").expect("seed cache file");
    }

    let first = run_isolated(&ws, &["--clear-cache", "--json"]);
    assert!(first.status.success(), "clear-cache failed: {first:?}");
    assert!(
        first.stderr.is_empty(),
        "--clear-cache --json wrote stderr: {:?}",
        String::from_utf8_lossy(&first.stderr)
    );
    let json: Value = serde_json::from_slice(&first.stdout).expect("valid clear-cache json");
    assert_eq!(json, serde_json::json!({ "cleared": 3 }));
    assert!(!f1.exists() && !f2.exists() && !f3.exists());

    let second = run_isolated(&ws, &["--clear-cache", "--json"]);
    assert!(second.status.success());
    assert!(second.stderr.is_empty());
    let json2: Value = serde_json::from_slice(&second.stdout).expect("valid second json");
    assert_eq!(json2, serde_json::json!({ "cleared": 0 }));

    let human = run_isolated(&ws, &["--clear-cache"]);
    assert!(human.status.success());
    assert!(
        human.stdout.is_empty(),
        "human --clear-cache must keep stdout empty"
    );
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(stderr.contains("Cleared 0 cached metadata file(s)."));
}

#[test]
fn cli_invalid_flags_and_hostile_args_exit_nonzero_and_sanitize_stderr() {
    let ws = TempWorkspace::new("invalid_flags");

    for args in [
        &["--definitely-unknown-flag"][..],
        &["--json"][..],
        &["--clear-cache", "--warm"][..],
        &["--inspect="][..],
        &["inspect"][..],
        &["--width", "0"][..],
    ] {
        let out = run_isolated(&ws, args);
        assert!(
            !out.status.success(),
            "{args:?} unexpectedly succeeded: {out:?}"
        );
        assert!(
            out.stdout.is_empty(),
            "{args:?} wrote stdout on error: {:?}",
            String::from_utf8_lossy(&out.stdout)
        );
        assert!(
            !out.stderr.is_empty(),
            "{args:?} produced empty stderr on error"
        );
    }

    let hostile = run_isolated(&ws, &["--bad\x1b[31m\x07\u{202e}flag"]);
    assert!(!hostile.status.success());
    assert!(hostile.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&hostile.stderr);
    assert!(stderr.contains("Unknown option"));
    assert!(!stderr.contains('\x1b'));
    assert!(!stderr.contains('\x07'));
    assert!(!stderr.contains('\u{202e}'));
}

#[test]
fn cli_out_file_rejects_invalid_targets_and_truncates_existing_file_without_tty() {
    let ws = TempWorkspace::new("out_file");

    // 1. Empty --out-file path fails at CLI validation before TTY access.
    let empty_out = run_isolated(&ws, &["--out-file", ""]);
    assert!(!empty_out.status.success());
    assert!(empty_out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&empty_out.stderr).contains("must not be empty"));

    // 2. Directory passed to --out-file is rejected before TTY access.
    let dir_target = ws.path().join("out_dir");
    fs::create_dir_all(&dir_target).expect("create dir target");
    let dir_out = run_isolated(&ws, &["--out-file", dir_target.to_str().unwrap()]);
    assert!(!dir_out.status.success());
    assert!(dir_out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&dir_out.stderr).contains("not a regular file"));

    // 3. Unix symlink passed to --out-file is rejected before TTY access and leaves target intact.
    #[cfg(unix)]
    {
        let target = ws.path().join("sym_target.txt");
        let link = ws.path().join("sym_link.txt");
        fs::write(&target, b"keep-me").expect("write target");
        std::os::unix::fs::symlink(&target, &link).expect("create symlink");
        let sym_out = run_isolated(&ws, &["--out-file", link.to_str().unwrap()]);
        assert!(!sym_out.status.success());
        assert!(sym_out.stdout.is_empty());
        assert!(String::from_utf8_lossy(&sym_out.stderr).contains("symlink"));
        assert_eq!(fs::read_to_string(&target).unwrap(), "keep-me");
    }

    // 4. Pre-existing regular file is truncated before TerminalGuard::enter() when headless;
    //    a fresh non-existent path is not created when interactive startup fails without a TTY.
    let headless_safe = {
        #[cfg(windows)]
        {
            true
        }
        #[cfg(unix)]
        {
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tty")
                .is_err()
        }
    };
    if headless_safe {
        fn detached_cmd(ws: &TempWorkspace) -> Command {
            let mut cmd = isolated_cmd(ws);
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                cmd.creation_flags(DETACHED_PROCESS);
            }
            cmd
        }

        let existing = ws.path().join("existing_out.txt");
        fs::write(&existing, b"stale buffer").expect("write existing out file");
        let trunc_out = detached_cmd(&ws)
            .args(["--out-file", existing.to_str().unwrap()])
            .output()
            .expect("run detached --out-file existing");
        assert!(!trunc_out.status.success());
        assert!(trunc_out.stdout.is_empty());
        assert_eq!(
            fs::read_to_string(&existing).expect("read truncated out file"),
            ""
        );

        let fresh = ws.path().join("fresh_out.txt");
        let fresh_out = detached_cmd(&ws)
            .args(["--out-file", fresh.to_str().unwrap()])
            .output()
            .expect("run detached --out-file fresh");
        assert!(!fresh_out.status.success());
        assert!(fresh_out.stdout.is_empty());
        assert!(
            !fresh.exists(),
            "non-existent --out-file should not be created when TUI never starts"
        );
    }
}

#[test]
fn cli_env_overrides_zpane_shell_and_zpane_config_control_behavior() {
    let ws = TempWorkspace::new("env_overrides");
    write_demo_spec(&ws, "zpanedemo");

    // 1. ZPANE_SHELL overrides detected dialect across all supported values.
    for (env_val, expected_tag) in [("pwsh", "pwsh"), ("posix", "posix"), ("fish", "fish")] {
        let out = isolated_cmd(&ws)
            .env("ZPANE_SHELL", env_val)
            .args(["inspect", "--json", "zpanedemo"])
            .output()
            .expect("run inspect with ZPANE_SHELL");
        assert!(out.status.success());
        assert!(out.stderr.is_empty());
        let json: Value = serde_json::from_slice(&out.stdout).expect("valid json");
        assert_eq!(json["dialect"], expected_tag);
        assert_eq!(json["command"]["text"], "zpanedemo");
    }

    // 2. ZPANE_CONFIG pointing to a config with disabled_tools hides the spec entry.
    let disabled_cfg = ws.path().join("custom_disabled.toml");
    fs::write(&disabled_cfg, "disabled_tools = [\"zpanedemo\"]\n").expect("write ZPANE_CONFIG");

    let disabled_out = isolated_cmd(&ws)
        .env("ZPANE_SHELL", "posix")
        .env("ZPANE_CONFIG", &disabled_cfg)
        .args(["inspect", "--json", "zpanedemo"])
        .output()
        .expect("run inspect with disabled tool in ZPANE_CONFIG");
    assert!(disabled_out.status.success());
    assert!(disabled_out.stderr.is_empty());
    let disabled_json: Value = serde_json::from_slice(&disabled_out.stdout).expect("valid json");
    assert!(
        disabled_json["command"].is_null(),
        "ZPANE_CONFIG disabled_tools should suppress catalog command: {disabled_json}"
    );
    assert!(
        !disabled_json["entries"]
            .as_array()
            .is_some_and(|arr| arr.iter().any(|e| e["text"] == "remote")),
        "disabled spec command must not expose spec subcommands: {disabled_json}"
    );

    // 3. Switching ZPANE_CONFIG to an enabled config restores discovery.
    let enabled_cfg = ws.path().join("custom_enabled.toml");
    fs::write(&enabled_cfg, "disabled_tools = []\n").expect("write enabled ZPANE_CONFIG");

    let enabled_out = isolated_cmd(&ws)
        .env("ZPANE_SHELL", "posix")
        .env("ZPANE_CONFIG", &enabled_cfg)
        .args(["inspect", "--json", "zpanedemo"])
        .output()
        .expect("run inspect with enabled ZPANE_CONFIG");
    assert!(enabled_out.status.success());
    assert!(enabled_out.stderr.is_empty());
    let enabled_json: Value = serde_json::from_slice(&enabled_out.stdout).expect("valid json");
    assert_eq!(enabled_json["command"]["text"], "zpanedemo");
    assert!(enabled_json["entries"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|e| e["text"] == "remote")));
}
