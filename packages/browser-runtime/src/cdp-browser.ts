import { execFileSync, spawn } from "node:child_process";
import type { ChildProcess } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, unlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import type { ToolDescriptor, ToolProvider, ToolRegistry } from "../../tool-runtime/src/tool-runtime.ts";

interface CdpMessage {
  id?: number;
  method?: string;
  params?: unknown;
  sessionId?: string;
  result?: unknown;
  error?: { code: number; message: string };
}

interface PendingCdpRequest {
  resolve(value: unknown): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
}

interface CdpEventWaiter {
  method: string;
  sessionId?: string;
  resolve(params: unknown): void;
  reject(error: Error): void;
  timer: ReturnType<typeof setTimeout>;
}

class CdpConnection {
  readonly #socket: WebSocket;
  readonly #pending = new Map<number, PendingCdpRequest>();
  readonly #waiters = new Set<CdpEventWaiter>();
  #nextId = 1;

  private constructor(socket: WebSocket) {
    this.#socket = socket;
    socket.addEventListener("message", (event) => this.handleMessage(String(event.data)));
    socket.addEventListener("close", () => this.failAll(new Error("cdp-connection-closed")));
    socket.addEventListener("error", () => this.failAll(new Error("cdp-connection-error")));
  }

  static async connect(url: string, timeoutMs = 10_000): Promise<CdpConnection> {
    const socket = new WebSocket(url);
    await new Promise<void>((resolveOpen, rejectOpen) => {
      const timer = setTimeout(() => rejectOpen(new Error("cdp-websocket-open-timeout")), timeoutMs);
      timer.unref();
      socket.addEventListener("open", () => {
        clearTimeout(timer);
        resolveOpen();
      }, { once: true });
      socket.addEventListener("error", () => {
        clearTimeout(timer);
        rejectOpen(new Error("cdp-websocket-open-failed"));
      }, { once: true });
    });
    return new CdpConnection(socket);
  }

  send(method: string, params: unknown = {}, sessionId?: string, timeoutMs = 10_000): Promise<unknown> {
    const id = this.#nextId++;
    return new Promise((resolveRequest, rejectRequest) => {
      const timer = setTimeout(() => {
        this.#pending.delete(id);
        rejectRequest(new Error(`cdp-request-timeout: ${method}`));
      }, timeoutMs);
      timer.unref();
      this.#pending.set(id, { resolve: resolveRequest, reject: rejectRequest, timer });
      this.#socket.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  }

  waitForEvent(method: string, sessionId?: string, timeoutMs = 10_000): Promise<unknown> {
    return new Promise((resolveEvent, rejectEvent) => {
      const waiter: CdpEventWaiter = {
        method,
        ...(sessionId ? { sessionId } : {}),
        resolve: resolveEvent,
        reject: rejectEvent,
        timer: setTimeout(() => {
          this.#waiters.delete(waiter);
          rejectEvent(new Error(`cdp-event-timeout: ${method}`));
        }, timeoutMs),
      };
      waiter.timer.unref();
      this.#waiters.add(waiter);
    });
  }

  async close(): Promise<void> {
    if (this.#socket.readyState === WebSocket.CLOSED) return;
    this.#socket.close();
    await new Promise<void>((resolveClose) => {
      const timer = setTimeout(resolveClose, 500);
      timer.unref();
      this.#socket.addEventListener("close", () => {
        clearTimeout(timer);
        resolveClose();
      }, { once: true });
    });
  }

  private handleMessage(raw: string): void {
    let message: CdpMessage;
    try {
      message = JSON.parse(raw) as CdpMessage;
    } catch {
      this.failAll(new Error("cdp-invalid-json"));
      return;
    }

    if (typeof message.id === "number") {
      const pending = this.#pending.get(message.id);
      if (!pending) return;
      this.#pending.delete(message.id);
      clearTimeout(pending.timer);
      if (message.error) pending.reject(new Error(`cdp-error ${message.error.code}: ${message.error.message}`));
      else pending.resolve(message.result);
      return;
    }

    if (message.method) {
      for (const waiter of [...this.#waiters]) {
        if (waiter.method !== message.method || (waiter.sessionId && waiter.sessionId !== message.sessionId)) continue;
        this.#waiters.delete(waiter);
        clearTimeout(waiter.timer);
        waiter.resolve(message.params);
      }
    }
  }

  private failAll(error: Error): void {
    for (const pending of this.#pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(error);
    }
    this.#pending.clear();
    for (const waiter of this.#waiters) {
      clearTimeout(waiter.timer);
      waiter.reject(error);
    }
    this.#waiters.clear();
  }
}

export interface BrowserSnapshotNode {
  ref?: string;
  role: string;
  name: string;
  description?: string;
}

export interface BrowserSnapshot {
  url: string;
  title: string;
  nodes: BrowserSnapshotNode[];
}

export interface BrowserActionRecord {
  sequence: number;
  action: "navigate" | "snapshot" | "click" | "type" | "screenshot";
  target?: string;
  origin?: string;
  status: "completed" | "failed";
  recordedAt: string;
  error?: string;
}

export interface BrowserArtifactRef {
  id: string;
  path: string;
  mimeType: "image/png";
  bytes: number;
}

export interface BrowserArtifactStore {
  savePng(sessionId: string, base64: string): Promise<BrowserArtifactRef>;
}

export class FileBrowserArtifactStore implements BrowserArtifactStore {
  readonly rootDirectory: string;

  constructor(rootDirectory: string) {
    this.rootDirectory = rootDirectory;
    mkdirSync(rootDirectory, { recursive: true });
  }

  async savePng(sessionId: string, base64: string): Promise<BrowserArtifactRef> {
    const safeSessionId = sessionId.replace(/[^A-Za-z0-9_-]/g, "-");
    const id = `browser-shot-${Date.now()}`;
    const path = join(this.rootDirectory, `${safeSessionId}-${id}.png`);
    const bytes = Buffer.from(base64, "base64");
    writeFileSync(path, bytes);
    return { id, path, mimeType: "image/png", bytes: bytes.length };
  }
}

export interface BrowserSessionConfig {
  id: string;
  executablePath: string;
  profileDirectory: string;
  headless: boolean;
  allowedOrigins: string[];
  artifactStore: BrowserArtifactStore;
}

const interactiveRoles = new Set([
  "button",
  "link",
  "textbox",
  "searchbox",
  "checkbox",
  "radio",
  "combobox",
  "listbox",
  "menuitem",
  "tab",
  "switch",
  "slider",
]);

function valueOfAxProperty(value: unknown): string {
  if (typeof value !== "object" || value === null) return "";
  const raw = (value as { value?: unknown }).value;
  return raw === undefined || raw === null ? "" : String(raw);
}

function parseDevToolsPort(profileDirectory: string): { port: number; browserPath: string } | undefined {
  try {
    const lines = readFileSync(join(profileDirectory, "DevToolsActivePort"), "utf8").trim().split(/\r?\n/);
    const port = Number(lines[0]);
    const browserPath = lines[1];
    if (!Number.isInteger(port) || port <= 0 || !browserPath) return undefined;
    return { port, browserPath };
  } catch {
    return undefined;
  }
}

async function waitForDevToolsPort(profileDirectory: string, child: ChildProcess, timeoutMs = 10_000) {
  const started = Date.now();
  while (Date.now() - started < timeoutMs) {
    const endpoint = parseDevToolsPort(profileDirectory);
    if (endpoint) return endpoint;
    if (child.exitCode !== null) throw new Error(`browser-exited-before-cdp: ${child.exitCode}`);
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
  }
  throw new Error("browser-cdp-port-timeout");
}

/** 通过注册表/which 做真实发现，不把安装路径猜测写进 Runtime。 */
export function discoverInstalledBrowser(): string | undefined {
  if (process.platform === "win32") {
    // Chrome headless 在受限 Windows 环境中比 Edge 的 Graphite/GPU 启动路径更稳定。
    for (const executable of ["chrome.exe", "msedge.exe"]) {
      const key = `HKLM\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\${executable}`;
      try {
        const output = execFileSync("reg.exe", ["query", key, "/ve"], { encoding: "utf8", windowsHide: true });
        const match = output.match(/REG_SZ\s+(.+\.exe)\s*$/im);
        if (match?.[1]) return match[1].trim();
      } catch {
        // 继续检查下一个已知注册表入口。
      }
    }
    return undefined;
  }
  for (const executable of ["google-chrome", "chromium", "microsoft-edge"]) {
    try {
      const path = execFileSync("which", [executable], { encoding: "utf8" }).trim();
      if (path) return path;
    } catch {
      // 继续检查下一个命令。
    }
  }
  return undefined;
}

export class CdpBrowserSession {
  readonly id: string;
  readonly config: BrowserSessionConfig;
  readonly #child: ChildProcess;
  readonly #connection: CdpConnection;
  readonly #targetId: string;
  readonly #sessionId: string;
  readonly #refs = new Map<string, number>();
  readonly #actions: BrowserActionRecord[] = [];

  private constructor(input: {
    config: BrowserSessionConfig;
    child: ChildProcess;
    connection: CdpConnection;
    targetId: string;
    sessionId: string;
  }) {
    this.id = input.config.id;
    this.config = input.config;
    this.#child = input.child;
    this.#connection = input.connection;
    this.#targetId = input.targetId;
    this.#sessionId = input.sessionId;
  }

  static async launch(config: BrowserSessionConfig): Promise<CdpBrowserSession> {
    mkdirSync(config.profileDirectory, { recursive: true });
    const devToolsPortFile = join(config.profileDirectory, "DevToolsActivePort");
    if (existsSync(devToolsPortFile)) unlinkSync(devToolsPortFile);
    const args = [
      "--remote-debugging-port=0",
      `--user-data-dir=${config.profileDirectory}`,
      "--no-first-run",
      "--no-default-browser-check",
      "--disable-background-networking",
      "--disable-sync",
      "--disable-component-update",
      "--disable-features=Translate",
      ...(config.headless
        ? [
            "--headless=new",
            "--hide-scrollbars",
            "--disable-gpu",
            "--disable-gpu-compositing",
            "--disable-gpu-shader-disk-cache",
          ]
        : []),
      "about:blank",
    ];
    const child = spawn(config.executablePath, args, {
      stdio: ["ignore", "ignore", "pipe"],
      windowsHide: true,
      shell: false,
    });
    let stderrTail = "";
    child.stderr?.setEncoding("utf8");
    child.stderr?.on("data", (chunk: string) => {
      stderrTail = `${stderrTail}${chunk}`.slice(-8_192);
    });
    try {
      const endpoint = await waitForDevToolsPort(config.profileDirectory, child);
      const connection = await CdpConnection.connect(`ws://127.0.0.1:${endpoint.port}${endpoint.browserPath}`);
      const targetResult = (await connection.send("Target.createTarget", { url: "about:blank" })) as { targetId?: string };
      if (!targetResult.targetId) throw new Error("cdp-target-id-missing");
      const attachResult = (await connection.send("Target.attachToTarget", {
        targetId: targetResult.targetId,
        flatten: true,
      })) as { sessionId?: string };
      if (!attachResult.sessionId) throw new Error("cdp-session-id-missing");
      await Promise.all([
        connection.send("Page.enable", {}, attachResult.sessionId),
        connection.send("DOM.enable", {}, attachResult.sessionId),
        connection.send("Accessibility.enable", {}, attachResult.sessionId),
      ]);
      return new CdpBrowserSession({
        config,
        child,
        connection,
        targetId: targetResult.targetId,
        sessionId: attachResult.sessionId,
      });
    } catch (error) {
      child.kill();
      throw new Error(
        `browser-launch-failed: ${error instanceof Error ? error.message : String(error)}; exit=${child.exitCode}; stderr=${stderrTail}`,
        { cause: error },
      );
    }
  }

  listActions(): BrowserActionRecord[] {
    return [...this.#actions];
  }

  async navigate(url: string): Promise<void> {
    const parsed = new URL(url);
    if (!this.config.allowedOrigins.includes(parsed.origin)) {
      this.record("navigate", "failed", url, parsed.origin, "browser-origin-not-allowed");
      throw new Error(`browser-origin-not-allowed: ${parsed.origin}`);
    }
    try {
      const loaded = this.#connection.waitForEvent("Page.loadEventFired", this.#sessionId, 15_000);
      const result = (await this.#connection.send("Page.navigate", { url }, this.#sessionId, 15_000)) as {
        errorText?: string;
      };
      if (result.errorText) throw new Error(`browser-navigation-failed: ${result.errorText}`);
      await loaded;
      this.record("navigate", "completed", url, parsed.origin);
    } catch (error) {
      this.record("navigate", "failed", url, parsed.origin, error instanceof Error ? error.message : String(error));
      throw error;
    }
  }

  async snapshot(): Promise<BrowserSnapshot> {
    try {
      const result = (await this.#connection.send("Accessibility.getFullAXTree", {}, this.#sessionId)) as {
        nodes?: Array<{
          role?: unknown;
          name?: unknown;
          description?: unknown;
          backendDOMNodeId?: number;
          ignored?: boolean;
        }>;
      };
      const pageInfo = (await this.#connection.send(
        "Runtime.evaluate",
        { expression: "({url: location.href, title: document.title})", returnByValue: true },
        this.#sessionId,
      )) as { result?: { value?: { url?: string; title?: string } } };
      this.#refs.clear();
      let nextRef = 1;
      const nodes: BrowserSnapshotNode[] = [];
      for (const node of result.nodes ?? []) {
        if (node.ignored) continue;
        const role = valueOfAxProperty(node.role);
        const name = valueOfAxProperty(node.name);
        const description = valueOfAxProperty(node.description);
        if (!role || (!name && !interactiveRoles.has(role))) continue;
        const ref = interactiveRoles.has(role) && node.backendDOMNodeId ? `e${nextRef++}` : undefined;
        if (ref && node.backendDOMNodeId) this.#refs.set(ref, node.backendDOMNodeId);
        nodes.push({
          ...(ref ? { ref } : {}),
          role,
          name,
          ...(description ? { description } : {}),
        });
        if (nodes.length >= 500) break;
      }
      this.record("snapshot", "completed");
      return {
        url: pageInfo.result?.value?.url ?? "",
        title: pageInfo.result?.value?.title ?? "",
        nodes,
      };
    } catch (error) {
      this.record("snapshot", "failed", undefined, undefined, error instanceof Error ? error.message : String(error));
      throw error;
    }
  }

  async click(ref: string): Promise<void> {
    try {
      const point = await this.pointForRef(ref);
      await this.#connection.send("Input.dispatchMouseEvent", { type: "mousePressed", x: point.x, y: point.y, button: "left", clickCount: 1 }, this.#sessionId);
      await this.#connection.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: point.x, y: point.y, button: "left", clickCount: 1 }, this.#sessionId);
      this.record("click", "completed", ref);
    } catch (error) {
      this.record("click", "failed", ref, undefined, error instanceof Error ? error.message : String(error));
      throw error;
    }
  }

  async type(ref: string, text: string): Promise<void> {
    try {
      await this.click(ref);
      await this.#connection.send("Input.insertText", { text }, this.#sessionId);
      this.record("type", "completed", ref);
    } catch (error) {
      this.record("type", "failed", ref, undefined, error instanceof Error ? error.message : String(error));
      throw error;
    }
  }

  async screenshot(): Promise<BrowserArtifactRef> {
    try {
      const result = (await this.#connection.send(
        "Page.captureScreenshot",
        { format: "png", fromSurface: true, captureBeyondViewport: false },
        this.#sessionId,
        15_000,
      )) as { data?: string };
      if (!result.data) throw new Error("browser-screenshot-data-missing");
      const artifact = await this.config.artifactStore.savePng(this.id, result.data);
      this.record("screenshot", "completed", artifact.id);
      return artifact;
    } catch (error) {
      this.record("screenshot", "failed", undefined, undefined, error instanceof Error ? error.message : String(error));
      throw error;
    }
  }

  async close(): Promise<void> {
    try {
      await this.#connection.send("Target.closeTarget", { targetId: this.#targetId });
      await this.#connection.send("Browser.close");
    } catch {
      // 浏览器可能已经退出；继续释放 WebSocket/进程。
    }
    await this.#connection.close();
    if (this.#child.exitCode === null) this.#child.kill();
  }

  private async pointForRef(ref: string): Promise<{ x: number; y: number }> {
    const backendNodeId = this.#refs.get(ref);
    if (!backendNodeId) throw new Error(`browser-ref-not-found: ${ref}; take a new snapshot`);
    const box = (await this.#connection.send(
      "DOM.getBoxModel",
      { backendNodeId },
      this.#sessionId,
    )) as { model?: { content?: number[]; border?: number[] } };
    const quad = box.model?.content ?? box.model?.border;
    if (!quad || quad.length < 8) throw new Error(`browser-node-has-no-box: ${ref}`);
    const xs = [quad[0], quad[2], quad[4], quad[6]].map(Number);
    const ys = [quad[1], quad[3], quad[5], quad[7]].map(Number);
    return {
      x: xs.reduce((sum, value) => sum + value, 0) / xs.length,
      y: ys.reduce((sum, value) => sum + value, 0) / ys.length,
    };
  }

  private record(
    action: BrowserActionRecord["action"],
    status: BrowserActionRecord["status"],
    target?: string,
    origin?: string,
    error?: string,
  ): void {
    this.#actions.push({
      sequence: this.#actions.length + 1,
      action,
      ...(target ? { target } : {}),
      ...(origin ? { origin } : {}),
      status,
      recordedAt: new Date().toISOString(),
      ...(error ? { error } : {}),
    });
  }
}

export interface StructuredBrowserController {
  navigate(url: string): Promise<void>;
  snapshot(): Promise<BrowserSnapshot>;
  click(ref: string): Promise<void>;
  type(ref: string, text: string): Promise<void>;
  screenshot(): Promise<BrowserArtifactRef>;
}

function objectArgs(value: unknown, tool: string): Record<string, unknown> {
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`invalid-${tool}-args`);
  }
  return value as Record<string, unknown>;
}

/**
 * Agent 只看到这五个结构化 Tool；CdpConnection/send/Runtime.evaluate 都不会注册到 Catalog。
 */
export function contributeBrowserTools(
  registry: ToolRegistry,
  controller: StructuredBrowserController,
  currentOrigin: () => string,
): () => void {
  const disposers: Array<() => void> = [];
  const register = <Args, Result>(
    descriptor: ToolDescriptor<Args, Result>,
    provider: ToolProvider<Args, Result>,
  ): void => {
    disposers.push(registry.register(descriptor, provider));
  };

  register(
    {
      canonicalName: "browser.navigate",
      version: "1",
      description: "导航到权限允许的 URL",
      effectClass: "verifiable-effect",
      validateArgs(value) {
        const args = objectArgs(value, "browser-navigate");
        if (typeof args.url !== "string") throw new Error("browser-navigate-url-required");
        return { url: args.url };
      },
      validateResult(value) {
        if (value !== null) throw new Error("browser-navigate-result-must-be-null");
        return null;
      },
      permissionAction(args) {
        return { kind: "browser.open", origin: new URL(args.url).origin };
      },
    },
    { async execute({ args }) { await controller.navigate(args.url); return null; } },
  );

  register(
    {
      canonicalName: "browser.snapshot",
      version: "1",
      description: "读取经过过滤的无障碍树快照",
      effectClass: "read-only-retryable",
      validateArgs(value) { objectArgs(value, "browser-snapshot"); return {}; },
      validateResult(value) { return value as BrowserSnapshot; },
      permissionAction() { return { kind: "browser.snapshot", origin: currentOrigin() }; },
    },
    { execute: () => controller.snapshot() },
  );

  for (const action of ["click", "type"] as const) {
    register(
      {
        canonicalName: `browser.${action}`,
        version: "1",
        description: action === "click" ? "点击最新 Snapshot 的元素 ref" : "向最新 Snapshot 的可编辑元素输入非敏感文本",
        effectClass: "verifiable-effect",
        validateArgs(value) {
          const args = objectArgs(value, `browser-${action}`);
          if (typeof args.ref !== "string") throw new Error(`browser-${action}-ref-required`);
          if (action === "type" && typeof args.text !== "string") throw new Error("browser-type-text-required");
          return action === "type" ? { ref: args.ref, text: args.text as string } : { ref: args.ref };
        },
        validateResult(value) {
          if (value !== null) throw new Error(`browser-${action}-result-must-be-null`);
          return null;
        },
        permissionAction() { return { kind: "browser.act", origin: currentOrigin(), action }; },
      },
      {
        async execute({ args }) {
          if (action === "click") await controller.click(args.ref);
          else await controller.type(args.ref, (args as { ref: string; text: string }).text);
          return null;
        },
      },
    );
  }

  register(
    {
      canonicalName: "browser.screenshot",
      version: "1",
      description: "保存当前视口截图为 Artifact",
      effectClass: "read-only-retryable",
      validateArgs(value) { objectArgs(value, "browser-screenshot"); return {}; },
      validateResult(value) { return value as BrowserArtifactRef; },
      permissionAction() { return { kind: "browser.snapshot", origin: currentOrigin() }; },
    },
    { execute: () => controller.screenshot() },
  );

  return () => {
    for (const dispose of disposers.reverse()) dispose();
  };
}
