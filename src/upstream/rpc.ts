import { Effect, Option, Predicate, Schema, Stream } from "effect";
import { UpstreamError } from "../errors.js";
import { Tool } from "../policy.js";

const ToolsListResponse = Schema.fromJsonString(
  Schema.Struct({ result: Schema.Struct({ tools: Schema.Array(Tool) }) }),
);

interface SseEvent {
  fields: ReadonlyArray<string>;
  data: ReadonlyArray<string>;
}

const emptyEvent: SseEvent = { fields: [], data: [] };

const isEventStream = (response: Response): boolean =>
  (response.headers.get("content-type") ?? "").includes("text/event-stream");

const collectEvent = (event: SseEvent, line: string): readonly [SseEvent, ReadonlyArray<SseEvent>] => {
  if (line === "") return event.fields.length + event.data.length === 0 ? [event, []] : [emptyEvent, [event]];
  if (!line.startsWith("data:")) return [{ ...event, fields: [...event.fields, line] }, []];
  return [{ ...event, data: [...event.data, line.slice("data:".length).replace(/^ /, "")] }, []];
};

const sseEvents = <E>(text: Stream.Stream<string, E>): Stream.Stream<SseEvent, E> =>
  text.pipe(
    Stream.splitLines,
    Stream.mapAccum(() => emptyEvent, collectEvent, {
      onHalt: (event) => (event.fields.length + event.data.length === 0 ? [] : [event]),
    }),
  );

const bodyText = (response: Response) =>
  Stream.fromReadableStream({
    evaluate: () => response.body ?? new ReadableStream<Uint8Array>(),
    onError: (cause) => new UpstreamError({ message: "Upstream closed the connection", cause }),
  }).pipe(Stream.decodeText);

export const rpcError = (id: unknown, code: number, message: string) => ({
  jsonrpc: "2.0",
  id: id ?? null,
  error: { code, message },
});

export const rpcResult = (id: unknown, result: unknown) => ({ jsonrpc: "2.0", id: id ?? null, result });

export const jsonPayloads = (response: Response) =>
  isEventStream(response)
    ? sseEvents(bodyText(response)).pipe(
        Stream.filter((event) => event.data.length > 0),
        Stream.map((event) => event.data.join("\n")),
        Stream.runCollect,
      )
    : Effect.tryPromise({
        try: () => response.text(),
        catch: (cause) => new UpstreamError({ message: "Upstream closed the connection", cause }),
      }).pipe(Effect.map((body) => [body]));

export const decodeToolsList = (payload: string, source: string) =>
  Schema.decodeUnknownEffect(ToolsListResponse)(payload).pipe(
    Effect.map((decoded) => decoded.result.tools),
    Effect.mapError((cause) => new UpstreamError({ message: `${source} returned an invalid tools/list`, cause })),
  );

const filterToolsMessage = (message: unknown, allowed: ReadonlySet<string>): unknown => {
  if (Array.isArray(message)) return message.map((item) => filterToolsMessage(item, allowed));
  if (!Predicate.isObject(message) || !Predicate.isObject(message.result)) return message;
  const { tools } = message.result;
  if (!Array.isArray(tools)) return message;
  const visible = tools.filter(
    (tool) => Predicate.isObject(tool) && typeof tool.name === "string" && allowed.has(tool.name),
  );
  return { ...message, result: { ...message.result, tools: visible } };
};

const decodeJson = Schema.decodeUnknownOption(Schema.fromJsonString(Schema.Unknown));

const filterJson = (text: string, allowed: ReadonlySet<string>): Option.Option<string> =>
  Option.map(decodeJson(text), (message) => JSON.stringify(filterToolsMessage(message, allowed)));

const filterEvent = (event: SseEvent, allowed: ReadonlySet<string>): string => {
  const data = event.data.length === 0 ? Option.none() : filterJson(event.data.join("\n"), allowed);
  const lines = Option.match(data, {
    onNone: () => event.fields,
    onSome: (json) => [...event.fields, `data: ${json}`],
  });
  return lines.length === 0 ? "" : `${lines.join("\n")}\n\n`;
};

const invalidResponse = JSON.stringify({
  jsonrpc: "2.0",
  id: null,
  error: { code: -32603, message: "Upstream returned an invalid response" },
});

export const filterToolsList = (response: Response, allowed: ReadonlySet<string>) => {
  const headers = new Headers(response.headers);
  headers.delete("content-length");
  if (isEventStream(response)) {
    const body = sseEvents(bodyText(response)).pipe(
      Stream.map((event) => filterEvent(event, allowed)),
      Stream.filter((text) => text.length > 0),
      Stream.encodeText,
      Stream.toReadableStream(),
    );
    return Effect.succeed(new Response(body, { status: response.status, headers }));
  }
  return Effect.tryPromise({
    try: () => response.text(),
    catch: (cause) => new UpstreamError({ message: "Upstream closed the connection", cause }),
  }).pipe(
    Effect.map((text) =>
      Option.match(filterJson(text, allowed), {
        onNone: () => new Response(invalidResponse, { status: 502, headers }),
        onSome: (json) => new Response(json, { status: response.status, headers }),
      }),
    ),
  );
};
