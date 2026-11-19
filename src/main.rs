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
    clear_disk_cache, detect_runtime_context, inspect_command_chain, load_catalog,
    load_user_config_with_diagnostics, reload_catalog, sync_powershell_metadata, warm_cli_cache,
    CliIntrospector, InspectReport, ShellDialect, UiStyle, UserConfig,
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
       zpane init <pwsh|zsh|bash|fish|nu> [--bind <CHORD> | --no-bind]
       zpane inspect [--json] <CMD>...

Options:
      --initial <TEXT>       Seed the command builder with an existing command buffer
      --cursor <OFFSET>      Cursor character offset in --initial (preserves trailing suffix verbatim)
      --out-file <PATH>      Write the accepted command to <PATH> instead of stdout
      --width <COLS>         Fallback terminal width (default: 120)
      --height <ROWS>        Fallback terminal height (default: 30)
      --context <JSON>       Dynamic runtime context JSON payload (git branch, cwd, history)
      --context-file <PATH>  Read dynamic runtime context JSON payload from <PATH>
      --git-branch <NAME>    Override current Git branch name for runtime context
      --history <CMD>        Add a recent command to the top of Pane 1 (repeatable)
      --init <SHELL>         Print shell integration snippet (`pwsh`, `zsh`, `bash`, `fish`, `nu`) and exit
      --bind <CHORD>         Keybinding chord for `init`/`--init` (e.g. `Alt+m`, `Ctrl+g`, `Ctrl+Space`, `F2`)
      --no-bind              Define the shell init widget/helper without registering a keybinding
      --inspect <CMD>        Inspect a command or subcommand chain non-interactively and exit
      --json                 Emit machine-readable JSON for non-interactive commands
      --sync-ps              Sync PowerShell cmdlet & parameter metadata (pwsh or powershell.exe) and exit
      --warm                 Sync PowerShell metadata (if available) + pre-warm CLI help cache and exit
      --clear-cache          Delete persisted CLI help & PowerShell metadata cache files and exit
  -h, --help                 Print help
  -V, --version              Print version

Environment:
  ZPANE_SHELL                Force token dialect: `pwsh` ($env:X), `posix` ($X), `fish` ($status), or `nu` ($env.X)
  ZPANE_CONFIG               Override user config path (`config.toml`)
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellInitTarget {
    Pwsh,
    Zsh,
    Bash,
    Fish,
    Nushell,
}

impl ShellInitTarget {
    fn parse(raw: &str) -> anyhow::Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "pwsh" | "powershell" => Ok(ShellInitTarget::Pwsh),
            "zsh" => Ok(ShellInitTarget::Zsh),
            "bash" => Ok(ShellInitTarget::Bash),
            "fish" => Ok(ShellInitTarget::Fish),
            "nu" | "nushell" => Ok(ShellInitTarget::Nushell),
            other => anyhow::bail!(
                "Unknown shell target '{}' (expected one of: pwsh, zsh, bash, fish, nu)",
                sanitize_error_arg(other)
            ),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitKey {
    Letter(char),
    Digit(char),
    Space,
    Tab,
    F(u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InitKeyChord {
    ctrl: bool,
    alt: bool,
    shift: bool,
    key: InitKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InitBindMode {
    Bind(InitKeyChord),
    Disabled,
}

impl Default for InitBindMode {
    fn default() -> Self {
        Self::Bind(InitKeyChord::default_alt_m())
    }
}

impl InitKeyChord {
    const fn default_alt_m() -> Self {
        Self {
            ctrl: false,
            alt: true,
            shift: false,
            key: InitKey::Letter('m'),
        }
    }

    fn parse(raw: &str) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.is_empty()
            || !trimmed.is_ascii()
            || trimmed.bytes().any(|b| b < 0x20 || b == 0x7f)
        {
            anyhow::bail!(
                "Invalid keybinding chord '{}' (expected e.g. Alt+m, Ctrl+g, Ctrl+Space, Shift+Tab, F2)",
                sanitize_error_arg(raw)
            );
        }

        let parts: Vec<&str> = trimmed.split('+').map(str::trim).collect();
        if parts.iter().any(|p| p.is_empty()) {
            anyhow::bail!(
                "Invalid keybinding chord '{}' (empty modifier or key segment)",
                sanitize_error_arg(raw)
            );
        }

        let mut ctrl = false;
        let mut alt = false;
        let mut shift = false;
        for modifier in &parts[..parts.len() - 1] {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => {
                    if ctrl {
                        anyhow::bail!(
                            "Duplicate modifier in keybinding chord '{}'",
                            sanitize_error_arg(raw)
                        );
                    }
                    ctrl = true;
                }
                "alt" | "meta" => {
                    if alt {
                        anyhow::bail!(
                            "Duplicate modifier in keybinding chord '{}'",
                            sanitize_error_arg(raw)
                        );
                    }
                    alt = true;
                }
                "shift" => {
                    if shift {
                        anyhow::bail!(
                            "Duplicate modifier in keybinding chord '{}'",
                            sanitize_error_arg(raw)
                        );
                    }
                    shift = true;
                }
                _ => {
                    anyhow::bail!(
                        "Unknown modifier '{}' in keybinding chord '{}'",
                        sanitize_error_arg(modifier),
                        sanitize_error_arg(raw)
                    );
                }
            }
        }

        let key_part = parts[parts.len() - 1];
        let key_lower = key_part.to_ascii_lowercase();
        let key = match key_lower.as_str() {
            "space" | "spacebar" => InitKey::Space,
            "tab" => InitKey::Tab,
            "backtab" => {
                shift = true;
                InitKey::Tab
            }
            _ if key_lower.starts_with('f') && key_lower.len() >= 2 => {
                let Ok(num) = key_lower[1..].parse::<u8>() else {
                    anyhow::bail!(
                        "Invalid key '{}' in keybinding chord '{}'",
                        sanitize_error_arg(key_part),
                        sanitize_error_arg(raw)
                    );
                };
                if !(1..=12).contains(&num) {
                    anyhow::bail!(
                        "Function key in '{}' must be between F1 and F12",
                        sanitize_error_arg(raw)
                    );
                }
                InitKey::F(num)
            }
            _ => {
                let mut chars = key_lower.chars();
                let Some(ch) = chars.next() else {
                    anyhow::bail!(
                        "Missing key in keybinding chord '{}'",
                        sanitize_error_arg(raw)
                    );
                };
                if chars.next().is_some() {
                    anyhow::bail!(
                        "Unsupported key '{}' in keybinding chord '{}'",
                        sanitize_error_arg(key_part),
                        sanitize_error_arg(raw)
                    );
                }
                if ch.is_ascii_lowercase() {
                    InitKey::Letter(ch)
                } else if ch.is_ascii_digit() {
                    InitKey::Digit(ch)
                } else {
                    anyhow::bail!(
                        "Unsupported key '{}' in keybinding chord '{}'",
                        sanitize_error_arg(key_part),
                        sanitize_error_arg(raw)
                    );
                }
            }
        };

        let valid_combo = match key {
            InitKey::Letter(_) => (ctrl || alt) && !(ctrl && shift),
            InitKey::Digit(_) => alt && !ctrl && !shift,
            InitKey::Space => (ctrl ^ alt) && !shift,
            InitKey::Tab => !ctrl && !(alt && shift),
            InitKey::F(_) => !ctrl && !alt && !shift,
        };
        if !valid_combo {
            anyhow::bail!(
                "Unsupported modifier/key combination '{}' (use e.g. Alt+m, Ctrl+g, Ctrl+Alt+m, Alt+Shift+m, Ctrl+Space, Tab, Shift+Tab, F1-F12)",
                sanitize_error_arg(raw)
            );
        }

        Ok(Self {
            ctrl,
            alt,
            shift,
            key,
        })
    }

    fn to_pwsh_chord(self) -> String {
        let mut out = String::new();
        if self.ctrl {
            out.push_str("Ctrl+");
        }
        if self.alt {
            out.push_str("Alt+");
        }
        if self.shift {
            out.push_str("Shift+");
        }
        match self.key {
            InitKey::Letter(c) | InitKey::Digit(c) => out.push(c),
            InitKey::Space => out.push_str("Spacebar"),
            InitKey::Tab => out.push_str("Tab"),
            InitKey::F(n) => out.push_str(&format!("F{n}")),
        }
        out
    }

    fn to_zsh_bindkey(self) -> String {
        match self.key {
            InitKey::Letter(c) => match (self.ctrl, self.alt, self.shift) {
                (true, true, _) => format!("^[^{}", c.to_ascii_uppercase()),
                (true, false, _) => format!("^{}", c.to_ascii_uppercase()),
                (false, true, true) => format!("^[{}", c.to_ascii_uppercase()),
                (false, true, false) => format!("^[{c}"),
                _ => unreachable!(),
            },
            InitKey::Digit(d) => format!("^[{d}"),
            InitKey::Space => {
                if self.ctrl {
                    "^@".to_string()
                } else {
                    "^[ ".to_string()
                }
            }
            InitKey::Tab => {
                if self.shift {
                    "^[[Z".to_string()
                } else if self.alt {
                    "^[^I".to_string()
                } else {
                    "^I".to_string()
                }
            }
            InitKey::F(n) => match n {
                1 => "^[OP",
                2 => "^[OQ",
                3 => "^[OR",
                4 => "^[OS",
                5 => "^[[15~",
                6 => "^[[17~",
                7 => "^[[18~",
                8 => "^[[19~",
                9 => "^[[20~",
                10 => "^[[21~",
                11 => "^[[23~",
                12 => "^[[24~",
                _ => unreachable!(),
            }
            .to_string(),
        }
    }

    fn to_bash_readline(self) -> String {
        match self.key {
            InitKey::Letter(c) => match (self.ctrl, self.alt, self.shift) {
                (true, true, _) => format!("\\e\\C-{c}"),
                (true, false, _) => format!("\\C-{c}"),
                (false, true, true) => format!("\\e{}", c.to_ascii_uppercase()),
                (false, true, false) => format!("\\e{c}"),
                _ => unreachable!(),
            },
            InitKey::Digit(d) => format!("\\e{d}"),
            InitKey::Space => {
                if self.ctrl {
                    "\\C-@".to_string()
                } else {
                    "\\e ".to_string()
                }
            }
            InitKey::Tab => {
                if self.shift {
                    "\\e[Z".to_string()
                } else if self.alt {
                    "\\e\\t".to_string()
                } else {
                    "\\t".to_string()
                }
            }
            InitKey::F(n) => match n {
                1 => "\\eOP",
                2 => "\\eOQ",
                3 => "\\eOR",
                4 => "\\eOS",
                5 => "\\e[15~",
                6 => "\\e[17~",
                7 => "\\e[18~",
                8 => "\\e[19~",
                9 => "\\e[20~",
                10 => "\\e[21~",
                11 => "\\e[23~",
                12 => "\\e[24~",
                _ => unreachable!(),
            }
            .to_string(),
        }
    }

    fn to_fish_bind(self) -> String {
        match self.key {
            InitKey::Letter(c) => match (self.ctrl, self.alt, self.shift) {
                (true, true, _) => format!("\\e\\c{c}"),
                (true, false, _) => format!("\\c{c}"),
                (false, true, true) => format!("\\e{}", c.to_ascii_uppercase()),
                (false, true, false) => format!("\\e{c}"),
                _ => unreachable!(),
            },
            InitKey::Digit(d) => format!("\\e{d}"),
            InitKey::Space => {
                if self.ctrl {
                    "\\c@".to_string()
                } else {
                    "\\e' '".to_string()
                }
            }
            InitKey::Tab => {
                if self.shift {
                    "-k btab".to_string()
                } else if self.alt {
                    "\\e\\t".to_string()
                } else {
                    "\\t".to_string()
                }
            }
            InitKey::F(n) => format!("-k f{n}"),
        }
    }

    fn to_nu_binding(self) -> (&'static str, String) {
        let modifier = match (self.ctrl, self.alt, self.shift) {
            (true, true, false) => "control_alt",
            (true, false, false) => "control",
            (false, true, true) => "alt_shift",
            (false, true, false) => "alt",
            (false, false, true) => "shift",
            (false, false, false) => "none",
            _ => unreachable!(),
        };
        let keycode = match self.key {
            InitKey::Letter(c) | InitKey::Digit(c) => format!("char_{c}"),
            InitKey::Space => "space".to_string(),
            InitKey::Tab => {
                if self.shift {
                    "backtab".to_string()
                } else {
                    "tab".to_string()
                }
            }
            InitKey::F(n) => format!("f{n}"),
        };
        (modifier, keycode)
    }
}

fn render_shell_init(target: ShellInitTarget, bind_mode: InitBindMode) -> String {
    match target {
        ShellInitTarget::Pwsh => {
            let mut out = String::from(
                r#"function Invoke-ZpaneWidget {
    $line = $null
    $cursor = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)
    if ($null -eq $line) { $line = '' }
    if ($null -eq $cursor -or $cursor -lt 0) { $cursor = $line.Length }

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
        $sVars = @(Get-Variable -Scope Global -ErrorAction SilentlyContinue | Where-Object {
            $_.Name -match '^[A-Za-z_][A-Za-z0-9_]*$' -and
            $_.Name -notmatch '^(PWD|HOME|PROFILE|PID|PSVersionTable|PSHOME|PSUICulture|PSCulture|ExecutionContext|Host|MyInvocation|PSBoundParameters|PSCmdlet|PSCommandPath|PSScriptRoot|Sender|Event|EventArgs|EventSubscriber|SourceArgs|SourceEventArgs|Matches|LastExitCode|Error|StackTrace|Input|Args|OFS|ConfirmPreference|DebugPreference|ErrorActionPreference|InformationPreference|ProgressPreference|VerbosePreference|WarningPreference|WhatIfPreference|FormatEnumerationLimit|MaximumHistoryCount|OutputEncoding|NestedPromptLevel|ShellId|true|false|null|_)$' -and
            ($_.Value -is [string] -or $_.Value -is [ValueType])
        } | Select-Object -First 64 | ForEach-Object {
            $vStr = ("$($_.Value)" -replace '[\r\n]+', ' ')
            if ($vStr.Length -gt 120) { $vStr = $vStr.Substring(0, 120) }
            @{ name = $_.Name; value = $vStr }
        })
        $sFuncs = @(Get-ChildItem Function: -ErrorAction SilentlyContinue | Where-Object {
            -not $_.ModuleName -and $_.Name -notmatch '^([A-Za-z]:|__|TabExpansion|Prompt|Clear-Host|more|oss|help|Pause|cd\.\.|cd\\)$'
        } | Select-Object -First 128 | ForEach-Object {
            @{ name = $_.Name; desc = 'Session function' }
        })
        $ctxPayload = @{
            history       = @($recent)
            session_vars  = @($sVars)
            session_funcs = @($sFuncs)
        } | ConvertTo-Json -Depth 4 -Compress
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
            zpane --initial "$initVal" --cursor "$cursor" --context-file "$ctxFile" --out-file "$tmp"
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
"#,
            );
            if let InitBindMode::Bind(chord) = bind_mode {
                let chord_str = chord.to_pwsh_chord();
                out.push_str(&format!(
                    "Set-PSReadLineKeyHandler -Chord '{chord_str}' -ScriptBlock {{ Invoke-ZpaneWidget }}\n"
                ));
            }
            out
        }
        ShellInitTarget::Zsh => {
            let mut out = String::from(
                r#"_zpane_widget() {
    setopt localoptions localtraps
    local tmp line
    local -a hist_args
    tmp="$(umask 077 && mktemp)" || return
    trap 'rm -f -- "$tmp"' EXIT INT TERM
    hist_args=()
    while IFS= read -r line; do
        line="${line#"${line%%[![:space:]]*}"}"
        [[ -n "$line" ]] && hist_args=(--history "$line" "${hist_args[@]}")
    done < <(fc -ln -15 2>/dev/null || true)
    if zpane --initial "$BUFFER" --cursor "$CURSOR" "${hist_args[@]}" --out-file "$tmp" && [[ -s "$tmp" ]]; then
        BUFFER="$(<"$tmp")"
        CURSOR=${#BUFFER}
    fi
    rm -f -- "$tmp"
    zle redisplay
}
zle -N _zpane_widget
"#,
            );
            if let InitBindMode::Bind(chord) = bind_mode {
                let seq = chord.to_zsh_bindkey();
                out.push_str(&format!("bindkey '{seq}' _zpane_widget\n"));
            }
            out
        }
        ShellInitTarget::Bash => {
            let mut out = String::from(
                r#"_zpane_readline() {
    local tmp line
    local -a hist_args=()
    tmp="$(umask 077 && mktemp)" || return
    trap 'rm -f -- "$tmp"' RETURN INT TERM
    while IFS= read -r line; do
        if [[ "$line" =~ ^[[:space:]]*[0-9]+\*?[[:space:]]+(.+)$ ]]; then
            hist_args=(--history "${BASH_REMATCH[1]}" ${hist_args[@]+"${hist_args[@]}"})
        fi
    done < <(HISTTIMEFORMAT='' history 15 2>/dev/null || true)
    if zpane --initial "$READLINE_LINE" --cursor "$READLINE_POINT" ${hist_args[@]+"${hist_args[@]}"} --out-file "$tmp" && [[ -s "$tmp" ]]; then
        READLINE_LINE="$(<"$tmp")"
        READLINE_POINT=${#READLINE_LINE}
    fi
    rm -f -- "$tmp"
    trap - RETURN INT TERM
}
"#,
            );
            if let InitBindMode::Bind(chord) = bind_mode {
                let seq = chord.to_bash_readline();
                out.push_str(&format!("bind -x '\"{seq}\": _zpane_readline'\n"));
            }
            out
        }
        ShellInitTarget::Fish => {
            let mut out = String::from(
                r#"function _zpane_fish
    set -l old_umask (umask)
    umask 077
    set -l tmp (mktemp)
    set -l mk_status $status
    umask $old_umask
    test $mk_status -eq 0; or return

    set -l buf (commandline -b | string join \n)
    set -l cursor (commandline -C)
    set -l hist_args
    for h in (history --max 15 2>/dev/null)
        set -l trimmed (string trim -- "$h")
        if test -n "$trimmed"
            set -a hist_args --history "$trimmed"
        end
    end
    if test (count $buf) -gt 0
        zpane --initial "$buf" --cursor "$cursor" $hist_args --out-file "$tmp"
    else
        zpane --initial "" --cursor 0 $hist_args --out-file "$tmp"
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
"#,
            );
            if let InitBindMode::Bind(chord) = bind_mode {
                let seq = chord.to_fish_bind();
                out.push_str(&format!("bind {seq} _zpane_fish\n"));
            }
            out
        }
        ShellInitTarget::Nushell => {
            let mut out = String::from(
                r#"def _zpane_widget [] {
    let line = (commandline)
    let cursor = (commandline get-cursor)
    let tmp = (mktemp -t zpane.XXXXXX)
    let hist_args = (
        try {
            history
            | last 15
            | get command
            | reverse
            | where {|c| ($c | str trim | is-not-empty) }
            | each {|c| ["--history", ($c | str trim)] }
            | flatten
        } catch { [] }
    )
    try {
        ^zpane --initial $line --cursor ($cursor | into string) ...$hist_args --out-file $tmp
        if $env.LAST_EXIT_CODE == 0 and ($tmp | path exists) {
            let accepted = (open --raw $tmp | str trim)
            if ($accepted | is-not-empty) {
                commandline edit --replace $accepted
                commandline set-cursor ($accepted | str length)
            }
        }
    } catch { }
    if ($tmp | path exists) {
        rm -f $tmp
    }
}
"#,
            );
            if let InitBindMode::Bind(chord) = bind_mode {
                let (modifier, keycode) = chord.to_nu_binding();
                out.push_str(&format!(
                    r#"$env.config.keybindings = (
    ($env.config.keybindings? | default [])
    | append {{
        name: zpane_widget
        modifier: {modifier}
        keycode: {keycode}
        mode: [emacs, vi_normal, vi_insert]
        event: {{ send: executehostcommand, cmd: "_zpane_widget" }}
    }}
)
"#
                ));
            }
            out
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CliOptions {
    initial_buffer: String,
    cursor_offset: Option<usize>,
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
    Init {
        target: ShellInitTarget,
        bind_mode: InitBindMode,
    },
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
    if !incoming.session_vars.is_empty() {
        base.session_vars.extend(incoming.session_vars);
    }
    if !incoming.session_funcs.is_empty() {
        base.session_funcs.extend(incoming.session_funcs);
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

fn resolve_init_bind_mode(
    bind_chord: Option<InitKeyChord>,
    no_bind: bool,
) -> anyhow::Result<InitBindMode> {
    if bind_chord.is_some() && no_bind {
        anyhow::bail!("Conflicting options: --bind and --no-bind cannot be used together");
    }
    if no_bind {
        Ok(InitBindMode::Disabled)
    } else if let Some(chord) = bind_chord {
        Ok(InitBindMode::Bind(chord))
    } else {
        Ok(InitBindMode::default())
    }
}

fn parse_cursor_offset(raw: &str) -> anyhow::Result<usize> {
    raw.parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid integer for --cursor: {}", sanitize_error_arg(raw)))
}

fn parse_init_subcommand(args: &[String]) -> anyhow::Result<CliCommand> {
    let mut target: Option<ShellInitTarget> = None;
    let mut bind_chord: Option<InitKeyChord> = None;
    let mut no_bind = false;
    let mut i = 1;

    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "-h" || arg == "--help" {
            return Ok(CliCommand::Help);
        }
        if arg.starts_with('-') {
            let (flag, inline_val) = if arg.starts_with("--") {
                match arg.split_once('=') {
                    Some((f, v)) => (f, Some(v)),
                    None => (arg, None),
                }
            } else {
                (arg, None)
            };
            match flag {
                "-h" | "--help" => {
                    ensure_no_inline_value(flag, inline_val)?;
                    return Ok(CliCommand::Help);
                }
                "--bind" => {
                    let val = take_flag_value(flag, inline_val, args, &mut i)?;
                    bind_chord = Some(InitKeyChord::parse(val)?);
                }
                "--no-bind" => {
                    ensure_no_inline_value(flag, inline_val)?;
                    no_bind = true;
                    i += 1;
                }
                _ => {
                    anyhow::bail!(
                        "Unexpected argument '{}' for `init`",
                        sanitize_error_arg(arg)
                    );
                }
            }
        } else if target.is_none() {
            target = Some(ShellInitTarget::parse(arg)?);
            i += 1;
        } else {
            anyhow::bail!(
                "Unexpected argument '{}' for `init`",
                sanitize_error_arg(arg)
            );
        }
    }

    let Some(target) = target else {
        anyhow::bail!("Missing shell target for `init` (expected: pwsh, zsh, bash, fish, nu)");
    };
    let bind_mode = resolve_init_bind_mode(bind_chord, no_bind)?;
    Ok(CliCommand::Init { target, bind_mode })
}

fn parse_cli_args(args: &[String]) -> anyhow::Result<CliCommand> {
    if args.first().map(String::as_str) == Some("init") {
        return parse_init_subcommand(args);
    }

    let mut initial_buffer = String::new();
    let mut cursor_offset: Option<usize> = None;
    let mut out_file: Option<PathBuf> = None;
    let mut fallback_width: usize = 120;
    let mut fallback_height: usize = 30;
    let mut context_args: Vec<ContextArg<'_>> = Vec::new();
    let mut git_branch_override: Option<String> = None;
    let mut history_overrides: Vec<String> = Vec::new();
    let mut mode_cmd: Option<PendingMode> = None;
    let mut inspect_tokens: Vec<String> = Vec::new();
    let mut json = false;
    let mut init_bind_chord: Option<InitKeyChord> = None;
    let mut init_no_bind = false;

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
                    "Unexpected positional argument '{}' with standalone command flag",
                    sanitize_error_arg(arg)
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
            "--bind" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                init_bind_chord = Some(InitKeyChord::parse(val)?);
            }
            "--no-bind" => {
                ensure_no_inline_value(flag, inline_val)?;
                init_no_bind = true;
                i += 1;
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
            "--cursor" => {
                let val = take_flag_value(flag, inline_val, args, &mut i)?;
                cursor_offset = Some(parse_cursor_offset(val)?);
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

    if !matches!(mode_cmd, Some(PendingMode::Init(_)))
        && (init_bind_chord.is_some() || init_no_bind)
    {
        anyhow::bail!("Options --bind and --no-bind can only be used with `init` or `--init`");
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
            let bind_mode = resolve_init_bind_mode(init_bind_chord, init_no_bind)?;
            return Ok(CliCommand::Init { target, bind_mode });
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
        cursor_offset,
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

#[allow(clippy::too_many_arguments)]
fn build_interactive_session<I: zpane::discovery::Introspect>(
    pane1_entries: Vec<zpane::model::Entry>,
    introspector: I,
    initial_buffer: &str,
    cursor_offset: Option<usize>,
    viewport: ui::Viewport,
    dialect: ShellDialect,
    runtime_ctx: RuntimeContext,
    user_config: UserConfig,
    ui_style: UiStyle,
) -> MillerSession<I> {
    let session = MillerSession::new(
        pane1_entries,
        introspector,
        initial_buffer,
        viewport,
        dialect,
    )
    .with_runtime_context(runtime_ctx)
    .with_ui_style(ui_style)
    .with_keybindings(user_config.keybindings);
    if let Some(offset) = cursor_offset {
        session.with_cursor_offset(offset)
    } else {
        session
    }
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
        CliCommand::Init { target, bind_mode } => {
            print!("{}", render_shell_init(target, bind_mode));
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
                    return Err(e.context(
                        "PowerShell metadata sync failed (is `pwsh` or `powershell.exe` on PATH?)",
                    ));
                }
                Err(_) => {
                    if !json {
                        eprintln!(
                            "Skipping PowerShell metadata sync (`pwsh` / `powershell.exe` not available)."
                        );
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

    let (user_config, config_diagnostics) = load_user_config_with_diagnostics();
    for diag in &config_diagnostics {
        eprintln!("zpane config warning ({diag})");
    }
    let ui_style = user_config.ui.resolve_process_env();

    let runtime_ctx = detect_runtime_context(opts.runtime_context);
    let pane1_entries = load_catalog(dialect, &runtime_ctx);
    let introspector = CliIntrospector::with_timeout(
        dialect,
        &runtime_ctx,
        user_config.effective_introspect_timeout(),
    );

    let (tw, th) = size().unwrap_or((
        opts.fallback_width.min(u16::MAX as usize) as u16,
        opts.fallback_height.min(u16::MAX as usize) as u16,
    ));
    let viewport =
        ui::Viewport::from_terminal_size(tw, th, opts.fallback_width, opts.fallback_height);

    let reload_ctx = runtime_ctx.clone();
    let mut session = build_interactive_session(
        pane1_entries,
        introspector,
        &opts.initial_buffer,
        opts.cursor_offset,
        viewport,
        dialect,
        runtime_ctx,
        user_config,
        ui_style,
    )
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
                cursor_offset: None,
                out_file: Some(PathBuf::from("/tmp/out")),
                fallback_width: 80,
                fallback_height: 24,
                runtime_context: RuntimeContext::default(),
            })
        );

        let with_cursor = parse_cli_args(&s(&[
            "--initial",
            "git status && cargo test",
            "--cursor",
            "10",
        ]))
        .unwrap();
        assert_eq!(
            with_cursor,
            CliCommand::Interactive(CliOptions {
                initial_buffer: "git status && cargo test".to_string(),
                cursor_offset: Some(10),
                out_file: None,
                fallback_width: 120,
                fallback_height: 30,
                runtime_context: RuntimeContext::default(),
            })
        );
        let with_cursor_eq = parse_cli_args(&s(&["--cursor=0", "--initial=git"])).unwrap();
        assert_eq!(
            with_cursor_eq,
            CliCommand::Interactive(CliOptions {
                initial_buffer: "git".to_string(),
                cursor_offset: Some(0),
                out_file: None,
                fallback_width: 120,
                fallback_height: 30,
                runtime_context: RuntimeContext::default(),
            })
        );

        let err_small_w = parse_cli_args(&s(&["--width", "10"])).unwrap_err();
        assert!(err_small_w.to_string().contains("at least 40"));

        let err_bad_h = parse_cli_args(&s(&["--height", "abc"])).unwrap_err();
        assert!(err_bad_h.to_string().contains("Invalid integer"));

        let err_bad_cursor = parse_cli_args(&s(&["--cursor", "-1"])).unwrap_err();
        assert!(err_bad_cursor.to_string().contains("Invalid integer"));
        let err_missing_cursor = parse_cli_args(&s(&["--cursor"])).unwrap_err();
        assert!(err_missing_cursor.to_string().contains("Missing value"));

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
                cursor_offset: None,
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
                    ..RuntimeContext::default()
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
        let default_bind = InitBindMode::default();

        assert_eq!(
            parse_cli_args(&s(&["--clear-cache"])).unwrap(),
            CliCommand::ClearCache { json: false }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "pwsh"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Pwsh,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["--init", "powershell"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Pwsh,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "zsh"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Zsh,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "bash"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Bash,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "fish"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Fish,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["init", "nu"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Nushell,
                bind_mode: default_bind,
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["--init=nushell"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Nushell,
                bind_mode: default_bind,
            }
        );

        for target in [
            ShellInitTarget::Pwsh,
            ShellInitTarget::Zsh,
            ShellInitTarget::Bash,
            ShellInitTarget::Fish,
            ShellInitTarget::Nushell,
        ] {
            let script = render_shell_init(target, default_bind);
            assert!(script.contains("zpane --initial"));
            assert!(script.contains("--cursor"));
            assert!(script.contains("--out-file"));
        }

        let err_missing = parse_cli_args(&s(&["init"])).unwrap_err();
        assert!(err_missing.to_string().contains("Missing shell target"));

        let err_unknown = parse_cli_args(&s(&["init", "tcsh"])).unwrap_err();
        assert!(err_unknown.to_string().contains("Unknown shell target"));

        let err_extra = parse_cli_args(&s(&["init", "pwsh", "extra"])).unwrap_err();
        assert!(err_extra.to_string().contains("Unexpected argument"));

        assert_eq!(
            parse_cli_args(&s(&["init", "--help"])).unwrap(),
            CliCommand::Help
        );
        assert_eq!(
            parse_cli_args(&s(&["--init=fish"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Fish,
                bind_mode: default_bind,
            }
        );

        let pwsh = render_shell_init(ShellInitTarget::Pwsh, default_bind);
        assert!(pwsh.contains("function Invoke-ZpaneWidget"));
        assert!(pwsh.contains("Set-PSReadLineKeyHandler -Chord 'Alt+m'"));
        assert!(pwsh.contains("--cursor \"$cursor\""));
        assert!(pwsh.contains("@(Get-Content -LiteralPath $histPath"));
        assert!(pwsh.contains("ReadAllText($tmp"));
        assert!(pwsh.contains("if (-not [string]::IsNullOrEmpty($line))"));
        assert!(pwsh.contains("$LASTEXITCODE -eq 0"));
        assert!(!pwsh.contains("Invoke-Expression"));

        let fish = render_shell_init(ShellInitTarget::Fish, default_bind);
        assert!(fish.contains("umask 077"));
        assert!(fish.contains("umask $old_umask"));
        assert!(fish.contains("commandline -C"));
        assert!(fish.contains("history --max 15 2>/dev/null"));
        assert!(fish.contains("if test (count $buf) -gt 0"));
        assert!(fish.contains("zpane --initial \"\" --cursor 0 $hist_args --out-file"));
        assert!(fish.contains("rm -f -- \"$tmp\""));
        assert!(fish.contains("bind \\em _zpane_fish"));
        assert!(!fish.contains("eval "));

        let zsh = render_shell_init(ShellInitTarget::Zsh, default_bind);
        assert!(zsh.contains("tmp=\"$(umask 077 && mktemp)\" || return"));
        assert!(zsh.contains("setopt localoptions localtraps"));
        assert!(zsh.contains("trap 'rm -f -- \"$tmp\"' EXIT INT TERM"));
        assert!(zsh.contains("fc -ln -15 2>/dev/null || true"));
        assert!(zsh.contains("--initial \"$BUFFER\" --cursor \"$CURSOR\""));
        assert!(zsh.contains("bindkey '^[m' _zpane_widget"));
        assert!(!zsh.contains("eval "));

        let bash = render_shell_init(ShellInitTarget::Bash, default_bind);
        assert!(bash.contains("tmp=\"$(umask 077 && mktemp)\" || return"));
        assert!(bash.contains("trap 'rm -f -- \"$tmp\"' RETURN INT TERM"));
        assert!(bash.contains("history 15 2>/dev/null || true"));
        assert!(bash.contains("--initial \"$READLINE_LINE\" --cursor \"$READLINE_POINT\""));
        assert!(bash.contains("bind -x '\"\\em\": _zpane_readline'"));
        assert!(!bash.contains("eval "));

        let nu = render_shell_init(ShellInitTarget::Nushell, default_bind);
        assert!(nu.contains("def _zpane_widget []"));
        assert!(nu.contains("commandline get-cursor"));
        assert!(nu.contains("commandline edit --replace $accepted"));
        assert!(nu.contains("$env.config.keybindings"));
        assert!(nu.contains("modifier: alt"));
        assert!(nu.contains("keycode: char_m"));
        assert!(!nu.contains("eval "));
    }

    #[test]
    fn test_init_bind_and_no_bind_options_and_security_validation() {
        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();

        // 1. `--no-bind` omits keybinding registration while still defining widget/helper across all 5 shells
        for (shell_arg, expected_def, forbidden_bind) in [
            (
                "pwsh",
                "function Invoke-ZpaneWidget",
                "Set-PSReadLineKeyHandler",
            ),
            ("zsh", "zle -N _zpane_widget", "bindkey "),
            ("bash", "_zpane_readline()", "bind -x"),
            ("fish", "function _zpane_fish", "\nbind "),
            ("nu", "def _zpane_widget []", "$env.config.keybindings"),
        ] {
            for argv in [
                vec!["init", shell_arg, "--no-bind"],
                vec!["--init", shell_arg, "--no-bind"],
                vec!["--no-bind", "--init", shell_arg],
            ] {
                let cmd = parse_cli_args(&s(&argv))
                    .unwrap_or_else(|e| panic!("parse {argv:?} failed: {e}"));
                match cmd {
                    CliCommand::Init { target, bind_mode } => {
                        assert_eq!(bind_mode, InitBindMode::Disabled);
                        let script = render_shell_init(target, bind_mode);
                        assert!(script.contains(expected_def), "{shell_arg} missing def");
                        assert!(
                            !script.contains(forbidden_bind),
                            "{shell_arg} --no-bind emitted binding: {script}"
                        );
                    }
                    other => panic!("expected Init, got {other:?}"),
                }
            }
        }

        // 2. `--bind <CHORD>` and `--bind=<CHORD>` translate cleanly for every shell
        let ctrl_g = InitKeyChord::parse("Ctrl+g").unwrap();
        assert_eq!(
            parse_cli_args(&s(&["init", "pwsh", "--bind", "Ctrl+g"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Pwsh,
                bind_mode: InitBindMode::Bind(ctrl_g),
            }
        );
        assert_eq!(
            parse_cli_args(&s(&["--init=zsh", "--bind=Ctrl+g"])).unwrap(),
            CliCommand::Init {
                target: ShellInitTarget::Zsh,
                bind_mode: InitBindMode::Bind(ctrl_g),
            }
        );

        assert!(
            render_shell_init(ShellInitTarget::Pwsh, InitBindMode::Bind(ctrl_g))
                .contains("Set-PSReadLineKeyHandler -Chord 'Ctrl+g'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Zsh, InitBindMode::Bind(ctrl_g))
                .contains("bindkey '^G' _zpane_widget")
        );
        assert!(
            render_shell_init(ShellInitTarget::Bash, InitBindMode::Bind(ctrl_g))
                .contains("bind -x '\"\\C-g\": _zpane_readline'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Fish, InitBindMode::Bind(ctrl_g))
                .contains("bind \\cg _zpane_fish")
        );
        let nu_ctrl_g = render_shell_init(ShellInitTarget::Nushell, InitBindMode::Bind(ctrl_g));
        assert!(nu_ctrl_g.contains("modifier: control"));
        assert!(nu_ctrl_g.contains("keycode: char_g"));

        let ctrl_space = InitKeyChord::parse("Ctrl+Space").unwrap();
        assert!(
            render_shell_init(ShellInitTarget::Pwsh, InitBindMode::Bind(ctrl_space))
                .contains("-Chord 'Ctrl+Spacebar'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Zsh, InitBindMode::Bind(ctrl_space))
                .contains("bindkey '^@' _zpane_widget")
        );
        assert!(
            render_shell_init(ShellInitTarget::Bash, InitBindMode::Bind(ctrl_space))
                .contains("bind -x '\"\\C-@\": _zpane_readline'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Fish, InitBindMode::Bind(ctrl_space))
                .contains("bind \\c@ _zpane_fish")
        );
        let nu_ctrl_space =
            render_shell_init(ShellInitTarget::Nushell, InitBindMode::Bind(ctrl_space));
        assert!(
            nu_ctrl_space.contains("modifier: control") && nu_ctrl_space.contains("keycode: space")
        );

        let f2 = InitKeyChord::parse("F2").unwrap();
        assert!(
            render_shell_init(ShellInitTarget::Pwsh, InitBindMode::Bind(f2))
                .contains("-Chord 'F2'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Zsh, InitBindMode::Bind(f2))
                .contains("bindkey '^[OQ' _zpane_widget")
        );
        assert!(
            render_shell_init(ShellInitTarget::Bash, InitBindMode::Bind(f2))
                .contains("bind -x '\"\\eOQ\": _zpane_readline'")
        );
        assert!(
            render_shell_init(ShellInitTarget::Fish, InitBindMode::Bind(f2))
                .contains("bind -k f2 _zpane_fish")
        );
        assert!(
            render_shell_init(ShellInitTarget::Nushell, InitBindMode::Bind(f2))
                .contains("keycode: f2")
        );

        // 3. Conflicts, misuse outside `init`, and shell-injection chords must fail cleanly
        for bad_args in [
            vec!["init", "pwsh", "--bind", "Alt+m", "--no-bind"],
            vec!["--init", "zsh", "--bind=Ctrl+g", "--no-bind"],
            vec!["--bind", "Alt+m"],
            vec!["--no-bind"],
            vec!["--clear-cache", "--no-bind"],
            vec!["init", "bash", "--no-bind=1"],
            vec!["init", "bash", "--bind"],
            vec!["init", "bash", "--bind", ""],
            vec!["init", "bash", "--bind", "m"],
            vec!["init", "bash", "--bind", "Ctrl+1"],
            vec!["init", "bash", "--bind", "Alt+m'; rm -rf /"],
            vec!["init", "pwsh", "--bind", "Alt+$(id)"],
            vec!["init", "zsh", "--bind", "Alt+m\nwhoami"],
            vec!["init", "nu", "--bind", "Alt+\x1b[31mm"],
        ] {
            let err = match parse_cli_args(&s(&bad_args)) {
                Ok(cmd) => panic!("expected error for {bad_args:?}, got {cmd:?}"),
                Err(err) => err,
            };
            let msg = err.to_string();
            assert!(
                !msg.contains('\x1b') && !msg.contains('\n'),
                "unsanitized err: {msg:?}"
            );
        }
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
                cursor_offset: None,
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
                cursor_offset: None,
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
                cursor_offset: None,
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

    #[test]
    fn test_build_interactive_session_wires_user_config_ui_and_env_overrides() {
        use zpane::discovery::parse_user_config_toml_with_diagnostics;

        let (user_config, diags) = parse_user_config_toml_with_diagnostics(
            r#"
[ui]
theme = "dark"
icons = "ascii"
borders = "plain"
"#,
        );
        assert!(diags.is_empty());

        let ui_style = user_config.ui.resolve_env(Some("1"), None);
        let mut session = build_interactive_session(
            vec![Entry::new("git", "git", EntryKind::Cli, "Git VCS")],
            NullIntrospector,
            "",
            None,
            Viewport::new(80, 14),
            ShellDialect::Posix,
            RuntimeContext::default(),
            user_config,
            ui_style,
        );

        let frame = session.render();
        assert!(frame.contains("┌"));
        assert!(frame.contains("> git"));
        assert!(!frame.contains("\x1b[38;5;"));
    }

    #[test]
    fn test_pwsh_shell_init_and_context_merge_include_session_vars_and_funcs() {
        let init = render_shell_init(ShellInitTarget::Pwsh, InitBindMode::default());
        assert!(init.contains("session_vars"));
        assert!(init.contains("session_funcs"));
        assert!(init.contains("--context-file"));
        assert!(init.contains("ConvertTo-Json -Depth 4 -Compress"));

        let s = |vals: &[&str]| vals.iter().map(|v| (*v).to_string()).collect::<Vec<_>>();
        let parsed = parse_cli_args(&s(&[
            "--context",
            r#"{"session_vars":[{"name":"apiUrl","value":"https://example.test"}],"session_funcs":[{"name":"Invoke-Local","desc":"Custom"}]}"#,
        ]))
        .expect("parse context with session vars/funcs");

        match parsed {
            CliCommand::Interactive(opts) => {
                assert_eq!(opts.runtime_context.session_vars.len(), 1);
                assert_eq!(opts.runtime_context.session_vars[0].name, "apiUrl");
                assert_eq!(opts.runtime_context.session_funcs.len(), 1);
                assert_eq!(opts.runtime_context.session_funcs[0].name, "Invoke-Local");
            }
            other => panic!("expected Interactive, got {other:?}"),
        }
    }
}
