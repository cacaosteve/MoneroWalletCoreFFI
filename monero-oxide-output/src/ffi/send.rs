//! Send-related FFI surface extracted from the historical mega-`lib.rs`.
//!
//! This module intentionally keeps behavior identical to the inlined implementation,
//! while relying on `crate::support` for a small, stable set of re-exports.

#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_return)]

use crate::support::*;

use core::ffi::c_char;
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use std::{
    ffi::{CStr, CString},
    ptr,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

// External types used by the send path.
use monero_address::MoneroAddress;
use monero_interface::{FeeError, FeeRate};
use monero_wallet::Scanner;

/// Auto-retry configuration for the send path.
///
/// Default: retry up to 2 times when we *actually* quarantine a newly-discovered toxic input.
/// This is deliberately bounded to avoid long UI stalls.
///
/// Override with:
/// - `WALLETCORE_SEND_RETRY_MAX` (integer, default 2)
/// - `WALLETCORE_SEND_AUTORETRY=0` to disable
fn walletcore_send_retry_max() -> usize {
    if std::env::var("WALLETCORE_SEND_AUTORETRY")
        .ok()
        .is_some_and(|v| v == "0")
    {
        return 0;
    }

    std::env::var("WALLETCORE_SEND_RETRY_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(2)
}

/// Single-destination convenience send (legacy API).
#[no_mangle]
pub extern "C" fn wallet_send(
    wallet_id: *const c_char,
    node_url: *const c_char,
    to_address: *const c_char,
    amount_piconero: u64,
    ring_len: u8,
) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() || to_address.is_null() {
        record_error(-11, "wallet_send: null argument(s)");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_send: wallet_id contained invalid UTF-8");
            return ptr::null_mut();
        }
    };

    let recipient_str = match unsafe { CStr::from_ptr(to_address) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(-10, "wallet_send: to_address contained invalid UTF-8");
            return ptr::null_mut();
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

    // Lookup wallet snapshot
    let mut snapshot = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get(id) {
            Some(state) => state.clone(),
            None => {
                record_error(-13, format!("wallet_send: wallet '{id}' not registered"));
                return ptr::null_mut();
            }
        }
    };

    // Parse recipient address on the same network
    let recipient_address = match MoneroAddress::from_str(snapshot.network, recipient_str) {
        Ok(addr) => addr,
        Err(_) => {
            record_error(
                -10,
                "wallet_send: invalid recipient address for wallet network",
            );
            return ptr::null_mut();
        }
    };

    // Build daemon RPC client (upstream)
    let rpc_client: RpcClient = match TOKIO_RUNTIME.block_on(
        monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()),
    ) {
        Ok(d) => d,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_send: failed to connect daemon '{base_url}': {e}"),
            );
            return ptr::null_mut();
        }
    };

    // Daemon height (0-based latest_block_number + 1)
    let daemon_height = match TOKIO_RUNTIME.block_on(rpc_client.latest_block_number()) {
        Ok(n) => n.saturating_add(1) as u64,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_send: failed to query daemon height '{base_url}': {e}"),
            );
            return ptr::null_mut();
        }
    };

    let daemon = DaemonStatus {
        height: daemon_height,
        top_block_timestamp: 0,
    };

    // Construct master keys and view pair
    let master = match master_keys_from_mnemonic_str(&snapshot.mnemonic) {
        Ok(keys) => keys,
        Err(code) => {
            record_error(code, "wallet_send: unable to parse mnemonic");
            return ptr::null_mut();
        }
    };
    let view_pair = match master.to_view_pair() {
        Ok(pair) => pair,
        Err(code) => {
            record_error(code, "wallet_send: failed to construct view pair");
            return ptr::null_mut();
        }
    };

    // Prepare scanner with registered subaddresses up to gap_limit
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

    // Fee rate (once)
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
                record_error(code, "wallet_send: fee_rate failed");
                return ptr::null_mut();
            }
        };

    // Change to primary account
    let change = monero_wallet::send::Change::new(view_pair.clone(), None);

    // Ring length normalization (default to 16 when caller passes nonsense)
    let mut rng = OsRng;
    let ring_len_eff: u8 = if ring_len < 2 { 16 } else { ring_len };

    // Bounded auto-retry: only retries when we *actually* quarantine a newly discovered toxic input.
    let max_retries = walletcore_send_retry_max();
    let mut quarantined_this_call: usize = 0;

    for attempt in 0..=max_retries {
        // Always re-pull snapshot after a quarantine so spendable universe changes.
        if attempt > 0 {
            snapshot = {
                let map = WALLET_STORE.lock().expect("wallet store poisoned");
                match map.get(id) {
                    Some(state) => state.clone(),
                    None => {
                        record_error(-13, format!("wallet_send: wallet '{id}' not registered"));
                        return ptr::null_mut();
                    }
                }
            };

            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🔁 wallet_send auto-retry attempt={} max_retries={} quarantined_this_call={} wallet_id={}",
                    attempt,
                    max_retries,
                    quarantined_this_call,
                    id
                ),
            );
        }

        // Choose spendable outputs (unspent and unlocked), excluding quarantined outpoints.
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

        // Input selection order
        match walletcore_input_select_mode() {
            InputSelectMode::SmallestFirst => spendable.sort_by_key(|o| o.amount),
            InputSelectMode::LargestFirst => spendable.sort_by(|a, b| b.amount.cmp(&a.amount)),
        }

        // Iterative input selection until amount+fee is covered
        let mut selected_tracked: Vec<TrackedOutput> = Vec::new();
        let mut selected_sum: u64 = 0;
        let max_selection_rounds: usize = 24;

        for _round in 0..max_selection_rounds {
            if selected_tracked.is_empty() {
                for o in &spendable {
                    selected_tracked.push(o.clone());
                    selected_sum = selected_sum.saturating_add(o.amount);
                    if selected_sum >= amount_piconero {
                        break;
                    }
                }

                if selected_sum < amount_piconero {
                    record_error(
                        -18,
                        format!(
                            "wallet_send: insufficient unlocked funds (have {}, need {})",
                            selected_sum, amount_piconero
                        ),
                    );
                    return ptr::null_mut();
                }
            }

            // Reconstruct wallet outputs + decoys for current selection
            let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
            for t in &selected_tracked {
                let block_number = match usize::try_from(t.block_height) {
                    Ok(value) => value,
                    Err(_) => {
                        record_error(-16, "wallet_send: block number conversion overflow");
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
                                "wallet_send: RPC block fetch failed at height {}",
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
                            format!("wallet_send: scanner failed at height {}", t.block_height),
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
                            "wallet_send: failed to reconstruct selected output (not found after scan)",
                        );
                        return ptr::null_mut();
                    }
                };

                let with_decoys = if walletcore_decoy_mode_bin16() {
                    let ring_len_eff: u8 = 16;
                    let daemon_iface = match TOKIO_RUNTIME
                        .block_on(make_bin_decoy_daemon(&base_url))
                    {
                        Ok(d) => d,
                        Err(e) => {
                            let code = map_rpc_error(e.clone());
                            record_error(
                                code,
                                format!(
                                    "wallet_send: failed to construct bin16 decoy daemon for '{base_url}': {e}"
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
                                format!("wallet_send: decoy selection failed ({err:?})"),
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
                                format!("wallet_send: decoy selection failed ({err:?})"),
                            );
                            return ptr::null_mut();
                        }
                    }
                };

                inputs.push(with_decoys);
            }

            // New OVK seed each attempt
            let mut ovk = [0u8; 32];
            rng.fill_bytes(&mut ovk);

            // Build signable tx
            let intent = match monero_wallet::send::SignableTransaction::new(
                monero_wallet::ringct::RctType::ClsagBulletproofPlus,
                Zeroizing::new(ovk),
                inputs,
                vec![(recipient_address, amount_piconero)],
                change.clone(),
                Vec::new(),
                fee_rate,
            ) {
                Ok(tx) => tx,
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("not enough funds") {
                        // Add one more input and retry
                        let mut added_any = false;
                        for o in &spendable {
                            if selected_tracked
                                .iter()
                                .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
                            {
                                continue;
                            }
                            selected_tracked.push(o.clone());
                            selected_sum = selected_sum.saturating_add(o.amount);
                            added_any = true;
                            break;
                        }
                        if !added_any {
                            record_error(
                                -18,
                                format!(
                                    "wallet_send: insufficient unlocked funds for amount+fee (have {}, need at least {})",
                                    selected_sum, amount_piconero
                                ),
                            );
                            return ptr::null_mut();
                        }
                        continue;
                    }

                    record_error(
                        -16,
                        format!("wallet_send: transaction construction failed ({e})"),
                    );
                    return ptr::null_mut();
                }
            };

            let fee_piconero = intent.necessary_fee();
            let needed_total = amount_piconero.saturating_add(fee_piconero);

            if selected_sum >= needed_total {
                break;
            }

            // Add more inputs until we cover needed_total, then retry
            let mut added_any = false;
            for o in &spendable {
                if selected_tracked
                    .iter()
                    .any(|s| s.tx_hash == o.tx_hash && s.index_in_tx == o.index_in_tx)
                {
                    continue;
                }
                selected_tracked.push(o.clone());
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
                        "wallet_send: insufficient unlocked funds for amount+fee (have {}, need {})",
                        selected_sum, needed_total
                    ),
                );
                return ptr::null_mut();
            }
        }

        // Rebuild inputs one last time for final tx
        let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
        for t in &selected_tracked {
            let block_number = match usize::try_from(t.block_height) {
                Ok(value) => value,
                Err(_) => {
                    record_error(-16, "wallet_send: block number conversion overflow");
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
                                "wallet_send: RPC block fetch failed at height {}",
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
                        format!("wallet_send: scanner failed at height {}", t.block_height),
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
                        "wallet_send: failed to reconstruct selected output (not found after scan)",
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
                                "wallet_send: failed to construct bin16 decoy daemon for '{base_url}': {e}"
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
                            format!("wallet_send: decoy selection failed ({err:?})"),
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
                            format!("wallet_send: decoy selection failed ({err:?})"),
                        );
                        return ptr::null_mut();
                    }
                }
            };

            inputs.push(with_decoys);
        }

        // New OVK seed
        let mut ovk = [0u8; 32];
        rng.fill_bytes(&mut ovk);

        let intent = match monero_wallet::send::SignableTransaction::new(
            monero_wallet::ringct::RctType::ClsagBulletproofPlus,
            Zeroizing::new(ovk),
            inputs,
            vec![(recipient_address, amount_piconero)],
            change.clone(),
            Vec::new(),
            fee_rate,
        ) {
            Ok(tx) => tx,
            Err(e) => {
                record_error(
                    -16,
                    format!("wallet_send: transaction construction failed ({e})"),
                );
                return ptr::null_mut();
            }
        };
        let fee_piconero = intent.necessary_fee();

        // Sign
        let spend_key = Zeroizing::new(monero_wallet::ed25519::Scalar::from(master.spend_scalar));
        let mut signer_rng = OsRng;
        let tx = match intent.sign(&mut signer_rng, &spend_key) {
            Ok(tx) => tx,
            Err(e) => {
                record_error(-16, format!("wallet_send: signing failed ({e})"));
                return ptr::null_mut();
            }
        };

        // Broadcast via /send_raw_transaction
        let tx_blob = tx.serialize();
        if let Err(err) =
            TOKIO_RUNTIME.block_on(broadcast_send_raw_transaction(&base_url, &tx_blob))
        {
            let code = map_rpc_error(err.clone());
            let msg = format!("wallet_send: send_raw_transaction failed ({err})");

            // Log selected inputs for correlation
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧾 send selected_inputs wallet_id={} selected_count={} selected_sum={} inputs={}",
                    id,
                    selected_tracked.len(),
                    selected_sum,
                    selected_tracked
                        .iter()
                        .map(|o| {
                            format!(
                                "{}:{}@{}:{}",
                                hex_lowercase(&o.tx_hash),
                                o.index_in_tx,
                                o.block_height,
                                o.amount
                            )
                        })
                        .collect::<Vec<String>>()
                        .join(",")
                ),
            );

            walletcore_debug_dump_tracked_outputs(
                id,
                snapshot.network,
                "send selected_input_dump",
                &selected_tracked,
                daemon.height,
                daemon.top_block_timestamp,
            );

            // Optional: bisect on invalid_input, and optionally on status=Failed.
            let should_bisect = walletcore_send_bisect_enabled()
                && (is_invalid_input_send_raw_tx_error(&msg)
                    || (walletcore_send_bisect_on_failed_enabled()
                        && is_failed_send_raw_tx_error(&msg)));

            // If we're not bisecting (or can't quarantine), just return the error.
            if !should_bisect {
                record_error(code, msg);
                return ptr::null_mut();
            }

            let start = Instant::now();
            let budget = Duration::from_secs(20);

            let mut all = selected_tracked.clone();
            all.sort_by(|a, b| b.amount.cmp(&a.amount));

            let mut try_subset = |subset: &[TrackedOutput]| -> Result<(), String> {
                let mut rng = OsRng;
                let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();

                for t in subset {
                    let block_number = usize::try_from(t.block_height)
                        .map_err(|_| "block number conversion overflow".to_string())?;
                    let scannable = TOKIO_RUNTIME
                        .block_on(rpc_client.scannable_block_by_number(block_number))
                        .map_err(|e| {
                            format!(
                                "RPC block fetch failed at height {} ({})",
                                t.block_height, e
                            )
                        })?;
                    let outputs = scanner
                        .scan(scannable)
                        .map_err(|_| format!("scanner failed at height {}", t.block_height))?
                        .ignore_additional_timelock();
                    let wallet_out = outputs
                        .into_iter()
                        .find(|wo| {
                            wo.transaction() == t.tx_hash
                                && wo.index_in_transaction() == t.index_in_tx
                        })
                        .ok_or_else(|| "failed to reconstruct selected output".to_string())?;

                    let with_decoys = if walletcore_decoy_mode_bin16() {
                        let ring_len_eff: u8 = 16;
                        let daemon_iface = TOKIO_RUNTIME
                            .block_on(make_bin_decoy_daemon(&base_url))
                            .map_err(|e| {
                                format!("failed to construct bin16 decoy daemon ({})", e)
                            })?;
                        TOKIO_RUNTIME
                            .block_on(monero_wallet::OutputWithDecoys::new(
                                &mut rng,
                                &daemon_iface,
                                ring_len_eff,
                                usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                                wallet_out,
                            ))
                            .map_err(|e| format!("decoy selection failed ({:?})", e))?
                    } else {
                        TOKIO_RUNTIME
                            .block_on(monero_wallet::OutputWithDecoys::new(
                                &mut rng,
                                &rpc_client,
                                ring_len_eff,
                                usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                                wallet_out,
                            ))
                            .map_err(|e| format!("decoy selection failed ({:?})", e))?
                    };

                    inputs.push(with_decoys);
                }

                let mut ovk = [0u8; 32];
                rng.fill_bytes(&mut ovk);

                let intent = monero_wallet::send::SignableTransaction::new(
                    monero_wallet::ringct::RctType::ClsagBulletproofPlus,
                    Zeroizing::new(ovk),
                    inputs,
                    vec![(recipient_address, amount_piconero)],
                    change.clone(),
                    Vec::new(),
                    fee_rate,
                )
                .map_err(|e| format!("construct failed ({e})"))?;

                let spend_key =
                    Zeroizing::new(monero_wallet::ed25519::Scalar::from(master.spend_scalar));
                let mut signer_rng = OsRng;
                let tx = intent
                    .sign(&mut signer_rng, &spend_key)
                    .map_err(|e| format!("sign failed ({e})"))?;

                let tx_blob = tx.serialize();
                match TOKIO_RUNTIME.block_on(broadcast_send_raw_transaction(&base_url, &tx_blob)) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        let tag = if is_invalid_input_send_raw_tx_error(&format!("{e}")) {
                            "invalid_input"
                        } else if walletcore_send_bisect_on_failed_enabled()
                            && is_failed_send_raw_tx_error(&format!("{e}"))
                        {
                            "failed"
                        } else {
                            "other"
                        };
                        Err(format!("broadcast failed ({}): {}", tag, e))
                    }
                }
            };

            let mut lo = 0usize;
            let mut hi = all.len();
            let mut last_err: Option<String> = None;

            while start.elapsed() < budget && lo + 1 < hi {
                let mid = (lo + hi) / 2;
                let subset = &all[..mid];
                match try_subset(subset) {
                    Ok(()) => {
                        lo = mid.max(lo + 1);
                    }
                    Err(e) => {
                        let is_signal = e.contains("broadcast failed (invalid_input):")
                            || (walletcore_send_bisect_on_failed_enabled()
                                && e.contains("broadcast failed (failed):"));
                        if is_signal {
                            last_err = Some(e);
                            hi = mid.max(lo + 1);
                        } else {
                            lo = mid.max(lo + 1);
                        }
                    }
                }
            }

            let mut newly_inserted = false;
            let mut quarantined_out: Option<(String, u32)> = None;

            if hi <= all.len() && hi > 0 {
                let bad = &all[hi - 1];
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧨 send_bisect: candidate invalid_input output wallet_id={} txid={} index_in_tx={} height={} amount_piconero={} err={}",
                        id,
                        hex_dump_prefix(&bad.tx_hash, 32),
                        bad.index_in_tx,
                        bad.block_height,
                        bad.amount,
                        last_err.clone().unwrap_or_else(|| "(none)".to_string())
                    ),
                );

                if let Ok(mut map) = WALLET_STORE.lock() {
                    if let Some(state) = map.get_mut(id) {
                        let key = (bad.tx_hash, bad.index_in_tx);
                        newly_inserted = state.invalid_input_quarantine.insert(key);
                        quarantined_out =
                            Some((hex_lowercase(&bad.tx_hash), bad.index_in_tx as u32));
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧾 invalid_input_quarantine {} wallet_id={} out={} quarantine_size={}",
                                if newly_inserted {
                                    "added"
                                } else {
                                    "already_present"
                                },
                                id,
                                format!("{}:{}", hex_lowercase(&bad.tx_hash), bad.index_in_tx),
                                state.invalid_input_quarantine.len()
                            ),
                        );
                    }
                }
            }

            if newly_inserted && attempt < max_retries {
                quarantined_this_call = quarantined_this_call.saturating_add(1);
                if let Some((txid_hex, idx)) = quarantined_out {
                    walletcore_log_line(
                        id,
                        snapshot.network,
                        &format!(
                            "🔁 wallet_send auto-retry scheduling next attempt wallet_id={} quarantined_out={}:{} attempt={} max_retries={}",
                            id,
                            txid_hex,
                            idx,
                            attempt,
                            max_retries
                        ),
                    );
                }
                continue;
            }

            // No quarantine happened, or retries exhausted -> return the original broadcast error.
            if newly_inserted && attempt >= max_retries {
                record_error(
                    code,
                    format!(
                        "wallet_send: send_raw_transaction failed; quarantined {}; retries exhausted ({}/{}) ({})",
                        quarantined_this_call,
                        attempt,
                        max_retries,
                        err
                    ),
                );
                return ptr::null_mut();
            }

            record_error(code, msg);
            return ptr::null_mut();
        }

        // Broadcast succeeded -> update store and return success.
        {
            let mut map = WALLET_STORE.lock().expect("wallet store poisoned");
            if let Some(state) = map.get_mut(id) {
                let spent_sum: u64 = selected_tracked.iter().map(|t| t.amount).sum();
                for t in &selected_tracked {
                    if let Some(o) = state
                        .tracked_outputs
                        .iter_mut()
                        .find(|o| o.tx_hash == t.tx_hash && o.index_in_tx == t.index_in_tx)
                    {
                        o.spent = true;
                    }
                }
                state.total = state.total.saturating_sub(spent_sum);
                state.unlocked = state.unlocked.saturating_sub(spent_sum);
            }
        }

        let tx_hash = tx.hash();
        let hex = hex_lowercase(&tx_hash);

        {
            let mut map = WALLET_STORE.lock().expect("wallet store poisoned");
            if let Some(state) = map.get_mut(id) {
                state.pending_outgoing.push(PendingOutgoingTx {
                    txid: hex.clone(),
                    amount: amount_piconero,
                    fee: fee_piconero,
                    created_at: state.chain_time,
                });

                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧾 pending_outgoing recorded wallet_id={} txid={} amount_piconero={} fee_piconero={} created_at={} pending_outgoing_count={}",
                        id,
                        hex,
                        amount_piconero,
                        fee_piconero,
                        state.chain_time,
                        state.pending_outgoing.len()
                    ),
                );

                state.tx_ledger.insert(
                    hex.clone(),
                    LedgerEntry {
                        txid: hex.clone(),
                        direction: "out".to_string(),
                        amount: amount_piconero,
                        fee: Some(fee_piconero),
                        height: None,
                        timestamp: Some(state.chain_time),
                        is_pending: true,
                        is_coinbase: false,
                    },
                );
            }
        }

        let result_json = match serde_json::to_string(&serde_json::json!({
            "txid": hex,
            "fee": fee_piconero
        })) {
            Ok(s) => s,
            Err(err) => {
                record_error(
                    -16,
                    format!("wallet_send: result JSON serialization failed ({err})"),
                );
                return ptr::null_mut();
            }
        };

        return match CString::new(result_json) {
            Ok(cstr) => {
                clear_last_error();
                cstr.into_raw()
            }
            Err(_) => {
                record_error(
                    -16,
                    "wallet_send: result JSON contained interior null bytes",
                );
                ptr::null_mut()
            }
        };
    }

    record_error(
        -16,
        format!(
            "wallet_send: failed after retries (max_retries={})",
            walletcore_send_retry_max()
        ),
    );
    ptr::null_mut()
}

/// Multi-destination send with optional input filtering.
#[no_mangle]
pub extern "C" fn wallet_send_with_filter(
    wallet_id: *const c_char,
    node_url: *const c_char,
    destinations_json: *const c_char,
    filter_json: *const c_char,
    ring_len: u8,
) -> *mut c_char {
    clear_last_error();

    if wallet_id.is_null() || destinations_json.is_null() {
        record_error(-11, "wallet_send_with_filter: null argument(s)");
        return ptr::null_mut();
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(
                -10,
                "wallet_send_with_filter: wallet_id contained invalid UTF-8",
            );
            return ptr::null_mut();
        }
    };

    let dests_str = match unsafe { CStr::from_ptr(destinations_json) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            record_error(
                -10,
                "wallet_send_with_filter: destinations_json invalid UTF-8",
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
                format!("wallet_send_with_filter: invalid destinations JSON ({err})"),
            );
            return ptr::null_mut();
        }
    };
    if pays.is_empty() {
        record_error(-11, "wallet_send_with_filter: empty destinations");
        return ptr::null_mut();
    }

    let filter: Option<InputFilter> = match filt_str_opt {
        Some(s) if !s.is_empty() => match serde_json::from_str(s) {
            Ok(f) => Some(f),
            Err(err) => {
                record_error(
                    -11,
                    format!("wallet_send_with_filter: invalid filter JSON ({err})"),
                );
                return ptr::null_mut();
            }
        },
        _ => None,
    };

    let snapshot = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get(id) {
            Some(state) => state.clone(),
            None => {
                record_error(
                    -13,
                    format!("wallet_send_with_filter: wallet '{id}' not registered"),
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
                format!("wallet_send_with_filter: failed to connect daemon '{base_url}': {e}"),
            );
            return ptr::null_mut();
        }
    };

    let daemon_height = match TOKIO_RUNTIME.block_on(rpc_client.latest_block_number()) {
        Ok(n) => n.saturating_add(1) as u64,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_send_with_filter: failed to query daemon height '{base_url}': {e}"),
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
            record_error(code, "wallet_send_with_filter: unable to parse mnemonic");
            return ptr::null_mut();
        }
    };
    let view_pair = match master.to_view_pair() {
        Ok(pair) => pair,
        Err(code) => {
            record_error(
                code,
                "wallet_send_with_filter: failed to construct view pair",
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

    let mut destinations: Vec<(monero_address::MoneroAddress, u64)> =
        Vec::with_capacity(pays.len());
    let mut total_needed: u64 = 0;
    for p in &pays {
        let addr = match MoneroAddress::from_str(snapshot.network, &p.address) {
            Ok(a) => a,
            Err(_) => {
                record_error(-10, "wallet_send_with_filter: invalid destination address");
                return ptr::null_mut();
            }
        };
        total_needed = total_needed.saturating_add(p.amount);
        destinations.push((addr, p.amount));
    }

    // Filter spendable outputs
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

    // Fee rate once
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
                record_error(code, "wallet_send_with_filter: fee_rate failed");
                return ptr::null_mut();
            }
        };

    let change = monero_wallet::send::Change::new(view_pair.clone(), None);

    let mut rng = OsRng;
    let ring_len_eff: u8 = if ring_len < 2 { 16 } else { ring_len };

    let mut selected: Vec<TrackedOutput> = Vec::new();
    let mut selected_sum: u64 = 0;

    let max_selection_rounds: usize = 24;

    for _round in 0..max_selection_rounds {
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
                        "wallet_send_with_filter: insufficient unlocked funds (have {}, need {})",
                        selected_sum, total_needed
                    ),
                );
                return ptr::null_mut();
            }
        }

        let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
        for t in &selected {
            let block_number = match usize::try_from(t.block_height) {
                Ok(value) => value,
                Err(_) => {
                    record_error(
                        -16,
                        "wallet_send_with_filter: block number conversion overflow",
                    );
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
                                "wallet_send_with_filter: RPC block fetch failed at height {}",
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
                            "wallet_send_with_filter: scanner failed at height {}",
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
                        "wallet_send_with_filter: failed to reconstruct selected output",
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
                                "wallet_send_with_filter: failed to construct bin16 decoy daemon for '{base_url}': {e}"
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
                            format!("wallet_send_with_filter: decoy selection failed ({err:?})"),
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
                            format!("wallet_send_with_filter: decoy selection failed ({err:?})"),
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
                                "wallet_send_with_filter: insufficient unlocked funds for amount+fee (have {}, need at least {})",
                                selected_sum, total_needed
                            ),
                        );
                        return ptr::null_mut();
                    }

                    continue;
                }

                record_error(
                    -16,
                    format!("wallet_send_with_filter: transaction construction failed ({e})"),
                );
                return ptr::null_mut();
            }
        };

        let fee_piconero = intent.necessary_fee();
        let needed_total = total_needed.saturating_add(fee_piconero);

        if selected_sum >= needed_total {
            break;
        }

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
                    "wallet_send_with_filter: insufficient unlocked funds for amount+fee (have {}, need {})",
                    selected_sum, needed_total
                ),
            );
            return ptr::null_mut();
        }
    }

    // Rebuild final tx for signing/broadcast
    let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();
    for t in &selected {
        let block_number = match usize::try_from(t.block_height) {
            Ok(value) => value,
            Err(_) => {
                record_error(
                    -16,
                    "wallet_send_with_filter: block number conversion overflow",
                );
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
                            "wallet_send_with_filter: RPC block fetch failed at height {}",
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
                        "wallet_send_with_filter: scanner failed at height {}",
                        t.block_height
                    ),
                );
                return ptr::null_mut();
            }
        };
        let wallet_out = match outputs
            .into_iter()
            .find(|wo| wo.transaction() == t.tx_hash && wo.index_in_transaction() == t.index_in_tx)
        {
            Some(wo) => wo,
            None => {
                record_error(
                    -16,
                    "wallet_send_with_filter: failed to reconstruct selected output",
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
                            "wallet_send_with_filter: failed to construct bin16 decoy daemon for '{base_url}': {e}"
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
                        format!("wallet_send_with_filter: decoy selection failed ({err:?})"),
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
                        format!("wallet_send_with_filter: decoy selection failed ({err:?})"),
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
            record_error(
                -16,
                format!("wallet_send_with_filter: transaction construction failed ({e})"),
            );
            return ptr::null_mut();
        }
    };
    let fee_piconero = intent.necessary_fee();

    let spend_key = Zeroizing::new(monero_wallet::ed25519::Scalar::from(master.spend_scalar));
    let mut signer_rng = OsRng;
    let tx = match intent.sign(&mut signer_rng, &spend_key) {
        Ok(tx) => tx,
        Err(e) => {
            record_error(
                -16,
                format!("wallet_send_with_filter: signing failed ({e})"),
            );
            return ptr::null_mut();
        }
    };

    let tx_blob = tx.serialize();
    if let Err(err) = TOKIO_RUNTIME.block_on(broadcast_send_raw_transaction(&base_url, &tx_blob)) {
        let code = map_rpc_error(err.clone());
        let msg = format!("wallet_send_with_filter: send_raw_transaction failed ({err})");

        // Optional bisect (legacy: only on invalid_input)
        if walletcore_send_bisect_enabled() && is_invalid_input_send_raw_tx_error(&msg) {
            let start = Instant::now();
            let budget = Duration::from_secs(20);

            let mut all = selected.clone();
            all.sort_by(|a, b| b.amount.cmp(&a.amount));

            let mut try_subset = |subset: &[TrackedOutput]| -> Result<(), String> {
                let mut rng = OsRng;
                let mut inputs: Vec<monero_wallet::OutputWithDecoys> = Vec::new();

                for t in subset {
                    let block_number = usize::try_from(t.block_height)
                        .map_err(|_| "block number conversion overflow".to_string())?;
                    let scannable = TOKIO_RUNTIME
                        .block_on(rpc_client.scannable_block_by_number(block_number))
                        .map_err(|e| {
                            format!(
                                "RPC block fetch failed at height {} ({})",
                                t.block_height, e
                            )
                        })?;
                    let outputs = scanner
                        .scan(scannable)
                        .map_err(|_| format!("scanner failed at height {}", t.block_height))?
                        .ignore_additional_timelock();
                    let wallet_out = outputs
                        .into_iter()
                        .find(|wo| {
                            wo.transaction() == t.tx_hash
                                && wo.index_in_transaction() == t.index_in_tx
                        })
                        .ok_or_else(|| "failed to reconstruct selected output".to_string())?;

                    let with_decoys = if walletcore_decoy_mode_bin16() {
                        let ring_len_eff: u8 = 16;
                        let daemon_iface = TOKIO_RUNTIME
                            .block_on(make_bin_decoy_daemon(&base_url))
                            .map_err(|e| {
                                format!("failed to construct bin16 decoy daemon ({})", e)
                            })?;
                        TOKIO_RUNTIME
                            .block_on(monero_wallet::OutputWithDecoys::new(
                                &mut rng,
                                &daemon_iface,
                                ring_len_eff,
                                usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                                wallet_out,
                            ))
                            .map_err(|e| format!("decoy selection failed ({:?})", e))?
                    } else {
                        TOKIO_RUNTIME
                            .block_on(monero_wallet::OutputWithDecoys::new(
                                &mut rng,
                                &rpc_client,
                                ring_len_eff,
                                usize::try_from(daemon.height).unwrap_or(daemon.height as usize),
                                wallet_out,
                            ))
                            .map_err(|e| format!("decoy selection failed ({:?})", e))?
                    };

                    inputs.push(with_decoys);
                }

                let mut ovk = [0u8; 32];
                rng.fill_bytes(&mut ovk);

                let intent = monero_wallet::send::SignableTransaction::new(
                    monero_wallet::ringct::RctType::ClsagBulletproofPlus,
                    Zeroizing::new(ovk),
                    inputs,
                    destinations.clone(),
                    change.clone(),
                    Vec::new(),
                    fee_rate,
                )
                .map_err(|e| format!("construct failed ({e})"))?;

                let spend_key =
                    Zeroizing::new(monero_wallet::ed25519::Scalar::from(master.spend_scalar));
                let mut signer_rng = OsRng;
                let tx = intent
                    .sign(&mut signer_rng, &spend_key)
                    .map_err(|e| format!("sign failed ({e})"))?;

                let tx_blob = tx.serialize();
                match TOKIO_RUNTIME.block_on(broadcast_send_raw_transaction(&base_url, &tx_blob)) {
                    Ok(()) => Ok(()),
                    Err(e) => Err(format!(
                        "broadcast failed ({}): {}",
                        if is_invalid_input_send_raw_tx_error(&format!("{e}")) {
                            "invalid_input"
                        } else {
                            "other"
                        },
                        e
                    )),
                }
            };

            let mut lo = 0usize;
            let mut hi = all.len();
            let mut last_err: Option<String> = None;

            while lo < hi && start.elapsed() <= budget {
                let mid = (lo + hi) / 2;
                let test: Vec<TrackedOutput> = all[lo..mid.max(lo + 1)].to_vec();

                match try_subset(&test) {
                    Ok(()) => {
                        lo = mid.max(lo + 1);
                    }
                    Err(e) => {
                        if e.contains("broadcast failed (invalid_input):") {
                            last_err = Some(e);
                            hi = mid.max(lo + 1);
                        } else {
                            lo = mid.max(lo + 1);
                        }
                    }
                }

                if hi.saturating_sub(lo) == 1 {
                    let bad = &all[lo];
                    walletcore_log_line(
                        id,
                        snapshot.network,
                        &format!(
                            "🧨 send_bisect: candidate invalid_input output wallet_id={} txid={} index_in_tx={} height={} amount_piconero={} err={}",
                            id,
                            hex_dump_prefix(&bad.tx_hash, 32),
                            bad.index_in_tx,
                            bad.block_height,
                            bad.amount,
                            last_err.clone().unwrap_or_else(|| "(none)".to_string())
                        ),
                    );
                    break;
                }
            }
        }

        record_error(code, msg);
        return ptr::null_mut();
    }

    // Mark spent + adjust totals
    {
        let mut map = WALLET_STORE.lock().expect("wallet store poisoned");
        if let Some(state) = map.get_mut(id) {
            let spent_sum: u64 = selected.iter().map(|t| t.amount).sum();
            for t in &selected {
                if let Some(o) = state
                    .tracked_outputs
                    .iter_mut()
                    .find(|o| o.tx_hash == t.tx_hash && o.index_in_tx == t.index_in_tx)
                {
                    o.spent = true;
                }
            }
            state.total = state.total.saturating_sub(spent_sum);
            state.unlocked = state.unlocked.saturating_sub(spent_sum);
        }
    }

    let tx_hash = tx.hash();
    let hex = hex_lowercase(&tx_hash);

    let result_json = match serde_json::to_string(&serde_json::json!({
        "txid": hex,
        "fee": fee_piconero
    })) {
        Ok(s) => s,
        Err(err) => {
            record_error(
                -16,
                format!("wallet_send_with_filter: result JSON serialization failed ({err})"),
            );
            return ptr::null_mut();
        }
    };

    match CString::new(result_json) {
        Ok(cstr) => {
            clear_last_error();
            cstr.into_raw()
        }
        Err(_) => {
            record_error(
                -16,
                "wallet_send_with_filter: result JSON contained interior null bytes",
            );
            ptr::null_mut()
        }
    }
}
