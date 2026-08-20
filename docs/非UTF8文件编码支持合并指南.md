# 非 UTF-8 文件编码支持合并指南

## 适用范围

本文用于将 `gf` 分支中的非 UTF-8 文件编码支持（GBK/Big5/Shift_JIS 等文本文件的透明读写）迁移到更新后的 grok 源码，或者在 grok 上游更新后维护该功能。

基线信息：

- grok 基线提交：`df58c78`（UserPromptTransform 钩子提交，编码支持改动起点）
- 功能分支：`gf`
- 补丁文件：`patches/non-utf8-encoding.diff`
- 实现细节与设计推导：`docs/非UTF8文件编码支持实现计划.md`

## 功能概述

改造前，工具链统一用 `String::from_utf8_lossy` 读文件、`as_bytes()` 写回。对 GBK 等非 UTF-8 文件，这会把每个非法字节替换成 U+FFFD 再以 UTF-8 全量覆盖写回，**原始字节不可逆丢失** —— 因为写入是全文覆盖，即使只改一行 ASCII 注释，全文中文也会被一次性摧毁。

本功能在文件系统抽象层加一层转码装饰器：

1. `read_file` 把非 UTF-8 文本解码后以 UTF-8 字节返回，工具层无感；
2. `write_file` 按目标文件的原编码写回，实现字节级往返无损；
3. 单文件 `grep` 透传 `--encoding`，让搜索结果与 `read_file` 所见一致。

对 LLM 侧零影响 —— 编码是 I/O 边界的事，模型看到的始终是正确的 Unicode 文本。

## 设计边界

- **刻意不改 5 套工具集的任何 read/edit/write 实现**。它们全部通过 Resources 注入拿 `AsyncFileSystem`，而生产注入点只有一处，包一层装饰器即可全覆盖。这是为压低上游合并成本做的核心取舍。
- **不改 `AsyncFileSystem` trait 定义**（tools 侧 `computer/types.rs`、workspace 侧 `file_system/fs.rs`）。字节级签名是正确的，转换层加在其上。
- **不动 `FsReadEncoding` 与 `walk.rs` 的 `encode_chunk`**。那是传输编码（utf8/base64），与字符编码无关。
- 不覆盖：ACP 远程路径（协议层只有 `ReadTextFile`/`WriteTextFile`）、子进程输出解码、会话回滚快照、`@file` 引用。详见实现计划文档「已知边界」。

## 文件清单

### 新文件

- `crates/codegen/xai-text-codec/Cargo.toml`
- `crates/codegen/xai-text-codec/src/lib.rs` —— 编码探测与转码核心
- `crates/codegen/xai-text-codec/tests/diagnose.rs` —— 真实文件诊断工具（`DIAG_FILE` 驱动，默认跳过）
- `crates/codegen/xai-grok-tools/src/computer/transcoding_fs.rs` —— `TranscodingFs` 装饰器
- `crates/codegen/xai-grok-tools/tests/transcoding_fs.rs` —— 装饰器行为验证
- `docs/非UTF8文件编码支持实现计划.md`
- `docs/非UTF8文件编码支持合并指南.md`（本文）

### 修改文件

- `Cargo.toml` —— `members` 注册 `xai-text-codec`；`[workspace.dependencies]` 新增 `chardetng`、`xai-text-codec`
- `crates/codegen/xai-grok-tools/Cargo.toml` —— `encoding_rs` 去掉 `optional`；新增 `xai-text-codec`
- `crates/codegen/xai-grok-tools/src/computer/mod.rs` —— 挂 `transcoding_fs` 模块
- `crates/codegen/xai-grok-tools/src/registry/types.rs` —— **唯一的生产接入点**
- `crates/codegen/xai-grok-tools/src/implementations/grok_build/grep/mod.rs` —— 单文件编码透传

## 推荐合并流程

### 方法一：合并功能分支

```powershell
git switch gf
git merge <上游分支>
```

本功能的主体在新增 crate 与新增文件中，对现有文件只有 59 行改动，冲突面很小。若发生冲突，按下方检查表逐项确认接入点仍然存在。

### 方法二：应用补丁

```powershell
git apply --check patches/non-utf8-encoding.diff
git apply --3way patches/non-utf8-encoding.diff
```

如果 `--check` 失败，按文件清单手工接入 —— 只有 4 个接入点，见下。

## 冲突处理检查表

### 1. 装饰器注入点（最关键）

`crates/codegen/xai-grok-tools/src/registry/types.rs`，在 `resources.insert(Terminal(ctx.backend))` 之后确认 `FileSystem` 被装饰器包裹：

```rust
resources.insert(crate::types::resources::FileSystem(std::sync::Arc::new(
    crate::computer::transcoding_fs::TranscodingFs::new(ctx.fs),
)));
```

**若上游把这行改回 `FileSystem(ctx.fs)`，整个功能会静默失效**（不报错、不崩溃，只是回到 lossy 行为）。合并后务必确认这一处。

其余 20 余处 `FileSystem(Arc::new(LocalFs))` 都在 `#[cfg(test)]` 内，无需改动 —— 测试用裸 `LocalFs` 是刻意的，装饰器行为由独立的集成测试覆盖。

### 2. 模块挂载

`crates/codegen/xai-grok-tools/src/computer/mod.rs` 中存在：

```rust
pub mod transcoding_fs;
```

### 3. `AsyncFileSystem` trait 是否变化

`TranscodingFs` 实现了 `read_file` / `write_file` / `delete_file` / `file_exists` 四个方法。若上游给 trait 增加了新方法，需要在 `transcoding_fs.rs` 中补上对应的委托实现（除读写外一律直接转发给 `inner`）。

### 4. grep 编码透传

`implementations/grok_build/grep/mod.rs` 中：

- `prepare_grep` 之前存在辅助函数 `single_file_encoding_label`；
- 命令参数构造末尾（`--max-filesize` 之后）存在透传逻辑。

若上游重构了参数构造流程，把透传移到新的构造位置即可，顺序不敏感。

### 5. Cargo 依赖

- 根 `Cargo.toml` 的 `members` 含 `crates/codegen/xai-text-codec`；
- `[workspace.dependencies]` 含 `chardetng` 与 `xai-text-codec`；
- `xai-grok-tools` 的 `encoding_rs` **没有** `optional = true`（上游原本是 optional 且零引用的死依赖）。

## 验证

```powershell
# 1. 编解码核心
cargo test --release -p xai-text-codec --lib

# 2. 装饰器行为
cargo test --release -p xai-grok-tools --test transcoding_fs

# 3. 完整构建
& .\tools\windows-protoc\build-release.ps1
```

预期：单元测试 15 项全通过，集成测试 10 项全通过。

> `cargo test -p xai-grok-tools --lib` 在 Windows 上无法编译，这是与本功能无关的上游既有问题（`computer/local/terminal.rs:2992` 的 `parse_login_env_capture` 标了 `#[cfg(unix)]`，但 `terminal.rs:5004`、`terminal.rs:5034` 两个测试没标）。装饰器测试因此放在 `tests/` 目录。

排查某个文件读出来是乱码时，用诊断工具直接观察判定过程：

```powershell
$env:DIAG_FILE = "R:\Temp\AppCommon.pas"
cargo test --release -p xai-text-codec --test diagnose -- --nocapture
```

## 注意事项

### 构建产物不会自动安装

`build-release.ps1` 只产出 `target\release\xai-grok-pager.exe`，**不会**复制到 `~\.grok\bin\grok.exe`。验证新行为前需手动安装：

```powershell
Copy-Item "$env:USERPROFILE\.grok\bin\grok.exe" "$env:USERPROFILE\.grok\bin\grok.exe.bak" -Force
Copy-Item "target\release\xai-grok-pager.exe" "$env:USERPROFILE\.grok\bin\grok.exe" -Force
```

### 测试样本的长度要求

装饰器测试的样本必须让非 ASCII 字节数 ≥ 64，否则会走先验路径，结果随运行机器的区域设置变化（中文 Windows 判 GBK、韩文 Windows 判 EUC-KR、英文 Linux 无先验），造成跨平台不稳定。先验路径本身由 `xai-text-codec` 的 `weak_sample_*` 单元测试覆盖（显式传入 `preferred`，不依赖环境）。

### 用户可覆盖编码先验

繁体项目在中文 Windows 上，中文字数少的 Big5 文件会被 GBK 先验带偏。设 `GROK_FILE_ENCODING=big5` 可覆盖（接受 WHATWG label）。

## 更新补丁文件

功能提交变化后，从共同基线重新生成补丁：

```powershell
git diff --binary --full-index df58c78...HEAD --output=patches/non-utf8-encoding.diff -- . ":(exclude)patches/**"
```

随后验证：

```powershell
git apply --check --reverse patches/non-utf8-encoding.diff
```
