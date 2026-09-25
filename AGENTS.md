# AGENTS.md

porchlight exposes local MCP servers to cloud MCP clients through a tunnel. It is a **Bun** CLI, not a Node app.

## Commands

- `bun install` — required before anything (lockfile pinned via `bunfig.toml` `exact = true`).
- `bun run check` — the full gate: `tsc --noEmit` → `prettier --check src tests` → `bun test`. CI runs exactly this.
- `bun test` — all tests. Single file: `bun test tests/security.test.ts`. Single test: `bun test -t "name"`.
- `bun run format` — Prettier over `src tests` only (not `script/`).
- `bun run schema` — regenerate `porchlight.schema.json`. **Run this after any change to the setting schemas in `src/config.ts`**; the file is committed and CI does not check for drift.
- `bun run build` — typecheck then compile a standalone binary to `dist/`. Publishing uses `bin: src/index.ts` directly; the binary is not the release artifact.
- Run the CLI from source with `bun src/index.ts ...` (or `bun start`).

## Toolchain constraints

- Bun APIs are the house style: `Bun.serve`, `Bun.$`, `bun:sqlite`, `Bun.file`. Don't reach for Node equivalents or add frameworks.
- **Effect v4 beta is pinned to exact `4.0.0-beta.42`** with an `@effect/platform-node-shared` override. Do not bump it. v4 APIs differ from v3: `Effect.fn("Module.name")`, `Schema.TaggedErrorClass`, `ServiceMap.Service` + `Layer`, `Schema` to decode all external input, CLI from `effect/unstable/cli`.
- TypeScript is strict with `noUnusedLocals`, `isolatedModules` and `verbatimModuleSyntax` (use `import type`). Relative imports use the `.js` extension.
- No comments, no type casts, no non-null assertions (see `CONTRIBUTING.md` for the full style list).

## Architecture

- `src/index.ts` is the runtime entrypoint: it defines the Effect CLI, wires `Auth.layer` over `Store.layer`, and maps every error to an exit code. Add new errors to `src/errors.ts` and handle them in the `Effect.catchTags` block at the bottom of `index.ts`.
- One daemon (`src/serve.ts`) owns the tunnel, the listeners and a `Registry` (`src/registry.ts`) of exposed servers. Each registry entry has a supervisor fiber that retries until the upstream is reachable ("waiting" → "live"). The CLI talks to a running daemon through a local Unix socket (`src/control.ts`) instead of starting a second copy.
- Every kind of server implements `Upstream` (`src/upstream/types.ts`): `http.ts` proxies Streamable HTTP, `stdio.ts` bridges stdio servers with one process per session using the runner in `process.ts` (argument list, no shell, minimal env, logs, cleanup). `src/servers.ts` turns a name, URL or config entry into a `ServerSpec` for both the CLI and the daemon.
- Two loopback listeners: the gateway (`src/gateway.ts`) serves OAuth + proxies `/<server>/mcp` and is the only port the tunnel forwards to; the approval page (`src/approval.ts`) is on a second loopback port the tunnel never reaches.
- Client tokens are validated at the gateway and never forwarded upstream. Tool filters (`allow`/`deny`/`readOnly`, `src/policy.ts`) apply to both `tools/list` and `tools/call`.
- State lives in `~/.porchlight/state.db` (SQLite, WAL); settings in `~/.porchlight/porchlight.json`. `PORCHLIGHT_HOME` moves the whole folder; tests set it to a temp dir in `tests/setup.ts` and use `Store.layer(":memory:")`.

## Changing things

- **Add a known server**: add its name + local MCP URL to `knownServers` in `src/discovery.ts`, then verify `isDangerous` in `src/policy.ts` doesn't flag it (or justify why it should).
- **Tests target security guarantees only** — approval, tokens, tool policy, listeners. Don't add tests for helpers or formatting. Integration tests bind real loopback ports and start an upstream `Bun.serve`; keep new ones self-contained the same way.

## Ground rules (do not violate)

- Never add a flag, environment variable, or JSON field that approves access.
- Never write to a user's MCP config files.
- No telemetry.
