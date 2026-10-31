# AGENTS.md

porchlight shares MCP servers (HTTP or stdio) and plain command-line programs, wrapped as MCP tools, with remote MCP clients through a tunnel. porchlight is itself an MCP server. It is a single Rust binary: bare `porchlight` opens a full-screen terminal app, and every action there is also a subcommand.

## Commands

- `cargo fmt --check`, `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked` are the gate. CI runs exactly these on macOS and Linux.
- One test file: `cargo test --test security`. One test: `cargo test name_fragment`.
- `cargo run -- <args>` runs the CLI from source. `.cargo/config.toml` sets `PORCHLIGHT_HOME` to `target/dev-home`, so `cargo run` and `cargo test` never touch the real `~/.porchlight`. Anything run outside cargo must set `PORCHLIGHT_HOME` to a temp dir for the same reason.
- `cargo build --release` builds the binary. To release, bump `version` in `Cargo.toml`, then push the tag `v<version>`. `.github/workflows/release.yml` refuses a tag that doesn't match `Cargo.toml`, then builds four targets and publishes them with `install.sh`. `Cargo.toml` is the only place the version lives; code uses `core::VERSION`.

## Code rules

- Clippy is pedantic, and `unwrap`, `expect`, `panic`, indexing, `as` casts and string slicing are denied (`Cargo.toml` `[lints]`). Tests may use `unwrap` and friends (`clippy.toml`). `unsafe` is forbidden.
- No comments. Names carry the meaning.
- Every error becomes an `ExitError` with a code from `src/exit.rs`. CLI output goes through the `out!` macro in `src/lib.rs`, which exits quietly when the reader closes the pipe.
- `rustfmt.toml` sets the width to 120.

## Architecture

The primitives, the layers and the rules behind them are in `docs/design/primitives.md`. In short: each fact about a tool has one owner, dependencies only point down a layer, and operations return data that the CLI, TUI and pages put into words. `tests/layers.rs` fails if a module uses one in a higher layer.

- `src/main.rs` parses the clap derive command tree. No subcommand opens `tui::run`; otherwise `cli::run` handles it. `serve` is hidden and is what the background service runs.
- `porchlight.json` is the source of command tools, MCP servers and links. `src/config.rs` decodes it one entry at a time so a broken entry becomes a `Problem` instead of failing the file, and edits it in place through the `jsonc-parser` CST so comments and key order survive. Every write and every load leaves the file at 0600.
- `src/spec.rs` `CommandSpec` is one command tool as declared, with its group's settings inherited. It is decoded from `Settings` in one place (`spec::lookup`, `spec::members`) and owns the tool's template, placeholders, risk and JSON form. Nothing else parses a command.
- `src/apps.rs` `apps_from` turns `Settings` into `AppSpec`s, link plans, tool `Rules` and problems. Broken entries appear in status and the app; they are not fatal to the file.
- The daemon is `daemon::start` in `src/daemon.rs`. It binds the gateway and approval listeners on 127.0.0.1, opens the tunnel (`Daemon` in `src/serve.rs`), starts the `Reloader` (which re-reads `porchlight.json` every 250 ms, revokes grants for links whose membership changed and writes a `config.changed` audit row), the upkeep sweeps and the control socket.
- Four module seams, each an enum so nothing else branches on a name:
  - `src/sources.rs`: `SourceSpec`/`Source` for command apps (`sources/commands.rs`), HTTP MCP servers (`sources/mcp_http.rs`) and stdio MCP servers (`sources/mcp_stdio.rs`, processes in `sources/process.rs`). Each binds connect, list, call, risk and resources.
  - `src/links.rs`: `LinkPlan::Direct` (one MCP server, its own `/<app>/mcp`) and `LinkPlan::Commands` (command apps composed into a named link). A plan has a membership signature and projects physical apps into a `LinkEntry`.
  - `src/surfaces.rs`: `McpSurface` serves a `LinkEntry` over MCP. It allowlists methods, refuses tools that are off, filters `tools/list` and `resources/list`, seals session ids to the grant and records every call.
  - `src/tunnels.rs`: `Provider` (OpenTunnel today, `tunnels/opentunnel.rs`) or the user's own https URL.
- `src/registry.rs` supervises physical apps: each has a task that retries every 3 s until the app answers, then re-checks HTTP servers every 5 s. States are `starting`, `waiting`, `live` and `refused` (a tool can run code and `allowDangerous` is off). `src/policy.rs` holds `is_dangerous`, the `app/tool` rule matching and `Risk`: what a client could make a tool do, with an ordered `Level` (`read-only` < `changes` < `client-chooses` < `runs-commands`). Each source computes `Risk` for its own tools (`SourceSpec::risk`): command tools from their `CommandSpec`, MCP tools from what the server announces. Everything else reads it.
- Two loopback listeners. The gateway (`src/gateway.rs`) serves OAuth and `/<link>/mcp` and is the only port the tunnel forwards. The approval listener (`src/approval.rs`, HTML in `src/pages.rs`) serves the code view (the only place a waiting client's code appears besides the TUI, CLI and notification), Deny and the local home page; the tunnel never reaches it, and every POST needs the exact Host, a same-origin request and a CSRF token.
- `src/auth.rs` owns approval, codes, tokens and grants. `src/store.rs` is SQLite (WAL, 0600) at `~/.porchlight/state.db`. `src/client_metadata.rs` fetches client metadata documents with public-address pinning and no redirects.
- `src/control.rs` is the Unix socket the CLI and app use to talk to a running daemon: `/state`, `/reload`, `/tunnel` and `/try` (read-only MCP tools only).
- The CLI is `src/cli.rs` (the dispatcher and shared helpers) with one file per command group in `src/cli/`; its commands are `serve`, `status`, `logs`, `start`, `stop`, `tools`, `servers`, `links`, `approve`/`deny`, `clients`, `tokens`, `tunnel` and `completions`. The TUI is `src/tui/` (`rows` for what it shows, `keys` for input, `act` for changes, `answer` for dialogs, `view` for drawing) and uses the same user-facing vocabulary: Tools, Links, Clients, Logs and Tunnel. Both read through `src/snapshot.rs` and change things through `src/actions.rs`, which returns outcomes such as `Shared` rather than sentences; `src/explain.rs` holds the wording both share. The running daemon reports itself as `DaemonState` in `src/state.rs`. `src/system.rs` installs the launchd or systemd login service and shows notifications. `src/discovery.rs` finds MCP servers in Claude, Cursor and VS Code settings without writing to them.
- Client tokens are checked at the gateway and never forwarded upstream. Child processes get only `PATH`, `HOME`, `USER`, `LOGNAME`, `LANG`, `LC_ALL` and `TMPDIR` plus their own `environment`. Commands run without a shell, and client input is always one argument.

## Changing things

- **Add a setting.** Add the field to the structs in `src/config.rs` and its key to the matching `*_KEYS` list, read it in `apps_from` or wherever it applies, and add it to `porchlight.schema.json` by hand. `the_committed_schema_lists_every_setting` fails until the schema and the key lists agree.
- **Add a CLI command.** Add a clap-derived command and 1–3 concrete `after_help` examples in `src/cli.rs`; route all stdout through `out!`/`say`, return `ExitError` for failures, and make success machine-readable with `--json`. Put shared changes in `src/actions.rs`, reload after configuration writes, and call `needs_person` before approving or creating a token. Update README and `skills/porchlight/SKILL.md`.
- **Add a source kind.** Write the transport in `src/sources/`, add a variant to `SourceSpec` and `Source`, decode its settings in `src/config.rs`, build it in `apps_from`, and compute `Risk` for its tools in `SourceSpec::risk`. Registry, surfaces and the CLI must not need to know its name.
- **Add a module.** Put it in a layer in `tests/layers.rs` and in `docs/design/primitives.md`. It may use only its own layer and lower ones.
- **Add a link composer.** Add a `LinkPlan` variant with a stable signature and a projector in `src/links.rs`, and compile it in `apps_from`. A projector never reads grants or tokens.
- **Add a tunnel provider.** Add a `Provider` variant in `src/tunnels.rs` with `open`, `installed`, `install` and `warnings`.
- **Add a known server.** Add it to `KNOWN_SERVERS` in `src/discovery.rs` and check its tools against `is_dangerous`.
- **Tests cover security guarantees.** `tests/config.rs` covers what `porchlight.json` can and can't do to grants, `tests/security.rs` covers approval, tokens, tool policy and command inputs, `tests/servers.rs` is the integration suite: real loopback listeners, an upstream server and the stdio fixture in `tests/fixtures/`, and `tests/layers.rs` keeps the layering true. Don't add tests for helpers or formatting.

## Ground rules (do not violate)

- Never add a flag, environment variable, or JSON field that approves access.
- Never write to a user's MCP config files.
- No telemetry.
