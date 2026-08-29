import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync } from "node:fs";
import { createServer } from "node:http";
import { join } from "node:path";
import test from "node:test";
import {
  CdpBrowserSession,
  FileBrowserArtifactStore,
  discoverInstalledBrowser,
} from "../packages/browser-runtime/src/cdp-browser.ts";

test("独立 CDP Browser Session 支持 snapshot/ref/click/type/screenshot 且限制 Origin", async (context) => {
  if (process.env.HARNESS_BROWSER_E2E !== "1") {
    context.skip("设置 HARNESS_BROWSER_E2E=1 后运行隔离浏览器 E2E");
    return;
  }
  const executablePath = discoverInstalledBrowser();
  if (!executablePath) {
    context.skip("本机没有通过注册表/which 发现 Chromium 浏览器");
    return;
  }

  const server = createServer((_request, response) => {
    response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    response.end(`<!doctype html><html><head><title>Browser Worker Test</title></head><body>
      <h1 id="status">idle</h1>
      <label>Message <input aria-label="Message" id="message" /></label>
      <button id="apply" onclick="document.querySelector('#status').textContent=document.querySelector('#message').value">Apply</button>
    </body></html>`);
  });
  await new Promise<void>((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("test-server-address-missing");
  const origin = `http://127.0.0.1:${address.port}`;
  const tempRoot = join(process.cwd(), "output", "test-temp");
  mkdirSync(tempRoot, { recursive: true });
  const profileDirectory = mkdtempSync(join(tempRoot, "browser-profile-"));
  const artifacts = new FileBrowserArtifactStore(join(profileDirectory, "artifacts"));
  let browser: CdpBrowserSession | undefined;
  let stage = "launch";

  try {
    browser = await CdpBrowserSession.launch({
      id: "browser:test",
      executablePath,
      profileDirectory,
      headless: true,
      allowedOrigins: [origin],
      artifactStore: artifacts,
    });
    stage = "navigate";
    await browser.navigate(`${origin}/`);
    stage = "snapshot-before";
    const before = await browser.snapshot();
    assert.equal(before.title, "Browser Worker Test");
    const textbox = before.nodes.find((node) => node.role === "textbox" && node.name === "Message");
    const button = before.nodes.find((node) => node.role === "button" && node.name === "Apply");
    assert.ok(textbox?.ref);
    assert.ok(button?.ref);

    stage = "type";
    await browser.type(textbox.ref, "Harness Browser");
    stage = "click";
    await browser.click(button.ref);
    stage = "snapshot-after";
    const after = await browser.snapshot();
    assert.ok(after.nodes.some((node) => node.role === "heading" && node.name === "Harness Browser"));

    stage = "screenshot";
    const screenshot = await browser.screenshot();
    assert.ok(screenshot.bytes > 1000);
    assert.match(screenshot.path, /browser-test-browser-shot-.*\.png$/);

    stage = "origin-deny";
    await assert.rejects(browser.navigate("https://example.com/"), /browser-origin-not-allowed/);
    const actions = browser.listActions();
    assert.ok(actions.some((action) => action.action === "navigate" && action.status === "failed"));
    assert.ok(actions.every((action, index) => action.sequence === index + 1));
  } catch (error) {
    throw new Error(`browser-test-stage=${stage}: ${error instanceof Error ? error.message : String(error)}`, { cause: error });
  } finally {
    await browser?.close();
    await new Promise<void>((resolveClose, rejectClose) =>
      server.close((error) => (error ? rejectClose(error) : resolveClose())),
    );
  }
});
