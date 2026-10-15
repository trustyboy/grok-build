"""使用隔离配置和本地假上游验证 Fast 配置、会话开关及真实请求载荷。"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import tomllib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


# 复用模型发现回归的 ACP 客户端，统一通知和反向请求处理。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "model_discovery_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)


class Upstream(BaseHTTPRequestHandler):
    """记录推理请求并返回无工具调用的固定流式回复。"""
    def log_message(self, *_args):
        """测试只输出断言结果。"""
        pass

    def respond(self, body, content_type="application/json"):
        """发送带长度的响应以避免客户端等待连接关闭。"""
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """模型目录由本地配置提供。"""
        self.respond(b'{"data":[]}')

    def do_POST(self):
        """同时覆盖 Responses 和 Chat Completions 的实际传输。"""
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append((self.path, payload))
        if self.path.endswith("/responses"):
            response = {
                "id": "resp_fixture", "object": "response", "created_at": 1,
                "model": payload["model"], "status": "completed",
                "output": [{
                    "id": "msg_fixture", "type": "message", "role": "assistant",
                    "status": "completed", "content": [{"type": "output_text", "text": "ok", "annotations": []}],
                }],
                "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                          "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 11},
            }
            if payload.get("stream"):
                events = [{"type": "response.completed", "sequence_number": 0, "response": response}]
                body = "".join("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n" for event in events)
                self.respond(body.encode(), "text/event-stream")
            else:
                self.respond(json.dumps(response).encode())
        else:
            chunks = [{
                "id": "fixture", "object": "chat.completion.chunk", "created": 1,
                "model": payload["model"], "choices": [{
                    "index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": "stop",
                }],
            }]
            body = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
            self.respond(body.encode(), "text/event-stream")


def make_config(port):
    """省略、显式关闭、持久开启和非 Responses 四种配置。"""
    config = '''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "omitted"
allowed_models = ["omitted", "off", "on", "chat"]
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''
    for name, backend, fast in [("omitted", "responses", None), ("off", "responses", False), ("on", "responses", True), ("chat", "chat_completions", True)]:
        config += f'''
[model.{name}]
model = "{name}-wire"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "fixture-key"
api_backend = "{backend}"
context_window = 32000
'''
        if fast is not None:
            config += f"fast_mode = {str(fast).lower()}\n"
    return config


def run_checks(exe, root, server):
    """启动真实 Release 后端，所有修改限定在临时配置和会话。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    config = make_config(server.server_port)
    config_path = home / "config.toml"
    config_path.write_text(config, encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith(("GROK_", "XAI_"))}
    env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
               APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
               XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
               GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false")
    with (root / "stderr.log").open("w+", encoding="utf-8") as stderr:
        process = subprocess.Popen([str(exe), "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                                   text=True, encoding="utf-8")
        try:
            rpc = FIXTURE.Rpc(process)
            rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                                   "_meta": {"clientType": "grok-pager", "startupHints": {
                                       "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}}})
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            first, second = [rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
                                                     "_meta": {"modelId": "omitted"}})["sessionId"] for _ in range(2)]

            def state(action="status", session=first, **params):
                """读取或更新指定会话的 Fast 状态。"""
                return rpc.ext("x.ai/session/fast_mode", {"sessionId": session, "action": action, **params})

            def select(model, session=first):
                """通过正式模型切换入口应用配置默认值。"""
                rpc.call("session/set_model", {"sessionId": session, "modelId": model})

            def prompt(model, enabled, session=first):
                """验证本轮所有该模型请求的 service_tier，避免只检查状态提示。"""
                server.requests.clear()
                result = rpc.call("session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": "回复 ok"}]})
                assert result["stopReason"] == "end_turn", result
                requests = [(path, body) for path, body in server.requests if body.get("model") == model + "-wire"]
                assert requests, server.requests
                for path, body in requests:
                    assert path.endswith("/chat/completions" if model == "chat" else "/responses"), path
                    if enabled:
                        assert body.get("service_tier") == "priority", body
                    else:
                        assert "service_tier" not in body, body

            assert state() == {"modelId": "omitted", "enabled": False, "supported": True}
            prompt("omitted", False)
            assert state("toggle", expectedModelId="omitted")["enabled"]
            prompt("omitted", True)
            assert not state(session=second)["enabled"]
            prompt("omitted", False, second)
            assert not state("off")["enabled"]
            prompt("omitted", False)
            select("on")
            assert state()["enabled"]
            prompt("on", True)
            assert not state("toggle")["enabled"]
            prompt("on", False)
            select("off")
            assert not state()["enabled"]
            assert state("on")["enabled"]
            select("on")
            assert not state()["enabled"]
            select("off")
            assert state()["enabled"]
            prompt("off", True)
            select("on", second)
            assert state(session=second)["enabled"]
            select("chat")
            assert state() == {"modelId": "chat", "enabled": False, "supported": False}
            rpc.ext("x.ai/session/fast_mode", {"sessionId": first, "action": "on"}, expect_error=True)
            prompt("chat", False)
            select("omitted")
            rpc.ext("x.ai/session/fast_mode", {"sessionId": first, "action": "on", "expectedModelId": "chat"}, expect_error=True)
            assert not state()["enabled"]
            # 登录初始化可能保存其他设置；逐项验证模型配置及默认选择保持不变。
            saved = tomllib.loads(config_path.read_text(encoding="utf-8"))
            original = tomllib.loads(config)
            assert saved["model"] == original["model"], ("会话开关修改了模型配置", saved["model"])
            assert saved["models"] == original["models"], ("会话开关修改了默认模型", saved["models"])
            print("PASS: Fast 默认值、持久配置、切换、会话隔离、模型切换、接口限制、过期请求及真实载荷", flush=True)
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
            process.stdin.close()
            process.stdout.close()


def main():
    """只使用已构建的 Release 程序，结束时清理临时目录和服务器。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请先构建 Release 程序：" + str(exe))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="grok-fast-smoke-", dir=tempfile.gettempdir()) as root:
            run_checks(exe, Path(root), server)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
