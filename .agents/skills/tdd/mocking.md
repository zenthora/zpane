# When to Mock

Mock or stub at **system boundaries** only:

- External processes / host binaries (`pwsh`, `git`, CLI `--help` subprocesses)
- Controlling terminals / TTY I/O (`CONOUT$`, `/dev/tty`, raw terminal events)
- Clock / time / randomness
- Filesystem / persistent caches (prefer `tempfile` / isolated temp paths over mocking `std::fs`)

Don't mock:

- Your own structs, enums, or modules
- Internal collaborators
- Pure in-process state machines or parsers

## Designing for Substitutability

At system boundaries, design Rust interfaces that accept adapters cleanly:

**1. Inject dependencies, don't read globals or spawn processes inline**

Pass OS/process adapters or explicit values (`ShellDialect`, `Viewport`, `CommandRunner`) into constructors instead of reading `env::var`, `OnceLock` singletons, or calling `std::process::Command::new` directly inside domain logic:

```rust
// Easy to test: accepts a CommandRunner adapter at the boundary
pub type CommandRunner = dyn Fn(&str, &[&str]) -> Option<String> + Send + Sync;

pub fn introspect_cli_chain_with<F>(chain: &[&str], runner: &F) -> Vec<MillerItem>
where
    F: Fn(&str, &[&str]) -> Option<String>,
{
    ...
}

// Hard to test: hardcodes std::process::Command inside the parser/resolver
pub fn introspect_cli_chain(chain: &[&str]) -> Vec<MillerItem> {
    let out = std::process::Command::new(chain[0]).arg("--help").output();
    ...
}
```

**2. Prefer focused trait methods over generic string dispatchers**

When a boundary has distinct operations, define typed methods on a trait instead of one generic `call(op, payload)` function that forces conditional branching inside every test fake:

```rust
// GOOD: Each boundary operation has its own signature and return type
pub trait MetadataStore {
    fn load_cli_cache(&self) -> HashMap<String, Vec<MillerItem>>;
    fn save_cli_cache(&self, cache: &HashMap<String, Vec<MillerItem>>) -> std::io::Result<()>;
}

// BAD: Generic string dispatcher forces match-arms in every fake
pub trait Store {
    fn execute(&self, action: &str, json_payload: &str) -> std::io::Result<String>;
}
```

Focused traits mean:
- Each fake method returns one concrete Rust type
- No string matching or conditional dispatch in test setup
- Compiler enforces the contract at every test double
- Prefer hand-written in-memory fakes in `#[cfg(test)]` over macro mocking crates unless verifying complex call protocols at a true external boundary
