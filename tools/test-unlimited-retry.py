"""使用隔离配置和本地假上游验证 /retry 无限重试：持续 503 时按 1 秒间隔重试且耗尽预算仍不中断。"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


# 复用模型发现回归的 ACP 客户端，统一通知和反向请求处理。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "model_discovery_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)

# 默认采样预算为 15 次；超过它才能证明无限重试确实绕过了上限。
FAILURES = 20
# 固定节拍为 1 秒，只放宽到覆盖调度与传输抖动。
MIN_GAP = 0.5
MAX_GAP = 2.5


class Upstream(BaseHTTPRequestHandler):
    """前 N 次推理请求返回 503，之后返回固定流式回复，并记录每次时间。"""

    failures_remaining = FAILURES
    attempts = []

    def log_message(self, *_args):
        """测试只输出断言结果。"""
        pass

    def respond(self, status, body, content_type="application/json"):
        """发送带长度的响应以避免客户端等待连接关闭。"""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """模型目录由本地配置提供。"""
        self.respond(200, b'{"data":[]}')

    def do_POST(self):
        """覆盖 Responses 后端：先制造 503，再返回合法的完成事件。"""
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        if not self.path.endswith("/responses"):
            raise AssertionError("无限重试脚本只接受 Responses 请求：" + self.path)
        if self.server.failures_remaining > 0:
            self.server.failures_remaining -= 1
            self.server.attempts.append((time.monotonic(), False))
            body = json.dumps({
                "error": {"message": "upstream overloaded", "type": "service_unavailable_error"}
            }).encode()
            self.respond(503, body)
            return
        self.server.attempts.append((time.monotonic(), True))
        response = {
            "id": "resp_fixture", "object": "response", "created_at": 1,
            "model": payload["model"], "status": "completed",
            "output": [{
                "id": "msg_fixture", "type": "message", "role": "assistant",
                "status": "completed",
                "content": [{"type": "output_text", "text": "ok", "annotations": []}],
            }],
            "usage": {
                "input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 11,
            },
        }
        event = {"type": "response.completed", "sequence_number": 0, "response": response}
        body = "event: response.completed\ndata: " + json.dumps(event) + "\n\n"
        self.respond(200, body.encode(), "text/event-stream")


def make_config(port):
    """单个 Responses 模型，保留默认采样预算。"""
    return f'''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "flaky"
allowed_models = ["flaky"]
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false

[model.flaky]
model = "flaky-wire"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "fixture-key"
api_backend = "responses"
context_window = 32000
'''


def check_retry_intervals(attempts):
    """校验首次为失败、最后一次成功，且相邻请求落在 1 秒节拍附近。"""
    assert len(attempts) == FAILURES + 1, ("请求次数不符合预期", len(attempts))
    assert not attempts[0][1], "首次请求应为 503"
    assert attempts[-1][1], "最后一次请求应成功"
    gaps = [later[0] - earlier[0] for earlier, later in zip(attempts, attempts[1:])]
    out_of_range = [round(gap, 3) for gap in gaps if not MIN_GAP <= gap <= MAX_GAP]
    assert not out_of_range, ("重试间隔偏离 1 秒", out_of_range, [round(g, 3) for g in gaps])


def run_checks(exe, root, server):
    """启动真实 Release 后端，所有修改限定在临时配置和会话。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    config_path = home / "config.toml"
    config_path.write_text(make_config(server.server_port), encoding="utf-8")
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
                                                     "_meta": {"modelId": "flaky"}})["sessionId"] for _ in range(2)]

            def state(action="status", session=first):
                """读取或修改指定会话的无限重试状态。"""
                return rpc.ext("x.ai/session/unlimited_retry", {"sessionId": session, "action": action})

            # 默认关闭，且开关只作用于当前会话。
            assert state() == {"enabled": False, "intervalSecs": 1}, state()
            assert state("on")["enabled"]
            assert not state(session=second)["enabled"], "开关泄漏到了其他会话"
            rpc.ext("x.ai/session/unlimited_retry", {"sessionId": first, "action": "bogus"}, expect_error=True)

            started = time.monotonic()
            result = rpc.call("session/prompt", {"sessionId": first, "prompt": [{"type": "text", "text": "回复 ok"}]})
            elapsed = time.monotonic() - started

            assert result["stopReason"] == "end_turn", result
            check_retry_intervals(server.attempts)
            # 证明重试确实跨过了默认 15 次预算。
            assert elapsed >= (FAILURES - 1) * (MIN_GAP + 0.3), ("总耗时过短", elapsed)
            assert state("off") == {"enabled": False, "intervalSecs": 1}, "关闭后状态未复位"
            print("PASS: 无限重试绕过采样预算、固定 1 秒间隔、会话隔离及关闭复位", flush=True)
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
    global FAILURES
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe")
    parser.add_argument("--failures", type=int, default=FAILURES, help="首次成功前返回 503 的次数")
    args = parser.parse_args()
    FAILURES = args.failures
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请先构建 Release 程序：" + str(exe))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    server.failures_remaining = FAILURES
    server.attempts = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        with tempfile.TemporaryDirectory(prefix="grok-retry-smoke-", dir=tempfile.gettempdir()) as root:
            run_checks(exe, Path(root), server)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
