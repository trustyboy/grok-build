# Grok Build 项目协作规则

## 编译规则

- 本项目只允许编译 Release 版本，不编译 Debug 版本。
- Windows 下唯一的正式编译入口是：

  ```powershell
  & .\tools\windows-protoc\build-release.ps1
  ```

- 需要向 Cargo 传递额外参数时，直接追加到脚本命令末尾，例如：

  ```powershell
  & .\tools\windows-protoc\build-release.ps1 --locked
  ```

- Windows 定向测试使用 `tools/windows-protoc/test-release.ps1 -p <包名> <过滤条件>`，与正式构建共享 protoc/ripgrep 环境；测试程序参数用带引号的 `'--'` 分隔。同一批代码的多个过滤条件优先复用已编译测试程序。
- 保留 Release 增量编译；旧缓存维护用 `tools/windows-protoc/clean-incremental.ps1` 预览，经用户授权后加 `-Apply`。默认保留每个 crate 最新 2 个变体及 14 天内缓存，构建期间拒绝清理；不得自动清空整个 target。
- 禁止执行不带 `--release` 的 `cargo build`。
- 禁止通过 `cargo run` 隐式触发 Debug 构建。
- 不得为了快速验证而主动编译 Debug 产物；需要验证时，应优先使用不会生成 Debug 可执行文件的静态检查或针对性测试。若验证命令会触发 Rust 编译，也必须显式使用 Release 配置。
- Windows/MSVC 可执行程序在 `xai-grok-pager-bin/build.rs` 预留 8 MiB 主线程栈；恢复默认 1 MiB 会导致后台启动时栈溢出，不能只验证 `--version`。
- Release 可执行文件输出到：

  ```text
  target\release\xai-grok-pager.exe
  ```

## Windows protoc

- 原生 Windows 构建必须使用 `tools/windows-protoc` 中的永久兼容方案，不要直接使用 PATH 中的 `protoc.exe`。
- 首次构建或持久工具缺失时，执行：

  ```powershell
  & .\tools\windows-protoc\setup.ps1
  ```

- 初始化脚本固定安装官方 `protoc 29.3` Windows x64 版本，并校验下载包 SHA-256。
- `tools/windows-protoc/runtime` 保存下载包、官方 `protoc.exe`、include 文件和兼容 Wrapper。该目录不会提交到 Git，但属于本机长期构建环境。
- 禁止自动删除、清空或移动 `tools/windows-protoc/runtime`。如果确实需要更换版本或清理，必须先获得用户明确许可。
- 不要修改 `crates/build/xai-proto-build` 来处理 Windows 的 `/dev/stdout` 与 `/dev/null` 问题；兼容逻辑应保持在独立的 `tools/windows-protoc` 模块内，以降低合并上游版本时的冲突。
- 构建脚本只在子进程期间设置 `PROTOC`，执行完成后会恢复原来的环境变量。

## Windows ripgrep 打包

- `grep` 工具外挂 ripgrep 子进程。Windows 上 `build.rs` 不会自动打包（官方只发 `.zip`，无解压路径），产物默认依赖目标机器 PATH 中的 `rg`，缺失时只报 `program not found`，表现为「搜索永远无结果」。
- `build-release.ps1` 会自动发现本机 `rg` 并通过 `GROK_TOOLS_BUNDLE_RG_PATH` 打包进产物，使 exe 自包含、可直接复制到其他 Windows 机器。构建机需先 `winget install BurntSushi.ripgrep.MSVC`；找不到时只警告不阻断构建。
- winget 的 `rg.exe` 是 0 字节符号链接，必须解析到真实文件后再打包。
- `resolve_bundled_rg()` 在 Windows 下必须给解出的文件补 `.exe` 后缀，否则 `CreateProcessW` 无法执行。
- 排查 grep 异常时，先看会话 `updates.jsonl` 中 `rawOutput.stderr` 的字节数组 —— 解出 `program not found` 即为此问题。

## 文件编码

- 非 UTF-8 文本文件（GBK/Big5 等）的读写由 `computer/transcoding_fs.rs` 的装饰器在 fs 抽象层统一处理，工具层无需感知。设计见 `docs/非UTF8文件编码支持实现计划.md`，合并见同名合并指南。
- 生产接入点只有 `registry/types.rs` 一处，若被改回 `FileSystem(ctx.fs)`，功能会**静默失效**（不报错、不崩溃，只是退回乱码行为）。
- 装饰器需保留有界读取及内容搜索委托；非 UTF-8 单文件搜索仍使用带 `--encoding` 的 ripgrep，不能被不支持编码参数的搜索提供方接管。
- 弱样本（非 ASCII 字节 < 64）时按 `GROK_FILE_ENCODING` 或系统 ANSI 代码页判定编码，可用该环境变量覆盖。
- 排查文件乱码用 `cargo test --release -p xai-text-codec --test diagnose -- --nocapture`（需设 `DIAG_FILE`）。

## 上游合并

- Windows 构建辅助模块应与业务代码和主题移植保持独立。
- 合并上游版本后，先检查仓库 `bin/protoc` 使用的版本；只有上游版本变化时，才同步更新 `tools/windows-protoc/setup.ps1` 中的版本、下载地址和 SHA-256。
- 不得使用 Git 还原用户已有改动。发现无关的未提交文件时，应保留并绕开这些改动。
- Windows 下 `cc 1.2.48` 与 `find-msvc-tools 0.1.13` 存在临时文件标志类型不兼容；锁文件使用 `find-msvc-tools 0.1.5`，升级时需验证 Release 编译，勿修改 Cargo 缓存源码。

## 运行时模型与视觉

- `/model <未知ID>` 优先查询当前上游；未命中或请求失败后查询其他已配置且可选的连接。完整连接信息去重，独立认证；回退按本地配置顺序选择首个命中，响应快慢不影响优先级；错误提示显示脱敏后的上游地址。回退并发 4、总时限 25 秒，逻辑在 `handlers/model_discovery.rs`。
- 临时模型不写配置或默认模型，能力默认为视觉关闭、256000 上下文、无思考参数。
- 临时目录在 `agent/remote_config/manager/runtime.rs` 保存来源引用；鉴权路径必须识别临时模型，不能只从磁盘查找后误判为登录认证。
- `/support_vision [on|off]` 的覆盖只属于当前会话和模型；hook 与 sampler 使用 `session/acp_session_impl/sampler_turn.rs` 的同一轮次快照，禁止改回读取共享 `current_model_id`。
- 本地端到端验证：先完成 Release 构建，再运行 `python tools/test-model-discovery.py`。脚本使用临时配置和本地假上游，不需要真实凭据。

## Chat Completions 工具调用兼容

- `xai-grok-sampler/src/stream/chat_completions.rs` 按工具调用 `index` 合并响应片段；后续 `function.name` 为空字符串、null 或缺省时保留已有名称，流式事件也应跳过空名称更新。部分 DeepSeek 兼容接口会持续返回空名称占位符，合并上游时需保留此处理。
- 回归测试位于同文件的 `tool_call_stream_*`，覆盖交错调用、空名称、null、字段缺省和延迟名称；原始流始终缺少名称时保持原有错误路径。

## 插件系统（vision-bridge）

- 功能：主模型不支持视觉时，`UserPromptTransform` 钩子在用户消息发送前注入提示词，引导模型调用 vision MCP 工具识别图片（如 `opencode-vision-lite` 的 `vision_analyze`）；模型 `supports_vision=true` 时插件放行不注入。
- 合并/维护指南：见 `docs/vision-bridge-插件系统合并指南.md`；功能设计见 `docs/用户提示词变换钩子实现计划.md`。
- 插件本体在用户主目录 `~/.grok/plugins/vision-bridge/`，**不在仓库内**（`bridge.js` 三通道检测 + `hooks.json`）。迁移到新机器需单独复制该目录。
- 插件钩子命令必须用 `${GROK_PLUGIN_ROOT}` 占位符写成绝对路径（如 `node ${GROK_PLUGIN_ROOT}/hooks/bin/bridge.js`）。grok 执行钩子命令时 cwd 是工作区，相对路径（如 `node bin/bridge.js`）会静默失败。
- 会话启动与钩子重载通过 `with_plugin_hooks` 注册插件层，插件重载通过 `replace_plugin_hooks` 替换；不要再额外追加注册，避免重复执行。`spawn.rs` 必须在首轮前完成注册，并保留受管钩子禁令。
- 用户级插件从 `$GROK_HOME/plugins`（默认 `~/.grok/plugins`）发现，不从项目 `.grok/plugins` 之外的路径扫描。
