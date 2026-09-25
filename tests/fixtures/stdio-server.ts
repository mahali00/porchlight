const tools = [{ name: "echo", annotations: { readOnlyHint: true } }, { name: "wipe" }];

const answer = (method: string) => {
  if (method === "initialize") {
    return {
      protocolVersion: "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "fixture", version: "1" },
    };
  }
  if (method === "tools/list") return { tools };
  if (method === "tools/call") return { content: [{ type: "text", text: `pid ${process.pid}` }] };
  return {};
};

const decoder = new TextDecoder();
let buffer = "";
for await (const chunk of Bun.stdin.stream()) {
  buffer += decoder.decode(chunk, { stream: true });
  let newline = buffer.indexOf("\n");
  while (newline >= 0) {
    const line = buffer.slice(0, newline).trim();
    buffer = buffer.slice(newline + 1);
    newline = buffer.indexOf("\n");
    if (line.length === 0) continue;
    const message = JSON.parse(line);
    if (message.id === undefined) continue;
    process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id: message.id, result: answer(message.method) })}\n`);
  }
}
