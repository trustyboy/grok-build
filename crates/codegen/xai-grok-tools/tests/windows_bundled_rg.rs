//! 验证 Windows 打包的 ripgrep 能经过完整性校验、解压并实际启动。
#![cfg(all(windows, bundle_rg))]

/// 运行真实子进程，防止遗漏扩展名或错误嵌入压缩资源后仅路径检查通过。
#[test]
fn bundled_ripgrep_runs_on_windows() {
    let path = xai_grok_tools::implementations::grok_build::grep::ripgrep::rg_path()
        .expect("打包的 ripgrep 应通过完整性校验");
    assert!(path.is_absolute(), "必须使用打包产物，不能退回 PATH");
    assert_eq!(path.extension().and_then(|s| s.to_str()), Some("exe"));
    let output = std::process::Command::new(path)
        .arg("--version")
        .output()
        .expect("Windows 应能启动解压后的 ripgrep");
    assert!(output.status.success(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("ripgrep "));
}
