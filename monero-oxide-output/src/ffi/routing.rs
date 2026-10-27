use std::{
    ffi::CStr,
    os::raw::{c_char, c_int},
};

/// Configure the I2P HTTP proxy, not a process-wide proxy for clearnet clients.
/// Each client snapshots its route. NULL/empty disables I2P; missing/invalid
/// settings always prevent new I2P requests rather than opening a direct socket.
///
/// # Safety
/// A non-null `proxy` must point to a readable NUL-terminated UTF-8 C string.
#[no_mangle]
pub unsafe extern "C" fn wallet_set_i2p_http_proxy(proxy: *const c_char) -> c_int {
    super::boundary::guard("wallet_set_i2p_http_proxy", || {
        let value = if proxy.is_null() {
            None
        } else {
            match unsafe { CStr::from_ptr(proxy) }.to_str() {
                Ok(value) => Some(value),
                Err(_) => {
                    let _ = crate::support::routing::configure_i2p_proxy(Some("://invalid"));
                    return crate::record_error(-11, "Invalid proxy UTF-8");
                }
            }
        };
        match crate::support::routing::configure_i2p_proxy(value) {
            Ok(()) => 0,
            Err(message) => crate::record_error(-14, message),
        }
    })
}
