# Good and Bad Tests

## Good Tests

**Integration-style**: Test through real public interfaces, not mocks of internal parts.

```rust
// GOOD: Tests observable behavior through the session seam
#[test]
fn typing_custom_value_in_col3_emits_confirmed_command() {
    let mut git = MillerItem::new("git", "git", ItemKind::Cli, "Git VCS");
    git.children = vec![MillerItem::new("clone", "clone", ItemKind::Subcmd, "Clone repo")];

    let introspector = CliIntrospector::with_initial_cache(ShellDialect::Posix, Default::default());
    let mut session = MillerSession::new(vec![git], introspector, "", Viewport::new(120, 30), ShellDialect::Posix);

    session.apply(UserAction::StepRight);
    session.apply(UserAction::StepRight);
    for ch in "https://example.com/repo".chars() {
        session.apply(UserAction::InsertChar(ch));
    }

    assert_eq!(session.preview_command(), "git clone https://example.com/repo");
    assert_eq!(
        session.apply(UserAction::Confirm),
        SessionOutcome::Confirmed("git clone https://example.com/repo".to_string())
    );
}
```

Characteristics:

- Tests behavior callers care about
- Uses public `pub` / crate interface only
- Survives internal refactors
- Describes WHAT, not HOW
- One logical assertion per test

## Bad Tests

**Implementation-detail tests**: Coupled to internal structure or private fields.

```rust
// BAD: Inspects internal dirty flags or private cache keys
#[test]
fn stepping_right_sets_col2_cache_dirty() {
    let mut session = make_session();
    session.apply(UserAction::StepRight);
    assert!(session.col2_filter_dirty);
    assert_eq!(session.col2_cache_state.unwrap().c1_item_idx, Some(0));
}
```

Red flags:

- Mocking internal collaborators
- Asserting on private fields or cache keys instead of observable outputs
- Asserting on call counts/order of internal helpers
- Test breaks when refactoring without behavior change
- Test name describes HOW not WHAT
- Verifying through a side channel (e.g., reading a cache file directly instead of querying through the interface)

```rust
// BAD: Bypasses the interface to check internal storage
#[test]
fn warm_writes_cache_file() {
    introspector.warm_priority_tools(&items);
    let raw = std::fs::read_to_string(cli_cache_path()).unwrap();
    assert!(raw.contains("git::commit"));
}

// GOOD: Verifies observable cache hit through the public interface
#[test]
fn query_returns_warmed_items_without_re_spawning() {
    let items = introspector.query(&["git", "commit"]);
    assert!(items.iter().any(|i| i.text == "--message"));
}
```

**Tautological tests**: Expected value restates the implementation, so the test passes by construction.

```rust
// BAD: Expected width is recomputed the same way the function computes it
#[test]
fn column_rows_matches_height_minus_chrome() {
    let vp = Viewport::new(120, 30);
    assert_eq!(vp.rows(Column::Third), 30 - 6 - 3);
}

// GOOD: Expected value is an independent, worked literal
#[test]
fn col3_rows_accounts_for_chrome_and_inspector_header() {
    assert_eq!(Viewport::new(120, 30).rows(Column::Third), 21);
    assert_eq!(Viewport::new(120, 8).rows(Column::Third), 0);
}
```
