"""使用临时配置、本地假上游和真实 Release ACP 验证 Responses 原生自动压缩。"""
import argparse
from collections import Counter
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
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
             "tool-roundtrip", "source-isolation", "protocol-isolation", "rejected",
             "cold-resume", "usage-calibration", "exact-context-details",
             "native-disabled-client-compaction", "oversized-first-request")
OLD_MARKER = "original-history-marker-must-disappear"
SYSTEM_MARKER = "fixture-system-instruction-must-survive"
TOOL_MARKER = "actual-read-file-result-token"
CHECKPOINT_FIELD = "__grok_context_checkpoint"
ARCHIVE_MARKER = "archive-only-before-compaction"
ARCHIVE_TEXT = ARCHIVE_MARKER + " archived-assistant-detail" * 10000
SUMMARY_MARKER = "legacy-client-summary-marker"
SUMMARY_TEXT = "<summary>\n" + "\n".join([
    "1. Primary Request and Intent: Continue the isolated compaction regression. " + SUMMARY_MARKER,
    "2. Key Technical Concepts: Responses input projection, context limits and persisted ACP sessions.",
    "3. Files and Code Sections: The fixture work directory contains tool-result.txt; no source was changed.",
    "4. Errors and Fixes: None. The upstream and model credentials belong to this local fixture only.",
    "5. Problem Solving: Earlier turns were summarized so the next assistant can finish within the window.",
    "6. All User Messages: The user requested a compaction regression followed by a normal response.",
    "7. Pending Tasks: Answer the latest followup without calling tools or repeating the earlier archive.",
    "8. Current Work: The summary preserves the request while omitting obsolete verbose assistant details.",
    "9. Optional Next Step: Return a concise final answer to the latest user request.",
]) + "\n</summary>"


def is_summary_request(payload):
    """只识别最后一条输入中的正式摘要指令，避免误认后续携带的摘要历史。"""
    items = payload.get("input", [])
    if not items or items[-1].get("role") != "user":
        return False
    last = json.dumps(items[-1].get("content", ""), ensure_ascii=False)
    return ("Your task is to produce a faithful, concise summary" in last
            or "<summary_request>" in last)


def message(text):
    """生成可进入历史的普通 assistant 消息。"""
    return {"id": "msg-" + text[:64], "type": "message", "role": "assistant", "status": "completed",
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
        if self.path.startswith("/auxiliary/"):
            with self.server.lock:
                self.server.auxiliary_requests.append((self.path, payload))
            response = COMPAT.make_response(payload["model"], [message("Fixture session title")])
            if payload.get("stream"):
                self.respond(COMPAT.encode_events([
                    {"type": "response.completed", "sequence_number": 0, "response": response}
                ]), "text/event-stream")
            else:
                self.respond(json.dumps(response).encode())
            return
        with self.server.lock:
            self.server.requests.append((self.path, dict(self.headers), payload))
            number = len(self.server.requests)
        scenario = self.server.scenario
        if number > 8:
            self.respond(b'{"error":{"message":"fixture-request-budget-exhausted","type":"invalid_request_error"}}', status=400)
            return
        if is_summary_request(payload):
            response = COMPAT.make_response(payload["model"], [message(SUMMARY_TEXT)])
            # 摘要采样器消费 text delta；仅 completed 不会产生摘要正文。
            self.respond(COMPAT.encode_events([
                {"type": "response.output_text.delta", "sequence_number": 0, "item_id": "msg-summary",
                 "output_index": 0, "content_index": 0, "delta": SUMMARY_TEXT},
                {"type": "response.completed", "sequence_number": 1, "response": response},
            ]), "text/event-stream")
            return
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
        elif scenario == "cold-resume":
            if number == 1:
                output = [message(ARCHIVE_TEXT), compaction(1), message("answer-1")]
            elif number == 2:
                output = [function_call(1, self.server.tool_path)]
        elif scenario in ("usage-calibration", "exact-context-details") and number == 1:
            output.insert(0, compaction(1))
        response = COMPAT.make_response(payload["model"], output)
        if scenario == "near-threshold" or (scenario == "native-disabled-client-compaction" and number == 1):
            # 32000 的 85% 为 27200；模拟已越过软阈值但未超过硬窗口。
            response["usage"].update(input_tokens=28000, total_tokens=28001)
        elif scenario == "cold-resume":
            response["usage"].update(input_tokens=180000, output_tokens=90000, total_tokens=270000,
                                     context_details={"input_tokens": 4096, "output_tokens": 512})
        elif scenario == "exact-context-details":
            response["usage"].update(input_tokens=180000, output_tokens=90000, total_tokens=270000,
                                     context_details={"input_tokens": 4096, "output_tokens": 512})
        elif scenario == "usage-calibration":
            response["usage"].update(input_tokens=28000 if number == 1 else 7000,
                                     output_tokens=100, total_tokens=28100 if number == 1 else 7100)
        if payload.get("stream"):
            self.respond(COMPAT.encode_events([
                {"type": "response.completed", "sequence_number": 0, "response": response}
            ]), "text/event-stream")
        else:
            self.respond(json.dumps(response).encode())


def make_config(port, scenario):
    """保持正常窗口和输出预留；旧客户端场景固定单次摘要，避免后台双阶段竞态。"""
    config = '''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "on"
web_search = "auxiliary"
session_summary = "auxiliary"
image_description = "auxiliary"
prompt_suggestion = "auxiliary"
allowed_models = ["off", "on", "other", "connection", "chat", "messages"]
[turn_summary]
model = "auxiliary"
[session]
auto_compact_threshold_percent = 85
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''
    # 未显式选中的内置辅助模型也只能访问本地假上游。
    config += f'''[endpoints]
xai_api_base_url = "http://127.0.0.1:{port}/auxiliary/v1"
cli_chat_proxy_base_url = "http://127.0.0.1:{port}/auxiliary"
'''
    if scenario == "native-disabled-client-compaction":
        config = config.replace("[features]\n", "[features]\ntwo_pass_compaction = false\n", 1)
    for name, wire, backend, route in [
        ("off", "off-wire", "responses", "primary"),
        ("on", "on-wire", "responses", "primary"),
        ("other", "other-wire", "responses", "primary"),
        ("connection", "on-wire" if scenario == "source-isolation" else "connection-wire", "responses", "secondary"),
        ("chat", "chat-wire", "chat_completions", "primary"),
        ("messages", "messages-wire", "messages", "primary"),
        ("auxiliary", "fixture-auxiliary", "responses", "auxiliary"),
    ]:
        config += f'''
[model.{name}]
model = "{wire}"
base_url = "http://127.0.0.1:{port}/{route}/v1"
api_key = "fixture-{route}-key"
api_backend = "{backend}"
context_window = 32000
max_completion_tokens = 1024
'''
        if name not in ("off", "auxiliary"):
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


def initialize(process):
    """每个新进程独立完成 ACP 初始化与认证。"""
    rpc = COMPAT.FIXTURE.Rpc(process)
    rpc.call("initialize", {"protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
             "_meta": {"clientType": "grok-pager", "startupHints": {
                 "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}}})
    rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
    return rpc


def stop_process(process):
    """优先通过 stdin EOF 退出，超时后终止，始终关闭管道并等待进程回收。"""
    try:
        try:
            process.stdin.close()
        except OSError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
    finally:
        process.stdout.close()


def wait_history(home, session, assistant_count):
    """等到真实会话的 JSONL 完整写入，避免用内存通知冒充持久化。"""
    deadline = time.monotonic() + 15
    last = "未找到会话历史"
    while time.monotonic() < deadline:
        paths = [path for path in (home / "sessions").rglob("chat_history.jsonl")
                 if path.parent.name == session]
        if len(paths) == 1:
            try:
                items = [json.loads(line) for line in paths[0].read_text(encoding="utf-8").splitlines() if line]
                count = sum(item.get("type") == "assistant" for item in items)
                if count >= assistant_count:
                    return items
                last = f"assistant 仅写入 {count}/{assistant_count}"
            except (OSError, ValueError) as error:
                last = str(error)
        time.sleep(0.05)
    raise AssertionError("等待持久化历史超时：" + last)


def context_record(item):
    """核对 Rust 版本化记录与所属 assistant 的来源绑定。"""
    raw = item.get("responses_output", [])
    assert raw and CHECKPOINT_FIELD in raw[0], ("缺少持久化上下文记录", item)
    record = raw[0][CHECKPOINT_FIELD]
    assert record["version"] == 1, record
    assert record["source"] == item.get("responses_source") and record["source"], record
    assert record["input_fingerprint"] == item.get("responses_input_fingerprint"), record
    assert isinstance(record["usage"]["tokens"], int) and record["usage"]["tokens"] > 0, record
    assert isinstance(record["usage"]["estimated"], bool), record
    return record


def history_diagnostic(items):
    """仅输出类型、长度和摘要，避免失败日志展开超大历史或凭据。"""
    result = []
    for item in items:
        content = json.dumps(item.get("content", ""), ensure_ascii=False, sort_keys=True)
        raw = item.get("responses_output") or []
        result.append({"type": item.get("type"), "content_chars": len(content),
                       "content_sha256": hashlib.sha256(content.encode()).hexdigest(),
                       "system_marker": SYSTEM_MARKER in content,
                       "raw_types": [entry.get("type") for entry in raw],
                       "source": item.get("responses_source"),
                       "fingerprint": item.get("responses_input_fingerprint"),
                       "context": raw[0].get(CHECKPOINT_FIELD, {}).get("usage") if raw else None})
    return result


def request_diagnostic(path, payload):
    """给出真实线上投影的边界和系统提示指纹。"""
    items = payload.get("input", [])
    serialized = json.dumps(items, ensure_ascii=False)
    systems = [item for item in items if item.get("role") in ("system", "developer")]
    return {"path": path, "model": payload.get("model"), "summary": is_summary_request(payload),
            "context_management": payload.get("context_management"),
            "input_chars": len(serialized), "old_marker": OLD_MARKER in serialized,
            "archive_marker": ARCHIVE_MARKER in serialized,
            "types": [item.get("type") for item in items], "systems": history_diagnostic(systems)}


def run_checks(exe, root, server, scenario):
    """每个场景使用独立进程、HOME、工作目录，始终回收进程。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    server.tool_path = cwd / "tool-result.txt"
    server.tool_path.write_text(TOOL_MARKER + "\n", encoding="utf-8")
    config = make_config(server.server_port, scenario)
    config_path = home / "config.toml"
    config_path.write_text(config, encoding="utf-8")
    env = {key: value for key, value in os.environ.items() if not key.startswith(("GROK_", "XAI_"))}
    env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
               APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
               XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
               GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false")
    with (root / "stderr.log").open("w+", encoding="utf-8") as stderr:
        def start():
            """重启时复用同一 HOME/cwd/env，只创建新的真实进程。"""
            return subprocess.Popen([str(exe), "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
                                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                                    text=True, encoding="utf-8")

        process = start()
        diagnostics = []
        try:
            rpc = initialize(process)
            disabled = scenario in ("disabled", "native-disabled-client-compaction")
            session = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
                               "_meta": {"modelId": "off" if disabled else "on",
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
            elif scenario == "cold-resume":
                prompt(OLD_MARKER)
                assert len(server.requests) == 1, server.requests
                history = wait_history(home, session, 1)
                assistants = [item for item in history if item.get("type") == "assistant"]
                # 只计通用正文即可超过窗口，不把 JSON 或检查点副本算成历史 token。
                archive_tokens = sum(len(item.get("content", "").encode("utf-8")) // 4 for item in assistants)
                assert archive_tokens > 32000, ("冷恢复夹具没有超过本地窗口", archive_tokens)
                checkpoint = context_record(assistants[-1])
                replacement = {"input": checkpoint["replacement_input"]}
                assert_compacted(replacement, 1)
                assert ARCHIVE_MARKER not in json.dumps(replacement), "检查点包含已压缩正文"
                assert checkpoint["usage"] == {"tokens": 4608, "estimated": False}, checkpoint
                diagnostics.append({"phase": "before-restart", "history": history_diagnostic(history)})
                stop_process(process)
                process = start()
                rpc = initialize(process)
                loaded = rpc.call("session/load", {"sessionId": session, "cwd": str(cwd), "mcpServers": []})
                diagnostics.append({"phase": "after-load", "models": loaded.get("models"),
                                    "history": history_diagnostic(wait_history(home, session, 1))})
                assert loaded["models"]["currentModelId"] == "on", loaded.get("models")
                prompt("cold-resume-followup: 使用 read_file 读取 " + str(server.tool_path))
                expected_count = 3
                for payload in (server.requests[1][2], server.requests[2][2]):
                    assert_compacted(payload, 1, "cold-resume-followup")
                    assert ARCHIVE_MARKER not in json.dumps(payload["input"]), "冷恢复重新发送了大段旧正文"
                latest = server.requests[2][2]
                outputs = [item for item in latest["input"] if item.get("type") == "function_call_output"]
                assert len(outputs) == 1 and outputs[0]["call_id"] == "call-1", outputs
                assert TOOL_MARKER in json.dumps(outputs[0]["output"]), outputs
                assert "error" not in json.dumps(outputs[0]["output"]).lower(), outputs
            elif scenario in ("usage-calibration", "exact-context-details"):
                prompt(OLD_MARKER)
                history = wait_history(home, session, 1)
                first = context_record([item for item in history if item.get("type") == "assistant"][-1])
                assert "replacement_input" in first and first["checkpoint_key"], first
                if scenario == "usage-calibration":
                    assert first["usage"]["estimated"] is True, first
                    assert 2000 <= first["usage"]["tokens"] < 28100, first
                else:
                    assert first["usage"] == {"tokens": 4608, "estimated": False}, first
                prompt("usage-calibration-followup")
                history = wait_history(home, session, 2)
                anchor = context_record([item for item in history if item.get("type") == "assistant"][-1])
                assert anchor["checkpoint_key"] == first["checkpoint_key"], anchor
                assert "replacement_input" not in anchor, anchor
                tokens = 7100 if scenario == "usage-calibration" else 4608
                assert anchor["usage"] == {"tokens": tokens, "estimated": False}, anchor
                if scenario == "usage-calibration":
                    stop_process(process)
                    process = start()
                    rpc = initialize(process)
                    rpc.call("session/load", {"sessionId": session, "cwd": str(cwd), "mcpServers": []})
                prompt("usage-calibration-final")
                expected_count = 3
                assert_compacted(server.requests[1][2], 1, "usage-calibration-followup")
                assert_compacted(server.requests[2][2], 1, "usage-calibration-final")
            elif scenario == "native-disabled-client-compaction":
                prompt(OLD_MARKER)
                prompt("legacy-client-followup")
                expected_count = 3
                summaries = [payload for _path, _headers, payload in server.requests if is_summary_request(payload)]
                assert len(summaries) == 1 and is_summary_request(server.requests[1][2]), server.requests
                latest = server.requests[-1][2]
                assert SUMMARY_MARKER in json.dumps(latest["input"]), ("未采用旧客户端摘要", latest)
                assert "legacy-client-followup" in json.dumps(latest["input"]), latest
                history = wait_history(home, session, 1)
                assert CHECKPOINT_FIELD not in json.dumps(history), "原生关闭时不应写入原生计数记录"
            elif scenario == "oversized-first-request":
                try:
                    result = COMPAT.send_prompt(rpc, session, "oversized-first-request: " + ARCHIVE_TEXT * 3)
                except AssertionError as error:
                    result = {"rpcError": str(error)}
                    assert "RPC 超时" not in str(error) and "ACP 进程已退出" not in str(error), str(error)
                expected_count = len(server.requests)
                assert expected_count <= 4, ("容量恢复请求未受限", expected_count)
                summaries = [payload for _path, _headers, payload in server.requests if is_summary_request(payload)]
                normal = [payload for _path, _headers, payload in server.requests if not is_summary_request(payload)]
                assert len(summaries) <= 3 and len(normal) <= 1, (len(summaries), len(normal))
                for payload in normal:
                    assert len(json.dumps(payload["input"]).encode("utf-8")) < 32000 * 4, "超长原请求到达上游"
                    assert summaries and SUMMARY_MARKER in json.dumps(payload["input"]), "未摘要就发送原请求"
                if result.get("stopReason") == "end_turn":
                    assert normal, ("没有正常请求却返回成功", result)
                else:
                    diagnostic = json.dumps({"result": result, "notifications": rpc.notifications}, ensure_ascii=False).lower()
                    assert any(word in diagnostic for word in ("context", "compact", "overflow", "上下文", "压缩")), diagnostic
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
                enabled = path.endswith("/responses") and not disabled and not is_summary_request(payload)
                if enabled:
                    assert payload.get("context_management") == [{"type": "compaction", "compact_threshold": 2000}], payload
                    assert payload.get("max_output_tokens") == 1024, payload
                else:
                    assert "context_management" not in payload, payload
                assert "responses_compact_threshold" not in payload, payload
                assert CHECKPOINT_FIELD not in json.dumps(payload), "本地检查点字段泄漏到线上请求"
                if path.endswith("/responses"):
                    assert_pairs(payload)
            for path, payload in server.auxiliary_requests:
                assert payload.get("model") == "fixture-auxiliary", ("辅助模型未使用隔离配置", path, payload.get("model"))
                assert "context_management" not in payload, ("辅助请求不应开启原生压缩", path)
            saved, original = tomllib.loads(config_path.read_text(encoding="utf-8")), tomllib.loads(config)
            assert saved["model"] == original["model"] and saved["models"] == original["models"], saved
            print(f"PASS: {scenario}", flush=True)
        except Exception:
            print(json.dumps({"diagnostics": diagnostics,
                              "requests": [request_diagnostic(path, payload) for path, _headers, payload in server.requests],
                              "auxiliary": [{"path": path, "model": payload.get("model")}
                                            for path, payload in server.auxiliary_requests]}, ensure_ascii=False), flush=True)
            stderr.flush()
            stderr.seek(0)
            print(stderr.read()[-12000:])
            raise
        finally:
            stop_process(process)


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
        server.auxiliary_requests = []
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
