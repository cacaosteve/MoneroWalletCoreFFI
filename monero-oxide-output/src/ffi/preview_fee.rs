//! Fee preview FFI surface extracted from the historical mega-`lib.rs`.
//!
//! This module keeps behavior identical to the inlined implementation, while relying on
//! `crate::support` for a stable, small set of re-exports.
//!
//! Exposes:
//! - `wallet_preview_fee`
//! - `wallet_preview_fee_with_filter`

#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_return)]

use crate::support::*;

use core::ffi::c_char;
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use std::{
    ffi::{CStr, CString},
    ptr,
};
use zeroize::Zeroizing;

// External types used by the preview path.
use monero_address::MoneroAddress;
use monero_interface::{FeeError, FeeRate};
use monero_wallet::Scanner;

#[no_mangle]
pub extern "C" fn wallet_preview_fee(
    wallet_id: *const c_char,
    node_url: *const c_char,
    destinations_json: *const c_char,
    ring_len: u8,
) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() || destinations_json.is_null() {
        record_error(-11, "wallet_preview_fee: null argument(s)");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_preview_fee: wallet_id contained invalid UTF-8");
            return ptr::null_mut();
        }
    };

    let dests_str = match unsafe { CStr::from_ptr(destinations_json) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_preview_fee: destinations_json invalid UTF-8");
            return ptr::null_mut();
        }
    };

    #[derive(Deserialize)]
    struct Pay {
        address: String,
        amount: u64,
    }

    let pays: Vec<Pay> = match serde_json::from_str(dests_str) {
        Ok(v) => v,
        Err(err) => {
            record_error(
                -11,
                format!("wallet_preview_fee: invalid destinations JSON ({err})"),
            );
            return ptr::null_mut();
        }
    };
    if pays.is_empty() {
        record_error(-11, "wallet_preview_fee: empty destinations");
        return ptr::null_mut();
    }

    let snapshot = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get(id) {
            Some(state) => state.clone(),
            None => {
                record_error(
                    -13,
                    format!("wallet_preview_fee: wallet '{id}' not registered"),
                );
                return ptr::null_mut();
            }
        }
    };

    let arg_url = if !node_url.is_null() {
        unsafe { CStr::from_ptr(node_url) }
            .to_str()
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    };
    let env_url = std::env::var("MONERO_URL").ok();
    let base_url = arg_url
        .filter(|s| !s.is_empty())
        .or(env_url)
        .unwrap_or_else(|| "http://127.0.0.1:18081".to_string());

    let rpc_client: RpcClient = match TOKIO_RUNTIME.block_on(
        monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()),
    ) {
        Ok(d) => d,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_preview_fee: failed to connect daemon '{base_url}': {e}"),
            );
            return ptr::null_mut();
        }
    };

    let daemon_height = match TOKIO_RUNTIME.block_on(rpc_client.latest_block_number()) {
        Ok(n) => n.saturating_add(1) as u64,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_preview_fee: failed to query daemon height '{base_url}': {e}"),
            );
            return ptr::null_mut();
        }
    };

    let daemon = DaemonStatus {
        height: daemon_height,
        top_block_timestamp: 0,
    };

    let master = match master_keys_from_mnemonic_str(&snapshot.mnemonic) {
        Ok(keys) => keys,
        Err(code) => {
            record_error(code, "wallet_preview_fee: unable to parse mnemonic");
            return ptr::null_mut();
        }
    };
    let view_pair = match master.to_view_pair() {
        Ok(pair) => pair,
        Err(code) => {
            record_error(code, "wallet_preview_fee: failed to construct view pair");
            return ptr::null_mut();
        }
    };

    let mut scanner = Scanner::new(view_pair.clone());
    let gap_limit = snapshot.gap_limit.max(1);
    if let Some(i0) = SubaddressIndex::new(0, 0) {
        scanner.register_subaddress(i0);
    }
    for minor in 1..=gap_limit {
        if let Some(index) = SubaddressIndex::new(0, minor) {
            scanner.register_subaddress(index);
        }
    }

    // Parse destinations into monero addresses.
    let mut destinations: Vec<(monero_address::MoneroAddress, u64)> =
        Vec::with_capacity(pays.len());
    let mut total_needed: u64 = 0;
    for p in &pays {
        let addr = match MoneroAddress::from_str(snapshot.network, &p.address) {
            Ok(a) => a,
            Err(_) => {
                record_error(-10, "wallet_preview_fee: invalid destination address");
                return ptr::null_mut();
            }
        };
        total_needed = total_needed.saturating_add(p.amount);
        destinations.push((addr, p.amount));
    }

    // Gather unlocked, unspent outputs (excluding quarantined).
    let mut spendable = snapshot
        .tracked_outputs
        .iter()
        .cloned()
        .filter(|o| !o.spent && o.is_unlocked(daemon.height, daemon.top_block_timestamp))
        .filter(|o| {
            !snapshot
                .invalid_input_quarantine
                .contains(&(o.tx_hash, o.index_in_tx))
        })
        .collect::<Vec<_>>();

    // Input selection order for preview fee: match send defaults (smallest_first unless overridden).
    match walletcore_input_select_mode() {
        InputSelectMode::SmallestFirst => spendable.sort_by_key(|o| o.amount),
        InputSelectMode::LargestFirst => spendable.sort_by(|a, b| b.amount.cmp(&a.amount)),
    }

    // Optional diagnostics: dump spendable universe for debugging.
    walletcore_debug_dump_tracked_outputs(
        id,
        snapshot.network,
        "preview_fee spendable_input_dump",
        &spendable,
        daemon.height,
        daemon.top_block_timestamp,
    );

    // Fetch fee rate once.
    let max_per_weight = fee_rate_max_per_weight_cap();
    let fee_priority = walletcore_fee_priority();
    let fee_rate: FeeRate =
        match TOKIO_RUNTIME.block_on(rpc_client.fee_rate(fee_priority, max_per_weight)) {
            Ok(fr) => fr,
            Err(e) => {
                let code = match e {
                    FeeError::InterfaceError(inner) => map_rpc_error(inner),
                    _ => -16,
                };
                record_error(code, "wallet_preview_fee: fee_rate failed");
                return ptr::null_mut();
            }
        };

    // Debug toggle: allow preview to run without fetching decoys (placeholder fee).
    if walletcore_disable_decoys() {
        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "🧪 WALLETCORE_DISABLE_DECOYS=1: preview_fee returning placeholder fee without decoy selection wallet_id={} base_url={}",
                id, base_url
            ),
        );

        let placeholder_fee: u64 = 30_700_000;
        let json = match serde_json::to_string(&serde_json::json!({ "fee": placeholder_fee })) {
            Ok(s) => s,
            Err(err) => {
                record_error(
                    -16,
                    format!("wallet_preview_fee: result JSON serialization failed ({err})"),
                );
                return ptr::null_mut();
            }
        };
        return match CString::new(json) {
            Ok(cstr) => {
                clear_last_error();
                cstr.into_raw()
            }
            Err(_) => {
                record_error(
                    -16,
                    "wallet_preview_fee: result JSON contained interior null bytes",
                );
                ptr::null_mut()
            }
        };
    }

    if walletcore_decoy_mode_bin16() {
        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "🧪 WALLETCORE_DECOY_MODE=bin16 enabled: preview_fee will use monero-daemon-rpc (bin_rpc) decoy provider wallet_id={} base_url={}",
                id, base_url
            ),
        );
    }

    let change = monero_wallet::send::Change::new(view_pair.clone(), None);

    // Iteratively select inputs until we can construct a tx covering amount+fee.
    let mut selected: Vec<TrackedOutput> = Vec::new();
    let mut selected_sum: u64 = 0;

    let max_selection_rounds: usize = 24;
    let mut last_needed_total: Option<u64> = None;

    let mut rng = OsRng;
    let ring_len_eff: u8 = if ring_len < 2 { 16 } else { ring_len };

    for round in 0..max_selection_rounds {
        if walletcore_debug_input_dump_enabled() {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧾 preview_fee selection_round wallet_id={} round={} selected_count={} selected_sum={} total_needed={}",
                    id,
                    round,
                    selected.len(),
                    selected_sum,
                    total_needed
                ),
            );
        }

        // First pass: select enough to cover destination totals.
        if selected.is_empty() {
            for o in &spendable {
                selected.push(o.clone());
                selected_sum = selected_sum.saturating_add(o.amount);
                if selected_sum >= total_needed {
                    break;
                }
            }
            if selected_sum < total_needed {
                record_error(
                    -18,
                    format!(
                        "wallet_preview_fee: insufficient unlocked funds (have {}, need {})",
                        selected_sum, total_needed
                    ),
                );
                return ptr::null_mut();
            }

            walletcore_debug_dump_tracked_outputs(
                id,
                snapshot.network,
                "preview_fee selected_input_dump (initial)",
                &selected,
                daemon.height,
                daemon.top_block_timestamp,
            );
        }

        // Build inputs with decoys for current selection.
        let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
        for t in &selected {
            let block_number = match usize::try_from(t.block_height) {
                Ok(value) => value,
                Err(_) => {
                    record_error(-16, "wallet_preview_fee: block number conversion overflow");
                    return ptr::null_mut();
                }
            };

            let scannable =
                match TOKIO_RUNTIME.block_on(rpc_client.scannable_block_by_number(block_number)) {
                    Ok(block) => block,
                    Err(err) => {
                        let code = map_rpc_error(err);
                        record_error(
                            code,
                            format!(
                                "wallet_preview_fee: RPC block fetch failed at height {}",
                                t.block_height
                            ),
                        );
                        return ptr::null_mut();
                    }
                };

            let outputs = match scanner.scan(scannable) {
                Ok(result) => result.ignore_additional_timelock(),
                Err(_) => {
                    record_error(
                        -16,
                        format!(
                            "wallet_preview_fee: scanner failed at height {}",
                            t.block_height
                        ),
                    );
                    return ptr::null_mut();
                }
            };

            let wallet_out = match outputs.into_iter().find(|wo| {
                wo.transaction() == t.tx_hash && wo.index_in_transaction() == t.index_in_tx
            }) {
                Some(wo) => wo,
                None => {
                    record_error(
                        -16,
                        "wallet_preview_fee: failed to reconstruct selected output",
                    );
                    return ptr::null_mut();
                }
            };

            let with_decoys = if walletcore_decoy_mode_bin16() {
                let ring_len_eff: u8 = 16;
                let daemon_iface = match TOKIO_RUNTIME.block_on(make_bin_decoy_daemon(&base_url)) {
                    Ok(d) => d,
                    Err(e) => {
                        let code = map_rpc_error(e.clone());
                        record_error(
                            code,
                            format!(
                                "wallet_preview_fee: failed to construct bin16 decoy daemon for '{base_url}': {e}"
                            ),
                        );
                        return ptr::null_mut();
                    }
                };

                match TOKIO_RUNTIME.block_on(monero_wallet::OutputWithDecoys::new(
                    &mut rng,
                    &daemon_iface,
                    ring_len_eff,
                    usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                    wallet_out,
                )) {
                    Ok(i) => i,
                    Err(err) => {
                        let code = match &err {
                            monero_interface::TransactionsError::InterfaceError(inner) => {
                                map_rpc_error(inner.clone())
                            }
                            monero_interface::TransactionsError::TransactionNotFound => -16,
                            monero_interface::TransactionsError::PrunedTransaction => -16,
                        };
                        record_error(
                            code,
                            format!("wallet_preview_fee: decoy selection failed ({err:?})"),
                        );
                        return ptr::null_mut();
                    }
                }
            } else {
                match TOKIO_RUNTIME.block_on(monero_wallet::OutputWithDecoys::new(
                    &mut rng,
                    &rpc_client,
                    ring_len_eff,
                    usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                    wallet_out,
                )) {
                    Ok(i) => i,
                    Err(err) => {
                        let code = match &err {
                            monero_interface::TransactionsError::InterfaceError(inner) => {
                                map_rpc_error(inner.clone())
                            }
                            monero_interface::TransactionsError::TransactionNotFound => -16,
                            monero_interface::TransactionsError::PrunedTransaction => -16,
                        };
                        record_error(
                            code,
                            format!("wallet_preview_fee: decoy selection failed ({err:?})"),
                        );
                        return ptr::null_mut();
                    }
                }
            };

            inputs.push(with_decoys);
        }

        // New OVK each attempt; should not affect fee, but keep it fresh.
        let mut ovk = [0u8; 32];
        rng.fill_bytes(&mut ovk);

        let intent = match monero_wallet::send::SignableTransaction::new(
            monero_wallet::ringct::RctType::ClsagBulletproofPlus,
            Zeroizing::new(ovk),
            inputs,
            destinations.clone(),
            change.clone(),
            Vec::new(),
            fee_rate,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not enough funds") {
                    // Recoverable: add one more input and retry.
                    let mut added_any = false;
                    for o in &spendable {
                        if selected
                            .iter()
                            .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
                        {
                            continue;
                        }
                        selected.push(o.clone());
                        selected_sum = selected_sum.saturating_add(o.amount);
                        added_any = true;
                        break;
                    }

                    if !added_any {
                        record_error(
                            -18,
                            format!(
                                "wallet_preview_fee: insufficient unlocked funds for amount+fee (have {}, need at least {})",
                                selected_sum, total_needed
                            ),
                        );
                        return ptr::null_mut();
                    }

                    continue;
                }

                record_error(
                    -16,
                    format!("wallet_preview_fee: transaction construction failed ({e})"),
                );
                return ptr::null_mut();
            }
        };

        let fee = intent.necessary_fee();
        let needed_total = total_needed.saturating_add(fee);

        if selected_sum >= needed_total {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "✅ wallet_preview_fee ok wallet_id={} inputs_selected={} inputs_sum={} total_needed={} fee={} needed_total={}",
                    id,
                    selected.len(),
                    selected_sum,
                    total_needed,
                    fee,
                    needed_total
                ),
            );

            let json = match serde_json::to_string(&serde_json::json!({ "fee": fee })) {
                Ok(s) => s,
                Err(err) => {
                    record_error(
                        -16,
                        format!("wallet_preview_fee: result JSON serialization failed ({err})"),
                    );
                    return ptr::null_mut();
                }
            };

            return match CString::new(json) {
                Ok(cstr) => {
                    clear_last_error();
                    cstr.into_raw()
                }
                Err(_) => {
                    record_error(
                        -16,
                        "wallet_preview_fee: result JSON contained interior null bytes",
                    );
                    ptr::null_mut()
                }
            };
        }

        if let Some(last) = last_needed_total {
            if needed_total <= last && round > 0 {
                // Not a hard error; just continue selecting more.
            }
        }
        last_needed_total = Some(needed_total);

        // Select more inputs until we cover the newly estimated required total, then loop.
        let mut added_any = false;
        for o in &spendable {
            if selected
                .iter()
                .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
            {
                continue;
            }
            selected.push(o.clone());
            selected_sum = selected_sum.saturating_add(o.amount);
            added_any = true;
            if selected_sum >= needed_total {
                break;
            }
        }

        if !added_any {
            record_error(
                -18,
                format!(
                    "wallet_preview_fee: insufficient unlocked funds for amount+fee (have {}, need {})",
                    selected_sum, needed_total
                ),
            );
            return ptr::null_mut();
        }
    }

    record_error(
        -16,
        "wallet_preview_fee: fee estimation did not converge (too many selection rounds)",
    );
    return ptr::null_mut();
}

#[no_mangle]
pub extern "C" fn wallet_preview_fee_with_filter(
    wallet_id: *const c_char,
    node_url: *const c_char,
    destinations_json: *const c_char,
    filter_json: *const c_char,
    ring_len: u8,
) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() || destinations_json.is_null() {
        record_error(-11, "wallet_preview_fee_with_filter: null argument(s)");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(
                -10,
                "wallet_preview_fee_with_filter: wallet_id contained invalid UTF-8",
            );
            return ptr::null_mut();
        }
    };

    let dests_str = match unsafe { CStr::from_ptr(destinations_json) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(
                -10,
                "wallet_preview_fee_with_filter: destinations_json invalid UTF-8",
            );
            return ptr::null_mut();
        }
    };

    let filt_str_opt = if !filter_json.is_null() {
        unsafe { CStr::from_ptr(filter_json) }.to_str().ok()
    } else {
        None
    };

    #[derive(Deserialize)]
    struct Pay {
        address: String,
        amount: u64,
    }
    #[derive(Deserialize)]
    struct InputFilter {
        subaddress_minor: Option<u32>,
    }

    let pays: Vec<Pay> = match serde_json::from_str(dests_str) {
        Ok(v) => v,
        Err(err) => {
            record_error(
                -11,
                format!("wallet_preview_fee_with_filter: invalid destinations JSON ({err})"),
            );
            return ptr::null_mut();
        }
    };
    if pays.is_empty() {
        record_error(-11, "wallet_preview_fee_with_filter: empty destinations");
        return ptr::null_mut();
    }

    let filter: Option<InputFilter> = match filt_str_opt {
        Some(s) if !s.is_empty() => match serde_json::from_str(s) {
            Ok(f) => Some(f),
            Err(err) => {
                record_error(
                    -11,
                    format!("wallet_preview_fee_with_filter: invalid filter JSON ({err})"),
                );
                return ptr::null_mut();
            }
        },
        _ => None,
    };

    // This FFI mirrors `wallet_preview_fee` behavior, except it restricts spendable inputs
    // to a given subaddress (account 0) when requested. Destinations are still arbitrary.
    //
    // For now, we implement it by parsing inputs exactly the same way and applying the filter
    // before the selection loop.
    //
    // NOTE: We intentionally do not share code between the two functions yet to keep the
    // extraction low-risk; later, we can refactor into shared helpers.

    let snapshot = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get(id) {
            Some(state) => state.clone(),
            None => {
                record_error(
                    -13,
                    format!("wallet_preview_fee_with_filter: wallet '{id}' not registered"),
                );
                return ptr::null_mut();
            }
        }
    };

    let arg_url = if !node_url.is_null() {
        unsafe { CStr::from_ptr(node_url) }
            .to_str()
            .ok()
            .map(|s| s.trim().to_string())
    } else {
        None
    };
    let env_url = std::env::var("MONERO_URL").ok();
    let base_url = arg_url
        .filter(|s| !s.is_empty())
        .or(env_url)
        .unwrap_or_else(|| "http://127.0.0.1:18081".to_string());

    let rpc_client: RpcClient = match TOKIO_RUNTIME.block_on(
        monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()),
    ) {
        Ok(d) => d,
        Err(e) => {
            record_error(
                -16,
                format!(
                    "wallet_preview_fee_with_filter: failed to connect daemon '{base_url}': {e}"
                ),
            );
            return ptr::null_mut();
        }
    };

    let daemon_height = match TOKIO_RUNTIME.block_on(rpc_client.latest_block_number()) {
        Ok(n) => n.saturating_add(1) as u64,
        Err(e) => {
            record_error(
                -16,
                format!(
                    "wallet_preview_fee_with_filter: failed to query daemon height '{base_url}': {e}"
                ),
            );
            return ptr::null_mut();
        }
    };

    let daemon = DaemonStatus {
        height: daemon_height,
        top_block_timestamp: 0,
    };

    let master = match master_keys_from_mnemonic_str(&snapshot.mnemonic) {
        Ok(keys) => keys,
        Err(code) => {
            record_error(
                code,
                "wallet_preview_fee_with_filter: unable to parse mnemonic",
            );
            return ptr::null_mut();
        }
    };
    let view_pair = match master.to_view_pair() {
        Ok(pair) => pair,
        Err(code) => {
            record_error(
                code,
                "wallet_preview_fee_with_filter: failed to construct view pair",
            );
            return ptr::null_mut();
        }
    };

    let mut scanner = Scanner::new(view_pair.clone());
    let gap_limit = snapshot.gap_limit.max(1);
    if let Some(i0) = SubaddressIndex::new(0, 0) {
        scanner.register_subaddress(i0);
    }
    for minor in 1..=gap_limit {
        if let Some(index) = SubaddressIndex::new(0, minor) {
            scanner.register_subaddress(index);
        }
    }

    // Parse destinations.
    let mut destinations: Vec<(monero_address::MoneroAddress, u64)> =
        Vec::with_capacity(pays.len());
    let mut total_needed: u64 = 0;
    for p in &pays {
        let addr = match MoneroAddress::from_str(snapshot.network, &p.address) {
            Ok(a) => a,
            Err(_) => {
                record_error(
                    -10,
                    "wallet_preview_fee_with_filter: invalid destination address",
                );
                return ptr::null_mut();
            }
        };
        total_needed = total_needed.saturating_add(p.amount);
        destinations.push((addr, p.amount));
    }

    // Gather spendable outputs (unspent + unlocked), excluding quarantined, then apply filter.
    let mut spendable: Vec<TrackedOutput> = snapshot
        .tracked_outputs
        .iter()
        .cloned()
        .filter(|o| !o.spent && o.is_unlocked(daemon.height, daemon.top_block_timestamp))
        .filter(|o| {
            !snapshot
                .invalid_input_quarantine
                .contains(&(o.tx_hash, o.index_in_tx))
        })
        .collect();

    if let Some(f) = &filter {
        if let Some(minor) = f.subaddress_minor {
            spendable.retain(|o| o.subaddress_major == 0 && o.subaddress_minor == minor);
        }
    }

    match walletcore_input_select_mode() {
        InputSelectMode::SmallestFirst => spendable.sort_by_key(|o| o.amount),
        InputSelectMode::LargestFirst => spendable.sort_by(|a, b| b.amount.cmp(&a.amount)),
    }

    walletcore_debug_dump_tracked_outputs(
        id,
        snapshot.network,
        "preview_fee_with_filter spendable_input_dump",
        &spendable,
        daemon.height,
        daemon.top_block_timestamp,
    );

    // Fee rate once.
    let max_per_weight = fee_rate_max_per_weight_cap();
    let fee_priority = walletcore_fee_priority();
    let fee_rate: FeeRate =
        match TOKIO_RUNTIME.block_on(rpc_client.fee_rate(fee_priority, max_per_weight)) {
            Ok(fr) => fr,
            Err(e) => {
                let code = match e {
                    FeeError::InterfaceError(inner) => map_rpc_error(inner),
                    _ => -16,
                };
                record_error(code, "wallet_preview_fee_with_filter: fee_rate failed");
                return ptr::null_mut();
            }
        };

    // Debug toggle: allow preview to run without fetching decoys (placeholder fee).
    if walletcore_disable_decoys() {
        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "🧪 WALLETCORE_DISABLE_DECOYS=1: preview_fee_with_filter returning placeholder fee without decoy selection wallet_id={} base_url={}",
                id, base_url
            ),
        );

        let placeholder_fee: u64 = 30_700_000;
        let json = match serde_json::to_string(&serde_json::json!({ "fee": placeholder_fee })) {
            Ok(s) => s,
            Err(err) => {
                record_error(
                    -16,
                    format!(
                        "wallet_preview_fee_with_filter: result JSON serialization failed ({err})"
                    ),
                );
                return ptr::null_mut();
            }
        };
        return match CString::new(json) {
            Ok(cstr) => {
                clear_last_error();
                cstr.into_raw()
            }
            Err(_) => {
                record_error(
                    -16,
                    "wallet_preview_fee_with_filter: result JSON contained interior null bytes",
                );
                ptr::null_mut()
            }
        };
    }

    if walletcore_decoy_mode_bin16() {
        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "🧪 WALLETCORE_DECOY_MODE=bin16 enabled: preview_fee_with_filter will use monero-daemon-rpc (bin_rpc) decoy provider wallet_id={} base_url={}",
                id, base_url
            ),
        );
    }

    let change = monero_wallet::send::Change::new(view_pair.clone(), None);

    let mut selected: Vec<TrackedOutput> = Vec::new();
    let mut selected_sum: u64 = 0;

    let max_selection_rounds: usize = 24;
    let mut last_needed_total: Option<u64> = None;

    let mut rng = OsRng;
    let ring_len_eff: u8 = if ring_len < 2 { 16 } else { ring_len };

    for round in 0..max_selection_rounds {
        if walletcore_debug_input_dump_enabled() {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧾 preview_fee_with_filter selection_round wallet_id={} round={} selected_count={} selected_sum={} total_needed={}",
                    id,
                    round,
                    selected.len(),
                    selected_sum,
                    total_needed
                ),
            );
        }

        if selected.is_empty() {
            for o in &spendable {
                selected.push(o.clone());
                selected_sum = selected_sum.saturating_add(o.amount);
                if selected_sum >= total_needed {
                    break;
                }
            }
            if selected_sum < total_needed {
                record_error(
                    -18,
                    format!(
                        "wallet_preview_fee_with_filter: insufficient unlocked funds (have {}, need {})",
                        selected_sum, total_needed
                    ),
                );
                return ptr::null_mut();
            }

            walletcore_debug_dump_tracked_outputs(
                id,
                snapshot.network,
                "preview_fee_with_filter selected_input_dump (initial)",
                &selected,
                daemon.height,
                daemon.top_block_timestamp,
            );
        }

        let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
        for t in &selected {
            let block_number = match usize::try_from(t.block_height) {
                Ok(value) => value,
                Err(_) => {
                    record_error(
                        -16,
                        "wallet_preview_fee_with_filter: block number conversion overflow",
                    );
                    return ptr::null_mut();
                }
            };

            let scannable = match TOKIO_RUNTIME
                .block_on(rpc_client.scannable_block_by_number(block_number))
            {
                Ok(block) => block,
                Err(err) => {
                    let code = map_rpc_error(err);
                    record_error(
                        code,
                        format!(
                            "wallet_preview_fee_with_filter: RPC block fetch failed at height {}",
                            t.block_height
                        ),
                    );
                    return ptr::null_mut();
                }
            };

            let outputs = match scanner.scan(scannable) {
                Ok(result) => result.ignore_additional_timelock(),
                Err(_) => {
                    record_error(
                        -16,
                        format!(
                            "wallet_preview_fee_with_filter: scanner failed at height {}",
                            t.block_height
                        ),
                    );
                    return ptr::null_mut();
                }
            };

            let wallet_out = match outputs.into_iter().find(|wo| {
                wo.transaction() == t.tx_hash && wo.index_in_transaction() == t.index_in_tx
            }) {
                Some(wo) => wo,
                None => {
                    record_error(
                        -16,
                        "wallet_preview_fee_with_filter: failed to reconstruct selected output",
                    );
                    return ptr::null_mut();
                }
            };

            let with_decoys = if walletcore_decoy_mode_bin16() {
                let ring_len_eff: u8 = 16;
                let daemon_iface = match TOKIO_RUNTIME.block_on(make_bin_decoy_daemon(&base_url)) {
                    Ok(d) => d,
                    Err(e) => {
                        let code = map_rpc_error(e.clone());
                        record_error(
                            code,
                            format!(
                                "wallet_preview_fee_with_filter: failed to construct bin16 decoy daemon for '{base_url}': {e}"
                            ),
                        );
                        return ptr::null_mut();
                    }
                };

                match TOKIO_RUNTIME.block_on(monero_wallet::OutputWithDecoys::new(
                    &mut rng,
                    &daemon_iface,
                    ring_len_eff,
                    usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                    wallet_out,
                )) {
                    Ok(i) => i,
                    Err(err) => {
                        let code = match &err {
                            monero_interface::TransactionsError::InterfaceError(inner) => {
                                map_rpc_error(inner.clone())
                            }
                            monero_interface::TransactionsError::TransactionNotFound => -16,
                            monero_interface::TransactionsError::PrunedTransaction => -16,
                        };
                        record_error(
                            code,
                            format!(
                                "wallet_preview_fee_with_filter: decoy selection failed ({err:?})"
                            ),
                        );
                        return ptr::null_mut();
                    }
                }
            } else {
                match TOKIO_RUNTIME.block_on(monero_wallet::OutputWithDecoys::new(
                    &mut rng,
                    &rpc_client,
                    ring_len_eff,
                    usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                    wallet_out,
                )) {
                    Ok(i) => i,
                    Err(err) => {
                        let code = match &err {
                            monero_interface::TransactionsError::InterfaceError(inner) => {
                                map_rpc_error(inner.clone())
                            }
                            monero_interface::TransactionsError::TransactionNotFound => -16,
                            monero_interface::TransactionsError::PrunedTransaction => -16,
                        };
                        record_error(
                            code,
                            format!(
                                "wallet_preview_fee_with_filter: decoy selection failed ({err:?})"
                            ),
                        );
                        return ptr::null_mut();
                    }
                }
            };

            inputs.push(with_decoys);
        }

        let mut ovk = [0u8; 32];
        rng.fill_bytes(&mut ovk);

        let intent = match monero_wallet::send::SignableTransaction::new(
            monero_wallet::ringct::RctType::ClsagBulletproofPlus,
            Zeroizing::new(ovk),
            inputs,
            destinations.clone(),
            change.clone(),
            Vec::new(),
            fee_rate,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not enough funds") {
                    let mut added_any = false;
                    for o in &spendable {
                        if selected
                            .iter()
                            .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
                        {
                            continue;
                        }
                        selected.push(o.clone());
                        selected_sum = selected_sum.saturating_add(o.amount);
                        added_any = true;
                        break;
                    }

                    if !added_any {
                        record_error(
                            -18,
                            format!(
                                "wallet_preview_fee_with_filter: insufficient unlocked funds for amount+fee (have {}, need at least {})",
                                selected_sum, total_needed
                            ),
                        );
                        return ptr::null_mut();
                    }

                    continue;
                }

                record_error(
                    -16,
                    format!(
                        "wallet_preview_fee_with_filter: transaction construction failed ({e})"
                    ),
                );
                return ptr::null_mut();
            }
        };

        let fee = intent.necessary_fee();
        let needed_total = total_needed.saturating_add(fee);

        if selected_sum >= needed_total {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "✅ wallet_preview_fee_with_filter ok wallet_id={} inputs_selected={} inputs_sum={} total_needed={} fee={} needed_total={}",
                    id,
                    selected.len(),
                    selected_sum,
                    total_needed,
                    fee,
                    needed_total
                ),
            );

            let json = match serde_json::to_string(&serde_json::json!({ "fee": fee })) {
                Ok(s) => s,
                Err(err) => {
                    record_error(
                        -16,
                        format!(
                            "wallet_preview_fee_with_filter: result JSON serialization failed ({err})"
                        ),
                    );
                    return ptr::null_mut();
                }
            };

            return match CString::new(json) {
                Ok(cstr) => {
                    clear_last_error();
                    cstr.into_raw()
                }
                Err(_) => {
                    record_error(
                        -16,
                        "wallet_preview_fee_with_filter: result JSON contained interior null bytes",
                    );
                    ptr::null_mut()
                }
            };
        }

        if let Some(last) = last_needed_total {
            if needed_total <= last && round > 0 {
                // Not a hard error; just continue selecting more.
            }
        }
        last_needed_total = Some(needed_total);

        let mut added_any = false;
        for o in &spendable {
            if selected
                .iter()
                .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
            {
                continue;
            }
            selected.push(o.clone());
            selected_sum = selected_sum.saturating_add(o.amount);
            added_any = true;
            if selected_sum >= needed_total {
                break;
            }
        }

        if !added_any {
            record_error(
                -18,
                format!(
                    "wallet_preview_fee_with_filter: insufficient unlocked funds for amount+fee (have {}, need {})",
                    selected_sum, needed_total
                ),
            );
            return ptr::null_mut();
        }
    }

    record_error(
        -16,
        "wallet_preview_fee_with_filter: fee estimation did not converge (too many selection rounds)",
    );
    return ptr::null_mut();
}
