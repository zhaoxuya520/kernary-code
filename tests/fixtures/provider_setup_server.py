import argparse
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def _authorized(self) -> bool:
        return self.headers.get("Authorization") == "Bearer setup-key"

    def _json(self, status: int, payload: dict) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if self.path != "/v1/models":
            self._json(404, {"error": "not-found"})
            return
        if not self._authorized():
            self._json(401, {"error": "unauthorized"})
            return
        self._json(
            200,
            {
                "object": "list",
                "data": [
                    {"id": "coder-small"},
                    {"id": "coder-large"},
                ],
            },
        )

    def do_POST(self) -> None:
        if self.path != "/v1/embeddings":
            self._json(404, {"error": "not-found"})
            return
        if not self._authorized():
            self._json(401, {"error": "unauthorized"})
            return
        length = int(self.headers.get("Content-Length", "0"))
        payload = json.loads(self.rfile.read(length) or b"{}")
        if payload.get("model") != "embed-test":
            self._json(400, {"error": "unknown-model"})
            return
        self._json(
            200,
            {
                "object": "list",
                "data": [{"index": 0, "embedding": [1.0, 0.0, 0.5, -0.25]}],
            },
        )

    def log_message(self, _format: str, *_args: object) -> None:
        return


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, required=True)
    args = parser.parse_args()
    ThreadingHTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
