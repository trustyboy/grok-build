//! 编码探测诊断工具：观察本 crate 在**真实文件**上的判定过程。
//!
//! 不设 `DIAG_FILE` 时自动跳过，因此不影响常规测试与 CI。
//!
//! 用法：
//! ```powershell
//! $env:DIAG_FILE = "R:\Temp\AppCommon.pas"
//! cargo test --release -p xai-text-codec --test diagnose -- --nocapture
//! ```
//!
//! 输出包含 `chardetng` 的原始猜测、各候选编码的严格解码与往返情况、当前生效的
//! 系统/配置先验，以及最终结论。排查「某个文件读出来是乱码」时从这里入手最快。

#[test]
fn diagnose() {
    let path = std::env::var("DIAG_FILE").unwrap_or_default();
    if path.is_empty() {
        eprintln!("跳过：未设置 DIAG_FILE");
        return;
    }
    let bytes = std::fs::read(&path).expect("读取文件");
    println!("文件: {path}");
    println!("大小: {} 字节", bytes.len());
    println!(
        "非 ASCII 字节数: {}",
        bytes.iter().filter(|b| **b > 127).count()
    );
    println!("是否合法 UTF-8: {}", std::str::from_utf8(&bytes).is_ok());

    // chardetng 原始猜测（不加任何白名单/校验）
    let mut det = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Deny);
    det.feed(&bytes, true);
    let raw_guess = det.guess(None, chardetng::Utf8Detection::Deny);
    println!("chardetng 猜测: {}", raw_guess.name());

    // 各候选编码的严格解码 + 往返情况
    for enc in [
        encoding_rs::GBK,
        encoding_rs::GB18030,
        encoding_rs::BIG5,
        encoding_rs::EUC_KR,
        encoding_rs::SHIFT_JIS,
    ] {
        let decoded = enc.decode_without_bom_handling_and_without_replacement(&bytes);
        let roundtrip = decoded
            .as_ref()
            .map(|t| enc.encode(t).0.as_ref() == bytes.as_slice());
        println!(
            "  {:<10} 严格解码={:<5} 往返一致={:?}",
            enc.name(),
            decoded.is_some(),
            roundtrip
        );
    }

    // 本 crate 的最终结论
    println!(
        "系统/配置先验: {:?}",
        xai_text_codec::preferred_fallback().map(|e| e.name())
    );
    match xai_text_codec::decode(&bytes) {
        Ok(d) => {
            println!("=> decode 成功，编码={}", d.encoding.name());
            // 打印非 ASCII 字符所在片段，直观确认解出的是不是正确文字
            if let Some(pos) = d.text.find(|c: char| !c.is_ascii()) {
                let snippet: String = d.text[pos..].chars().take(20).collect();
                println!("=> 非 ASCII 片段: {snippet}");
            }
        }
        Err(e) => println!("=> decode 拒绝：{e}"),
    }
}
