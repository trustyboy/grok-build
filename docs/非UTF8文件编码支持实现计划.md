# 非 UTF-8 文件（GBK 等）编码支持实现计划

> 状态：已实施（Phase 0–2 完成并通过验证）
> 创建日期：2026-08-19

## 一、问题背景

grok 读写 GBK/Big5/Shift-JIS 等非 UTF-8 文件时会把文件改成乱码。这**不是显示层问题，而是磁盘数据不可逆损坏**。

### 根因链（已实测验证）

| 步骤 | 关键代码 | 行为 |
|---|---|---|
| 1. 二进制检测放行 | `crates/codegen/xai-grok-tools/src/util/binary.rs:19` `is_binary()` | 只查 NUL 字节与控制字符比例。GBK 汉字字节（0x81–0xFE）两者都不沾，必然被判为文本放行 |
| 2. 读取时丢失字节 | `implementations/grok_build/read_file/mod.rs:480` 等 | `String::from_utf8_lossy` 把非法字节替换成 U+FFFD，原始字节**永久丢失** |
| 3. 写回时全量覆盖 | `implementations/grok_build/search_replace/mod.rs:715` 等 | `write_text.as_bytes()` 以 UTF-8 全量覆盖整个文件 |

### 实测数据

GBK 的「你好世界 hello」：

```
原始 GBK:  c4 e3 ba c3 ca c0 bd e7 20 68 65 6c 6c 6f          (14 字节)
lossy 后:  U+FFFD x7 + " hello"
写回磁盘:  ef bf bd ef bf bd ... ef bf bd 20 68 65 6c 6c 6f   (27 字节)
```

**因为写入是全文覆盖，即使模型只改一行 ASCII 注释，全文中文也会被一次性摧毁。**

### 为什么当前没有防线

- `encoding_rs` 虽在 `crates/codegen/xai-grok-tools/Cargo.toml:14` 声明，但 `optional = true` 且无 feature 启用、**全仓零引用**，是死依赖；
- `chardetng`（编码探测库）不在 `Cargo.lock` 中，需新增。

## 二、目标与范围

**目标**：非 UTF-8 文本文件读取时正确解码、写回时按原编码还原，实现字节级往返无损。

**对 LLM 侧零影响**：编码只是字节层的表示方式，解码后的字符序列是同一个。模型看到的始终是正确的 Unicode 文本，token 消耗也完全一致。编码是 I/O 边界的事，不应泄漏到模型层。

**范围**：

- ✅ 文件读写核心（read_file / edit / write / apply_patch），覆盖全部 5 套工具集
- ✅ grep 搜索一致性
- ✅ GBK 遇不可编码字符时自动升级 GB18030
- ❌ 子进程输出解码（bash 的 Windows GBK 控制台输出）

**关键约束**：本仓库是开源上游 monorepo 的 fork（历史中有多次 `Synced from monorepo`）。方案必须把对**现有文件**的改动压到最低，否则每次上游同步都要处理冲突。**这一约束直接决定了架构选型。**

## 三、核心设计：转码装饰器

### 3.1 为什么不逐个改工具

朴素方案是把 11 个读点、9 个写点逐一替换，分布在 5 套工具集（`grok_build` / `grok_build_concise` / `grok_build_hashline` / `codex` / `opencode`）的 15 个文件里。功能可行，但每个改动点都落在上游高频变动的业务文件中，合并冲突成本极高。**已放弃。**

### 3.2 采用方案

全部 5 套工具集都通过 Resources 注入拿 `fs`，而**生产环境的注入点只有一处**：

```rust
// crates/codegen/xai-grok-tools/src/registry/types.rs:998
resources.insert(crate::types::resources::FileSystem(ctx.fs));
```

> 其余 20 余处 `FileSystem(Arc::new(LocalFs))` 全部位于 `#[cfg(test)]` 内，不影响生产路径。
> `ctx.fs` 来源：`registry/types.rs:231` 的 `ToolContext.fs: Arc<dyn AsyncFileSystem>`。

在此处包一层装饰器：

```rust
resources.insert(FileSystem(Arc::new(TranscodingFs::new(ctx.fs))));
```

- `read_file`：读原始字节 → 判定为非 UTF-8 文本则解码 → **返回 UTF-8 字节**
- `write_file`：收到 UTF-8 字节 → 探测目标文件当前编码 → 按原编码编码 → 写回

**效果**：工具层代码一行不改，`String::from_utf8_lossy` 全部保留原样 —— 因为它们拿到的字节已经是合法 UTF-8，lossy 分支不再触发。5 套工具集自动全覆盖。

### 3.3 无需跨工具传递编码状态

所有写回点在写之前**本来就重新完整读了一遍文件**（`search_replace/mod.rs:536`、`opencode/edit/mod.rs:359`、`codex/apply_patch/tool.rs:222` 等）。探测是纯函数（同样字节 → 同样编码），装饰器在 `write_file` 内自行探测目标文件即可。

**不需要** Resources 共享状态或 `persistence.rs` 持久化，也不存在并发失效或 mtime 过期问题。这消除了本方案最大的复杂度来源。

### 3.4 二进制安全：白名单判据

装饰器位于所有文件 I/O 的最底层，会看到图片、PDF、PPTX 等二进制流，**误转码等于损坏文件**。只有全部条件满足才转码：

1. 字节**不是**合法 UTF-8（合法 UTF-8 直接透传，零成本快速路径）；
2. 复用现有 `crate::util::binary::is_binary(&ext, &bytes)`（`util/binary.rs:19`）判定为文本；
3. `chardetng` 探测置信度 ≥ 阈值；
4. 探测结果落在 **CJK 传统编码白名单**内（GBK / GB18030 / Big5 / Shift_JIS / EUC-JP / EUC-KR）。

任一条不满足 → **原样透传字节**，行为与现状完全一致。白名单而非黑名单是这里的安全底线。

### 3.5 新增独立 crate：`xai-text-codec`

位置：`crates/codegen/xai-text-codec/`，只依赖 `encoding_rs` + `chardetng`。

> 不放 `xai-file-utils`：该 crate 语义是「S3/GCS 上传队列与 blob 存储」（`lib.rs:8`），已带 aws-sdk/reqwest/opentelemetry 等重依赖。独立 crate 位于依赖图底层，tools 与 workspace 两侧均可引用。

核心 API：

```rust
pub struct DecodedText {
    pub text: String,                 // 已解码文本，无 lossy 替换
    pub encoding: &'static Encoding,  // 写回用编码
    pub had_bom: bool,
}

pub fn decode(bytes: &[u8]) -> Result<DecodedText, CodecError>;
pub fn encode_or_upgrade(text: &str, encoding: &'static Encoding, had_bom: bool) -> Result<Vec<u8>, CodecError>;
```

**探测顺序**（前三步为快速路径，保证无性能回退）：

1. 纯 ASCII / 空文件 → 直接 `UTF_8`，零分配；
2. BOM 嗅探 `Encoding::for_bom()` → `had_bom = true`。**只接受 UTF-8 BOM**：UTF-16/32 的 encoder 在 WHATWG 规范下映射到 UTF-8，无法按原编码写回；
3. 严格 `std::str::from_utf8()`（SIMD 加速）→ 成功即 UTF-8。**绝大多数文件在此命中，不触碰 chardetng**；
4. `chardetng::EncodingDetector` 探测，采样 ≤64KB；
5. 白名单校验 → 严格解码（无替换）→ **往返校验**。

### 关键实现决策：用往返校验代替置信度阈值

> 这是实施阶段与原设计的**重要偏差**，务必知悉。

原计划设想用「置信度 ≥ 70」作为探测可信度判据，但实际接入时发现 **`chardetng` 1.0.0 根本不提供置信度数值**（其 0.1.x 的 `guess_assess` 在 1.0 中已移除，公开 API 只有 `new` / `feed` / `guess` / `tld_may_affect_guess`）。

改用更强的判据 —— **往返校验**：

```rust
let (re_encoded, _, had_unmappable) = guess.encode(&text);
if had_unmappable || re_encoded.as_ref() != bytes {
    return Err(CodecError::UnknownEncoding { reason: "往返校验不一致，拒绝转码" });
}
```

即「解码后再编码必须逐字节还原原文」。这比置信度更可靠：它把「探测是否正确」这个概率问题，转化成了一个**可验证的等式**，并直接保证了核心不变量 —— 凡是允许转码的文件，写回时必然能还原。

代码位置：`crates/codegen/xai-text-codec/src/lib.rs` 的 `verify_candidate()`。

**性能影响可忽略**：往返校验只在「非 ASCII 且非合法 UTF-8」的文件上执行，UTF-8 项目一次都不会触发；全程内存操作无临时文件；`encoding_rs` 吞吐在 GB/s 量级，100KB 文件约 0.2ms。

### 往返校验的局限，与 preferred 先验机制

> 这是**实测真实文件后追加**的第二个关键机制，起因是一次线上失效。

**真实案例**：`R:\Temp\AppCommon.pas`（Delphi 源码，4677 字节）里只有 **8 个非 ASCII 字节** —— `b9 fe` 重复 4 次。诊断结果：

```
chardetng 猜测: EUC-KR
  GBK        严格解码=true  往返一致=true    ← 「哈哈哈哈」（正确）
  Big5       严格解码=true  往返一致=true
  EUC-KR     严格解码=true  往返一致=true    ← 「벗벗벗벗」（被选中，错误）
  Shift_JIS  严格解码=false 往返一致=None
```

**往返校验只能排除「字节序列非法」的候选，无法区分多个都合法的编码。** 模型因此读到韩文乱码，转而用 PowerShell 手动读写文件，绕过了整个工具层。

**解决机制**：非 ASCII 字节数低于 `SUFFICIENT_NON_ASCII`（64 字节，约 32 个 CJK 字符）时，判定 `chardetng` 缺乏统计依据，改用先验编码：

| 优先级 | 来源 |
|---|---|
| 1 | `GROK_FILE_ENCODING` 环境变量（如 `gbk`、`big5`，接受 WHATWG label） |
| 2 | 系统 ANSI 代码页（Windows `GetACP()`：936→GBK、949→EUC-KR、950→Big5、932→Shift_JIS；Unix 解析 `LC_ALL`/`LC_CTYPE`/`LANG`） |

依据是：**遗留文件几乎都用保存者的系统默认编码写成**。样本充分时仍然信任 `chardetng`，先验不会覆盖有依据的判断。

修复后同一文件的诊断输出：

```
系统/配置先验: Some("GBK")
=> decode 成功，编码=GBK
=> 非 ASCII 片段: 哈哈哈哈
```

相关 API：`decode()`（自动取先验）、`decode_with(bytes, preferred)`（显式指定）、`preferred_fallback()`。

### 纵深防御：NUL 字节判定

引入先验后暴露出一个安全回退：PNG 头部字节能被 GBK「成功」解码且往返一致，绕过了原有的拒绝逻辑。因此在 `detect_legacy()` 中增加 NUL 字节检查 —— 任何传统文本编码都不会在正常文本里产生 NUL。这道防线不依赖调用方的 `is_binary`，属于 crate 自带的纵深防御。


**写回策略**：

- `encoding_rs::Encoding::encode()` 返回的第三个值 `had_unmappable: bool` **必须检查**，否则会静默产生 `&#128512;` 这类 HTML 数字引用混进代码文件；
- GBK 遇 unmappable → 自动升级 **GB18030**（GBK 的字节兼容超集，能表示全部 Unicode，纯中文内容字节完全不变）；
- Big5/Shift-JIS 等无兼容升级路径 → 返回 `Err`，让写入失败并向上报错，**绝不静默损坏**；
- 目标文件不存在（新建）→ 一律 UTF-8；
- 现有 CRLF 保留逻辑（`search_replace/mod.rs:576-581, 710-714`）在工具层，与装饰器层正交，无需改动也不冲突。

## 四、实施阶段

### Phase 0 — 地基

- 新建 `crates/codegen/xai-text-codec/`，实现 API + 探测 + 单元测试；
- 根 `Cargo.toml`：`members`（第 8 行起）注册新 crate，`[workspace.dependencies]`（第 106 行起）新增 `chardetng`；
- `xai-grok-tools/Cargo.toml:14`：`encoding_rs` 去掉 `optional = true`，新增 `xai-text-codec` 依赖。

### Phase 1 — 装饰器接入

- 新增 `crates/codegen/xai-grok-tools/src/computer/transcoding_fs.rs`；
- `computer/mod.rs` 挂模块；
- `registry/types.rs:998` 改 1 行接入。

> 读写必须在同一阶段合入，不可拆分 —— 只改读取会制造更糟的中间态（模型看到正确中文，一编辑反而坏得更彻底）。

### Phase 2 — grep 一致性

grep 外挂 ripgrep 子进程直读磁盘，**绕过了 fs 抽象**，装饰器覆盖不到，需单独处理。

**约束**：ripgrep 的 `--encoding` 是**全局单值**（"on all files searched"），盲传 `--encoding gbk` 会把 UTF-8 文件也当 GBK 解，制造新乱码。

- 单文件 grep：先探测，非 UTF-8 则传 `--encoding <label>`（`encoding_rs` 的 `name()` 即 WHATWG label，ripgrep 直接接受）；
- 多文件/目录 grep：保持现状并注释为已知限制；
- 不做「对 rg 输出二次编码修复」—— rg 输出已是被当 UTF-8 解释过的字节，不可逆。

改动位置：`implementations/grok_build/grep/mod.rs:761-827`（参数构造处）。

### Phase 3 — 文档化

## 五、关键代码位置速查

**新增文件**（无合并冲突）：

- `crates/codegen/xai-text-codec/Cargo.toml`
- `crates/codegen/xai-text-codec/src/lib.rs` — `decode()` / `encode()` / `encode_or_upgrade()` / `detect_legacy()`
- `crates/codegen/xai-grok-tools/src/computer/transcoding_fs.rs` — `TranscodingFs`，核心为 `decode_to_utf8()` 与 `encode_for_target()`
- `crates/codegen/xai-grok-tools/tests/transcoding_fs.rs` — 装饰器行为验证

**修改文件**（生产代码实际改动极小）：

| 文件 | 改动 |
|---|---|
| `xai-grok-tools/src/registry/types.rs:998` | 包装装饰器（唯一的生产接入点） |
| `xai-grok-tools/src/computer/mod.rs` | 挂 `transcoding_fs` 模块 |
| 根 `Cargo.toml` | members 注册 + `chardetng = "1.0.0"` + `xai-text-codec` |
| `xai-grok-tools/Cargo.toml` | `encoding_rs` 去 optional + `xai-text-codec` |
| `implementations/grok_build/grep/mod.rs` | 新增 `single_file_encoding_label()`，在参数构造末尾透传 `--encoding` |

**参考位置（不改动）**：

- `xai-grok-tools/src/computer/types.rs:51-74` — tools 侧 `AsyncFileSystem` trait（字节级签名，正确）
- `xai-grok-workspace/src/file_system/fs.rs:15-43` — workspace 侧 `AsyncFileSystem` trait
- `xai-grok-tools/src/types/resources.rs:834` — `pub struct FileSystem(pub Arc<dyn AsyncFileSystem>)`
- `xai-grok-workspace-types/src/rpc/fs.rs:347` — `FsReadEncoding`，是**传输编码**（utf8/base64），与字符编码无关，不要动
- `xai-grok-workspace/src/file_system/walk.rs:284` — `encode_chunk`，同上，传输层

## 六、验证结果

项目规定**只编译 Release**：`& .\tools\windows-protoc\build-release.ps1`

### 已通过的测试

| 验证项 | 命令 | 结果 |
|---|---|---|
| 编解码单元测试 | `cargo test --release -p xai-text-codec --lib` | **15/15 通过** |
| 装饰器集成测试 | `cargo test --release -p xai-grok-tools --test transcoding_fs` | **10/10 通过** |
| 主代码静态检查 | `cargo check --release -p xai-grok-tools` | 通过（仅既有 warning） |
| 真实文件诊断 | `cargo test --release -p xai-text-codec --test diagnose` | GBK 判定正确 |

装饰器测试覆盖：GBK 读取转 UTF-8、UTF-8 原样透传、二进制（PNG）读写原样透传、GBK 编辑后磁盘仍为 GBK 且可严格解码、**未编辑区域字节逐字节不变**、新建文件为 UTF-8、GBK + emoji 自动升级 GB18030、Big5 完整往返。

### 测试样本的长度要求（重要）

装饰器测试的样本**必须让非 ASCII 字节数 ≥ 64**，否则会走先验路径，测试结果将随运行机器的区域设置变化（中文 Windows 判 GBK、韩文 Windows 判 EUC-KR、英文 Linux 无先验），造成跨平台不稳定。

这一点在实施中真实踩到过：最初的 Big5 样本只有 54 字节非 ASCII，在中文 Windows 上被 GBK 先验解成乱码而失败。先验路径本身由 `xai-text-codec` 的 `weak_sample_*` 单元测试专门覆盖（显式传入 `preferred`，不依赖环境）。

### 编码诊断工具

排查「某文件读出来是乱码」时，用 `crates/codegen/xai-text-codec/tests/diagnose.rs`：

```powershell
$env:DIAG_FILE = "R:\Temp\AppCommon.pas"
cargo test --release -p xai-text-codec --test diagnose -- --nocapture
```

会输出 `chardetng` 原始猜测、各候选编码的严格解码与往返情况、当前生效的先验，以及最终结论。不设 `DIAG_FILE` 时自动跳过，不影响 CI。

### grep 实测数据

```
$ rg "中文" sample.txt                    # 当前行为
退出码: 1                                  # 搜不到

$ rg --encoding gbk "中文" sample.txt      # 修复后行为
第一行：中文注释内容
第二行：另一段中文文本
退出码: 0
```

### 测试放在集成测试目录的原因

装饰器测试位于 `crates/codegen/xai-grok-tools/tests/transcoding_fs.rs` 而非 `#[cfg(test)] mod tests`，原因有二：

1. 装饰器全部走公开 API，集成测试完全够用；
2. **`xai-grok-tools` 的 lib test 在 Windows 上无法编译** —— 这是与本次改造无关的既有问题：`computer/local/terminal.rs:2992` 的 `parse_login_env_capture` 标了 `#[cfg(unix)]`，但引用它的两个测试（`terminal.rs:5004`、`terminal.rs:5034`）没有标注。集成测试编译的是 lib 的非 test 配置，可以绕开该问题，让 Windows 本地也能真实跑通验证。

> 若将来要修复该既有问题，只需给那两个测试函数补 `#[cfg(unix)]`。本次改造刻意未动，以免增加上游合并冲突。


## 七、已知边界与代价

### 装饰器方案的代价

1. **语义隐式**：转码发生在 fs 层，读工具代码时不易察觉。缓解：`transcoding_fs.rs` 内写足注释 + 本文档。
2. **现有测试不覆盖**：测试用裸 `LocalFs`，装饰器行为需新增专门测试。
3. **写时多一次读**：`write_file` 需读目标文件探测编码。编辑操作本就读过全文，OS page cache 命中，成本可忽略；可对超大文件设上限直接透传。
4. **工具层拿不到编码信息**，无法向模型展示「此文件是 GBK」。若后续需要，可单点增强 `read_file` 输出。

### 不解决的问题

1. **ACP 远程路径**。`xai-grok-workspace/src/file_system/adapter.rs:38-47` 的 `read_file` 拿到的是 ACP 协议返回的 `String`（已由编辑器解码），`write_file:50-52` 强制 `String::from_utf8`；且 `agent-client-protocol` 只有 `ReadTextFile`/`WriteTextFile`，**没有二进制读写请求**。这是协议层限制 —— ACP 模式下文件 I/O 委托给编辑器，本就该由编辑器处理编码。
2. **多文件 grep 的混合编码**：ripgrep 全局单值限制。
3. **子进程输出解码**：`implementations/grok_build/bash/mod.rs:431` 的 lossy。不损坏文件，本次不改。项目已有 `xai-grok-config/src/shell.rs:300-303` 的 `PYTHONUTF8` 先例可供未来参考。
4. **模型绕过工具层直接读写文件**：观察到模型在遇到乱码时会改用 `run_terminal_command` 执行 PowerShell（`[System.IO.File]::ReadAllBytes` + `GetString`）手动读写文件，这条路径**完全绕过 `AsyncFileSystem` 抽象**，装饰器无法介入。

   真实案例（会话 `01a01cf3-afd8-7b72-a3bd-1dea4f6740b5`）中，模型调用 `run_terminal_command` 21 次，远超 `search_replace` 的 12 次，正是因为编码判错后读到乱码而转向手动处理。

   技术上无法拦截子进程的磁盘操作。当前策略是**先把编码判对**，让模型没有理由绕过工具层。若后续仍观察到此现象，可考虑在系统提示词中引导优先使用 `edit`。
5. **会话回滚（rewind）的文件快照**：`xai-grok-workspace/src/session/file_state.rs` 走 workspace 侧的 `bytes_to_string`（`file_system/fs.rs:45`，`String::from_utf8` 严格解码）。对 GBK 文件快照会失败并返回 `Err`，导致该文件无法参与回滚 —— 但因为是**严格解码而非 lossy**，不会损坏文件。属独立功能，本次未覆盖。
6. **`@file` 引用**：`xai-grok-workspace/src/file_system/attach_file.rs:60` 用 `String::from_utf8(...).ok()`，非 UTF-8 文件会被**静默丢弃**（表现为引用了却没内容）。同样不损坏文件，未在本次范围内。

### 最大风险：编码探测误判

GBK/Big5/Shift-JIS/EUC-KR 字节范围高度重叠，误判后果比现状更隐蔽（不是显眼的 `�`，而是伪装成正常中文的错字）。因此采用**置信度阈值 + CJK 白名单 + 短文件直接拒绝**的三重保守策略，宁可透传也不猜 —— 这是本方案最重要的安全边界。
