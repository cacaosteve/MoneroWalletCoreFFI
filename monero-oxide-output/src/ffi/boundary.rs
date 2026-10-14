//! Rust panics must not unwind across C/Swift/JNI. Fail closed without recovering a
//! potentially inconsistent poisoned wallet store; a process restart is required.

#[cfg(panic = "abort")]
compile_error!("WalletCore FFI requires panic=unwind so native failures can be returned safely");

pub(crate) trait FailureValue {
    fn failure() -> Self;
}
impl FailureValue for std::os::raw::c_int {
    fn failure() -> Self {
        -19
    }
}
impl<T> FailureValue for *mut T {
    fn failure() -> Self {
        std::ptr::null_mut()
    }
}

pub(crate) fn guard<T: FailureValue>(name: &str, operation: impl FnOnce() -> T) -> T {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(operation)) {
        Ok(value) => value,
        Err(_) => {
            // Do not expose panic payloads, which may contain native wallet data.
            let _ = std::panic::catch_unwind(|| {
                crate::record_error(
                    -19,
                    format!("{name}: native operation failed; restart the app before retrying"),
                )
            });
            T::failure()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panic_returns_an_error_and_sanitized_message() {
        assert_eq!(
            guard("fixture", || -> i32 { panic!("synthetic private payload") }),
            -19
        );
        let error = crate::last_error_clone().unwrap();
        assert!(error.contains("restart the app"));
        assert!(!error.contains("private payload"));
        let result: *mut u8 = guard("fixture", || panic!("synthetic pointer failure"));
        assert!(result.is_null());
        assert_eq!(guard("fixture", || 7i32), 7);
    }
    #[test]
    fn poisoned_store_is_not_recovered_or_allowed_to_mutate() {
        let store = std::sync::Mutex::new(7);
        let _ = std::panic::catch_unwind(|| {
            let _held = store.lock().unwrap();
            panic!("fixture");
        });
        assert!(store.is_poisoned());
        assert_eq!(
            guard("fixture", || {
                *store.lock().expect("poisoned") = 99;
                0i32
            }),
            -19
        );
        assert!(store.is_poisoned());
        assert_eq!(**store.lock().unwrap_err().get_ref(), 7);
    }

    #[test]
    fn actual_ffi_poison_child() {
        if std::env::var_os("WALLETCORE_POISON_CHILD").is_none() {
            return;
        }
        // Only the isolated child poisons the real registry; other tests stay independent.
        let _ = std::panic::catch_unwind(|| {
            let _store = crate::WALLET_STORE.lock().unwrap();
            panic!("synthetic registry failure");
        });
        let id = std::ffi::CString::new("fixture").unwrap();
        let mut total = 0;
        let mut unlocked = 0;
        assert_eq!(
            crate::wallet_get_balance(id.as_ptr(), &mut total, &mut unlocked),
            -19
        );
        assert!(crate::ffi::transfers::wallet_list_transfers_json(id.as_ptr()).is_null());
        assert!(crate::last_error_clone()
            .unwrap()
            .contains("restart the app"));
        assert_eq!(crate::wallet_seal(id.as_ptr(), 1), -19);
    }

    #[test]
    fn actual_ffi_entries_return_errors_instead_of_aborting_the_process() {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["ffi::boundary::tests::actual_ffi_poison_child", "--exact"])
            .env("WALLETCORE_POISON_CHILD", "1")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
