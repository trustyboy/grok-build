"""用隔离 ACP 进程和本地假上游验证 Responses 协议兼容回归。"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


# 复用现有 ACP Rpc，保持客户端消息处理与项目回归脚本一致。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "model_discovery_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)


class Upstream(BaseHTTPRequestHandler):
    """按场景生成 Responses JSON 或 SSE，并记录实际请求。"""
    def log_message(self, *_args):
        """关闭 HTTP 默认日志。"""
        pass

    def respond(self, body, content_type):
        """发送带长度的本地响应。"""
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """本配置只使用静态模型，不请求模型目录。"""
        self.respond(b'{"data":[]}', "application/json")

    def do_POST(self):
        """检查 Responses 输入并返回每个场景配置的响应。"""
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        self.server.state["requests"].append(payload)
        scenario = self.server.state["scenario"]
        if scenario == "metadata-progress":
            response = make_response(payload["model"], [{
                "id": "msg-meta", "type": "message", "role": "assistant", "status": "completed",
                "content": [{"type": "output_text", "text": "metadata-ok", "annotations": []}],
            }], metadata={"fixture": "response-metadata"})
            events = [
                {"type": "response.metadata", "metadata": {"fixture": "stream-metadata"}},
                {"type": "response.compaction.compacting"},
                {"type": "response.completed", "sequence_number": 2, "response": response},
            ]
            self.respond(encode_events(events), "text/event-stream")
        elif scenario == "function-tool":
            tool_output = next((item for item in payload.get("input", [])
                                if item.get("type") == "function_call_output" and item.get("call_id") == "call-1"), None)
            if len(self.server.state["requests"]) == 1 or tool_output is None:
                path = str((self.server.state["fixture_dir"] / "tool-result.txt").resolve())
                response = make_response(payload["model"], [
                    {"id": "fc-1", "type": "function_call", "call_id": "call-1",
                     "name": "read_file", "arguments": json.dumps({"target_file": path}), "status": "completed"},
                ])
                events = [
                    {"type": "response.output_item.added", "sequence_number": 0,
                     "output_index": 0, "item": {"id": "fc-1", "type": "function_call",
                     "call_id": "call-1", "name": "read_file", "arguments": "", "status": "in_progress"}},
                    {"type": "response.function_call_arguments.delta", "sequence_number": 1,
                     "item_id": "fc-1", "output_index": 0,
                     "delta": json.dumps({"target_file": str((self.server.state["fixture_dir"] / "tool-result.txt").resolve())})},
                    {"type": "response.completed", "sequence_number": 2, "response": response},
                ]
            else:
                assert_has_call_output(payload, "call-1")
                call_output = next(item["output"] for item in payload["input"]
                                   if item.get("type") == "function_call_output" and item.get("call_id") == "call-1")
                assert "roundtrip-token" in json.dumps(call_output), call_output
                response = make_response(payload["model"], [{
                    "id": "msg-2", "type": "message", "role": "assistant", "status": "completed",
                    "content": [{"type": "output_text", "text": "tool-roundtrip-ok", "annotations": []}],
                }])
                events = [{"type": "response.completed", "sequence_number": 0, "response": response}]
            self.respond(encode_events(events), "text/event-stream")
        elif scenario == "phase-refusal":
            if len(self.server.state["requests"]) == 1:
                response = make_response(payload["model"], [
                    {"id": "msg-commentary", "type": "message", "role": "assistant",
                     "phase": "commentary", "status": "completed",
                     "content": [{"type": "output_text", "text": "commentary-token", "annotations": []}]},
                    {"id": "msg-final", "type": "message", "role": "assistant",
                     "phase": "final_answer", "status": "completed",
                     "content": [{"type": "output_text", "text": "final-token", "annotations": []}]},
                ])
            else:
                request_items = payload.get("input", [])
                assistant_messages = [item for item in request_items
                                      if item.get("type") == "message" and item.get("role") == "assistant"]
                phases = [item.get("phase") for item in assistant_messages]
                assert "commentary" in phases and "final_answer" in phases, (
                    "下一轮 assistant input message 未保留 commentary/final_answer 边界", request_items)
                assert any(any(part.get("type") == "output_text" and "commentary-token" in part.get("text", "")
                               for part in item.get("content", [])) for item in assistant_messages), request_items
                response = make_response(payload["model"], [{
                    "id": "msg-refusal", "type": "message", "role": "assistant", "phase": "final_answer",
                    "status": "completed", "content": [{"type": "refusal", "refusal": "fixture-refusal-token"}],
                }])
                self.server.state["refusal_response_sent"] = True
            self.respond(encode_events([{
                "type": "response.completed", "sequence_number": 0, "response": response,
            }]), "text/event-stream")
        else:
            # 返回合法 shell_call 响应，检查客户端明确拒绝执行。
            response = make_response(payload["model"], [{
                "id": "shell-1", "type": "shell_call", "call_id": "shell-call-1",
                "action": {"commands": ["python -c \\\"from pathlib import Path; Path(r'%s').write_text('executed')\\\"" % str(self.server.state["execution_marker"])],
                           "timeout_ms": 5000, "max_output_length": 1000},
                "status": "completed", "environment": {"type": "local"},
            }])
            self.respond(encode_events([{
                "type": "response.completed", "sequence_number": 0, "response": response,
            }]), "text/event-stream")


def make_response(model, output, metadata=None):
    """构造解码器所需的最小完整 Responses 对象。"""
    return {
        "id": "resp-fixture", "object": "response", "created_at": 1,
        "model": model, "status": "completed", "output": output,
        "metadata": metadata or {},
        "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                  "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0}, "total_tokens": 11},
    }


def encode_events(events):
    """编码 SSE 事件名与数据，模拟真实 Responses 流。"""
    return "".join("event: " + event["type"] + "\ndata: " + json.dumps(event) + "\n\n"
                   for event in events).encode()


def assert_has_call_output(payload, call_id):
    """要求下一轮输入包含执行结果，避免只看工具事件显示。"""
    assert any(item.get("type") == "function_call_output" and item.get("call_id") == call_id
               for item in payload.get("input", [])), ("缺少函数工具结果", payload.get("input"))


def config_text(port):
    """配置单一 Responses 模型，禁用可影响自动化的远程功能。"""
    return f'''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "fixture"
allowed_models = ["fixture"]
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
[model.fixture]
model = "fixture-responses"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "fixture-key"
api_backend = "responses"
context_window = 32000
'''


def send_prompt(rpc, session, text):
    """执行一轮 ACP 用户消息并返回真实 stop reason。"""
    return rpc.call("session/prompt", {
        "sessionId": session, "prompt": [{"type": "text", "text": text}],
    })


def run_checks(exe, root, server, scenario):
    """在临时 HOME 和工作目录运行真实 Release ACP 进程。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    fixture_dir = root / "fixture"
    fixture_dir.mkdir()
    (fixture_dir / "tool-result.txt").write_text("roundtrip-token\\n", encoding="utf-8")
    server.state["fixture_dir"] = fixture_dir
    (home / "config.toml").write_text(config_text(server.server_port), encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith(("GROK_", "XAI_"))}
    env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
               APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
               XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
               GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false")
    with (root / "stderr.log").open("w+", encoding="utf-8") as stderr:
        process = subprocess.Popen(
            [str(exe), "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
            text=True, encoding="utf-8",
        )
        try:
            rpc = FIXTURE.Rpc(process)
            rpc.call("initialize", {
                "protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                "_meta": {"clientType": "grok-pager", "startupHints": {
                    "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True,
                }},
            })
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            session = rpc.call("session/new", {
                "cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "fixture"},
            })["sessionId"]
            server.state["execution_marker"] = root / "shell-executed.marker"
            if scenario == "metadata-progress":
                result = send_prompt(rpc, session, "完成本地兼容性检查")
                assert result["stopReason"] == "end_turn", result
                assert any(message.get("method") == "session/update" for message in rpc.notifications), rpc.notifications
                assert len(server.state["requests"]) == 1, server.state["requests"]
                print("PASS: response.metadata 与 response.compaction.compacting 事件", flush=True)
            elif scenario == "function-tool":
                result = send_prompt(rpc, session, "使用 read_file 读取本地测试夹具文件")
                assert result["stopReason"] == "end_turn", result
                assert result["stopReason"] == "end_turn", result
                requests = server.state["requests"]
                assert len(requests) >= 2, requests
                assert_has_call_output(requests[1], "call-1")
                second_input = requests[1].get("input", [])
                assert any(item.get("type") == "function_call" and item.get("call_id") == "call-1"
                           for item in second_input), requests[1]
                output = next(item["output"] for item in second_input
                              if item.get("type") == "function_call_output" and item.get("call_id") == "call-1")
                assert "roundtrip-token" in json.dumps(output), output
                assert "error" not in json.dumps(output).lower(), output
                print("PASS: read_file 工具真实执行，结果含 roundtrip-token 且无错误", flush=True)
            elif scenario == "phase-refusal":
                first = send_prompt(rpc, session, "第一轮")
                assert first["stopReason"] == "end_turn", first
                second = send_prompt(rpc, session, "继续第二轮")
                assert second["stopReason"] == "end_turn", second
                third = send_prompt(rpc, session, "第三轮确认拒绝历史")
                assert third["stopReason"] == "end_turn", third
                assert len(server.state["requests"]) >= 3, server.state["requests"]
                assert server.state.get("refusal_response_sent"), server.state
                refusal_items = [item for item in server.state["requests"][2].get("input", [])
                                 if item.get("type") == "message" and item.get("role") == "assistant"]
                assert any(part.get("type") == "refusal" and part.get("refusal") == "fixture-refusal-token"
                           for item in refusal_items for part in item.get("content", [])), refusal_items
                print("PASS: 下一轮 input 保留 assistant phase 边界及 refusal 类型/文字", flush=True)
            else:
                try:
                    result = send_prompt(rpc, session, "运行本地 shell 命令创建测试标志")
                except AssertionError as error:
                    result = {"rpcError": str(error)}
                assert result.get("stopReason") != "end_turn", result
                assert len(server.state["requests"]) == 1, server.state["requests"]
                assert not (root / "shell-executed.marker").exists(), "shell_call 命令被实际执行"
                diagnostic = json.dumps({"result": result, "notifications": rpc.notifications}, ensure_ascii=False)
                assert "尚未支持 Responses 执行能力" in diagnostic, diagnostic
                print("PASS: shell_call 明确报告未支持且标志文件未创建", flush=True)
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


SCENARIOS = ("metadata-progress", "function-tool", "phase-refusal", "unsupported-shell")


def main():
    """默认运行全部场景；每次只启动一个隔离 ACP 进程。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序")
    parser.add_argument("--scenario", action="append", choices=SCENARIOS, help="指定场景，可重复")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请提供已构建的 Release 程序：" + str(exe))
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    server.state = {"scenario": None, "requests": [], "fixture_dir": None, "execution_marker": None}
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    scenarios = list(dict.fromkeys(args.scenario or SCENARIOS))
    try:
        for scenario in scenarios:
            server.state.update(scenario=scenario, requests=[], refusal_response_sent=False)
            with tempfile.TemporaryDirectory(prefix="grok-responses-compat-") as root:
                run_checks(exe, Path(root), server, scenario)
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
