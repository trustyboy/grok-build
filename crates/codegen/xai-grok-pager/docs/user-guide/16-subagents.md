# Subagents and Personas

Subagents are independent child sessions that handle tasks in parallel. Each subagent has its own context window, so the main agent can delegate work (research, implementation, testing, and code review) without consuming its own context. A subagent reports a summary back to the parent when it finishes.

Subagents are enabled by default.

---

## Agents vs Personas

Agents and personas both customize behavior, but they operate at different levels:

| | **Agents** | **Personas** |
|---|---|---|
| **What they configure** | The whole session: model, tools, prompt mode, system prompt | A behavioral overlay added to a subagent's prompt |
| **Scope** | Primary session or subagent | Subagents only |
| **How you set them** | At startup, or with agent definitions (`.md` files in `.grok/agents/` or `~/.grok/agents/`) | In `config.toml` (`[subagent.personas]`) or `.toml` files under `.grok/personas/`; applied during subagent resolution |
| **What they control** | Model, tool availability, prompt body, skills | Tone, output format, task focus, and input/output contracts |
| **Who edits them** | You -- create, delete, or toggle them in the agents modal or by editing files | You -- define custom personas in config or files; bundled personas are read-only |
| **Examples** | `grok-build`, `explore`, `plan` | `researcher`, `concise` |

An agent defines the session itself. A persona shapes how a subagent behaves within a session. A subagent always runs as an agent type (for example, `general-purpose`), and resolution can layer a persona on top.

Manage both in the agents modal. Open it with `/config-agents` (alias `/agents`), or open the Personas tab directly with `/personas`. The modal has two tabs: **Agents** and **Personas**.

---

## Disabling Subagents

Disable subagents with a CLI flag, an environment variable, or the config file (highest priority first). The same rules apply to the interactive `grok` TUI, `grok agent stdio`, and headless runs.

```bash
grok --no-subagents                  # This session only
export GROK_SUBAGENTS=0              # Environment variable
```

```toml
# ~/.grok/config.toml
[subagent]
enabled = false
```

Only an explicit `enabled = false` turns subagents off. A `[subagent]` table that sets `max_depth`, `[subagent.models]`, or `[subagent.toggle]` without an `enabled` key keeps them on.

---

## How Subagents Work

When the main agent identifies work to delegate, it calls the `spawn_subagent` tool to start a child session. The child runs with:

- Its own context window, independent of the parent
- A toolset determined by its agent type and optional capability mode
- Optional persona instructions applied during resolution

The parent receives the child's output -- usually a summary -- when the child finishes.

---

## Built-in Agent Types

Built-in types still exist as host types. The model cannot pick them by name. An omitted `subagent_type` is `general-purpose`.

| Type              | Description                                          |
| ----------------- | ---------------------------------------------------- |
| `general-purpose` | Default type. Full-capability agent for any task.    |
| `explore`         | Research agent. Searches, reads, greps, and runs shell commands, but does not edit files. Use it for codebase investigation. |
| `plan`            | Planning agent. Explores the codebase and produces a structured implementation plan; does not edit files. |

Project- or user-defined agents can add new types or shadow these built-ins by name.

`spawn_subagent` has an optional `subagent_type` enum when plugin, project, or user agents exist. Its values are those agents, for example `my-plugin:reviewer`. Built-in names and xAI-bundled agents are not listed. If a parent's `tools` list restricts spawning with `Agent(...)`, the enum shows only the allowed types. An unknown type fails the call with the list of valid types.

---

## Personas

A persona is a named behavioral overlay. Its instructions are injected into the subagent's conversation as a `<system-reminder>`, which shapes tone, output format, and task focus without changing the subagent's agent type, model, or tools.

Define personas in `config.toml` or in `.toml` files:

```toml
[subagent.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build discovers file-based personas from these locations, in priority order:

- `.grok/personas/*.toml` (project)
- `~/.grok/personas/*.toml` (user)
- The bundled personas directory (lowest priority)

Each file defines one persona, and the file name (without the extension) becomes the persona name. Inline `config.toml` personas take precedence over files. Only `.toml` files are discovered.

Manage personas in the Personas tab of the agents modal (`/personas`). Bundled personas are read-only; personas you define are editable.

> **Note:** Grok Build applies personas through subagent resolution and roles, not through a `spawn_subagent` parameter. The main agent does not pass a persona name when it spawns a child.

### Persona Fields

| Field               | Description                                                          |
| ------------------- | ------------------------------------------------------------------- |
| `instructions`      | Inline instruction text applied as the persona layer.               |
| `instructions_file` | Path to an instruction file, loaded at spawn time and merged after `instructions`. |
| `description`       | Short summary shown in the persona catalog. Falls back to the first paragraph of `instructions`. |
| `inputs` / `outputs`| Declared input and output contract (see below).                     |
| `model`             | Model override applied when the persona is used.                    |
| `reasoning_effort`  | Reasoning effort applied when the persona is used.                  |
| `default_isolation` | Default isolation mode (`none` or `worktree`).                      |

### Input/Output Contracts

A persona can declare the inputs it expects and the outputs it produces. The parent agent reads these to know what context to supply and what artifacts to expect. This lets you chain personas, so one persona's output file becomes the next persona's input:

```toml
[[subagent.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagent.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

Each field has a `name`, an `io_type` (defaults to `file`), a `required` flag, and a `description`.

### Persona Resolution

应用 persona 时，角色默认值优先于 persona 默认值。模型及推理强度还会结合父会话临时选择、父模型专属配置、全局子代理配置和 agent 定义，完整顺序见下方[模型与推理强度优先级](#模型与推理强度优先级)。

隔离模式仍按「单次显式指定 > 角色默认 > persona 默认 > `none`」解析，不继承父会话隔离模式。

If a persona is requested but cannot be resolved -- it is not found, has no instructions, or its `instructions_file` is unreadable -- the spawn fails.

---

## Spawning Subagents

The main agent calls the `spawn_subagent` tool. Its parameters:

| Parameter           | Description                                                       |
| ------------------- | ---------------------------------------------------------------- |
| `prompt`            | The full task prompt for the subagent.                           |
| `description`       | A short label for the task (3-5 words).                          |
| `run_in_background` | Run in the background and return a subagent ID. Defaults to `true`. |
| `isolation`         | `none` (shared workspace, the default) or `worktree` (isolated git worktree). |
| `resume_from`       | Continue a completed subagent's conversation. Pass its subagent ID. |
| `cwd`               | Working directory for the subagent. Mutually exclusive with `isolation: worktree`; ignored when `resume_from` is set (the resumed child inherits its source's directory). |

When you run a subagent in the background, retrieve its result later with `get_command_or_subagent_output`.

### Sending messages to subagents

The `send_subagent_message` tool is off by default. Enable it with `GROK_ACTIVE_AGENT_MESSAGES` or `[features] active_agent_messages`.

The root session can send a follow-up to a subagent it owns. When the flag is on, a granted child also receives the tool:

- `subagent_id: "parent"` targets that child's active parent subagent.
- A durable agent id targets another local subagent. An eligible completed subagent resumes with the same identity.

A child whose parent is the root session cannot message the root. Curated harness toolsets never receive the tool. A capability mode that excludes this kind also removes it.

Child senders are bounded: 4 in-flight messages per sender-target pair, and 32 outbound messages per sender attempt. A send over the limit returns `QuotaExceeded`.

An inactive subagent always wakes with the message as its next turn. For an active subagent, the optional `delivery` parameter controls how the message lands:

- `steer` (the default) joins the current turn at its next safe point.
- `queue` waits as a protected later turn instead of entering the active turn.
- `interject` is urgent: it is delivered ahead of pending steers at the earliest safe point, and it interrupts a subagent that is blocked waiting on background work so the subagent reads the message at once. Only the wait call ends early. The background work keeps running.

If the subagent is active but between turns, `steer` and `interject` each become one protected queued turn and the subagent starts on it. The legacy `queue: true` flag is still accepted and means `delivery: "queue"`. `delivery` wins when both are present.

The transcript shows each send as a one-line `Message` row: a verb for the outcome, then the subagent's label (its persona, role, tag, or Subagent fallback) and its description in curly quotes, as its `Subagent …: “…”` scrollback row quotes it, clamped to the first line and 40 characters. The verb carries the delivery, so a steer stays unmarked:

- `Message sent to Subagent “find callers”` (steer)
- `Message queued for Subagent “find callers”` / `Message interjected to Subagent “find callers”`
- `Message sending to …` with an animated bullet while the send is in flight
- `Message rejected · Subagent “find callers”` for a refused send, `Message unconfirmed · Subagent “find callers”` for one the shell could not confirm
- `Message sent to parent` when a child messages its parent

The collapsed row never shows the message or the reason. **Right** (or `l`/`e` in vim mode) expands the row to show the requested delivery, the full message text, and the reason of a rejected or unconfirmed send; **Left** (or `h`) collapses it again. **Enter**, **Ctrl+F**, or a double-click on the row opens that subagent's view, exactly as on its `Subagent` row (Right/Left still fold it). If the subagent was never spawned in this session (a headless `grok export`, or an id from another session), the row names it `subagent …xxxxxxxx` from the last 8 characters of its id, shows the raw `Subagent ID:` when expanded, and cannot open it.

---

## Capability Modes

Capability mode is not a spawn argument. A child's tools come from its **agent type** and any **role / definition default**. `general-purpose` is unrestricted (`all`). The built-in `explore` and `plan` types read, search, and run shell commands but cannot edit files.

| Mode         | Read | Write | Execute | Description                                  |
| ------------ | ---- | ----- | ------- | -------------------------------------------- |
| `read-only`  | Yes  | No    | No      | Read, search, and inspect (also web search and LSP); no file edits or shell. |
| `read-write` | Yes  | Yes   | No      | Read, plus create, edit, delete, and move files. No shell. |
| `execute`    | Yes  | No    | Yes     | Read, plus run shell commands and background tasks. No file edits. |
| `all`        | Yes  | Yes   | Yes     | Unrestricted tool access. Default for `general-purpose`. |

---

## Context Inheritance

### resume_from

The `resume_from` parameter lets a new subagent continue where a completed subagent left off, which is useful for multi-stage workflows:

1. Spawn a research subagent to investigate a problem.
2. Spawn a second subagent with `resume_from` set to the first subagent's ID, so it picks up with the full research context.

The new subagent inherits the source's transcript, tool state, and model; its system prompt and tools are re-rendered from the current agent definition. The source must be completed (not running), belong to the current session, and use the same agent type.

### MCP inheritance

The primary session overlays the active agent’s `mcpServers` frontmatter onto the disk/client merge by name (agent.md headers beat `config.toml`). Switching the primary agent replaces that overlay with the new seat only. Child inline `mcpServers` still become owned clients and beat inherited shared clients. Plugin agents cannot declare `mcpServers`.

Subagents inherit the parent session’s **already-connected** MCP servers by default. That includes local stdio/HTTP servers and plugin-sourced agents (for example `my-plugin:reviewer`). The child discovers and calls those tools with `search_tool` / `use_tool` the same way the parent does.

Control inheritance with agent frontmatter `mcpInheritance`:

| Value | Effect |
| ----- | ------ |
| `all` (default if omitted) | Inherit every parent-connected MCP server |
| `none` | Inherit no parent MCP servers |
| `named: [server, …]` | Inherit only the listed server names |
| `except: [server, …]` | Inherit all parent servers except the listed names |

Example:

```yaml
---
name: research-only
description: Read MCP tools but not internal connectors
tools: search_tool, use_tool, Read
mcpInheritance:
  except:
    - internal-tools
---
```

**Plugin agents** inherit parent MCP the same way. For security they still cannot:

- Declare their own `mcpServers` in agent frontmatter (ignored with a warning)
- Declare hooks in agent frontmatter
- Set `permissionMode: bypassPermissions`

Plugin-bundled MCP servers (plugin `.mcp.json`) still attach to the **parent/session** after the plugin is trusted — they are not a child-only frontmatter declaration. See [Plugins](09-plugins.md) and [MCP Servers](07-mcp-servers.md).

---

## Isolation: Worktree Mode

For tasks that modify files, run a subagent in an isolated git worktree with `isolation: worktree`. This keeps the child's edits from conflicting with the parent's:

- The subagent works in its own copy of the working tree.
- Its changes stay isolated from the parent until you merge them.
- The subagent's result includes the worktree path.

Grok Build manages worktrees through the `x.ai/git/worktree/*` extension methods, including an apply operation that merges changes back into the main working directory.

---

## Configuration

正式名称为 `[subagent]` 及其 `models/roles/personas/toggle` 子表。旧 `[subagents]` 仍兼容读取，同键以单数为准，旧表独有键保留；`--no-subagents`、`GROK_SUBAGENTS`、内部目录与历史文件名不改名。

### 全局默认模型与推理强度

在用户级 `config.toml` 中为普通新建子代理设置统一默认值，不改变主会话的模型：

```toml
[subagent]
model = "nd-gpt-astra"
reasoning_effort = "high"
```

`model`、`reasoning_effort` 均可独立省略或留空。模型 ID 应已存在于配置或加载后的模型目录；沿用该模型自己的上游、API 协议、认证及能力。只设置 `reasoning_effort = "high"` 时，没有其他模型覆盖的子代理仍继承父会话当前模型。两项均省略时，仍按父模型专属配置等优先级解析。

推理强度接受 `none`、`minimal`、`low`、`medium`、`high`、`xhigh`、`max`；不保证每个模型都支持所有档位。非法 effort 字符串告警后继续低优先级；选中的合法档位若不受目标支持，则告警并保留基础配置。未知静态模型告警后回退，其他有效字段继续生效。状态查询同时展示配置值和实际生效值，避免把配置意图误认成上游实际参数。

### 主模型专属默认模型

在已有主模型配置中添加专属子代理模型与推理强度：

```toml
[model.cpa-deepseek-v4-flash]
subagent_model = "nd-gpt-astra"
subagent_model_reasoning_effort = "high"

# 只设置 effort，模型仍按其他规则选择
[model.my-parent]
subagent_model_reasoning_effort = "medium"
```

示例应合并到已有模型配置，模型 ID 替换为实际目录键。两个字段独立可选；`subagent_model_reasoning_effort` 为可选字符串，使用上方列出的 effort 档位，不要求同时配置 `subagent_model`。

二者共用父会话**实时模型目录键**查找，不按上游请求模型名或共享默认模型查找。省略或空白分别回退全局 `[subagent] model/reasoning_effort`；无效模型告警后回退全局，非法 effort 字符串告警后继续低优先级。选中的合法档位不受目标支持时，告警并保留基础配置，不再尝试较低优先级的 effort。临时发现父模型不借用其上游来源模型的配置。

切主模型影响后续新建 child 的模型及 effort，各父会话独立，child 沿用目标自身接口、凭据和能力。临时 set 的模型及显式 effort 保持优先；未指定临时 effort 时仍按当前父模型解析。`inherit` 仅决定模型，跳过主模型专属及全局模型配置，仍应用父模型专属及全局 effort；`reset` 恢复当前主模型规则。

### 会话内临时选择

正式命令是 `/subagent-model`，旧 `/subagents-model` 保留兼容别名。

| 命令 | 行为 |
| --- | --- |
| `/subagent-model` 或 `/subagent-model status` | 查询普通 `general-purpose` 的默认模型、实际 effort、窗口、来源和警告 |
| `/subagent-model <模型ID或名称> [window] [effort]` | 临时覆盖当前父会话的新建子代理默认值，例如 `/subagent-model nd-gpt-astra 256k high` |
| `/subagent-model <未知完整ID>` | 优先查询当前上游，再跨已配置可选连接发现，沿用命中来源的接口与认证 |
| `/subagent-model inherit` | 动态跟随父模型，跳过较低优先级模型默认值 |
| `/subagent-model reset` | 清除会话覆盖，恢复当前主模型专属、全局、按类型及角色等配置规则 |

模型 ID 优先精确匹配，也支持显示名称和与 `/model` 相同的窗口、effort 参数顺序及补全。`status`、`inherit`、`reset` 为保留控制词。窗口和 effort 必须由目标模型声明支持；未知单 ID 需先单独发现，再为已加载目标配置参数。发现模型默认 `256000` 上下文、关闭原生视觉且不发送 effort，这只是保守能力默认值，不代表服务商保证的真实上限。

设置经后端确认后，仅影响该父会话之后新建的子代理，不修改父模型、其他会话或磁盘默认配置。指定临时模型后，主模型切换不会改变该选择；`inherit` 则跟随父模型变化。临时窗口与 effort 只对最终采用同一临时目标模型的子代理生效。`inherit` 和 `reset` 一并清除临时窗口、effort，仍按优先级解析父模型专属及全局 effort；`inherit` 仅决定模型，`reset` 恢复配置规则。

已运行子代理保留启动时快照；`resume_from` 恢复时固定使用原子代理模型；内部复制父上下文的 fork-context 路径固定使用父模型。覆盖不跨进程重启或会话重新加载保存。命令失败时保留原选择。

### 模型与推理强度优先级

普通新建子代理的模型，从高到低：

1. 单次启动显式模型，包括工作流显式参数。
2. 当前父会话 `/subagent-model` 的指定模型或 `inherit`。
3. role / persona 模型默认值（role 优先于 persona）。
4. `[subagent.models]` 按类型指定。
5. 当前主模型 `[model.<id>] subagent_model`。
6. `[subagent] model` 统一默认值。
7. agent 定义中的模型。
8. 父会话当前模型与实时采样配置。

推理强度在模型确定后独立解析，从高到低：

1. 单次启动显式 effort。
2. 当前父会话临时 effort，仅在最终采用该临时目标模型时应用。
3. role / persona 默认 effort。
4. 当前父模型 `[model.<id>] subagent_model_reasoning_effort`。
5. `[subagent] reasoning_effort`。
6. agent 定义中的 effort。
7. 基础采样配置：继承模型时取父会话当前值，指定其他模型时取目标自身默认值。

非法 effort 字符串告警并继续低优先级；目标模型不支持选中的合法档位时，告警并保留基础配置。恢复子代理及内部 fork-context 保持既有固定路径，不应用以上普通新建子代理的覆盖规则。状态接口描述默认 `general-purpose`，不是任何具体角色或显式启动的最终预测。

### ACP 接口与本地回归

正式扩展为 `x.ai/session/subagent_model`，旧 `x.ai/session/subagents_model` 保留兼容别名（JSON-RPC 方法名加前缀 `_`）。参数为 `sessionId`、可选父模型快照 `expectedModelId`、`action: "status" | "inherit" | "reset" | "set"`；仅 `set` 使用 `modelId` 以及可选 `reasoningEffort`、`contextWindow`，如 `reasoningEffort: "high"`、`contextWindow: 256000`。

返回默认状态包括 `modelId`、`modelSource`、`reasoningEffort`、`effortSource`、`contextWindow`、`configuredModel`、`configuredEffort`、`warning`、`availableModels`，可选 `parentModelId`、`parentConfiguredModel`、`parentConfiguredEffort` 分别表示当前父会话模型、专属模型及专属 effort 原值。`configuredModel`、`configuredEffort` 始终保留全局 `[subagent] model/reasoning_effort`，不会被主模型专属值或临时选择覆盖；`modelSource` 或 `effortSource` 为 `parent_model_config` 时，在 UI 显示「主模型专属配置」。

`reasoningEffort` 和配置/警告字段可以为 `null`，`availableModels` 为 ACP `ModelInfo` 列表；旧客户端可忽略新增可选字段。客户端应以实际值判断效果，请求失败不应显示为设置成功。

仓库回归脚本为 `tools/test-subagents-model.py`，需要 Python 3.11+ 和已构建的 Release 程序。从仓库根目录运行：

- 全部场景：`python tools/test-subagents-model.py`。
- 定向场景：`python tools/test-subagents-model.py --scenario temporary-lifecycle --scenario discovery-auth --scenario resume-original`。
- 指定产物：`python tools/test-subagents-model.py --exe target/release/xai-grok-pager.exe`。

脚本不构建程序、不使用真实凭据或修改用户配置；隔离 `GROK_HOME`、用户目录和工作区，以本地假上游让真实 parent 调用 `spawn_subagent`。它核对 child 实际 HTTP 模型名、API、认证及 effort，并在响应暂停期间通过 ACP 查询真实 child 上下文窗口。每个场景使用临时目录，成功或失败均清理；失败仅输出请求摘要及相关日志尾部。

| 场景参数 | 检查内容 |
| --- | --- |
| `default-inherit` | 两个全局字段省略时继承父模型、effort、窗口及父模型变化 |
| `effort-only` | 只设全局 effort，模型继承、临时选择及 inherit |
| `global-selection` | 全局模型和 effort，child Responses 与 parent Chat 的独立认证 |
| `model-only` | 只设置全局模型，使用目标自身默认 effort，而非父会话 effort |
| `legacy-section` / `mixed-sections` | 旧 `[subagents]` 读取，新旧同键单数优先、旧表独有 effort 保留 |
| `parent-model-selection` | `parent→temporary`、`alternate→plain`，双父会话隔离与即时切换，目标独立凭据；全局和父模型配置值分开报告 |
| `parent-model-only` | 只配置专属 model、无专属或全局 effort 时采用目标默认值；无专属配置回退 global |
| `parent-model-fallback` | 专属字段省略/空白回退 global，无效目标告警回退且不触发发现 |
| `parent-model-lifecycle` | 专属配置下 set 保持、inherit 跟随、reset 恢复当前主模型规则，显式模型优先及窗口/effort 清除 |
| `parent-model-discovery` | 临时发现主模型不借用来源专属值，回退 global，保持认证和双会话隔离 |
| `parent-effort-selection` / `parent-effort-only` | 父模型专属 effort、独立配置与空白修剪、双会话切换隔离，目标模型自身的子代理配置不参与 |
| `parent-effort-priority` / `parent-effort-lifecycle` | role、会话 effort 和父模型专属值的优先级，异目标隔离，切主模型及 set/inherit/reset |
| `parent-effort-fallback` / `parent-effort-discovery` | 省略/空白/非法值及不支持档位回退，临时发现父模型不借来源配置 |
| `parent-effort-resume` | 父模型专属配置和临时 effort 变化后，恢复仍采用原目标及既有基础 effort 规则 |
| `temporary-lifecycle` | set、窗口/effort、两父会话隔离、父模型变化、单次显式模型、inherit/reset |
| `unsupported-effort` | 无推理模型、缺少指定档位的基础值回退和警告 |
| `invalid-parameters` | 零/未声明窗口、非法或不支持的 effort、未知 ID 带参数、过期父模型快照失败后保持原值 |
| `discovery-auth` | 未知 ID 跨上游发现、正确凭据/API、失败回滚、主模型不变 |
| `resume-original` | 新建 child 使用新选择，恢复 child 仍使用原模型 |

该脚本不覆盖 TUI 参数补全及旧命令别名、并发切换时序、完整 role/persona/type pins/definition 优先级、单次显式 effort、内部 fork-context、跨进程重载及全局静态空白/无效配置回退；这些需要命令层或 Rust 定向测试。跨上游发现的去重、并发上限及多命中顺序由 `tools/test-model-discovery.py` 单独回归。

### Per-Type Toggles and Model Overrides

Disable specific agent types, or route them to a different model:

```toml
[subagent.toggle]
explore = true                       # default -- omit to keep enabled
plan = false                         # disable the plan subagent

[subagent.models]
explore = "grok-4.6"                 # route explore to a specific model
```

按类型模型覆盖适用于所有父会话，但低于会话临时选择和 role/persona。未设置按类型覆盖时继续检查主模型 `subagent_model`、统一默认模型及 agent 定义，均无有效选择才继承父模型。

### Model Selection by the Agent

`spawn_subagent` 的 `model` 参数供主代理在用户明确要求时指定单次模型，工具说明会列出可选模型。开启 `[features] subagent_model_inheritance = true`（或 `GROK_SUBAGENT_MODEL_INHERITANCE=1`）且目录中所有可选模型均属 xAI 时，会隐藏模型参数及选择说明；仍发送该参数的调用会被拒绝并要求重试。包含第三方模型、未声明 family 的模型或目录仍在加载时保留参数。该功能限制模型发出的显式选择，不取消用户的会话临时选择、主模型专属默认、全局默认、按类型 pin、role/persona 等配置规则。此开关在会话启动时读取，修改后需要重启；优先级为 requirements/MDM pin、环境变量、config.toml、远端设置、默认关闭。

You can also toggle it from `/settings` → Models → **Subagent model inheritance**:

- On: Grok cannot set models for subagents
- Off: Grok may choose a different model for a subagent. Takes effect after restart.
- NOTE: This setting only applies when all models are xAI "model_family". You likely don't need to configure this setting.

The row shows the value that applies after restart. Toggling writes `[features] subagent_model_inheritance = true` or `= false` (an explicit `false` overrides a remote `true`); `d` (reset) deletes the key so `managed_config.toml`, remote settings, or the default apply again. Agents already running keep the mode they started with. When a layer your `config.toml` cannot override decides the value — a `requirements.toml`/MDM pin, the environment variable, the `GROK_CONFIG` overlay, or an active campaign — both the toggle and the reset are refused with a toast that names that layer.

### Custom Roles and Personas

Define custom roles with their own capability and model defaults:

```toml
[subagent.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-4.6"
prompt_file = ".grok/prompts/researcher.md"
```

Define custom personas with behavioral instructions:

```toml
[subagent.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".grok/personas/concise.md"  # or load from a file
```

Grok Build also discovers roles from `.grok/roles/*.toml` and personas from `.grok/personas/*.toml`. Inline `config.toml` definitions take precedence over files.

---

## The Tasks Pane (TUI)

Grok Build shows running and finished work in side panes on the agent screen:

- Press `Ctrl+G` to toggle the tasks pane, which lists active and completed subagents and background commands with their status.
- Press `Ctrl+T` to toggle the separate todo pane.

To view the available agent types and personas, open the command palette with `Ctrl+P` and choose **Manage Agents** (`/config-agents`).

Subagents appear at the top of the tasks pane in their own collapsible "Subagents" group.

---

## Viewing Subagents in the TUI

Subagents appear in several places in the interactive TUI:

### Scrollback (parent conversation history)

When a subagent is spawned, a compact lifecycle block is added to the *parent's* scrollback:

- `Subagent running: "do the thing" (Implementer · grok-4.6) · Thinking`
- Or for background subagents: `Subagent started: "..."`

While running, the block shows a live activity suffix (e.g. "Running: cargo test", "Compacting", "Retrying (2/3)") pulled from the child's turn tracker. The bullet animates (or is colored) according to state.

Press **Enter** (or Ctrl-F) on the block to open the subagent's full transcript.

For blocking subagents the single entry updates its bullet color when the child finishes. For background ones, a follow-up `Subagent completed/failed/cancelled in Xs: "..."` block is appended.

### Tasks pane (Ctrl+G)

As noted above — grouped under "Subagents", with spinners, elapsed times, and quick access to kill or inspect. Press `h` to toggle hide-completed / show-all.

### Dock (when enabled)

The dock above the prompt lists subagents. With the dock focused, `h` toggles hide-completed / show-all (same filter as the tasks pane). Left / Right collapse and expand a section header.

### Fullscreen framed view (the child transcript)

When you open a subagent (from a scrollback block or the tasks pane), a bordered frame replaces the parent view and shows the child's full transcript:

- Title bar inside the frame: status icon (spinner / ✓ / ✗), label + bold description + model, optional "resumed"/"forked" badge, live activity · elapsed time, and [✗] close button.
- The child's own scrollback, thinking, and tool calls render inside the frame.
- The parent tasks pane, todos pane, and dock hide for the duration of the view.

This view is observational. The composer is hidden (zero rows). You cannot focus it, type a prompt, stash a draft, or send a follow-up from here. The parent session still owns prompts. To steer a running child, close the view and use `send_subagent_message` from the parent (see [Sending messages to subagents](#sending-messages-to-subagents)).

**What still works**

- Scroll, fold, copy, open links, and open the block viewer on the child's transcript.
- `Ctrl+C` cancels **this child's** turn. It does not cancel the parent.
- `Ctrl+.` / `Ctrl+X` opens the shortcuts cheatsheet for the child's keys.
- The child view paints no `[Dashboard]` button. Inside the dashboard overlay the button is the way back.
- Idle `Enter` in the **block viewer** quotes the selected line into the parent composer and closes the view.

**What does nothing (fail closed)**

Root-only chords never start on this surface. They do not open a modal on the child, and they do not leak to the parent:

- Command palette (`Ctrl+P`), model picker (`Ctrl+M`), session picker (`Ctrl+R`)
- Settings, extensions, always-approve (`Ctrl+O`), send-to-background (`Ctrl+B`)
- External prompt editor, Shift+Tab mode cycle

A denied action is a silent redraw. There is no toast.

If a prompt-queue overlay appears, it is a **read-only mirror**. You cannot edit, send-now, or remove rows. Queue RPCs always target the parent session.

**How to leave**

- `q` or `Esc` from bare scrollback, or click [✗].
- If scrollback search is open, `q` / `Esc` closes search first. A later press closes the view.
- `Ctrl+Q` always quits Grok. It is never swallowed here.

The parent's scrollback keeps showing the subagent's status after you close.

---

## Depth Limits

Only the top-level session spawns subagents. A subagent cannot spawn its own subagents: the maximum nesting depth is one. If a subagent calls `spawn_subagent`, the call fails with a depth-limit error. This keeps the agent tree flat and prevents runaway spawning.

---

## When to Use Subagents

**Good use cases:**

- Researching a codebase while the parent continues other work
- Running tests in parallel while the parent implements changes
- Reviewing generated changes before you commit them
- Delegating independent tasks that do not depend on each other

**When not to use:**

- Simple tasks that the parent can handle directly
- Tasks that require tight back-and-forth with the user, since a subagent runs autonomously and isn't suited to interactive exchanges
- Tasks where the context setup cost exceeds the parallelism benefit
