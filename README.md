<div align="center">

<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://media.x.ai/v1/website/spacexai-symbol-white-transparent-0c31957f.png">
    <source media="(prefers-color-scheme: light)" srcset="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png">
    <img alt="SpaceXAI logo" src="https://media.x.ai/v1/website/spacexai-symbol-black-transparent-6435cf42.png" width="96">
  </picture>
  <br>
  Grok Build (<code>grok</code>)
</h1>

**Grok Build** is SpaceXAI's terminal-based AI coding agent. It runs as a
full-screen TUI that understands your codebase, edits files, executes shell
commands, searches the web, and manages long-running tasks — interactively,
headlessly for scripting/CI, or embedded in editors via the Agent Client
Protocol (ACP).

[Installing the released binary](#installing-the-released-binary) ·
[Building from source](#building-from-source) ·
[Documentation](#documentation) ·
[Repository layout](#repository-layout) ·
[Development](#development) ·
[Contributing](#contributing) ·
[License](#license)

![Grok Build TUI](https://media.x.ai/v1/website/universe-tui-screenshot-6f7a0837.png)

**Learn more about Grok Build at [x.ai/cli](https://x.ai/cli)**

This repository contains the Rust source for the `grok` CLI/TUI and its agent
runtime. It is synced periodically from the SpaceXAI monorepo.

A small `SOURCE_REV` file at the root records the full monorepo commit SHA
for the version of the code present in this tree.

</div>

---

## 运行时模型发现与视觉开关（本分支）

在已有会话中输入 `/model <完整上游模型ID>`：若本地目录没有该模型，程序优先查询当前模型所用上游的 `/models`。当前上游未命中或查询失败时，继续查询其他已配置且可选的上游连接，按配置顺序选择首个命中来源，沿用该来源的连接和认证切换。

相同连接的多个模型只查询一次；同地址的不同凭据或 API 协议分别处理。当前上游命中时直接使用当前来源；回退有多个命中时自动选择配置顺序中的第一个，网络响应快慢不影响优先级。回退最多并发查询 4 个连接，总等待上限为 25 秒。全部未命中、查询失败且无命中或超时均保留原模型，来源错误显示脱敏后的上游地址。临时模型仅保存在内存中，`config.toml` 和全局默认模型保持原值。

新发现模型默认关闭原生视觉、上下文窗口为 `256000`，不发送思考级别参数。该上下文值是默认值，不代表服务商确认的真实上限；需要精确能力或长期保留时仍应添加模型配置。

| 命令 | 用途 |
| --- | --- |
| `/support_vision` | 查询当前会话、当前模型的原生视觉状态 |
| `/support_vision on` | 从下一轮开始允许发送图片 |
| `/support_vision off` | 从下一轮开始剥离原生图片内容，保留图片路径供视觉桥接使用 |

视觉覆盖仅作用于当前会话，切走再切回该模型仍保留选择，不影响其他会话。关闭原生视觉不会禁止添加附件；vision-bridge 仍可引导模型调用识图工具。开启开关不会赋予模型本身不具备的视觉能力。

运行时模型在后台进程退出后不再保留；视觉覆盖也不跨进程保存。用户主动查询已配置的自定义上游不会开启后台远端设置获取，管理层的远程访问禁令仍然优先。

## Responses API 兼容范围（本分支）

Responses 连接支持正文、拒绝说明和普通函数工具调用。附加元数据与扩展进度事件可以继续处理；已知事件格式损坏、接口错误和缺少正常结束事件仍会报错。

会话保存 Responses 原始输出，并在后续请求中保留合法的消息阶段 `phase`、消息边界、拒绝内容和工具关联信息。正文及函数参数的完成事件会补齐缺失增量，避免重复展示和调用。旧会话及 Chat Completions、Messages 连接保持通用历史转换。

音频播放、Responses 原生 Shell、Computer Use、程序式工具执行、动态工具搜索、服务端 MCP 审批及后台任务调度暂未实现。需要客户端执行的未支持调用会明确报错；原始输出中的复杂数据按支持范围保存，只有已确认合法的输入类型会回传。原生压缩结果按来源校验后使用，自动触发需要显式配置下方阈值。

Release 构建后运行 `python tools/test-responses-compat.py`，使用本地假上游验证扩展事件、函数调用、多轮阶段及拒绝历史、未支持执行能力。

### Responses 原生自动压缩

在已有模型配置中设置 `responses_compact_threshold`，单位为 token：

```toml
[model.example]
api_backend = "responses"
responses_compact_threshold = 200000
```

默认关闭，省略该字段沿用客户端摘要压缩。阈值必须为正整数且小于模型当前有效上下文窗口；请预留足够的输出与工具结果空间。Chat Completions 和 Messages 配置该字段会告警并禁用。程序不自动探测上游能力；上游拒绝参数时显示错误，需要自行调整配置。

启用后，Responses 请求携带 `context_management`，服务端负责常规阈值压缩。Grok 将有效压缩点、请求投影和计数依据随会话历史保存；退出进程再恢复会话时，会重新验证来源、历史前缀与工具配对，再构造实际请求。本地完整记录继续保留，实际请求只包含有效压缩点、必要系统指令和后续内容。

每次发送前，程序按准备完成的实际请求检查上下文、工具定义和输出预留。模型、连接、认证或历史内容变化导致压缩点失效时，使用通用历史重新检查容量；超限时最多尝试一次客户端摘要恢复，仍超限则明确报错。未配置原生阈值时，现有客户端百分比压缩及后台预压缩继续生效；手动 `/compact` 继续可用。

`/context` 显示原生压缩阈值和计数来源；压缩后的总量按有效上下文展示，无法精确拆分的分类另标为本地估算。上游提供 `usage.context_details` 时采用其上下文用量；缺少该字段的压缩响应使用估算，后续普通成功响应可通过上游 usage 校准。新追加内容和工具定义变化仍需估算；重新加载后，界面在下一次请求校验前标记为待校验。累计输入/输出账单与上下文计数分别记录，密文长度不作为精确 token 数。

本功能不提供压缩完成百分比，也不调用独立 `/responses/compact` 接口。原生压缩可能产生额外费用，压缩结果无法人工直接阅读。

Release 构建后运行 `python tools/test-responses-auto-compaction.py`，使用隔离配置和本地假上游验证开关、历史裁剪、工具调用、模型/协议隔离、进程重启恢复、计数校准、容量拦截及旧客户端摘要。

## Fast 快速模式（本分支）

在会话中使用 `/fast` 切换当前模型的快速模式。该功能仅支持 Responses API，请先确认上游服务、模型和账户支持优先服务；开启后账单更高，实际加速和计费以服务商为准。

| 命令 | 用途 |
| --- | --- |
| `/fast` | 翻转当前快速模式状态 |
| `/fast on` | 开启快速模式 |
| `/fast off` | 关闭快速模式 |
| `/fast status` | 只查询，不修改状态；不支持时提示仅 Responses API 支持 |

修改由后端确认，从下一轮生效，不改变正在运行的一轮。开关按「会话 + 模型」隔离：切换到其他模型不会携带当前模型的覆盖值，切回时仍保留该模型的选择；不会影响其他会话，也不写入 `config.toml`。运行时覆盖不跨进程保存。非 Responses 后端拒绝修改请求。

模型配置项 `[model.<id>].fast_mode` 默认为 `false`；需要长期默认开启时，在已有的 Responses 模型配置中设置 `fast_mode = true`。会话内仍可用 `/fast off` 临时覆盖，不修改这个配置默认值。

开启时仅 Responses 请求携带 `service_tier: "priority"`（OpenAI Fast 兼容值）；关闭时省略 `service_tier` 字段，沿用上游默认服务等级。Fast 不改变模型、思考级别或上下文窗口，也不能让不支持优先服务的上游自动获得该能力。

详见[命令说明](crates/codegen/xai-grok-pager/docs/user-guide/04-slash-commands.md)和[模型配置](crates/codegen/xai-grok-pager/docs/user-guide/11-custom-models.md)。

## 子代理默认模型与临时切换（本分支）

普通新建子代理可以使用统一默认模型及推理强度，二者均可独立省略。在用户级 `config.toml` 中配置，模型 ID 需已存在于模型目录：

```toml
[subagent]
model = "nd-gpt-astra"
reasoning_effort = "high"
```

还可在已有主模型配置中添加专属子代理模型及推理强度（目标 ID 需已配置）：

```toml
[model.cpa-deepseek-v4-flash]
subagent_model = "nd-gpt-astra"
subagent_model_reasoning_effort = "high"
```

两个字段独立可选；只设置 `subagent_model_reasoning_effort` 不改变模型选择。二者共用父会话**实时模型目录键**查找，省略或空白分别回退全局 `[subagent] model/reasoning_effort`；无效模型告警后回退全局，非法 effort 字符串告警后继续低优先级。临时发现的主模型不借用其上游来源模型的配置。

只设置 `reasoning_effort` 且无其他模型规则时，子代理仍跟随父会话当前模型。模型不支持推理强度或指定档位时，提示原因并使用基础配置。`[subagent]` 及其 `models`、`roles`、`personas`、`toggle` 子表为正式名称；旧 `[subagents]` 仍兼容读取，新旧同键以单数为准，旧表独有键保留。

| 命令 | 行为 |
| --- | --- |
| `/subagent-model` 或 `/subagent-model status` | 查询普通 `general-purpose` 新建子代理的默认模型、实际 effort、窗口、来源及警告 |
| `/subagent-model <模型ID或名称> [window] [effort]` | 当前父会话临时选择；例如 `/subagent-model nd-gpt-astra 256k high`，参数须由目标模型声明支持 |
| `/subagent-model <未知完整ID>` | 先查询当前上游，未命中或失败再跨已配置可选上游发现；沿用命中来源的接口与认证 |
| `/subagent-model inherit` | 临时直接跟随父模型，并清除临时窗口和 effort |
| `/subagent-model reset` | 清除会话覆盖，恢复配置规则 |

模型优先级：**单次显式指定 > 父会话临时选择 > role/persona > 按类型 `[subagent.models]` > 主模型 `subagent_model` > 全局 `[subagent] model` > agent 定义 > 父模型**。effort 独立解析：**单次显式 > 会话临时（最终同一目标）> role/persona > 父模型 `subagent_model_reasoning_effort` > `[subagent] reasoning_effort` > agent 定义 > 基础配置**。基础值在继承路径取父会话当前值，其他模型取自身默认值。

临时设置只影响当前父会话之后**新建**的子代理，不改变主模型、其他父会话或磁盘配置。切主模型影响后续新建子代理，临时 set 的模型及显式 effort 保持优先；未指定临时 effort 时仍按当前父模型解析。`inherit` 仅决定模型，跳过主模型专属和全局模型配置，仍应用父模型专属及全局 effort；`reset` 恢复当前主模型对应的配置规则；两者均清除临时窗口与 effort。已启动子代理不切换；恢复固定原模型，内部复制父上下文固定父模型。临时选择不跨进程重启或会话重新加载保存。

正式 ACP 入口为 `x.ai/session/subagent_model`；旧 `/subagents-model` 和 `x.ai/session/subagents_model` 仍是兼容别名。状态中 `configuredModel`、`configuredEffort` 保持全局值；可选 `parentModelId`、`parentConfiguredModel`、`parentConfiguredEffort` 分别表示当前父模型、专属模型及专属 effort 原值。`modelSource` 或 `effortSource` 为 `parent_model_config` 时显示「主模型专属配置」。

未知 ID 必须先单独发现，再为已加载模型设置其支持的参数；发现模型默认窗口为 `256000`、不支持原生视觉和 effort，该窗口不是服务商保证的上限。失败不替换已有选择。更多细节见[子代理指南](crates/codegen/xai-grok-pager/docs/user-guide/16-subagents.md)和[配置参考](crates/codegen/xai-grok-pager/docs/user-guide/26-config-reference.md)。

本地回归使用 Python 3.11+，先由正式入口构建 Release，再运行 `python tools/test-subagents-model.py`。脚本不触发构建，不访问真实模型服务或修改用户配置；真实后端由本地假上游驱动 `spawn_subagent`，检查实际请求、认证、effort 和运行中 child 的窗口。支持重复指定 `--scenario`，新增 `parent-model-selection`、`parent-model-only`、`parent-model-fallback`、`parent-model-lifecycle`、`parent-model-discovery` 检查专属配置及回退，`legacy-section`、`mixed-sections` 检查旧表兼容与同键单数优先；`--exe target/release/xai-grok-pager.exe` 可指定产物。配置、会话和失败日志均放系统临时目录，结束后清理。

## 第三方模型工具调用兼容（本分支）

Chat Completions 流式工具调用支持后续片段将 `function.name` 留空、设为 `null` 或省略的接口。程序会保留已收到的有效工具名并继续拼接参数，解决部分 DeepSeek 兼容网关触发的空工具名 `Tool not found` 错误。该兼容处理自动生效，使用包含修复的新构建即可。

## Installing the released binary

Prebuilt binaries are published for macOS, Linux, and Windows:

```sh
curl -fsSL https://x.ai/cli/install.sh | bash   # macOS / Linux / Git Bash
irm https://x.ai/cli/install.ps1 | iex          # Windows PowerShell
grok --version
```

See the [changelog](https://x.ai/build/changelog) for the latest fixes,
features, and improvements in each release.

## Building from source

### 本分支 Windows Release 构建与缓存维护

使用 PowerShell 7 和 MSVC 工具链：

```powershell
& .\tools\windows-protoc\build-release.ps1 --locked
```

定向测试使用 `tools/windows-protoc/test-release.ps1`，与正式构建共享 protoc 和 ripgrep 环境。保留增量编译时，可以使用 `tools/windows-protoc/clean-incremental.ps1` 预览旧缓存候选，确认后加 `-Apply` 清理。默认保留每个 crate 最新两个变体及 14 天内缓存，保留已编译依赖和正式程序。

详细规则见 [Windows 构建与缓存维护](tools/windows-protoc/README.md)。以下为上游跨平台构建说明。

Requirements:

- **Rust** — the toolchain is pinned by [`rust-toolchain.toml`](rust-toolchain.toml);
  `rustup` installs it automatically on first build.
- **[DotSlash](https://dotslash-cli.com)** — required so hermetic tools under
  [`bin/`](bin/) (notably [`bin/protoc`](bin/protoc)) can download and run.
  Install it and ensure `dotslash` is on your `PATH` **before** building:

  ```sh
  cargo install dotslash
  # or: prebuilt packages — https://dotslash-cli.com/docs/installation/
  /usr/bin/env dotslash --help   # sanity check
  ```

- **protoc** — proto codegen resolves [`bin/protoc`](bin/protoc) via DotSlash,
  or falls back to a `protoc` on `PATH` / `$PROTOC`.
- macOS and Linux are supported build hosts; Windows builds are best-effort
  and not currently tested from this tree.

```sh
cargo run -p xai-grok-pager-bin              # build + launch the TUI
cargo build -p xai-grok-pager-bin --release  # release binary: target/release/xai-grok-pager
cargo check -p xai-grok-pager-bin            # fast validation
```

The binary artifact is named `xai-grok-pager`; official installs ship it as
`grok`. On first launch it opens your browser to authenticate — see the
[authentication guide](crates/codegen/xai-grok-pager/docs/user-guide/02-authentication.md).

## Documentation

Full online documentation is available at
[docs.x.ai/build/overview](https://docs.x.ai/build/overview).

The user guide ships with the pager crate:
[`crates/codegen/xai-grok-pager/docs/user-guide/`](crates/codegen/xai-grok-pager/docs/user-guide/)
— getting started, keyboard shortcuts, slash commands, configuration, theming,
MCP servers, skills, plugins, hooks, headless mode, sandboxing, and more.

## Repository layout

| Path | Contents |
|------|----------|
| `crates/codegen/xai-grok-pager-bin` | Composition-root package; builds the `xai-grok-pager` binary |
| `crates/codegen/xai-grok-pager` | The TUI: scrollback, prompt, modals, rendering |
| `crates/codegen/xai-grok-shell` | Agent runtime + leader/stdio/headless entry points |
| `crates/codegen/xai-grok-tools` | Tool implementations (terminal, file edit, search, ...) |
| `crates/codegen/xai-grok-workspace` | Host filesystem, VCS, execution, checkpoints |
| `crates/codegen/...` | The rest of the CLI crate closure (config, MCP, markdown, sandbox, ...) |
| `crates/common/`, `crates/build/`, `prod/mc/` | Small shared leaf crates pulled in by the closure |
| `third_party/` | Vendored upstream source (Mermaid diagram stack) — see below |

> [!IMPORTANT]
> The root `Cargo.toml` (workspace members, dependency versions, lints,
> profiles) is **generated** — treat it as read-only. Prefer editing per-crate
> `Cargo.toml` files.

## Development

```sh
cargo check -p <crate>        # always target specific crates; full-workspace builds are slow
cargo test -p xai-grok-config # per-crate tests
cargo clippy -p <crate>       # lint config: clippy.toml at the repo root
cargo fmt --all               # rustfmt.toml at the repo root
```

## Contributing

> [!NOTE]
> External contributions are not accepted. See [`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

First-party code in this repository is licensed under the **Apache License,
Version 2.0** — see [`LICENSE`](LICENSE).

Third-party and vendored code remains under its original licenses. See:

- [`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES) — crates.io / git dependencies,
  bundled UI themes, and **in-tree source ports** (including openai/codex and
  sst/opencode tool implementations)
- [`crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md`](crates/codegen/xai-grok-tools/THIRD_PARTY_NOTICES.md)
  — crate-local notice for the codex and opencode ports (license texts +
  Apache §4(b) change notice)
- [`third_party/NOTICE`](third_party/NOTICE) — vendored Mermaid-stack index
