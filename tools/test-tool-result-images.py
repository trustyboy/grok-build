"""使用真实 Release ACP 进程和本地假上游验证工具图片、切模型、冷恢复及视觉关闭。"""
import argparse
import base64
from contextlib import contextmanager
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


# 复用有效 PNG 和 ACP 客户端，禁止导入时生成仓库字节码缓存。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "tool_images_model_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)

IMAGE_CALL = "fixture-image-call"
TEXT_CALL = "fixture-text-call"
TEXT_MARKER = "fixture-tool-result-images-text"
ANSWER = "fixture-tool-result-images-ok"
IMAGE_URL = "data:image/png;base64," + FIXTURE.PNG
MODELS = ("vision-first", "vision-second")


def image_blocks(messages):
    """收集消息级图片块，保留消息索引以检查组后位置。"""
    return [(index, block) for index, message in enumerate(messages)
            if isinstance(message.get("content"), list)
            for block in message["content"] if block.get("type") == "image_url"]


def check_history(payload, enabled):
    """检查真实请求的工具配对、文本、图片完整性及来源标签。"""
    messages = payload["messages"]
    tools = [message for message in messages if message.get("role") == "tool"]
    assert len(tools) == 2, "工具结果缺失或重复"
    assert all(isinstance(message.get("content"), str) for message in tools), "tool content 必须全部为字符串"
    assert {message.get("tool_call_id") for message in tools} == {IMAGE_CALL, TEXT_CALL}, "工具调用 ID 不匹配"
    calls = [(index, message["tool_calls"]) for index, message in enumerate(messages)
             if message.get("role") == "assistant" and message.get("tool_calls")]
    assert len(calls) == 1, "历史工具调用组缺失或重复"
    start, declared = calls[0]
    assert len(declared) == 2 and {call["id"] for call in declared} == {IMAGE_CALL, TEXT_CALL}, "并行调用声明不完整"
    assert all(message.get("role") == "tool" for message in messages[start + 1:start + 3]), "并行工具结果被其他消息打断"
    assert {message.get("tool_call_id") for message in messages[start + 1:start + 3]} == {IMAGE_CALL, TEXT_CALL}, "结果组配对不完整"
    results = {message["tool_call_id"]: message["content"] for message in tools}
    assert TEXT_MARKER in results[TEXT_CALL], "真实文本文件结果丢失或工具执行失败"
    assert "fixture-image.png" in results[IMAGE_CALL], "图片工具来源路径丢失"
    assert "Read image file:" in results[IMAGE_CALL], "图片未通过真实 read_file 图片分支"
    images = image_blocks(messages)
    serialized = json.dumps(messages, ensure_ascii=False)
    if enabled:
        assert len(images) == 1, "历史图片缺失或重复"
        index, block = images[0]
        assert index == start + 3 and messages[index]["role"] == "user", "图片必须紧随完整工具结果组出现在 user 消息"
        assert block.get("image_url", {}).get("url") == IMAGE_URL, "图片 URL 被截断、改写或替换"
        assert serialized.count(IMAGE_URL) == 1, "图片 URL 在请求中重复"
        parts = messages[index]["content"]
        position = parts.index(block)
        labels = "\n".join(part.get("text", "") for part in parts[:position] if part.get("type") == "text")
        assert IMAGE_CALL in labels, "图片前的来源标签未关联工具调用 ID"
    else:
        assert not images and "data:image/" not in serialized, "关闭视觉后仍发送图片"
        assert FIXTURE.PNG not in serialized, "关闭视觉后 base64 泄漏到文字"


class MockState:
    """锁保护请求记录；上游线程的断言错误由主线程明确报告。"""
    def __init__(self, cwd):
        """夹具仅调用临时工作区中的两个只读文件。"""
        self.cwd = cwd
        self.lock = threading.Lock()
        self.requests = []
        self.failures = []
        self.expected_model = MODELS[0] + "-wire"
        self.enabled = True

    def configure(self, model, enabled):
        """仅在上一轮 ACP 完成后更新下一轮期望。"""
        with self.lock:
            self.expected_model = model + "-wire"
            self.enabled = enabled

    def reply(self, path, authorization, payload):
        """首个请求发出同批真实工具调用，随后每次重放都必须通过载荷断言。"""
        with self.lock:
            first = not self.requests
            self.requests.append(payload)
            assert path == "/v1/chat/completions", "意外上游接口：" + path
            assert authorization == "Bearer fixture-tool-images-key", "隔离认证不匹配"
            assert payload.get("model") == self.expected_model, "请求未采用预期模型"
            assert payload.get("stream") is True, "ACP 未使用流式请求"
            if first:
                assert not image_blocks(payload["messages"]), "首轮不应已有图片"
                definitions = {tool.get("function", {}).get("name") for tool in payload.get("tools", [])}
                assert "read_file" in definitions, "真实程序未提供 read_file 工具"
                return {"role": "assistant", "tool_calls": [
                    {"index": index, "id": call_id, "type": "function", "function": {
                        "name": "read_file", "arguments": json.dumps({"target_file": str(self.cwd / name)})}}
                    for index, (call_id, name) in enumerate([
                        (IMAGE_CALL, "fixture-image.png"), (TEXT_CALL, "fixture-text.txt")])
                ]}
            check_history(payload, self.enabled)
            return {"role": "assistant", "content": ANSWER}

    def fail(self, error):
        """保存 HTTP 线程错误，避免客户端重试掩盖首次失败。"""
        with self.lock:
            self.failures.append(str(error))

    def snapshot(self):
        """读取稳定快照，供每轮断言请求数和线程错误。"""
        with self.lock:
            return list(self.requests), list(self.failures)


class Upstream(BaseHTTPRequestHandler):
    """仅处理本机 Chat Completions，错误立即返回 400 并记录。"""
    timeout = 10

    def log_message(self, *_args):
        """关闭默认访问日志，由主线程汇总失败信息。"""
        pass

    def respond(self, status, body, content_type="application/json"):
        """发送带长度的响应，避免客户端等待连接关闭。"""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """启动探针与模型列表只使用隔离的本地数据。"""
        self.respond(200, json.dumps({"data": [{"id": model + "-wire"} for model in MODELS]}).encode())

    def do_POST(self):
        """返回完整工具调用流或最终正文，严格检查后续真实请求。"""
        try:
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            if self.path == "/auxiliary/v1/responses":
                # 自动标题使用独立路由，不占用主会话的工具响应和请求计数。
                response = {
                    "id": "fixture-title", "object": "response", "created_at": 0,
                    "status": "completed", "model": payload["model"],
                    "output": [{"type": "message", "id": "fixture-title-message",
                        "role": "assistant", "status": "completed", "content": [
                            {"type": "output_text", "text": "工具图片回归", "annotations": []}]}],
                    "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
                }
                if payload.get("stream"):
                    event = {"type": "response.completed", "sequence_number": 0, "response": response}
                    self.respond(200, ("data: " + json.dumps(event) + "\n\n").encode(), "text/event-stream")
                else:
                    self.respond(200, json.dumps(response).encode())
                return
            delta = self.server.state.reply(self.path, self.headers.get("Authorization"), payload)
            chunk = {
                "id": "fixture-tool-images", "object": "chat.completion.chunk", "created": 1,
                "model": payload["model"], "choices": [{"index": 0, "delta": delta,
                    "finish_reason": "tool_calls" if "tool_calls" in delta else "stop"}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 10, "total_tokens": 110},
            }
            body = "data: " + json.dumps(chunk) + "\n\ndata: [DONE]\n\n"
            self.respond(200, body.encode(), "text/event-stream")
        except Exception as error:
            self.server.state.fail(error)
            self.respond(400, b'{"error":"fixture tool image validation failed"}')


def make_config(port):
    """两个模型默认开启视觉，辅助请求使用本地独立路由。"""
    config = f'''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
auto_wake = false
title_refresh = false
session_recap = false
turn_summary = false
[cli]
use_leader = false
auto_update = false
[models]
default = "{MODELS[0]}"
allowed_models = {json.dumps(list(MODELS))}
max_retries = 0
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
[memory]
enabled = false
[memory_v2]
enabled = false
[ui]
prompt_suggestions = false
[endpoints]
xai_api_base_url = "http://127.0.0.1:{port}/auxiliary/v1"
cli_chat_proxy_base_url = "http://127.0.0.1:{port}/auxiliary"
'''
    for model in MODELS:
        config += f'''
[model.{model}]
model = "{model}-wire"
base_url = "http://127.0.0.1:{port}/v1"
api_key = "fixture-tool-images-key"
api_backend = "chat_completions"
context_window = 32000
max_completion_tokens = 1024
supports_vision = true
'''
    return config


@contextmanager
def agent(exe, root, env, phase):
    """每次独立初始化 ACP，退出时关闭 stdin、终止超时进程并关闭管道。"""
    with (root / (phase + "-stderr.log")).open("w+", encoding="utf-8") as stderr:
        process = subprocess.Popen(
            [str(exe), "agent", "--no-leader", "stdio"], cwd=root / "work", env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr, text=True, encoding="utf-8",
        )
        try:
            rpc = FIXTURE.Rpc(process)
            rpc.call("initialize", {
                "protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                "_meta": {"clientType": "grok-pager", "startupHints": {
                    "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}},
            })
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            yield rpc
        except Exception:
            stderr.flush()
            stderr.seek(0)
            print(stderr.read()[-12000:], flush=True)
            raise
        finally:
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


def wait_history(home, session):
    """等待当前隔离会话落盘，确保冷恢复读取真实工具图片历史。"""
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        paths = [path for path in (home / "sessions").rglob("chat_history.jsonl") if path.parent.name == session]
        if len(paths) == 1:
            try:
                items = [json.loads(line) for line in paths[0].read_text(encoding="utf-8").splitlines() if line]
                if sum(item.get("type") == "assistant" for item in items) >= 3:
                    tools = [item for item in items if item.get("type") == "tool_result"]
                    assert len(tools) == 2, "持久化工具结果缺失或重复"
                    assert sum(part.get("url") == IMAGE_URL for item in tools for part in item.get("images", [])) == 1, "历史未保留原始工具图片"
                    return
            except (OSError, ValueError):
                pass
        time.sleep(0.05)
    raise AssertionError("等待隔离会话完整持久化超时")


def run_checks(exe, root, server):
    """同一临时 HOME 内覆盖首次工具执行、切模型、进程重启及关闭视觉。"""
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    (cwd / "fixture-image.png").write_bytes(base64.b64decode(FIXTURE.PNG))
    (cwd / "fixture-text.txt").write_text(TEXT_MARKER + "\n", encoding="utf-8")
    server.state = MockState(cwd)
    config = make_config(server.server_port)
    config_path = home / "config.toml"
    config_path.write_text(config, encoding="utf-8")
    env = {key: value for key, value in os.environ.items()
           if not key.upper().startswith(("GROK_", "XAI_", "OPENAI_", "ANTHROPIC_"))
           and key.upper() not in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY")}
    env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
               APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
               TMP=str(root), TEMP=str(root), TMPDIR=str(root), NO_PROXY="*",
               XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
               GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false")

    def prompt(rpc, session, phase, count):
        """每轮检查所有 HTTP 断言及精确请求数，禁止失败后无图片重试冒充成功。"""
        try:
            result = rpc.call("session/prompt", {"sessionId": session, "prompt": [
                {"type": "text", "text": phase + "：执行假上游返回的工具调用，随后回复 ok。"}]})
            assert result["stopReason"] == "end_turn", result
        finally:
            requests, failures = server.state.snapshot()
            assert not failures, failures
        assert len(requests) == count, (phase, "意外重试或额外请求", len(requests), count)
        print("PASS: " + phase, flush=True)

    with agent(exe, root, env, "initial") as rpc:
        session = rpc.call("session/new", {"cwd": str(cwd), "mcpServers": [],
            "_meta": {"modelId": MODELS[0], "yoloMode": True}})["sessionId"]
        assert rpc.ext("x.ai/session/support_vision", {"sessionId": session})["enabled"] is True
        prompt(rpc, session, "真实双工具结果与图片组后发送", 2)
        rpc.call("session/set_model", {"sessionId": session, "modelId": MODELS[1]})
        server.state.configure(MODELS[1], True)
        assert rpc.ext("x.ai/session/support_vision", {"sessionId": session})["enabled"] is True
        prompt(rpc, session, "同会话切模型保留历史图片", 3)
        wait_history(home, session)
    with agent(exe, root, env, "resumed") as rpc:
        loaded = rpc.call("session/load", {"sessionId": session, "cwd": str(cwd), "mcpServers": []})
        assert loaded["models"]["currentModelId"] == MODELS[1], "冷恢复未保留当前模型"
        assert rpc.ext("x.ai/session/support_vision", {"sessionId": session})["enabled"] is True
        prompt(rpc, session, "终止进程后冷恢复历史图片", 4)
        status = rpc.ext("x.ai/session/support_vision", {
            "sessionId": session, "expectedModelId": MODELS[1], "enabled": False})
        assert status["enabled"] is False
        server.state.configure(MODELS[1], False)
        prompt(rpc, session, "关闭视觉剥离图片并保留工具文字", 5)
    original = tomllib.loads(config)
    saved = tomllib.loads(config_path.read_text(encoding="utf-8"))
    assert saved["model"] == original["model"] and saved["models"] == original["models"], "会话操作修改了持久模型配置"


def main():
    """仅使用已构建程序；临时文件、会话、进程与 HTTP 服务始终清理。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请指定已构建的 Release 程序：" + str(exe))
    with tempfile.TemporaryDirectory(prefix="grok-tool-images-", dir=tempfile.gettempdir()) as scratch:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            run_checks(exe, Path(scratch), server)
            print("PASS: 工具图片 Chat Completions 兼容、切模型、冷恢复及关闭视觉", flush=True)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
