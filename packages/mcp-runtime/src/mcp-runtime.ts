import { spawn } from "node:child_process";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import { createInterface } from "node:readline";
import type { Interface as ReadLineInterface } from "node:readline";
import type { ToolDescriptor, ToolRegistry } from "../../tool-runtime/src/tool-runtime.ts";

type JsonObject = Record<string, unknown>;

function object(value: unknown, name: string): JsonObject {
  if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error(`invalid-${name}: expected object`);
  return value as JsonObject;
}

function string(value: unknown, name: string): string {
  if (typeof value !== "string") throw new Error(`invalid-${name}: expected string`);
  return value;
}

export interface McpToolDescriptor {
  name: string;
  title?: string;
  description?: string;
  inputSchema: JsonObject;
  annotations?: {
    readOnlyHint?: boolean;
    destructiveHint?: boolean;
    idempotentHint?: boolean;
    openWorldHint?: boolean;
  };
}

export interface McpResourceDescriptor {
  uri: string;
  name: string;
  description?: string;
  mimeType?: string;
}

export interface McpCallToolResult {
  content: unknown[];
  isError?: boolean;
  structuredContent?: unknown;
}

export interface McpServerConfig {
  id: string;
  name: string;
  transport: "stdio";
  command: string;
  args: string[];
  cwd?: string;
  env?: Record<string, string>;
  requestTimeoutMs?: number;
}

export type McpConnectionStatus = "disconnected" | "connecting" | "ready" | "degraded" | "failed";

export interface McpServerView {
  id: string;
  name: string;
  status: McpConnectionStatus;
  protocolVersion?: string;
  serverName?: string;
  serverVersion?: string;
  toolCount: number;
  resourceCount: number;
  lastError?: string;
}

interface JsonRpcSuccess {
  jsonrpc: "2.0";
  id: number;
  result: unknown;
}

interface JsonRpcFailure {
  jsonrpc: "2.0";
  id: number;
  error: { code: number; message: string; data?: unknown };
}

interface PendingRequest {
  resolve(value: unknown): void;
  reject(error: Error): void;
  timeout: ReturnType<typeof setTimeout>;
}

export interface McpTransport {
  request(method: string, params?: unknown): Promise<unknown>;
  notify(method: string, params?: unknown): void;
  close(): Promise<void>;
}

function minimalEnvironment(extra?: Record<string, string>): Record<string, string> {
  const allowedKeys = ["PATH", "Path", "PATHEXT", "SystemRoot", "WINDIR", "TEMP", "TMP", "HOME", "USERPROFILE"];
  const environment: Record<string, string> = {};
  for (const key of allowedKeys) {
    const value = process.env[key];
    if (value !== undefined) environment[key] = value;
  }
  return { ...environment, ...extra };
}

/** MCP STDIO：每行一个 JSON-RPC 消息，stderr 只用于诊断，不能混入协议流。 */
export class StdioMcpTransport implements McpTransport {
  readonly #child: ChildProcessWithoutNullStreams;
  readonly #reader: ReadLineInterface;
  readonly #pending = new Map<number, PendingRequest>();
  readonly #requestTimeoutMs: number;
  #nextId = 1;
  #closed = false;
  #stderrTail = "";

  constructor(config: McpServerConfig) {
    this.#requestTimeoutMs = config.requestTimeoutMs ?? 10_000;
    this.#child = spawn(config.command, config.args, {
      cwd: config.cwd,
      env: minimalEnvironment(config.env),
      stdio: ["pipe", "pipe", "pipe"],
      windowsHide: true,
      shell: false,
    });
    this.#reader = createInterface({ input: this.#child.stdout, crlfDelay: Infinity });
    this.#reader.on("line", (line) => this.handleLine(line));
    this.#child.stderr.setEncoding("utf8");
    this.#child.stderr.on("data", (chunk: string) => {
      this.#stderrTail = `${this.#stderrTail}${chunk}`.slice(-8_192);
    });
    this.#child.once("error", (error) => this.failAll(error));
    this.#child.once("exit", (code, signal) => {
      if (!this.#closed) {
        this.failAll(new Error(`mcp-process-exited: code=${code}, signal=${signal}, stderr=${this.#stderrTail}`));
      }
    });
  }

  request(method: string, params?: unknown): Promise<unknown> {
    if (this.#closed) return Promise.reject(new Error("mcp-transport-closed"));
    const id = this.#nextId++;
    const payload = { jsonrpc: "2.0", id, method, ...(params === undefined ? {} : { params }) };
    return new Promise((resolve, reject) => {
      const timeout = setTimeout(() => {
        this.#pending.delete(id);
        reject(new Error(`mcp-request-timeout: ${method}`));
      }, this.#requestTimeoutMs);
      timeout.unref();
      this.#pending.set(id, { resolve, reject, timeout });
      this.#child.stdin.write(`${JSON.stringify(payload)}\n`);
    });
  }

  notify(method: string, params?: unknown): void {
    if (this.#closed) throw new Error("mcp-transport-closed");
    this.#child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", method, ...(params === undefined ? {} : { params }) })}\n`);
  }

  async close(): Promise<void> {
    if (this.#closed) return;
    this.#closed = true;
    this.#reader.close();
    this.#child.stdin.end();
    if (this.#child.exitCode !== null) return;
    await new Promise<void>((resolveClose) => {
      const killTimer = setTimeout(() => {
        if (this.#child.exitCode === null) this.#child.kill();
      }, 500);
      killTimer.unref();
      this.#child.once("exit", () => {
        clearTimeout(killTimer);
        resolveClose();
      });
    });
  }

  private handleLine(line: string): void {
    if (!line.trim()) return;
    if (line.length > 1_048_576) {
      this.failAll(new Error("mcp-message-too-large"));
      return;
    }
    let message: unknown;
    try {
      message = JSON.parse(line);
    } catch {
      this.failAll(new Error("mcp-invalid-json-line"));
      return;
    }
    const input = object(message, "mcp-response");
    if (typeof input.id !== "number") return; // D0 忽略 server notification。
    const pending = this.#pending.get(input.id);
    if (!pending) return;
    this.#pending.delete(input.id);
    clearTimeout(pending.timeout);
    if (input.error !== undefined) {
      const error = object(input.error, "mcp-error");
      pending.reject(new Error(`mcp-error ${String(error.code)}: ${String(error.message)}`));
      return;
    }
    pending.resolve((input as unknown as JsonRpcSuccess).result);
  }

  private failAll(error: Error): void {
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timeout);
      pending.reject(error);
    }
    this.#pending.clear();
  }
}

export class McpClient {
  readonly transport: McpTransport;
  protocolVersion?: string;
  serverInfo?: { name: string; version: string };

  constructor(transport: McpTransport) {
    this.transport = transport;
  }

  async initialize(): Promise<void> {
    const raw = await this.transport.request("initialize", {
      protocolVersion: "2025-06-18",
      capabilities: {},
      clientInfo: { name: "harness-terminal", version: "0.0.1" },
    });
    const result = object(raw, "mcp-initialize-result");
    this.protocolVersion = string(result.protocolVersion, "mcp-protocol-version");
    const serverInfo = object(result.serverInfo, "mcp-server-info");
    this.serverInfo = {
      name: string(serverInfo.name, "mcp-server-name"),
      version: string(serverInfo.version, "mcp-server-version"),
    };
    this.transport.notify("notifications/initialized");
  }

  async listTools(): Promise<McpToolDescriptor[]> {
    const result = object(await this.transport.request("tools/list", {}), "mcp-tools-list-result");
    if (!Array.isArray(result.tools)) throw new Error("invalid-mcp-tools-list");
    return result.tools.map((value) => {
      const tool = object(value, "mcp-tool");
      const annotations = tool.annotations === undefined ? undefined : object(tool.annotations, "mcp-tool-annotations");
      return {
        name: string(tool.name, "mcp-tool-name"),
        ...(typeof tool.title === "string" ? { title: tool.title } : {}),
        ...(typeof tool.description === "string" ? { description: tool.description } : {}),
        inputSchema: object(tool.inputSchema, "mcp-tool-input-schema"),
        ...(annotations
          ? {
              annotations: {
                ...(typeof annotations.readOnlyHint === "boolean" ? { readOnlyHint: annotations.readOnlyHint } : {}),
                ...(typeof annotations.destructiveHint === "boolean" ? { destructiveHint: annotations.destructiveHint } : {}),
                ...(typeof annotations.idempotentHint === "boolean" ? { idempotentHint: annotations.idempotentHint } : {}),
                ...(typeof annotations.openWorldHint === "boolean" ? { openWorldHint: annotations.openWorldHint } : {}),
              },
            }
          : {}),
      };
    });
  }

  async callTool(name: string, args: unknown): Promise<McpCallToolResult> {
    const result = object(await this.transport.request("tools/call", { name, arguments: args }), "mcp-call-result");
    if (!Array.isArray(result.content)) throw new Error("invalid-mcp-tool-content");
    return {
      content: result.content,
      ...(typeof result.isError === "boolean" ? { isError: result.isError } : {}),
      ...(result.structuredContent === undefined ? {} : { structuredContent: result.structuredContent }),
    };
  }

  async listResources(): Promise<McpResourceDescriptor[]> {
    const result = object(await this.transport.request("resources/list", {}), "mcp-resources-list-result");
    if (!Array.isArray(result.resources)) throw new Error("invalid-mcp-resources-list");
    return result.resources.map((value) => {
      const resource = object(value, "mcp-resource");
      return {
        uri: string(resource.uri, "mcp-resource-uri"),
        name: string(resource.name, "mcp-resource-name"),
        ...(typeof resource.description === "string" ? { description: resource.description } : {}),
        ...(typeof resource.mimeType === "string" ? { mimeType: resource.mimeType } : {}),
      };
    });
  }

  async readResource(uri: string): Promise<unknown[]> {
    const result = object(await this.transport.request("resources/read", { uri }), "mcp-resource-read-result");
    if (!Array.isArray(result.contents)) throw new Error("invalid-mcp-resource-contents");
    return result.contents;
  }
}

interface ManagedServer {
  config: McpServerConfig;
  status: McpConnectionStatus;
  client?: McpClient;
  tools: McpToolDescriptor[];
  resources: McpResourceDescriptor[];
  lastError?: string;
  toolDisposers: Array<() => void>;
}

export class McpManager {
  readonly #servers = new Map<string, ManagedServer>();

  addServer(config: McpServerConfig): void {
    if (this.#servers.has(config.id)) throw new Error(`mcp-server-exists: ${config.id}`);
    this.#servers.set(config.id, {
      config,
      status: "disconnected",
      tools: [],
      resources: [],
      toolDisposers: [],
    });
  }

  async connect(serverId: string): Promise<McpServerView> {
    const server = this.server(serverId);
    if (server.status === "ready") return this.view(server);
    server.status = "connecting";
    let client: McpClient | undefined;
    try {
      client = new McpClient(new StdioMcpTransport(server.config));
      await client.initialize();
      server.client = client;
      server.tools = await client.listTools();
      server.resources = await client.listResources();
      server.status = "ready";
      server.lastError = undefined;
    } catch (error) {
      server.status = "failed";
      server.lastError = error instanceof Error ? error.message : String(error);
      await client?.transport.close();
      server.client = undefined;
    }
    return this.view(server);
  }

  async disconnect(serverId: string): Promise<void> {
    const server = this.server(serverId);
    for (const dispose of server.toolDisposers.splice(0).reverse()) dispose();
    await server.client?.transport.close();
    server.client = undefined;
    server.tools = [];
    server.resources = [];
    server.status = "disconnected";
  }

  listServers(): McpServerView[] {
    return [...this.#servers.values()].map((server) => this.view(server));
  }

  listTools(serverId: string): McpToolDescriptor[] {
    return [...this.server(serverId).tools];
  }

  listResources(serverId: string): McpResourceDescriptor[] {
    return [...this.server(serverId).resources];
  }

  async readResource(serverId: string, uri: string): Promise<unknown[]> {
    const client = this.readyClient(serverId);
    return client.readResource(uri);
  }

  async callTool(serverId: string, toolName: string, args: unknown): Promise<McpCallToolResult> {
    const client = this.readyClient(serverId);
    if (!this.server(serverId).tools.some((tool) => tool.name === toolName)) throw new Error(`mcp-tool-not-found: ${toolName}`);
    return client.callTool(toolName, args);
  }

  /** MCP catalog 按需注册到统一 ToolRegistry，并返回可逆 disposer。 */
  contributeTools(serverId: string, registry: ToolRegistry): () => void {
    const server = this.server(serverId);
    if (server.status !== "ready") throw new Error(`mcp-server-not-ready: ${serverId}`);
    for (const tool of server.tools) {
      const canonicalName = `mcp.${serverId}.${tool.name}`;
      const descriptor: ToolDescriptor<unknown, McpCallToolResult> = {
        canonicalName,
        version: "1",
        description: tool.description ?? tool.title ?? tool.name,
        effectClass: tool.annotations?.readOnlyHint
          ? "read-only-retryable"
          : tool.annotations?.idempotentHint
            ? "idempotent-effect"
            : tool.annotations?.destructiveHint
              ? "non-repeatable-effect"
              : "verifiable-effect",
        validateArgs(value) {
          if (typeof value !== "object" || value === null || Array.isArray(value)) throw new Error("invalid-mcp-tool-args");
          return value;
        },
        validateResult(value) {
          const result = object(value, "mcp-tool-result");
          if (!Array.isArray(result.content)) throw new Error("invalid-mcp-tool-result-content");
          return value as McpCallToolResult;
        },
        permissionAction() {
          return {
            kind: "mcp.call",
            serverId,
            toolName: tool.name,
            sideEffect: !tool.annotations?.readOnlyHint,
          };
        },
      };
      server.toolDisposers.push(
        registry.register(descriptor, {
          execute: ({ args }) => this.callTool(serverId, tool.name, args),
        }),
      );
    }
    return () => {
      for (const dispose of server.toolDisposers.splice(0).reverse()) dispose();
    };
  }

  private server(serverId: string): ManagedServer {
    const server = this.#servers.get(serverId);
    if (!server) throw new Error(`mcp-server-not-found: ${serverId}`);
    return server;
  }

  private readyClient(serverId: string): McpClient {
    const server = this.server(serverId);
    if (server.status !== "ready" || !server.client) throw new Error(`mcp-server-not-ready: ${serverId}`);
    return server.client;
  }

  private view(server: ManagedServer): McpServerView {
    return {
      id: server.config.id,
      name: server.config.name,
      status: server.status,
      ...(server.client?.protocolVersion ? { protocolVersion: server.client.protocolVersion } : {}),
      ...(server.client?.serverInfo
        ? { serverName: server.client.serverInfo.name, serverVersion: server.client.serverInfo.version }
        : {}),
      toolCount: server.tools.length,
      resourceCount: server.resources.length,
      ...(server.lastError ? { lastError: server.lastError } : {}),
    };
  }
}
