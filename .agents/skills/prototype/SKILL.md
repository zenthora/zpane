---
name: prototype
description: Build a throwaway prototype to answer a design question. Use when the user wants to sanity-check whether a state model or logic feels right, or explore what a UI/TUI layout should look like.
---

# Prototype

A prototype is **throwaway code that answers a question**. The question decides the shape.

## Pick a branch

Identify which question is being answered, using the user's prompt, the surrounding code, or by asking if the user is around:

- **"Does this logic / state model feel right?"** → [LOGIC.md](LOGIC.md). Build either a throwaway `examples/proto_<name>.rs` driver (`cargo run --example proto_<name>`) or, when a non-developer needs to click through it, a single shareable HTML file with free-play buttons and guided scenarios.
- **"What should this TUI / UI look like?"** → [UI.md](UI.md). Generate several radically different layout variations in one throwaway `examples/proto_<name>.rs` (or behind a prototype flag/mode), switchable via CLI arg, env var (`ZPANE_VARIANT`), or `←`/`→` key cycling.

The two branches produce very different artifacts, so getting this wrong wastes the whole prototype. If the question is genuinely ambiguous and the user isn't reachable, default to whichever branch better matches the surrounding code (state/parsing/discovery → logic; terminal rendering/layout → UI) and state the assumption at the top of the prototype.

## Rules that apply to both

1. **Throwaway from day one, and clearly marked as such.** Place Rust prototypes under `examples/proto_<name>.rs` (or a clearly named `proto_` module) so Cargo compiles them without touching `src/` binaries, and name them so a casual reader immediately sees they are prototypes, not production code.
2. **Trivial to run.** A single command: `cargo run --example proto_<name>` (or `cargo run --example proto_<name> -- --variant B`, or opening a single HTML file). No setup or explanation required.
3. **No persistence by default.** State lives in memory. If the question explicitly involves disk caches (`cli_cache.json` / `ps_metadata.json`), write to a temp path with a clear `proto-wipe-me` name.
4. **Skip the polish.** No tests, minimal error handling (`unwrap()`/`expect()` is fine in a prototype), no speculative abstractions. The point is to learn something fast.
5. **Surface the state.** After every action (logic) or on every variant switch (UI), print or render the full relevant state/frame so the user can see what changed.
6. **Capture it when done.** Fold any validated decision into the real `src/` code, then capture the prototype itself as a **primary source**: commit it to a throwaway `prototype/<name>` branch, out of main, and leave a context pointer to that branch on the implementation issue. Capture the answer too (the verdict and the question it settled) in the issue or commit. The main branch keeps only the validated decision.
