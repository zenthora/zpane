mod discovery;
mod fuzzy;
mod model;
mod session;
mod ui;

use crossterm::{
    event,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use discovery::{
    detect_runtime_context, load_catalog, reload_catalog, sync_powershell_metadata, warm_cli_cache,
    CliIntrospector, ShellDialect,
};
use model::RuntimeContext;
use session::{MillerSession, SessionOutcome};
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

/// Windows console output: writes UTF-16 frames straight to `CONOUT$` via `WriteConsoleW`,
/// so the TUI renders even when stdout is redirected by the shell integration.
#[cfg(windows)]
mod conout {
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::io::AsRawHandle;

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

    pub struct ConOut {
        file: File,
        utf16_buf: Vec<u16>,
    }

    impl ConOut {
        pub fn open() -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
            let s = Self {
                file,
                utf16_buf: Vec::with_capacity(32768),
            };
            s.enable_vt();
            Ok(s)
        }

        fn handle(&self) -> *mut std::ffi::c_void {
            self.file.as_raw_handle()
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

        pub fn write_raw(&mut self, s: &str) {
            self.utf16_buf.clear();
            self.utf16_buf.extend(s.encode_utf16());
            let mut written: u32 = 0;
            unsafe {
                WriteConsoleW(
                    self.handle(),
                    self.utf16_buf.as_ptr(),
                    self.utf16_buf.len() as u32,
                    &mut written,
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

/// Unix (Linux / Termux) output: writes frames to the controlling terminal `/dev/tty`,
/// so the TUI renders even when stdout is captured by a shell widget (`$(zpane)`).
#[cfg(unix)]
mod conout {
    use std::fs::{File, OpenOptions};
    use std::io::{self, Write};

    pub struct ConOut {
        file: File,
    }

    impl ConOut {
        pub fn open() -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
            Ok(Self { file })
        }

        pub fn write_raw(&mut self, s: &str) {
            let _ = self.file.write_all(s.as_bytes());
            let _ = self.file.flush();
        }
    }
}

use conout::ConOut;

struct TerminalGuard {
    conout: ConOut,
    last_frame: String,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut conout = ConOut::open()?;
        enable_raw_mode()?;
        conout.write_raw("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H");
        Ok(Self {
            conout,
            last_frame: String::new(),
        })
    }

    fn write_frame(&mut self, frame: &str) {
        if frame == self.last_frame {
            return;
        }
        self.last_frame.clear();
        self.last_frame.push_str(frame);
        self.conout.write_raw(frame);
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.conout.write_raw("\x1b[?25h\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}

const HELP_TEXT: &str = "\
zpane - 3-pane Miller column command, parameter & variable explorer

Usage: zpane [OPTIONS] [INITIAL_TOKENS]...

Options:
      --initial <TEXT>       Seed the command builder with an existing command buffer
      --out-file <PATH>      Write the accepted command to <PATH> instead of stdout
      --width <COLS>         Fallback terminal width (default: 120)
      --height <ROWS>        Fallback terminal height (default: 30)
      --context <JSON>       Dynamic runtime context JSON payload (git branch, cwd, history)
      --context-file <PATH>  Read dynamic runtime context JSON payload from <PATH>
      --git-branch <NAME>    Override current Git branch name for runtime context
      --history <CMD>        Add a recent command to the top of Pane 1 (repeatable)
      --sync-ps              Sync PowerShell cmdlet & parameter metadata (requires pwsh) and exit
      --warm                 Sync PowerShell metadata (if available) + pre-warm CLI help cache and exit
  -h, --help                 Print help
  -V, --version              Print version

Environment:
  ZPANE_SHELL                Force token dialect: `pwsh` ($env:X, cmdlets), `posix` ($X), or `fish` ($status)
";

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    initial_buffer: String,
    out_file: Option<PathBuf>,
    fallback_width: usize,
    fallback_height: usize,
    runtime_context: RuntimeContext,
}

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Version,
    Help,
    SyncPs { warm: bool },
    Interactive(CliOptions),
}

fn parse_dimension(flag: &str, raw: &str, min: usize) -> anyhow::Result<usize> {
    let val = raw
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid integer for {flag}: {raw}"))?;
    if val < min {
        anyhow::bail!("{flag} must be at least {min}, got {val}");
    }
    Ok(val)
}

fn parse_context_json(raw: &str) -> anyhow::Result<RuntimeContext> {
    let cleaned = raw.trim_start_matches('\u{feff}');
    serde_json::from_str::<RuntimeContext>(cleaned)
        .map_err(|e| anyhow::anyhow!("Invalid runtime context JSON: {e}"))
}

fn merge_runtime_context(base: &mut RuntimeContext, incoming: RuntimeContext) {
    if incoming.git_branch.is_some() {
        base.git_branch = incoming.git_branch;
    }
    if !incoming.git_branches.is_empty() {
        base.git_branches = incoming.git_branches;
    }
    if !incoming.cwd_entries.is_empty() {
        base.cwd_entries = incoming.cwd_entries;
    }
    if !incoming.history.is_empty() {
        base.history.extend(incoming.history);
    }
}

fn parse_cli_args(args: &[String]) -> anyhow::Result<CliCommand> {
    if args.iter().any(|a| a == "-V" || a == "--version") {
        return Ok(CliCommand::Version);
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        return Ok(CliCommand::Help);
    }
    if args.iter().any(|a| a == "--sync-ps" || a == "--warm") {
        let warm = args.iter().any(|a| a == "--warm");
        return Ok(CliCommand::SyncPs { warm });
    }

    let mut initial_buffer = String::new();
    let mut out_file: Option<PathBuf> = None;
    let mut fallback_width: usize = 120;
    let mut fallback_height: usize = 30;
    let mut runtime_context = RuntimeContext::default();

    let mut i = 0;
    while i < args.len() {
        let flag = args[i].as_str();
        match flag {
            "--initial" | "--out-file" | "--width" | "--height" | "--context"
            | "--context-file" | "--git-branch" | "--history" => {
                let Some(val) = args.get(i + 1) else {
                    anyhow::bail!("Missing value for option {flag}");
                };
                match flag {
                    "--initial" => initial_buffer = val.clone(),
                    "--out-file" => out_file = Some(PathBuf::from(val)),
                    "--width" => {
                        fallback_width = parse_dimension(flag, val, ui::Viewport::MIN_WIDTH)?;
                    }
                    "--height" => {
                        fallback_height = parse_dimension(flag, val, ui::Viewport::MIN_HEIGHT)?;
                    }
                    "--context" => {
                        let parsed = parse_context_json(val)?;
                        merge_runtime_context(&mut runtime_context, parsed);
                    }
                    "--context-file" => {
                        let raw = fs::read_to_string(val).map_err(|e| {
                            anyhow::anyhow!("Failed to read context file '{val}': {e}")
                        })?;
                        let parsed = parse_context_json(&raw)?;
                        merge_runtime_context(&mut runtime_context, parsed);
                    }
                    "--git-branch" => {
                        let trimmed = val.trim();
                        if !trimmed.is_empty() {
                            runtime_context.git_branch = Some(trimmed.to_string());
                        }
                    }
                    "--history" => {
                        let trimmed = val.trim();
                        if !trimmed.is_empty() {
                            runtime_context.history.push(trimmed.to_string());
                        }
                    }
                    _ => unreachable!(),
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

    Ok(CliCommand::Interactive(CliOptions {
        initial_buffer,
        out_file,
        fallback_width,
        fallback_height,
        runtime_context,
    }))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let dialect = ShellDialect::detect();

    let opts = match parse_cli_args(&args)? {
        CliCommand::Version => {
            println!("zpane {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        CliCommand::Help => {
            print!("{HELP_TEXT}");
            return Ok(());
        }
        CliCommand::SyncPs { warm } => {
            match sync_powershell_metadata() {
                Ok(count) => {
                    eprintln!("Synced {count} PowerShell commands & parameter definitions.");
                }
                // PowerShell is optional on Linux / Termux: only a hard error when explicitly requested.
                Err(e) if !warm => {
                    return Err(e.context("PowerShell metadata sync failed (is `pwsh` on PATH?)"));
                }
                Err(_) => {
                    eprintln!("Skipping PowerShell metadata sync (`pwsh` not available).");
                }
            }
            if warm {
                let warmed = warm_cli_cache(dialect);
                eprintln!("Pre-warmed CLI cache for {warmed} installed priority tools.");
            }
            return Ok(());
        }
        CliCommand::Interactive(opts) => opts,
    };

    if let Some(ref f) = opts.out_file {
        let _ = fs::remove_file(f);
    }

    let runtime_ctx = detect_runtime_context(opts.runtime_context);
    let pane1_entries = load_catalog(dialect, &runtime_ctx);
    let introspector = CliIntrospector::new(dialect, &runtime_ctx);

    let (tw, th) = size().unwrap_or((opts.fallback_width as u16, opts.fallback_height as u16));
    let viewport =
        ui::Viewport::from_terminal_size(tw, th, opts.fallback_width, opts.fallback_height);

    let reload_ctx = runtime_ctx.clone();
    let mut session = MillerSession::new(
        pane1_entries,
        introspector,
        &opts.initial_buffer,
        viewport,
        dialect,
    )
    .with_runtime_context(runtime_ctx)
    .with_catalog_loader(move |d| reload_catalog(d, &reload_ctx));

    let mut guard = TerminalGuard::enter()?;
    let accepted: Option<String>;

    'main_loop: loop {
        // 1. Drain completed background introspection & first-run metadata refreshes
        session.poll_discovery();

        // 2. Recompute filtered views and draw frame when state changed
        if session.is_dirty() {
            let frame = session.render();
            guard.write_frame(&frame);
        }

        // 3. Poll for keyboard / resize events, then drain any queued burst events before rendering
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }

        loop {
            let ev = event::read()?;
            match session.handle_terminal_event(ev) {
                SessionOutcome::Continue => {}
                SessionOutcome::Confirmed(cmd) => {
                    accepted = Some(cmd);
                    break 'main_loop;
                }
                SessionOutcome::Canceled => {
                    accepted = None;
                    break 'main_loop;
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }

    drop(guard);

    session.flush_introspector();

    if let Some(cmd) = accepted {
        let trimmed = cmd.trim();
        if !trimmed.is_empty() {
            if let Some(ref f) = opts.out_file {
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
    use crate::discovery::{Introspect, PollResult};
    use crate::model::{Entry, EntryKind, Pane};
    use crate::session::UserAction;
    use crate::ui::Viewport;

    struct NullIntrospector;

    impl Introspect for NullIntrospector {
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

    #[test]
    fn test_parse_cli_args_validates_flags_and_dimensions() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        assert_eq!(
            parse_cli_args(&s(&["--version"])).unwrap(),
            CliCommand::Version
        );
        assert_eq!(parse_cli_args(&s(&["-h"])).unwrap(), CliCommand::Help);
        assert_eq!(
            parse_cli_args(&s(&["--sync-ps"])).unwrap(),
            CliCommand::SyncPs { warm: false }
        );
        assert_eq!(
            parse_cli_args(&s(&["--warm"])).unwrap(),
            CliCommand::SyncPs { warm: true }
        );

        let interactive = parse_cli_args(&s(&[
            "--width",
            "80",
            "--height",
            "24",
            "--out-file",
            "/tmp/out",
            "git",
            "commit",
        ]))
        .unwrap();
        assert_eq!(
            interactive,
            CliCommand::Interactive(CliOptions {
                initial_buffer: "git commit".to_string(),
                out_file: Some(PathBuf::from("/tmp/out")),
                fallback_width: 80,
                fallback_height: 24,
                runtime_context: RuntimeContext::default(),
            })
        );

        let err_small_w = parse_cli_args(&s(&["--width", "10"])).unwrap_err();
        assert!(err_small_w.to_string().contains("at least 40"));

        let err_bad_h = parse_cli_args(&s(&["--height", "abc"])).unwrap_err();
        assert!(err_bad_h.to_string().contains("Invalid integer"));

        let err_missing = parse_cli_args(&s(&["--initial"])).unwrap_err();
        assert!(err_missing.to_string().contains("Missing value"));
    }

    #[test]
    fn test_parse_cli_args_runtime_context_flags_and_errors() {
        use crate::model::CwdEntry;

        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();
        let ctx_path =
            env::temp_dir().join(format!("zpane_cli_ctx_test_{}.json", std::process::id()));
        fs::write(
            &ctx_path,
            "\u{feff}{\"git_branch\":\"main\",\"git_branches\":[\"main\",\"dev\"],\"cwd_entries\":[{\"name\":\"src\",\"is_dir\":true}],\"history\":[\"git status\"]}",
        )
        .unwrap();

        let parsed = parse_cli_args(&s(&[
            "--context-file",
            ctx_path.to_str().unwrap(),
            "--git-branch",
            "feat/context",
            "--history",
            "cargo test --locked",
        ]))
        .unwrap();
        let _ = fs::remove_file(&ctx_path);

        assert_eq!(
            parsed,
            CliCommand::Interactive(CliOptions {
                initial_buffer: String::new(),
                out_file: None,
                fallback_width: 120,
                fallback_height: 30,
                runtime_context: RuntimeContext {
                    git_branch: Some("feat/context".to_string()),
                    git_branches: vec!["main".to_string(), "dev".to_string()],
                    cwd_entries: vec![CwdEntry {
                        name: "src".to_string(),
                        is_dir: true,
                    }],
                    history: vec!["git status".to_string(), "cargo test --locked".to_string()],
                },
            })
        );

        let err_json = parse_cli_args(&s(&["--context", "{not-json}"])).unwrap_err();
        assert!(err_json
            .to_string()
            .contains("Invalid runtime context JSON"));

        let err_file =
            parse_cli_args(&s(&["--context-file", "/nonexistent/zpane_ctx.json"])).unwrap_err();
        assert!(err_file.to_string().contains("Failed to read context file"));
    }

    #[test]
    fn test_interactive_custom_value_journey() {
        let mut git = Entry::new("git", "git", EntryKind::Cli, "Git VCS");
        git.children = vec![Entry::new(
            "clone",
            "clone",
            EntryKind::Subcmd,
            "Clone repository",
        )];

        let mut session = MillerSession::new(
            vec![git],
            NullIntrospector,
            "",
            Viewport::new(120, 30),
            ShellDialect::Posix,
        );

        session.apply(UserAction::StepRight);
        session.apply(UserAction::StepRight);
        assert_eq!(session.view().active_pane, Pane::Third);

        let custom_url = "https://github.com/user/my-repo";
        for ch in custom_url.chars() {
            session.apply(UserAction::InsertChar(ch));
        }

        let frame = session.render();
        assert!(frame.contains("Use \"https://github.com/user/my-repo\""));
        assert_eq!(
            session.command_line(),
            "git clone https://github.com/user/my-repo"
        );
        assert_eq!(
            session.apply(UserAction::Confirm),
            SessionOutcome::Confirmed("git clone https://github.com/user/my-repo".to_string())
        );
    }

    #[test]
    fn test_tui_stutter_feedback_loop() {
        use std::time::Instant;

        // Deterministic minimal synthetic dataset matching real scale (5,000 commands, 15 params each)
        let pane1_entries: Vec<Entry> = (0..5000)
            .map(|i| {
                let name = format!("Get-SyntheticResourceItem{i:04}");
                let desc = format!(
                    "Retrieves and manages synthetic resource item number {i} from the system provider module"
                );
                let mut entry = Entry::new(&name, &name, EntryKind::Cmdlet, desc);
                entry.children = (0..15)
                    .map(|p| {
                        let p_name = format!("-Parameter{p}");
                        Entry::new(
                            &p_name,
                            format!("{p_name} (-p{p})"),
                            EntryKind::Param,
                            format!("[String] Parameter {p_name}"),
                        )
                        .with_aliases(vec![format!("p{p}")])
                    })
                    .collect();
                entry
            })
            .collect();

        // Tight interactive budgets (both debug and release fail by >6x without caching):
        let (total_budget_ms, step_budget_ms) = if cfg!(debug_assertions) {
            (35.0, 5.0)
        } else {
            (12.0, 1.5)
        };

        // Simulate typing "git" character by character + 30 rapid Down presses.
        // Scheduler noise under parallel test/hook load can add a fraction of a
        // millisecond to one run, so compare the median of three runs. A real
        // regression (>6x) is consistent and still fails the median.
        let mut total_runs = Vec::with_capacity(3);
        let mut step_runs = Vec::with_capacity(3);

        for _ in 0..3 {
            let mut session = MillerSession::new(
                pane1_entries.clone(),
                NullIntrospector,
                "",
                Viewport::new(120, 30),
                ShellDialect::PowerShell,
            );
            let _ = session.render(); // initial frame

            let mut sim_actions: Vec<UserAction> = vec![
                UserAction::MoveDown,
                UserAction::MoveDown,
                UserAction::MoveDown,
                UserAction::MoveDown,
                UserAction::MoveDown,
                UserAction::InsertChar('g'),
                UserAction::InsertChar('i'),
                UserAction::InsertChar('t'),
            ];
            sim_actions.extend(std::iter::repeat_n(UserAction::MoveDown, 30));

            let mut max_step_ms = 0.0_f64;
            let mut total_nav_ms = 0.0_f64;
            for action in sim_actions {
                let step_start = Instant::now();
                let _ = session.apply(action);
                if session.is_dirty() {
                    let _frame = session.render();
                }
                let step_ms = step_start.elapsed().as_secs_f64() * 1000.0;
                max_step_ms = max_step_ms.max(step_ms);
                total_nav_ms += step_ms;
            }

            total_runs.push(total_nav_ms);
            step_runs.push(max_step_ms);
        }

        total_runs.sort_by(f64::total_cmp);
        step_runs.sort_by(f64::total_cmp);
        let median_total_ms = total_runs[1];
        let median_step_ms = step_runs[1];

        assert!(
            median_total_ms < total_budget_ms,
            "Interactive navigation/filtering is too slow: {:.2}ms median for 38 steps (budget < {:.1}ms)",
            median_total_ms,
            total_budget_ms
        );
        assert!(
            median_step_ms < step_budget_ms,
            "Worst-case single interactive step is too slow: {:.2}ms median (budget < {:.1}ms)",
            median_step_ms,
            step_budget_ms
        );
    }
}
