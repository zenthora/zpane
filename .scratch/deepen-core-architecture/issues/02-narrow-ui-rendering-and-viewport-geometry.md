# 02: Narrow UI Frame Seam and Authoritative Viewport Geometry

**What to build:** Move `ItemKind::ansi_color` out of `src/model.rs` into `src/ui.rs`, establish a single authoritative viewport row-count calculation in `src/ui.rs` shared by scroll clamping, `PageUp`/`PageDown` paging, and `render_frame` (including small terminals with `term_h < 16` and Column 3's 3-row inspector header), and narrow `RenderState` so `src/ui.rs` derives the Column 3 inspector header and bottom info bar directly from the selected `MillerItem`.

**Blocked by:** None (can start immediately)

**Status:** resolved

- [x] `ItemKind::ansi_color` is moved from `src/model.rs` to `src/ui.rs`.
- [x] `src/ui.rs` exposes a single viewport row-count helper (`viewport_rows(term_h: usize, col: usize) -> usize`) used by scroll clamping, `PageUp`/`PageDown` navigation, and `render_frame` so `main.rs` and `ui.rs` never disagree when `term_h < 16` or in Column 3.
- [x] `RenderState` drops pre-formatted `info_title` and `info_desc` fields; `src/ui.rs` derives the inspector header and bottom info bar directly from the active column's selected `MillerItem`.
- [x] Unit tests in `src/ui.rs` verify viewport row counts, small-terminal (`term_h < 16`) scroll alignment, and inspector header derivation.
