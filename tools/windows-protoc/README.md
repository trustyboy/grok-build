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

脚本只在构建进程期间设置 `PROTOC`，完成后恢复调用前的环境变量。持久目录中的压缩包、官方编译器、include 文件和 Wrapper 均不会删除。

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

需要运行其他 Cargo 命令时，可以在当前 PowerShell 会话中设置：

```powershell
$env:PROTOC = (Resolve-Path .\tools\windows-protoc\runtime\bin\protoc-wrapper.exe).Path
cargo check -p xai-grok-tools-api
```

## 更新版本

上游 `bin/protoc` 升级后，应同步修改 `setup.ps1` 中的版本、下载地址和 SHA-256，然后重新运行初始化脚本。旧 runtime 不会被脚本静默删除；如需更换已安装版本，应先人工归档旧目录。

