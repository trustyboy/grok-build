"""使用隔离目录和本地假上游验证模型发现及视觉命令的 ACP 链路。"""
import argparse
import base64
import struct
import zlib
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

REQUESTS = []
def png_chunk(kind, data):
    """生成有效 CRC，避免测试依赖外部图片文件。"""
    return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))


PNG = base64.b64encode(
    b"\x89PNG\r\n\x1a\n"
    + png_chunk(b"IHDR", struct.pack("!2I5B", 32, 32, 8, 2, 0, 0, 0))
    + png_chunk(b"IDAT", zlib.compress((b"\x00" + b"\xff\x00\x00" * 32) * 32))
    + png_chunk(b"IEND", b"")
).decode()


class Upstream(BaseHTTPRequestHandler):
    """仅提供模型列表和不调用工具的流式文本响应。"""
    def log_message(self, *_args):
        pass

    def do_GET(self):
        REQUESTS.append((self.path, dict(self.headers), None))
        body = json.dumps({"data": [{"id": "discovered-fixture", "baseUrl": "https://ignored.invalid"}]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        REQUESTS.append((self.path, dict(self.headers), payload))
        chunks = [
            {"id": "fixture", "object": "chat.completion.chunk", "created": 0, "model": payload.get("model"), "choices": [{"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": None}]},
            {"id": "fixture", "object": "chat.completion.chunk", "created": 0, "model": payload.get("model"), "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11}},
        ]
        body = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Rpc:
    """消费通知并给不需要的客户端请求返回明确错误。"""
    def __init__(self, process):
        self.process = process
        self.messages = queue.Queue()
        self.counter = 0
        self.notifications = []
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except ValueError:
                continue
        self.messages.put(None)

    def call(self, method, params, expect_error=False):
        self.counter += 1
        call_id = self.counter
        self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": call_id, "method": method, "params": params}) + "\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            message = self.messages.get(timeout=max(0.1, deadline - time.monotonic()))
            assert message is not None, "agent exited"
            if "method" in message:
                self.notifications.append(message)
                if "id" in message:
                    self.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": message["id"], "error": {"code": -32601, "message": "fixture has no client tools"}}) + "\n")
                    self.process.stdin.flush()
                continue
            if message.get("id") == call_id:
                if expect_error:
                    assert "error" in message, message
                    return message["error"]
                assert "error" not in message, message
                return message["result"]
        raise AssertionError("RPC timed out: " + method)

    def ext(self, name, params, expect_error=False):
        result = self.call("_" + name, params, expect_error)
        return result if expect_error else result["result"]


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe")
    args = parser.parse_args()
    exe = str(Path(args.exe).resolve())
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="grok-model-smoke-") as root:
        home = Path(root) / "home"
        cwd = Path(root) / "work"
        home.mkdir()
        cwd.mkdir()
        config = f'''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "source-entry"
[model.source-entry]
model = "source-wire"
base_url = "http://127.0.0.1:{server.server_port}/v1"
api_key = "fixture-only-key"
api_backend = "chat_completions"
context_window = 32000
supports_vision = true
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''
        (home / "config.toml").write_text(config, encoding="utf-8")
        env = {k: v for k, v in os.environ.items() if not k.startswith(("GROK_", "XAI_"))}
        env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
                   APPDATA=str(Path(root) / "AppData" / "Roaming"),
                   LOCALAPPDATA=str(Path(root) / "AppData" / "Local"),
                   XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
                   GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false")
        stderr = open(Path(root) / "stderr.log", "w+", encoding="utf-8")
        process = subprocess.Popen([exe, "agent", "--no-leader", "stdio"], cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8")
        try:
            rpc = Rpc(process)
            rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False}, "_meta": {"clientType": "grok-pager", "startupHints": {"nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}}})
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            a = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "source-entry"}})["sessionId"]
            b = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "source-entry"}})["sessionId"]
            discover = {"sessionId": a, "modelId": "discovered-fixture", "expectedModelId": "source-entry"}
            result = rpc.ext("x.ai/models/discover_and_switch", discover)
            assert result["models"]["currentModelId"] == "discovered-fixture", result
            entry = next(m for m in result["models"]["availableModels"] if m["modelId"] == "discovered-fixture")
            assert entry["_meta"]["runtimeDiscovered"] is True
            assert entry["_meta"]["totalContextTokens"] == 256000
            assert rpc.ext("x.ai/session/support_vision", {"sessionId": a})["enabled"] is False
            assert rpc.ext("x.ai/session/support_vision", {"sessionId": b})["enabled"] is True
            rpc.ext("x.ai/session/support_vision", {"sessionId": a, "expectedModelId": "discovered-fixture", "enabled": True})
            rpc.call("session/set_model", {"sessionId": a, "modelId": "source-entry"})
            rpc.call("session/set_model", {"sessionId": a, "modelId": "discovered-fixture"})
            assert rpc.ext("x.ai/session/support_vision", {"sessionId": a})["enabled"] is True
            rpc.ext("x.ai/session/support_vision", {"sessionId": a, "enabled": False})
            rpc.ext("x.ai/models/discover_and_switch", {"sessionId": a, "modelId": "missing-fixture", "expectedModelId": "discovered-fixture"}, expect_error=True)
            assert rpc.ext("x.ai/session/support_vision", {"sessionId": a})["modelId"] == "discovered-fixture"
            for enabled in (False, True):
                rpc.ext("x.ai/session/support_vision", {"sessionId": a, "enabled": enabled})
                before = len(REQUESTS)
                rpc.call("session/prompt", {"sessionId": a, "prompt": [{"type": "text", "text": "Reply only ok, do not call tools."}, {"type": "image", "data": PNG, "mimeType": "image/png"}]})
                posted = [r for r in REQUESTS[before:] if r[2] and r[2].get("model") == "discovered-fixture"]
                assert posted, "No inference for discovered model"
                path, headers, body = posted[0]
                assert headers.get("authorization", headers.get("Authorization")) == "Bearer fixture-only-key", headers
                assert "reasoning_effort" not in body
                user_content = [m.get("content") for m in body.get("messages", []) if m.get("role") == "user"]
                assert ("image_url" in json.dumps(body)) == enabled, (enabled, str(user_content[-1:])[-2500:])
            saved = tomllib.loads((home / "config.toml").read_text(encoding="utf-8"))
            original = tomllib.loads(config)
            assert saved["models"] == original["models"], "Default model config changed"
            assert saved["model"] == original["model"], "Runtime model or vision override persisted"
            lists = [r for r in REQUESTS if r[2] is None and r[0].split("?")[0] == "/v1/models"]
            assert lists and all(r[1].get("authorization", r[1].get("Authorization")) == "Bearer fixture-only-key" for r in lists), [(r[0], r[1].get("authorization", r[1].get("Authorization"))) for r in lists]
            print("PASS: discovery, defaults, two-session vision isolation, switching, miss rollback, two-turn auth and image filtering, unchanged model config")
        except Exception:
            stderr.flush()
            stderr.seek(0)
            print(stderr.read()[-12000:])
            raise
        finally:
            process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            stderr.close()
    server.shutdown()


if __name__ == "__main__":
    main()
