/*! Transfer listing/export FFI surface.

This module is extracted from the historical mega-`lib.rs` and keeps behavior identical.

Exposes:
- `wallet_export_outputs_json`
- `wallet_list_transfers_json`
*/

#![allow(clippy::needless_return)]

use crate::support::*;

use core::ffi::c_char;
use core::ptr;
use std::ffi::{CStr, CString};

#[no_mangle]
pub extern "C" fn wallet_export_outputs_json(wallet_id: *const c_char) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() {
        record_error(-11, "wallet_export_outputs_json: invalid wallet_id");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_export_outputs_json: invalid wallet_id utf8");
            return ptr::null_mut();
        }
    };

    let envelope = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        let Some(state) = map.get(id) else {
            record_error(
                -13,
                format!("wallet_export_outputs_json: wallet '{id}' not opened"),
            );
            return ptr::null_mut();
        };

        let outputs = state
            .tracked_outputs
            .iter()
            .map(|o| ObservedOutput::from_tracked(o, state.chain_height, state.chain_time))
            .collect();

        ObservedOutputsEnvelope {
            wallet_id: id.to_string(),
            restore_height: state.restore_height,
            last_scanned_height: state.last_scanned,
            chain_height: state.chain_height,
            chain_time: state.chain_time,
            outputs,
        }
    };

    let json = match serde_json::to_string(&envelope) {
        Ok(json) => json,
        Err(err) => {
            record_error(
                -16,
                format!("wallet_export_outputs_json: serialization failed ({err})"),
            );
            return ptr::null_mut();
        }
    };

    match CString::new(json) {
        Ok(cstr) => {
            clear_last_error();
            cstr.into_raw()
        }
        Err(_) => {
            record_error(
                -16,
                "wallet_export_outputs_json: JSON contained interior null bytes",
            );
            ptr::null_mut()
        }
    }
}

#[no_mangle]
pub extern "C" fn wallet_list_transfers_json(wallet_id: *const c_char) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() {
        record_error(-11, "wallet_list_transfers_json: invalid wallet_id");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_list_transfers_json: invalid wallet_id utf8");
            return ptr::null_mut();
        }
    };

    let transfers: Vec<ObservedTransfer> = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        let Some(state) = map.get(id) else {
            record_error(
                -13,
                format!("wallet_list_transfers_json: wallet '{id}' not opened"),
            );
            return ptr::null_mut();
        };

        // Build rows from the persisted transfer ledger so history remains stable even after outputs are spent.
        let mut rows: Vec<ObservedTransfer> = Vec::new();

        for entry in state.tx_ledger.values() {
            let height = entry.height.unwrap_or(0);
            let confirmations = if entry.is_pending {
                0
            } else {
                confirmations_for_height(state.chain_height, height)
            };

            rows.push(ObservedTransfer {
                txid: entry.txid.clone(),
                direction: entry.direction.clone(),
                amount: entry.amount,
                fee: entry.fee,
                height: entry.height,
                timestamp: entry.timestamp,
                confirmations,
                is_pending: entry.is_pending,
                subaddress_major: None,
                subaddress_minor: None,
            });
        }

        // Sort: pending first (newest first), then confirmed by height desc.
        rows.sort_by(|a, b| match (a.is_pending, b.is_pending) {
            (true, false) => core::cmp::Ordering::Less,
            (false, true) => core::cmp::Ordering::Greater,
            _ => {
                let ah = a.height.unwrap_or(0);
                let bh = b.height.unwrap_or(0);
                bh.cmp(&ah)
                    .then_with(|| b.timestamp.unwrap_or(0).cmp(&a.timestamp.unwrap_or(0)))
            }
        });

        rows
    };

    let json = match serde_json::to_string(&transfers) {
        Ok(s) => s,
        Err(err) => {
            record_error(
                -16,
                format!("wallet_list_transfers_json: serialization failed ({err})"),
            );
            return ptr::null_mut();
        }
    };

    match CString::new(json) {
        Ok(cstr) => {
            clear_last_error();
            cstr.into_raw()
        }
        Err(_) => {
            record_error(
                -16,
                "wallet_list_transfers_json: JSON contained interior null bytes",
            );
            ptr::null_mut()
        }
    }
}
