//! `TranscodingFs` 的端到端行为验证。
//!
//! 放在集成测试而非 `#[cfg(test)] mod tests` 里有两个原因：
//! 一是装饰器全部走公开 API，无需访问私有项；二是 `xai-grok-tools` 的 lib test
//! 在 Windows 上因 `computer::local::terminal` 中既有的 `#[cfg(unix)]` 标注缺失
//! 而无法编译，集成测试可以绕开该问题，让本地也能真实跑通验证。

use std::{path::PathBuf, sync::Arc};

use xai_grok_tools::computer::{
    local::LocalFs, transcoding_fs::TranscodingFs, types::AsyncFileSystem,
};

/// 构造一个包装本地文件系统的装饰器与临时目录。
fn fixture() -> (TranscodingFs, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("创建临时目录");
    (TranscodingFs::new(Arc::new(LocalFs)), dir)
}

/// 用指定编码写入原始字节，模拟磁盘上已存在的非 UTF-8 文件。
fn write_raw(
    dir: &tempfile::TempDir,
    name: &str,
    text: &str,
    enc: &'static encoding_rs::Encoding,
) -> PathBuf {
    let (bytes, actual, had_unmappable) = enc.encode(text);
    assert_eq!(actual, enc, "fixture 编码与请求不符");
    assert!(!had_unmappable, "fixture 含不可编码字符");
    let path = dir.path().join(name);
    std::fs::write(&path, &bytes).expect("写入 fixture");
    path
}

/// 足够长的中文样本。
///
/// 长度必须让非 ASCII 字节数落在 `xai-text-codec` 的「强样本」区间（≥64 字节），
/// 否则探测会改用系统 ANSI 代码页作为先验，测试结果就会随运行机器的区域设置变化
/// （中文 Windows 判 GBK、韩文 Windows 判 EUC-KR、英文 Linux 无先验）。
/// 先验路径本身由 `xai-text-codec` 的 `weak_sample_*` 单元测试覆盖。
const SAMPLE: &str = "中文注释：这是一个用于验证编码探测的示例文件，内容足够长。\
                      再补一段中文，确保非 ASCII 字节数落在强样本区间，避免依赖系统区域设置。";

#[tokio::test]
async fn reads_gbk_file_as_utf8() {
    let (fs, dir) = fixture();
    let path = write_raw(&dir, "gbk.rs", SAMPLE, encoding_rs::GBK);

    let bytes = fs.read_file(&path).await.expect("读取应成功");

    assert_eq!(
        String::from_utf8(bytes).expect("应为合法 UTF-8"),
        SAMPLE,
        "GBK 文件应被解码为正确的 UTF-8 文本"
    );
}

#[tokio::test]
async fn utf8_file_passes_through_unchanged() {
    let (fs, dir) = fixture();
    let text = "你好世界 hello";
    let path = dir.path().join("utf8.txt");
    std::fs::write(&path, text).unwrap();

    let bytes = fs.read_file(&path).await.unwrap();

    assert_eq!(bytes, text.as_bytes(), "UTF-8 文件应原样返回");
}

#[tokio::test]
async fn binary_file_passes_through_unchanged() {
    let (fs, dir) = fixture();
    // PNG 魔数 + 二进制负载，绝不能被当作文本转码。
    let raw: Vec<u8> = vec![
        0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44,
        0x52, 0xC4, 0xE3, 0xBA, 0xC3,
    ];
    let path = dir.path().join("image.png");
    std::fs::write(&path, &raw).unwrap();

    let bytes = fs.read_file(&path).await.unwrap();

    assert_eq!(bytes, raw, "二进制文件必须原样返回");
}

#[tokio::test]
async fn edit_preserves_gbk_encoding_on_disk() {
    let (fs, dir) = fixture();
    let path = write_raw(&dir, "gbk.rs", SAMPLE, encoding_rs::GBK);

    // 模拟一次典型编辑：读出 UTF-8 文本，改一处，再整体写回。
    let text = String::from_utf8(fs.read_file(&path).await.unwrap()).unwrap();
    let edited = text.replace("示例文件", "测试文件");
    fs.write_file(&path, edited.as_bytes()).await.unwrap();

    // 磁盘上必须仍是 GBK 字节，而不是 UTF-8。
    let on_disk = std::fs::read(&path).unwrap();
    let expected = encoding_rs::GBK.encode(&edited).0.into_owned();
    assert_eq!(on_disk, expected, "写回后应保持 GBK 编码");
    // 严格解码（不允许替换字符）必须成功，证明磁盘内容仍是合法 GBK 且无损。
    assert_eq!(
        encoding_rs::GBK
            .decode_without_bom_handling_and_without_replacement(&on_disk)
            .expect("磁盘内容应能按 GBK 严格解码"),
        edited,
        "磁盘内容应能无损还原为编辑后的文本"
    );
}

#[tokio::test]
async fn untouched_regions_keep_original_bytes() {
    let (fs, dir) = fixture();
    let path = write_raw(&dir, "gbk.rs", SAMPLE, encoding_rs::GBK);
    let before = std::fs::read(&path).unwrap();

    // 只在末尾追加 ASCII，原有中文区域的字节必须逐字节不变 ——
    // 这正是改造前被破坏得最彻底的场景。
    let text = String::from_utf8(fs.read_file(&path).await.unwrap()).unwrap();
    fs.write_file(&path, format!("{text}// ok").as_bytes())
        .await
        .unwrap();

    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        &after[..before.len()],
        &before[..],
        "未编辑区域的字节必须完全不变"
    );
}

#[tokio::test]
async fn new_file_is_written_as_utf8() {
    let (fs, dir) = fixture();
    let path = dir.path().join("new.txt");
    let text = "全新文件的中文内容";

    fs.write_file(&path, text.as_bytes()).await.unwrap();

    assert_eq!(
        std::fs::read(&path).unwrap(),
        text.as_bytes(),
        "新建文件应为 UTF-8"
    );
}

#[tokio::test]
async fn utf8_file_edit_stays_utf8() {
    let (fs, dir) = fixture();
    let path = dir.path().join("utf8.txt");
    std::fs::write(&path, "原有中文内容").unwrap();

    let updated = "更新后的中文内容";
    fs.write_file(&path, updated.as_bytes()).await.unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), updated.as_bytes());
}

#[tokio::test]
async fn gbk_file_accepts_emoji_via_gb18030_upgrade() {
    let (fs, dir) = fixture();
    let path = write_raw(&dir, "gbk.rs", SAMPLE, encoding_rs::GBK);

    // GBK 表示不了 emoji，应自动升级 GB18030 而不是报错或写坏。
    let edited = format!("{SAMPLE}😀");
    fs.write_file(&path, edited.as_bytes()).await.unwrap();

    let on_disk = std::fs::read(&path).unwrap();
    assert_eq!(
        encoding_rs::GB18030
            .decode_without_bom_handling_and_without_replacement(&on_disk)
            .expect("应能按 GB18030 解码"),
        edited
    );
}

#[tokio::test]
async fn binary_write_passes_through() {
    let (fs, dir) = fixture();
    let path = dir.path().join("blob.bin");
    let raw: Vec<u8> = vec![0x00, 0x01, 0xFF, 0xFE, 0x89, 0x50];

    fs.write_file(&path, &raw).await.unwrap();

    assert_eq!(std::fs::read(&path).unwrap(), raw, "二进制写入应原样落盘");
}

#[tokio::test]
async fn big5_file_roundtrips() {
    let (fs, dir) = fixture();
    // 同样需要足够长以落入强样本区间，理由见 SAMPLE 的说明。
    let text = "繁體中文測試檔案，這段文字用來驗證編碼偵測是否正確運作。".repeat(4);
    let path = write_raw(&dir, "big5.txt", &text, encoding_rs::BIG5);

    let decoded = String::from_utf8(fs.read_file(&path).await.unwrap()).unwrap();
    assert_eq!(decoded, text, "Big5 应被正确解码");

    fs.write_file(&path, decoded.as_bytes()).await.unwrap();
    assert_eq!(
        std::fs::read(&path).unwrap(),
        encoding_rs::BIG5.encode(&text).0.into_owned(),
        "Big5 应按原编码写回"
    );
}
