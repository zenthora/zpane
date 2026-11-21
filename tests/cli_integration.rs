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
        .env_remove("NU_VERSION")
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

    for args in [
        &["--help"][..],
        &["-h"][..],
        &["init", "--help"][..],
        &["doctor", "--help"][..],
    ] {
        let out = run_isolated(&ws, args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote unexpected stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        assert!(stdout.contains("Usage: zpane"), "missing usage: {stdout}");
        assert!(stdout.contains("zpane init <pwsh|zsh|bash|fish|nu>"));
        assert!(stdout.contains("zpane doctor [--json]"));
        assert!(stdout.contains("--doctor"));
        assert!(stdout.contains("--cursor <OFFSET>"));
        assert!(stdout.contains("--height <ROWS|PCT>"));
        assert!(stdout.contains("--bind <CHORD>"));
        assert!(stdout.contains("--no-bind"));
        assert!(stdout.contains("zpane inspect [--json] <CMD>..."));
        assert!(stdout.contains("ZPANE_SHELL"));
        assert!(stdout.contains("ZPANE_CONFIG"));
        assert!(stdout.contains("NO_COLOR"));
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
        (&["init", "nu"][..], "commandline edit --replace"),
        (&["init", "nushell"][..], "$env.config.keybindings"),
        (&["--init=bash"][..], "bind -x"),
        (&["--init=nu", "--bind", "Ctrl+g"][..], "keycode: char_g"),
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
            stdout.contains(needle) && stdout.contains("--cursor") && stdout.contains("--out-file"),
            "unexpected init script for {args:?}: {stdout}"
        );
    }

    // `--no-bind` defines widgets without registering default bindings across all shells.
    for (args, def_needle, forbidden_bind) in [
        (
            &["init", "pwsh", "--no-bind"][..],
            "function Invoke-ZpaneWidget",
            "Set-PSReadLineKeyHandler",
        ),
        (
            &["init", "zsh", "--no-bind"][..],
            "zle -N _zpane_widget",
            "bindkey ",
        ),
        (
            &["init", "bash", "--no-bind"][..],
            "_zpane_readline()",
            "bind -x",
        ),
        (
            &["init", "fish", "--no-bind"][..],
            "function _zpane_fish",
            "\nbind ",
        ),
        (
            &["init", "nu", "--no-bind"][..],
            "def _zpane_widget []",
            "$env.config.keybindings",
        ),
    ] {
        let out = run_isolated(&ws, args);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(out.stderr.is_empty());
        let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
        assert!(stdout.contains(def_needle), "{args:?}: {stdout}");
        assert!(!stdout.contains(forbidden_bind), "{args:?}: {stdout}");
    }

    let bad = run_isolated(&ws, &["init", "tcsh"]);
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

    // Conflicting `--bind` + `--no-bind` and malicious bind injection must fail cleanly.
    for bad_init in [
        &["init", "zsh", "--bind", "Ctrl+g", "--no-bind"][..],
        &["--init=pwsh", "--bind=Alt+m'; rm -rf /"][..],
    ] {
        let out = run_isolated(&ws, bad_init);
        assert!(!out.status.success(), "{bad_init:?} must fail");
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
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
    let f3 = cache.join("cli_cache_nu.json");
    let f4 = cache.join("ps_metadata.json");
    for f in [&f1, &f2, &f3, &f4] {
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
    assert_eq!(json, serde_json::json!({ "cleared": 4 }));
    assert!(!f1.exists() && !f2.exists() && !f3.exists() && !f4.exists());

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
        &["--height", "0%"][..],
        &["--height", "101%"][..],
        &["--height", "11"][..],
        &["--height", "abc"][..],
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
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const DETACHED_PROCESS: u32 = 0x0000_0008;
                let mut cmd = isolated_cmd(ws);
                cmd.creation_flags(DETACHED_PROCESS);
                cmd
            }
            #[cfg(not(windows))]
            {
                isolated_cmd(ws)
            }
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
    for (env_val, expected_tag) in [
        ("pwsh", "pwsh"),
        ("posix", "posix"),
        ("fish", "fish"),
        ("nu", "nu"),
        ("nushell", "nu"),
    ] {
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

    // 1b. NU_VERSION auto-detects Nushell when ZPANE_SHELL is unset.
    let nu_auto = isolated_cmd(&ws)
        .env("NU_VERSION", "0.100.0")
        .args(["inspect", "--json", "zpanedemo"])
        .output()
        .expect("run inspect with NU_VERSION");
    assert!(nu_auto.status.success());
    assert!(nu_auto.stderr.is_empty());
    let nu_json: Value = serde_json::from_slice(&nu_auto.stdout).expect("valid nu json");
    assert_eq!(nu_json["dialect"], "nu");

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

#[test]
fn cli_cursor_validation_and_session_suffix_preservation() {
    use zpane::discovery::{Introspect, PollResult, ShellDialect};
    use zpane::model::{Entry, EntryKind};
    use zpane::session::{MillerSession, SessionOutcome, UserAction};
    use zpane::ui::Viewport;

    let ws = TempWorkspace::new("cursor");
    for bad in [
        &["--cursor", "-5"][..],
        &["--cursor=notanint"][..],
        &["--cursor"][..],
    ] {
        let out = run_isolated(&ws, bad);
        assert!(!out.status.success(), "{bad:?} must fail");
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }

    struct EmptyIntrospector;
    impl Introspect for EmptyIntrospector {
        fn query(&mut self, _chain: &[&str]) -> Vec<Entry> {
            Vec::new()
        }
        fn poll_updates(&mut self) -> PollResult {
            PollResult::default()
        }
        fn flush(&mut self) -> bool {
            false
        }
    }

    let mut status = Entry::new("status", "status", EntryKind::Subcmd, "Status");
    status.children = vec![Entry::new("--short", "--short", EntryKind::Flag, "Short")];
    let mut git = Entry::new("git", "git", EntryKind::Cli, "Git");
    git.children = vec![status];

    let mut session = MillerSession::new(
        vec![git],
        EmptyIntrospector,
        "git status && cargo test",
        Viewport::new(120, 30),
        ShellDialect::Nushell,
    )
    .with_cursor_offset(10);
    session.apply(UserAction::AcceptToken);
    assert_eq!(
        session.apply(UserAction::Confirm),
        SessionOutcome::Confirmed("git status --short && cargo test".to_string())
    );
}

#[test]
fn cli_doctor_human_and_json_report_diagnostics_read_only() {
    let ws = TempWorkspace::new("doctor");
    let uncreated_cache_base = ws.path().join("missing_cache_base");
    let uncreated_config_base = ws.path().join("missing_config_base");
    let uncreated_config_file = ws.path().join("missing_cfg").join("config.toml");
    let empty_bin = ws.path().join("empty_bin");
    let work_dir = ws.path().join("work");
    fs::create_dir_all(&empty_bin).expect("create empty bin");
    fs::create_dir_all(&work_dir).expect("create work dir");

    let run_doctor = |args: &[&str], cfg_path: &Path| -> Output {
        let mut cmd = Command::new(zpane_bin());
        cmd.current_dir(&work_dir)
            .env("LOCALAPPDATA", &uncreated_cache_base)
            .env("APPDATA", &uncreated_config_base)
            .env("USERPROFILE", &uncreated_config_base)
            .env("HOME", &uncreated_config_base)
            .env("XDG_CACHE_HOME", &uncreated_cache_base)
            .env("XDG_CONFIG_HOME", &uncreated_config_base)
            .env("XDG_DATA_HOME", &uncreated_config_base)
            .env("ZPANE_CONFIG", cfg_path)
            .env("ZPANE_SHELL", "posix")
            .env("PATH", &empty_bin)
            .env_remove("NO_COLOR")
            .env_remove("TERM")
            .env_remove("HISTFILE")
            .env_remove("PSModulePath")
            .env_remove("FISH_VERSION")
            .env_remove("NU_VERSION")
            .env_remove("SHELL")
            .args(args);
        cmd.output().expect("execute zpane doctor")
    };

    // 1. Human `zpane doctor` exits 0 with clean stdout & empty stderr and creates nothing.
    let human = run_doctor(&["doctor"], &uncreated_config_file);
    assert!(human.status.success(), "doctor failed: {human:?}");
    assert!(
        human.stderr.is_empty(),
        "doctor wrote unexpected stderr: {:?}",
        String::from_utf8_lossy(&human.stderr)
    );
    let human_out = String::from_utf8(human.stdout).expect("utf8 stdout");
    assert!(human_out.contains("zpane doctor"));
    assert!(human_out.contains("Shell Dialect: POSIX (posix)"));
    assert!(human_out.contains("ZPANE_SHELL=posix"));
    assert!(human_out.contains("Providers:"));
    assert!(human_out.contains("Runtime Context:"));

    // 2. Both `zpane doctor --json` and `zpane --doctor --json` emit valid structured JSON
    //    with empty stderr and leave missing cache/config directories uncreated.
    for args in [&["doctor", "--json"][..], &["--doctor", "--json"][..]] {
        let out = run_doctor(args, &uncreated_config_file);
        assert!(out.status.success(), "{args:?} failed: {out:?}");
        assert!(
            out.stderr.is_empty(),
            "{args:?} wrote unexpected stderr: {:?}",
            String::from_utf8_lossy(&out.stderr)
        );
        let json: Value = serde_json::from_slice(&out.stdout).expect("valid doctor JSON");
        assert_eq!(json["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(json["dialect"]["dialect"], "posix");
        assert_eq!(json["dialect"]["label"], "POSIX");
        assert_eq!(json["dialect"]["reason"], "ZPANE_SHELL=posix");
        assert_eq!(json["config"]["exists"], false);
        assert_eq!(json["config"]["status"], "missing");
        assert_eq!(json["cache"]["dir_exists"], false);
        assert_eq!(json["cache"]["ps_metadata"]["status"], "missing");
        assert_eq!(json["cache"]["cli_cache"]["status"], "missing");
        assert!(json["specs"]["search_dirs"].is_array());
        assert!(json["providers"]["pwsh"]["available"].is_boolean());
        assert!(json["providers"]["powershell_exe"]["available"].is_boolean());
        assert!(json["providers"]["carapace"]["available"].is_boolean());
        assert!(json["providers"]["man"]["available"].is_boolean());
        assert!(json["runtime"]["project_task_files"].is_array());
    }

    assert!(
        !uncreated_cache_base.exists(),
        "zpane doctor must never create missing cache directories"
    );
    assert!(
        !uncreated_config_base.exists(),
        "zpane doctor must never create missing config/spec directories"
    );
    assert!(
        !uncreated_config_file.exists(),
        "zpane doctor must never create missing config.toml"
    );

    // 3. Invalid ZPANE_CONFIG diagnostics and CWD project task files surface cleanly.
    let bad_cfg = ws.path().join("warn_config.toml");
    fs::write(
        &bad_cfg,
        "history_limit = 22\nunknown_option = \"oops\"\n[ui]\ntheme = \"light\"\n",
    )
    .expect("write config with warning");
    fs::write(
        work_dir.join("package.json"),
        r#"{"scripts":{"dev":"vite","test":"cargo test"}}"#,
    )
    .expect("write package.json");

    let warn_json_out = run_doctor(&["doctor", "--json"], &bad_cfg);
    assert!(warn_json_out.status.success());
    assert!(warn_json_out.stderr.is_empty());
    let warn_json: Value = serde_json::from_slice(&warn_json_out.stdout).expect("valid json");
    assert_eq!(warn_json["config"]["exists"], true);
    assert_eq!(warn_json["config"]["status"], "warnings");
    assert_eq!(warn_json["config"]["overrides"]["history_limit"], 22);
    assert_eq!(
        warn_json["config"]["overrides"]["ui"]["configured_theme"],
        "light"
    );
    assert!(warn_json["config"]["diagnostics"]
        .as_array()
        .is_some_and(|arr| arr.iter().any(|d| d["message"]
            .as_str()
            .is_some_and(|m| m.contains("unknown_option")))));
    assert!(warn_json["runtime"]["project_task_files"]
        .as_array()
        .is_some_and(|arr| arr
            .iter()
            .any(|t| t["file"] == "package.json" && t["task_count"] == 2)));

    // 4. Invalid `doctor` CLI invocations exit non-zero with empty stdout and non-empty stderr.
    for bad in [
        &["doctor", "extra"][..],
        &["--doctor", "--warm"][..],
        &["--doctor=true"][..],
    ] {
        let out = run_isolated(&ws, bad);
        assert!(!out.status.success(), "{bad:?} must fail");
        assert!(out.stdout.is_empty());
        assert!(!out.stderr.is_empty());
    }
}
