use crossterm::{
    event,
    terminal::{disable_raw_mode, enable_raw_mode, size},
};
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use zpane::discovery::{
    clear_disk_cache, detect_runtime_context, inspect_command_chain, load_catalog, reload_catalog,
    sync_powershell_metadata, warm_cli_cache, CliIntrospector, InspectReport, ShellDialect,
};
use zpane::model::RuntimeContext;
use zpane::sanitize::strip_escapes_and_controls_into;
use zpane::session::{MillerSession, SessionOutcome};
use zpane::ui;

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
            // SAFETY: `self.handle()` returns the live `CONOUT$` Win32 console HANDLE owned by
            // `self.file` for the duration of `&self`, and `&mut mode` points to an initialized u32.
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
            let mut offset = 0usize;
            while offset < self.utf16_buf.len() {
                let chunk_len = (self.utf16_buf.len() - offset).min(16384) as u32;
                let mut written: u32 = 0;
                // SAFETY: `self.handle()` returns the live `CONOUT$` HANDLE owned by `self.file`,
                // `utf16_buf[offset..]` holds at least `chunk_len` valid UTF-16 code units, and
                // `&mut written` points to an initialized stack u32.
                let ok = unsafe {
                    WriteConsoleW(
                        self.handle(),
                        self.utf16_buf[offset..].as_ptr(),
                        chunk_len,
                        &mut written,
                        std::ptr::null_mut(),
                    )
                };
                if ok == 0 || written == 0 {
                    break;
                }
                offset += written as usize;
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

Usage: zpane [OPTIONS] [--] [INITIAL_TOKENS]...
       zpane init <pwsh|zsh|bash|fish>
       zpane inspect [--json] <CMD>...

Options:
      --initial <TEXT>       Seed the command builder with an existing command buffer
      --out-file <PATH>      Write the accepted command to <PATH> instead of stdout
      --width <COLS>         Fallback terminal width (default: 120)
      --height <ROWS>        Fallback terminal height (default: 30)
      --context <JSON>       Dynamic runtime context JSON payload (git branch, cwd, history)
      --context-file <PATH>  Read dynamic runtime context JSON payload from <PATH>
      --git-branch <NAME>    Override current Git branch name for runtime context
      --history <CMD>        Add a recent command to the top of Pane 1 (repeatable)
      --init <SHELL>         Print shell integration snippet (`pwsh`, `zsh`, `bash`, `fish`) and exit
      --inspect <CMD>        Inspect a command or subcommand chain non-interactively and exit
      --json                 Emit machine-readable JSON for non-interactive commands
      --sync-ps              Sync PowerShell cmdlet & parameter metadata (requires pwsh) and exit
      --warm                 Sync PowerShell metadata (if available) + pre-warm CLI help cache and exit
      --clear-cache          Delete persisted CLI help & PowerShell metadata cache files and exit
  -h, --help                 Print help
  -V, --version              Print version

Environment:
  ZPANE_SHELL                Force token dialect: `pwsh` ($env:X, cmdlets), `posix` ($X), or `fish` ($status)
  ZPANE_CONFIG               Override user config path (`config.toml`)
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellInitTarget {
    Pwsh,
    Zsh,
    Bash,
    Fish,
}

impl ShellInitTarget {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pwsh" | "powershell" => Ok(ShellInitTarget::Pwsh),
            "zsh" => Ok(ShellInitTarget::Zsh),
            "bash" => Ok(ShellInitTarget::Bash),
            "fish" => Ok(ShellInitTarget::Fish),
            other => anyhow::bail!(
                "Unknown shell target '{}' (expected one of: pwsh, zsh, bash, fish)",
                sanitize_error_arg(other)
            ),
        }
    }
}

fn render_shell_init(target: ShellInitTarget) -> &'static str {
    match target {
        ShellInitTarget::Pwsh => {
            r#"Set-PSReadLineKeyHandler -Chord 'Alt+m' -ScriptBlock {
    $line = $null
    $cursor = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
    if ($null -eq $line) { $line = '' }

    $tmp = $null
    $ctxFile = $null
    try {
        $tmp = [System.IO.Path]::GetTempFileName()
        $ctxFile = [System.IO.Path]::GetTempFileName()

        $histPath = (Get-PSReadLineOption).HistorySavePath
        $recent = @()
        if ($histPath -and (Test-Path -LiteralPath $histPath)) {
            $recent = @(Get-Content -LiteralPath $histPath -Tail 15 -Encoding UTF8 -ErrorAction SilentlyContinue)
            if ($recent.Count -gt 1) {
                [array]::Reverse($recent)
            }
        }
        $ctxPayload = @{ history = @($recent) } | ConvertTo-Json -Compress
        [System.IO.File]::WriteAllText($ctxFile, $ctxPayload, [System.Text.UTF8Encoding]::new($false))

        if (-not [string]::IsNullOrEmpty($line)) {
            $legacyNativeArgs = ($PSVersionTable.PSVersion.Major -lt 7) -or
                ($PSVersionTable.PSVersion.Major -eq 7 -and $PSVersionTable.PSVersion.Minor -lt 3) -or
                ((Get-Variable -Name PSNativeCommandArgumentPassing -ValueOnly -ErrorAction SilentlyContinue) -eq 'Legacy')
            $initVal = if ($legacyNativeArgs) {
                $escaped = $line -replace '(\\*)"', '$1$1\"'
                if ($escaped -match '\s') { $escaped -replace '(\\+)$', '$1$1' } else { $escaped }
            } else {
                $line
            }
            zpane --initial "$initVal" --context-file "$ctxFile" --out-file "$tmp"
        } else {
            zpane --context-file "$ctxFile" --out-file "$tmp"
        }
        if ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $tmp)) {
            $accepted = [System.IO.File]::ReadAllText($tmp, [System.Text.UTF8Encoding]::new($false))
            if ($accepted) {
                $accepted = $accepted.Trim()
                if ($accepted) {
                    [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $accepted)
                }
            }
        }
    } finally {
        if ($tmp) { Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue }
        if ($ctxFile) { Remove-Item -LiteralPath $ctxFile -Force -ErrorAction SilentlyContinue }
    }
}
"#
        }
        ShellInitTarget::Zsh => {
            r#"_zpane_widget() {
    setopt localoptions localtraps
    local tmp
    tmp="$(umask 077 && mktemp)" || return
    trap 'rm -f -- "$tmp"' EXIT INT TERM
    if zpane --initial "$BUFFER" --out-file "$tmp" && [[ -s "$tmp" ]]; then
        BUFFER="$(<"$tmp")"
        CURSOR=${#BUFFER}
    fi
    rm -f -- "$tmp"
    zle redisplay
}
zle -N _zpane_widget
bindkey '^[m' _zpane_widget
"#
        }
        ShellInitTarget::Bash => {
            r#"_zpane_readline() {
    local tmp
    tmp="$(umask 077 && mktemp)" || return
    trap 'rm -f -- "$tmp"' RETURN INT TERM
    if zpane --initial "$READLINE_LINE" --out-file "$tmp" && [[ -s "$tmp" ]]; then
        READLINE_LINE="$(<"$tmp")"
        READLINE_POINT=${#READLINE_LINE}
    fi
    rm -f -- "$tmp"
    trap - RETURN INT TERM
}
bind -x '"\em": _zpane_readline'
"#
        }
        ShellInitTarget::Fish => {
            r#"function _zpane_fish
    set -l old_umask (umask)
    umask 077
    set -l tmp (mktemp)
    set -l mk_status $status
    umask $old_umask
    test $mk_status -eq 0; or return

    set -l buf (commandline -b | string join \n)
    if test (count $buf) -gt 0
        zpane --initial "$buf" --out-file "$tmp"
    else
        zpane --initial "" --out-file "$tmp"
    end
    if test $status -eq 0 -a -s "$tmp"
        set -l accepted (string trim -- (string collect < "$tmp"))
        if test -n "$accepted"
            commandline -r -- "$accepted"
            commandline -C (string length -- "$accepted")
        end
    end
    rm -f -- "$tmp"
    commandline -f repaint
end
bind \em _zpane_fish
"#
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    initial_buffer: String,
    out_file: Option<PathBuf>,
    fallback_width: usize,
    fallback_height: usize,
    runtime_context: RuntimeContext,
}

#[derive(Debug, PartialEq, Eq)]
enum PendingMode {
    ClearCache,
    SyncPs { warm: bool },
    Init(ShellInitTarget),
    InspectFlag(String),
    InspectSubcmd,
}

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Version,
    Help,
    Init(ShellInitTarget),
    ClearCache {
        json: bool,
    },
    SyncPs {
        warm: bool,
        json: bool,
    },
    Inspect {
        raw_chain: Vec<String>,
        json: bool,
        runtime_context: RuntimeContext,
    },
    Interactive(CliOptions),
}

fn parse_dimension(flag: &str, raw: &str, min: usize) -> anyhow::Result<usize> {
    let val = raw
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid integer for {flag}: {raw}"))?;
    if val < min {
        anyhow::bail!("{flag} must be at least {min}, got {val}");
    }
    if val > u16::MAX as usize {
        anyhow::bail!("{flag} must be at most {}, got {val}", u16::MAX);
    }
    Ok(val)
}

const MAX_CONTEXT_FILE_BYTES: u64 = 2 * 1024 * 1024;

fn parse_context_json(raw: &str) -> anyhow::Result<RuntimeContext> {
    if raw.len() as u64 > MAX_CONTEXT_FILE_BYTES {
        anyhow::bail!(
            "Runtime context JSON exceeds {} byte limit",
            MAX_CONTEXT_FILE_BYTES
        );
    }
    let cleaned = raw.trim_start_matches('\u{feff}');
    serde_json::from_str::<RuntimeContext>(cleaned)
        .map_err(|e| anyhow::anyhow!("Invalid runtime context JSON: {e}"))
}

fn read_context_file_bounded(path: &str, max_bytes: u64) -> anyhow::Result<String> {
    let safe_path = sanitize_error_arg(path);
    let meta = fs::metadata(path)
        .map_err(|e| anyhow::anyhow!("Failed to read context file '{safe_path}': {e}"))?;
    if !meta.is_file() {
        anyhow::bail!("Failed to read context file '{safe_path}': not a regular file");
    }
    if meta.len() > max_bytes {
        anyhow::bail!("Failed to read context file '{safe_path}': exceeds {max_bytes} byte limit");
    }

    let file = fs::File::open(path)
        .map_err(|e| anyhow::anyhow!("Failed to read context file '{safe_path}': {e}"))?;
    let open_meta = file
        .metadata()
        .map_err(|e| anyhow::anyhow!("Failed to read context file '{safe_path}': {e}"))?;
    if !open_meta.is_file() {
        anyhow::bail!("Failed to read context file '{safe_path}': not a regular file");
    }

    let mut buf = Vec::with_capacity((open_meta.len() as usize).min(max_bytes as usize));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(|e| anyhow::anyhow!("Failed to read context file '{safe_path}': {e}"))?;
    if buf.len() as u64 > max_bytes {
        anyhow::bail!("Failed to read context file '{safe_path}': exceeds {max_bytes} byte limit");
    }
    String::from_utf8(buf).map_err(|e| {
        anyhow::anyhow!("Failed to read context file '{safe_path}': invalid UTF-8 ({e})")
    })
}

fn ensure_regular_non_symlink_if_exists(path: &Path) -> anyhow::Result<bool> {
    let safe_path = sanitize_error_arg(&path.to_string_lossy());
    match fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                anyhow::bail!("Refusing to access output file through symlink '{safe_path}'");
            }
            if !meta.file_type().is_file() {
                anyhow::bail!("Output path '{safe_path}' is not a regular file");
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(anyhow::anyhow!(
            "Failed to inspect output file '{safe_path}': {e}"
        )),
    }
}

fn verify_and_secure_out_file(file: &fs::File, path: &Path) -> anyhow::Result<()> {
    let safe_path = sanitize_error_arg(&path.to_string_lossy());
    if !file
        .metadata()
        .map_err(|e| anyhow::anyhow!("Failed to inspect output file '{safe_path}': {e}"))?
        .is_file()
    {
        anyhow::bail!("Output path '{safe_path}' is not a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("Failed to secure output file '{safe_path}': {e}"))?;
    }
    Ok(())
}

fn prepare_out_file(path: &Path) -> anyhow::Result<()> {
    if !ensure_regular_non_symlink_if_exists(path)? {
        return Ok(());
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .map_err(|e| anyhow::anyhow!("Failed to prepare output file '{}': {e}", path.display()))?;
    verify_and_secure_out_file(&file, path)
}

fn write_out_file(path: &Path, content: &str) -> anyhow::Result<()> {
    let exists = ensure_regular_non_symlink_if_exists(path)?;
    let mut opts = fs::OpenOptions::new();
    if exists {
        opts.write(true).truncate(true);
    } else {
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
    }
    let mut file = opts
        .open(path)
        .map_err(|e| anyhow::anyhow!("Failed to write output file '{}': {e}", path.display()))?;
    verify_and_secure_out_file(&file, path)?;
    file.write_all(content.as_bytes())
        .and_then(|()| file.flush())
        .map_err(|e| anyhow::anyhow!("Failed to write output file '{}': {e}", path.display()))
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

enum ContextArg<'a> {
    Inline(&'a str),
    File(&'a str),
}

fn take_flag_value<'a>(
    flag: &str,
    inline_val: Option<&'a str>,
    args: &'a [String],
    idx: &mut usize,
) -> anyhow::Result<&'a str> {
    if let Some(v) = inline_val {
        *idx += 1;
        Ok(v)
    } else {
        let Some(v) = args.get(*idx + 1) else {
            anyhow::bail!("Missing value for option {flag}");
        };
        *idx += 2;
        Ok(v.as_str())
    }
}

fn ensure_no_inline_value(flag: &str, inline_val: Option<&str>) -> anyhow::Result<()> {
    if inline_val.is_some() {
        anyhow::bail!("Option {flag} does not take a value");
    }
    Ok(())
}

fn build_runtime_context(
    context_args: Vec<ContextArg<'_>>,
    git_branch_override: Option<String>,
    history_overrides: Vec<String>,
) -> anyhow::Result<RuntimeContext> {
    let mut runtime_context = RuntimeContext::default();
    for src in context_args {
        match src {
            ContextArg::Inline(raw) => {
                merge_runtime_context(&mut runtime_context, parse_context_json(raw)?);
            }
            ContextArg::File(path) => {
                let raw = read_context_file_bounded(path, MAX_CONTEXT_FILE_BYTES)?;
                merge_runtime_context(&mut runtime_context, parse_context_json(&raw)?);
            }
        }
    }
    if let Some(branch) = git_branch_override {
        runtime_context.git_branch = Some(branch);
    }
    runtime_context.history.extend(history_overrides);
    Ok(runtime_context)
}

fn parse_cli_args(args: &[String]) -> anyhow::Result<CliCommand> {
    if args.first().map(String::as_str) == Some("init") {
        let Some(target) = args.get(1) else {
            anyhow::bail!("Missing shell target for `init` (expected: pwsh, zsh, bash, fish)");
        };
        if target == "-h" || target == "--help" {
            return Ok(CliCommand::Help);
        }
        let parsed = ShellInitTarget::parse(target)?;
        if args.iter().skip(2).any(|a| a == "-h" || a == "--help") {
            return Ok(CliCommand::Help);
        }
        if let Some(extra) = args.get(2) {
            anyhow::bail!(
                "Unexpected argument '{}' for `init`",
                sanitize_error_arg(extra)
            );
        }
        return Ok(CliCommand::Init(parsed));
    }

    let mut initial_buffer = String::new();
    let mut out_file: Option<PathBuf> = None;
    let mut fallback_width: usize = 120;
    let mut fallback_height: usize = 30;
    let mut context_args: Vec<ContextArg<'_>> = Vec::new();
    let mut git_branch_override: Option<String> = None;
    let mut history_overrides: Vec<String> = Vec::new();
    let mut mode_cmd: Option<PendingMode> = None;
    let mut inspect_tokens: Vec<String> = Vec::new();
    let mut json = false;

    let mut saw_double_dash = false;
    let mut has_positional = false;
    let mut i = 0;

    while i < args.len() {
        let arg = args[i].as_str();
        let in_inspect_subcmd = matches!(mode_cmd, Some(PendingMode::InspectSubcmd));
        if !saw_double_dash && (!has_positional || in_inspect_subcmd) && arg == "--" {
            saw_double_dash = true;
            i += 1;
            continue;
        }

        if saw_double_dash
            || (has_positional && !in_inspect_subcmd)
            || !arg.starts_with('-')
            || arg == "-"
        {
            if !saw_double_dash && !has_positional && mode_cmd.is_none() && arg == "inspect" {
                mode_cmd = Some(PendingMode::InspectSubcmd);
                i += 1;
                continue;
            }
            if in_inspect_subcmd {
                inspect_tokens.push(arg.to_string());
                i += 1;
                continue;
            }
            if mode_cmd.is_some() {
                anyhow::bail!(
                    "Unexpected positional argument '{arg}' with standalone command flag"
                );
            }
            has_positional = true;
            if !initial_buffer.is_empty() {
                initial_buffer.push(' ');
            }
            initial_buffer.push_str(arg);
            i += 1;
            continue;
        }

        let (flag, inline_val) = if arg.starts_with("--") {
            match arg.split_once('=') {
                Some((f, v)) => (f, Some(v)),
                None => (arg, None),
            }
        } else {
            (arg, None)
        };

        match flag {
            "-V" | "--version" => {
                ensure_no_inline_value(flag, inline_val)?;
                return Ok(CliCommand::Version);
            }
            "-h" | "--help" => {
                ensure_no_inline_value(flag, inline_val)?;
                return Ok(CliCommand::Help);
            }
            "--json" => {
                ensure_no_inline_value(flag, inline_val)?;
                json = true;
                i += 1;
            }
            "--clear-cache" => {
                ensure_no_inline_value(flag, inline_val)?;
                if mode_cmd.is_some() {
                    anyhow::bail!("Conflicting command mode option {flag}");
                }
                mode_cmd = Some(PendingMode::ClearCache);
                i += 1;
            }
            "--init" => {
                if mode_cmd.is_some() {
                    anyhow::bail!("Conflicting command mode option {flag}");
                }
                let target = take_flag_value(flag, inline_val, args, &mut i)?;
                mode_cmd = Some(PendingMode::Init(ShellInitTarget::parse(target)?));
            }
            "--inspect" => {
                if mode_cmd.is_some() {
                    anyhow::bail!("Conflicting command mode option {flag}");
                }
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                if val.trim().is_empty() {
                    anyhow::bail!("Option --inspect must not be empty");
                }
                mode_cmd = Some(PendingMode::InspectFlag(val.to_string()));
            }
            "--sync-ps" | "--warm" => {
                ensure_no_inline_value(flag, inline_val)?;
                let warm = flag == "--warm";
                match mode_cmd {
                    None => mode_cmd = Some(PendingMode::SyncPs { warm }),
                    Some(PendingMode::SyncPs { warm: prev_warm }) => {
                        mode_cmd = Some(PendingMode::SyncPs {
                            warm: prev_warm || warm,
                        });
                    }
                    Some(_) => anyhow::bail!("Conflicting command mode option {flag}"),
                }
                i += 1;
            }
            "--initial" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                initial_buffer = val.to_string();
            }
            "--out-file" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                if val.trim().is_empty() {
                    anyhow::bail!("Option --out-file path must not be empty");
                }
                out_file = Some(PathBuf::from(val));
            }
            "--width" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                fallback_width = parse_dimension(flag, val, ui::Viewport::MIN_WIDTH)?;
            }
            "--height" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                fallback_height = parse_dimension(flag, val, ui::Viewport::MIN_HEIGHT)?;
            }
            "--context" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                context_args.push(ContextArg::Inline(val));
            }
            "--context-file" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                context_args.push(ContextArg::File(val));
            }
            "--git-branch" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    git_branch_override = Some(trimmed.to_string());
                }
            }
            "--history" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    history_overrides.push(trimmed.to_string());
                }
            }
            _ => {
                anyhow::bail!(
                    "Unknown option '{}' (use '--' before positional tokens starting with '-')",
                    sanitize_error_arg(arg)
                );
            }
        }
    }

    match mode_cmd {
        Some(PendingMode::ClearCache) => return Ok(CliCommand::ClearCache { json }),
        Some(PendingMode::SyncPs { warm }) => return Ok(CliCommand::SyncPs { warm, json }),
        Some(PendingMode::Init(target)) => {
            if json {
                anyhow::bail!(
                    "Option --json requires a non-interactive command (--inspect, --clear-cache, --sync-ps, or --warm)"
                );
            }
            return Ok(CliCommand::Init(target));
        }
        Some(PendingMode::InspectFlag(raw)) => {
            let runtime_context =
                build_runtime_context(context_args, git_branch_override, history_overrides)?;
            return Ok(CliCommand::Inspect {
                raw_chain: vec![raw],
                json,
                runtime_context,
            });
        }
        Some(PendingMode::InspectSubcmd) => {
            if inspect_tokens.is_empty() {
                anyhow::bail!(
                    "Missing command for `inspect` (expected: zpane inspect [--json] <CMD>...)"
                );
            }
            let runtime_context =
                build_runtime_context(context_args, git_branch_override, history_overrides)?;
            return Ok(CliCommand::Inspect {
                raw_chain: inspect_tokens,
                json,
                runtime_context,
            });
        }
        None => {
            if json {
                anyhow::bail!(
                    "Option --json requires a non-interactive command (--inspect, --clear-cache, --sync-ps, or --warm)"
                );
            }
        }
    }

    let runtime_context =
        build_runtime_context(context_args, git_branch_override, history_overrides)?;

    Ok(CliCommand::Interactive(CliOptions {
        initial_buffer,
        out_file,
        fallback_width,
        fallback_height,
        runtime_context,
    }))
}

fn sanitize_plain_text(raw: &str, scratch: &mut String) -> String {
    strip_escapes_and_controls_into(raw, scratch);
    scratch.clone()
}

fn sanitize_error_arg(raw: &str) -> String {
    sanitize_plain_text(raw, &mut String::new())
}

fn format_inspect_human(report: &InspectReport) -> String {
    use std::fmt::Write as _;

    let mut scratch = String::new();
    let dialect = sanitize_plain_text(&report.dialect, &mut scratch);

    let mut clean_chain = Vec::with_capacity(report.chain.len());
    for (idx, part) in report.chain.iter().enumerate() {
        let token = if idx == 0 {
            if let Some(cmd) = &report.command {
                strip_escapes_and_controls_into(&cmd.text, &mut scratch);
                let clean_cmd = scratch.trim();
                if !clean_cmd.is_empty() {
                    clean_cmd.to_string()
                } else {
                    sanitize_plain_text(part, &mut scratch)
                }
            } else {
                sanitize_plain_text(part, &mut scratch)
            }
        } else {
            sanitize_plain_text(part, &mut scratch)
        };
        clean_chain.push(token);
    }

    let chain_str = if clean_chain.is_empty() {
        if let Some(cmd) = &report.command {
            sanitize_plain_text(&cmd.text, &mut scratch)
        } else {
            String::new()
        }
    } else {
        clean_chain.join(" ")
    };

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Command: {chain_str} (dialect: {dialect}, entries: {})",
        report.entries.len()
    );

    for entry in &report.entries {
        strip_escapes_and_controls_into(&entry.label, &mut scratch);
        let mut label = scratch.trim().to_string();
        if label.is_empty() && !entry.text.is_empty() {
            strip_escapes_and_controls_into(&entry.text, &mut scratch);
            label = scratch.trim().to_string();
        }

        strip_escapes_and_controls_into(&entry.desc, &mut scratch);
        let desc = scratch.trim();

        if desc.is_empty() {
            let _ = writeln!(out, "  {} {}", entry.kind.badge(), label);
        } else {
            let _ = writeln!(out, "  {} {} — {}", entry.kind.badge(), label, desc);
        }
    }

    out
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
        CliCommand::Init(target) => {
            print!("{}", render_shell_init(target));
            return Ok(());
        }
        CliCommand::ClearCache { json } => {
            let removed = clear_disk_cache()?;
            if json {
                println!("{}", serde_json::json!({ "cleared": removed }));
            } else {
                eprintln!("Cleared {removed} cached metadata file(s).");
            }
            return Ok(());
        }
        CliCommand::SyncPs { warm, json } => {
            let synced_ps = match sync_powershell_metadata() {
                Ok(count) => {
                    if !json {
                        eprintln!("Synced {count} PowerShell commands & parameter definitions.");
                    }
                    count
                }
                // PowerShell is optional on Linux / Termux: only a hard error when explicitly requested.
                Err(e) if !warm => {
                    return Err(e.context("PowerShell metadata sync failed (is `pwsh` on PATH?)"));
                }
                Err(_) => {
                    if !json {
                        eprintln!("Skipping PowerShell metadata sync (`pwsh` not available).");
                    }
                    0
                }
            };
            let warmed_cli = if warm {
                let warmed = warm_cli_cache(dialect);
                if !json {
                    eprintln!("Pre-warmed CLI cache for {warmed} installed priority tools.");
                }
                warmed
            } else {
                0
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "synced_ps": synced_ps,
                        "warmed_cli": warmed_cli,
                    })
                );
            }
            return Ok(());
        }
        CliCommand::Inspect {
            raw_chain,
            json,
            runtime_context,
        } => {
            let runtime_ctx = detect_runtime_context(runtime_context);
            let report = inspect_command_chain(dialect, &runtime_ctx, &raw_chain)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", format_inspect_human(&report));
            }
            return Ok(());
        }
        CliCommand::Interactive(opts) => opts,
    };

    if let Some(ref f) = opts.out_file {
        prepare_out_file(f)?;
    }

    let runtime_ctx = detect_runtime_context(opts.runtime_context);
    let pane1_entries = load_catalog(dialect, &runtime_ctx);
    let introspector = CliIntrospector::new(dialect, &runtime_ctx);

    let (tw, th) = size().unwrap_or((
        opts.fallback_width.min(u16::MAX as usize) as u16,
        opts.fallback_height.min(u16::MAX as usize) as u16,
    ));
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
                write_out_file(f, trimmed)?;
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
    use zpane::discovery::{Introspect, PollResult};
    use zpane::model::{Entry, EntryKind, Pane};
    use zpane::session::UserAction;
    use zpane::ui::Viewport;

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
            CliCommand::SyncPs {
                warm: false,
                json: false,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["--warm"])).unwrap(),
            CliCommand::SyncPs {
                warm: true,
                json: false,
            }
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
        use zpane::model::CwdEntry;

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
    fn test_parse_cli_args_init_shells_and_clear_cache() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        assert_eq!(
            parse_cli_args(&s(&["--clear-cache"])).unwrap(),
            CliCommand::ClearCache { json: false }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "pwsh"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Pwsh)
        );
        assert_eq!(
            parse_cli_args(&s(&["--init", "powershell"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Pwsh)
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "zsh"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Zsh)
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "bash"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Bash)
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "fish"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Fish)
        );

        for target in [
            ShellInitTarget::Pwsh,
            ShellInitTarget::Zsh,
            ShellInitTarget::Bash,
            ShellInitTarget::Fish,
        ] {
            let script = render_shell_init(target);
            assert!(script.contains("zpane --initial"));
            assert!(script.contains("--out-file"));
        }

        let err_missing = parse_cli_args(&s(&["init"])).unwrap_err();
        assert!(err_missing.to_string().contains("Missing shell target"));

        let err_unknown = parse_cli_args(&s(&["init", "nushell"])).unwrap_err();
        assert!(err_unknown.to_string().contains("Unknown shell target"));

        let err_extra = parse_cli_args(&s(&["init", "pwsh", "extra"])).unwrap_err();
        assert!(err_extra.to_string().contains("Unexpected argument"));

        assert_eq!(
            parse_cli_args(&s(&["init", "--help"])).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli_args(&s(&["--init=fish"])).unwrap(),
            CliCommand::Init(ShellInitTarget::Fish)
        );

        let pwsh = render_shell_init(ShellInitTarget::Pwsh);
        assert!(pwsh.contains("@(Get-Content -LiteralPath $histPath"));
        assert!(pwsh.contains("ReadAllText($tmp"));
        assert!(pwsh.contains("if (-not [string]::IsNullOrEmpty($line))"));
        assert!(pwsh.contains("$LASTEXITCODE -eq 0"));
        assert!(!pwsh.contains("Invoke-Expression"));

        let fish = render_shell_init(ShellInitTarget::Fish);
        assert!(fish.contains("umask 077"));
        assert!(fish.contains("umask $old_umask"));
        assert!(fish.contains("if test (count $buf) -gt 0"));
        assert!(fish.contains("zpane --initial \"\" --out-file"));
        assert!(fish.contains("rm -f -- \"$tmp\""));
        assert!(!fish.contains("eval "));

        let zsh = render_shell_init(ShellInitTarget::Zsh);
        assert!(zsh.contains("tmp=\"$(umask 077 && mktemp)\" || return"));
        assert!(zsh.contains("setopt localoptions localtraps"));
        assert!(zsh.contains("trap 'rm -f -- \"$tmp\"' EXIT INT TERM"));
        assert!(!zsh.contains("eval "));

        let bash = render_shell_init(ShellInitTarget::Bash);
        assert!(bash.contains("tmp=\"$(umask 077 && mktemp)\" || return"));
        assert!(bash.contains("trap 'rm -f -- \"$tmp\"' RETURN INT TERM"));
        assert!(!bash.contains("eval "));
    }

    #[test]
    fn test_pentest_context_file_bounded_size_and_non_regular_rejection() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();

        // 1. Oversized --context-file (> MAX_CONTEXT_FILE_BYTES) must fail before OOM JSON parsing
        let big_path =
            env::temp_dir().join(format!("zpane_ctx_big_{}_{uniq}.json", std::process::id()));
        let oversized_len = (MAX_CONTEXT_FILE_BYTES as usize) + 1;
        fs::write(&big_path, vec![b' '; oversized_len]).unwrap();
        let err_big =
            parse_cli_args(&s(&["--context-file", big_path.to_str().unwrap()])).unwrap_err();
        let _ = fs::remove_file(&big_path);
        assert!(err_big.to_string().contains("byte limit"));

        // 2. Directory (non-regular file) must be rejected cleanly before File::open
        let dir_path = env::temp_dir().join(format!("zpane_ctx_dir_{}_{uniq}", std::process::id()));
        fs::create_dir_all(&dir_path).unwrap();
        let err_dir =
            parse_cli_args(&s(&["--context-file", dir_path.to_str().unwrap()])).unwrap_err();
        let _ = fs::remove_dir_all(&dir_path);
        assert!(err_dir.to_string().contains("not a regular file"));
    }

    #[test]
    fn test_pentest_out_file_prepare_and_write_preserves_regular_and_blocks_non_regular() {
        let uniq = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let out_path =
            env::temp_dir().join(format!("zpane_out_sec_{}_{uniq}.txt", std::process::id()));
        fs::write(&out_path, b"stale secret").unwrap();

        // Pre-existing regular file is truncated in-place (never unlinked/re-raced)
        prepare_out_file(&out_path).unwrap();
        assert_eq!(fs::read_to_string(&out_path).unwrap(), "");
        write_out_file(&out_path, "git status").unwrap();
        assert_eq!(fs::read_to_string(&out_path).unwrap(), "git status");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_file(&out_path);

        // Fresh output path uses create_new securely and non-regular directory path is rejected
        let fresh_path =
            env::temp_dir().join(format!("zpane_out_fresh_{}_{uniq}.txt", std::process::id()));
        prepare_out_file(&fresh_path).unwrap();
        assert!(!fresh_path.exists());
        write_out_file(&fresh_path, "cargo test").unwrap();
        assert_eq!(fs::read_to_string(&fresh_path).unwrap(), "cargo test");
        let _ = fs::remove_file(&fresh_path);

        let dir_out = env::temp_dir().join(format!("zpane_out_dir_{}_{uniq}", std::process::id()));
        fs::create_dir_all(&dir_out).unwrap();
        let err_dir = write_out_file(&dir_out, "git status").unwrap_err();
        let _ = fs::remove_dir_all(&dir_out);
        assert!(err_dir.to_string().contains("not a regular file"));

        #[cfg(unix)]
        {
            let target =
                env::temp_dir().join(format!("zpane_sym_tgt_{}_{uniq}.txt", std::process::id()));
            let link =
                env::temp_dir().join(format!("zpane_sym_lnk_{}_{uniq}.txt", std::process::id()));
            fs::write(&target, b"keep").unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let err_prep = prepare_out_file(&link).unwrap_err();
            let err_write = write_out_file(&link, "pwned").unwrap_err();
            assert!(err_prep.to_string().contains("symlink"));
            assert!(err_write.to_string().contains("symlink"));
            assert_eq!(fs::read_to_string(&target).unwrap(), "keep");
            let _ = fs::remove_file(&link);
            let _ = fs::remove_file(&target);
        }
    }

    #[test]
    fn test_parse_cli_args_flag_values_double_dash_and_error_cases() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        // Values matching mode flags must not hijack the CLI command
        assert_eq!(
            parse_cli_args(&s(&["--initial", "--help", "--history", "--warm"])).unwrap(),
            CliCommand::Interactive(CliOptions {
                initial_buffer: "--help".to_string(),
                out_file: None,
                fallback_width: 120,
                fallback_height: 30,
                runtime_context: RuntimeContext {
                    history: vec!["--warm".to_string()],
                    ..RuntimeContext::default()
                },
            })
        );

        // `--flag=value` syntax and `--` end-of-options separator
        assert_eq!(
            parse_cli_args(&s(&[
                "--width=80",
                "--height=24",
                "--out-file=/tmp/zpane_out",
                "--",
                "-Id",
                "--help",
            ]))
            .unwrap(),
            CliCommand::Interactive(CliOptions {
                initial_buffer: "-Id --help".to_string(),
                out_file: Some(PathBuf::from("/tmp/zpane_out")),
                fallback_width: 80,
                fallback_height: 24,
                runtime_context: RuntimeContext::default(),
            })
        );

        // Flags following a positional command token remain part of INITIAL_TOKENS
        assert_eq!(
            parse_cli_args(&s(&["git", "commit", "--amend", "--help"])).unwrap(),
            CliCommand::Interactive(CliOptions {
                initial_buffer: "git commit --amend --help".to_string(),
                out_file: None,
                fallback_width: 120,
                fallback_height: 30,
                runtime_context: RuntimeContext::default(),
            })
        );

        let err_unknown = parse_cli_args(&s(&["--widht", "80"])).unwrap_err();
        assert!(err_unknown.to_string().contains("Unknown option"));

        let err_max = parse_cli_args(&s(&["--width", "70000"])).unwrap_err();
        assert!(err_max.to_string().contains("at most 65535"));

        let err_empty_out = parse_cli_args(&s(&["--out-file="])).unwrap_err();
        assert!(err_empty_out.to_string().contains("must not be empty"));

        let err_inline_bool = parse_cli_args(&s(&["--help=1"])).unwrap_err();
        assert!(err_inline_bool
            .to_string()
            .contains("does not take a value"));

        let err_conflict = parse_cli_args(&s(&["--clear-cache", "--warm"])).unwrap_err();
        assert!(err_conflict
            .to_string()
            .contains("Conflicting command mode"));
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

    #[test]
    fn test_parse_cli_args_inspect_and_json_modes() {
        let argv = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        match parse_cli_args(&argv(&["inspect", "git", "commit"])).expect("subcommand inspect") {
            CliCommand::Inspect {
                raw_chain, json, ..
            } => {
                assert_eq!(raw_chain, vec!["git", "commit"]);
                assert!(!json);
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }

        match parse_cli_args(&argv(&["inspect", "--json", "git", "commit"]))
            .expect("inspect --json")
        {
            CliCommand::Inspect {
                raw_chain, json, ..
            } => {
                assert_eq!(raw_chain, vec!["git", "commit"]);
                assert!(json);
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }

        match parse_cli_args(&argv(&["inspect", "git", "commit", "--json"]))
            .expect("inspect trailing --json")
        {
            CliCommand::Inspect {
                raw_chain, json, ..
            } => {
                assert_eq!(raw_chain, vec!["git", "commit"]);
                assert!(json);
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }

        match parse_cli_args(&argv(&["--json", "inspect", "git", "commit"]))
            .expect("--json before inspect")
        {
            CliCommand::Inspect {
                raw_chain, json, ..
            } => {
                assert_eq!(raw_chain, vec!["git", "commit"]);
                assert!(json);
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }

        let hist_path = std::env::temp_dir().join(format!(
            "zpane-test-inspect-history-{}.txt",
            std::process::id()
        ));
        std::fs::write(&hist_path, "git status\ncargo test\n").expect("write temp history");
        let hist_str = hist_path.to_string_lossy().to_string();

        match parse_cli_args(&argv(&[
            "--inspect",
            "git commit",
            "--context",
            r#"{"git_branch":"old","git_branches":["dev"]}"#,
            "--git-branch",
            "main",
            "--history",
            &hist_str,
        ]))
        .expect("--inspect with context options")
        {
            CliCommand::Inspect {
                raw_chain,
                json,
                runtime_context,
            } => {
                assert_eq!(raw_chain, vec!["git commit"]);
                assert!(!json);
                assert_eq!(runtime_context.git_branch.as_deref(), Some("main"));
                assert_eq!(runtime_context.git_branches, vec!["dev"]);
                assert!(!runtime_context.history.is_empty());
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }
        let _ = std::fs::remove_file(&hist_path);

        match parse_cli_args(&argv(&["--inspect=git commit", "--json"]))
            .expect("--inspect= with --json")
        {
            CliCommand::Inspect {
                raw_chain, json, ..
            } => {
                assert_eq!(raw_chain, vec!["git commit"]);
                assert!(json);
            }
            other => panic!("expected CliCommand::Inspect, got {other:?}"),
        }

        assert!(matches!(
            parse_cli_args(&argv(&["--clear-cache", "--json"])).expect("--clear-cache --json"),
            CliCommand::ClearCache { json: true }
        ));
        assert!(matches!(
            parse_cli_args(&argv(&["--sync-ps", "--json"])).expect("--sync-ps --json"),
            CliCommand::SyncPs {
                warm: false,
                json: true
            }
        ));
        assert!(matches!(
            parse_cli_args(&argv(&["--warm", "--json"])).expect("--warm --json"),
            CliCommand::SyncPs {
                warm: true,
                json: true
            }
        ));

        // `-- inspect` treats `inspect` as positional interactive query rather than subcommand.
        assert!(matches!(
            parse_cli_args(&argv(&["--", "inspect"])).expect("-- inspect"),
            CliCommand::Interactive(ref opts) if opts.initial_buffer == "inspect"
        ));
        // `git inspect` keeps `inspect` as second token of interactive query.
        assert!(matches!(
            parse_cli_args(&argv(&["git", "inspect"])).expect("git inspect"),
            CliCommand::Interactive(ref opts) if opts.initial_buffer == "git inspect"
        ));
        assert!(matches!(
            parse_cli_args(&argv(&["inspect", "--help"])).expect("inspect --help"),
            CliCommand::Help
        ));
    }

    #[test]
    fn test_parse_cli_args_inspect_and_json_errors() {
        let argv = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();

        let bare_json = parse_cli_args(&argv(&["--json"])).expect_err("--json alone must fail");
        assert!(
            bare_json
                .to_string()
                .contains("Option --json requires a non-interactive command"),
            "unexpected error: {bare_json}"
        );

        for invalid in [
            vec!["--json", "git", "status"],
            vec!["--init", "pwsh", "--json"],
            vec!["init", "pwsh", "--json"],
            vec!["--json=foo", "--clear-cache"],
            vec!["--inspect"],
            vec!["--inspect="],
            vec!["--inspect", ""],
            vec!["--inspect", "   "],
            vec!["inspect"],
            vec!["inspect", "--json"],
            vec!["inspect", "git", "--clear-cache"],
            vec!["--inspect", "git", "--warm"],
            vec!["--clear-cache", "--inspect", "git"],
            vec!["--inspect", "git", "--inspect", "cargo"],
            vec!["--clear-cache", "inspect"],
        ] {
            let err = parse_cli_args(&argv(&invalid))
                .expect_err(&format!("expected error for args {invalid:?}"));
            assert!(
                !err.to_string().trim().is_empty(),
                "error message should not be empty for {invalid:?}"
            );
        }

        for hostile_cli in [
            vec!["--bogus\x1b[31mflag\u{202e}"],
            vec!["init", "bad\x1b]0;pwn\x07target"],
            vec!["init", "pwsh", "extra\x1b[2J\u{202e}arg"],
        ] {
            let err = parse_cli_args(&argv(&hostile_cli))
                .expect_err(&format!("expected error for hostile args {hostile_cli:?}"));
            let msg = err.to_string();
            assert!(
                !msg.contains('\x1b') && !msg.contains('\x07') && !msg.contains('\u{202e}'),
                "CLI error leaked control/BiDi chars for {hostile_cli:?}: {msg:?}"
            );
        }
    }

    #[test]
    fn test_format_inspect_human_and_pentest_sanitization() {
        use zpane::discovery::{inspect_command_chain_with, InspectReport, ShellDialect};
        use zpane::model::{Entry, EntryKind};

        // 1. Standard human output formatting with desc and empty desc.
        let mut git = Entry::new("git", "git", EntryKind::Cmdlet, "Distributed VCS");
        git.children = vec![
            Entry::new("commit", "commit", EntryKind::Subcmd, "Record changes"),
            Entry::new("--version", "--version", EntryKind::Flag, ""),
        ];
        let catalog = vec![git];

        let report = inspect_command_chain_with(
            ShellDialect::Fish,
            &catalog,
            &["git".to_string()],
            None,
            false,
            &|_, _| None,
        )
        .expect("inspect should succeed");
        let rendered = format_inspect_human(&report);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines.len(), 3, "unexpected human output:\n{rendered}");
        assert_eq!(lines[0], "Command: git (dialect: fish, entries: 2)");
        assert_eq!(
            lines[1],
            format!(
                "  {} commit \u{2014} Record changes",
                EntryKind::Subcmd.badge()
            )
        );
        assert_eq!(lines[2], format!("  {} --version", EntryKind::Flag.badge()));

        let json_str = serde_json::to_string_pretty(&report).expect("serialize InspectReport");
        let parsed_json: serde_json::Value =
            serde_json::from_str(&json_str).expect("parse InspectReport JSON");
        assert_eq!(parsed_json["dialect"], "fish");
        assert_eq!(parsed_json["chain"], serde_json::json!(["git"]));
        assert_eq!(parsed_json["command"]["text"], "git");
        assert_eq!(
            parsed_json["command"]["children"],
            serde_json::json!([]),
            "command.children should be cleared so entries are not duplicated"
        );
        assert_eq!(parsed_json["entries"].as_array().map(Vec::len), Some(2));

        // 2. Hostile escape sequences, DCS/OSC/CSI/C1, CRLF, and BiDi controls are stripped.
        let hostile_cmd = Entry::new("git\x1b]0;pwn\x07", "git", EntryKind::Cmdlet, "desc");
        let hostile_entry = Entry::new(
            "commit",
            "commit\x1b[2J\u{061c}",
            EntryKind::Subcmd,
            "Hello\x1bP1;2$q\x1b\\\u{009b}31m\nWorld\u{202a}",
        );
        let hostile = InspectReport {
            dialect: "pwsh\x1b[31m\nspoof".to_string(),
            chain: vec!["git\u{202e}".to_string(), "commit\r\nFAKE".to_string()],
            command: Some(hostile_cmd),
            entries: vec![hostile_entry],
        };

        let sanitized = format_inspect_human(&hostile);
        assert_eq!(
            sanitized.lines().count(),
            2,
            "newlines must not inject extra output lines:\n{sanitized}"
        );
        for forbidden in [
            '\x1b', '\x07', '\u{009b}', '\u{202e}', '\u{202a}', '\u{061c}', '\r',
        ] {
            assert!(
                !sanitized.contains(forbidden),
                "forbidden control/BiDi char {forbidden:?} survived in {sanitized:?}"
            );
        }
        assert!(
            sanitized.contains("Command: git commit  FAKE (dialect: pwsh spoof, entries: 1)"),
            "unexpected sanitized header:\n{sanitized:?}"
        );
        assert!(
            sanitized.contains("commit — Hello World"),
            "unexpected sanitized entry:\n{sanitized:?}"
        );
    }

    #[test]
    fn test_help_text_and_readme_document_inspect_and_json() {
        let readme = include_str!("../README.md");
        for needle in [
            "zpane inspect [--json] <CMD>...",
            "--inspect <CMD>",
            "--json",
        ] {
            assert!(HELP_TEXT.contains(needle), "HELP_TEXT missing `{needle}`");
            assert!(readme.contains(needle), "README.md missing `{needle}`");
        }
    }
}
