---
name: implement
description: "Implement a piece of work based on a spec or set of tickets."
disable-model-invocation: true
---

Implement the work described by the user in the spec or tickets.

If the user passes a ticket reference, fetch it from the issue tracker and state its title before starting. If the reference is ambiguous, ask.

Call the Skill tool with "tdd" where possible, at pre-agreed seams.

Run `cargo check --all-targets` (or `cargo clippy --all-targets -- -D warnings`) and targeted `cargo test <name>` regularly while iterating. Before finishing, run `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --locked`.

Once done, call the Skill tool with "code-review" to review the work.

Commit your work to the current branch using Conventional Commits (matching `prek.toml` / `cliff.toml` if configured).
