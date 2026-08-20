//! 文件系统装饰器：在字节层透明地处理非 UTF-8 文本文件的编解码。
//!
//! # 为什么放在这一层
//!
//! 工具层有 5 套并行实现（`grok_build` / `grok_build_concise` / `grok_build_hashline`
//! / `codex` / `opencode`），各自都有独立的 read/edit/write，逐个改造需要动十几个
//! 上游高频变动的业务文件，合并成本极高。
//!
//! 而它们全都通过 Resources 注入拿 [`AsyncFileSystem`]，生产路径上的注入点只有
//! `registry::types` 一处。在那里包一层装饰器，即可让全部工具集自动获得正确的编码
//! 处理，业务代码一行不用改 —— 它们拿到的字节已经是合法 UTF-8，原有的
//! `String::from_utf8_lossy` 不再触发替换。
//!
//! # 契约
//!
//! - `read_file`：非 UTF-8 的文本文件解码后**以 UTF-8 字节返回**；其余情况原样透传。
//! - `write_file`：传入的 UTF-8 字节按目标文件的**原编码**写回；其余情况原样透传。
//!
//! # 安全原则
//!
//! 装饰器位于所有文件 I/O 的最底层，会看到图片、PDF、PPTX 等二进制流，**误转码等于
//! 损坏文件**。因此一切判据都以「不确定就透传」为准：任何一步存疑，都退化成改造前
//! 的行为，绝不会让现有场景变坏。具体的编码白名单与往返校验见 [`xai_text_codec`]。
//!
//! 对应的测试位于 `tests/transcoding_fs.rs`。

use std::{borrow::Cow, path::Path, sync::Arc};

use crate::computer::types::{AsyncFileSystem, ComputerError};

/// 包装任意 [`AsyncFileSystem`]，为其加上非 UTF-8 文本的透明编解码能力。
pub struct TranscodingFs {
    /// 被包装的底层文件系统，所有实际 I/O 都委托给它。
    inner: Arc<dyn AsyncFileSystem>,
}

impl TranscodingFs {
    /// 包装一个底层文件系统。
    pub fn new(inner: Arc<dyn AsyncFileSystem>) -> Self {
        Self { inner }
    }

    /// 取路径的小写扩展名，供二进制判定使用。
    ///
    /// `is_binary` 内部对扩展名表用二分查找，表中全为小写，因此必须归一化。
    fn extension_of(path: &Path) -> String {
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    /// 判定 `raw` 是否为「应当转码的非 UTF-8 文本」，是则返回 UTF-8 字节。
    ///
    /// 返回 `None` 表示按原样透传 —— 包括合法 UTF-8、二进制内容，以及任何无法可靠
    /// 判定编码的情况。
    fn decode_to_utf8(path: &Path, raw: &[u8]) -> Option<Vec<u8>> {
        // 快速路径：合法 UTF-8 无需任何处理，直接透传避免多余拷贝。
        // 现代项目的绝大多数文件在此返回，探测逻辑一次都不会执行。
        if std::str::from_utf8(raw).is_ok() {
            return None;
        }

        // 二进制保护：图片/PDF/可执行文件等绝不能进入编码探测。
        if crate::util::binary::is_binary(&Self::extension_of(path), raw) {
            return None;
        }

        match xai_text_codec::decode(raw) {
            // 探测成功且确实是非 UTF-8 编码，转成 UTF-8 交给工具层。
            Ok(decoded) if decoded.encoding != encoding_rs::UTF_8 => {
                tracing::debug!(
                    ?path,
                    encoding = decoded.encoding.name(),
                    "读取时将非 UTF-8 文本转码为 UTF-8"
                );
                Some(decoded.text.into_bytes())
            }
            // 其余情况一律透传，维持改造前的行为。
            _ => None,
        }
    }

    /// 决定实际写入磁盘的字节：若目标文件原本是非 UTF-8 文本，则按其原编码写回。
    ///
    /// 返回 `Cow::Borrowed` 表示无需转码，避免在常见路径上产生拷贝。
    async fn encode_for_target<'a>(
        &self,
        path: &Path,
        data: &'a [u8],
    ) -> Result<Cow<'a, [u8]>, ComputerError> {
        // 快速路径 1：纯 ASCII 在本 crate 支持的所有编码中字节表示一致，
        // 无需读取目标文件即可直接写入。
        if data.is_ascii() {
            return Ok(Cow::Borrowed(data));
        }

        // 快速路径 2：待写内容不是合法 UTF-8，说明调用方在写二进制，原样透传。
        let Ok(text) = std::str::from_utf8(data) else {
            return Ok(Cow::Borrowed(data));
        };

        // 读不到目标文件（通常是新建）时一律按 UTF-8 写入。
        let Ok(existing) = self.inner.read_file(path).await else {
            return Ok(Cow::Borrowed(data));
        };

        // 快速路径 3：目标文件已是合法 UTF-8，或本身是二进制，都无需转码。
        if std::str::from_utf8(&existing).is_ok()
            || crate::util::binary::is_binary(&Self::extension_of(path), &existing)
        {
            return Ok(Cow::Borrowed(data));
        }

        let Ok(decoded) = xai_text_codec::decode(&existing) else {
            // 无法可靠判定原编码时按 UTF-8 写入，与改造前行为一致。
            return Ok(Cow::Borrowed(data));
        };
        if decoded.encoding == encoding_rs::UTF_8 {
            return Ok(Cow::Borrowed(data));
        }

        // 按原编码写回。遇到原编码无法表示的字符时 GBK 会自动升级 GB18030；
        // 其余编码则报错中止写入 —— 宁可让本次编辑失败，也绝不静默损坏文件。
        match xai_text_codec::encode_or_upgrade(text, decoded.encoding, decoded.had_bom) {
            Ok(bytes) => {
                tracing::debug!(
                    ?path,
                    encoding = decoded.encoding.name(),
                    "写回时按原编码转码"
                );
                Ok(Cow::Owned(bytes))
            }
            Err(err) => Err(ComputerError::io(format!(
                "无法按原编码 {} 写回 {}：{err}。请先将文件转换为 UTF-8 再编辑。",
                decoded.encoding.name(),
                path.display()
            ))),
        }
    }
}

#[async_trait::async_trait]
impl AsyncFileSystem for TranscodingFs {
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, ComputerError> {
        let raw = self.inner.read_file(path).await?;
        Ok(Self::decode_to_utf8(path, &raw).unwrap_or(raw))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), ComputerError> {
        let bytes = self.encode_for_target(path, data).await?;
        self.inner.write_file(path, &bytes).await
    }

    async fn delete_file(&self, path: &Path) -> Result<(), ComputerError> {
        self.inner.delete_file(path).await
    }

    async fn file_exists(&self, path: &Path) -> Result<bool, ComputerError> {
        self.inner.file_exists(path).await
    }
}
