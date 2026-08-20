//! 非 UTF-8 文本文件的编码探测与无损转码。
//!
//! # 为什么需要这个 crate
//!
//! 工具链此前统一用 `String::from_utf8_lossy` 读文件、`as_bytes()` 写回。对 GBK 等
//! 非 UTF-8 文件，这会把每个非法字节替换成 U+FFFD，再以 UTF-8 全量覆盖写回 ——
//! 原始字节不可逆丢失，一次小编辑就足以摧毁整个文件的中文。
//!
//! 本 crate 提供「探测 → 解码 → 按原编码写回」的能力，使非 UTF-8 文件的读写往返无损。
//!
//! # 安全优先的设计原则
//!
//! 编码探测天然不可靠：GBK / Big5 / Shift_JIS / EUC-KR 的字节范围高度重叠，而
//! `chardetng` 1.0 并不提供置信度数值。误判的后果比不处理更糟 —— 不是显眼的 `�`，
//! 而是伪装成正常中文的错字。
//!
//! 因此这里对每个候选编码都要求三重判据全部通过，否则跳过；全部候选都不通过就返回
//! `Err`，由调用方原样透传字节（退化到改造前的行为，绝不会让现有场景变坏）：
//!
//! 1. 落在 [`TRANSCODABLE`] 白名单内；
//! 2. 能被该编码**严格解码**（无替换字符）；
//! 3. **往返校验**通过 —— 解码再编码能逐字节还原原文。
//!
//! 第 3 条是最重要的闸门：它保证了「凡是允许转码的文件，写回时必然能还原」，把探测
//! 正确性问题转化成了一个可验证的等式。
//!
//! # 往返校验的局限，以及为什么需要 `preferred`
//!
//! 往返校验只能排除「字节序列非法」的候选，**无法区分多个都合法的编码**。例如字节
//! `b9 fe` 在 GBK 是「哈」、在 EUC-KR 是「벗」，两者都能严格解码且完美往返。当文件里
//! 非 ASCII 字节很少时（真实案例：4677 字节的 Delphi 源码只有 8 个非 ASCII 字节），
//! `chardetng` 的选择近乎随机，曾把中文注释判成韩文。
//!
//! 为此引入 [`preferred_fallback`]：非 ASCII 字节数低于 [`SUFFICIENT_NON_ASCII`] 时，
//! 优先采用用户配置或系统 ANSI 代码页对应的编码 —— 遗留文件通常正是用保存者的系统
//! 默认编码写成的，这个先验比盲信探测器可靠得多。样本充分时仍然信任 `chardetng`。

use encoding_rs::Encoding;

/// 允许自动转码的传统编码白名单。
///
/// 只收录实际会遇到的 CJK 传统编码。刻意**不含** UTF-16/UTF-32：WHATWG 规范规定
/// 它们的 encoder 映射到 UTF-8，`Encoding::encode` 无法还原原始字节，写回必然损坏。
static TRANSCODABLE: &[&Encoding] = &[
    encoding_rs::GBK,
    encoding_rs::GB18030,
    encoding_rs::BIG5,
    encoding_rs::SHIFT_JIS,
    encoding_rs::EUC_JP,
    encoding_rs::EUC_KR,
];

/// 探测的文件大小上限。超过此值直接放弃，交由调用方透传。
///
/// 编辑类工具的正常输入远小于此值；对超大文件做全文往返校验得不偿失。
const MAX_DETECT_BYTES: usize = 8 * 1024 * 1024;

/// 喂给 `chardetng` 的采样上限。
///
/// 探测只需统计特征，头部若干 KB 已足够；严格解码与往返校验仍对**全文**执行，
/// 正确性不受采样影响。
const DETECT_SAMPLE_LIMIT: usize = 64 * 1024;

/// 启动探测所需的最小字节数。
///
/// 过短的非 ASCII 样本没有足够统计特征，探测必然是瞎猜 —— 宁可拒绝。
const MIN_DETECT_BYTES: usize = 16;

/// 认为 `chardetng` 的猜测「有统计依据」所需的非 ASCII 字节数。
///
/// 低于此值时多个 CJK 编码往往都能合法解码且往返一致（例如 `b9 fe` 在 GBK 是「哈」、
/// 在 EUC-KR 是「벗」，两者都合法），`chardetng` 的选择近乎随机。此时改用用户偏好或
/// 系统代码页作为先验，比盲信探测器可靠得多 —— 遗留文件通常正是用保存者的系统默认
/// 编码写成的。
const SUFFICIENT_NON_ASCII: usize = 64;

/// UTF-8 BOM 字节序列。
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// 一次成功解码的结果，同时携带按原样写回所需的全部信息。
#[derive(Debug, Clone)]
pub struct DecodedText {
    /// 解码后的文本。不含任何 U+FFFD 替换字符。
    pub text: String,
    /// 原文件编码，写回时用它还原。
    pub encoding: &'static Encoding,
    /// 原文件是否带 BOM，写回时需要一并还原。
    pub had_bom: bool,
}

/// 编解码失败的原因。
///
/// 调用方收到任何变体都应退化为「原样透传字节」，而不是尝试补救。
#[derive(Debug, Clone, thiserror::Error)]
pub enum CodecError {
    /// 无法可靠判定编码。附带具体原因便于排查。
    #[error("无法可靠判定文件编码：{reason}")]
    UnknownEncoding {
        /// 判定失败的具体环节。
        reason: &'static str,
    },
    /// 目标编码无法表示文本中的某个字符。
    #[error("编码 {encoding} 无法表示字符 {ch:?}")]
    Unmappable {
        /// 目标编码名称。
        encoding: &'static str,
        /// 第一个无法表示的字符。
        ch: char,
    },
}

/// 把字节解码为文本，并判定其原始编码。使用 [`preferred_fallback`] 作为弱样本先验。
///
/// 分支按成本从低到高排列，绝大多数文件在前三步返回，不会触碰探测逻辑。
///
/// 返回 `Err` 表示「无法安全处理」，调用方应原样透传字节。
pub fn decode(bytes: &[u8]) -> Result<DecodedText, CodecError> {
    decode_with(bytes, preferred_fallback())
}

/// 与 [`decode`] 相同，但显式指定弱样本时优先采用的编码。
///
/// `preferred` 只在「多个编码都能合法解码且往返一致」且样本不足以支撑统计判断时生效，
/// 不会覆盖有充分依据的探测结果。
pub fn decode_with(
    bytes: &[u8],
    preferred: Option<&'static Encoding>,
) -> Result<DecodedText, CodecError> {
    // 快速路径 1：空文件与纯 ASCII 都是 UTF-8 的子集，无需任何探测。
    if bytes.is_empty() || bytes.is_ascii() {
        return Ok(DecodedText {
            // ASCII 必然是合法 UTF-8，这里不会失败。
            text: String::from_utf8(bytes.to_vec())
                .unwrap_or_else(|_| unreachable!("ASCII 必然是合法 UTF-8")),
            encoding: encoding_rs::UTF_8,
            had_bom: false,
        });
    }

    // 快速路径 2：BOM 是最可靠的编码标记，优先于一切内容启发。
    if let Some((bom_encoding, bom_len)) = Encoding::for_bom(bytes) {
        // 只接受 UTF-8 BOM。UTF-16/32 的 encoder 在 WHATWG 规范下映射到 UTF-8，
        // 无法按原编码写回，只能交给调用方透传。
        if bom_encoding == encoding_rs::UTF_8
            && let Some(text) =
                bom_encoding.decode_without_bom_handling_and_without_replacement(&bytes[bom_len..])
        {
            return Ok(DecodedText {
                text: text.into_owned(),
                encoding: encoding_rs::UTF_8,
                had_bom: true,
            });
        }
        return Err(CodecError::UnknownEncoding {
            reason: "带非 UTF-8 的 BOM，该编码无法按原样写回",
        });
    }

    // 快速路径 3：严格 UTF-8 校验（标准库为此做了 SIMD 优化）。
    // 现代项目的绝大多数文件在此命中，探测逻辑一次都不会执行。
    if let Ok(text) = std::str::from_utf8(bytes) {
        return Ok(DecodedText {
            text: text.to_owned(),
            encoding: encoding_rs::UTF_8,
            had_bom: false,
        });
    }

    // 走到这里才是真正的候选：非 ASCII 且非合法 UTF-8 的字节流。
    detect_legacy(bytes, preferred)
}

/// 对确认非 UTF-8 的字节流做传统编码探测。
///
/// 每一步失败都返回 `Err` 而非退而求其次 —— 猜错的代价是不可逆的数据损坏。
fn detect_legacy(
    bytes: &[u8],
    preferred: Option<&'static Encoding>,
) -> Result<DecodedText, CodecError> {
    if bytes.len() < MIN_DETECT_BYTES {
        return Err(CodecError::UnknownEncoding {
            reason: "样本过短，不足以可靠探测",
        });
    }
    if bytes.len() > MAX_DETECT_BYTES {
        return Err(CodecError::UnknownEncoding {
            reason: "文件过大，跳过探测",
        });
    }
    // NUL 字节是二进制内容的可靠标志：任何传统文本编码都不会在正常文本里产生它。
    // 这是本模块自带的纵深防御 —— 传统编码的字节覆盖率很高（PNG 头部就能被 GBK
    // 「成功」解码并往返一致），不能只依赖调用方的二进制判定。
    if bytes.contains(&0) {
        return Err(CodecError::UnknownEncoding {
            reason: "含 NUL 字节，判定为二进制内容",
        });
    }

    // 探测只喂采样即可；ISO-2022-JP 是有状态转义编码，转码风险高，直接排除。
    let sample = &bytes[..bytes.len().min(DETECT_SAMPLE_LIMIT)];
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    detector.feed(sample, sample.len() == bytes.len());
    // UTF-8 已在上游被严格排除，这里禁止它作为猜测结果。
    let guess = detector.guess(None, chardetng::Utf8Detection::Deny);

    // 非 ASCII 字节的数量决定了探测器的判断有多少统计依据可依。
    let non_ascii = bytes.iter().filter(|b| **b > 127).count();
    let guess_is_grounded = non_ascii >= SUFFICIENT_NON_ASCII;

    // 依优先级挑选：样本充分时信任探测器；否则先用偏好编码，最后才回退到探测结果。
    // 每个候选都必须通过「白名单 + 严格解码 + 往返校验」，任何一项不过即跳过。
    let candidates: [Option<&'static Encoding>; 3] = if guess_is_grounded {
        [Some(guess), preferred, None]
    } else {
        [preferred, Some(guess), None]
    };

    for candidate in candidates.into_iter().flatten() {
        if let Some(text) = verify_candidate(bytes, candidate) {
            return Ok(DecodedText {
                text,
                encoding: candidate,
                had_bom: false,
            });
        }
    }

    Err(CodecError::UnknownEncoding {
        reason: "没有候选编码能通过白名单与往返校验",
    })
}

/// 校验单个候选编码能否无损承载 `bytes`，可以则返回解码后的文本。
///
/// 三道关卡缺一不可：
/// 1. 必须在 [`TRANSCODABLE`] 白名单内；
/// 2. 严格解码（不允许替换字符）成功；
/// 3. **往返校验** —— 解码再编码必须逐字节还原原文。
///
/// 第 3 条保证了「凡是允许转码的文件，写回时必然能还原」，把探测正确性问题转化成
/// 了一个可验证的等式。注意它只能排除「字节序列非法」的候选，无法区分多个都合法的
/// 编码 —— 那正是 `preferred` 存在的意义。
fn verify_candidate(bytes: &[u8], encoding: &'static Encoding) -> Option<String> {
    if !TRANSCODABLE.contains(&encoding) {
        return None;
    }
    let text = encoding
        .decode_without_bom_handling_and_without_replacement(bytes)?
        .into_owned();
    let (re_encoded, _, had_unmappable) = encoding.encode(&text);
    (!had_unmappable && re_encoded.as_ref() == bytes).then_some(text)
}

/// 弱样本时优先采用的编码。
///
/// 优先级：`GROK_FILE_ENCODING` 环境变量 > 系统 ANSI 代码页。
///
/// 结果会被缓存 —— 两者在进程生命周期内都不会变化。
pub fn preferred_fallback() -> Option<&'static Encoding> {
    static CACHED: std::sync::OnceLock<Option<&'static Encoding>> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        if let Ok(label) = std::env::var("GROK_FILE_ENCODING")
            && let Some(encoding) = Encoding::for_label(label.trim().as_bytes())
        {
            return Some(encoding);
        }
        system_encoding()
    })
}

/// Windows 的系统 ANSI 代码页对应的编码。
///
/// 遗留文本文件通常正是以该编码保存的，因此它是弱样本下最合理的先验：
/// 936→GBK、949→EUC-KR、950→Big5、932→Shift_JIS。UTF-8（65001）无需回退。
#[cfg(windows)]
fn system_encoding() -> Option<&'static Encoding> {
    // 直接声明 FFI，避免为一个调用引入 windows crate 的 Globalization feature。
    unsafe extern "system" {
        fn GetACP() -> u32;
    }
    let label: &[u8] = match unsafe { GetACP() } {
        936 => b"gbk",
        949 => b"euc-kr",
        950 => b"big5",
        932 => b"shift_jis",
        _ => return None,
    };
    Encoding::for_label(label)
}

/// 类 Unix 系统从 locale 环境变量推断编码。
///
/// 现代发行版几乎都是 UTF-8（此时无需回退，返回 `None`），此路径主要覆盖仍在使用
/// 传统 locale（如 `zh_CN.GBK`）的环境。
#[cfg(not(windows))]
fn system_encoding() -> Option<&'static Encoding> {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())?;
    let charset = locale.split('.').nth(1)?;
    let encoding = Encoding::for_label(charset.as_bytes())?;
    // UTF-8 locale 不构成传统编码先验。
    (encoding != encoding_rs::UTF_8).then_some(encoding)
}

/// 按指定编码把文本编码回字节，必要时还原 BOM。
///
/// 若目标编码无法表示文本中的某个字符，返回 [`CodecError::Unmappable`] 而**不是**
/// 静默替换 —— `encoding_rs` 默认会把不可映射字符写成 `&#128512;` 这类 HTML 数字
/// 引用，那会让这种字符串莫名混进源码文件。
pub fn encode(
    text: &str,
    encoding: &'static Encoding,
    had_bom: bool,
) -> Result<Vec<u8>, CodecError> {
    let (encoded, actual_encoding, had_unmappable) = encoding.encode(text);

    // encoding_rs 对不支持编码的字符集（UTF-16 等）会静默改用 UTF-8，
    // 那样写回的字节与原编码不符，必须拦下。
    if actual_encoding != encoding {
        return Err(CodecError::UnknownEncoding {
            reason: "该编码不支持编码回写",
        });
    }
    if had_unmappable {
        return Err(CodecError::Unmappable {
            encoding: encoding.name(),
            ch: first_unmappable(text, encoding).unwrap_or(char::REPLACEMENT_CHARACTER),
        });
    }

    // BOM 只对 UTF-8 还原：其余编码本就不该出现在白名单里。
    if had_bom && encoding == encoding_rs::UTF_8 {
        let mut out = Vec::with_capacity(UTF8_BOM.len() + encoded.len());
        out.extend_from_slice(&UTF8_BOM);
        out.extend_from_slice(&encoded);
        return Ok(out);
    }
    Ok(encoded.into_owned())
}

/// 与 [`encode`] 相同，但在 GBK 遇到不可表示字符时自动升级为 GB18030。
///
/// GB18030 是 GBK 的**字节兼容超集**且覆盖全部 Unicode：原有中文内容的字节完全不变，
/// 仅新增字符使用扩展序列。这让「模型在 GBK 文件里插入了 emoji」这类常见情况得以
/// 无损写回，而不必让整次编辑失败。
///
/// 其余编码（Big5、Shift_JIS 等）没有字节兼容的升级路径，仍返回 `Err`，
/// 由调用方决定是报错还是放弃写入 —— 绝不静默损坏。
pub fn encode_or_upgrade(
    text: &str,
    encoding: &'static Encoding,
    had_bom: bool,
) -> Result<Vec<u8>, CodecError> {
    match encode(text, encoding, had_bom) {
        Err(CodecError::Unmappable { .. }) if encoding == encoding_rs::GBK => {
            encode(text, encoding_rs::GB18030, had_bom)
        }
        other => other,
    }
}

/// 找出文本中第一个无法被目标编码表示的字符，仅用于生成可读的错误信息。
///
/// 只在错误路径上调用，逐字符试编码的开销无关紧要。
fn first_unmappable(text: &str, encoding: &'static Encoding) -> Option<char> {
    text.chars().find(|ch| {
        let mut buf = [0u8; 4];
        let (_, _, had_unmappable) = encoding.encode(ch.encode_utf8(&mut buf));
        had_unmappable
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用指定编码构造测试字节，避免在仓库里存放会被 Git 改写的二进制 fixture。
    fn to_bytes(text: &str, encoding: &'static Encoding) -> Vec<u8> {
        let (bytes, actual, had_unmappable) = encoding.encode(text);
        assert_eq!(actual, encoding, "fixture 编码与请求不符");
        assert!(!had_unmappable, "fixture 含不可编码字符");
        bytes.into_owned()
    }

    /// 核心不变量：解码再编码必须逐字节还原原文。
    fn assert_roundtrip(bytes: &[u8], expected_encoding: &'static Encoding) {
        let decoded = decode(bytes).expect("应能解码");
        assert_eq!(decoded.encoding, expected_encoding);
        assert!(
            !decoded.text.contains(char::REPLACEMENT_CHARACTER),
            "解码结果不应含替换字符"
        );
        let re_encoded =
            encode_or_upgrade(&decoded.text, decoded.encoding, decoded.had_bom).expect("应能编码");
        assert_eq!(re_encoded, bytes, "往返后字节不一致");
    }

    #[test]
    fn empty_and_ascii_take_fast_path() {
        for raw in [&b""[..], b"hello world", b"fn main() {}\n"] {
            let decoded = decode(raw).expect("ASCII 应被接受");
            assert_eq!(decoded.encoding, encoding_rs::UTF_8);
            assert!(!decoded.had_bom);
            assert_eq!(decoded.text.as_bytes(), raw);
        }
    }

    #[test]
    fn utf8_roundtrips() {
        assert_roundtrip("你好世界 hello".as_bytes(), encoding_rs::UTF_8);
    }

    #[test]
    fn utf8_bom_is_detected_and_preserved() {
        let mut raw = UTF8_BOM.to_vec();
        raw.extend_from_slice("你好世界".as_bytes());

        let decoded = decode(&raw).expect("应识别 UTF-8 BOM");
        assert!(decoded.had_bom, "应记录 BOM");
        assert_eq!(decoded.text, "你好世界", "解码结果不应包含 BOM");
        assert_roundtrip(&raw, encoding_rs::UTF_8);
    }

    #[test]
    fn gbk_is_detected_and_roundtrips() {
        // 需要足够长度才能给探测器提供统计特征。
        let raw = to_bytes(
            "中文注释：这是一个用于验证编码探测的示例文件，内容足够长。",
            encoding_rs::GBK,
        );
        // GBK 与 GB18030 共享解码表，探测器可能报告任一者，两者字节在此范围内等价。
        let decoded = decode(&raw).expect("应能解码 GBK");
        assert!(
            decoded.encoding == encoding_rs::GBK || decoded.encoding == encoding_rs::GB18030,
            "预期 GBK 系编码，实际 {}",
            decoded.encoding.name()
        );
        assert!(decoded.text.contains("中文注释"));
        let re_encoded =
            encode_or_upgrade(&decoded.text, decoded.encoding, decoded.had_bom).unwrap();
        assert_eq!(re_encoded, raw, "GBK 往返后字节不一致");
    }

    #[test]
    fn big5_is_detected_and_roundtrips() {
        // 样本需足够长才落入「强样本」区间，由探测器自行判定；
        // 中文字数偏少的 Big5 文件会走先验路径，那条路径由 weak_sample_* 系列覆盖。
        let raw = to_bytes(
            &"繁體中文測試檔案，這段文字用來驗證編碼偵測是否正確運作。".repeat(4),
            encoding_rs::BIG5,
        );
        assert_roundtrip(&raw, encoding_rs::BIG5);
    }

    #[test]
    fn short_non_ascii_sample_is_rejected() {
        // 4 字节的 GBK 内容不足以支撑可靠探测，必须拒绝而非瞎猜。
        let raw = to_bytes("中文", encoding_rs::GBK);
        assert!(raw.len() < MIN_DETECT_BYTES);
        assert!(matches!(
            decode(&raw),
            Err(CodecError::UnknownEncoding { .. })
        ));
    }

    #[test]
    fn binary_like_bytes_are_rejected() {
        // PNG magic + 随机二进制：不应被当成任何文本编码。
        let raw = [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x01, 0x00,
        ];
        assert!(decode(&raw).is_err(), "二进制内容不应被转码");
    }

    #[test]
    fn utf16_bom_is_rejected_because_it_cannot_be_written_back() {
        // UTF-16LE BOM + "中"。encoding_rs 的 UTF-16 encoder 映射到 UTF-8，
        // 无法还原原始字节，因此必须拒绝而不是半途转码。
        let raw = [0xFF, 0xFE, 0x2D, 0x4E];
        assert!(matches!(
            decode(&raw),
            Err(CodecError::UnknownEncoding { .. })
        ));
    }

    #[test]
    fn gbk_upgrades_to_gb18030_for_unmappable_chars() {
        let text = "中文内容";
        let gbk_bytes = to_bytes(text, encoding_rs::GBK);

        // GBK 表示不了 emoji，直接 encode 必须报错而非静默产生 HTML 数字引用。
        let with_emoji = format!("{text}😀");
        let err = encode(&with_emoji, encoding_rs::GBK, false).unwrap_err();
        assert!(matches!(err, CodecError::Unmappable { .. }));

        // 升级到 GB18030 后应当成功，且原有中文字节保持不变（字节兼容超集）。
        let upgraded = encode_or_upgrade(&with_emoji, encoding_rs::GBK, false).unwrap();
        assert!(
            upgraded.starts_with(&gbk_bytes),
            "GB18030 应保持原 GBK 字节前缀不变"
        );
        assert_eq!(
            encoding_rs::GB18030
                .decode_without_bom_handling_and_without_replacement(&upgraded)
                .unwrap(),
            with_emoji
        );
    }

    #[test]
    fn unmappable_error_reports_the_offending_char() {
        let err = encode("中文😀内容", encoding_rs::GBK, false).unwrap_err();
        match err {
            CodecError::Unmappable { ch, .. } => assert_eq!(ch, '😀'),
            other => panic!("预期 Unmappable，实际 {other:?}"),
        }
    }

    #[test]
    fn decoded_text_never_contains_replacement_char() {
        // 构造一段合法 GBK，确认解码结果里不会混入 U+FFFD（改造前的典型症状）。
        let raw = to_bytes(
            "这段文字专门用来确认解码结果中不会出现替换字符，长度足够触发探测。",
            encoding_rs::GBK,
        );
        let decoded = decode(&raw).expect("应能解码");
        assert!(!decoded.text.contains(char::REPLACEMENT_CHARACTER));
    }

    /// 复刻真实案例：4677 字节的 Delphi 源码中只有 8 个非 ASCII 字节（`b9 fe` ×4）。
    /// 该字节序列在 GBK 是「哈」、在 EUC-KR 是「벗」，两者都能严格解码且完美往返，
    /// `chardetng` 曾据此把中文注释判成韩文。
    fn ascii_heavy_sample() -> Vec<u8> {
        let mut raw = b"// application constants and helpers\n".repeat(20);
        raw.extend_from_slice(&[0xB9, 0xFE, 0xB9, 0xFE, 0xB9, 0xFE, 0xB9, 0xFE]);
        raw.extend_from_slice(b"\nfunction MakeSurroundRect(P1, P2: TPoint);\n");
        raw
    }

    #[test]
    fn weak_sample_follows_preferred_encoding() {
        let raw = ascii_heavy_sample();
        assert!(
            raw.iter().filter(|b| **b > 127).count() < SUFFICIENT_NON_ASCII,
            "该样本应落在弱样本区间"
        );

        // 中文环境：偏好 GBK，应解出「哈哈哈哈」。
        let zh = decode_with(&raw, Some(encoding_rs::GBK)).expect("应能解码");
        assert_eq!(zh.encoding, encoding_rs::GBK);
        assert!(zh.text.contains("哈哈哈哈"), "实际解出：{}", zh.text);

        // 韩文环境：偏好 EUC-KR，同样字节应解出「벗벗벗벗」——
        // 对该地区用户这才是正确结果，说明先验确实按环境生效。
        let ko = decode_with(&raw, Some(encoding_rs::EUC_KR)).expect("应能解码");
        assert_eq!(ko.encoding, encoding_rs::EUC_KR);
        assert!(ko.text.contains("벗벗벗벗"), "实际解出：{}", ko.text);
    }

    #[test]
    fn weak_sample_still_roundtrips_under_either_preference() {
        // 无论先验选了哪个编码，字节层面都必须无损 —— 这是不可让步的底线。
        let raw = ascii_heavy_sample();
        for pref in [encoding_rs::GBK, encoding_rs::EUC_KR] {
            let decoded = decode_with(&raw, Some(pref)).expect("应能解码");
            let re_encoded =
                encode_or_upgrade(&decoded.text, decoded.encoding, decoded.had_bom).unwrap();
            assert_eq!(re_encoded, raw, "{} 往返后字节不一致", pref.name());
        }
    }

    #[test]
    fn strong_sample_ignores_wrong_preference() {
        // 大量繁体中文 → chardetng 有充分依据判定 Big5，
        // 此时即便偏好设成 GBK 也不应被带偏。
        let text = "繁體中文測試檔案，這段文字用來驗證編碼偵測是否正確運作。".repeat(4);
        let raw = to_bytes(&text, encoding_rs::BIG5);
        assert!(
            raw.iter().filter(|b| **b > 127).count() >= SUFFICIENT_NON_ASCII,
            "该样本应落在强样本区间"
        );

        let decoded = decode_with(&raw, Some(encoding_rs::GBK)).expect("应能解码");
        assert_eq!(decoded.text, text, "强样本应由探测器判定，不受错误先验影响");
    }

    #[test]
    fn preference_is_ignored_when_it_cannot_decode() {
        // 偏好编码无法严格解码时必须跳过它，回退到探测结果，而不是失败或硬套。
        let text = "日本語のテストファイルです。この文章は文字コード判定の検証に使います。";
        let raw = to_bytes(text, encoding_rs::SHIFT_JIS);

        let decoded = decode_with(&raw, Some(encoding_rs::EUC_KR)).expect("应能解码");
        assert_eq!(decoded.text, text);
    }
}
