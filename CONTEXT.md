# zpane

An interactive 3-pane Miller-column command explorer that lets shell users visually traverse commands, parameters, and variables and emit an assembled command back into their shell prompt.

## Language

### Navigation & Structure

**Pane**:
One of the three cascading vertical lists arranged left-to-right (Pane 1 for commands and variable scopes, Pane 2 for subcommands and options, Pane 3 for values, arguments, and inspection).
_Avoid_: Column, Col

**Entry**:
A row displayed inside a Pane, representing either a navigable container, an emittable command token, or a non-emittable UI indicator.
_Avoid_: Item, MillerItem, Row

**Scope Root**:
A synthetic Entry in Pane 1 (such as `$env:` or `$`) that opens a collection of variables in Pane 2 without contributing a token to the emitted command.
_Avoid_: Env root, Var root

**Placeholder**:
A non-emittable Entry (such as a background introspection loading indicator or a switch no-value hint) that communicates state inside a Pane.
_Avoid_: Loading item, Dummy item

### Aliases

**Command Alias**:
A shorthand name in Pane 1 that refers to an underlying command or cmdlet.
_Avoid_: Alias

**Parameter Alias**:
An alternate flag name (such as `-PID` for `-Id`) belonging to a specific parameter or option in Pane 2.
_Avoid_: Alias

### Options

**Parameter**:
An option in Pane 2 that accepts a value or argument in Pane 3.
_Avoid_: Flag (when it takes a value)

**Flag**:
A boolean or toggle option in Pane 2 that takes no required value (including PowerShell switches, which may optionally accept explicit `:$true` or `:$false`).
_Avoid_: Switch, Parameter (when valueless)

**Parameter Set**:
A named group of mutually compatible options on a PowerShell command, alongside universal options valid in every set.
_Avoid_: Param set

### Command Construction

**Token**:
An individual shell word (command, subcommand, Parameter, Flag, value, or variable reference) that can appear in a Command Line.

**Committed Token**:
A Token locked into the session through initial seeding or explicit acceptance, preserved across navigation until removed.
_Avoid_: Accepted token

**Command Line**:
The assembled shell command string displayed in the status footer and emitted to the host shell on confirmation.
_Avoid_: Command buffer, Command preview, Preview

### Discovery & Shells

**Shell Dialect**:
The target shell family (`PowerShell`, `POSIX`, or `Fish`) governing variable syntax, path-list separators, built-in variables, and cmdlet availability.
_Avoid_: Shell type, Shell mode

**Command Catalog**:
The top-level collection of Pane 1 Entries (commands, Command Aliases, functions, and Scope Roots) loaded for the active Shell Dialect.
_Avoid_: Col1 items, Command list

**Introspection**:
The on-demand extraction of subcommands, Parameters, Flags, and allowed values from a native CLI's help output for a command chain.
_Avoid_: Help scraping, Discovery parsing
