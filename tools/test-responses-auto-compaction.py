"""使用临时配置、本地假上游和真实 Release ACP 验证 Responses 原生自动压缩。"""
import argparse
from collections import Counter
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


sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "responses_compat_fixture", Path(__file__).with_name("test-responses-compat.py")
)
COMPAT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = COMPAT
SPEC.loader.exec_module(COMPAT)

SCENARIOS = ("disabled", "compaction-first", "compaction-last", "near-threshold",
             "tool-roundtrip", "source-isolation", "protocol-isolation", "rejected")
OLD_MARKER = "original-history-marker-must-disappear"
SYSTEM_MARKER = "fixture-system-instruction-must-survive"
TOOL_MARKER = "actual-read-file-result-token"


def message(text):
    """生成可进入历史的普通 assistant 消息。"""
    return {"id": "msg-" + text, "type": "message", "role": "assistant", "status": "completed",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}


def compaction(number):
    """加密字段只作不透明往返标记，不在夹具内解密。"""
    return {"id": f"cmp-{number}", "type": "compaction", "encrypted_content": f"opaque-compaction-{number}"}


def function_call(number, path):
    """使用真实 read_file 工具而非伪造工具返回值。"""
    return {"id": f"fc-{number}", "type": "function_call", "call_id": f"call-{number}",
            "name": "read_file", "arguments": json.dumps({"target_file": str(path)}), "status": "completed"}


class Upstream(BaseHTTPRequestHandler):
    """记录全部请求；额外摘要、探测或重试会改变断言中的精确请求数。"""
    def log_message(self, *_args):
        """关闭默认 HTTP 日志。"""
        pass

    def respond(self, body, content_type="application/json", status=200):
        """返回带长度的响应，避免等待连接关闭。"""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """记录任何不应发生的能力或模型探测。"""
        with self.server.lock:
            self.server.gets.append(self.path)
        self.respond(b'{"data":[]}')

    def do_POST(self):
        """按场景生成最小有效协议响应，不在 HTTP 线程内吞掉断言。"""
        payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        with self.server.lock:
            self.server.requests.append((self.path, dict(self.headers), payload))
            number = len(self.server.requests)
        scenario = self.server.scenario
        if scenario == "rejected":
            self.respond(json.dumps({"error": {"message": "fixture-rejected-context_management",
                         "type": "invalid_request_error", "param": "context_management",
                         "code": "unsupported_parameter"}}).encode(), status=400)
            return
        if self.path.endswith("/chat/completions"):
            chunk = {"id": "chat-fixture", "object": "chat.completion.chunk", "created": 1,
                     "model": payload["model"], "choices": [{"index": 0, "delta": {
                         "role": "assistant", "content": "ok"}, "finish_reason": "stop"}]}
            self.respond(("data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n").encode(), "text/event-stream")
            return
        if self.path.endswith("/messages"):
            events = [
                {"type": "message_start", "message": {"id": "msg_fixture", "type": "message",
                 "role": "assistant", "model": payload["model"], "content": [], "stop_reason": None,
                 "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 0}}},
                {"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}},
                {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}},
                {"type": "content_block_stop", "index": 0},
                {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                 "usage": {"output_tokens": 1}},
                {"type": "message_stop"},
            ]
            self.respond(COMPAT.encode_events(events), "text/event-stream")
            return
        output = [message(f"answer-{number}")]
        if scenario in ("compaction-first", "compaction-last", "near-threshold") and number <= 2:
            output = [compaction(number), message(f"answer-{number}")]
            if scenario == "compaction-last":
                output.reverse()
        if scenario == "near-threshold" and number == 1:
            output = [message("answer-1")]
        elif scenario in ("source-isolation", "protocol-isolation") and number == 1:
            output.insert(0, compaction(1))
        elif scenario == "tool-roundtrip":
            if number in (1, 3):
                output = [compaction((number + 1) // 2),
                          function_call((number + 1) // 2, self.server.tool_path)]
        response = COMPAT.make_response(payload["model"], output)
        if scenario == "near-threshold":
            # 32000 的 85% 为 27200；模拟已越过软阈值但未超过硬窗口。
            response["usage"].update(input_tokens=28000, total_tokens=28001)
        if payload.get("stream"):
            self.respond(COMPAT.encode_events([
                {"type": "response.completed", "sequence_number": 0, "response": response}
            ]), "text/event-stream")
        else:
            self.respond(json.dumps(response).encode())


def make_config(port):
    """保持正常窗口；同模型不同连接独立凭据用于来源隔离。"""
    config = '''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "on"
allowed_models = ["off", "on", "other", "connection", "chat", "messages"]
[session]
auto_compact_threshold_percent = 85
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''
    for name, wire, backend, route in [
        ("off", "off-wire", "responses", "primary"),
        ("on", "on-wire", "responses", "primary"),
        ("other", "other-wire", "responses", "primary"),
        ("connection", "on-wire", "responses", "secondary"),
        ("chat", "chat-wire", "chat_completions", "primary"),
        ("messages", "messages-wire", "messages", "primary"),
    ]:
        config += f'''
[model.{name}]
model = "{wire}"
base_url = "http://127.0.0.1:{port}/{route}/v1"
api_key = "fixture-{route}-key"
api_backend = "{backend}"
context_window = 32000
'''
        if name != "off":
            config += "responses_compact_threshold = 2000\n"
    return config


def assert_pairs(payload):
    """每个工具调用必须恰好有一个结果，且调用在结果之前。"""
    items = payload.get("input", [])
    calls = Counter(item["call_id"] for item in items if item.get("type") == "function_call")
    outputs = Counter(item["call_id"] for item in items if item.get("type") == "function_call_output")
    assert calls == outputs and all(count == 1 for count in calls.values()), (calls, outputs, items)
    for call_id in calls:
        positions = {item["type"]: index for index, item in enumerate(items) if item.get("call_id") == call_id}
        assert positions["function_call"] < positions["function_call_output"], items


def assert_compacted(payload, number, after_text=None, before_text=None):
    """检查最新压缩点、覆盖前缀裁剪及系统指令保留。"""
    items = payload.get("input", [])
    compacted = [item for item in items if item.get("type") == "compaction"]
    assert len(compacted) == 1 and compacted[0]["encrypted_content"] == f"opaque-compaction-{number}", items
    serialized = json.dumps(items, ensure_ascii=False)
    assert OLD_MARKER not in serialized, items
    instructions = [payload.get("instructions", "")] + [
        item.get("content", "") for item in items if item.get("role") in ("system", "developer")]
    assert SYSTEM_MARKER in json.dumps(instructions), ("系统指令丢失", payload)
    if after_text:
        assert after_text in serialized, items
    if before_text:
        assert before_text not in serialized, items
    assert_pairs(payload)


def run_checks(exe, root, server, scenario):
    """每个场景使用独立进程、HOME、工作目录，始终回收进程。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    server.tool_path = cwd / "tool-result.txt"
    server.tool_path.write_text(TOOL_MARKER + "\n", encoding="utf-8")
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
            rpc = COMPAT.FIXTURE.Rpc(process)
            rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                     "_meta": {"clientType": "grok-pager", "startupHints": {
                         "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}}})
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            session = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
                               "_meta": {"modelId": "off" if scenario == "disabled" else "on",
                                         "rules": SYSTEM_MARKER}})["sessionId"]

            def prompt(text):
                """执行真实用户轮次并要求正常终止。"""
                result = COMPAT.send_prompt(rpc, session, text)
                assert result["stopReason"] == "end_turn", result

            def select(model):
                """通过正式 ACP 接口切换同一会话模型。"""
                rpc.call("session/set_model", {"sessionId": session, "modelId": model})

            if scenario == "rejected":
                try:
                    result = COMPAT.send_prompt(rpc, session, "检查上游拒绝参数")
                except AssertionError as error:
                    result = {"rpcError": str(error)}
                assert result.get("stopReason") != "end_turn", result
                diagnostic = json.dumps({"result": result, "notifications": rpc.notifications}, ensure_ascii=False)
                assert "context_management" in diagnostic, diagnostic
                expected_count = 1
            elif scenario == "disabled":
                prompt("默认关闭检查")
                expected_count = 1
            elif scenario in ("compaction-first", "compaction-last", "near-threshold"):
                prompt(OLD_MARKER)
                prompt("followup-one")
                prompt("followup-two")
                expected_count = 3
                first = server.requests[1][2]
                latest = server.requests[2][2]
                if scenario == "near-threshold":
                    # 首轮尚无压缩点，高 usage 本身也不能触发客户端摘要。
                    assert OLD_MARKER in json.dumps(first["input"]), first
                    assert not any(item.get("type") == "compaction" for item in first["input"]), first
                else:
                    assert_compacted(first, 1, "followup-one",
                                     "answer-1" if scenario == "compaction-last" else None)
                assert_compacted(latest, 2, "followup-two", "followup-one")
                serialized = json.dumps(latest["input"])
                assert "opaque-compaction-1" not in serialized and "answer-1" not in serialized, latest
                if scenario != "compaction-last":
                    assert "answer-1" in json.dumps(first["input"]), first
                    assert "answer-2" in serialized, latest
                else:
                    assert "answer-2" not in serialized, latest
            elif scenario == "tool-roundtrip":
                prompt(OLD_MARKER + ": 使用 read_file 读取 " + str(server.tool_path))
                prompt("再次读取同一个文件")
                prompt("确认第二次工具执行结果")
                expected_count = 5
                for index, number in [(1, 1), (3, 2)]:
                    payload = server.requests[index][2]
                    assert_compacted(payload, number)
                    outputs = [item for item in payload["input"] if item.get("type") == "function_call_output"]
                    assert len(outputs) == 1 and outputs[0]["call_id"] == f"call-{number}", outputs
                    assert TOOL_MARKER in json.dumps(outputs[0]["output"]), outputs
                    assert "error" not in json.dumps(outputs[0]["output"]).lower(), outputs
                assert_compacted(server.requests[4][2], 2, "确认第二次工具执行结果")
                assert "call-1" not in json.dumps(server.requests[4][2]["input"]), server.requests[4]
            else:
                prompt(OLD_MARKER)
                targets = ("other", "connection") if scenario == "source-isolation" else ("chat", "messages")
                for target in targets:
                    select(target)
                    prompt("切换后的正常回复")
                expected_count = 3
                for path, headers, payload in server.requests[1:]:
                    assert "opaque-compaction-1" not in json.dumps(payload), payload
                    assert not any(item.get("type") == "compaction" for item in payload.get("input", [])), payload
                if scenario == "source-isolation":
                    assert server.requests[1][2]["model"] == "other-wire", server.requests
                    path, headers, payload = server.requests[2]
                    assert path == "/secondary/v1/responses" and payload["model"] == "on-wire", server.requests
                    assert {key.lower(): value for key, value in headers.items()}["authorization"] == "Bearer fixture-secondary-key", headers
                else:
                    assert server.requests[1][0].endswith("/chat/completions"), server.requests
                    assert server.requests[2][0].endswith("/messages"), server.requests

            assert len(server.requests) == expected_count, ("出现额外摘要、重试或探测请求", server.requests)
            # 现有连接预热允许访问根路径；能力探测不得增加其他 GET。
            assert all(path == "/" for path in server.gets), ("不应自动探测", server.gets)
            for path, _headers, payload in server.requests:
                enabled = path.endswith("/responses") and scenario != "disabled"
                if enabled:
                    assert payload.get("context_management") == [{"type": "compaction", "compact_threshold": 2000}], payload
                else:
                    assert "context_management" not in payload, payload
                assert "responses_compact_threshold" not in payload, payload
                if path.endswith("/responses"):
                    assert_pairs(payload)
            saved, original = tomllib.loads(config_path.read_text(encoding="utf-8")), tomllib.loads(config)
            assert saved["model"] == original["model"] and saved["models"] == original["models"], saved
            print(f"PASS: {scenario}", flush=True)
        except Exception:
            stderr.flush()
            stderr.seek(0)
            print(stderr.read()[-12000:])
            raise
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
            process.stdin.close()
            process.stdout.close()


def main():
    """只运行已构建程序；可重复指定有限场景，退出时清理全部临时资源。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序")
    parser.add_argument("--scenario", action="append", choices=SCENARIOS, help="指定场景，可重复")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请提供已构建的 Release 程序：" + str(exe))
    for scenario in dict.fromkeys(args.scenario or SCENARIOS):
        server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        server.scenario, server.requests, server.gets = scenario, [], []
        server.lock = threading.Lock()
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory(prefix="grok-responses-compaction-") as root:
                run_checks(exe, Path(root), server, scenario)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
