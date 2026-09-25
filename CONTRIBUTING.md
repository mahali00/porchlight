# Contributing

```bash
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

CI runs the same three commands on macOS and Linux. `cargo run` and `cargo test` keep their state in
`target/dev-home`, so they never touch your real `~/.porchlight`.

## Code style

- Rust 2024. Clippy runs pedantic, and `unwrap`, `expect`, `panic`, indexing, `as` casts and string slicing are
  denied outside tests. Return an error instead.
- No comments. If a line needs one, rename something.
- Every error ends up as an `ExitError` with a code from `src/exit.rs`.
- The terminal app and the CLI change things through `src/actions.rs` and read through `src/snapshot.rs`. Add new
  behavior there first, then wire it into both.
- When you add a setting in `src/config.rs`, add it to `porchlight.schema.json` too. A test checks they match.

## Tests

Add tests for security guarantees: approval, tokens, tool policy, command inputs and the listeners. Skip tests for
helpers and formatting.

## Adding a known server

Add an entry to `KNOWN_SERVERS` in `src/discovery.rs` with its name and local MCP URL. Check that it isn't flagged by
`is_dangerous` in `src/policy.rs`, or explain why it should be.

## Ground rules

- Never add a flag, environment variable or JSON field that approves access.
- Never write to a user's MCP config files.
- No telemetry.
