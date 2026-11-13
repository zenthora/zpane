---
name: pr
description: "Use when writing a PR body."
metadata:
  credits:
    skill: show-me
    author: Dex Horthy
    organisation: Humanlayer
    url: "https://github.com/humanlayer/skills/blob/main/plugins/show-me/skills/show-me/SKILL.md"
---

Use this template for writing the PR body:

```markdown
## Summary

<diagram, diff-sketch, or tree>

## Evidence

- **Before:** <terminal capture / failing `cargo test` output>
  **After:** <terminal capture / passing `cargo test` & `cargo clippy` output>

## Merge Danger

**Door:** <one-way or two-way>

<optional: description>

**Blast Radius:** <one-word description>

<optional: potential ramifications of merge>
```

## Sections

Skip all preambles and keep prose brief. Use the user's domain language from `GLOSSARY.md` (or `CONTEXT.md` if present).

### Summary

Pick the smallest view that makes the key point clear.

- Show logic or an algorithm as pseudocode:

```text
on(AcceptToken)
  if col2 item is childless flag or switch
    commit token, reset col2 filter, suppress index-0 preview
  else
    commit token and step into col3
```

- Show runtime control flow as a call tree:

```text
main
  MillerSession::new
    CliIntrospector::new
    load_col1_commands(dialect)
  loop
    session.poll_introspector()
    session.render()
    session.handle_terminal_event(ev)
```

- Show TUI layout structure as a pane/region sketch, including state seams that matter:

```text
RenderState (ui.rs)
  ├── Col 1: Command / Scope  (viewport.rows(Column::First))
  ├── Col 2: Params / Subcmds (viewport.rows(Column::Second))
  └── Col 3: Inspector + Values (3 header rows + viewport.rows(Column::Third))
```

- Show file responsibility or a broad refactor as a shallow file tree:

```text
src/
├── main.rs         # CLI args, CONOUT$/tty guard, event loop
├── model.rs        # Core MillerItem, ItemKind, Column types
├── session.rs      # Interactive 3-pane state machine
├── discovery.rs    # Shell dialect, PS metadata & CLI introspection
├── fuzzy.rs        # Subsequence matching & scoring
└── ui.rs           # Viewport geometry & ANSI frame rendering
```

- Show component interaction, control flow, or data flow with Mermaid:

```mermaid
sequenceDiagram
    participant Loop as Event Loop (main.rs)
    participant Session as MillerSession
    participant Introspector as CliIntrospector
    Loop->>Session: apply(UserAction)
    Session->>Introspector: query(&["git"])
    Introspector-->>Session: [Loading] + enqueue worker
    Loop->>Session: poll_introspector()
    Introspector-->>Session: cache updated -> dirty
```

- Use `diff` when the point is what changes and the surrounding shape already exists. Match the diff shape to the topic.

For a file-layout change:

```diff
 src/
 ├── main.rs
 ├── model.rs
+├── session.rs          # owns 3-pane state & token assembly
-└── discovery.rs        # mixed global OnceLock + worker
+└── discovery/
+    ├── mod.rs          # CliIntrospector seam
+    └── parser.rs       # --help grammar parsers
```

For a call-tree change:

```diff
 handle_terminal_event
   UserAction::from_key_event
   apply(action)
+    sync_dependent_columns
     refresh_filtered_views
```

For a state or control-flow change:

```diff
 on(Backspace)
   if active filter is non-empty
     pop last char
   else
-    always step left
+    pop last committed token or clear pending token first
```

- Show the whole Rust block when most of it is new, when omitted context would hide ownership or order, or when the user needs a copyable target shape:

```rust
pub fn active_parameter_sets(&self, tokens: &[String]) -> Option<HashSet<String>> {
    // ...
}
```

#### Guidance

Place each visual next to the short text it supports. Keep only the calls, files, types, states, and seams needed to answer the user's current question or resolve the current discussion point.

You may use one of these, you may use several, it is unlikely you will use all of them. Use your judgement and don't overwhelm the user.

### Evidence

Concrete evidence that the change works. Show a before and after.

Terminal frame captures or screenshots are S-tier when the change is visual (`ui.rs` layout/rendering).

Execution-based evidence is A-tier: exact `cargo test --locked`, `cargo clippy --all-targets -- -D warnings`, and `cargo fmt --all -- --check` output showing the test that failed before and passes after.

### Merge Danger

Describe whether it's a one-way or two-way door. You can walk back through two-way doors, but not one-way doors. A PR that is cheap to roll back is lower risk. Changes that break cache schemas (`cli_cache.json` / `ps_metadata.json`), shell wrapper contracts (`--out-file` / `--initial`), or cross-platform TTY behavior (`CONOUT$` / `/dev/tty`) have a wider blast radius than pure in-process refactors.

The blast radius is the potential impact or scope of the changes introduced by this PR (e.g., `Isolated`, `Session`, `Cross-platform`, `Packaging`).
