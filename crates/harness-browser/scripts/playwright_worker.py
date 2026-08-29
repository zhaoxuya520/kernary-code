# 受控 Playwright Worker：stdin/stdout 只传 JSONL，不接受任意脚本或原始 CDP。
import hashlib
import json
import os
import re
import sys
import time
import uuid
from pathlib import Path
from urllib.parse import urlparse

from playwright.sync_api import sync_playwright


def emit(message):
    sys.stdout.write(json.dumps(message, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def origin(value):
    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https") or not parsed.hostname:
        raise RuntimeError("browser-origin-invalid")
    host = f"[{parsed.hostname}]" if ":" in parsed.hostname else parsed.hostname
    default = (parsed.scheme == "http" and parsed.port in (None, 80)) or (
        parsed.scheme == "https" and parsed.port in (None, 443)
    )
    return f"{parsed.scheme}://{host}" if default else f"{parsed.scheme}://{host}:{parsed.port}"


class Worker:
    def __init__(self):
        self.playwright = None
        self.context = None
        self.page = None
        self.allowed_origins = set()
        self.artifact_directory = None
        self.download_directory = None
        self.allow_uploads = False
        self.allow_downloads = False
        self.timeout = 10000
        self.generation = 0
        # ref 只保存在受控 Worker 内存中，绝不写回不可信页面 DOM。
        self.refs = {}

    def open(self, config):
        self.allowed_origins = set(config["allowedOrigins"])
        self.artifact_directory = Path(config["artifactDirectory"]).resolve()
        self.download_directory = Path(config["downloadDirectory"]).resolve()
        self.artifact_directory.mkdir(parents=True, exist_ok=True)
        self.download_directory.mkdir(parents=True, exist_ok=True)
        self.allow_uploads = bool(config.get("allowUploads", False))
        self.allow_downloads = bool(config.get("allowDownloads", False))
        self.timeout = int(config.get("timeoutMillis", 10000))
        self.playwright = sync_playwright().start()
        self.context = self.playwright.chromium.launch_persistent_context(
            user_data_dir=str(Path(config["profileDirectory"]).resolve()),
            executable_path=str(Path(config["browserExecutable"]).resolve()),
            headless=bool(config.get("headless", True)),
            accept_downloads=self.allow_downloads,
            downloads_path=str(self.download_directory),
            args=[
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-component-update",
                "--disable-features=Translate",
            ],
        )
        self.context.set_default_timeout(self.timeout)
        self.context.set_default_navigation_timeout(self.timeout)
        self.context.route("**/*", self.route_request)
        self.page = self.context.pages[0] if self.context.pages else self.context.new_page()
        return {"kind": "unit"}

    def route_request(self, route):
        url = route.request.url
        if url.startswith(("about:", "data:", "blob:")):
            route.continue_()
            return
        try:
            allowed = origin(url) in self.allowed_origins
        except Exception:
            allowed = False
        route.continue_() if allowed else route.abort("blockedbyclient")

    def locator(self, ref_id):
        if not re.fullmatch(r"e[1-9][0-9]{0,5}", ref_id):
            raise RuntimeError("browser-ref-invalid")
        element = self.refs.get(ref_id)
        if element is None:
            raise RuntimeError("browser-ref-not-found-take-new-snapshot")
        try:
            if not element.evaluate("el => el.isConnected"):
                raise RuntimeError("browser-ref-not-found-take-new-snapshot")
        except Exception as error:
            raise RuntimeError("browser-ref-not-found-take-new-snapshot") from error
        return element

    def clear_refs(self):
        # ElementHandle 属于上一代快照；主动释放，避免长会话持有失效 DOM。
        for element in self.refs.values():
            try:
                element.dispose()
            except Exception:
                pass
        self.refs = {}

    def execute(self, command):
        kind = command["kind"]
        if kind == "navigate":
            target = command["url"]
            target_origin = origin(target)
            if target_origin not in self.allowed_origins:
                raise RuntimeError(f"browser-origin-not-allowed:{target_origin}")
            self.page.goto(target, wait_until="load")
            self.clear_refs()
            self.generation += 1
            return {"kind": "unit"}
        if kind == "snapshot":
            self.clear_refs()
            self.generation += 1
            nodes = []
            selector = "button,a,input,textarea,select,[role],h1,h2,h3,h4,h5,h6"
            interactive = {
                "button", "link", "textbox", "searchbox", "checkbox", "radio",
                "combobox", "listbox", "menuitem", "tab", "switch", "slider",
            }
            candidates = self.page.locator(selector)
            for index in range(min(candidates.count(), 500)):
                element = candidates.nth(index).element_handle()
                if element is None:
                    continue
                node = element.evaluate(
                    """el => {
                    const tag = el.tagName.toLowerCase();
                    let role = el.getAttribute('role') || ({a:'link',button:'button',textarea:'textbox',select:'combobox',h1:'heading',h2:'heading',h3:'heading',h4:'heading',h5:'heading',h6:'heading'}[tag]);
                    if (!role && tag === 'input') {
                      const type = (el.getAttribute('type') || 'text').toLowerCase();
                      role = ({checkbox:'checkbox',radio:'radio',search:'searchbox',range:'slider'}[type] || 'textbox');
                    }
                    role = role || 'generic';
                    const inputType = (el.getAttribute('type') || '').toLowerCase();
                    const autocomplete = (el.getAttribute('autocomplete') || '').toLowerCase();
                    const sensitive = tag === 'input' && (inputType === 'password' || ['current-password','new-password','one-time-code'].includes(autocomplete));
                    const name = el.getAttribute('aria-label') || el.getAttribute('alt') || el.getAttribute('placeholder') || (el.innerText || '').trim();
                    const description = el.getAttribute('aria-description') || el.getAttribute('title') || null;
                    return {role, name: String(name || '').slice(0, 2048), description: description ? String(description).slice(0, 2048) : null, sensitive};
                  }"""
                )
                ref_id = None
                if node["role"] in interactive and not node["sensitive"]:
                    ref_id = f"e{len(self.refs) + 1}"
                    self.refs[ref_id] = element
                node["refId"] = ref_id
                if node["role"] and (node["name"] or ref_id):
                    nodes.append(node)
            return {
                "kind": "snapshot",
                "snapshot": {
                    "url": self.page.url,
                    "title": self.page.title(),
                    "generation": self.generation,
                    "nodes": nodes,
                },
            }
        if kind == "click":
            self.locator(command["refId"]).click()
            return {"kind": "unit"}
        if kind == "type":
            self.locator(command["refId"]).fill(command["text"])
            return {"kind": "unit"}
        if kind == "read":
            locator = self.locator(command["refId"])
            element_type = (locator.get_attribute("type") or "").lower()
            if element_type == "password":
                raise RuntimeError("browser-secret-read-denied")
            text = locator.input_value() if locator.evaluate("el => ['INPUT','TEXTAREA','SELECT'].includes(el.tagName)") else locator.inner_text()
            return {"kind": "text", "text": str(text)[:1000000]}
        if kind == "inspect":
            ref_id = command["refId"]
            result = self.locator(ref_id).evaluate(
                """(el, refId) => {
                  const allowed = ['id','name','type','role','aria-label','aria-description','placeholder','disabled','checked','selected'];
                  const attributes = {};
                  for (const key of allowed) if (el.hasAttribute(key)) attributes[key] = String(el.getAttribute(key)).slice(0, 2048);
                  const box = el.getBoundingClientRect();
                  return {refId, role: el.getAttribute('role') || el.tagName.toLowerCase(), name: el.getAttribute('aria-label') || (el.innerText || '').trim(), tag: el.tagName.toLowerCase(), attributes, bounds:[box.x,box.y,box.width,box.height]};
                }""",
                ref_id,
            )
            return {"kind": "inspect", "result": result}
        if kind == "wait":
            wait = command["wait"]
            if wait["kind"] == "millis":
                millis = min(int(wait["millis"]), self.timeout)
                self.page.wait_for_timeout(millis)
            elif wait["kind"] == "ref":
                self.locator(wait["refId"]).wait_for_element_state("visible")
            else:
                self.page.wait_for_load_state(wait["state"])
            return {"kind": "unit"}
        if kind == "screenshot":
            name = f"browser-shot-{int(time.time() * 1000)}-{uuid.uuid4().hex[:8]}.png"
            path = (self.artifact_directory / name).resolve()
            self.page.screenshot(path=str(path), full_page=False)
            return {"kind": "artifact", "artifact": self.artifact(path, "image/png")}
        if kind == "upload":
            if not self.allow_uploads:
                raise RuntimeError("browser-upload-disabled")
            self.locator(command["refId"]).set_input_files(command["path"])
            return {"kind": "unit"}
        if kind == "download":
            if not self.allow_downloads:
                raise RuntimeError("browser-download-disabled")
            with self.page.expect_download() as download_info:
                self.locator(command["refId"]).click()
            download = download_info.value
            safe_name = re.sub(r"[^A-Za-z0-9._-]", "_", download.suggested_filename)[:180] or "download.bin"
            path = (self.download_directory / f"{int(time.time() * 1000)}-{safe_name}").resolve()
            download.save_as(str(path))
            return {"kind": "artifact", "artifact": self.artifact(path, "application/octet-stream")}
        raise RuntimeError(f"browser-command-unsupported:{kind}")

    def artifact(self, path, mime_type):
        data = path.read_bytes()
        return {
            "id": path.stem,
            "path": str(path),
            "mimeType": mime_type,
            "bytes": len(data),
            "sha256": hashlib.sha256(data).hexdigest(),
        }

    def close(self):
        self.clear_refs()
        if self.context:
            self.context.close()
            self.context = None
        if self.playwright:
            self.playwright.stop()
            self.playwright = None
        return {"kind": "unit"}


worker = Worker()
for raw in sys.stdin:
    request = {}
    try:
        request = json.loads(raw)
        request_id = int(request["id"])
        action = request["action"]
        if action == "open":
            result = worker.open(request["payload"])
        elif action == "execute":
            result = worker.execute(request["payload"])
        elif action == "close":
            result = worker.close()
        else:
            raise RuntimeError("browser-worker-action-unsupported")
        emit({"id": request_id, "ok": True, "result": result})
        if action == "close":
            break
    except Exception as error:
        code = str(error).split(":", 1)[0] if str(error) else "browser-worker-error"
        emit({
            "id": request.get("id", 0) if isinstance(request, dict) else 0,
            "ok": False,
            "errorCode": code,
            "error": f"{type(error).__name__}: {str(error)[:1024]}",
        })
        if isinstance(request, dict) and request.get("action") == "open":
            break
