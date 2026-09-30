# Windows protoc 永久兼容方案

该模块解决 Grok Build 在原生 Windows 上调用 `protoc` 时遇到的 Unix 设备路径兼容问题，同时保持上游 `xai-proto-build` 源码不变。

## 原因

`crates/build/xai-proto-build` 在生成依赖信息时固定传入：

```text
--dependency_out=/dev/stdout
--descriptor_set_out=/dev/null
```

官方 Windows `protoc.exe` 不能直接使用 `/dev/stdout` 和 `/dev/null`。本模块的 Wrapper 会把它们改写为单次调用的临时文件，调用官方编译器，再把 dependency 内容恢复成上游解析器要求的 `/dev/null:` 格式。

## 安装

在仓库根目录执行：

```powershell
& .\tools\windows-protoc\setup.ps1
```

初始化过程会：

1. 下载仓库 `bin/protoc` 同版本的官方 `protoc 29.3` Windows x64 压缩包。
2. 校验固定的 SHA-256：`57EA59E9F551AD8D71FFAA9B5CFBE0CA1F4E720972A1DB7EC2D12AB44BFF9383`。
3. 解压到 `tools/windows-protoc/runtime`。
4. 使用当前 Rust 工具链编译 `protoc-wrapper.exe`。

`runtime` 被局部 `.gitignore` 排除，避免把第三方二进制和压缩包提交进补丁；它不会被初始化脚本或构建脚本删除，切换分支和合并上游代码时也会继续保留。

## Release 构建

```powershell
& .\tools\windows-protoc\build-release.ps1
```

额外 Cargo 参数可以直接追加，例如：

```powershell
& .\tools\windows-protoc\build-release.ps1 --locked
```

构建和测试入口通过 `invoke-cargo-release.ps1` 共用工具环境，仅在执行 Cargo 期间设置 `PROTOC` 和 `GROK_TOOLS_BUNDLE_RG_PATH`，完成或失败后均恢复原值。持久目录中的压缩包、官方编译器、include 文件和 Wrapper 均不会删除。

## Release 定向测试与缓存复用

测试也使用同一工具环境，避免测试后正式构建因 ripgrep 打包配置变化而重新编译：

```powershell
& .\tools\windows-protoc\test-release.ps1 -p xai-grok-shell --lib model_discovery
```

传给测试程序的参数需要用带引号的 `'--'` 分隔，防止 PowerShell 将它当作自身的参数结束符：

```powershell
& .\tools\windows-protoc\test-release.ps1 -p xai-grok-shell --lib model_discovery '--' --nocapture
```

同一批修改有多个过滤条件时，可以先用 `--no-run` 编译一次，再使用 Cargo 输出的测试可执行文件路径分别运行过滤条件。代码变化后需要重新编译测试程序。

保留 Release 增量编译；固定 Rust 版本、目标架构、功能开关、`RUSTFLAGS` 和工具路径。构建与测试自身的优化、panic 和特性差异仍可能生成不同产物，共享工具环境只减少额外的配置切换。

## 受控清理旧增量缓存

先预览：

```powershell
& .\tools\windows-protoc\clean-incremental.ps1
```

确认清单后执行：

```powershell
& .\tools\windows-protoc\clean-incremental.ps1 -Apply
```

默认规则：

- 清理范围固定为本仓库 `target/release/incremental` 下的完整缓存目录。
- 每个 crate 至少保留最新 2 个变体，额外变体中只选择超过 14 天未修改的目录；可通过 `-KeepLatest`、`-OlderThanDays` 调整。
- 保留独有缓存、近期缓存和无法按名称归属包的 `build_script_build` 缓存。
- 检测到 Cargo/rustc 运行、无法独占 Release 构建锁或路径含链接时停止或跳过；删除前再次检查候选是否发生变化。
- `-Apply -WhatIf` 只预览删除操作。正常构建不会自动清理。
- `deps`、`build`、正式可执行文件和 `tools/windows-protoc/runtime` 均保留。

修改时间用于保守筛选，不能证明缓存完全失效；再次使用已清理的旧变体时会重新生成缓存。预览大小按文件逻辑大小统计，硬链接可能使该值高于实际释放空间。

## ripgrep 打包

`build-release.ps1` 还会自动发现本机 ripgrep 并打包进产物，使 `grep` 工具自包含。

### 为什么需要

grok 的 `grep` 工具是外挂 ripgrep 子进程实现的。Linux/macOS 上 `build.rs` 会自动下载 ripgrep 的 `.tar.gz` 并嵌入二进制，用户无感；但 ripgrep 官方在 Windows 上只发 `.zip`，构建脚本没有解压路径，因此 `build.rs` 直接跳过打包（见其中 `Skip auto-bundling on Windows` 注释），产物退化为依赖目标机器 PATH 中的 `rg`。

缺失时 grep 只会返回 `program not found`，模型看到的是「搜索无结果」，排查成本很高。

### 行为

脚本在构建前查找 `rg`，找到就设 `GROK_TOOLS_BUNDLE_RG_PATH`（`build.rs` 在显式指定该变量时**不会**跳过 Windows 打包），并在构建结束后恢复环境变量。

winget 安装的 `rg.exe` 是长度为 0 的命令别名符号链接，脚本会 `ResolveLinkTarget` 解析到真实可执行文件 —— 否则打包进去的会是一个空文件。

找不到 ripgrep 时**不会阻断构建**，只打印警告，产物仍可用但 grep 依赖目标机器 PATH。安装：

```powershell
winget install BurntSushi.ripgrep.MSVC
```

### 配套的运行时修复

`implementations/grok_build/grep/ripgrep.rs` 的 `resolve_bundled_rg()` 原本把嵌入的二进制解到 `~/.grok/vendor/rg-<版本>-<目标>`，**没有 `.exe` 后缀**。Windows 的 `CreateProcessW` 不会为显式程序路径补扩展名，这样解出的文件无法执行。该函数已用 `#[cfg(windows)]` 补上后缀，Unix 侧行为不变（仍靠 mode 0o755）。

打包生效后，产物 exe 复制到任何 Windows 机器都能直接使用 grep，无需逐台安装 ripgrep。体积增加约 4 MB。

## 手动使用

需要 Release 静态编译检查时，可以复用相同环境：

```powershell
& .\tools\windows-protoc\invoke-cargo-release.ps1 -CargoCommand check -CargoArguments @('-p', 'xai-grok-tools-api')
```

该入口固定附加 `--release`；正式程序构建继续使用 `build-release.ps1`。

## 更新版本

上游 `bin/protoc` 升级后，应同步修改 `setup.ps1` 中的版本、下载地址和 SHA-256，然后重新运行初始化脚本。旧 runtime 不会被脚本静默删除；如需更换已安装版本，应先人工归档旧目录。

