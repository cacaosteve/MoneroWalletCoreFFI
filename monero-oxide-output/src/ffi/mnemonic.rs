/// FFI: English mnemonic generation.
///
/// This module exposes a single C-ABI function which generates a fresh Monero
/// mnemonic (English) and writes it into a caller-provided buffer.
///
/// ABI shape follows the existing walletcore pattern:
/// - Return `0` on success
/// - Return negative error codes on failure
/// - Always NUL-terminate on success
/// - If the buffer is too small, zero the output buffer and return `-12`
///
/// Safety / security:
/// - Uses the `rand` crate's OS-backed RNG (`OsRng`) for entropy.
/// - Does not persist the mnemonic; callers are responsible for secure storage.
/// - Does not log or otherwise expose the mnemonic.
///
/// Notes:
/// - This file is intended to be re-exported from `src/lib.rs` and/or wired
///   through `src/ffi/mod.rs`.
/// - The mnemonic format is produced by the `monero-seed` crate.
///
/// Error codes (mirrors existing conventions):
/// - `-11` invalid argument (null pointers, etc.)
/// - `-12` output buffer too small
/// - `-20` mnemonic generation failed (unexpected internal error)
use std::{ffi::c_char, os::raw::c_int, ptr};

use monero_seed::{Language as MoneroSeedLanguage, Seed as MoneroSeed};
use rand::rngs::OsRng;

#[inline]
fn zero_outputs(out_buf: *mut c_char, out_buf_len: usize, out_written: *mut usize) {
    unsafe {
        if !out_buf.is_null() && out_buf_len > 0 {
            ptr::write_bytes(out_buf as *mut u8, 0, out_buf_len);
        }
        if !out_written.is_null() {
            *out_written = 0;
        }
    }
}

/// Generate a new English Monero mnemonic and write it to `out_buf`.
///
/// Parameters:
/// - `out_buf`: caller-provided buffer for the ASCII mnemonic string
/// - `out_buf_len`: size of `out_buf` in bytes
/// - `out_written`: optional out-parameter for bytes written (excluding NUL)
///
/// Returns:
/// - `0` on success
/// - negative error codes on failure
#[no_mangle]
pub extern "C" fn wallet_generate_mnemonic_english(
    out_buf: *mut c_char,
    out_buf_len: usize,
    out_written: *mut usize,
) -> c_int {
    if out_buf.is_null() || out_buf_len == 0 {
        return -11;
    }

    // `monero-seed` provides RNG-based generation. Use OS RNG.
    let mut rng = OsRng;
    let seed = MoneroSeed::new(&mut rng, MoneroSeedLanguage::English);
    let mnemonic = seed.to_string();

    let bytes = mnemonic.as_bytes();
    let needed = bytes.len();

    // Need room for NUL terminator.
    if out_buf_len <= needed {
        zero_outputs(out_buf, out_buf_len, out_written);
        return -12;
    }

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out_buf, needed);
        *out_buf.add(needed) = 0;
        if !out_written.is_null() {
            *out_written = needed;
        }
    }

    0
}
