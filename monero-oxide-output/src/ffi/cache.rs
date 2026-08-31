/*! Cache import/export FFI surface extracted from the historical mega-`lib.rs`.

This module mirrors the previous inlined behavior as closely as possible while using
`crate::support` for shared globals/helpers.

Exposes:
- `wallet_import_cache`
- `wallet_export_cache`
*/

#![allow(clippy::needless_return)]
#![allow(clippy::too_many_arguments)]

use crate::support::*;

use core::ffi::{c_char, c_int};
use core::{ptr, slice};
use std::ffi::CStr;

// Cache compatibility version.
// Bump this when the persisted cache format OR the semantics of persisted fields change
// in a way that makes old caches unsafe to import (e.g. key image derivation changes).
const WALLETCORE_CACHE_VERSION: u32 = 3;

#[no_mangle]
pub extern "C" fn wallet_import_cache(
    wallet_id: *const c_char,
    cache_ptr: *const u8,
    cache_len: usize,
) -> c_int {
    clear_last_error();

    if wallet_id.is_null() || cache_ptr.is_null() || cache_len == 0 {
        return record_error(-11, "wallet_import_cache: invalid arguments");
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => return record_error(-10, "wallet_import_cache: invalid wallet_id utf8"),
    };

    let data = unsafe { slice::from_raw_parts(cache_ptr, cache_len) };

    let persisted: PersistedWallet = match bincode::deserialize(data) {
        Ok(p) => p,
        Err(err) => {
            return record_error(
                -16,
                format!("wallet_import_cache: deserialize failed ({err})"),
            )
        }
    };

    // Cache compatibility gate.
    //
    // We persist key images inside `tracked_outputs`. If the derivation logic ever changes,
    // importing old caches becomes unsafe and can lead to `key_image_mismatch` quarantine spirals
    // and confusing send failures. Reject incompatible blobs so the app can delete the file
    // and rebuild via refresh/rescan.
    if persisted.cache_version != WALLETCORE_CACHE_VERSION {
        return record_error(
            -16,
            format!(
                "wallet_import_cache: incompatible cache version (have {}, want {})",
                persisted.cache_version, WALLETCORE_CACHE_VERSION
            ),
        );
    }

    match crate::ffi::refresh::with_refresh_stopped(id, || {
        let mut map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get_mut(id) {
            Some(state) => {
                if let Err(message) = cache_identity_matches(&persisted, state) {
                    return record_error(-16, message);
                }

                // Apply persisted snapshot onto in-memory state.
                persisted.apply_to_state(state);

                // Rebuild from tracked outputs on every import. Previous incoming-only rebuild
                // overwrote spend rows with change-as-receive and ignored outgoing net amounts.
                let known_fees = known_transaction_fees(&state.tx_ledger);
                state.tx_ledger = rebuild_transfer_ledger(
                    &state.tracked_outputs,
                    &state.pending_outgoing,
                    &known_fees,
                    state.chain_time,
                    &state.block_timestamps,
                );

                // Invariant enforcement:
                // Cache blobs may have been exported mid-refresh (or from older versions), which can result in
                // tracked outputs/ledger being present while total/unlocked are stale (e.g., 0).
                // Recompute balances from currently unspent tracked outputs using the imported chain height/time.
                let mut total: u64 = 0;
                let mut unlocked: u64 = 0;
                for o in state.tracked_outputs.iter() {
                    if o.spent {
                        continue;
                    }
                    total = total.saturating_add(o.amount);
                    if o.is_unlocked(state.chain_height, state.chain_time) {
                        unlocked = unlocked.saturating_add(o.amount);
                    }
                }
                state.total = total;
                state.unlocked = unlocked;

                clear_last_error();
                0
            }
            None => record_error(
                -13,
                format!("wallet_import_cache: wallet '{id}' not opened"),
            ),
        }
    }) {
        Ok(rc) => rc,
        Err(()) => record_error(
            -31,
            format!("wallet_import_cache: refresh already running for wallet '{id}'"),
        ),
    }
}

#[no_mangle]
pub extern "C" fn wallet_export_cache(
    wallet_id: *const c_char,
    out_buf: *mut u8,
    out_buf_len: usize,
    out_written: *mut usize,
) -> c_int {
    clear_last_error();

    if wallet_id.is_null() {
        if !out_written.is_null() {
            unsafe { *out_written = 0 };
        }
        return record_error(-11, "wallet_export_cache: invalid wallet_id");
    }

    if out_buf.is_null() && out_buf_len > 0 {
        if !out_written.is_null() {
            unsafe { *out_written = 0 };
        }
        return record_error(
            -11,
            "wallet_export_cache: null output buffer with non-zero length",
        );
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => return record_error(-10, "wallet_export_cache: invalid wallet_id utf8"),
    };

    let map = WALLET_STORE.lock().expect("wallet store poisoned");
    let state = match map.get(id) {
        Some(state) => state,
        None => {
            return record_error(
                -13,
                format!("wallet_export_cache: wallet '{id}' not opened"),
            );
        }
    };

    let persisted = PersistedWallet::from(state);

    let bytes = match bincode::serialize(&persisted) {
        Ok(b) => b,
        Err(err) => {
            return record_error(
                -16,
                format!("wallet_export_cache: serialize failed ({err})"),
            )
        }
    };

    // Size-query mode: caller provides null buffer to query required size.
    if out_buf.is_null() {
        if !out_written.is_null() {
            unsafe { *out_written = bytes.len() };
        }
        return -12;
    }

    // Buffer too small.
    if bytes.len() > out_buf_len {
        if !out_written.is_null() {
            unsafe { *out_written = bytes.len() };
        }
        return record_error(
            -12,
            format!(
                "wallet_export_cache: buffer too small (need {}, have {})",
                bytes.len(),
                out_buf_len
            ),
        );
    }

    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf, bytes.len());
        if !out_written.is_null() {
            *out_written = bytes.len();
        }
    }

    clear_last_error();
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::support::WALLET_STORE;
    use crate::PersistedNetwork;
    use std::ffi::CString;

    const TEST_MNEMONIC: &str =
        "ability pockets lordship tomorrow gypsy match neutral uncle avatar \
        betting bicycle junk unzip pyramid lynx mammal edgy empty uneven knowledge juvenile wiring \
        paradise psychic betting";

    fn open_wallet(id: &str) {
        let id = CString::new(id).unwrap();
        let mnemonic = CString::new(TEST_MNEMONIC).unwrap();
        assert_eq!(
            crate::wallet_open_from_mnemonic(id.as_ptr(), mnemonic.as_ptr(), 100, 1),
            0
        );
    }

    fn export_bytes(id: &str) -> Vec<u8> {
        let id = CString::new(id).unwrap();
        let mut written = 0usize;
        // Size probe returns -12 with the required length in `written`.
        let probe = wallet_export_cache(id.as_ptr(), std::ptr::null_mut(), 0, &mut written);
        assert_eq!(probe, -12);
        assert!(written > 0);
        let mut buf = vec![0u8; written];
        written = 0;
        assert_eq!(
            wallet_export_cache(id.as_ptr(), buf.as_mut_ptr(), buf.len(), &mut written),
            0
        );
        buf.truncate(written);
        buf
    }

    #[test]
    fn export_binds_primary_address_and_same_wallet_import_succeeds() {
        let id = "cache-bind-same";
        open_wallet(id);
        let bytes = export_bytes(id);
        let persisted: PersistedWallet = bincode::deserialize(&bytes).expect("decode");
        assert_eq!(persisted.cache_version, WALLETCORE_CACHE_VERSION);
        assert!(!persisted.bound_primary_address.is_empty());
        {
            let map = WALLET_STORE.lock().unwrap();
            let state = map.get(id).unwrap();
            assert_eq!(
                persisted.bound_primary_address,
                crate::wallet_cache_binding(state)
            );
        }
        let id_c = CString::new(id).unwrap();
        assert_eq!(
            wallet_import_cache(id_c.as_ptr(), bytes.as_ptr(), bytes.len()),
            0
        );
        WALLET_STORE.lock().unwrap().remove(id);
    }

    #[test]
    fn import_rejects_foreign_wallet_cache() {
        let id = "cache-bind-foreign";
        open_wallet(id);
        let bytes = export_bytes(id);
        let mut persisted: PersistedWallet = bincode::deserialize(&bytes).unwrap();
        persisted.bound_primary_address =
            "4AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
                .into();
        let tampered = bincode::serialize(&persisted).unwrap();
        let id_c = CString::new(id).unwrap();
        assert_ne!(
            wallet_import_cache(id_c.as_ptr(), tampered.as_ptr(), tampered.len()),
            0
        );
        WALLET_STORE.lock().unwrap().remove(id);
    }

    #[test]
    fn import_rejects_network_mismatch() {
        let id = "cache-bind-network";
        open_wallet(id);
        let bytes = export_bytes(id);
        let mut persisted: PersistedWallet = bincode::deserialize(&bytes).unwrap();
        // Keep address (would still fail address check if we changed network-derived address),
        // force enum mismatch to exercise the network gate independently.
        persisted.network = match persisted.network {
            PersistedNetwork::Mainnet => PersistedNetwork::Stagenet,
            PersistedNetwork::Stagenet => PersistedNetwork::Mainnet,
        };
        let tampered = bincode::serialize(&persisted).unwrap();
        let id_c = CString::new(id).unwrap();
        assert_ne!(
            wallet_import_cache(id_c.as_ptr(), tampered.as_ptr(), tampered.len()),
            0
        );
        WALLET_STORE.lock().unwrap().remove(id);
    }

    #[test]
    fn import_rejects_missing_binding_even_at_current_version() {
        let id = "cache-bind-missing";
        open_wallet(id);
        let bytes = export_bytes(id);
        let mut persisted: PersistedWallet = bincode::deserialize(&bytes).unwrap();
        persisted.bound_primary_address.clear();
        let tampered = bincode::serialize(&persisted).unwrap();
        let id_c = CString::new(id).unwrap();
        assert_ne!(
            wallet_import_cache(id_c.as_ptr(), tampered.as_ptr(), tampered.len()),
            0
        );
        WALLET_STORE.lock().unwrap().remove(id);
    }
}
