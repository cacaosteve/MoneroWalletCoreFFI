//! Refresh-related FFI surface extracted from the historical mega-`lib.rs`.
//!
//! This module is intentionally "mechanical": it mirrors the previous inlined behavior
//! as closely as possible while using `crate::support` for shared globals/helpers.
//!
//! Exposes:
//! - `wallet_refresh`
//! - `wallet_refresh_async`
//! - `wallet_sync_status`

#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_return)]
#![allow(clippy::let_and_return)]
#![allow(clippy::type_complexity)]

use crate::support::*;

use core::ffi::{c_char, c_int};
use std::{
    collections::{HashMap, VecDeque},
    ffi::{CStr, CString},
    time::Instant,
};

// External types used by refresh.
use monero_interface::ScannableBlock;
use monero_wallet::{transaction::Transaction, Scanner};

// scanner micro-profiler is feature-gated by monero-wallet.
#[cfg(feature = "scanner-microprof")]
use monero_wallet::scanner_microprof_snapshot;

// Bring crate-local alias into scope for prefetch JoinHandle result typing.
use crate::RpcError;

#[no_mangle]
pub extern "C" fn wallet_refresh(
    wallet_id: *const c_char,
    node_url: *const c_char,
    out_last_scanned: *mut u64,
) -> c_int {
    clear_last_error();

    if wallet_id.is_null() {
        return record_error(-11, "wallet_refresh: wallet_id pointer was null");
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => return record_error(-11, "wallet_refresh: wallet_id contained invalid UTF-8"),
    };

    if id.is_empty() {
        return record_error(-14, "wallet_refresh: wallet_id was empty");
    }

    // If cancellation was requested before we even start, abort immediately.
    if refresh_cancelled_for_wallet(id) {
        return record_error(-30, "wallet_refresh: cancelled");
    }

    // Install panic hook once per process for better crash diagnostics.
    let _ = &*PANIC_HOOK_INSTALLED;

    // Clear any stale cancellation request once we have decided to start.
    set_refresh_cancel_for_wallet(id, false);

    // Stage logging to diagnose early refresh termination
    println!("🧭 wallet_refresh stage=init wallet_id={}", id);

    // Build/runtime sanity logs (once per process).
    static BUILD_INFO_LOGGED: std::sync::Once = std::sync::Once::new();
    BUILD_INFO_LOGGED.call_once(|| {
        println!(
            "🧭 walletcore_build target_os={} target_arch={} compile_time_generators={} scanner_microprof_feature={}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cfg!(feature = "compile-time-generators"),
            cfg!(feature = "scanner-microprof")
        );
    });

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

    // Refresh entry stamp
    let env_par = std::env::var("WALLETCORE_SCAN_PAR")
        .ok()
        .unwrap_or_else(|| "(unset)".to_string());
    let env_batch = std::env::var("WALLETCORE_SCAN_BATCH")
        .ok()
        .unwrap_or_else(|| "(unset)".to_string());
    let env_bulk_fetch = std::env::var("WALLETCORE_BULK_FETCH")
        .ok()
        .unwrap_or_else(|| "(unset)".to_string());
    let env_bulk_mode = std::env::var("WALLETCORE_BULK_MODE")
        .ok()
        .unwrap_or_else(|| "(default=wallet2)".to_string());
    let env_bulk_fetch_batch = std::env::var("WALLETCORE_BULK_FETCH_BATCH")
        .ok()
        .unwrap_or_else(|| "(default=200)".to_string());

    print!(
        "🧩 walletcore refresh entry: version={} build={} wallet_id={} node_url={} env{{scan_par={} scan_batch={} bulk_fetch={} bulk_mode={} bulk_fetch_batch={}}}\n",
        WALLETCORE_LOG_VERSION,
        build_stamp(),
        id,
        base_url,
        env_par,
        env_batch,
        env_bulk_fetch,
        env_bulk_mode,
        env_bulk_fetch_batch
    );
    println!(
        "🧭 wallet_refresh stage=after_entry_stamp wallet_id={} node_url={}",
        id, base_url
    );

    // Snapshot
    let snapshot = {
        let map = WALLET_STORE.lock().expect("wallet store poisoned");
        match map.get(id) {
            Some(state) => state.clone(),
            None => {
                return record_error(-13, format!("wallet_refresh: wallet '{id}' not registered"))
            }
        }
    };

    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=snapshot_loaded wallet_id={} network={:?}",
            id, snapshot.network
        ),
    );

    // Refresh-level timing summary accumulators
    let refresh_t0 = Instant::now();
    let mut refresh_scan_ms_total: u128 = 0;
    let mut refresh_persist_ms_total: u128 = 0;
    let mut refresh_blocks_total: usize = 0;
    let mut refresh_outputs_added_total: usize = 0;
    let mut refresh_batches_total: usize = 0;

    let mut persist_span_start: Option<Instant> = None;

    let refresh_telemetry_enabled: bool = std::env::var("WALLETCORE_REFRESH_TELEMETRY")
        .ok()
        .and_then(|s| s.parse::<u8>().ok())
        .map(|v| v != 0)
        .unwrap_or(false);

    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=after_entry_stamp wallet_id={} node_url={}",
            id, base_url
        ),
    );

    // Connect daemon clients
    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=daemon_connect_start wallet_id={}",
            id
        ),
    );

    let rpc_client: RpcClient = match TOKIO_RUNTIME.block_on(
        monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()),
    ) {
        Ok(d) => d,
        Err(e) => {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧭 wallet_refresh stage=daemon_connect_error wallet_id={} err={}",
                    id, e
                ),
            );
            return record_error(
                -16,
                format!("wallet_refresh: failed to connect daemon '{base_url}': {e}"),
            );
        }
    };

    let prefetch_rpc_client: RpcClient = match TOKIO_RUNTIME.block_on(
        monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()),
    ) {
        Ok(d) => d,
        Err(e) => {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧭 wallet_refresh stage=daemon_connect_error wallet_id={} err={}",
                    id, e
                ),
            );
            return record_error(
                -16,
                format!("wallet_refresh: failed to connect daemon (prefetch) '{base_url}': {e}"),
            );
        }
    };

    walletcore_log_line(
        id,
        snapshot.network,
        &format!("🧭 wallet_refresh stage=daemon_connect_ok wallet_id={}", id),
    );

    // Daemon height
    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=daemon_height_start wallet_id={}",
            id
        ),
    );

    let daemon_height = match TOKIO_RUNTIME.block_on(rpc_client.latest_block_number()) {
        Ok(n) => {
            let h = n.saturating_add(1) as u64;
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧭 wallet_refresh stage=daemon_height_ok wallet_id={} height={}",
                    id, h
                ),
            );
            h
        }
        Err(e) => {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧭 wallet_refresh stage=daemon_height_error wallet_id={} err={}",
                    id, e
                ),
            );
            return record_error(
                -16,
                format!("wallet_refresh: failed to query daemon height '{base_url}': {e}"),
            );
        }
    };

    let upstream_block_batch: u64 = std::env::var("WALLETCORE_UPSTREAM_BLOCK_BATCH")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(25)
        .clamp(1, 500);

    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=upstream_batch_config wallet_id={} upstream_block_batch={}",
            id, upstream_block_batch
        ),
    );

    let daemon = DaemonStatus {
        height: daemon_height,
        top_block_timestamp: 0,
    };

    // Keys + scanner
    let master = match master_keys_from_mnemonic_str(&snapshot.mnemonic) {
        Ok(keys) => keys,
        Err(code) => {
            return record_error(
                code,
                format!("wallet_refresh: unable to parse mnemonic ({code})"),
            )
        }
    };
    let view_pair = match master.to_view_pair() {
        Ok(pair) => pair,
        Err(code) => {
            return record_error(
                code,
                format!("wallet_refresh: failed to construct view pair ({code})"),
            )
        }
    };

    let mut scanner = Scanner::new(view_pair.clone());
    let gap_limit = snapshot.gap_limit.max(1);

    let account_gap: u32 = std::env::var("WALLETCORE_ACCOUNT_GAP")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|v| v.max(1))
        .unwrap_or(1);

    // Fingerprints + derived address logs
    let spend_scalar_bytes = master.spend_scalar.to_bytes();
    let view_scalar_bytes = master.view_scalar_dalek.to_bytes();
    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🔐 wallet_fingerprint wallet_id={} spend_scalar_fpr={} view_scalar_fpr={} entropy_fpr={}",
            id,
            fingerprint32("spend_scalar", &spend_scalar_bytes),
            fingerprint32("view_scalar", &view_scalar_bytes),
            fingerprint32("entropy", master.entropy.as_ref()),
        ),
    );

    let derived_primary_address = derive_address_string(&master, 0, 0, snapshot.network);
    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🏠 derived_primary_address wallet_id={} address={}",
            id, derived_primary_address
        ),
    );

    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 scanner_subaddress_plan wallet_id={} account_gap={} gap_limit={} majors=[0..{}) minors=[0..={}]",
            id, account_gap, gap_limit, account_gap, gap_limit
        ),
    );

    let mut registered: u64 = 0;
    let mut failed: u64 = 0;
    let mut first_failed: Option<(u32, u32)> = None;
    let mut last_failed: Option<(u32, u32)> = None;

    for major in 0..account_gap {
        for minor in 1..=gap_limit {
            if let Some(idx) = SubaddressIndex::new(major, minor) {
                scanner.register_subaddress(idx);
                registered = registered.saturating_add(1);
            } else {
                failed = failed.saturating_add(1);
                first_failed = first_failed.or(Some((major, minor)));
                last_failed = Some((major, minor));
            }
        }
    }

    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 scanner_subaddress_registered wallet_id={} registered_count={} failed_count={} first_failed={:?} last_failed={:?}",
            id, registered, failed, first_failed, last_failed
        ),
    );

    // Debug controls
    let debug_txid = walletcore_debug_target_txid();
    let debug_height = walletcore_debug_target_height();
    let debug_height_window = walletcore_debug_target_window();

    // Working state
    let mut working_outputs = snapshot.tracked_outputs.clone();
    let mut seen_outpoints = snapshot.seen_outpoints.clone();
    let mut scan_cursor = snapshot.last_scanned.max(snapshot.restore_height);

    update_scan_progress(
        id,
        scan_cursor.min(daemon.height),
        daemon.height,
        daemon.top_block_timestamp,
        snapshot.restore_height,
    );

    // Perf logging controls
    let log_perf: bool = std::env::var("WALLETCORE_SCAN_LOG")
        .ok()
        .map(|s| s != "0")
        .unwrap_or(false);
    let overall_start: Option<Instant> = if log_perf { Some(Instant::now()) } else { None };
    let initial_outputs: usize = working_outputs.len();

    // (Legacy env plumbing retained even though parallel path is currently disabled)
    let _par: usize = std::env::var("WALLETCORE_SCAN_PAR")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let _batch: usize = std::env::var("WALLETCORE_SCAN_BATCH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(200);
    let _bulk_rpc: bool = std::env::var("WALLETCORE_BULK_RPC")
        .ok()
        .map(|s| s != "0")
        .unwrap_or(true);

    let bulk_fetch_mode = bulk_fetch_mode_from_env();
    let bulk_fetch_batch = bulk_fetch_batch_from_env();

    print!(
        "🧱 bulk-fetch mode resolved: requested={} batch={} (pre-clearnet-gating)\n",
        bulk_mode_str(bulk_fetch_mode),
        bulk_fetch_batch
    );

    if scan_cursor < daemon.height {
        // Sequential scan: upstream daemon RPC + pipelined fetch
        let prefetch_depth: usize = std::env::var("WALLETCORE_PREFETCH_DEPTH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 2);

        let mut next_scannables_q: VecDeque<(u64, u64, Vec<ScannableBlock>)> = VecDeque::new();
        let mut prefetch_in_flight: VecDeque<
            tokio::task::JoinHandle<(u64, u64, u128, Result<Vec<ScannableBlock>, RpcError>)>,
        > = VecDeque::new();

        let prefetch_rpc_client = std::sync::Arc::new(prefetch_rpc_client);

        while scan_cursor < daemon.height {
            if refresh_cancelled_for_wallet(id) {
                if let Some(p0) = persist_span_start.take() {
                    refresh_persist_ms_total =
                        refresh_persist_ms_total.saturating_add(p0.elapsed().as_millis());
                }

                let total_ms = refresh_t0.elapsed().as_millis();
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "📈 wallet_refresh summary wallet_id={} status=cancelled total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} persist_ms_total={}",
                        id,
                        total_ms,
                        refresh_batches_total,
                        refresh_blocks_total,
                        refresh_outputs_added_total,
                        refresh_scan_ms_total,
                        refresh_persist_ms_total
                    ),
                );
                return record_error(-30, "wallet_refresh: cancelled");
            }

            let end_exclusive = core::cmp::min(
                daemon.height,
                scan_cursor.saturating_add(upstream_block_batch),
            );
            if end_exclusive <= scan_cursor {
                break;
            }

            let start_bn_u64 = scan_cursor;
            let end_bn_inclusive_u64 = end_exclusive.saturating_sub(1);

            let start_bn = match usize::try_from(start_bn_u64) {
                Ok(v) => v,
                Err(_) => {
                    return record_error(-16, "wallet_refresh: block number conversion overflow")
                }
            };
            let end_bn_inclusive = match usize::try_from(end_bn_inclusive_u64) {
                Ok(v) => v,
                Err(_) => {
                    return record_error(-16, "wallet_refresh: block number conversion overflow")
                }
            };

            if let Some(p0) = persist_span_start.take() {
                refresh_persist_ms_total =
                    refresh_persist_ms_total.saturating_add(p0.elapsed().as_millis());
            }

            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_start wallet_id={} range={}..={}",
                    id, start_bn, end_bn_inclusive
                ),
            );

            let scannables: Vec<ScannableBlock> = if let Some((pf_start, pf_end, pf_vec)) =
                next_scannables_q.pop_front()
            {
                if pf_start == start_bn_u64 && pf_end == end_bn_inclusive_u64 {
                    pf_vec
                } else {
                    // Prefetch mismatch; fetch synchronously.
                    let fetch_t0 = Instant::now();
                    match TOKIO_RUNTIME.block_on(
                        rpc_client.contiguous_scannable_blocks(start_bn..=end_bn_inclusive),
                    ) {
                        Ok(v) => {
                            let fetch_ms = fetch_t0.elapsed().as_millis();
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_ms={} blocks_per_s={:.2}",
                                    id,
                                    v.len(),
                                    fetch_ms,
                                    if fetch_ms > 0 {
                                        (v.len() as f64) / (fetch_ms as f64 / 1000.0)
                                    } else {
                                        0.0
                                    }
                                ),
                            );
                            v
                        }
                        Err(err) => {
                            let fetch_ms = fetch_t0.elapsed().as_millis();
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                    id, fetch_ms, err
                                ),
                            );
                            return record_error(
                                -16,
                                format!(
                                    "wallet_refresh: contiguous_scannable_blocks failed at heights {}..{}: {}",
                                    scan_cursor,
                                    end_exclusive.saturating_sub(1),
                                    err
                                ),
                            );
                        }
                    }
                }
            } else {
                let fetch_t0 = Instant::now();
                match TOKIO_RUNTIME
                    .block_on(rpc_client.contiguous_scannable_blocks(start_bn..=end_bn_inclusive))
                {
                    Ok(v) => {
                        let fetch_ms = fetch_t0.elapsed().as_millis();
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_ms={} blocks_per_s={:.2}",
                                id,
                                v.len(),
                                fetch_ms,
                                if fetch_ms > 0 {
                                    (v.len() as f64) / (fetch_ms as f64 / 1000.0)
                                } else {
                                    0.0
                                }
                            ),
                        );
                        v
                    }
                    Err(err) => {
                        let fetch_ms = fetch_t0.elapsed().as_millis();
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                id, fetch_ms, err
                            ),
                        );
                        return record_error(
                            -16,
                            format!(
                                "wallet_refresh: contiguous_scannable_blocks failed at heights {}..{}: {}",
                                scan_cursor,
                                end_exclusive.saturating_sub(1),
                                err
                            ),
                        );
                    }
                }
            };

            if scannables.is_empty() {
                return record_error(
                    -16,
                    format!(
                        "wallet_refresh: contiguous_scannable_blocks returned 0 blocks for heights {}..{}",
                        scan_cursor,
                        end_exclusive.saturating_sub(1)
                    ),
                );
            }

            // Ensure prefetch depth.
            let mut cursor_for_prefetch = end_exclusive;
            for _ in next_scannables_q.iter() {
                cursor_for_prefetch = cursor_for_prefetch.saturating_add(upstream_block_batch);
            }
            for _ in prefetch_in_flight.iter() {
                cursor_for_prefetch = cursor_for_prefetch.saturating_add(upstream_block_batch);
            }

            while prefetch_in_flight.len() + next_scannables_q.len() < prefetch_depth {
                let next_start = cursor_for_prefetch;
                let next_end_exclusive = core::cmp::min(
                    daemon.height,
                    next_start.saturating_add(upstream_block_batch),
                );
                if next_end_exclusive <= next_start {
                    break;
                }
                let next_end_inclusive = next_end_exclusive.saturating_sub(1);

                let next_start_bn = match usize::try_from(next_start) {
                    Ok(v) => v,
                    Err(_) => {
                        return record_error(
                            -16,
                            "wallet_refresh: block number conversion overflow",
                        )
                    }
                };
                let next_end_bn = match usize::try_from(next_end_inclusive) {
                    Ok(v) => v,
                    Err(_) => {
                        return record_error(
                            -16,
                            "wallet_refresh: block number conversion overflow",
                        )
                    }
                };

                let prefetch_client = prefetch_rpc_client.clone();
                let handle = TOKIO_RUNTIME.spawn(async move {
                    let t0 = Instant::now();
                    let res = prefetch_client
                        .contiguous_scannable_blocks(next_start_bn..=next_end_bn)
                        .await;
                    let prefetch_ms = t0.elapsed().as_millis();
                    (next_start, next_end_inclusive, prefetch_ms, res)
                });
                prefetch_in_flight.push_back(handle);

                cursor_for_prefetch = next_end_exclusive;
            }

            // ---- Scan batch ----
            let scan_t0 = Instant::now();
            let mut outputs_added_in_batch: usize = 0;
            let blocks_in_batch: usize = scannables.len();

            // per-batch scannable completeness stats (optional)
            let mut batch_txs_total: usize = 0;
            let mut batch_txs_v1: usize = 0;
            let mut batch_txs_v2: usize = 0;
            let mut batch_txs_v2_proofs_some: usize = 0;
            let mut batch_txs_v2_proofs_none: usize = 0;
            let mut batch_txs_extra_nonempty: usize = 0;
            let mut batch_txs_outputs_nonzero: usize = 0;
            let mut batch_outputs_total: usize = 0;

            let mut th = scan_cursor;

            // Read watch controls once per batch.
            let watch_ki = watch_key_image_from_env();
            let watch_txid = watch_txid_from_env();

            for scannable in scannables {
                if refresh_cancelled_for_wallet(id) {
                    return record_error(-30, "wallet_refresh: cancelled");
                }

                // Recent hash history
                {
                    let block_hash = scannable.block.hash();
                    if let Ok(mut map) = WALLET_STORE.lock() {
                        if let Some(state) = map.get_mut(id) {
                            push_recent_block_hash(state, th, block_hash);
                        }
                    }
                }

                let miner_hash = scannable.block.miner_transaction().hash();

                // Spend detection: build KI map from current working_outputs
                let mut key_image_to_output_index: HashMap<[u8; 32], usize> = HashMap::new();
                for (i, o) in working_outputs.iter().enumerate() {
                    if o.key_image != [0u8; 32] {
                        key_image_to_output_index.entry(o.key_image).or_insert(i);
                    }
                }

                // Aggregate gross spent per spending txid in this block (debug)
                let mut spent_inputs_by_txid: HashMap<[u8; 32], u64> = HashMap::new();

                // Miner tx inputs
                {
                    let tx = scannable.block.miner_transaction();
                    for input in &tx.prefix().inputs {
                        if let monero_wallet::transaction::Input::ToKey { key_image, .. } = input {
                            let ki_bytes = key_image.to_bytes();
                            if let Some(out_idx) = key_image_to_output_index.get(&ki_bytes).copied()
                            {
                                let spent_amount = working_outputs[out_idx].amount;
                                working_outputs[out_idx].spent = true;

                                let spend_txid = tx.hash();
                                working_outputs[out_idx].spending_txid = Some(spend_txid);
                                let e = spent_inputs_by_txid.entry(spend_txid).or_insert(0);
                                *e = e.saturating_add(spent_amount);

                                walletcore_log_line(
                                    id,
                                    snapshot.network,
                                    &format!(
                                        "🧾 spend_detected wallet_id={} spending_txid={} key_image={} spent_amount_piconero={} source_out_txid={} source_out_index={}",
                                        id,
                                        hex_dump_prefix(&spend_txid, 32),
                                        hex_dump_prefix(&ki_bytes, 32),
                                        spent_amount,
                                        hex_dump_prefix(&working_outputs[out_idx].tx_hash, 32),
                                        working_outputs[out_idx].index_in_tx
                                    ),
                                );
                            }
                        }
                    }
                }

                // Non-miner tx inputs
                for (tx_i, tx_ref) in scannable.transactions.iter().enumerate() {
                    let spend_txid_opt: Option<[u8; 32]> =
                        scannable.block.transactions.get(tx_i).copied();

                    if let (Some(watch), Some(spend_txid)) = (watch_txid, spend_txid_opt) {
                        if watch == spend_txid {
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🕵️ watch_spend_txid_seen wallet_id={} height={} spending_txid={}",
                                    id,
                                    th,
                                    hex_dump_prefix(&spend_txid, 32)
                                ),
                            );
                        }
                    }

                    for input in &tx_ref.prefix().inputs {
                        if let monero_wallet::transaction::Input::ToKey { key_image, .. } = input {
                            let ki_bytes = key_image.to_bytes();

                            if let Some(watch) = watch_ki {
                                if watch == ki_bytes {
                                    let matched =
                                        key_image_to_output_index.get(&ki_bytes).copied().is_some();
                                    walletcore_log_line(
                                        id,
                                        snapshot.network,
                                        &format!(
                                            "🕵️ watch_key_image_seen wallet_id={} height={} spending_txid={} key_image={} matched_owned_output={}",
                                            id,
                                            th,
                                            match spend_txid_opt {
                                                Some(txid) => hex_dump_prefix(&txid, 32),
                                                None => "(unknown)".to_string(),
                                            },
                                            hex_dump_prefix(&ki_bytes, 32),
                                            matched
                                        ),
                                    );
                                }
                            }

                            if let Some(out_idx) = key_image_to_output_index.get(&ki_bytes).copied()
                            {
                                let spent_amount = working_outputs[out_idx].amount;
                                working_outputs[out_idx].spent = true;

                                if let Some(spend_txid) = spend_txid_opt {
                                    working_outputs[out_idx].spending_txid = Some(spend_txid);
                                    let e = spent_inputs_by_txid.entry(spend_txid).or_insert(0);
                                    *e = e.saturating_add(spent_amount);

                                    walletcore_log_line(
                                        id,
                                        snapshot.network,
                                        &format!(
                                            "🧾 spend_detected wallet_id={} spending_txid={} key_image={} spent_amount_piconero={} source_out_txid={} source_out_index={}",
                                            id,
                                            hex_dump_prefix(&spend_txid, 32),
                                            hex_dump_prefix(&ki_bytes, 32),
                                            spent_amount,
                                            hex_dump_prefix(&working_outputs[out_idx].tx_hash, 32),
                                            working_outputs[out_idx].index_in_tx
                                        ),
                                    );
                                } else {
                                    working_outputs[out_idx].spending_txid = None;
                                    walletcore_log_line(
                                        id,
                                        snapshot.network,
                                        &format!(
                                            "🧾 spend_detected wallet_id={} spending_txid=(unknown) key_image={} spent_amount_piconero={} source_out_txid={} source_out_index={}",
                                            id,
                                            hex_dump_prefix(&ki_bytes, 32),
                                            spent_amount,
                                            hex_dump_prefix(&working_outputs[out_idx].tx_hash, 32),
                                            working_outputs[out_idx].index_in_tx
                                        ),
                                    );
                                }
                            }
                        }
                    }
                }

                // Spend summary throttling
                let spend_log_every_n_blocks = spend_log_every_n_blocks_from_env();
                if spend_log_every_n_blocks > 0 && (th % spend_log_every_n_blocks == 0) {
                    walletcore_log_line(
                        id,
                        snapshot.network,
                        &format!(
                            "🧾 spend_detection_summary wallet_id={} height={} distinct_spend_txs={}",
                            id,
                            th,
                            spent_inputs_by_txid.len(),
                        ),
                    );
                } else if !spent_inputs_by_txid.is_empty() {
                    walletcore_log_line(
                        id,
                        snapshot.network,
                        &format!(
                            "🧾 spend_detection_summary wallet_id={} height={} distinct_spend_txs={}",
                            id,
                            th,
                            spent_inputs_by_txid.len(),
                        ),
                    );
                }

                // Transaction completeness stats
                {
                    batch_txs_total = batch_txs_total.saturating_add(1);
                    match scannable.block.miner_transaction() {
                        Transaction::V1 { .. } => batch_txs_v1 = batch_txs_v1.saturating_add(1),
                        Transaction::V2 { proofs, .. } => {
                            batch_txs_v2 = batch_txs_v2.saturating_add(1);
                            if proofs.is_some() {
                                batch_txs_v2_proofs_some =
                                    batch_txs_v2_proofs_some.saturating_add(1);
                            } else {
                                batch_txs_v2_proofs_none =
                                    batch_txs_v2_proofs_none.saturating_add(1);
                            }
                        }
                    }
                    let miner_prefix = scannable.block.miner_transaction().prefix();
                    if !miner_prefix.extra.is_empty() {
                        batch_txs_extra_nonempty = batch_txs_extra_nonempty.saturating_add(1);
                    }
                    if !miner_prefix.outputs.is_empty() {
                        batch_txs_outputs_nonzero = batch_txs_outputs_nonzero.saturating_add(1);
                        batch_outputs_total =
                            batch_outputs_total.saturating_add(miner_prefix.outputs.len());
                    }
                }
                for tx_ref in &scannable.transactions {
                    batch_txs_total = batch_txs_total.saturating_add(1);
                    match tx_ref {
                        Transaction::V1 { .. } => batch_txs_v1 = batch_txs_v1.saturating_add(1),
                        Transaction::V2 { proofs, .. } => {
                            batch_txs_v2 = batch_txs_v2.saturating_add(1);
                            if proofs.is_some() {
                                batch_txs_v2_proofs_some =
                                    batch_txs_v2_proofs_some.saturating_add(1);
                            } else {
                                batch_txs_v2_proofs_none =
                                    batch_txs_v2_proofs_none.saturating_add(1);
                            }
                        }
                    }
                    let prefix = tx_ref.prefix();
                    if !prefix.extra.is_empty() {
                        batch_txs_extra_nonempty = batch_txs_extra_nonempty.saturating_add(1);
                    }
                    if !prefix.outputs.is_empty() {
                        batch_txs_outputs_nonzero = batch_txs_outputs_nonzero.saturating_add(1);
                        batch_outputs_total =
                            batch_outputs_total.saturating_add(prefix.outputs.len());
                    }
                }

                let dbg_this_height = debug_height
                    .map(|h| {
                        let w = debug_height_window;
                        th >= h.saturating_sub(w) && th <= h.saturating_add(w)
                    })
                    .unwrap_or(false);

                let should_log_this_height = if debug_height.is_some() {
                    dbg_this_height
                } else {
                    debug_txid.is_some()
                };

                if should_log_this_height {
                    if dbg_this_height {
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧪 debug_target height={} txs_in_block={} (non_miner) miner_tx_hash={}",
                                th,
                                scannable.transactions.len(),
                                hex_dump_prefix(&scannable.block.miner_transaction().hash(), 32)
                            ),
                        );
                    }
                    if let Some(target) = debug_txid {
                        let mut contains = false;
                        for h in &scannable.block.transactions {
                            if *h == target {
                                contains = true;
                                break;
                            }
                        }
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧪 debug_target_txid height={} target_txid={} block_contains={}",
                                th,
                                hex_dump_prefix(&target, 32),
                                contains
                            ),
                        );
                    }
                }

                let outputs = match scanner.scan(scannable) {
                    Ok(result) => result.ignore_additional_timelock(),
                    Err(_) => {
                        return record_error(
                            -16,
                            format!("wallet_refresh: scanner failed at height {}", th),
                        );
                    }
                };

                for output in outputs {
                    let key = (output.transaction(), output.index_in_transaction());
                    if !seen_outpoints.insert(key) {
                        continue;
                    }

                    if let Some(target) = debug_txid {
                        if output.transaction() == target {
                            let (maj, min) = output
                                .subaddress()
                                .map(|idx| (idx.account(), idx.address()))
                                .unwrap_or((0, 0));
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🧪 debug_target_match height={} txid={} out_index={} subaddr=({}, {}) amount_piconero={}",
                                    th,
                                    hex_dump_prefix(&target, 32),
                                    output.index_in_transaction(),
                                    maj,
                                    min,
                                    output.commitment().amount
                                ),
                            );
                        }
                    }

                    let (major, minor) = output
                        .subaddress()
                        .map(|idx| (idx.account(), idx.address()))
                        .unwrap_or((0, 0));

                    // Compute key image for this owned output so we can detect on-chain spends.
                    let key_image_bytes: [u8; 32] = {
                        let ko_bytes: [u8; 32] = <[u8; 32]>::from(output.key_offset());
                        let ko_dalek = curve25519_dalek::Scalar::from_canonical_bytes(ko_bytes)
                            .into_option()
                            .unwrap_or(curve25519_dalek::Scalar::ZERO);

                        let a = master.spend_scalar;

                        let m_dalek = if major == 0 && minor == 0 {
                            curve25519_dalek::Scalar::ZERO
                        } else {
                            let mut data = Vec::with_capacity(8 + 32 + 4 + 4);
                            data.extend_from_slice(b"SubAddr\0");
                            data.extend_from_slice(&<[u8; 32]>::from(master.view_scalar_ed));
                            data.extend_from_slice(&major.to_le_bytes());
                            data.extend_from_slice(&minor.to_le_bytes());
                            let m_ed: monero_wallet::ed25519::Scalar =
                                monero_wallet::ed25519::Scalar::hash(&data);
                            let m_d: curve25519_dalek::Scalar = m_ed.into();
                            m_d
                        };

                        let x = a + ko_dalek + m_dalek;

                        let p = output.key();
                        let p_bytes = p.compress().to_bytes();
                        let hp_p = monero_wallet::ed25519::Point::biased_hash(p_bytes);
                        let hp_p_bytes = hp_p.compress().to_bytes();

                        use curve25519_dalek::traits::Identity;
                        let hp_p_dalek = curve25519_dalek::edwards::CompressedEdwardsY(hp_p_bytes)
                            .decompress()
                            .unwrap_or(curve25519_dalek::EdwardsPoint::identity());

                        let ki = hp_p_dalek * x;
                        ki.compress().to_bytes()
                    };

                    working_outputs.push(TrackedOutput {
                        tx_hash: output.transaction(),
                        index_in_tx: output.index_in_transaction(),
                        key_image: key_image_bytes,
                        amount: output.commitment().amount,
                        block_height: th,
                        additional_timelock: output.additional_timelock(),
                        is_coinbase: output.transaction() == miner_hash,
                        subaddress_major: major,
                        subaddress_minor: minor,
                        spent: false,
                        spending_txid: None,
                    });

                    outputs_added_in_batch = outputs_added_in_batch.saturating_add(1);
                }

                th = th.saturating_add(1);
            }

            let scan_ms = scan_t0.elapsed().as_millis();
            refresh_scan_ms_total = refresh_scan_ms_total.saturating_add(scan_ms);
            refresh_blocks_total = refresh_blocks_total.saturating_add(blocks_in_batch);
            refresh_outputs_added_total =
                refresh_outputs_added_total.saturating_add(outputs_added_in_batch);
            refresh_batches_total = refresh_batches_total.saturating_add(1);

            if refresh_telemetry_enabled {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧪 scannable_completeness wallet_id={} range={}..={} blocks={} txs_total={} txs_v1={} txs_v2={} v2_proofs_some={} v2_proofs_none={} txs_extra_nonempty={} txs_outputs_nonzero={} outputs_total={}",
                        id,
                        start_bn,
                        end_bn_inclusive,
                        blocks_in_batch,
                        batch_txs_total,
                        batch_txs_v1,
                        batch_txs_v2,
                        batch_txs_v2_proofs_some,
                        batch_txs_v2_proofs_none,
                        batch_txs_extra_nonempty,
                        batch_txs_outputs_nonzero,
                        batch_outputs_total
                    ),
                );

                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "📊 wallet_refresh batch_stats wallet_id={} range={}..={} blocks={} outputs_added={} scan_ms={} blocks_per_s={:.2} outputs_per_s={:.2}",
                        id,
                        start_bn,
                        end_bn_inclusive,
                        blocks_in_batch,
                        outputs_added_in_batch,
                        scan_ms,
                        if scan_ms > 0 {
                            (blocks_in_batch as f64) / (scan_ms as f64 / 1000.0)
                        } else {
                            0.0
                        },
                        if scan_ms > 0 {
                            (outputs_added_in_batch as f64) / (scan_ms as f64 / 1000.0)
                        } else {
                            0.0
                        }
                    ),
                );
            }

            persist_span_start = Some(Instant::now());

            // Drain prefetch tasks into ready queue (only await enough to keep moving).
            while next_scannables_q.is_empty() {
                let Some(handle) = prefetch_in_flight.pop_front() else {
                    break;
                };

                let join_wait_t0 = Instant::now();
                match TOKIO_RUNTIME.block_on(handle) {
                    Ok((pf_start, pf_end, _pf_ms, Ok(v))) => {
                        let _ = join_wait_t0.elapsed().as_millis();
                        next_scannables_q.push_back((pf_start, pf_end, v));
                    }
                    Ok((_pf_start, _pf_end, _pf_ms, Err(err))) => {
                        let _ = join_wait_t0.elapsed().as_millis();
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} err={}",
                                id, err
                            ),
                        );
                        return record_error(
                            -16,
                            format!(
                                "wallet_refresh: contiguous_scannable_blocks (prefetch) failed: {}",
                                err
                            ),
                        );
                    }
                    Err(join_err) => {
                        let _ = join_wait_t0.elapsed().as_millis();
                        return record_error(
                            -16,
                            format!("wallet_refresh: prefetch task join error: {}", join_err),
                        );
                    }
                }
            }

            scan_cursor = th;
            update_scan_progress(
                id,
                scan_cursor.min(daemon.height),
                daemon.height,
                daemon.top_block_timestamp,
                snapshot.restore_height,
            );
        }

        scan_cursor = daemon.height;
    }

    // Close trailing persist span.
    if let Some(p0) = persist_span_start.take() {
        refresh_persist_ms_total =
            refresh_persist_ms_total.saturating_add(p0.elapsed().as_millis());
    }

    // Final refresh summary
    {
        let total_ms = refresh_t0.elapsed().as_millis();
        let total_ms_u128: u128 = total_ms;

        let other_ms = total_ms_u128
            .saturating_sub(refresh_scan_ms_total)
            .saturating_sub(refresh_persist_ms_total);

        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "📈 wallet_refresh summary wallet_id={} status=ok total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} persist_ms_total={} other_ms={}",
                id,
                total_ms,
                refresh_batches_total,
                refresh_blocks_total,
                refresh_outputs_added_total,
                refresh_scan_ms_total,
                refresh_persist_ms_total,
                other_ms
            ),
        );

        #[cfg(feature = "scanner-microprof")]
        {
            if let Some(mp) = scanner_microprof_snapshot(true) {
                let ns_to_ms = |ns: u64| -> u64 { ns / 1_000_000 };
                let ecdh_mul_us_per_miss = if mp.ecdh_cache_misses > 0 {
                    (mp.ns_ecdh_mul as f64) / (mp.ecdh_cache_misses as f64) / 1_000.0
                } else {
                    0.0
                };
                let scan_us_per_output = if mp.outputs_visited > 0 {
                    (mp.ns_scan_transaction as f64) / (mp.outputs_visited as f64) / 1_000.0
                } else {
                    0.0
                };

                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧬 scanner_microprof wallet_id={} blocks={} txs_scanned={} outputs_visited={} ecdh_derivations={} ecdh_cache_hits={} ecdh_cache_misses={} viewtag_mismatch={} commitment_verify_attempts={} commitment_verify_fail={} outputs_matched={} extra_parse_fail={} tx_keys_missing={} ms_block_setup={} ms_scan_transaction={} ms_commitment_verify={} ms_ecdh_mul={} ms_ecdh_cache_lookup_hit={} ms_ecdh_cache_lookup_miss={} ms_output_derivations={} ms_subaddress_lookup={} ecdh_mul_us_per_miss={:.3} scan_us_per_output={:.3}",
                        id,
                        mp.blocks,
                        mp.txs_scanned,
                        mp.outputs_visited,
                        mp.ecdh_derivations,
                        mp.ecdh_cache_hits,
                        mp.ecdh_cache_misses,
                        mp.viewtag_mismatch,
                        mp.commitment_verify_attempts,
                        mp.commitment_verify_fail,
                        mp.outputs_matched,
                        mp.extra_parse_fail,
                        mp.tx_keys_missing,
                        ns_to_ms(mp.ns_block_setup),
                        ns_to_ms(mp.ns_scan_transaction),
                        ns_to_ms(mp.ns_commitment_verify),
                        ns_to_ms(mp.ns_ecdh_mul),
                        ns_to_ms(mp.ns_ecdh_cache_lookup_hit),
                        ns_to_ms(mp.ns_ecdh_cache_lookup_miss),
                        ns_to_ms(mp.ns_output_derivations),
                        ns_to_ms(mp.ns_subaddress_lookup),
                        ecdh_mul_us_per_miss,
                        scan_us_per_output
                    ),
                );
            }
        }
    }

    // Overall perf log
    if log_perf {
        let blocks_scanned =
            scan_cursor.saturating_sub(snapshot.last_scanned.max(snapshot.restore_height));
        let new_outputs = working_outputs.len().saturating_sub(initial_outputs);
        if let Some(start) = overall_start {
            let secs = start.elapsed().as_secs_f64();
            eprintln!(
                "wallet_refresh: scanned {} blocks; new_outputs={}; elapsed={:.3}s",
                blocks_scanned, new_outputs, secs
            );
        }
    }

    // Update stable transfer ledger based on observed outputs + spends.
    let mut computed_ledger: HashMap<String, LedgerEntry> = snapshot.tx_ledger.clone();

    // 1) Outgoing detection from spends (gross)
    {
        let mut gross_spent_by_spend_txid: HashMap<String, u64> = HashMap::new();

        for o in &working_outputs {
            if o.spent {
                if let Some(spend_txid_bytes) = o.spending_txid {
                    let spend_txid = hex_lowercase(&spend_txid_bytes);
                    let e = gross_spent_by_spend_txid.entry(spend_txid).or_insert(0);
                    *e = e.saturating_add(o.amount);
                }
            }
        }

        for (spend_txid, gross_amount) in gross_spent_by_spend_txid {
            match computed_ledger.get_mut(&spend_txid) {
                Some(entry) => {
                    if entry.direction == "out" {
                        entry.amount = entry.amount.max(gross_amount);
                        if entry.fee.is_none() {
                            entry.fee = None;
                        }
                    }
                }
                None => {
                    computed_ledger.insert(
                        spend_txid.clone(),
                        LedgerEntry {
                            txid: spend_txid,
                            direction: "out".to_string(),
                            amount: gross_amount,
                            fee: None,
                            height: None,
                            timestamp: None,
                            is_pending: false,
                            is_coinbase: false,
                        },
                    );
                }
            }
        }
    }

    // 2) Incoming aggregation and opportunistic outgoing confirmation.
    for o in &working_outputs {
        let txid = hex_lowercase(&o.tx_hash);

        if let Some(entry) = computed_ledger.get_mut(&txid) {
            if entry.direction == "out" && entry.is_pending {
                entry.is_pending = false;
                if entry.height.is_none() || entry.height == Some(0) {
                    entry.height = if o.block_height == 0 {
                        None
                    } else {
                        Some(o.block_height)
                    };
                }
                if entry.timestamp.is_none() && daemon.top_block_timestamp > 0 {
                    entry.timestamp = Some(daemon.top_block_timestamp);
                }
            }
        }

        match computed_ledger.get_mut(&txid) {
            Some(entry) => {
                if entry.direction == "in" {
                    entry.amount = entry.amount.saturating_add(o.amount);
                    entry.is_coinbase = entry.is_coinbase || o.is_coinbase;
                    if entry.height.is_none() || entry.height == Some(0) {
                        entry.height = if o.block_height == 0 {
                            None
                        } else {
                            Some(o.block_height)
                        };
                    } else if let Some(h) = entry.height {
                        if o.block_height != 0 && o.block_height < h {
                            entry.height = Some(o.block_height);
                        }
                    }
                    if entry.timestamp.is_none() && daemon.top_block_timestamp > 0 {
                        entry.timestamp = Some(daemon.top_block_timestamp);
                    }
                }
            }
            None => {
                computed_ledger.insert(
                    txid.clone(),
                    LedgerEntry {
                        txid,
                        direction: "in".to_string(),
                        amount: o.amount,
                        fee: None,
                        height: if o.block_height == 0 {
                            None
                        } else {
                            Some(o.block_height)
                        },
                        timestamp: if daemon.top_block_timestamp > 0 {
                            Some(daemon.top_block_timestamp)
                        } else {
                            None
                        },
                        is_pending: false,
                        is_coinbase: o.is_coinbase,
                    },
                );
            }
        }
    }

    // Drop pending_outgoing entries that are now confirmed in the ledger.
    let mut pending_outgoing = snapshot.pending_outgoing.clone();
    pending_outgoing.retain(|p| {
        if let Some(entry) = computed_ledger.get(&p.txid) {
            entry.direction == "out" && entry.is_pending
        } else {
            true
        }
    });

    // Balances (NOTE: this matches existing behavior; it includes spent outputs in total as-is).
    // Kept identical to inlined implementation for now.
    let mut total = 0u64;
    let mut unlocked = 0u64;
    for output in &working_outputs {
        total = total.saturating_add(output.amount);
        if output.is_unlocked(daemon.height, daemon.top_block_timestamp) {
            unlocked = unlocked.saturating_add(output.amount);
        }
    }

    // Persist into WALLET_STORE.
    {
        let mut map = WALLET_STORE.lock().expect("wallet store poisoned");
        let Some(state) = map.get_mut(id) else {
            return -13;
        };
        state.last_scanned = scan_cursor.max(state.restore_height);
        state.total = total;
        state.unlocked = unlocked;
        state.chain_height = daemon.height;
        state.chain_time = daemon.top_block_timestamp;
        if daemon.top_block_timestamp > 0 {
            state.last_refresh_timestamp = daemon.top_block_timestamp;
        }
        state.tracked_outputs = working_outputs;
        state.seen_outpoints = seen_outpoints;
        state.tx_ledger = computed_ledger;
        state.pending_outgoing = pending_outgoing;
    }

    if !out_last_scanned.is_null() {
        unsafe {
            *out_last_scanned = scan_cursor.max(snapshot.restore_height);
        }
    }

    clear_last_error();
    0
}

#[no_mangle]
pub extern "C" fn wallet_refresh_async(wallet_id: *const c_char, node_url: *const c_char) -> c_int {
    clear_last_error();

    if wallet_id.is_null() {
        return record_error(-11, "wallet_refresh_async: wallet_id pointer was null");
    }

    let id_str = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            return record_error(
                -10,
                "wallet_refresh_async: wallet_id contained invalid UTF-8",
            )
        }
    };

    if id_str.is_empty() {
        return record_error(-14, "wallet_refresh_async: wallet_id was empty");
    }

    if refresh_cancelled_for_wallet(id_str) {
        return record_error(-30, "wallet_refresh_async: cancelled");
    }

    set_refresh_cancel_for_wallet(id_str, false);

    let id_owned = id_str.to_string();

    let node_owned = if node_url.is_null() {
        None
    } else {
        match unsafe { CStr::from_ptr(node_url) }.to_str() {
            Ok(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                }
            }
            Err(_) => {
                return record_error(
                    -10,
                    "wallet_refresh_async: node_url contained invalid UTF-8",
                )
            }
        }
    };

    std::thread::spawn(move || {
        if let Ok(wallet_cstr) = CString::new(id_owned) {
            let node_cstr = node_owned.and_then(|url| CString::new(url).ok());
            let mut last_scanned: u64 = 0;
            let node_ptr = node_cstr
                .as_ref()
                .map(|c| c.as_ptr())
                .unwrap_or(std::ptr::null::<c_char>());
            let _ = wallet_refresh(
                wallet_cstr.as_ptr(),
                node_ptr,
                &mut last_scanned as *mut u64,
            );
        }
    });

    0
}

#[no_mangle]
pub extern "C" fn wallet_sync_status(
    wallet_id: *const c_char,
    out_chain_height: *mut u64,
    out_chain_time: *mut u64,
    out_last_refresh_timestamp: *mut u64,
    out_last_scanned: *mut u64,
    out_restore_height: *mut u64,
) -> c_int {
    clear_last_error();

    if wallet_id.is_null() {
        return record_error(-11, "wallet_sync_status: wallet_id pointer was null");
    }

    let id = match unsafe { CStr::from_ptr(wallet_id) }.to_str() {
        Ok(s) => s.trim(),
        Err(_) => {
            return record_error(-10, "wallet_sync_status: wallet_id contained invalid UTF-8")
        }
    };

    let map = WALLET_STORE.lock().expect("wallet store poisoned");
    let Some(state) = map.get(id) else {
        return record_error(-13, format!("wallet_sync_status: wallet '{id}' not opened"));
    };

    if !out_chain_height.is_null() {
        unsafe {
            *out_chain_height = state.chain_height;
        }
    }
    if !out_chain_time.is_null() {
        unsafe {
            *out_chain_time = state.chain_time;
        }
    }
    if !out_last_refresh_timestamp.is_null() {
        unsafe {
            *out_last_refresh_timestamp = state.last_refresh_timestamp;
        }
    }
    if !out_last_scanned.is_null() {
        unsafe {
            *out_last_scanned = state.last_scanned;
        }
    }
    if !out_restore_height.is_null() {
        unsafe {
            *out_restore_height = state.restore_height;
        }
    }

    0
}
