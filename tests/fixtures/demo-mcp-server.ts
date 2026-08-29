import { createInterface } from "node:readline";

/** 仅供 MCP contract test 使用，不访问网络或外部账户。 */
const reader = createInterface({ input: process.stdin, crlfDelay: Infinity });

function send(id: number, result: unknown): void {
  process.stdout.write(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
}

reader.on("line", (line) => {
  const request = JSON.parse(line);
  if (typeof request.id !== "number") return;
  switch (request.method) {
    case "initialize":
      send(request.id, {
        protocolVersion: request.params.protocolVersion,
        capabilities: { tools: {}, resources: {} },
        serverInfo: { name: "harness-demo-mcp", version: "1.0.0" },
      });
      break;
    case "tools/list":
      send(request.id, {
        tools: [
          {
            name: "echo.read",
            description: "回显输入",
            inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
            annotations: { readOnlyHint: true },
          },
          {
            name: "message.send",
            description: "模拟发送消息",
            inputSchema: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
            annotations: { destructiveHint: true },
          },
        ],
      });
      break;
    case "tools/call":
      send(request.id, {
        content: [{ type: "text", text: String(request.params.arguments?.text ?? "") }],
        structuredContent: { tool: request.params.name, args: request.params.arguments },
      });
      break;
    case "resources/list":
      send(request.id, {
        resources: [{ uri: "memory://guide", name: "Harness Guide", mimeType: "text/plain" }],
      });
      break;
    case "resources/read":
      send(request.id, {
        contents: [{ uri: request.params.uri, mimeType: "text/plain", text: "fake resource" }],
      });
      break;
    default:
      process.stdout.write(
        `${JSON.stringify({ jsonrpc: "2.0", id: request.id, error: { code: -32601, message: "Method not found" } })}\n`,
      );
  }
});
