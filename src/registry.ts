import { Effect, Exit, FiberHandle, Option, Schedule, Scope, Stream, SubscriptionRef } from "effect";
import { allowedToolNames, isDangerous, runsClientCode, type Tool } from "./policy.js";
import { logFileFor, type Overrides, type ServerSpec } from "./servers.js";
import { Store } from "./store.js";
import { makeCliUpstream } from "./upstream/cli.js";
import { makeHttpUpstream } from "./upstream/http.js";
import { makeStdioUpstream } from "./upstream/stdio.js";
import type { Upstream } from "./upstream/types.js";

export type ServerState = "starting" | "waiting" | "live" | "refused";

export interface Entry {
  readonly spec: ServerSpec;
  readonly overrides: Overrides;
  readonly upstream: Upstream;
  readonly state: ServerState;
  readonly detail: string;
  readonly tools: ReadonlyArray<Tool>;
  readonly allowed: ReadonlySet<string>;
}

interface Slot {
  ref: SubscriptionRef.SubscriptionRef<Entry>;
  scope: Scope.Closeable;
  supervisor: FiberHandle.FiberHandle;
}

const retryEvery = "3 seconds";
const settleTimeout = "6 seconds";

const waiting = (entry: Entry): Entry => ({
  ...entry,
  state: "waiting",
  detail: `waiting for ${entry.spec.name} to start`,
});

const risky = (spec: ServerSpec, tool: Tool): boolean =>
  spec.source.kind === "cli" ? runsClientCode(spec.source.tools[tool.name]?.run ?? []) : isDangerous(tool);

const checked = (entry: Entry, tools: ReadonlyArray<Tool>): Entry => {
  const allowed = allowedToolNames(tools, entry.spec.policy);
  const dangerous = tools.filter((tool) => allowed.has(tool.name) && risky(entry.spec, tool)).map((tool) => tool.name);
  if (dangerous.length === 0 || entry.spec.allowDangerous) {
    return { ...entry, tools, allowed, state: "live", detail: `${allowed.size} of ${tools.length} tools` };
  }
  const detail =
    `refused: ${dangerous.slice(0, 5).join(", ")} can run commands or code. ` +
    `Hide them with --deny ${dangerous.join(",")}, ` +
    `or allow them with porchlight apps add ${entry.spec.key} --allow-dangerous`;
  return { ...entry, tools, allowed, state: "refused", detail };
};

export const makeRegistry = Effect.fn("Registry.make")(function* () {
  const parent = yield* Scope.fork(yield* Scope.Scope, "parallel");
  const store = yield* Store;
  const slots = new Map<string, Slot>();

  const check = (ref: SubscriptionRef.SubscriptionRef<Entry>) =>
    Effect.gen(function* () {
      const entry = yield* SubscriptionRef.get(ref);
      const next = yield* entry.upstream.listTools.pipe(
        Effect.map((tools) => checked(entry, tools)),
        Effect.orElseSucceed(() => waiting(entry)),
      );
      yield* SubscriptionRef.set(ref, next);
      return next.state !== "waiting";
    });

  const supervise = (slot: Slot) =>
    FiberHandle.run(
      slot.supervisor,
      check(slot.ref).pipe(Effect.repeat({ until: (settled) => settled, schedule: Schedule.spaced(retryEvery) })),
    );

  const persist = Effect.fn("Registry.persist")(function* () {
    const entries = [...slots.values()].map((slot) => SubscriptionRef.getUnsafe(slot.ref));
    yield* store.exposed.set(entries.map((entry) => ({ key: entry.spec.key, overrides: entry.overrides })));
  });

  const remove = Effect.fn("Registry.remove")(function* (slug: string) {
    const slot = slots.get(slug);
    if (!slot) return false;
    slots.delete(slug);
    yield* Scope.close(slot.scope, Exit.void);
    yield* persist();
    return true;
  });

  const connect = (spec: ServerSpec, scope: Scope.Closeable) => {
    const { source } = spec;
    if (source.kind === "http") return Effect.succeed(makeHttpUpstream(source.url, source.headers));
    if (source.kind === "cli") {
      return Effect.succeed(
        makeCliUpstream({ name: spec.name, tools: source.tools, env: source.env, cwd: source.cwd }),
      );
    }
    return makeStdioUpstream({
      name: spec.name,
      slug: spec.slug,
      command: source.command,
      env: source.env,
      cwd: source.cwd,
      logFile: logFileFor(spec.slug),
    }).pipe(Scope.provide(scope), Effect.provideService(Store, store));
  };

  const add = Effect.fn("Registry.add")(function* (spec: ServerSpec, overrides: Overrides) {
    yield* remove(spec.slug);
    const scope = yield* Scope.fork(parent, "parallel");
    const upstream = yield* connect(spec, scope);
    const entry: Entry = {
      spec,
      overrides,
      upstream,
      state: "starting",
      detail: "starting",
      tools: [],
      allowed: new Set(),
    };
    const ref = yield* SubscriptionRef.make(entry);
    const supervisor = yield* FiberHandle.make().pipe(Scope.provide(scope));
    const slot: Slot = { ref, scope, supervisor };
    slots.set(spec.slug, slot);
    yield* supervise(slot);
    yield* persist();
    return entry;
  });

  const markDown = Effect.fn("Registry.markDown")(function* (slug: string) {
    const slot = slots.get(slug);
    if (!slot) return;
    const wasLive = yield* SubscriptionRef.modify(slot.ref, (entry) =>
      entry.state === "live" ? [true, waiting(entry)] : [false, entry],
    );
    if (wasLive) yield* supervise(slot);
  });

  const get = (slug: string): Entry | undefined => {
    const slot = slots.get(slug);
    return slot ? SubscriptionRef.getUnsafe(slot.ref) : undefined;
  };

  const changes = (slug: string): Stream.Stream<Entry> => {
    const slot = slots.get(slug);
    return slot ? SubscriptionRef.changes(slot.ref) : Stream.empty;
  };

  const settled = Effect.fn("Registry.settled")(function* (slug: string) {
    const next = yield* changes(slug).pipe(
      Stream.filter((entry) => entry.state !== "starting"),
      Stream.runHead,
      Effect.timeoutOrElse({ duration: settleTimeout, orElse: () => Effect.succeed(Option.none()) }),
    );
    return Option.getOrElse(next, () => get(slug));
  });

  return {
    add,
    remove,
    markDown,
    settled,
    changes,
    get,
    list: () => [...slots.values()].map((slot) => SubscriptionRef.getUnsafe(slot.ref)),
  };
});

export type Registry = Effect.Success<ReturnType<typeof makeRegistry>>;
