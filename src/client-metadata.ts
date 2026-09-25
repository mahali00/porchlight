import { Effect, Option, Schema, Stream } from "effect";
import type { ClientIdentity } from "./pages.js";
import type { LookupAddress } from "node:dns";
import { lookup } from "node:dns/promises";
import { BlockList, isIP } from "node:net";
import { ClientMetadataError } from "./errors.js";

const maxDocumentBytes = 64 * 1024;
const fetchTimeout = "5 seconds";
const cacheTtlMs = 24 * 60 * 60 * 1000;
const decoder = new TextDecoder();

const MetadataDocument = Schema.fromJsonString(
  Schema.Struct({
    redirect_uris: Schema.Array(Schema.String),
    client_name: Schema.optional(Schema.String),
  }),
);

export interface ClientMetadata {
  host: string;
  name: string | undefined;
  redirectUris: ReadonlyArray<string>;
}

const privateSubnets: ReadonlyArray<{ network: string; prefix: number; type: "ipv4" | "ipv6" }> = [
  { network: "0.0.0.0", prefix: 8, type: "ipv4" },
  { network: "10.0.0.0", prefix: 8, type: "ipv4" },
  { network: "100.64.0.0", prefix: 10, type: "ipv4" },
  { network: "127.0.0.0", prefix: 8, type: "ipv4" },
  { network: "169.254.0.0", prefix: 16, type: "ipv4" },
  { network: "172.16.0.0", prefix: 12, type: "ipv4" },
  { network: "192.168.0.0", prefix: 16, type: "ipv4" },
  { network: "::", prefix: 128, type: "ipv6" },
  { network: "::1", prefix: 128, type: "ipv6" },
  { network: "fc00::", prefix: 7, type: "ipv6" },
  { network: "fe80::", prefix: 10, type: "ipv6" },
];

const privateRanges = new BlockList();
for (const { network, prefix, type } of privateSubnets) privateRanges.addSubnet(network, prefix, type);

const failure = (message: string, cause?: unknown) => new ClientMetadataError({ message, cause });

const isPublic = ({ address, family }: LookupAddress): boolean =>
  !privateRanges.check(address, family === 6 ? "ipv6" : "ipv4");

const publicAddresses = Effect.fn("ClientMetadata.publicAddresses")(function* (hostname: string) {
  const addresses = yield* Effect.tryPromise({
    try: () => lookup(hostname, { all: true }),
    catch: (cause) => failure(`Couldn't resolve ${hostname}`, cause),
  });
  if (addresses.length === 0 || !addresses.every(isPublic)) return yield* failure("Client metadata host is private");
  return [...addresses].sort((left, right) => left.family - right.family);
});

const cache = new Map<string, { metadata: ClientMetadata; fetchedAt: number }>();
const maxCachedClients = 256;
const maxDownloadsPerMinute = 20;
const recentDownloads: Array<number> = [];

const remember = (clientId: string, metadata: ClientMetadata): void => {
  cache.delete(clientId);
  cache.set(clientId, { metadata, fetchedAt: Date.now() });
  const oldest = cache.keys().next();
  if (cache.size > maxCachedClients && !oldest.done) cache.delete(oldest.value);
};

const takeDownloadSlot = Effect.sync(() => {
  const now = Date.now();
  while (recentDownloads.length > 0 && (recentDownloads[0] ?? now) < now - 60_000) recentDownloads.shift();
  if (recentDownloads.length >= maxDownloadsPerMinute) return false;
  recentDownloads.push(now);
  return true;
});

export const isValidRedirectUri = (uri: string): boolean => {
  if (!URL.canParse(uri)) return false;
  const { protocol, hostname } = new URL(uri);
  if (protocol === "https:") return true;
  return protocol === "http:" && (hostname === "127.0.0.1" || hostname === "localhost");
};

const pinned = (url: URL, { address, family }: LookupAddress): Request => {
  const target = new URL(url);
  target.hostname = family === 6 ? `[${address}]` : address;
  return new Request(target, { headers: { host: url.host } });
};

const request = (url: URL, hostname: string, address: LookupAddress) =>
  Effect.tryPromise({
    try: (signal) =>
      isIP(hostname) === 0
        ? fetch(pinned(url, address), { tls: { serverName: hostname }, redirect: "manual", signal })
        : fetch(url, { redirect: "manual", signal }),
    catch: (cause) => failure(`Couldn't reach ${url.host}`, cause),
  });

const readCapped = (response: Response) =>
  Stream.fromReadableStream({
    evaluate: () => response.body ?? new ReadableStream<Uint8Array>(),
    onError: (cause) => failure("Couldn't read client metadata", cause),
  }).pipe(
    Stream.runFoldEffect(
      () => ({ chunks: new Array<Uint8Array>(), size: 0 }),
      (read, chunk) =>
        read.size + chunk.byteLength > maxDocumentBytes
          ? Effect.fail(failure(`Client metadata document exceeds ${maxDocumentBytes} bytes`))
          : Effect.sync(() => {
              read.chunks.push(chunk);
              return { chunks: read.chunks, size: read.size + chunk.byteLength };
            }),
    ),
    Effect.map(({ chunks }) => decoder.decode(Buffer.concat(chunks))),
  );

const download = Effect.fn("ClientMetadata.download")(function* (clientId: string) {
  const url = new URL(clientId);
  const hostname = url.hostname.replace(/^\[|\]$/g, "");
  const [first, ...rest] = yield* publicAddresses(hostname);
  if (!first) return yield* failure("Client metadata host is private");
  const response = yield* rest.reduce(
    (attempt, address) => attempt.pipe(Effect.catch(() => request(url, hostname, address))),
    request(url, hostname, first),
  );
  if (response.status !== 200) return yield* failure(`Client metadata returned ${response.status}`);
  return yield* readCapped(response);
});

export const fetchClientMetadata = Effect.fn("ClientMetadata.fetch")(function* (clientId: string) {
  if (!clientId.startsWith("https://") || !URL.canParse(clientId)) return Option.none<ClientMetadata>();
  const cached = cache.get(clientId);
  if (cached && Date.now() - cached.fetchedAt < cacheTtlMs) return Option.some(cached.metadata);
  if (!(yield* takeDownloadSlot)) return Option.none<ClientMetadata>();
  const document = yield* download(clientId).pipe(
    Effect.timeout(fetchTimeout),
    Effect.flatMap(Schema.decodeUnknownEffect(MetadataDocument)),
    Effect.option,
  );
  return Option.map(document, (doc) => {
    const metadata = { host: new URL(clientId).host, name: doc.client_name, redirectUris: doc.redirect_uris };
    remember(clientId, metadata);
    return metadata;
  });
});

export const identifyClient = Effect.fn("ClientMetadata.identify")(function* (clientId: string, fallbackName: string) {
  const metadata = yield* fetchClientMetadata(clientId);
  return Option.match(metadata, {
    onNone: (): ClientIdentity => ({ name: fallbackName, verifiedHost: undefined }),
    onSome: (doc): ClientIdentity => ({ name: doc.name ?? doc.host, verifiedHost: doc.host }),
  });
});
