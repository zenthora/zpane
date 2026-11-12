# `miller-cmd`

**Blazing-fast 3-pane Miller Column Command, Subcommand, Parameter & Variable Explorer for PowerShell and Windows CLI.**

`miller-cmd` brings the spatial orientation of 3-column Miller file managers (`yazi`, `ranger`, `lf`) to interactive shell command construction. Instead of cycling through flat 1D tab-completion dropdowns or opening slow GUI dialogs (`Show-Command`), `miller-cmd` lets you visually traverse commands, subcommands, PowerShell parameter sets, enum constraints, and environment variables in a zero-flicker terminal overlay—injecting the assembled command straight back into your active `PSReadLine` prompt buffer.

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
│ [↑/↓] Move  [←/→] Column  [Type] Fuzzy Filter  [Tab] Append Token  [Ctrl+R] Sync PS  [Enter] Accept  [Esc]   │
╰──────────────────────────────────────────────────────────────────────────────────────────────────────────────╯
```

---

## Why `miller-cmd`?

Modern Windows terminal workflows straddle two worlds: **PowerShell's rich CLR object/cmdlet ecosystem** (`Verb-Noun`, `[ValidateSet]` enums, mutually exclusive `ParameterSets`, `$env:*` / `$global:*` scopes) and **polyglot native CLI executables** (`git`, `cargo`, `winget`, `gh`, `docker`, `uv`, `prek`). Existing tools only cover one piece of the puzzle:

| Capability | `miller-cmd` | `bract` | `inshellisense` | `carapace-bin` | `Show-Command` | `PSReadLine` |
| :--- | :---: | :---: | :---: | :---: | :---: | :---: |
| **3-Pane Cascading Miller Layout** | **Yes** | Yes | No (1D Popup) | No (Host Menu) | No (WPF GUI) | No (2D Grid) |
| **Zero-Config Live CLI `--help` Parsing** | **Yes** | Yes | No (Static Specs) | No (Static Specs) | No | No |
| **PowerShell Cmdlet & `[ValidateSet]` Reflection** | **Yes** | No | No | Partial | Yes | Yes |
| **Dynamic `ParameterSet` Exclusivity Pruning** | **Yes** | No | No | No | Yes | No |
| **Interactive `$env:*` & `$global:*` Inspector** | **Yes** | No | No | No | No | Partial |
| **In-Place `PSReadLine` Prompt Replacement** | **Yes** | No | Yes | Yes | No | Yes |

See [`docs/prior-art-and-alternatives.md`](docs/prior-art-and-alternatives.md) for a deep-dive architectural comparison.

---

## Key Features

- **Dual-Engine Hybrid Discovery**:
  - **PowerShell CLR Reflection (`ps_metadata.json`)**: Indexes Cmdlets, Functions, and Aliases along with parameter types, shorthand aliases, `[Switch]` flags, `[ValidateSet]` / `Enum` values, and `ParameterSet` memberships.
  - **Asynchronous JIT CLI Introspection (`cli_cache.json`)**: Scans `%PATH%` for native executables (`.exe`, `.cmd`, `.bat`) and lazily introspects `--help` / `-h` in a LIFO-prioritized background worker using `CREATE_NO_WINDOW`. Includes section-aware grammar parsers for **Cobra** (`gh`, `kubectl`, `docker`), **Clap** (`cargo`, `rg`, `fd`, `prek`), **winget**, and **git** (`-h`), extracting `[possible values: ...]` and `[default: ...]` automatically.
- **Dynamic `ParameterSet` Exclusivity Pruning**:
  - Selecting a parameter bound to a specific PowerShell `ParameterSet` (e.g., `Get-Process -Id`) automatically prunes incompatible parameters (such as `-Name`) from Column 2 and displays the active set in the column header (`2. Params [Set: Id]`).
- **Interactive Value & Argument Prompts**:
  - Typing in Column 3 on any parameter or subcommand dynamically creates an interactive `✏ Use "<value>"` item (inspired by `navi`) and interpolates it into the live command preview.
- **First-Class Environment & Variable Scopes**:
  - Browse `$env:Variables` (including individual `;`-delimited `%PATH%` entries) and `$global:Variables` (`$PWD`, `$HOME`, `$PROFILE`, `$_`) directly from Column 1, choosing between inserting the variable token (`$env:TEMP`) or its quoted literal value.
- **Sub-Millisecond Synchronized Win32 Rendering**:
  - Directly drives `CONOUT$` via `WriteConsoleW` on the alternate screen buffer with DEC Mode 2026 Synchronized Output (`\x1b[?2026h`…`\x1b[?2026l`), event-burst coalescing, and zero-allocation fuzzy subsequence matching (~85µs per interactive step across 5,000+ commands).

---

## Installation

### Build from Source

Requires Rust 1.80+ and PowerShell 7 (`pwsh`) on Windows:

```powershell
cargo install --path .
```

### Pre-Warm Metadata & Priority CLI Cache (Recommended)

Run once after installation to export PowerShell cmdlet metadata and pre-cache `--help` trees for common CLI tools (`git`, `cargo`, `winget`, `rg`, `fd`, `uv`, `prek`, etc.):

```powershell
miller-cmd --warm
```

---

## PowerShell (`PSReadLine`) Integration

Add the following keybinding to your PowerShell `$PROFILE` (`notepad $PROFILE`) to launch `miller-cmd` with **`Ctrl+M`** (or `Alt+M`), pre-seeded with whatever you have already typed on the command line:

```powershell
Set-PSReadLineKeyHandler -Chord 'Alt+m' -ScriptBlock {
    $line = $null
    $cursor = $null
    [Microsoft.PowerShell.PSConsoleReadLine]::GetBufferState([ref]$line, [ref]$cursor)

    $tmp = [System.IO.Path]::GetTempFileName()
    try {
        miller-cmd --initial "$line" --out-file "$tmp"
        if (Test-Path $tmp) {
            $accepted = (Get-Content -Path $tmp -Raw -ErrorAction SilentlyContinue)
            if ($accepted) {
                $accepted = $accepted.Trim()
                [Microsoft.PowerShell.PSConsoleReadLine]::Replace(0, $line.Length, $accepted)
            }
        }
    } finally {
        Remove-Item -Path $tmp -Force -ErrorAction SilentlyContinue
    }
}
```

---

## Keyboard Shortcuts

| Key | Action |
| :--- | :--- |
| <kbd>↑</kbd> / <kbd>↓</kbd> | Move selection up / down in the active column (wraps around) |
| <kbd>PgUp</kbd> / <kbd>PgDn</kbd> | Jump 12 items up / down |
| <kbd>Home</kbd> / <kbd>End</kbd> | Jump to first / last item in the active column |
| <kbd>→</kbd> / <kbd>←</kbd> | Move focus to the next / previous Miller column |
| <kbd>Tab</kbd> | Append the highlighted token/flag/value to the command line and advance |
| `Type any char` | Fuzzy-filter the active column (or type a custom argument value in Column 3) |
| <kbd>Backspace</kbd> | Delete the last filter character, or pop the last appended command token if the filter is empty |
| <kbd>Ctrl+R</kbd> | Re-synchronize PowerShell cmdlet/function/alias metadata (`ps_metadata.json`) |
| <kbd>Enter</kbd> | Accept the assembled command line and exit |
| <kbd>Esc</kbd> / <kbd>Ctrl+C</kbd> | Cancel and exit without modifying the prompt |

---

## CLI Options

```text
miller-cmd [OPTIONS] [INITIAL_TOKENS]...

Options:
  --initial <TEXT>     Seed the command builder with an existing command buffer
  --out-file <PATH>    Write the accepted command to <PATH> instead of stdout
  --width <COLS>       Fallback terminal width (default: 120)
  --height <ROWS>      Fallback terminal height (default: 30)
  --sync-ps            Synchronize PowerShell cmdlet & parameter metadata and exit
  --warm               Synchronize PowerShell metadata + pre-warm priority CLI cache and exit
```

---

## Development

This repository uses [`prek`](https://prek.j178.dev) for pre-commit hooks and [`git-cliff`](https://git-cliff.org) for changelog generation:

```powershell
# Install git pre-commit hooks
prek install

# Run formatting, clippy, and unit/performance tests across all files
prek run --all-files

# Generate / update CHANGELOG.md
git-cliff -o CHANGELOG.md
```

## License

Licensed under the [MIT License](LICENSE).
