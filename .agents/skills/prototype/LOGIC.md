# Logic Prototype

A throwaway driver that lets you exercise a **state model, parser, or token-assembly rule** against concrete scenarios: the kind of thing that looks reasonable on paper but feels wrong once you push it through real sequences.

Pick the medium by who is driving it:
- **Developer / agent / terminal user** (default for Rust): `examples/proto_<name>.rs`, run with `cargo run --example proto_<name>`.
- **Non-developer stakeholder** (PM, designer): a single self-contained HTML/CSS/JS file (no framework, no bundler, no server) opened by double-click.

If the question is "what should this terminal layout look like?", this is the wrong branch. Use [UI.md](UI.md).

## Process

### 1. State the question

Before writing code, state the model and the exact question in a top-of-file doc comment (and print it at the top of the output). A logic prototype that answers the wrong question is waste, so make the question explicit.

### 2. Isolate the logic in a pure module

Put the prototype's core logic in a clean, pure Rust module (or self-contained `<script>` block for HTML) with zero TTY/disk/subprocess side effects, so the validated state machine or reducer can lift directly into `src/` later:

- **A pure transition function**: `fn apply(&mut self, action: Action) -> Outcome` when actions are discrete events.
- **An explicit enum state machine**: when "which transitions are legal in this state" is the question.
- **Small pure functions** over plain structs/slices: for parsers, fuzzy scoring, or token formatting.

### 3. Build the interactive scenarios

Include both:
1. **Named guided scenarios** covering the awkward cases: happy path, tricky edge case (e.g., `Tab` on a switch vs subcommand), and an invalid/boundary transition. Print each step, action, and full resulting state (`{:?}` or formatted table).
2. **Free-play input loop** (stdin line loop or key-step loop in Rust, or free-play buttons in HTML) so the user can poke at transitions in any order and watch the full state update after every step.

### 4. Hand it over

Show the single command to run (`cargo run --example proto_<name>`) or open the HTML file. Watch for moments like "wait, that token shouldn't appear yet" or "backspace stepped back too far"—those are bugs in the *idea*, which is the whole point.

### 5. Capture the answer and the prototype

Once the question is settled, lift the validated state/transition logic into `src/`, record the verdict, and archive the throwaway prototype on a `prototype/<name>` branch per [SKILL.md](SKILL.md).

## Anti-patterns

- **Don't add unit tests.** A prototype that needs tests is no longer a prototype.
- **Don't touch real caches or spawn host binaries.** Use in-memory synthetic `MillerItem` / `--help` fixtures unless persistence itself is the question.
- **Don't generalise.** Answer one question, not future hypotheticals.
- **Don't mix rendering/TTY raw mode with the logic** when testing state transitions. Keep the transition logic pure so it lifts cleanly into `src/`.
