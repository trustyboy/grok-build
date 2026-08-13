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

- 禁止执行不带 `--release` 的 `cargo build`。
- 禁止通过 `cargo run` 隐式触发 Debug 构建。
- 不得为了快速验证而主动编译 Debug 产物；需要验证时，应优先使用不会生成 Debug 可执行文件的静态检查或针对性测试。若验证命令会触发 Rust 编译，也必须显式使用 Release 配置。
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

## 上游合并

- Windows 构建辅助模块应与业务代码和主题移植保持独立。
- 合并上游版本后，先检查仓库 `bin/protoc` 使用的版本；只有上游版本变化时，才同步更新 `tools/windows-protoc/setup.ps1` 中的版本、下载地址和 SHA-256。
- 不得使用 Git 还原用户已有改动。发现无关的未提交文件时，应保留并绕开这些改动。

## 插件系统（vision-bridge）

- 功能：主模型不支持视觉时，`UserPromptTransform` 钩子在用户消息发送前注入提示词，引导模型调用 vision MCP 工具识别图片（如 `opencode-vision-lite` 的 `vision_analyze`）；模型 `supports_vision=true` 时插件放行不注入。
- 合并/维护指南：见 `docs/vision-bridge-插件系统合并指南.md`；功能设计见 `docs/用户提示词变换钩子实现计划.md`。
- 插件本体在用户主目录 `~/.grok/plugins/vision-bridge/`，**不在仓库内**（`bridge.js` 三通道检测 + `hooks.json`）。迁移到新机器需单独复制该目录。
- 插件钩子命令必须用 `${GROK_PLUGIN_ROOT}` 占位符写成绝对路径（如 `node ${GROK_PLUGIN_ROOT}/hooks/bin/bridge.js`）。grok 执行钩子命令时 cwd 是工作区，相对路径（如 `node bin/bridge.js`）会静默失败。
- 会话启动时插件钩子随 hook_registry 一起注册（`spawn.rs` 调用 `append_plugin_hooks_to`）；不要依赖 `/hooks reload` 等 mid-session 事件才加载。
- 用户级插件从 `$GROK_HOME/plugins`（默认 `~/.grok/plugins`）发现，不从项目 `.grok/plugins` 之外的路径扫描。
