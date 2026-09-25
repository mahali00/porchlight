import { expect, test } from "bun:test";
import { mkdtempSync, statSync } from "node:fs";
import * as Os from "node:os";
import * as Path from "node:path";
import { Array, Effect, Layer, Option } from "effect";
import { Auth } from "../src/auth.js";
import { fetchClientMetadata } from "../src/client-metadata.js";
import { pkceChallenge, sha256 } from "../src/crypto.js";
import { isDangerous } from "../src/policy.js";
import { Store } from "../src/store.js";

const run = <A, E>(program: Effect.Effect<A, E, Auth | Store>): Promise<A> =>
  Effect.runPromise(program.pipe(Effect.provide(Auth.layer.pipe(Layer.provideMerge(Store.layer(":memory:"))))));

const failure = <A, E extends { message: string }, R>(effect: Effect.Effect<A, E, R>) =>
  Effect.flip(effect).pipe(Effect.map((error) => error.message));

const begin = (requestId: string) =>
  Effect.gen(function* () {
    const auth = yield* Auth;
    return yield* auth.begin({
      id: requestId,
      clientId: "claude",
      server: "paper",
      redirectUri: "https://claude.ai/callback",
      challenge: pkceChallenge("verifier"),
      state: "",
    });
  });

const guessThenEnter = (requestId: string, wrongGuesses: number) =>
  Effect.gen(function* () {
    const auth = yield* Auth;
    const nonce = yield* begin(requestId);
    const code = yield* auth.issueDeviceCode(requestId);
    yield* Effect.forEach(Array.range(1, wrongGuesses), () =>
      Effect.ignore(auth.approveWithCode(requestId, "WRONG123", nonce)),
    );
    return yield* auth.approveWithCode(requestId, code, nonce);
  });

test("a device code still works after four wrong attempts", async () => {
  const approval = await run(guessThenEnter("typo", 4));
  expect(approval.redirectUri).toBe("https://claude.ai/callback");
});

test("a device code locks after five wrong attempts", async () => {
  expect(await run(failure(guessThenEnter("guess", 5)))).toBe("Too many attempts");
});

test("client metadata is never fetched from a host that resolves to a private address", async () => {
  let connections = 0;
  const listener = Bun.listen({ hostname: "::", port: 0, socket: { open: () => void connections++, data: () => {} } });
  const results = await Promise.all(
    ["localhost", "127.0.0.1", "[::1]", "[::ffff:127.0.0.1]"].map((host) =>
      Effect.runPromise(fetchClientMetadata(`https://${host}:${listener.port}/client.json`)),
    ),
  );
  await fetch(`https://127.0.0.1:${listener.port}/`, { signal: AbortSignal.timeout(200) }).catch(() => {});
  listener.stop(true);
  expect(results.every(Option.isNone)).toBe(true);
  expect(connections).toBe(1);
});

test("pending connection requests are capped", async () => {
  const last = await run(
    Effect.forEach(Array.range(1, 20), (index) => begin(`flood-${index}`)).pipe(
      Effect.andThen(failure(begin("one-too-many"))),
    ),
  );
  expect(last).toBe("Too many pending requests");
});

test("tools that run commands or code are flagged, ordinary tools that mention those words are not", () => {
  const flagged = [
    { name: "run_shell" },
    { name: "execute_command" },
    { name: "bash" },
    { name: "write_file" },
    { name: "query", description: "Runs shell commands on the host" },
    { name: "debugger-evaluate", description: "Execute arbitrary JavaScript in the app" },
    { name: "query", inputSchema: { type: "object", properties: { command: { type: "string" } } } },
  ];
  const ordinary = [
    { name: "gesture-swipe", description: "Execute a swipe gesture on the simulator" },
    { name: "native-describe-screen", description: "Describe the screen. Output is printed in the terminal" },
    { name: "boot-device", description: "Spawn a simulator and wait for it to boot" },
    { name: "flow-execute", description: "Replay a saved flow" },
    { name: "delete_nodes", description: "Delete nodes from the design" },
    { name: "write_html", inputSchema: { type: "object", properties: { html: {}, targetNodeId: {}, mode: {} } } },
    { name: "odd_schema", inputSchema: { properties: "not an object" } },
  ];
  expect(flagged.filter((tool) => !isDangerous(tool)).map((tool) => tool.name)).toEqual([]);
  expect(ordinary.filter(isDangerous).map((tool) => tool.name)).toEqual([]);
});

test("the state database and its WAL files are readable only by the owner", async () => {
  const previous = process.umask(0o022);
  const file = Path.join(mkdtempSync(Path.join(Os.tmpdir(), "porchlight-db-")), "state.db");
  await Effect.runPromise(
    Effect.flatMap(Store.asEffect(), (store) => store.audit.write("test")).pipe(Effect.provide(Store.layer(file))),
  );
  process.umask(previous);
  const modes = [file, `${file}-wal`, `${file}-shm`].map((path) => (statSync(path).mode & 0o777).toString(8));
  expect(modes).toEqual(["600", "600", "600"]);
});

const staleRefresh = (grantId: string, hash: string, expiresAt: number) =>
  Effect.flatMap(Store.asEffect(), (store) =>
    store.tokens.insert({ hash: sha256(hash), kind: "refresh", grantId, server: "paper", expiresAt }),
  );

test("an expired refresh token is refused without revoking the grant", async () => {
  const results = await run(
    Effect.gen(function* () {
      const auth = yield* Auth;
      const { token } = yield* auth.createStaticToken("paper", "client", 60_000);
      const grantId = (yield* auth.authorize(token, "paper")) ?? "";
      yield* staleRefresh(grantId, "expired", Date.now() - 1_000);
      const refused = yield* failure(auth.refresh("expired"));
      return { refused, stillAuthorized: (yield* auth.authorize(token, "paper")) === grantId };
    }),
  );
  expect(results).toEqual({ refused: "Refresh token expired", stillAuthorized: true });
});

test("pruning keeps used refresh tokens so a later reuse still revokes the grant", async () => {
  const results = await run(
    Effect.gen(function* () {
      const auth = yield* Auth;
      const store = yield* Store;
      const { token } = yield* auth.createStaticToken("paper", "client", 60_000);
      const grantId = (yield* auth.authorize(token, "paper")) ?? "";
      yield* staleRefresh(grantId, "used", Date.now() + 60_000);
      yield* auth.refresh("used");
      yield* store.tokens.prune(Date.now());
      const reuse = yield* failure(auth.refresh("used"));
      return { reuse, stillAuthorized: (yield* auth.authorize(token, "paper")) !== undefined };
    }),
  );
  expect(results).toEqual({ reuse: "Refresh token reused; grant revoked", stillAuthorized: false });
});
