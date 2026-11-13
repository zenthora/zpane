# UI / TUI Prototype

Generate **several radically different terminal layout variations** in one runnable Rust prototype, switchable via CLI arg (`--variant A|B|C`), env var (`ZPANE_VARIANT`), or interactive `←`/`→` key cycling. The user flips between variants in the terminal, picks one (or steals pieces from each), then throws the rest away.

If the question is about state transitions or token logic rather than terminal presentation, this is the wrong branch. Use [LOGIC.md](LOGIC.md).

## When this is the right shape

- "What should Column 3's inspector header look like on narrow terminals?"
- "How should parameter-set exclusivity or active filters be surfaced in the chrome?"
- "Try three different layouts for small split-pane viewports (`80x16`)."

## Two sub-shapes: prefer sub-shape A

A TUI prototype is much easier to judge when it renders **against real `zpane` frame geometry and ANSI colors** rather than dummy ASCII boxes.

### Sub-shape A: variant switch on a real frame harness (preferred)

Feed realistic `MillerItem` catalogs and `Viewport` sizes (`120x30`, `80x24`, `70x14`) into a prototype harness that shares the real data model and alternates only the rendering functions (`render_variant_a`, `render_variant_b`, `render_variant_c`).

### Sub-shape B: standalone layout sketch (last resort)

Only when prototyping an entirely new TUI surface with no existing frame structure to sit inside. Keep it in `examples/proto_<name>.rs`.

## Process

### 1. State the question and pick N

Default to **3 variants** (`A`, `B`, `C`; cap at 5). Write the question in one line at the top of `examples/proto_<name>.rs`:

> "Three variants of the narrow-terminal (<16 rows) inspector + status footer, switchable via `--variant` or `←`/`→`."

### 2. Generate radically different variants

Hold every variant to:
- Real `Viewport` constraints (width/height clamping, unicode display width via `unicode-width`, ANSI reset discipline)
- Existing `zpane` item kinds and metadata (commands, subcommands, params, switches, badges, descriptions)
- A distinct function name: `render_variant_a`, `render_variant_b`, `render_variant_c`

Variants must be **structurally different**: different pane proportions, different information hierarchy, different placement of inspector/filter/command-bar chrome—not just tweaked ANSI color codes.

### 3. Build the variant switcher

In `examples/proto_<name>.rs`, support:
- **Snapshot mode**: `cargo run --example proto_<name> -- --all --width 80 --height 20` prints each variant framed and labeled sequentially so differences can be compared side-by-side or captured in a terminal log.
- **Interactive mode**: `cargo run --example proto_<name>` enters alternate screen / raw mode with a bright bottom switcher pill (`[←] Variant B: Compact Inspector [→]  [w/h presets: 1/2/3]  [q] Quit`) where `←`/`→` cycles variants and `1`/`2`/`3` switches viewport presets (`120x30`, `80x24`, `60x14`).

Keep prototype code strictly in `examples/proto_<name>.rs` (never compiled into the `zpane` release binary).

### 4. Hand it over

Tell the user the exact `cargo run --example proto_<name>` command. The best feedback sounds like *"I want the compact footer from B with the inline inspector from C."*

### 5. Capture the answer and clean up

Once a design wins:
- Implement the chosen layout properly in `src/ui.rs` (with viewport geometry tests).
- Commit the throwaway `examples/proto_<name>.rs` to a `prototype/<name>` branch and remove it from `main` so dead variant renderers don't rot in the repo.

## Anti-patterns

- **Variants that differ only in ANSI colors.** Real variants disagree about layout and hierarchy.
- **Sharing a rigid layout wrapper.** Let each variant rearrange columns, headers, and footers freely.
- **Promoting prototype rendering directly to `src/ui.rs`.** Prototypes skip resize edge cases, zero-width clamping, and unicode width guards; rewrite cleanly when folding into `src/ui.rs`.
