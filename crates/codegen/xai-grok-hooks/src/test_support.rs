//! This module is gated on `#[cfg(test)]` and is exported as `pub(crate)` so any in-crate `#[cfg(test)] mod tests` can use it.
//! Integration tests under `tests/` cannot reach it; for those, copy or re-implement the handful of functions here that they need.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

/// 返回当前平台真实存在的临时目录，避免原生 Windows 的测试在 `/tmp` 启动失败。
pub(crate) fn workspace_root() -> &'static str {
    static ROOT: std::sync::LazyLock<String> =
        std::sync::LazyLock::new(|| std::env::temp_dir().to_string_lossy().into_owned());
    ROOT.as_str()
}

/// 按实际测试 shell 选择脚本，保持 POSIX 与 PowerShell 下的断言行为一致。
pub(crate) fn shell_script(posix: &'static str, powershell: &'static str) -> &'static str {
    #[cfg(not(unix))]
    {
        use xai_grok_config::shell::{WindowsShell, detect_windows_shell};
        if matches!(
            detect_windows_shell(),
            WindowsShell::Pwsh | WindowsShell::PowerShell
        ) {
            return powershell;
        }
    }
    #[cfg(unix)]
    let _ = powershell;
    posix
}

/// Run `f` with the env var `name` set to `value` (or unset if `value` is `None`), restoring the previous value on return.
/// Process env vars are process-global, so callers should pick uniquely-named vars to avoid inter-test races; nothing here enforces that.
pub(crate) fn with_env_var<R>(name: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
    let previous = std::env::var_os(name);
    // SAFETY: env-var writes are not thread-safe. Callers use uniquely
    // named vars so no concurrent test races on the same name.
    unsafe {
        match value {
            Some(v) => std::env::set_var(name, v),
            None => std::env::remove_var(name),
        }
    }

    let result = catch_unwind(AssertUnwindSafe(f));

    // SAFETY: see above. Restore unconditionally so a panic doesn't
    // leak env state to subsequent tests.
    unsafe {
        match previous {
            Some(prev) => std::env::set_var(name, prev),
            None => std::env::remove_var(name),
        }
    }

    match result {
        Ok(value) => value,
        Err(payload) => resume_unwind(payload),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restores_previous_value_on_normal_return() {
        let key = "GROK_HOOKS_TEST_SUPPORT_RESTORE";
        with_env_var(key, Some("first"), || {
            with_env_var(key, Some("second"), || {
                assert_eq!(std::env::var(key).unwrap(), "second");
            });
            assert_eq!(std::env::var(key).unwrap(), "first");
        });
        assert!(std::env::var(key).is_err());
    }

    #[test]
    fn restores_previous_unset_state_on_normal_return() {
        let key = "GROK_HOOKS_TEST_SUPPORT_UNSET_RESTORE";
        // SAFETY: see module-level note.
        unsafe {
            std::env::remove_var(key);
        }
        with_env_var(key, Some("temporary"), || {
            assert_eq!(std::env::var(key).unwrap(), "temporary");
        });
        assert!(std::env::var(key).is_err());
    }

    #[test]
    fn restores_after_panic() {
        let key = "GROK_HOOKS_TEST_SUPPORT_PANIC_RESTORE";
        // SAFETY: see module-level note.
        unsafe {
            std::env::remove_var(key);
        }
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            with_env_var(key, Some("during-panic"), || {
                panic!("intentional");
            });
        }));
        assert!(panicked.is_err(), "expected panic to propagate");
        assert!(
            std::env::var(key).is_err(),
            "env var must be restored after panic"
        );
    }

    #[test]
    fn allows_explicit_unset() {
        let key = "GROK_HOOKS_TEST_SUPPORT_EXPLICIT_UNSET";
        // SAFETY: see module-level note.
        unsafe {
            std::env::set_var(key, "before");
        }
        with_env_var(key, None, || {
            assert!(std::env::var(key).is_err());
        });
        assert_eq!(std::env::var(key).unwrap(), "before");
        // SAFETY: see module-level note.
        unsafe {
            std::env::remove_var(key);
        }
    }
}
