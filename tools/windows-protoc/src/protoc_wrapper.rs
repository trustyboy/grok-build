//! Windows `protoc` compatibility wrapper for Grok Build.
//!
//! `xai-proto-build` requests Unix device paths while collecting dependency
//! metadata. This wrapper maps those two paths to temporary Windows files,
//! delegates every other argument to the official compiler, and restores the
//! dependency output format expected by the upstream Rust code.

use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

const DEPENDENCY_STDOUT_ARGUMENT: &str = "--dependency_out=/dev/stdout";
const DESCRIPTOR_NULL_ARGUMENT: &str = "--descriptor_set_out=/dev/null";

/// 保存改写后的参数以及需要模拟的 Unix 输出目标。
struct RewrittenArguments {
    values: Vec<OsString>,
    dependency_output: Option<PathBuf>,
}

/// 返回与 Wrapper 位于同一目录的官方 Windows `protoc.exe`。
fn real_protoc_path() -> io::Result<PathBuf> {
    let wrapper = env::current_exe()?;
    let directory = wrapper
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "无法确定 Wrapper 所在目录"))?;
    Ok(directory.join("protoc.exe"))
}

/// 为一次调用创建不会与并行 Cargo 构建冲突的临时目录路径。
fn invocation_directory() -> io::Result<PathBuf> {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let directory = env::temp_dir().join(format!(
        "grok-protoc-wrapper-{}-{timestamp}",
        std::process::id()
    ));
    fs::create_dir_all(&directory)?;
    Ok(directory)
}

/// 将上游使用的两个 Unix 设备路径改写为本次调用的 Windows 临时文件。
fn rewrite_arguments(arguments: impl Iterator<Item = OsString>, temp: &Path) -> RewrittenArguments {
    let dependency_output = temp.join("dependencies.d");
    let descriptor_output = temp.join("descriptor.pb");
    let mut uses_dependency_stdout = false;

    let values = arguments
        .map(|argument| {
            if argument == DEPENDENCY_STDOUT_ARGUMENT {
                uses_dependency_stdout = true;
                OsString::from(format!("--dependency_out={}", dependency_output.display()))
            } else if argument == DESCRIPTOR_NULL_ARGUMENT {
                OsString::from(format!(
                    "--descriptor_set_out={}",
                    descriptor_output.display()
                ))
            } else {
                argument
            }
        })
        .collect();

    RewrittenArguments {
        values,
        dependency_output: uses_dependency_stdout.then_some(dependency_output),
    }
}

/// 把 Windows dependency 文件的目标部分规范成上游解析器要求的 `/dev/null:`。
fn normalize_dependency_output(contents: &str) -> io::Result<String> {
    let separator = contents
        .find(": ")
        .or_else(|| contents.find(":\r\n"))
        .or_else(|| contents.find(":\n"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "protoc dependency 输出中缺少目标分隔符",
            )
        })?;

    Ok(format!("/dev/null{}", &contents[separator..]))
}

/// 调用官方编译器，并在需要时把 dependency 文件回写到标准输出。
fn run() -> io::Result<i32> {
    let protoc = real_protoc_path()?;
    if !protoc.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("找不到官方 protoc：{}", protoc.display()),
        ));
    }

    let temp = invocation_directory()?;
    let rewritten = rewrite_arguments(env::args_os().skip(1), &temp);
    let status = Command::new(&protoc).args(&rewritten.values).status();

    let result = match status {
        Ok(status) if status.success() => {
            if let Some(path) = rewritten.dependency_output {
                let contents = fs::read_to_string(path)?;
                let normalized = normalize_dependency_output(&contents)?;
                io::stdout().write_all(normalized.as_bytes())?;
            }
            Ok(0)
        }
        Ok(status) => Ok(status.code().unwrap_or(1)),
        Err(error) => Err(error),
    };

    // 这里只清理单次调用产生的临时文件；runtime 下的持久工具不会被删除。
    let _ = fs::remove_dir_all(temp);
    result
}

/// 输出清晰错误并把底层进程退出码传递给 Cargo。
fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(1)),
        Err(error) => {
            eprintln!("protoc-wrapper: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::normalize_dependency_output;

    /// 验证普通 Windows 盘符不会被误认为 dependency 目标分隔符。
    #[test]
    fn normalizes_windows_dependency_target() {
        let input = "C:\\temp\\descriptor.pb: crates/api.proto \\\r\n crates/common.proto\r\n";
        let output = normalize_dependency_output(input).expect("dependency output should parse");

        assert_eq!(
            output,
            "/dev/null: crates/api.proto \\\r\n crates/common.proto\r\n"
        );
    }
}
