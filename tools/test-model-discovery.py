"""用一个可变本地上游和隔离 ACP 进程验证跨上游发现及视觉回归。"""
import argparse
import base64
from collections import Counter
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import tempfile
import threading
import time
import tomllib
from urllib.parse import parse_qs, urlsplit
import zlib


def png_chunk(kind, data):
    """生成有效 CRC，避免测试依赖外部图片文件。"""
    return struct.pack("!I", len(data)) + kind + data + struct.pack("!I", zlib.crc32(kind + data))


PNG = base64.b64encode(
    b"\x89PNG\r\n\x1a\n"
    + png_chunk(b"IHDR", struct.pack("!2I5B", 32, 32, 8, 2, 0, 0, 0))
    + png_chunk(b"IDAT", zlib.compress((b"\x00" + b"\xff\x00\x00" * 32) * 32))
    + png_chunk(b"IEND", b"")
).decode()


@dataclass(frozen=True)
class Connection:
    """同一服务按路径、凭据、请求头及查询参数区分连接。"""
    path: str
    key: str
    tenant: str = "shared"
    version: str = "1"

    def matches(self, method, path, headers):
        """严格匹配请求，认证或路由错误不会被假上游放行。"""
        url = urlsplit(path)
        suffix = "/models" if method == "GET" else "/chat/completions"
        return (
            url.path == self.path + suffix
            and headers.get("authorization") == "Bearer " + self.key
            and headers.get("x-fixture-tenant") == self.tenant
            and parse_qs(url.query) == {"fixture-version": [self.version]}
        )


CONNECTIONS = {
    "source-entry": Connection("/current/v1", "fixture-current-key", "current"),
    "backup-entry": Connection("/shared/v1", "fixture-backup-key"),
    "other-key-entry": Connection("/shared/v1", "fixture-other-key"),
    "header-entry": Connection("/shared/v1", "fixture-backup-key", "other-tenant"),
    "query-entry": Connection("/shared/v1", "fixture-backup-key", version="2"),
    "extra-entry": Connection("/extra/v1", "fixture-extra-key"),
    "hidden-entry": Connection("/hidden/v1", "fixture-hidden-key"),
    "disabled-entry": Connection("/disabled/v1", "fixture-disabled-key"),
    "blocked-entry": Connection("/blocked/v1", "fixture-blocked-key"),
}
FORBIDDEN = {"hidden-entry", "disabled-entry", "blocked-entry"}
ELIGIBLE = tuple(name for name in CONNECTIONS if name not in FORBIDDEN)
ALIASES = {"source-alias": "source-entry", "backup-alias": "backup-entry"}


class MockState:
    """串行切换场景，锁保护并发 HTTP 请求的记录与响应快照。"""
    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []
        self.responses = {}
        self.active = 0
        self.peak = 0

    def configure(self, target, hits=(), errors=None, delay=0):
        """近似 ID 不应命中；延迟仅用于验证回退并发上限。"""
        errors = errors or {}
        with self.lock:
            self.requests.clear()
            self.active = 0
            self.peak = 0
            self.responses = {
                name: (
                    errors.get(name, 200),
                    [target if name in hits else target + "-preview"],
                    delay if name in ELIGIBLE and name != "source-entry" else 0,
                )
                for name in CONNECTIONS
            }

    def record(self, method, path, headers, payload=None):
        """记录真实请求，而非仅依赖发现接口返回值判断来源。"""
        headers = {key.lower(): value for key, value in headers.items()}
        source = next(
            (name for name, conn in CONNECTIONS.items() if conn.matches(method, path, headers)),
            "unexpected-connection",
        )
        with self.lock:
            self.requests.append({
                "method": method, "source": source, "path": path,
                "headers": headers, "body": payload,
            })
            if method == "GET":
                self.active += 1
                self.peak = max(self.peak, self.active)
            return self.responses.get(source, (401, [], 0))

    def finish_get(self):
        """计数涵盖模拟处理时段，不包含响应发送后的线程清理。"""
        with self.lock:
            self.active -= 1

    def snapshot(self):
        """读取稳定副本，避免断言遍历正被 HTTP 线程修改的列表。"""
        with self.lock:
            return list(self.requests), self.peak


class Upstream(BaseHTTPRequestHandler):
    """只接受匹配的连接，并提供模型列表及无工具流式回复。"""
    def log_message(self, *_args):
        """关闭 HTTP 默认日志，失败时由场景断言报告请求。"""
        pass

    def respond(self, status, body, content_type="application/json"):
        """统一写入带长度的本地响应。"""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        """返回当前场景的模型列表或指定 HTTP 错误。"""
        state = self.server.state
        status, models, delay = state.record("GET", self.path, self.headers)
        try:
            if delay:
                time.sleep(delay)
            # 上游返回的地址与密钥不能接管本地连接。
            body = {"data": [
                {"id": model, "baseUrl": "https://ignored.invalid", "apiKey": "ignored"}
                for model in models
            ]} if status == 200 else {"error": "fixture-upstream-secret"}
        finally:
            state.finish_get()
        self.respond(status, json.dumps(body).encode())

    def do_POST(self):
        """推理也校验来源和模型，避免错误凭据得到假成功。"""
        payload = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
        status, models, _delay = self.server.state.record("POST", self.path, self.headers, payload)
        if status != 200 or payload.get("model") not in models:
            self.respond(status if status != 200 else 404, b'{"error":"wrong inference route"}')
            return
        chunks = [
            {
                "id": "fixture", "object": "chat.completion.chunk", "created": 0,
                "model": payload.get("model"), "choices": [{
                    "index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": None,
                }],
            },
            {
                "id": "fixture", "object": "chat.completion.chunk", "created": 0,
                "model": payload.get("model"), "choices": [{
                    "index": 0, "delta": {}, "finish_reason": "stop",
                }],
                "usage": {"prompt_tokens": 10, "completion_tokens": 1, "total_tokens": 11},
            },
        ]
        body = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n").encode()
        self.respond(200, body, "text/event-stream")


class Rpc:
    """消费通知并给不需要的客户端请求返回明确错误。"""
    def __init__(self, process):
        """读取线程只收集消息，由调用线程处理 RPC 顺序。"""
        self.process = process
        self.messages = queue.Queue()
        self.counter = 0
        self.notifications = []
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        """过滤非 JSON 输出，并显式报告进程退出。"""
        for line in self.process.stdout:
            try:
                self.messages.put(json.loads(line))
            except ValueError:
                continue
        self.messages.put(None)

    def call(self, method, params, expect_error=False):
        """等待对应响应，超时与进程退出均使测试失败。"""
        self.counter += 1
        call_id = self.counter
        self.process.stdin.write(json.dumps({
            "jsonrpc": "2.0", "id": call_id, "method": method, "params": params,
        }) + "\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + 90
        while time.monotonic() < deadline:
            try:
                message = self.messages.get(timeout=max(0.1, deadline - time.monotonic()))
            except queue.Empty:
                break
            assert message is not None, "ACP 进程已退出：" + method
            if "method" in message:
                self.notifications.append(message)
                if "id" in message:
                    self.process.stdin.write(json.dumps({
                        "jsonrpc": "2.0", "id": message["id"],
                        "error": {"code": -32601, "message": "fixture has no client tools"},
                    }) + "\n")
                    self.process.stdin.flush()
                continue
            if message.get("id") == call_id:
                if expect_error:
                    assert "error" in message, message
                    return message["error"]
                assert "error" not in message, message
                return message["result"]
        raise AssertionError("RPC 超时：" + method)

    def ext(self, name, params, expect_error=False):
        """解包 ACP 扩展响应。"""
        result = self.call("_" + name, params, expect_error)
        return result if expect_error else result["result"]


def make_config(port):
    """固定全部来源；场景只改变服务响应，不重启进程或改写配置。"""
    allowed = [name for name in CONNECTIONS if name != "blocked-entry"]
    allowed.extend(ALIASES)
    allowed.extend(("fixture-*", "uncredentialed-entry"))
    config = f'''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
[cli]
use_leader = false
[models]
default = "source-entry"
allowed_models = {json.dumps(allowed)}
hidden_models = ["hidden-entry"]
disabled_models = ["disabled-entry"]
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''
    for name in (*CONNECTIONS, *ALIASES):
        conn = CONNECTIONS[ALIASES.get(name, name)]
        alias = name in ALIASES
        config += f'''
[model.{name}]
model = "{name}-wire"
name = "{name}"
base_url = "http://127.0.0.1:{port}{conn.path}"
api_key = "{conn.key}"
api_backend = "chat_completions"
extra_headers = {{ "x-fixture-tenant" = "{conn.tenant}" }}
query_params = {{ "fixture-version" = "{conn.version}" }}
context_window = {64000 if alias else 32000}
supports_vision = {"false" if alias else "true"}
supports_reasoning_effort = {"false" if alias else "true"}
'''
    # 全局 XAI_API_KEY 存在时，这个第三方来源必须在发请求前被拒绝。
    config += f'''
[model.uncredentialed-entry]
model = "uncredentialed-wire"
base_url = "http://127.0.0.1:{port}/uncredentialed/v1"
api_backend = "chat_completions"
'''
    return config


class Checks:
    """复用两个会话，封装来源计数、失败回滚和临时能力断言。"""
    def __init__(self, rpc, state, session, other_session):
        """主会话执行发现，另一会话验证视觉状态隔离。"""
        self.rpc = rpc
        self.state = state
        self.session = session
        self.other_session = other_session

    def select(self, model):
        """显式选择已有模型，不触发发现。"""
        self.rpc.call("session/set_model", {"sessionId": self.session, "modelId": model})

    def vision(self, session=None, **params):
        """读取或设置指定会话的视觉覆盖。"""
        return self.rpc.ext("x.ai/session/support_vision", {
            "sessionId": session or self.session, **params,
        })

    def queries(self, expected=ELIGIBLE, current="source-entry"):
        """精确计数可同时发现重复查询、漏查、越权查询和凭据串用。"""
        requests, _peak = self.state.snapshot()
        lists = [request for request in requests if request["method"] == "GET"]
        actual = Counter(request["source"] for request in lists)
        assert actual == Counter(expected), (actual, expected, requests)
        assert lists[0]["source"] == current, lists
        assert not any(request["method"] == "POST" for request in requests), requests

    def discover(self, target, hits=(), errors=None, expect_error=False,
                 expected_model="source-entry", delay=0):
        """每次清空请求记录；错误必须保留此前选择。"""
        self.select(expected_model)
        self.state.configure(target, hits, errors, delay)
        result = self.rpc.ext("x.ai/models/discover_and_switch", {
            "sessionId": self.session, "modelId": target, "expectedModelId": expected_model,
        }, expect_error=expect_error)
        if expect_error:
            assert self.vision()["modelId"] == expected_model, result
            text = json.dumps(result, ensure_ascii=False)
            assert target in text, text
            assert "fixture-upstream-secret" not in text, text
            assert all(conn.key not in text for conn in CONNECTIONS.values()), text
            return text
        assert result["models"]["currentModelId"] == target, result
        entry = next(model for model in result["models"]["availableModels"] if model["modelId"] == target)
        assert entry["_meta"]["runtimeDiscovered"] is True, entry
        assert entry["_meta"]["totalContextTokens"] == 256000, entry
        assert self.vision()["enabled"] is False, entry
        return result


def current_priority(check):
    """其他来源也命中时，当前来源成功后不能再查询。"""
    check.discover("fixture-priority", hits=ELIGIBLE)
    check.queries(expected=("source-entry",))


def fallback_miss(check):
    """当前仅返回近似 ID，仍需精确匹配其他来源。"""
    check.discover("fixture-fallback", hits=("backup-entry",))
    check.queries()


def current_http_error(check):
    """认证拒绝和服务错误均允许回退。"""
    for status in (401, 503):
        check.discover(f"fixture-current-{status}", hits=("backup-entry",), errors={"source-entry": status})
        check.queries()


def fallback_http_error(check):
    """单个回退失败不能阻止另一来源的唯一命中。"""
    check.discover("fixture-partial-failure", hits=("backup-entry",), errors={"other-key-entry": 503})
    check.queries()


def deduplicate(check):
    """当前与回退各有能力不同的别名，每个连接只能查询一次。"""
    check.discover("fixture-deduplicated", hits=("backup-entry",))
    check.queries()


def distinct_credentials(check):
    """同 URL、头和查询参数，只在第二套凭据下命中。"""
    check.discover("fixture-other-credential", hits=("other-key-entry",))
    check.queries()


def connection_fields(check):
    """相同 URL 和密钥，头或查询参数不同仍是独立连接。"""
    for source in ("header-entry", "query-entry"):
        check.discover("fixture-" + source, hits=(source,))
        check.queries()


def filtered_sources(check):
    """隐藏、禁用及不在允许列表的来源即使命中也不能参与。"""
    check.discover("fixture-filtered", hits=("backup-entry", *FORBIDDEN))
    check.queries()


def multiple_matches(check):
    """多个连接命中时按配置顺序选择首个来源，并沿用该来源查询。"""
    target = "fixture-multiple-matches"
    check.discover(target, hits=("backup-entry", "other-key-entry"))
    check.queries()
    check.discover("fixture-selected-source", hits=ELIGIBLE, expected_model=target)
    check.queries(expected=("backup-entry",), current="backup-entry")


def no_match(check):
    """全部未命中、混合失败、全部失败都保留原模型并报告原因。"""
    text = check.discover("fixture-all-missing", expect_error=True)
    check.queries()
    assert any(word in text for word in ("未找到", "未命中")), text
    text = check.discover(
        "fixture-mixed-missing", errors={"other-key-entry": 503}, expect_error=True,
    )
    check.queries()
    assert "503" in text and "/shared/v1" in text, text
    errors = {name: 503 for name in ELIGIBLE}
    errors["source-entry"] = 401
    text = check.discover("fixture-all-failed", errors=errors, expect_error=True)
    check.queries()
    assert "401" in text and "503" in text, text
    assert "/current/v1" in text and "/shared/v1" in text, text


def concurrency(check):
    """五个回退连接短暂延迟，检查并行执行且最多四个在途。"""
    check.discover("fixture-concurrent", hits=("backup-entry",), delay=0.5)
    check.queries()
    _requests, peak = check.state.snapshot()
    assert peak == 4, ("回退并发数应为 4", peak)


def vision_auth(check):
    """跨上游临时模型两轮推理，复查认证、视觉隔离及失败回滚。"""
    target = "fixture-cross-upstream"
    check.discover(target, hits=("backup-entry",))
    check.queries()
    assert check.vision(check.other_session)["enabled"] is True
    check.vision(expectedModelId=target, enabled=True)
    check.select("source-entry")
    assert check.vision()["enabled"] is True
    check.select(target)
    assert check.vision()["enabled"] is True
    check.rpc.call("session/set_model", {"sessionId": check.other_session, "modelId": target})
    assert check.vision(check.other_session)["enabled"] is False
    check.rpc.call("session/set_model", {"sessionId": check.other_session, "modelId": "source-entry"})
    check.vision(enabled=False)
    check.discover("fixture-runtime-missing", expected_model=target, expect_error=True)
    check.queries(current="backup-entry")
    assert check.vision()["enabled"] is False
    check.state.configure(target, hits=("backup-entry",))
    for enabled in (False, True):
        check.vision(enabled=enabled)
        assert check.vision(check.other_session)["enabled"] is True
        before = len(check.state.snapshot()[0])
        result = check.rpc.call("session/prompt", {"sessionId": check.session, "prompt": [
            {"type": "text", "text": "Reply only ok, do not call tools."},
            {"type": "image", "data": PNG, "mimeType": "image/png"},
        ]})
        assert result["stopReason"] == "end_turn", result
        requests = check.state.snapshot()[0][before:]
        posted = [request for request in requests if request["method"] == "POST"]
        assert posted, "临时模型未发起推理"
        for request in posted:
            body = request["body"]
            assert request["source"] == "backup-entry", request
            assert body.get("model") == target, body
            assert request["headers"]["authorization"] == "Bearer fixture-backup-key", request
            assert "reasoning_effort" not in body, body
            assert ("image_url" in json.dumps(body)) == enabled, (enabled, body)
        assert check.vision()["modelId"] == target


SCENARIOS = {
    "current-priority": current_priority,
    "fallback-miss": fallback_miss,
    "current-http-error": current_http_error,
    "fallback-http-error": fallback_http_error,
    "deduplicate": deduplicate,
    "distinct-credentials": distinct_credentials,
    "connection-fields": connection_fields,
    "filtered-sources": filtered_sources,
    "multiple-matches": multiple_matches,
    "no-match": no_match,
    "concurrency": concurrency,
    "vision-auth": vision_auth,
}


def run_agent(exe, root, server, scenarios):
    """整个测试只启动一次 Release 程序，配置与日志均在临时目录。"""
    home = root / "home"
    cwd = root / "work"
    home.mkdir()
    cwd.mkdir()
    config = make_config(server.server_port)
    config_path = home / "config.toml"
    config_path.write_text(config, encoding="utf-8")
    original = tomllib.loads(config)
    env = {key: value for key, value in os.environ.items() if not key.startswith(("GROK_", "XAI_"))}
    env.update(
        GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
        APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
        XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
        GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false",
    )
    with (root / "stderr.log").open("w+", encoding="utf-8") as stderr:
        process = subprocess.Popen(
            [exe, "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
            text=True, encoding="utf-8",
        )
        try:
            rpc = Rpc(process)
            rpc.call("initialize", {
                "protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                "_meta": {"clientType": "grok-pager", "startupHints": {
                    "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True,
                }},
            })
            rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
            sessions = [rpc.call("session/new", {
                "cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "source-entry"},
            })["sessionId"] for _ in range(2)]
            check = Checks(rpc, server.state, *sessions)
            for name in scenarios:
                print("运行场景：" + name, flush=True)
                SCENARIOS[name](check)
                saved = tomllib.loads(config_path.read_text(encoding="utf-8"))
                assert saved["models"] == original["models"], "默认模型或模型过滤配置被修改"
                assert saved["model"] == original["model"], "临时模型或视觉覆盖被写入配置"
                print("PASS: " + name + "；模型配置未改变", flush=True)
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
    """默认运行全部场景，可重复指定 --scenario 做快速回归。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序")
    parser.add_argument("--scenario", action="append", choices=SCENARIOS, help="仅运行指定场景，可重复")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file():
        parser.error("Release 程序不存在，请先完成正式构建：" + str(exe))
    if "debug" in (part.lower() for part in exe.parts):
        parser.error("此测试只允许使用 Release 程序")
    # 测试产物放在系统临时目录，随场景结束清理。
    scratch = Path(tempfile.gettempdir())
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    server.state = MockState()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    scenarios = list(dict.fromkeys(args.scenario or SCENARIOS))
    try:
        with tempfile.TemporaryDirectory(prefix="grok-model-smoke-", dir=scratch) as root:
            run_agent(str(exe), Path(root), server, scenarios)
        print(f"PASS: {len(scenarios)} 个场景；跨上游发现与视觉回归完成")
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


if __name__ == "__main__":
    main()
