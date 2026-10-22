"""仅在编译服务器运行，验证历史列表、搜索及按 ID 恢复不会探测无关项目目录。"""
import argparse
from concurrent.futures import Future, TimeoutError
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from urllib.parse import quote
import uuid


# 复用现有 ACP 客户端，不在源码目录生成字节码。
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location(
    "model_discovery_fixture", Path(__file__).with_name("test-model-discovery.py")
)
FIXTURE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = FIXTURE
SPEC.loader.exec_module(FIXTURE)

CONFIG = '''[features]
remote_fetch = false
managed_config = false
telemetry = false
feedback = false
codebase_indexing = false
session_search = false
turn_summary = false
session_recap = false
auto_wake = false
[cli]
use_leader = false
auto_update = false
session_registry = false
[models]
default = "history-fixture"
allowed_models = ["history-fixture"]
[model.history-fixture]
model = "history-fixture"
base_url = "http://127.0.0.1:9/v1"
api_key = "fixture-unused-key"
api_backend = "chat_completions"
context_window = 32000
[memory]
enabled = false
[worktree.auto_gc]
enabled = false
[telemetry]
trace_upload = false
mixpanel_enabled = false
otel_enabled = false
'''


def call_before(rpc, method, params, deadline):
    """给现有 Rpc 加上共同期限，避免默认 90 秒掩盖网络盘阻塞。"""
    remaining = deadline - time.monotonic()
    assert remaining > 0, "调用总期限已到：" + method
    result = Future()

    def invoke():
        """转交响应或异常；超时后主线程负责终止该 ACP 进程。"""
        try:
            result.set_result(rpc.call(method, params))
        except Exception as error:
            result.set_exception(error)

    threading.Thread(target=invoke, daemon=True).start()
    try:
        return result.result(timeout=remaining)
    except TimeoutError:
        raise AssertionError("调用超时，可能访问了历史项目目录：" + method) from None


def start_agent(exe, cwd, env, stderr_path, processes):
    """启动独立 stdio 后端，只使用测试配置及虚构凭据。"""
    with stderr_path.open("w", encoding="utf-8") as stderr:
        process = subprocess.Popen(
            [str(exe), "agent", "--no-leader", "stdio"], cwd=cwd, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
            text=True, encoding="utf-8",
        )
    processes.append(process)
    rpc = FIXTURE.Rpc(process)
    deadline = time.monotonic() + 20
    call_before(rpc, "initialize", {
        "protocolVersion": 1, "clientCapabilities": {"fs": {}, "terminal": False},
        "_meta": {"clientType": "grok-pager", "startupHints": {
            "nonInteractive": True, "skipGitStatus": True, "skipProjectLayout": True,
        }},
    }, deadline)
    call_before(rpc, "authenticate", {
        "methodId": "xai.api_key", "_meta": {"headless": True},
    }, deadline)
    return process, rpc


def stop(process):
    """仅终止本脚本创建的进程及其子进程，不按程序名称清理。"""
    if process.poll() is None:
        try:
            subprocess.run(
                [str(Path(os.environ["SystemRoot"]) / "System32" / "taskkill.exe"),
                 "/PID", str(process.pid), "/T", "/F"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5,
            )
        finally:
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
    for stream in (process.stdin, process.stdout):
        if stream is not None:
            stream.close()


def check_rows(rows, expected):
    """旧记录必须可见，且响应也不得临时补全工作树身份。"""
    by_id = {row["info"]["id"]: row for row in rows}
    assert expected.keys() <= by_id.keys(), "列表遗漏旧记录：" + str(expected.keys() - by_id.keys())
    for session_id, cwd in expected.items():
        row = by_id[session_id]
        assert row["info"]["cwd"] == cwd, "列表改写了旧目录"
        assert row.get("session_kind") is None, "列表补全了会话类型"
        assert row.get("worktree_label") is None, "列表补全了工作树标签"


def check_unchanged(snapshots):
    """只检查本地存储文件，绝不访问摘要中的 UNC 项目路径。"""
    for path, (contents, mtime) in snapshots.items():
        assert path.read_bytes() == contents, "旧摘要被改写：" + str(path)
        assert path.stat().st_mtime_ns == mtime, "旧摘要修改时间改变：" + str(path)
        assert not path.with_name("summary.json.lock").exists(), "读取创建了摘要锁：" + str(path)


def main():
    """所有会话、日志和运行时临时文件均随测试结束清理。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--exe", default="target/release/xai-grok-pager.exe", help="已构建的 Release 程序路径")
    args = parser.parse_args()
    if os.name != "nt":
        parser.error("此回归仅支持 Windows 编译服务器 192.168.136.147")
    addresses = {item[4][0] for item in socket.getaddrinfo(socket.gethostname(), None, socket.AF_INET)}
    if "192.168.136.147" not in addresses:
        parser.error("请在编译服务器 192.168.136.147 上执行，禁止在本机运行")
    exe = Path(args.exe).resolve()
    parts = {part.lower() for part in exe.parts}
    if not exe.is_file() or "release" not in parts or "debug" in parts:
        parser.error("请指定已有的 Release 程序：" + str(exe))

    root = Path(tempfile.mkdtemp(prefix="grok-session-history-", dir=tempfile.gettempdir()))
    processes = []
    try:
        home, cwd, runtime_tmp = root / "home", root / "work", root / "tmp"
        for directory in (home, cwd, runtime_tmp):
            directory.mkdir()
        # 只继承运行 Windows 程序所需环境，排除用户凭据、配置覆盖、代理和终端注入。
        keep = {"SYSTEMROOT", "WINDIR", "COMSPEC", "PATH", "PATHEXT", "OS",
                "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"}
        env = {key: value for key, value in os.environ.items() if key.upper() in keep}
        env.update(
            GROK_HOME=str(home), HOME=str(root), USERPROFILE=str(root),
            APPDATA=str(root / "AppData" / "Roaming"), LOCALAPPDATA=str(root / "AppData" / "Local"),
            PROGRAMDATA=str(root / "ProgramData"), TEMP=str(runtime_tmp), TMP=str(runtime_tmp),
            XAI_API_KEY="fixture-global-unused", GROK_TELEMETRY_ENABLED="false", GROK_TRACE_UPLOAD="false",
        )
        config_path = home / "config.toml"
        config_path.write_text(CONFIG, encoding="utf-8")
        seed, rpc = start_agent(exe, cwd, env, root / "seed-stderr.log", processes)
        local_id = call_before(rpc, "session/new", {
            "cwd": str(cwd), "mcpServers": [], "_meta": {"modelId": "history-fixture"},
        }, time.monotonic() + 20)["sessionId"]
        stop(seed)
        local_paths = list((home / "sessions").glob("*/" + local_id + "/summary.json"))
        assert len(local_paths) == 1, "真实建会话未生成唯一持久化摘要"
        local_path = local_paths[0]
        original = local_path.read_bytes()
        (root / "original-summary.json").write_bytes(original)
        print("通过：真实 stdio 创建了可恢复会话，无提示或模型推理", flush=True)

        worktree_cwd = home / "worktrees" / "history-repo" / "legacy" / "src"
        worktree_cwd.mkdir(parents=True)
        marker = "grokhistorycontentfixture"
        expected, snapshots = {}, {}
        # UNC 仅作为字符串编码进本地会话目录，不对它执行 mkdir、stat 或路径解析。
        for old_cwd, title in ((r"\\192.0.2.1\grok-offline", "离线目录旧会话"),
                               (str(worktree_cwd), "未标记工作树旧会话")):
            session_id = str(uuid.uuid4())
            summary = json.loads(original)
            summary["info"] = {"id": session_id, "cwd": old_cwd}
            summary.update(num_messages=1, num_chat_messages=1, session_summary=title,
                           generated_title=None, title_is_manual=False, hidden=False,
                           session_kind=None, worktree_label=None, source_workspace_dir=None,
                           agent_id=None, attempt_id=None)
            session_dir = home / "sessions" / quote(old_cwd, safe="") / session_id
            session_dir.mkdir(parents=True)
            path = session_dir / "summary.json"
            path.write_text(json.dumps(summary, ensure_ascii=False), encoding="utf-8")
            # 正文检索使用本地历史样本，标记不出现在标题中。
            update = {"method": "session/update", "params": {
                "sessionId": session_id, "update": {"sessionUpdate": "user_message_chunk",
                    "content": {"type": "text", "text": marker}},
            }}
            (session_dir / "updates.jsonl").write_text(json.dumps(update) + "\n", encoding="utf-8")
            expected[session_id] = old_cwd
            snapshots[path] = (path.read_bytes(), path.stat().st_mtime_ns)

        # 建会话阶段禁用索引，重启后首次检索必须从含两条旧记录的存储全量初始化。
        config_path.write_text(CONFIG.replace("session_search = false", "session_search = true"), encoding="utf-8")
        reader, rpc = start_agent(exe, cwd, env, root / "reader-stderr.log", processes)
        deadline = time.monotonic() + 10
        overview = call_before(rpc, "_x.ai/session_summaries/workspace_list", {}, deadline)
        check_rows([row for rows in overview["all_sessions"].values() for row in rows], expected)
        recent = call_before(rpc, "_x.ai/session_summaries/workspace_list_recent", {"limit": 20}, deadline)
        check_rows(recent, expected)
        for session_id, old_cwd in expected.items():
            scoped = call_before(rpc, "_x.ai/session_summaries/session_list", {
                "workspace_directory": old_cwd,
            }, deadline)
            check_rows(scoped["session_summaries"], {session_id: old_cwd})
        check_unchanged(snapshots)
        print("通过：全量、最近及两种目录列表在 10 秒总期限内返回，旧摘要和锁状态不变", flush=True)

        deadline = time.monotonic() + 10
        while True:
            search = call_before(rpc, "_x.ai/session/search", {
                "query": marker, "limit": 20, "includeContent": True,
            }, deadline)["result"]
            if not search["bootstrapping"]:
                break
            time.sleep(0.1)
        hits = {hit["sessionId"]: hit for hit in search["results"]}
        assert expected.keys() <= hits.keys(), "索引初始化未收录旧会话正文"
        for session_id, old_cwd in expected.items():
            hit = hits[session_id]
            assert hit["cwd"] == old_cwd, "检索改写了旧目录"
            assert "content" in hit["matchedFields"] and marker in hit.get("snippet", ""), "正文检索未命中样本"
        check_unchanged(snapshots)
        assert local_path.read_bytes() == original, "只读阶段修改了真实会话摘要"
        stop(reader)
        print("通过：首次全量索引和真实正文搜索完成，旧摘要、修改时间及锁状态不变", flush=True)

        log_path = home / "logs" / "unified.jsonl"
        offset = log_path.stat().st_size
        # SSH 没有交互控制台；独立隐藏控制台供真实 TUI 初始化，避免影响调用者终端。
        startup = subprocess.STARTUPINFO()
        startup.dwFlags = subprocess.STARTF_USESHOWWINDOW
        startup.wShowWindow = subprocess.SW_HIDE
        started = time.monotonic()
        cli = subprocess.Popen(
            [str(exe), "--no-leader", "--no-auto-update", "--no-memory", "--trust", "--fullscreen",
             "--resume", local_id], cwd=cwd, env=env,
            creationflags=subprocess.CREATE_NEW_CONSOLE, startupinfo=startup,
        )
        processes.append(cli)
        loaded = completed = False
        while time.monotonic() - started < 15:
            for line in log_path.read_bytes()[offset:].splitlines():
                try:
                    entry = json.loads(line)
                except ValueError:
                    continue
                if entry.get("pid") != cli.pid:
                    continue
                loaded |= entry.get("msg") == "session.load.done" and entry.get("sid") == local_id
                completed |= (entry.get("msg") == "startup complete"
                              and entry.get("ctx", {}).get("outcome") == "ok")
            if loaded and completed:
                break
            assert cli.poll() is None, "CLI 在恢复完成前退出，退出码：" + str(cli.returncode)
            time.sleep(0.1)
        assert loaded and completed, "CLI 未在 15 秒总期限内完成目标会话恢复及启动"
        check_unchanged(snapshots)
        print(f"通过：真实 CLI 按 ID 恢复完成（{time.monotonic() - started:.1f} 秒），已核对本次进程及目标会话日志", flush=True)
    except Exception:
        for path in [*root.glob("*-stderr.log"), root / "home" / "logs" / "unified.jsonl"]:
            if path.is_file():
                print("失败诊断日志：" + path.name, file=sys.stderr)
                print(path.read_text(encoding="utf-8", errors="replace")[-8000:], file=sys.stderr)
        raise
    finally:
        errors = []
        for process in reversed(processes):
            try:
                stop(process)
            except Exception as error:
                errors.append(str(error))
        try:
            shutil.rmtree(root)
        except OSError as error:
            errors.append(str(error))
        if errors:
            raise RuntimeError("测试资源清理失败：" + "；".join(errors))
    print("通过：历史会话回归完成，自建进程和全部临时文件已清理", flush=True)


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print("失败：" + str(error), file=sys.stderr)
        sys.exit(1)
