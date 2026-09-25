import { Effect, Option, Schedule, Schema } from "effect";
import { startApproval } from "./approval.js";
import { loadSettings } from "./config.js";
import { startControl, type AddResult, type DaemonState, type RemoveResult, type ServerView } from "./control.js";
import { discover } from "./discovery.js";
import { startGateway } from "./gateway.js";
import { makeRegistry, type Entry, type Registry } from "./registry.js";
import { Overrides, resolveSpec } from "./servers.js";
import { Store } from "./store.js";
import { openTunnel } from "./tunnel.js";

export interface ServeOptions {
  gatewayPort: number;
  approvalPort: number;
  publicUrl: string | undefined;
}

const staleClientAge = 24 * 60 * 60 * 1000;
const auditRetention = 90 * 24 * 60 * 60 * 1000;

const AddRequest = Schema.Struct({ key: Schema.String, overrides: Overrides });

const RemoveRequest = Schema.Struct({ name: Schema.String });

const sweep = Effect.gen(function* () {
  const store = yield* Store;
  const now = Date.now();
  yield* store.pending.prune(now);
  yield* store.clients.prune(now - staleClientAge);
  yield* store.tokens.prune(now);
  yield* store.grants.prune(now - 60 * 60 * 1000);
  yield* store.audit.prune(now - auditRetention);
});

export const viewOf = (entry: Entry, publicUrl: string): ServerView => ({
  key: entry.spec.key,
  slug: entry.spec.slug,
  name: entry.spec.name,
  url: `${publicUrl}/${entry.spec.slug}/mcp`,
  state: entry.state,
  detail: entry.detail,
  tools: entry.allowed.size,
});

export const addServer = Effect.fn("Serve.addServer")(function* (
  registry: Registry,
  publicUrl: string,
  key: string,
  overrides: Overrides,
) {
  const settings = yield* loadSettings();
  const discovered = yield* discover(settings.servers);
  const spec = yield* resolveSpec(key, overrides, settings, discovered);
  const added = yield* registry.add(spec, overrides);
  const entry = (yield* registry.settled(spec.slug)) ?? added;
  return viewOf(entry, publicUrl);
});

export const serve = Effect.fn("Serve.start")(function* (options: ServeOptions) {
  const publicUrl = options.publicUrl ?? (yield* openTunnel(options.gatewayPort));
  const registry = yield* makeRegistry();
  const startedAt = Date.now();
  yield* startGateway({ port: options.gatewayPort, publicUrl, approvalPort: options.approvalPort, registry });
  yield* startApproval({ port: options.approvalPort, publicUrl, registry });
  const services = yield* Effect.services<Store>();
  yield* startControl({
    state: () =>
      Effect.succeed<DaemonState>({
        pid: process.pid,
        startedAt,
        tunnel: options.publicUrl === undefined ? "OpenTunnel" : "your own tunnel",
        publicUrl,
        servers: registry.list().map((entry) => viewOf(entry, publicUrl)),
      }),
    add: (body) =>
      Effect.gen(function* () {
        const request = Schema.decodeUnknownOption(AddRequest)(body);
        if (Option.isNone(request)) return { ok: false, error: "Invalid request" } satisfies AddResult;
        const server = yield* addServer(registry, publicUrl, request.value.key, request.value.overrides);
        return { ok: true, server } satisfies AddResult;
      }).pipe(
        Effect.catch((error) =>
          Effect.succeed<AddResult>({
            ok: false,
            error: error.message,
            ...("nextAction" in error && typeof error.nextAction === "string" ? { nextAction: error.nextAction } : {}),
          }),
        ),
        Effect.provideServices(services),
      ),
    remove: (body) =>
      Effect.gen(function* () {
        const request = Schema.decodeUnknownOption(RemoveRequest)(body);
        if (Option.isNone(request)) return { removed: false } satisfies RemoveResult;
        const { name } = request.value;
        const entry = registry.list().find((item) => item.spec.key === name || item.spec.slug === name);
        if (!entry) return { removed: false } satisfies RemoveResult;
        return { removed: yield* registry.remove(entry.spec.slug) } satisfies RemoveResult;
      }).pipe(Effect.orElseSucceed((): RemoveResult => ({ removed: false }))),
  });
  yield* Effect.forkScoped(sweep.pipe(Effect.ignore, Effect.repeat(Schedule.spaced("5 minutes"))));
  return { publicUrl, registry };
});
