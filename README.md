# `zpane`

**Blazing-fast 3-pane Miller Column Command, Subcommand, Parameter & Variable Explorer for PowerShell, Linux, and Termux.**

`zpane` brings the spatial orientation of 3-column Miller file managers (`yazi`, `ranger`, `lf`) to interactive shell command construction. Instead of cycling through flat 1D tab-completion dropdowns or opening slow GUI dialogs (`Show-Command`), `zpane` lets you visually traverse commands, subcommands, PowerShell parameter sets, enum constraints, and environment variables in a zero-flicker terminal overlay—injecting the assembled command straight back into your active shell prompt buffer.

```text
╭ 1. Command / Scope (4921) ─────┬ 2. Params [Set: Id] (19) ──────────┬ 3. Flags / Values / Info (11) ─────────╮
│   $env:Variables         [Env] │ ❯ -Id (-PID)               [Param] │ ℹ -Id (-PID) [Param]                   │
│   $global:Variables      [Var] │   -FileVersionInfo        [Switch] │   [Int32[]] Parameter -Id (-PID)       │
│   git                    [CLI] │   -Module                 [Switch] │────────────────────────────────────────│
│   prek                   [CLI] │   -IncludeUserName        [Switch] │ ❯ ✏ (Type custom value / path) [Value] │
│ ❯ Get-Process         [Cmdlet] │   -Verbose                [Switch] │   $PWD (Current Dir)             [Var] │
│   Get-ChildItem       [Cmdlet] │   -ErrorAction             [Param] │   $HOME (User Home)              [Var] │
├────────────────────────────────┴────────────────────────────────────┴────────────────────────────────────────┤
│ Filter (Col 2): █   [Col1: '' | Col2: '' | Col3: '']                                                         │
│ Command Line  : Get-Process -Id                                                                              │
│ [↑/↓] Move  [←/→] Column  [Type] Filter  [Tab] Append  [Ctrl+U] Clear  [Ctrl+R] Sync  [Enter] Accept  [Esc]  │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

---

## Why `zpane`?

Modern terminal workflows straddle two worlds: **PowerShell's rich CLR object/cmdlet ecosystem** (`Verb-Noun`, `[ValidateSet]` enums, mutually exclusive `ParameterSets`, `$env:*` / `$global:*` scopes) and **polyglot native CLI executables** (`git`, `cargo`, `winget`, `gh`, `docker`, `uv`, `prek`, `dnf`, `apt`, `pacman`, `pkg`). Existing tools only cover one piece of the puzzle:

| Capability | `zpane` | `bract` | `inshellisense` | `carapace-bin` | `Show-Command` | `PSReadLine` |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **3-Pane Cascading Miller Layout** | **Yes** | Yes | No (1D Popup) | No (Host Menu) | No (WPF GUI) | No (2D Grid) |
| **Zero-Config Live CLI `--help` Parsing** | **Yes** | Yes | No (Static Specs) | No (Static Specs) | No | No |
| **PowerShell Cmdlet & `[ValidateSet]` Reflection** | **Yes** | No | No | Partial | Yes | Yes |
| **Dynamic `ParameterSet` Exclusivity Pruning** | **Yes** | No | No | No | Yes | No |
| **Interactive `$env` / `$SHELL` Variable Inspector** | **Yes** | No | No | No | No | Partial |
| **In-Place Shell Prompt Buffer Replacement** | **Yes** | No | Yes | Yes | No | Yes |
| **Windows, Arch, Debian, Fedora & Termux** | **Yes** | Unix | Node.js | Yes | Windows GUI | PowerShell |

See [`docs/prior-art-and-alternatives.md`](docs/prior-art-and-alternatives.md) for a deep-dive architectural comparison.

---

## Key Features

- **Dual-Engine Hybrid Discovery**:
  - **PowerShell CLR Reflection (`ps_metadata.json`)**: Indexes Cmdlets, Functions, and Aliases along with parameter types, shorthand aliases, `[Switch]` flags, `[ValidateSet]` / `Enum` values, and `ParameterSet` memberships.
  - **Asynchronous JIT CLI Introspection (`cli_cache.json`)**: Scans `PATH` for native executables (`.exe`/`.cmd`/`.bat` on Windows; `+x` binaries and symlinks on Linux & Termux) and lazily introspects `--help` / `-h` in a LIFO-prioritized background worker. Includes section-aware grammar parsers for **Cobra** (`gh`, `kubectl`, `docker`), **Clap** (`cargo`, `rg`, `fd`, `prek`), **winget**, and **git** (`-h`), extracting `[possible values: ...]` and `[default: ...]` automatically.
- **Declarative Specs & Carapace Bridge**:
  - Loads custom `carapace-spec` (`.yaml`/`.yml`/`.json`) and simplified Fig (`.json`) command specs from `%LOCALAPPDATA%\zpane\specs\` (Windows) or `$XDG_CACHE_HOME/zpane/specs` / `~/.cache/zpane/specs` (Unix), and queries `carapace <cmd> export` automatically when `--help`/`-h` parsing yields no entries or when subcommands lack dynamic value candidates.
- **Dynamic Runtime Context & Multi-Level Drill-Down**:
  - Automatically detects current Git branches, working-directory entries, and shell history (`ConsoleHost_history.txt`, `~/.zsh_history`, `~/.bash_history`, `fish_history`), indexes POSIX/Fish shell aliases & functions alongside PowerShell cmdlets, slides the 3-pane Miller window through nested subcommand chains (`git remote add ...`), and supports quote-aware `--initial` seeding (`"..."`, `|`, `&&`) and `=`/` :$` parameter value attachment.
- **Expandable Inspector & User Configuration (`config.toml`)**:
  - Press `F1` (or `Alt+I`/`Alt+?`) to open a full wrapped parameter/description drawer with aliases and Parameter Sets, navigate with home-row bindings (`Ctrl+N`/`Ctrl+P`, `Alt+J`/`Alt+K`), and customize priority/disabled tools or `history_limit` in `%LOCALAPPDATA%\zpane\config.toml` or `~/.config/zpane/config.toml`.
- **Automatic Shell Dialect Adaptation (`PowerShell`, `POSIX`, and `Fish`)**:
  - Emits `$env:VAR` and indexes PowerShell cmdlets when running on Windows or inside `pwsh` on Linux; automatically switches to `$VAR` tokens and `:`-separated `PATH` inspection in `bash`, `zsh`, `fish` (with Fish-native `$status`, `$last_pid`, `$fish_pid` variables), and Android Termux (override anytime with `ZPANE_SHELL=pwsh|posix|fish`).
- **Dynamic `ParameterSet` Exclusivity Pruning**:
  - Selecting a parameter bound to a specific PowerShell `ParameterSet` (e.g., `Get-Process -Id`) automatically prunes incompatible parameters (such as `-Name`) from Column 2 and displays the active set in the column header (`2. Params [Set: Id]`).
- **Interactive Value & Argument Prompts**:
  - Typing in Column 3 on any parameter or subcommand dynamically creates an interactive `✏ Use "<value>"` item (inspired by `navi`) and interpolates it into the live command preview.
- **Sub-Millisecond Synchronized Terminal Rendering**:
  - Renders directly to `CONOUT$` (Windows) or `/dev/tty` (Linux/Termux) on the alternate screen buffer with DEC Mode 2026 Synchronized Output (`\x1b[?2026h`…`\x1b[?2026l`), event-burst coalescing, and zero-allocation fuzzy subsequence matching (~85µs per interactive step across 5,000+ commands).

---

## Installation

### Windows (Cargo)

```powershell
cargo install --path .
zpane --warm
```

### Arch Linux (`pacman` / `makepkg`)

Install the prebuilt `.pkg.tar.zst` release package or build from [`packaging/arch/PKGBUILD`](packaging/arch/PKGBUILD):

```bash
# Install release package directly
sudo pacman -U dist/zpane-0.3.0-1-x86_64.pkg.tar.zst

# Or build from PKGBUILD
cd packaging/arch && makepkg -si
```

### Debian / Ubuntu (`.deb`)

```bash
sudo dpkg -i dist/zpane_0.3.0-1_amd64.deb
```

### Fedora / RHEL / openSUSE (`.rpm`)

Install the prebuilt `.rpm` package via `dnf` / `rpm` or build from [`packaging/fedora/zpane.spec`](packaging/fedora/zpane.spec):

```bash
sudo dnf install ./dist/zpane-0.3.0-1.x86_64.rpm
# or
sudo rpm -Uvh dist/zpane-0.3.0-1.x86_64.rpm
```

### Android Termux (`aarch64`)

Install the Termux `.deb` package (built with the `/data/data/com.termux/files/usr` prefix) or see [`packaging/termux/build.sh`](packaging/termux/build.sh):

```bash
dpkg -i zpane_0.3.0_aarch64.deb
zpane --warm
```

---

## Shell Integration

Generate and evaluate the integration script directly with `zpane init`:

- **PowerShell (`$PROFILE`)**: `Invoke-Expression (& zpane init pwsh | Out-String)`
- **Zsh (`~/.zshrc`)**: `eval "$(zpane init zsh)"`
- **Bash (`~/.bashrc`)**: `eval "$(zpane init bash)"`
- **Fish (`~/.config/fish/config.fish`)**: `zpane init fish | source`

Or paste the explicit snippets below:

### PowerShell (`$PROFILE`)

Bind `Alt+M` in `PSReadLine` to open `zpane` seeded with your current prompt line and replace it in place upon pressing `Enter`:

```powershell
Set-PSReadLineKeyHandler -Chord 'Alt+m' -ScriptBlock {
    $line = $null
    $cursor = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)

    $tmp = [System.IO.Path]::GetTempFileName()
    $ctxFile = [System.IO.Path]::GetTempFileName()
    try {
        $histPath = (Get-PSReadLineOption).HistorySavePath
        $recent = @()
        if ($histPath -and (Test-Path $histPath)) {
            $recent = Get-Content -Path $histPath -Tail 15 -ErrorAction SilentlyContinue
            [array]::Reverse($recent)
        }
        $ctxPayload = @{ history = @($recent) } | ConvertTo-Json -Compress
        [System.IO.File]::WriteAllText($ctxFile, $ctxPayload, [System.Text.UTF8Encoding]::new($false))

        zpane --initial "$line" --context-file "$ctxFile" --out-file "$tmp"
        if (Test-Path $tmp) {
            $accepted = (Get-Content -Path $tmp -Raw -ErrorAction SilentlyContinue)
            if ($accepted) {
                $accepted = $accepted.Trim()
                [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $accepted)
            }
        }
    } finally {
        Remove-Item -Path $tmp, $ctxFile -Force -ErrorAction SilentlyContinue
    }
}
```

### Zsh (`~/.zshrc`)

```zsh
_zpane_widget() {
    local tmp="$(mktemp)"
    zpane --initial "$BUFFER" --out-file "$tmp"
    if [[ -s "$tmp" ]]; then
        BUFFER="$(<"$tmp")"
        CURSOR=${#BUFFER}
    fi
    rm -f "$tmp"
    zle redisplay
}
zle -N _zpane_widget
bindkey '^[m' _zpane_widget
```

### Bash (`~/.bashrc`)

```bash
_zpane_readline() {
    local tmp
    tmp="$(mktemp)"
    zpane --initial "$READLINE_LINE" --out-file "$tmp"
    if [[ -s "$tmp" ]]; then
        READLINE_LINE="$(<"$tmp")"
        READLINE_POINT=${#READLINE_LINE}
    fi
    rm -f "$tmp"
}
bind -x '"\em": _zpane_readline'
```

### Fish (`~/.config/fish/config.fish`)

```fish
function _zpane_fish
    set -l tmp (mktemp)
    zpane --initial (commandline -b) --out-file "$tmp"
    if test -s "$tmp"
        commandline -r (string trim (cat "$tmp"))
        commandline -C (string length (commandline -b))
    end
    rm -f "$tmp"
    commandline -f repaint
end
bind \em _zpane_fish
```

---

## Declarative Specs & Carapace Bridge

- Drop custom `carapace-spec` (`.yaml`/`.yml`/`.json`) or simplified Fig (`.json`) specs into `%LOCALAPPDATA%\zpane\specs\` (Windows) or `$XDG_CACHE_HOME/zpane/specs` / `~/.cache/zpane/specs` (Unix).
- If [`carapace`](https://carapace.sh/) is installed on `PATH`, `zpane` queries `carapace <cmd> export` automatically whenever `--help`/`-h` parsing yields no entries.

---

## Keyboard Shortcuts

| Key | Action |
| :--- | :--- |
| <kbd>↑</kbd> / <kbd>↓</kbd> / <kbd>Ctrl+N</kbd> / <kbd>Ctrl+P</kbd> | Move selection up / down in the active column (also <kbd>Alt+J</kbd> / <kbd>Alt+K</kbd>) |
| <kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Jump one visible screenful up / down in the active column (accounts for Column 3's inspector header) |
| <kbd>Home</kbd> / <kbd>End</kbd> | Jump to first / last item in the active column |
| <kbd>→</kbd> / <kbd>←</kbd> | Move focus to the next / previous Miller column (slides into deeper subcommand chains on <kbd>Subcmd</kbd>) |
| <kbd>Tab</kbd> | Append the highlighted token/flag/value to the command line and advance (quotes spaced custom values) |
| `Type any char` | Fuzzy-filter the active column (or type a custom argument value in Column 3) |
| <kbd>Backspace</kbd> | Delete the last filter character, or pop the last appended command token if the filter is empty |
| <kbd>Ctrl+U</kbd> | Clear the active column's fuzzy filter |
| <kbd>F1</kbd> / <kbd>Alt+I</kbd> | Toggle expanded multi-line Inspector drawer in Column 3 (full description, aliases, parameter sets) |
| <kbd>Ctrl+R</kbd> | Invalidate active CLI cache + re-sync PowerShell metadata (`Sync PS`) or reload the command catalog (`Reload`) |
| <kbd>Enter</kbd> | Accept the assembled command line and exit |
| <kbd>Esc</kbd> / <kbd>Ctrl+C</kbd> | Close expanded Inspector (if open), or cancel and exit without modifying the prompt |

---

## Configuration (`config.toml`)

Create `%LOCALAPPDATA%\zpane\config.toml` (Windows) or `~/.config/zpane/config.toml` (`$XDG_CONFIG_HOME/zpane/config.toml` on Unix, or set `ZPANE_CONFIG`):

```toml
history_limit = 20
priority_tools = ["git", "cargo", "rg"]
disabled_tools = ["unwanted-bin"]
```

---

## CLI Options

```text
zpane - 3-pane Miller column command, parameter & variable explorer

Usage: zpane [OPTIONS] [INITIAL_TOKENS]...
       zpane init <pwsh|zsh|bash|fish>

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
      --sync-ps              Sync PowerShell cmdlet & parameter metadata (requires pwsh) and exit
      --warm                 Sync PowerShell metadata (if available) + pre-warm CLI help cache and exit
      --clear-cache          Delete persisted CLI help & PowerShell metadata cache files and exit
  -h, --help                 Print help
  -V, --version              Print version

Environment:
  ZPANE_SHELL                Force token dialect: `pwsh` ($env:X, cmdlets), `posix` ($X), or `fish` ($status)
  ZPANE_CONFIG               Override user config path (`config.toml`)
```

---

## Development & Release Packaging

This repository uses [`prek`](https://prek.j178.dev) for `pre-commit` / `commit-msg` hooks and [`git-cliff`](https://git-cliff.org) for SemVer calculation and changelog generation:

```powershell
# Install git pre-commit and commit-msg (Conventional Commits) hooks
prek install

# Run formatting, clippy, tests, and config checks across all files
prek run --all-files

# Compute next SemVer via git-cliff, sync version manifests, and update CHANGELOG.md
prek run --stage manual git-cliff-changelog
```

## License

Licensed under the [MIT License](LICENSE).
