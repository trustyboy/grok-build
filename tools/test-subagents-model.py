"""使用真实 Release 后端、本地假上游和隔离 ACP 会话验证子代理模型选择。"""
import argparse
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
from urllib.parse import urlsplit


# 复用现有 ACP 客户端，不在仓库生成导入缓存。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "subagents_model_discovery_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)


@dataclass(frozen=True)
class Model:
    """目录 ID、线上模型名、接口和凭据刻意不同，以发现错误路由。"""
    name: str
    backend: str = "chat_completions"
    effort: str | None = "low"
    window: int = 128000

    @property
    def wire(self):
        """返回实际发送给上游的模型名。"""
        return self.name + "-wire"

    @property
    def path(self):
        """每个已配置来源使用独立 URL。"""
        return "/" + self.name + "/v1"

    @property
    def key(self):
        """测试凭据不来自用户环境。"""
        return "fixture-" + self.name + "-key"


MODELS = {model.name: model for model in (
    Model("parent"), Model("alternate", effort="medium", window=192000),
    Model("global", "responses", "medium", 192000),
    Model("temporary", effort="medium"), Model("plain", effort=None), Model("limited"),
)}
DISCOVERED = "fixture-discovered-child"
DISCOVERED_PARENT = "fixture-discovered-parent"


@dataclass(frozen=True)
class Profile:
    """全局值与主模型专属值独立；旧 section 仅供兼容场景使用。"""
    model: str | None = None
    effort: str | None = None
    parent_models: tuple[tuple[str, str], ...] = ()
    section: str = "subagent"
    legacy_model: str | None = None
    legacy_effort: str | None = None


PROFILES = {
    "inherit": Profile(),
    "effort": Profile(effort="high"),
    "model": Profile(model="global"),
    "global": Profile(model="global", effort="high"),
    "parent-model": Profile(model="global", effort="high", parent_models=(
        ("parent", "temporary"), ("alternate", "plain"),
        ("temporary", "   "), ("limited", "fixture-invalid-static"),
    )),
    "parent-model-only": Profile(model="global", parent_models=(("parent", "temporary"),)),
    "legacy-section": Profile(model="global", effort="high", section="subagents"),
    "mixed-sections": Profile(model="global", legacy_model="plain", legacy_effort="high"),
}


def text_content(value):
    """提取纯文本，兼容 Chat Completions 与 Responses 的内容块。"""
    if isinstance(value, str):
        return value
    if isinstance(value, list):
        return "\n".join(text_content(item) for item in value)
    if isinstance(value, dict):
        return text_content(value.get("text", value.get("content", "")))
    return ""


class MockState:
    """串行场景记录真实请求，子代理响应门闩用于检查运行中的实际窗口。"""
    def __init__(self):
        """所有请求和故障记录在当前临时测试进程内。"""
        self.lock = threading.Lock()
        self.requests = []
        self.failures = []
        self.sequence = 0
        self.parent_marker = ""
        self.child_marker = ""
        self.spawn_args = {}
        self.spawn_sent = False
        self.child_started = threading.Event()
        self.release_child = threading.Event()

    def prepare(self, **spawn_args):
        """每次启动使用新标记，恢复子代理时旧历史不会误认成本次请求。"""
        with self.lock:
            self.sequence += 1
            self.parent_marker = f"fixture-parent-{self.sequence:04d}"
            self.child_marker = f"fixture-child-{self.sequence:04d}"
            self.spawn_args = spawn_args
            self.spawn_sent = False
            self.child_started.clear()
            self.release_child.clear()
            return self.parent_marker

    def record(self, method, path, headers, body=None):
        """路由和认证均严格校验，不能用错误凭据获得固定成功回复。"""
        url = urlsplit(path)
        source = next((model for model in MODELS.values()
                       if url.path.startswith(model.path + "/")), None)
        suffix = "/models" if method == "GET" else (
            "/responses" if source and source.backend == "responses" else "/chat/completions"
        )
        record = {
            "method": method, "path": path, "source": source.name if source else None,
            "authorization": headers.get("Authorization"), "body": body,
            "turnIndex": headers.get("x-grok-turn-idx"),
        }
        with self.lock:
            self.requests.append(record)
        assert source is not None, "未知上游路径：" + path
        assert url.path == source.path + suffix and not url.query, "接口路径错误：" + path
        assert record["authorization"] == "Bearer " + source.key, "上游认证错误：" + source.name
        if body is not None:
            allowed = {source.wire}
            if source.name == "global":
                allowed.add(DISCOVERED)
            if source.name == "parent":
                allowed.add(DISCOVERED_PARENT)
            assert body.get("model") in allowed, (
                "实际模型与上游不匹配", source.name, body.get("model")
            )
        return source, record

    def reply(self, payload, record):
        """主代理只发一次真实 spawn 调用，子代理返回固定文本且不调用工具。"""
        # 标题等辅助请求没有轮次头，不能误触发子代理或占用响应门闩。
        if record["turnIndex"] is None:
            record["role"] = "auxiliary"
            return "fixture-title", None
        messages = payload.get("messages", payload.get("input", []))
        users = [text_content(message.get("content")) for message in messages
                 if isinstance(message, dict) and message.get("role") == "user"]
        with self.lock:
            role = next(("child" if self.child_marker in text else "parent"
                         for text in reversed(users)
                         if self.child_marker in text or self.parent_marker in text), None)
            assert role is not None, "收到非测试轮次推理；请检查自动标题或摘要是否关闭"
            record["role"] = role
            if role == "child":
                self.child_started.set()
                return "child-ok", None
            if self.spawn_sent:
                return "parent-ok", None
            tools = [tool.get("function", tool) for tool in payload.get("tools", [])]
            tool = next((tool for tool in tools if tool.get("name") == "spawn_subagent"), None)
            assert tool is not None, ("主代理未获得 spawn_subagent 工具", [tool.get("name") for tool in tools])
            properties = tool.get("parameters", {}).get("properties", {})
            background = next((key for key in ("background", "run_in_background") if key in properties), None)
            assert background, ("spawn 参数不匹配", list(properties))
            arguments = {
                "description": self.parent_marker,
                "prompt": self.child_marker + "：仅回复 child-ok，不调用工具。",
                background: True,
                **self.spawn_args,
            }
            self.spawn_sent = True
            return None, {
                "index": 0, "id": f"fixture_call_{self.sequence}", "type": "function",
                "function": {"name": tool["name"], "arguments": json.dumps(arguments)},
            }

    def fail(self, error):
        """HTTP 线程异常也必须让主线程失败，不能被重试掩盖。"""
        with self.lock:
            self.failures.append(str(error))
        self.release_child.set()

    def snapshot(self):
        """返回副本，避免断言遍历期间 HTTP 线程追加记录。"""
        with self.lock:
            return [dict(request) for request in self.requests], list(self.failures)


class Upstream(BaseHTTPRequestHandler):
    """只监听本机；支持主代理 Chat 工具流和子代理两种 API 的固定回复。"""
    timeout = 10

    def log_message(self, *_args):
        """失败时只输出针对性诊断，关闭默认 HTTP 访问日志。"""
        pass

    def respond(self, status, body, content_type="application/json"):
        """发送完整长度，避免客户端等待连接关闭。"""
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        try:
            self.wfile.write(body)
        except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError) as error:
            self.server.state.fail(error)

    def do_GET(self):
        """启动探针不属于模型发现，目录查询仍严格校验来源认证。"""
        if self.path == "/":
            self.respond(200, b'{"status":"ok"}')
            return
        try:
            source, _record = self.server.state.record("GET", self.path, self.headers)
            ids = [source.wire, DISCOVERED if source.name == "global" else DISCOVERED + "-preview",
                   DISCOVERED_PARENT if source.name == "parent" else DISCOVERED_PARENT + "-preview"]
            self.respond(200, json.dumps({"data": [{"id": name} for name in ids]}).encode())
        except Exception as error:
            self.server.state.fail(error)
            self.respond(400, b'{"error":"fixture GET rejected"}')

    def do_POST(self):
        """暂停子代理响应，真实 ACP 运行状态可在完成前读取窗口。"""
        try:
            payload = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            source, record = self.server.state.record("POST", self.path, self.headers, payload)
            text, tool_call = self.server.state.reply(payload, record)
            if record["role"] == "child":
                assert self.server.state.release_child.wait(60), "等待子代理窗口检查超时"
            if source.backend == "responses":
                assert tool_call is None, "Responses 来源仅用于子代理固定回复"
                response = {
                    "id": "resp_fixture", "object": "response", "created_at": 1,
                    "model": payload["model"], "status": "completed",
                    "output": [{"id": "msg_fixture", "type": "message", "role": "assistant",
                                "status": "completed", "content": [
                                    {"type": "output_text", "text": text, "annotations": []}]}],
                    "usage": {"input_tokens": 10, "input_tokens_details": {"cached_tokens": 0},
                              "output_tokens": 1, "output_tokens_details": {"reasoning_tokens": 0},
                              "total_tokens": 11},
                }
                if not payload.get("stream"):
                    self.respond(200, json.dumps(response).encode())
                    return
                event = {"type": "response.completed", "sequence_number": 0, "response": response}
                stream = "event: response.completed\ndata: " + json.dumps(event) + "\n\n"
            else:
                delta = {"role": "assistant"}
                delta.update({"tool_calls": [tool_call]} if tool_call else {"content": text})
                chunks = [{"id": "fixture", "object": "chat.completion.chunk", "created": 1,
                           "model": payload["model"], "choices": [{"index": 0, "delta": delta,
                           "finish_reason": "tool_calls" if tool_call else "stop"}]}]
                stream = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
                stream += "data: [DONE]\n\n"
            self.respond(200, stream.encode(), "text/event-stream")
        except Exception as error:
            self.server.state.fail(error)
            self.respond(400, b'{"error":"fixture POST rejected"}')


def make_config(port, profile):
    """隔离配置覆盖全局、主模型专属与新旧 section 的字段合并。"""
    profile = PROFILES[profile]
    config = '''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
auto_wake = false
title_refresh = false
session_recap = false
turn_summary = false
subagent_model_inheritance = false
[cli]
use_leader = false
auto_update = false
[models]
default = "parent"
allowed_models = ["parent", "alternate", "global", "temporary", "plain", "limited", "fixture-*"]
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
'''
    config += f'[{profile.section}]\nenabled = true\n'
    if profile.model is not None:
        config += f'model = "{profile.model}"\n'
    if profile.effort is not None:
        config += f'reasoning_effort = "{profile.effort}"\n'
    if profile.legacy_model is not None or profile.legacy_effort is not None:
        config += '[subagents]\n'
        if profile.legacy_model is not None:
            config += f'model = "{profile.legacy_model}"\n'
        if profile.legacy_effort is not None:
            config += f'reasoning_effort = "{profile.legacy_effort}"\n'
    parent_models = dict(profile.parent_models)
    for model in MODELS.values():
        config += f'''
[model.{model.name}]
model = "{model.wire}"
name = "Fixture {model.name}"
base_url = "http://127.0.0.1:{port}{model.path}"
api_key = "{model.key}"
api_backend = "{model.backend}"
context_window = {model.window}
context_windows = [{model.window}, 256000]
supports_vision = false
'''
        if model.name in parent_models:
            config += f'subagent_model = "{parent_models[model.name]}"\n'
        if model.effort is None:
            config += "supports_reasoning_effort = false\n"
        else:
            efforts = [model.effort] if model.name == "limited" else ["low", "medium", "high"]
            config += "reasoning_efforts = [" + ", ".join(
                '{ value = "' + effort + '", default = ' + str(effort == model.effort).lower() + " }"
                for effort in efforts
            ) + "]\n"
    return config


class Checks:
    """状态与真实 child 推理同时断言，来源检查使用接口标识而非 UI 文案。"""
    def __init__(self, rpc, mock, sessions, profile):
        """记录两个父会话各自模型，防止全局 manager 状态串用。"""
        self.rpc, self.mock, self.sessions = rpc, mock, sessions
        self.profile = PROFILES[profile]
        self.parents = {session: "parent" for session in sessions}
        self.observations = []

    def state(self, action="status", session=None, expect_error=False, **params):
        """调用正式扩展；修改携带父模型快照以覆盖过期请求保护。"""
        session = session or self.sessions[0]
        request = {"sessionId": session, "action": action}
        if action != "status":
            request["expectedModelId"] = self.parents[session]
        return self.rpc.ext("x.ai/session/subagent_model", {**request, **params}, expect_error)

    def select_parent(self, model, session=None):
        """只通过正式主模型切换入口改变父会话。"""
        session = session or self.sessions[0]
        self.rpc.call("session/set_model", {"sessionId": session, "modelId": model,
                                            "_meta": {"reasoningEffort": MODELS[model].effort}})
        self.parents[session] = model

    def discover_parent(self, session=None):
        """从带专属子代理配置的来源发现新主模型，验证不会借用来源字段。"""
        session = session or self.sessions[0]
        before = len(self.mock.snapshot()[0])
        result = self.rpc.ext("x.ai/models/discover_and_switch", {
            "sessionId": session, "modelId": DISCOVERED_PARENT,
            "expectedModelId": self.parents[session],
        })
        assert result["models"]["currentModelId"] == DISCOVERED_PARENT, result
        queries = [request for request in self.mock.snapshot()[0][before:] if request["method"] == "GET"]
        assert queries and all(request["source"] == "parent" for request in queries), queries
        self.parents[session] = DISCOVERED_PARENT
        self.parent_unchanged(session)

    def parent_unchanged(self, session):
        """视觉状态接口返回当前会话模型，不依赖共享的默认模型。"""
        current = self.rpc.ext("x.ai/session/support_vision", {"sessionId": session})
        assert current["modelId"] == self.parents[session], ("子代理操作修改了主模型", current)
        info = self.rpc.ext("x.ai/session/info", {"sessionId": session})
        window = 256000 if self.parents[session] == DISCOVERED_PARENT else MODELS[self.parents[session]].window
        assert info["context"]["total"] == window, (
            "子代理操作修改了父会话窗口", info["context"]["total"]
        )

    def effective(self, model, effort, window=None, session=None, warning=False, model_source=None):
        """检查默认 general-purpose 的实际值及可选配置值。"""
        session = session or self.sessions[0]
        state = self.state(session=session)
        expected_window = window or (256000 if model == DISCOVERED else MODELS[model].window)
        assert (state["modelId"], state["reasoningEffort"], state["contextWindow"]) == (
            model, effort, expected_window
        ), state
        assert state["configuredModel"] == self.profile.model, state
        assert state["configuredEffort"] == (self.profile.effort or self.profile.legacy_effort), state
        assert state.get("parentModelId") == self.parents[session], state
        parent_configured = dict(self.profile.parent_models).get(self.parents[session])
        if parent_configured is None or parent_configured.strip():
            assert state.get("parentConfiguredModel") == parent_configured, state
        else:
            assert not (state.get("parentConfiguredModel") or "").strip(), state
        assert isinstance(state["modelSource"], str) and isinstance(state["effortSource"], str), state
        if model_source is not None:
            assert state["modelSource"] == model_source, state
        assert any(item["modelId"] == model for item in state["availableModels"]), state
        if warning:
            assert state["warning"], state
        self.parent_unchanged(session)
        return state

    def failed_set(self, **params):
        """非法输入失败后，模型、effort 和窗口及配置来源必须保持原值。"""
        before = self.state()
        self.state("set", expect_error=True, **params)
        after = self.state()
        for key in ("modelId", "reasoningEffort", "contextWindow", "modelSource", "effortSource",
                    "configuredModel", "configuredEffort", "parentModelId", "parentConfiguredModel"):
            assert after.get(key) == before.get(key), (key, before, after)
        self.parent_unchanged(self.sessions[0])

    def spawn(self, expected_model, effort, window=None, session=None, **spawn_args):
        """真实 parent 发工具调用，查询运行中 child，再放行并确认完成与实际请求。"""
        model = expected_model
        session = session or self.sessions[0]
        window = window or (256000 if model == DISCOVERED else MODELS[model].window)
        before = len(self.mock.snapshot()[0])
        marker = self.mock.prepare(**spawn_args)
        child_id = None
        try:
            result = self.rpc.call("session/prompt", {
                "sessionId": session,
                "prompt": [{"type": "text", "text": marker + "：委派独立子任务，仅报告已经启动。"}],
            })
            assert result["stopReason"] == "end_turn", result
            deadline = time.monotonic() + 25
            while time.monotonic() < deadline:
                _requests, failures = self.mock.snapshot()
                assert not failures, failures
                running = self.rpc.ext("x.ai/subagent/list_running", {"sessionId": session})["subagents"]
                child = next((item for item in running if item["description"] == marker), None)
                if child and self.mock.child_started.is_set():
                    child_id = child["subagentId"]
                    actual_window = child["contextWindowTokens"]
                    if actual_window:
                        assert child["parentSessionId"] == session, child
                        assert actual_window == window, ("实际 child 上下文窗口不符", child, window)
                        break
                time.sleep(0.05)
            else:
                raise AssertionError("未观察到真实运行的子代理和上下文窗口：" + marker)
        finally:
            self.mock.release_child.set()
        completed = self.rpc.ext("x.ai/subagent/get", {
            "subagentId": child_id, "block": True, "timeoutMs": 30000,
        })["snapshot"]
        assert completed and completed["status"] == "completed", completed
        assert "child-ok" in completed["output"], completed
        if "resume_from" in spawn_args:
            assert completed.get("resumedFrom") == spawn_args["resume_from"], completed
        requests, failures = self.mock.snapshot()
        assert not failures, failures
        posts = [request for request in requests[before:] if request["method"] == "POST"]
        child_posts = [request for request in posts if request.get("role") == "child"]
        parent_posts = [request for request in posts if request.get("role") == "parent"]
        assert child_posts and parent_posts, "必须同时观察到真实 parent 和 child 请求"
        source = MODELS["global" if model == DISCOVERED else model]
        for request in child_posts:
            body = request["body"]
            assert request["source"] == source.name, (request["path"], request["source"], source.name)
            assert request["authorization"] == "Bearer " + source.key, request["source"]
            assert body["model"] == (DISCOVERED if model == DISCOVERED else source.wire), body["model"]
            reasoning = body.get("reasoning") or {}
            actual_effort = reasoning.get("effort") if source.backend == "responses" else body.get("reasoning_effort")
            assert actual_effort == effort, ("实际 child effort 不符", actual_effort, effort)
            if effort is None:
                assert "effort" not in reasoning and "reasoning_effort" not in body, "无推理能力时必须省略 effort"
            self.observations.append({"childId": child_id, "model": body["model"],
                                      "api": source.backend, "source": source.name,
                                      "authMatched": True, "effort": actual_effort, "contextWindow": window})
        for request in parent_posts:
            discovered = self.parents[session] == DISCOVERED_PARENT
            parent = MODELS["parent" if discovered else self.parents[session]]
            wire = DISCOVERED_PARENT if discovered else parent.wire
            assert request["source"] == parent.name and request["body"]["model"] == wire, request["source"]
            assert request["authorization"] == "Bearer " + parent.key, request["source"]
            expected_effort = None if discovered else parent.effort
            assert request["body"].get("reasoning_effort") == expected_effort, request["body"].get("reasoning_effort")
        self.parent_unchanged(session)
        return child_id


def default_inherit(check):
    """双字段省略时跟随父模型、effort 与窗口，父模型切换后重新读取。"""
    check.effective("parent", "low")
    check.spawn("parent", "low")
    check.select_parent("alternate")
    check.effective("alternate", "medium")
    check.spawn("alternate", "medium")


def effort_only(check):
    """只配置 effort，不改变继承模型；临时模型采用自己的基础窗口。"""
    check.effective("parent", "high")
    check.spawn("parent", "high")
    check.state("set", modelId="global")
    check.effective("global", "high")
    check.spawn("global", "high")
    check.state("inherit")
    check.select_parent("alternate")
    check.effective("alternate", "high")
    check.spawn("alternate", "high")


def global_selection(check):
    """全局模型使用自身 Responses 路由及凭据，主代理仍使用 Chat。"""
    check.effective("global", "high")
    check.spawn("global", "high")


def model_only(check):
    """只设全局模型时采用目标默认 effort，不继承父会话的 low。"""
    check.effective("global", "medium")
    check.spawn("global", "medium")


def temporary_lifecycle(check):
    """临时参数、两会话隔离、主模型变化、inherit/reset 和单次模型优先。"""
    first, second = check.sessions
    check.state("set", modelId="temporary", reasoningEffort="low", contextWindow=256000)
    check.effective("temporary", "low", 256000)
    check.spawn("temporary", "low", 256000)
    check.effective("global", "high", session=second)
    check.spawn("global", "high", session=second)
    check.select_parent("alternate", first)
    check.effective("temporary", "low", 256000)
    check.spawn("temporary", "low", 256000)
    # 显式改选其他模型时，不得泄漏临时目标的 effort 或窗口。
    check.spawn("global", "high", model="global")
    check.state("inherit")
    check.effective("alternate", "high")
    check.spawn("alternate", "high")
    check.select_parent("parent")
    check.effective("parent", "high")
    check.spawn("parent", "high")
    check.state("reset")
    check.effective("global", "high")
    check.spawn("global", "high")
    check.effective("global", "high", session=second)


def parent_model_selection(check):
    """实时目录键决定专属目标，双父会话独立，模型名、接口与凭据不混用。"""
    first, second = check.sessions
    check.effective("temporary", "high", model_source="parent_model_config")
    check.spawn("temporary", "high")
    check.select_parent("alternate", second)
    check.effective("plain", None, session=second, warning=True, model_source="parent_model_config")
    check.spawn("plain", None, session=second)
    check.effective("temporary", "high", session=first, model_source="parent_model_config")
    check.spawn("temporary", "high", session=first)
    check.select_parent("alternate", first)
    check.effective("plain", None, model_source="parent_model_config", warning=True)
    check.spawn("plain", None)
    check.select_parent("parent", second)
    check.effective("temporary", "high", session=second, model_source="parent_model_config")
    check.spawn("temporary", "high", session=second)
    check.effective("plain", None, model_source="parent_model_config", warning=True)


def parent_model_only(check):
    """主模型字段只选 model；无全局 effort 时采用目标自身默认值。"""
    check.effective("temporary", "medium", model_source="parent_model_config")
    check.spawn("temporary", "medium")
    check.select_parent("alternate")
    check.effective("global", "medium", model_source="config")
    check.spawn("global", "medium")


def parent_model_fallback(check):
    """省略、空白及无效专属目标都回退全局，无效值必须告警且不触发发现。"""
    before = len(check.mock.snapshot()[0])
    for parent, warning in (("plain", False), ("temporary", False), ("limited", True)):
        check.select_parent(parent)
        state = check.effective("global", "high", model_source="config", warning=warning)
        if warning:
            assert "fixture-invalid-static" in state["warning"], state
        check.spawn("global", "high")
    assert not any(request["method"] == "GET" for request in check.mock.snapshot()[0][before:]), (
        "静态无效目标不能触发上游发现"
    )
    check.select_parent("parent")
    check.effective("temporary", "high", model_source="parent_model_config")
    check.spawn("temporary", "high")


def parent_model_lifecycle(check):
    """set 保持、inherit 跳过专属配置、reset 读取当前父模型，不影响其他会话。"""
    first, second = check.sessions
    check.state("set", modelId="global", reasoningEffort="low", contextWindow=256000)
    check.effective("global", "low", 256000, model_source="session")
    check.spawn("global", "low", 256000)
    check.effective("temporary", "high", session=second, model_source="parent_model_config")
    check.select_parent("alternate", first)
    check.effective("global", "low", 256000, model_source="session")
    check.spawn("global", "low", 256000)
    check.spawn("temporary", "high", model="temporary")
    check.state("reset")
    check.effective("plain", None, model_source="parent_model_config", warning=True)
    check.spawn("plain", None)
    check.state("set", modelId="temporary", reasoningEffort="low", contextWindow=256000)
    check.state("inherit")
    check.effective("alternate", "high")
    check.spawn("alternate", "high")
    check.select_parent("parent")
    check.effective("parent", "high")
    check.spawn("parent", "high")
    check.state("reset")
    check.effective("temporary", "high", model_source="parent_model_config")
    check.spawn("temporary", "high")
    check.effective("temporary", "high", session=second, model_source="parent_model_config")
    check.spawn("temporary", "high", session=second)


def parent_model_discovery(check):
    """临时发现主模型不借用来源模型专属值，仍保持两会话隔离及正确认证。"""
    check.effective("temporary", "high", model_source="parent_model_config")
    check.discover_parent()
    check.effective("global", "high", model_source="config")
    check.spawn("global", "high")
    check.effective("temporary", "high", session=check.sessions[1], model_source="parent_model_config")
    check.spawn("temporary", "high", session=check.sessions[1])
    check.select_parent("parent")
    check.effective("temporary", "high", model_source="parent_model_config")
    check.spawn("temporary", "high")


def unsupported_effort(check):
    """无推理能力或缺少 high 档位时告警并保留各自基础值。"""
    for model, effort in (("plain", None), ("limited", "low")):
        check.state("set", modelId=model)
        check.effective(model, effort, warning=True)
        check.spawn(model, effort)


def invalid_parameters(check):
    """零窗口、未声明窗口、非法 effort 和过期父模型快照均不能破坏选择。"""
    check.state("set", modelId="temporary", reasoningEffort="low", contextWindow=256000)
    for params in ({"contextWindow": 0}, {"contextWindow": 12345},
                   {"reasoningEffort": "invalid"}, {"expectedModelId": "alternate"}):
        check.failed_set(modelId="temporary", **params)
    check.failed_set(modelId="plain", reasoningEffort="high")
    check.failed_set(modelId="limited", reasoningEffort="high")
    check.failed_set(modelId=DISCOVERED, contextWindow=256000)
    check.effective("temporary", "low", 256000)
    check.spawn("temporary", "low", 256000)


def discovery_auth(check):
    """未知单 ID 跨来源发现，不改主模型；失败保留已选项且不写配置。"""
    before = len(check.mock.snapshot()[0])
    check.state("set", modelId=DISCOVERED)
    queries = [request for request in check.mock.snapshot()[0][before:] if request["method"] == "GET"]
    assert queries and queries[0]["source"] == "parent", [(item["path"], item["source"]) for item in queries]
    assert any(request["source"] == "global" for request in queries), "未查询独立认证的回退来源"
    check.effective(DISCOVERED, None, warning=True)
    check.spawn(DISCOVERED, None)
    check.effective("global", "high", session=check.sessions[1])
    check.failed_set(modelId="fixture-never-exists")
    check.effective(DISCOVERED, None, warning=True)
    check.spawn(DISCOVERED, None)


def resume_original(check):
    """临时选择改变后，新建 child 使用新选择；恢复 child 仍使用原模型。"""
    check.state("set", modelId="temporary", reasoningEffort="medium")
    original = check.spawn("temporary", "medium")
    check.state("set", modelId="global")
    check.effective("global", "high")
    check.spawn("global", "high")
    check.spawn("temporary", "medium", resume_from=original)


SCENARIOS = {
    "default-inherit": ("inherit", default_inherit),
    "effort-only": ("effort", effort_only),
    "global-selection": ("global", global_selection),
    "model-only": ("model", model_only),
    "legacy-section": ("legacy-section", global_selection),
    "mixed-sections": ("mixed-sections", global_selection),
    "parent-model-selection": ("parent-model", parent_model_selection),
    "parent-model-only": ("parent-model-only", parent_model_only),
    "parent-model-fallback": ("parent-model", parent_model_fallback),
    "parent-model-lifecycle": ("parent-model", parent_model_lifecycle),
    "parent-model-discovery": ("parent-model", parent_model_discovery),
    "temporary-lifecycle": ("global", temporary_lifecycle),
    "unsupported-effort": ("global", unsupported_effort),
    "invalid-parameters": ("global", invalid_parameters),
    "discovery-auth": ("global", discovery_auth),
    "resume-original": ("global", resume_original),
}


def diagnostics(root, stderr, mock, check):
    """完整记录仅写临时目录；终端保留简短请求摘要和相关错误尾部。"""
    requests, failures = mock.snapshot()
    (root / "requests.json").write_text(json.dumps(requests, ensure_ascii=False, indent=2), encoding="utf-8")
    print("假上游错误：" + json.dumps(failures, ensure_ascii=False), file=sys.stderr)
    for request in requests[-12:]:
        body = request.get("body") or {}
        print(json.dumps({"method": request["method"], "path": request["path"],
                          "source": request["source"], "role": request.get("role"),
                          "model": body.get("model"), "reasoning": body.get("reasoning", body.get("reasoning_effort"))},
                         ensure_ascii=False), file=sys.stderr)
    if check:
        print("子代理实测：" + json.dumps(check.observations[-6:], ensure_ascii=False), file=sys.stderr)
    stderr.flush()
    stderr.seek(0)
    lines = stderr.read().splitlines()
    relevant = [line for line in lines if any(word in line.lower() for word in (
        "error", "warn", "panic", "subagent", "reasoning", "context_window"))]
    print("\n".join((relevant or lines)[-35:])[-9000:], file=sys.stderr)


def run_scenario(exe, root, name):
    """每个场景独立进程与配置，结束前先放行 HTTP，再回收进程和临时资源。"""
    profile, scenario = SCENARIOS[name]
    home, cwd = root / "home", root / "work"
    home.mkdir()
    cwd.mkdir()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
    server.state = MockState()
    server.daemon_threads = False
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    process, check = None, None
    try:
        config_path = home / "config.toml"
        config = make_config(server.server_port, profile)
        config_path.write_text(config, encoding="utf-8")
        original = tomllib.loads(config)
        env = {key: value for key, value in os.environ.items()
               if not key.upper().startswith(("GROK_", "XAI_", "OTEL_"))}
        env.update(GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
                   TEMP=str(root), TMP=str(root), TMPDIR=str(root),
                   XDG_CONFIG_HOME=str(root / "config"), XDG_CACHE_HOME=str(root / "cache"),
                   APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
                   XAI_API_KEY="fixture-global-unused", GROK_TURN_SUMMARY="false",
                   GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false",
                   NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
        with (root / "stderr.log").open("w+", encoding="utf-8") as stderr:
            try:
                process = subprocess.Popen([str(exe), "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
                                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                                           text=True, encoding="utf-8")
                rpc = FIXTURE.Rpc(process)
                rpc.call("initialize", {
                    "protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
                    "_meta": {"clientType": "grok-pager", "startupHints": {
                        "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True}},
                })
                rpc.call("authenticate", {"methodId": "xai.api_key", "_meta": {"headless": True}})
                sessions = [rpc.call("session/new", {
                    "cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "parent", "yoloMode": True},
                })["sessionId"] for _ in range(2)]
                check = Checks(rpc, server.state, sessions, profile)
                scenario(check)
                saved = tomllib.loads(config_path.read_text(encoding="utf-8"))
                for section in ("models", "model", "subagent", "subagents"):
                    assert saved.get(section) == original.get(section), "子代理操作修改了配置：" + section
                assert not server.state.snapshot()[1], server.state.snapshot()[1]
                print(f"PASS: {name}；{len(check.observations)} 次 child 实际请求；配置未改变", flush=True)
            except Exception:
                diagnostics(root, stderr, server.state, check)
                raise
            finally:
                server.state.release_child.set()
                if process is not None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()
                    process.stdin.close()
                    process.stdout.close()
    finally:
        server.state.release_child.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


def main():
    """不触发构建；默认运行全部场景，临时日志无论成功失败均随目录清理。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序")
    parser.add_argument("--scenario", action="append", choices=SCENARIOS, help="仅运行指定场景，可重复")
    args = parser.parse_args()
    exe = Path(args.exe).resolve()
    if not exe.is_file() or "debug" in (part.lower() for part in exe.parts):
        parser.error("请先通过正式入口构建 Release 程序：" + str(exe))
    scenarios = list(dict.fromkeys(args.scenario or SCENARIOS))
    for name in scenarios:
        print("运行场景：" + name, flush=True)
        with tempfile.TemporaryDirectory(prefix="grok-subagents-model-", dir=tempfile.gettempdir()) as root:
            run_scenario(exe, Path(root), name)
    print(f"PASS: {len(scenarios)} 个子代理模型 ACP 场景", flush=True)


if __name__ == "__main__":
    main()
