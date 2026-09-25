# Contributing

```bash
bun install
bun run check
```

`check` runs the typecheck, Prettier and the tests. CI runs the same command.

## Code style

- TypeScript on Bun. Use Bun's own APIs, like `Bun.serve` routes, `bun:sqlite` and `Bun.$`.
- Effect v4 beta. Follow the patterns already in `src/`: `Effect.fn("Module.name")`, `Schema.TaggedErrorClass`
  errors, `ServiceMap.Service` with a `Layer`, and `Schema` to decode anything from outside.
- No comments, type casts or non-null assertions.
- Every error maps to an exit code in `src/index.ts`.
- `bun run format` before committing.
- After changing settings in `src/config.ts`, run `bun run schema` to update `porchlight.schema.json`.

## Tests

Add tests for security guarantees: approval, tokens, tool policy and the listeners. Skip tests for helpers
and formatting.

## Adding a known server

Add an entry to `knownServers` in `src/discovery.ts` with its name and local MCP URL. Check that it isn't
flagged by `isDangerous` in `src/policy.ts`, or explain why it should be.

## Ground rules

- Never add a flag, environment variable or JSON field that approves access.
- Never write to a user's MCP config files.
- No telemetry.
