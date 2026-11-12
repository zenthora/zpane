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
    ensure_ps_metadata_async, load_col1_commands, sync_powershell_metadata, CliIntrospector,
    ShellDialect,
};
pub use session::{MillerSession, SessionOutcome, UserAction};
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
        last_frame: String,
    }

    impl ConOut {
        pub fn open() -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open("CONOUT$")?;
            let s = Self {
                file,
                utf16_buf: Vec::with_capacity(32768),
                last_frame: String::new(),
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

        pub fn write_str(&mut self, s: &str) {
            if s == self.last_frame {
                return;
            }
            self.last_frame.clear();
            self.last_frame.push_str(s);
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
        last_frame: String,
    }

    impl ConOut {
        pub fn open() -> io::Result<Self> {
            let file = OpenOptions::new().read(true).write(true).open("/dev/tty")?;
            Ok(Self {
                file,
                last_frame: String::new(),
            })
        }

        pub fn write_str(&mut self, s: &str) {
            if s == self.last_frame {
                return;
            }
            self.last_frame.clear();
            self.last_frame.push_str(s);
            let _ = self.file.write_all(s.as_bytes());
            let _ = self.file.flush();
        }
    }
}

use conout::ConOut;

struct TerminalGuard {
    conout: ConOut,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut conout = ConOut::open()?;
        enable_raw_mode()?;
        conout.write_str("\x1b[?1049h\x1b[?25l\x1b[2J\x1b[H");
        Ok(Self { conout })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.conout.write_str("\x1b[?25h\x1b[?1049l");
        let _ = disable_raw_mode();
    }
}

const HELP_TEXT: &str = "\
zpane - 3-pane Miller column command, parameter & variable explorer

Usage: zpane [OPTIONS] [INITIAL_TOKENS]...

Options:
      --initial <TEXT>   Seed the command builder with an existing command buffer
      --out-file <PATH>  Write the accepted command to <PATH> instead of stdout
      --width <COLS>     Fallback terminal width (default: 120)
      --height <ROWS>    Fallback terminal height (default: 30)
      --sync-ps          Sync PowerShell cmdlet & parameter metadata (requires pwsh) and exit
      --warm             Sync PowerShell metadata (if available) + pre-warm CLI help cache and exit
  -h, --help             Print help
  -V, --version          Print version

Environment:
  ZPANE_SHELL            Force token dialect: `pwsh` ($env:X, cmdlets), `posix` ($X), or `fish` ($status)
";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("zpane {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{HELP_TEXT}");
        return Ok(());
    }

    let dialect = ShellDialect::detect();

    if args.iter().any(|a| a == "--sync-ps" || a == "--warm") {
        let warm = args.iter().any(|a| a == "--warm");
        match sync_powershell_metadata() {
            Ok(count) => eprintln!("Synced {count} PowerShell commands & parameter definitions."),
            // PowerShell is optional on Linux / Termux: only a hard error when explicitly requested.
            Err(e) if !warm => {
                return Err(e.context("PowerShell metadata sync failed (is `pwsh` on PATH?)"))
            }
            Err(_) => eprintln!("Skipping PowerShell metadata sync (`pwsh` not available)."),
        }
        if warm {
            let col1_items = load_col1_commands(dialect);
            let mut introspector = CliIntrospector::new(dialect);
            let warmed = introspector.warm_priority_tools(&col1_items);
            introspector.flush();
            eprintln!("Pre-warmed CLI cache for {warmed} installed priority tools.");
        }
        return Ok(());
    }

    let mut initial_buffer = String::new();
    let mut out_file: Option<PathBuf> = None;
    let mut fallback_w: usize = 120;
    let mut fallback_h: usize = 30;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--initial" if i + 1 < args.len() => {
                initial_buffer = args[i + 1].clone();
                i += 2;
            }
            "--out-file" if i + 1 < args.len() => {
                out_file = Some(PathBuf::from(&args[i + 1]));
                i += 2;
            }
            "--width" if i + 1 < args.len() => {
                if let Ok(w) = args[i + 1].parse::<usize>() {
                    if w >= 40 {
                        fallback_w = w;
                    }
                }
                i += 2;
            }
            "--height" if i + 1 < args.len() => {
                if let Ok(h) = args[i + 1].parse::<usize>() {
                    if h >= 12 {
                        fallback_h = h;
                    }
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

    if let Some(ref f) = out_file {
        let _ = fs::remove_file(f);
    }

    ensure_ps_metadata_async(dialect);

    let col1_items = load_col1_commands(dialect);
    let introspector = CliIntrospector::new(dialect);

    let (tw, th) = size().unwrap_or((fallback_w as u16, fallback_h as u16));
    let term_w = if tw >= 40 { tw as usize } else { fallback_w }.saturating_sub(1);
    let term_h = if th >= 12 { th as usize } else { fallback_h }.saturating_sub(1);

    let mut session = MillerSession::new(
        col1_items,
        introspector,
        &initial_buffer,
        term_w,
        term_h,
        dialect,
    )
    .with_catalog_loader(|d| {
        let _ = sync_powershell_metadata();
        load_col1_commands(d)
    });

    let mut guard = TerminalGuard::enter()?;
    let accepted: Option<String>;

    'main_loop: loop {
        // 1. Drain any completed background CLI introspection responses
        session.poll_introspector();

        // 2. Recompute filtered views and draw frame when state changed
        if session.is_dirty() {
            let frame = session.render();
            guard.conout.write_str(&frame);
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
            if let Some(ref f) = out_file {
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
    use crate::model::{ItemKind, MillerItem};

    #[test]
    fn test_interactive_custom_value_journey() {
        let mut git = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
        git.children = vec![MillerItem::new(
            "clone",
            "clone",
            ItemKind::Subcmd,
            "Clone repository",
        )];

        let introspector =
            CliIntrospector::with_initial_cache(ShellDialect::Posix, Default::default());
        let mut session =
            MillerSession::new(vec![git], introspector, "", 120, 30, ShellDialect::Posix);

        session.apply(UserAction::StepRight);
        session.apply(UserAction::StepRight);
        assert_eq!(session.active_col(), 2);

        let custom_url = "https://github.com/user/my-repo";
        for ch in custom_url.chars() {
            session.apply(UserAction::InsertChar(ch));
        }

        let first_col3 = session.selected_item(2).expect("custom value prompt");
        assert_eq!(first_col3.text, custom_url);
        assert!(first_col3.label.contains(custom_url));
        assert_eq!(first_col3.kind, ItemKind::Value);
        assert_eq!(
            session.preview_command(),
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
        let col1_items: Vec<MillerItem> = (0..5000)
            .map(|i| {
                let name = format!("Get-SyntheticResourceItem{i:04}");
                let desc = format!(
                    "Retrieves and manages synthetic resource item number {i} from the system provider module"
                );
                let mut item = MillerItem::new(&name, &name, ItemKind::Cmdlet, desc);
                item.children = (0..15)
                    .map(|p| {
                        let p_name = format!("-Parameter{p}");
                        MillerItem::new(
                            &p_name,
                            format!("{p_name} (-p{p})"),
                            ItemKind::Param,
                            format!("[String] Parameter {p_name}"),
                        )
                        .with_aliases(vec![format!("p{p}")])
                    })
                    .collect();
                item
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
        // millisecond to one step, so measure the fastest of three runs. A real
        // regression (>6x) fails every run, so the signal is preserved.
        let mut best_total_ms = f64::INFINITY;
        let mut best_step_ms = f64::INFINITY;

        for _ in 0..3 {
            let introspector =
                CliIntrospector::with_initial_cache(ShellDialect::PowerShell, Default::default());
            let mut session = MillerSession::new(
                col1_items.clone(),
                introspector,
                "",
                120,
                30,
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

            best_total_ms = best_total_ms.min(total_nav_ms);
            best_step_ms = best_step_ms.min(max_step_ms);
        }

        assert!(
            best_total_ms < total_budget_ms,
            "Interactive navigation/filtering is too slow: {:.2}ms for 38 steps (budget < {:.1}ms)",
            best_total_ms,
            total_budget_ms
        );
        assert!(
            best_step_ms < step_budget_ms,
            "Worst-case single interactive step is too slow: {:.2}ms (budget < {:.1}ms)",
            best_step_ms,
            step_budget_ms
        );
    }
}
