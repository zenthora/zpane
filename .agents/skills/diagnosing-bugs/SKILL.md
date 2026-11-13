---
name: diagnosing-bugs
description: Diagnosis loop for hard bugs and performance regressions. Use when the user says "diagnose"/"debug this", or reports something broken/throwing/failing/slow.
---

# Diagnosing Bugs

A discipline for hard bugs. Skip phases only when explicitly justified.

When exploring the codebase, read `GLOSSARY.md` (if it exists) to get a clear mental model of the relevant modules, and check ADRs in the area you're touching.

## Redact

This skill has you show commands, outputs and captured artifacts. **Redact every secret first**: write `<REDACTED>` in its place. Build loops against env vars, so the credential stays in the environment rather than in what you show. Captured artifacts carry tokens or host paths: quote only the lines that carry the signal.

If the redacted output is not enough to diagnose the bug, say so and ask the user.

## Phase 1: Build a feedback loop

**This is the skill.** Everything else is mechanical. If you have a **tight** pass/fail signal for the bug (one that goes red on _this_ bug), you will find the cause; bisection, hypothesis-testing, and instrumentation all just consume it. If you don't have one, no amount of staring at code will save you.

Spend disproportionate effort here. **Be aggressive. Be creative. Refuse to give up.**

### Ways to construct one, in roughly this order

1. **Failing Rust test** (`cargo test <name>`) at whatever seam reaches the bug: `MillerSession` + `UserAction`, `ui::render_frame`, `CliIntrospector` with an in-memory `CommandRunner`, `parse_cli_help`, or `tests/*.rs` integration tests.
2. **CLI invocation** with deterministic flags/env (`cargo run -- --width 80 --height 24 ...`, `ZPANE_SHELL=pwsh|posix|fish`, `--help`/`--version`), diffing stdout/stderr against expected output.
3. **Replay a captured trace.** Save a real `--help` text dump, `ps_metadata.json` / `cli_cache.json` fixture, key-event sequence, or terminal size to a fixture and replay it through `MillerSession` or `parse_cli_help` in isolation.
4. **Throwaway `examples/` harness** (`cargo run --example proto_<name>`) that exercises the buggy subsystem with a minimal synthetic dataset.
5. **Property / stress / timing loop.** If the bug is "sometimes wrong output" or "interactive stutter", run hundreds of deterministic synthetic inputs or measure median step latency with `std::time::Instant` (see `test_tui_stutter_feedback_loop`).
6. **Bisection harness.** If the bug appeared between two known commits, automate "build and run the targeted test" so `git bisect run cargo test <name>` pinpoints the regression.
7. **Differential loop.** Run the same input through two `ShellDialect`s, two `Viewport` sizes, or old-vs-new commits and diff the rendered frames or emitted tokens.
8. **HITL bash script.** Last resort. If a real interactive TTY (`CONOUT$` / `/dev/tty` / specific terminal emulator / `PSReadLine` or `zle` binding) must be driven by a human, guide them with `scripts/hitl-loop.template.sh` so observations are structured.

Build the right feedback loop, and the bug is 90% fixed.

### Tighten the loop

Treat the loop as a product. Once you have _a_ loop, **tighten** it:

- Can I make it faster? (Use `cargo test <single_test_name>` instead of the whole crate; use synthetic in-memory fixtures instead of spawning `pwsh` or scanning `PATH`.)
- Can I make the signal sharper? (Assert on the exact rendered row, preview command, or ANSI sequence rather than "didn't panic".)
- Can I make it more deterministic? (Pass explicit `ShellDialect` and `Viewport`; inject `CommandRunner`; isolate cache paths with temp dirs; avoid global env mutation across parallel tests.)

A 30-second flaky loop is barely better than no loop; a sub-second deterministic `cargo test` is tight, a debugging superpower.

### Non-deterministic bugs

The goal is not a clean repro but a **higher reproduction rate**. Loop the trigger 100×, vary concurrency (`rayon` / background introspector thread timing), add deterministic stress, or narrow timing windows. A 50%-flake bug is debuggable; 1% is not, so keep raising the rate until it's debuggable.

### When you genuinely cannot build a loop

Stop and say so explicitly. List what you tried. Ask the user for: (a) access to the target shell/OS/terminal environment, (b) a redacted captured artifact (raw `--help` output, terminal dimensions, cache JSON, key sequence, or crash backtrace with `RUST_BACKTRACE=1`), or (c) permission to add temporary instrumentation. Do **not** proceed to hypothesise without a loop.

### Completion criterion: a tight loop that goes red

Phase 1 is done when the loop is **tight** and **red-capable**: you can name **one command** (e.g. `cargo test <test_name>` or a script path) that you have **already run at least once** (show the invocation and its output, redacted), and that is:

- [ ] **Red-capable**: it drives the actual bug code path and asserts the **user's exact symptom**, so it can go red on this bug and green once fixed. Not "compiles without erroring"; it must be able to _catch this specific bug_.
- [ ] **Deterministic**: same verdict every run (for race/timing bugs: a pinned, high reproduction rate, per above).
- [ ] **Fast**: seconds, not minutes.
- [ ] **Agent-runnable**: you can run it unattended; a human in the loop only via `scripts/hitl-loop.template.sh`.

If you catch yourself reading code to build a theory before this command exists, **stop: jumping straight to a hypothesis is the exact failure this skill prevents.** No red-capable command, no Phase 2.

## Phase 2: Reproduce + minimise

Run the loop. Watch it go red as the bug appears.

Confirm:

- [ ] The loop produces the failure mode the **user** described, not a different failure that happens to be nearby. Wrong bug = wrong fix.
- [ ] The failure is reproducible across multiple runs (or, for non-deterministic bugs, reproducible at a high enough rate to debug against).
- [ ] You have captured the exact symptom (panic message, wrong command string, clipped ANSI row, slow timing) so later phases can verify the fix actually addresses it.

### Minimise

Once it's red, shrink the repro to the **smallest scenario that still goes red**. Cut `MillerItem` fixtures, tokens, `UserAction` steps, and config **one at a time**, re-running the loop after each cut, and keep only what's load-bearing for the failure.

Why bother: a minimal repro shrinks the hypothesis space in Phase 3 (fewer moving parts left to suspect) and becomes the clean regression test in Phase 5.

Done when **every remaining element is load-bearing**: removing any one of them makes the loop go green.

Do not proceed until you have reproduced **and** minimised.

## Phase 3: Hypothesise

Generate **3–5 ranked hypotheses** before testing any of them. Single-hypothesis generation anchors on the first plausible idea.

Each hypothesis must be **falsifiable**: state the prediction it makes.

> Format: "If <X> is the cause, then <changing Y> will make the bug disappear / <changing Z> will make it worse."

If you cannot state the prediction, the hypothesis is a vibe: discard or sharpen it.

**Show the ranked list to the user before testing.** They often have domain knowledge that re-ranks instantly ("we just changed Column 2 preview suppression"), or know hypotheses they've already ruled out. Cheap checkpoint, big time saver. Don't block on it; proceed with your ranking if the user is AFK.

## Phase 4: Instrument

Each probe must map to a specific prediction from Phase 3. **Change one variable at a time.**

Tool preference:

1. **`dbg!()` / targeted `eprintln!` or debugger** at the seams that distinguish hypotheses.
2. Never "log everything and grep".

**Tag every debug log** with a unique prefix, e.g. `eprintln!("[DEBUG-a4f2] ...")`. Cleanup at the end becomes a single `rg` / `grep` (and `clippy` flags stray `dbg!()`). Untagged logs survive; tagged logs die.

**Perf branch.** For performance regressions, logs perturb timings and lie. Instead: establish a baseline measurement (`std::time::Instant` median across runs, `cargo test --release <bench_test> -- --nocapture`, or `criterion` / flamegraph), then bisect. Measure first, fix second.

## Phase 5: Fix + regression test

Write the regression test **before the fix**, but only if there is a **correct seam** for it.

A correct seam is one where the test exercises the **real bug pattern** as it occurs at the call site. If the only available seam is too shallow (testing a private helper when the bug arises from how `MillerSession` and `ui::render_frame` interact), a regression test there gives false confidence.

**If no correct seam exists, that itself is the finding.** Note it. The codebase architecture is preventing the bug from being locked down. Flag this for the next phase.

If a correct seam exists:

1. Turn the minimised repro into a failing `#[test]` at that seam.
2. Watch it fail. If you forced the red by mutating code or a fixture, `git diff` against a pristine copy to prove the mutation landed before you trust it.
3. Apply the fix.
4. Watch it pass (`cargo test <name>`).
5. Re-run the Phase 1 feedback loop against the original (un-minimised) scenario, plus `cargo clippy --all-targets -- -D warnings && cargo test --locked`.

## Phase 6: Cleanup

Required before declaring done:

- [ ] Original repro no longer reproduces (re-run the Phase 1 loop)
- [ ] Regression test passes and `cargo clippy --all-targets -- -D warnings && cargo test --locked` is green
- [ ] All `[DEBUG-...]` instrumentation and `dbg!()` calls removed (`rg "DEBUG-"` returns nothing)
- [ ] Throwaway `examples/` or scratch harnesses deleted (or moved to a clearly-marked debug branch)
- [ ] The hypothesis that turned out correct is stated in the commit / PR message, so the next debugger learns
