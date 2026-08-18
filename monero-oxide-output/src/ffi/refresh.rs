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

#[cfg(target_os = "android")]
mod android_log {
    use std::ffi::CString;

    // Android log priorities from <android/log.h>
    const ANDROID_LOG_INFO: i32 = 4;

    extern "C" {
        fn __android_log_write(prio: i32, tag: *const i8, text: *const i8) -> i32;
    }

    pub fn info(msg: &str) {
        // Best-effort: if CString conversion fails (interior NUL), replace with a safe placeholder.
        let tag =
            CString::new("walletcore").unwrap_or_else(|_| CString::new("walletcore").unwrap());
        let text = CString::new(msg)
            .unwrap_or_else(|_| CString::new("<walletcore log: interior NUL>").unwrap());
        unsafe {
            let _ = __android_log_write(
                ANDROID_LOG_INFO,
                tag.as_ptr() as *const i8,
                text.as_ptr() as *const i8,
            );
        }
    }
}

fn wc_log_line_android_or_stdout(msg: &str) {
    #[cfg(target_os = "android")]
    {
        android_log::info(msg);
        return;
    }
    #[cfg(not(target_os = "android"))]
    {
        println!("{msg}");
    }
}

#[cfg(target_os = "android")]
fn wc_android_force_env_default(key: &str, value: &str) {
    // Best-effort: only set if currently unset/empty.
    // SAFETY: setenv expects NUL-terminated strings; CString enforces that.
    if std::env::var(key)
        .ok()
        .is_some_and(|v| !v.trim().is_empty())
    {
        return;
    }

    let k = std::ffi::CString::new(key)
        .unwrap_or_else(|_| std::ffi::CString::new("<bad-key>").unwrap());
    let v = std::ffi::CString::new(value)
        .unwrap_or_else(|_| std::ffi::CString::new("<bad-value>").unwrap());

    unsafe {
        // int setenv(const char *name, const char *value, int overwrite);
        extern "C" {
            fn setenv(name: *const i8, value: *const i8, overwrite: i32) -> i32;
        }
        let _ = setenv(k.as_ptr() as *const i8, v.as_ptr() as *const i8, 0);
    }
}

// External types used by refresh.
use crate::BlockingRpcTransport;
use monero_interface::{PrunedTransactionWithPrunableHash, ScannableBlock};
use monero_wallet::{
    block::Block as MoneroBlock,
    transaction::{NotPruned, Pruned, Transaction},
    Scanner,
};

// scanner micro-profiler is feature-gated by monero-wallet.
#[cfg(feature = "scanner-microprof")]
use monero_wallet::scanner_microprof_snapshot;

// Bring crate-local alias into scope for prefetch JoinHandle result typing.
use crate::BulkFetchMode;
use crate::RpcError;

// Hard timeout to prevent indefinite hangs when fetching scannable blocks.
// This is intentionally enforced at the walletcore layer so we can always surface a diagnostic.
const CONTIGUOUS_BLOCKS_TIMEOUT_SECS: u64 = 30;

#[derive(serde::Deserialize)]
struct GetTransactionsOutputIndicesResponse {
    txs: Vec<GetTransactionsOutputIndicesEntry>,
}

#[derive(serde::Deserialize)]
struct GetTransactionsOutputIndicesEntry {
    tx_hash: String,
    output_indices: Vec<u64>,
}

fn fetch_output_indexes_via_get_transactions(
    base_url: &str,
    tx_hash: [u8; 32],
) -> Result<Vec<u64>, RpcError> {
    let tx_hash_hex: String = tx_hash.iter().map(|b| format!("{b:02x}")).collect();
    let body = serde_json::json!({
        "txs_hashes": [tx_hash_hex],
        "decode_as_json": false,
        "prune": false,
        "split": false,
    })
    .to_string();

    let response = ureq::post(&format!("{base_url}/get_transactions"))
        .set("Content-Type", "application/json")
        .send_string(&body)
        .map_err(|e| {
            RpcError::InvalidInterface(format!("get_transactions fallback request failed: {e}"))
        })?;

    let mut reader = response.into_reader();
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut reader, &mut bytes).map_err(|e| {
        RpcError::InvalidInterface(format!("get_transactions fallback read failed: {e}"))
    })?;

    let parsed: GetTransactionsOutputIndicesResponse =
        serde_json::from_slice(&bytes).map_err(|e| {
            RpcError::InvalidInterface(format!("get_transactions fallback json decode failed: {e}"))
        })?;

    parsed
        .txs
        .into_iter()
        .find(|tx| tx.tx_hash.eq_ignore_ascii_case(&tx_hash_hex))
        .map(|tx| tx.output_indices)
        .ok_or_else(|| {
            RpcError::InvalidInterface(
                "get_transactions fallback did not return requested tx output indices".to_string(),
            )
        })
}

fn get_output_indexes_from_block_response(
    block_idx: usize,
    miner_tx: &Transaction<Pruned>,
    transactions: &[Transaction<Pruned>],
    block_output_indices: Option<&crate::support::bulk_models::BlockOutputIndices>,
) -> Result<Option<Vec<u64>>, RpcError> {
    let Some(block_output_indices) = block_output_indices else {
        return Ok(None);
    };

    let txs_with_miner = 1usize.saturating_add(transactions.len());
    let tx_entries = &block_output_indices.indices;

    let pair_mode = if tx_entries.len() == txs_with_miner {
        Some(true)
    } else if tx_entries.len() == transactions.len() {
        Some(false)
    } else {
        None
    };

    let Some(include_miner) = pair_mode else {
        return Err(RpcError::InvalidInterface(format!(
            "range get_blocks.bin block[{block_idx}] output_indices had {} tx entries, expected {} (with miner) or {} (without miner)",
            tx_entries.len(),
            txs_with_miner,
            transactions.len()
        )));
    };

    if include_miner {
        for (tx, tx_output_indices) in std::iter::once(miner_tx)
            .chain(transactions.iter())
            .zip(tx_entries.iter())
        {
            if matches!(tx, Transaction::V1 { .. }) || tx.prefix().outputs.is_empty() {
                continue;
            }

            if tx_output_indices.indices.len() != tx.prefix().outputs.len() {
                return Err(RpcError::InvalidInterface(format!(
                    "range get_blocks.bin block[{block_idx}] output_indices count {} did not match tx outputs {}",
                    tx_output_indices.indices.len(),
                    tx.prefix().outputs.len()
                )));
            }

            return Ok(Some(tx_output_indices.indices.clone()));
        }
    } else {
        for (tx, tx_output_indices) in transactions.iter().zip(tx_entries.iter()) {
            if matches!(tx, Transaction::V1 { .. }) || tx.prefix().outputs.is_empty() {
                continue;
            }

            if tx_output_indices.indices.len() != tx.prefix().outputs.len() {
                return Err(RpcError::InvalidInterface(format!(
                    "range get_blocks.bin block[{block_idx}] output_indices count {} did not match tx outputs {}",
                    tx_output_indices.indices.len(),
                    tx.prefix().outputs.len()
                )));
            }

            return Ok(Some(tx_output_indices.indices.clone()));
        }
    }

    Ok(None)
}

#[derive(Debug)]
enum RangeFetchError {
    Rpc(RpcError),
    RetryUnpruned(RpcError),
}

impl From<RpcError> for RangeFetchError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

fn decode_range_transaction(
    blob: &[u8],
    prunable_hash: Option<[u8; 32]>,
    expected_hash: [u8; 32],
    block_idx: usize,
    tx_idx: usize,
) -> Result<Transaction<Pruned>, RangeFetchError> {
    let mut pruned_reader = blob;
    let pruned_decode_detail = match Transaction::<Pruned>::read(&mut pruned_reader) {
        Ok(tx_pruned) if pruned_reader.is_empty() => {
            let usable_prunable_hash = match &tx_pruned {
                Transaction::V1 { .. } => None,
                Transaction::V2 { proofs, .. } => {
                    if proofs.is_some() && prunable_hash == Some([0; 32]) {
                        return Err(RangeFetchError::RetryUnpruned(
                            RpcError::InvalidInterface(format!(
                                "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] pruned response had an uninitialized prunable_hash"
                            )),
                        ));
                    }
                    prunable_hash
                }
            };

            let Some(tx_with_hash) =
                PrunedTransactionWithPrunableHash::new(tx_pruned, usable_prunable_hash)
            else {
                return Err(RangeFetchError::RetryUnpruned(
                    RpcError::InvalidInterface(format!(
                        "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] pruned response was missing a usable prunable_hash"
                    )),
                ));
            };

            return tx_with_hash.verify_as_possible(expected_hash).map_err(|_| {
                RangeFetchError::Rpc(RpcError::InvalidInterface(format!(
                    "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] pruned transaction hash mismatch"
                )))
            });
        }
        Ok(_) => format!("pruned decode left {} trailing bytes", pruned_reader.len()),
        Err(error) => format!("pruned decode failed: {error}"),
    };

    // A daemon is allowed to ignore the requested pruning mode. Preserve compatibility by
    // accepting and validating a complete transaction blob when one is returned.
    let mut full_reader = blob;
    let tx_full = Transaction::<NotPruned>::read(&mut full_reader).map_err(|error| {
        RangeFetchError::Rpc(RpcError::InvalidInterface(format!(
            "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] decode failed ({pruned_decode_detail}; full decode failed: {error})"
        )))
    })?;
    if !full_reader.is_empty() {
        return Err(RangeFetchError::Rpc(RpcError::InvalidInterface(format!(
            "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] full transaction had {} trailing bytes",
            full_reader.len()
        ))));
    }

    if tx_full.hash() != expected_hash {
        return Err(RangeFetchError::Rpc(RpcError::InvalidInterface(format!(
            "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] full transaction hash mismatch"
        ))));
    }

    if let (Some(expected_prunable_hash), Some(actual_prunable_hash)) =
        (prunable_hash, tx_full.prunable_hash())
    {
        if actual_prunable_hash != expected_prunable_hash {
            return Err(RangeFetchError::Rpc(RpcError::InvalidInterface(format!(
                "range get_blocks.bin block[{block_idx}] tx[{tx_idx}] prunable_hash mismatch"
            ))));
        }
    }

    Ok(Transaction::from(tx_full))
}

fn fetch_scannable_blocks_range_bin_with_prune(
    rpc_client: &RpcClient,
    base_url: &str,
    start_bn: usize,
    end_bn_inclusive: usize,
    prune: bool,
) -> Result<Vec<ScannableBlock>, RangeFetchError> {
    let requested_blocks = end_bn_inclusive
        .checked_sub(start_bn)
        .and_then(|n| n.checked_add(1))
        .ok_or_else(|| RpcError::InternalError("range fetch block count overflow".to_string()))?;

    let start_height = u64::try_from(start_bn)
        .map_err(|_| RpcError::InternalError("range fetch start height overflow".to_string()))?;
    let count = u64::try_from(requested_blocks)
        .map_err(|_| RpcError::InternalError("range fetch count overflow".to_string()))?;

    let transport = BlockingRpcTransport::new(base_url).map_err(|code| {
        RpcError::InternalError(format!(
            "range fetch transport init failed for '{base_url}' (code={code})"
        ))
    })?;

    let resp = transport.get_blocks_bin(start_height, count, prune)?;

    wc_log_line_android_or_stdout(&format!(
        "🧭 range_get_blocks_bin rpc_ok start_height={} count={} prune={} returned_blocks={}",
        start_height,
        count,
        prune,
        resp.blocks.len()
    ));

    if let Some(status) = resp.status.as_deref() {
        if !status.eq_ignore_ascii_case("OK") {
            return Err(RpcError::InvalidInterface(format!(
                "range get_blocks.bin returned status={status}"
            ))
            .into());
        }
    }

    if resp.blocks.is_empty() {
        return Err(RpcError::InvalidInterface(format!(
            "range get_blocks.bin returned 0 blocks, expected up to {} for heights {}..={}",
            requested_blocks, start_bn, end_bn_inclusive
        ))
        .into());
    }
    if resp.blocks.len() > requested_blocks {
        return Err(RpcError::InvalidInterface(format!(
            "range get_blocks.bin returned {} blocks, requested {} for heights {}..={}",
            resp.blocks.len(),
            requested_blocks,
            start_bn,
            end_bn_inclusive
        ))
        .into());
    }
    if resp.blocks.len() < requested_blocks {
        let actual_end = start_bn.saturating_add(resp.blocks.len()).saturating_sub(1);
        wc_log_line_android_or_stdout(&format!(
            "🧭 range_get_blocks_bin partial start_height={} requested={} returned_blocks={} actual_range={}..={}",
            start_height,
            requested_blocks,
            resp.blocks.len(),
            start_bn,
            actual_end
        ));
    }

    let output_indices_by_block = resp.output_indices.unwrap_or_default();
    let mut out = Vec::with_capacity(resp.blocks.len());

    for (block_idx, entry) in resp.blocks.into_iter().enumerate() {
        let mut block_reader: &[u8] = entry.block.as_slice();
        let block = MoneroBlock::read(&mut block_reader).map_err(|e| {
            RpcError::InvalidInterface(format!(
                "range get_blocks.bin block[{block_idx}] decode failed: {e}"
            ))
        })?;
        if !block_reader.is_empty() {
            return Err(RpcError::InvalidInterface(format!(
                "range get_blocks.bin block[{block_idx}] had {} trailing bytes",
                block_reader.len()
            ))
            .into());
        }

        if block.transactions.len() != entry.txs.len() {
            return Err(RpcError::InvalidInterface(format!(
                "range get_blocks.bin block[{block_idx}] had {} tx hashes but {} tx blobs",
                block.transactions.len(),
                entry.txs.len()
            ))
            .into());
        }

        if block_idx == 0 {
            wc_log_line_android_or_stdout(&format!(
                "🧭 range_get_blocks_bin block_ok block_idx=0 tx_hashes={} tx_blobs={}",
                block.transactions.len(),
                entry.txs.len()
            ));
        }

        let mut transactions: Vec<Transaction<Pruned>> = Vec::with_capacity(entry.txs.len());
        for (tx_idx, (expected_hash, tx_entry)) in block
            .transactions
            .iter()
            .zip(entry.txs.into_iter())
            .enumerate()
        {
            transactions.push(decode_range_transaction(
                &tx_entry.blob,
                tx_entry.prunable_hash,
                *expected_hash,
                block_idx,
                tx_idx,
            )?);
        }

        if block_idx == 0 {
            wc_log_line_android_or_stdout(&format!(
                "🧭 range_get_blocks_bin txs_ok block_idx=0 txs={}",
                transactions.len()
            ));
        }

        let mut output_index_for_first_ringct_output = None;

        let miner_tx_hash = block.miner_transaction().hash();
        let miner_tx = Transaction::from(block.miner_transaction().clone());

        if let Some(output_indexes) = get_output_indexes_from_block_response(
            block_idx,
            &miner_tx,
            &transactions,
            output_indices_by_block.get(block_idx),
        )? {
            output_index_for_first_ringct_output = output_indexes.first().copied();
            let log_scan_details = std::env::var("WALLETCORE_SCAN_LOG")
                .ok()
                .map(|s| s != "0")
                .unwrap_or(false);
            if log_scan_details {
                wc_log_line_android_or_stdout(&format!(
                    "🧭 range_get_blocks_bin output_indices_inline block_idx={} count={} first={:?}",
                    block_idx,
                    output_indexes.len(),
                    output_index_for_first_ringct_output
                ));
            }
        }

        if output_index_for_first_ringct_output.is_none() {
            for (tx_hash, tx) in std::iter::once((miner_tx_hash, &miner_tx))
                .chain(block.transactions.iter().copied().zip(transactions.iter()))
            {
                if matches!(tx, Transaction::V1 { .. }) || tx.prefix().outputs.is_empty() {
                    continue;
                }

                wc_log_line_android_or_stdout(&format!(
                    "🧭 range_get_blocks_bin output_indexes_start block_idx={} outputs={} tx_hash_prefix={:02x}{:02x}{:02x}{:02x}",
                    block_idx,
                    tx.prefix().outputs.len(),
                    tx_hash[0],
                    tx_hash[1],
                    tx_hash[2],
                    tx_hash[3]
                ));

                let output_indexes = match TOKIO_RUNTIME.block_on(
                    monero_interface::ProvidesOutputs::output_indexes(rpc_client, tx_hash),
                ) {
                    Ok(v) => v,
                    Err(primary_err) => {
                        wc_log_line_android_or_stdout(&format!(
                            "🧭 range_get_blocks_bin output_indexes_fallback block_idx={} reason={}",
                            block_idx, primary_err
                        ));
                        fetch_output_indexes_via_get_transactions(base_url, tx_hash).map_err(
                            |fallback_err| {
                                RpcError::InvalidInterface(format!(
                                    "range get_blocks.bin block[{block_idx}] output_indexes failed: primary={primary_err}; fallback={fallback_err}"
                                ))
                            },
                        )?
                    }
                };

                if output_indexes.len() != tx.prefix().outputs.len() {
                    return Err(RpcError::InvalidInterface(format!(
                        "range get_blocks.bin returned {} output indexes for {} outputs",
                        output_indexes.len(),
                        tx.prefix().outputs.len()
                    ))
                    .into());
                }

                output_index_for_first_ringct_output = output_indexes.first().copied();
                wc_log_line_android_or_stdout(&format!(
                    "🧭 range_get_blocks_bin output_indexes_ok block_idx={} count={} first={:?}",
                    block_idx,
                    output_indexes.len(),
                    output_index_for_first_ringct_output
                ));
                break;
            }
        }

        out.push(ScannableBlock {
            block,
            transactions,
            output_index_for_first_ringct_output,
        });
    }

    Ok(out)
}

fn fetch_scannable_blocks_range_bin(
    rpc_client: &RpcClient,
    base_url: &str,
    start_bn: usize,
    end_bn_inclusive: usize,
) -> Result<Vec<ScannableBlock>, RpcError> {
    match fetch_scannable_blocks_range_bin_with_prune(
        rpc_client,
        base_url,
        start_bn,
        end_bn_inclusive,
        true,
    ) {
        Ok(blocks) => Ok(blocks),
        Err(RangeFetchError::RetryUnpruned(pruned_error)) => {
            wc_log_line_android_or_stdout(&format!(
                "🧭 range_get_blocks_bin retry_unpruned start_height={} end_height={} reason={}",
                start_bn, end_bn_inclusive, pruned_error
            ));
            match fetch_scannable_blocks_range_bin_with_prune(
                rpc_client,
                base_url,
                start_bn,
                end_bn_inclusive,
                false,
            ) {
                Ok(blocks) => Ok(blocks),
                Err(RangeFetchError::Rpc(error) | RangeFetchError::RetryUnpruned(error)) => {
                    Err(error)
                }
            }
        }
        Err(RangeFetchError::Rpc(error)) => Err(error),
    }
}

// ===== Android-only: dedicated contiguous block fetch worker (no per-batch thread spawning) =====
//
// Motivation:
// - Our previous timeout helper spawned a new OS thread per batch; Android can degrade/stall under
//   sustained thread churn.
// - Tokio timeouts are not available here (no timer driver/reactor), so we implement bounded waiting
//   via std::sync::mpsc + recv_timeout.
// - The worker executes requests sequentially on a single long-lived OS thread.
// - IMPORTANT: build and own the RPC client *inside* the worker thread. Sharing a transport/client
//   across OS threads can lead to Hyper ChannelClosed on Android.
// - Also: Hyper can still surface ChannelClosed sporadically; on Android we rebuild the client and
//   retry once when we detect ChannelClosed.
#[cfg(target_os = "android")]
struct AndroidContiguousFetchWorker {
    tx: std::sync::mpsc::Sender<AndroidFetchReq>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[cfg(target_os = "android")]
struct AndroidFetchReq {
    start_bn: usize,
    end_bn_inclusive: usize,
    resp_tx: std::sync::mpsc::Sender<Result<Vec<ScannableBlock>, RpcError>>,
}

#[cfg(target_os = "android")]
struct AndroidPendingFetch {
    resp_rx: std::sync::mpsc::Receiver<Result<Vec<ScannableBlock>, RpcError>>,
    /// When the request was queued on the worker (for RPC wall time vs main-thread wait).
    started_at: Instant,
}

#[cfg(target_os = "android")]
impl AndroidContiguousFetchWorker {
    fn start(base_url: String, bulk_fetch_mode: BulkFetchMode) -> Self {
        use std::sync::mpsc;

        let (tx, rx) = mpsc::channel::<AndroidFetchReq>();

        let thread = std::thread::spawn(move || {
            fn build_client(base_url: &str) -> Result<RpcClient, RpcError> {
                TOKIO_RUNTIME.block_on(async {
                    monero_simple_request_rpc::SimpleRequestTransport::new(base_url.to_string())
                        .await
                        .map_err(Into::into)
                })
            }

            fn is_channel_closed(err: &RpcError) -> bool {
                // Best-effort detection by string match; we don't want to depend on hyper error types here.
                // Expected formatting seen in logs: "interface error (Hyper(hyper::Error(ChannelClosed)))"
                let s = err.to_string();
                s.contains("ChannelClosed")
            }

                // Build the transport/client on this worker thread so hyper state is not shared across
                // threads.
                let mut client = match build_client(&base_url) {
                    Ok(c) => c,
                    Err(e) => {
                    while let Ok(req) = rx.recv() {
                        let _ = req.resp_tx.send(Err(e.clone()));
                    }
                    return;
                }
            };

            while let Ok(req) = rx.recv() {
                let start_bn = req.start_bn;
                let end_bn_inclusive = req.end_bn_inclusive;
                let resp_tx = req.resp_tx;

                // First attempt with current client
                let mut res: Result<Vec<ScannableBlock>, RpcError> =
                    match bulk_fetch_mode {
                        BulkFetchMode::RangeBlocks => fetch_scannable_blocks_range_bin(
                            &client,
                            &base_url,
                            start_bn,
                            end_bn_inclusive,
                        ),
                        _ => TOKIO_RUNTIME.block_on(async {
                            client
                                .contiguous_scannable_blocks(start_bn..=end_bn_inclusive)
                                .await
                                .map_err(Into::into)
                        }),
                    };

                // If Hyper channel got closed, rebuild client and retry once.
                if res.as_ref().is_err_and(is_channel_closed) {
                    if let Ok(new_client) = build_client(&base_url) {
                        client = new_client;

                        res = match bulk_fetch_mode {
                            BulkFetchMode::RangeBlocks => fetch_scannable_blocks_range_bin(
                                &client,
                                &base_url,
                                start_bn,
                                end_bn_inclusive,
                            ),
                            _ => TOKIO_RUNTIME.block_on(async {
                                client
                                    .contiguous_scannable_blocks(start_bn..=end_bn_inclusive)
                                    .await
                                    .map_err(Into::into)
                            }),
                        };
                    }
                }

                let _ = resp_tx.send(res);
            }
        });

        Self {
            tx,
            thread: Some(thread),
        }
    }

    /// Queue a contiguous fetch on the worker without waiting (one-ahead prefetch).
    fn begin_fetch(
        &self,
        start_bn: usize,
        end_bn_inclusive: usize,
    ) -> Result<AndroidPendingFetch, &'static str> {
        use std::sync::mpsc;

        let (resp_tx, resp_rx) = mpsc::channel::<Result<Vec<ScannableBlock>, RpcError>>();
        let req = AndroidFetchReq {
            start_bn,
            end_bn_inclusive,
            resp_tx,
        };
        if self.tx.send(req).is_err() {
            return Err("disconnected");
        }
        Ok(AndroidPendingFetch {
            resp_rx,
            started_at: Instant::now(),
        })
    }

    fn wait_pending(
        pending: AndroidPendingFetch,
        timeout_secs: u64,
    ) -> Result<Result<Vec<ScannableBlock>, RpcError>, &'static str> {
        use std::sync::mpsc;
        use std::time::Duration;

        match pending.resp_rx.recv_timeout(Duration::from_secs(timeout_secs)) {
            Ok(v) => Ok(v),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("timeout"),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err("disconnected"),
        }
    }

    fn fetch_with_timeout(
        &self,
        timeout_secs: u64,
        start_bn: usize,
        end_bn_inclusive: usize,
    ) -> Result<Result<Vec<ScannableBlock>, RpcError>, &'static str> {
        let pending = self.begin_fetch(start_bn, end_bn_inclusive)?;
        Self::wait_pending(pending, timeout_secs)
    }

    fn shutdown(mut self) {
        // Dropping tx closes the channel; worker exits its recv() loop.
        drop(self.tx);
        if let Some(h) = self.thread.take() {
            let _ = h.join();
        }
    }
}

// Non-Android builds keep the existing per-call helper for now.
#[cfg(not(target_os = "android"))]
// FFI-safe timeout wrapper for async RPC futures without relying on tokio::time (reactor),
// which can panic when called outside an entered runtime context.
//
// This variant takes a closure producing the future so all captures can be moved into the
// spawned worker thread cleanly.
fn recv_timeout_block_on<T, MakeFut, Fut>(
    timeout_secs: u64,
    make_fut: MakeFut,
) -> Result<T, &'static str>
where
    MakeFut: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    use std::sync::mpsc;
    use std::time::Duration;

    let (tx, rx) = mpsc::channel::<T>();

    std::thread::spawn(move || {
        let fut = make_fut();
        let res = TOKIO_RUNTIME.block_on(fut);
        let _ = tx.send(res);
    });

    match rx.recv_timeout(Duration::from_secs(timeout_secs)) {
        Ok(v) => Ok(v),
        Err(mpsc::RecvTimeoutError::Timeout) => Err("timeout"),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err("disconnected"),
    }
}

#[no_mangle]
pub extern "C" fn wallet_refresh(
    wallet_id: *const c_char,
    node_url: *const c_char,
    out_last_scanned: *mut u64,
) -> c_int {
    // NOTE:
    // Android builds often end up with panic=abort, which turns Rust panics into SIGABRT with no
    // useful message in logcat. Wrap the entire implementation so we can surface panics via
    // walletcore lastError + logcat.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wallet_refresh_impl(wallet_id, node_url, out_last_scanned)
    }));

    match result {
        Ok(code) => code,
        Err(panic_payload) => {
            let msg = if let Some(s) = panic_payload.downcast_ref::<&str>() {
                format!("wallet_refresh panic: {s}")
            } else if let Some(s) = panic_payload.downcast_ref::<String>() {
                format!("wallet_refresh panic: {s}")
            } else {
                "wallet_refresh panic: <non-string payload>".to_string()
            };

            wc_log_line_android_or_stdout(&format!("🧨 {msg}"));
            record_error(-16, msg)
        }
    }
}

fn wallet_refresh_impl(
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

    // Install panic hook once per process for better crash diagnostics.
    let _ = &*PANIC_HOOK_INSTALLED;

    // Clear any stale cancellation request from a previous refresh before starting.
    set_refresh_cancel_for_wallet(id, false);

    // Stage logging to diagnose early refresh termination.
    // IMPORTANT: on Android, stdout/stderr is often not captured, so emit directly to logcat.
    wc_log_line_android_or_stdout(&format!("🧭 wallet_refresh stage=init wallet_id={}", id));

    // Debug/perf logging is opt-in via WALLETCORE_SCAN_LOG=1. Do not force it on Android.

    // Build/runtime sanity logs (once per process).
    static BUILD_INFO_LOGGED: std::sync::Once = std::sync::Once::new();
    BUILD_INFO_LOGGED.call_once(|| {
        wc_log_line_android_or_stdout(&format!(
            "🧭 walletcore_build target_os={} target_arch={} compile_time_generators={} scanner_microprof_feature={}",
            std::env::consts::OS,
            std::env::consts::ARCH,
            cfg!(feature = "compile-time-generators"),
            cfg!(feature = "scanner-microprof")
        ));
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

    wc_log_line_android_or_stdout(&format!(
        "🧩 walletcore refresh entry: version={} build={} wallet_id={} node_url={} env{{scan_par={} scan_batch={} bulk_fetch={} bulk_mode={} bulk_fetch_batch={}}}",
        WALLETCORE_LOG_VERSION,
        build_stamp(),
        id,
        base_url,
        env_par,
        env_batch,
        env_bulk_fetch,
        env_bulk_mode,
        env_bulk_fetch_batch
    ));

    wc_log_line_android_or_stdout(&format!(
        "🧭 wallet_refresh stage=after_entry_stamp wallet_id={} node_url={}",
        id, base_url
    ));

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

    let refresh_t0 = Instant::now();

    let mut refresh_scan_ms_total: u128 = 0;
    let mut refresh_persist_ms_total: u128 = 0;
    // Main-thread time blocked waiting for the current batch's blocks.
    let mut refresh_fetch_wait_ms_total: u128 = 0;
    // Worker/RPC wall time for those fetches (includes overlap with prior scan on prefetch hit).
    let mut refresh_fetch_rpc_ms_total: u128 = 0;
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
    wc_log_line_android_or_stdout(&format!(
        "🧭 wallet_refresh stage=daemon_connect_ok wallet_id={}",
        id
    ));

    // Daemon height
    walletcore_log_line(
        id,
        snapshot.network,
        &format!(
            "🧭 wallet_refresh stage=daemon_height_start wallet_id={}",
            id
        ),
    );
    wc_log_line_android_or_stdout(&format!(
        "🧭 wallet_refresh stage=daemon_height_start wallet_id={}",
        id
    ));

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
            wc_log_line_android_or_stdout(&format!(
                "🧭 wallet_refresh stage=daemon_height_ok wallet_id={} height={}",
                id, h
            ));
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
    wc_log_line_android_or_stdout(&format!(
        "🧭 wallet_refresh stage=upstream_batch_config wallet_id={} upstream_block_batch={}",
        id, upstream_block_batch
    ));

    let daemon = DaemonStatus {
        height: daemon_height,
        top_block_timestamp: 0,
    };

    // Keys + scanner
    let master = snapshot.keys.clone();
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
    if walletcore_debug_input_dump_enabled() {
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
    }

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
    let log_batch_events = log_perf;
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
    wc_log_line_android_or_stdout(&format!(
        "🧱 bulk-fetch mode resolved: requested={} batch={} (pre-clearnet-gating)",
        bulk_mode_str(bulk_fetch_mode),
        bulk_fetch_batch
    ));

    if scan_cursor < daemon.height {
        // Android: keep Hyper on one dedicated worker thread (avoids ChannelClosed from
        // sharing transports across OS threads). Still overlap the *next* batch fetch with
        // the current batch scan via one-ahead prefetch on that same worker.
        let prefetch_depth: usize = std::env::var("WALLETCORE_PREFETCH_DEPTH")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(1)
            .clamp(1, 2);

        #[cfg(not(target_os = "android"))]
        let mut next_scannables_q: VecDeque<(u64, u64, u128, Vec<ScannableBlock>)> = VecDeque::new();

        #[cfg(not(target_os = "android"))]
        let mut prefetch_in_flight: VecDeque<
            tokio::task::JoinHandle<(u64, u64, u128, Result<Vec<ScannableBlock>, RpcError>)>,
        > = VecDeque::new();

        #[cfg(not(target_os = "android"))]
        let prefetch_rpc_client = std::sync::Arc::new(prefetch_rpc_client);

        // Android-only: single dedicated fetch worker for contiguous_scannable_blocks.
        // Build and own the RPC client inside the worker thread to avoid Hyper ChannelClosed.
        #[cfg(target_os = "android")]
        let android_fetch_worker =
            AndroidContiguousFetchWorker::start(base_url.clone(), bulk_fetch_mode);

        // Ensure the Android worker thread is always shut down, even on early returns.
        // Keep the worker inside an Option so Drop can move it out and join the thread.
        #[cfg(target_os = "android")]
        struct AndroidFetchWorkerGuard(Option<AndroidContiguousFetchWorker>);
        #[cfg(target_os = "android")]
        impl Drop for AndroidFetchWorkerGuard {
            fn drop(&mut self) {
                if let Some(w) = self.0.take() {
                    w.shutdown();
                }
            }
        }

        #[cfg(target_os = "android")]
        let mut _android_fetch_worker_guard = AndroidFetchWorkerGuard(Some(android_fetch_worker));
        #[cfg(target_os = "android")]
        let android_fetch_worker = _android_fetch_worker_guard.0.as_ref().unwrap();

        // One-ahead prefetch: (start_bn_u64, end_bn_inclusive_u64, pending response).
        #[cfg(target_os = "android")]
        let mut android_next_prefetch: Option<(u64, u64, AndroidPendingFetch)> = None;

        // Silence unused on Android (prefetch_depth reserved for future depth>1).
        #[cfg(target_os = "android")]
        let _ = prefetch_depth;

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
                        "📈 wallet_refresh summary wallet_id={} status=cancelled total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} fetch_wait_ms_total={} fetch_rpc_ms_total={} persist_ms_total={}",
                        id,
                        total_ms,
                        refresh_batches_total,
                        refresh_blocks_total,
                        refresh_outputs_added_total,
                        refresh_scan_ms_total,
                        refresh_fetch_wait_ms_total,
                        refresh_fetch_rpc_ms_total,
                        refresh_persist_ms_total
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "📈 wallet_refresh summary wallet_id={} status=cancelled total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} fetch_wait_ms_total={} fetch_rpc_ms_total={} persist_ms_total={}",
                    id,
                    total_ms,
                    refresh_batches_total,
                    refresh_blocks_total,
                    refresh_outputs_added_total,
                    refresh_scan_ms_total,
                    refresh_fetch_wait_ms_total,
                    refresh_fetch_rpc_ms_total,
                    refresh_persist_ms_total
                ));
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

            if log_batch_events {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_start wallet_id={} range={}..={}",
                        id, start_bn, end_bn_inclusive
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_start wallet_id={} range={}..={}",
                    id, start_bn, end_bn_inclusive
                ));
            }

            let mut batch_fetch_wait_ms: u128 = 0;
            let mut batch_fetch_rpc_ms: u128 = 0;
            let mut batch_prefetch: &'static str = "n/a";

            let scannables: Vec<ScannableBlock> = {
                #[cfg(not(target_os = "android"))]
                {
                    if let Some((pf_start, pf_end, pf_rpc_ms, pf_vec)) = next_scannables_q.pop_front() {
                        if pf_start == start_bn_u64 && pf_end == end_bn_inclusive_u64 {
                            batch_prefetch = "hit";
                            batch_fetch_wait_ms = 0;
                            batch_fetch_rpc_ms = pf_rpc_ms;
                            pf_vec
                        } else {
                            // Prefetch mismatch; fetch synchronously (with hard timeout).
                            let fetch_t0 = Instant::now();
                            let start_bn_local = start_bn;
                            let end_bn_inclusive_local = end_bn_inclusive;
                            batch_prefetch = "miss";

                            let fetch_res = {
                                // Clone outside the timeout closure so we don't move `rpc_client` into the closure
                                // (the closure must be 'static and would otherwise capture/move `rpc_client`,
                                // which is reused across loop iterations).
                                let rpc_client_for_timeout = rpc_client.clone();
                                let base_url_for_timeout = base_url.clone();
                                let bulk_fetch_mode_for_timeout = bulk_fetch_mode;
                                recv_timeout_block_on(
                                    CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                    move || async move {
                                        match bulk_fetch_mode_for_timeout {
                                            BulkFetchMode::RangeBlocks => {
                                                fetch_scannable_blocks_range_bin(
                                                    &rpc_client_for_timeout,
                                                    &base_url_for_timeout,
                                                    start_bn_local,
                                                    end_bn_inclusive_local,
                                                )
                                            }
                                            _ => rpc_client_for_timeout
                                                .contiguous_scannable_blocks(
                                                    start_bn_local..=end_bn_inclusive_local,
                                                )
                                                .await
                                                .map_err(Into::into),
                                        }
                                    },
                                )
                            };

                            match fetch_res {
                                Ok(Ok(v)) => {
                                    let fetch_ms = fetch_t0.elapsed().as_millis();
                                    batch_fetch_wait_ms = fetch_ms;
                                    batch_fetch_rpc_ms = fetch_ms;
                                    if log_batch_events {
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
                                        wc_log_line_android_or_stdout(&format!(
                                            "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_ms={} blocks_per_s={:.2}",
                                            id,
                                            v.len(),
                                            fetch_ms,
                                            if fetch_ms > 0 {
                                                (v.len() as f64) / (fetch_ms as f64 / 1000.0)
                                            } else {
                                                0.0
                                            }
                                        ));
                                    }
                                    v
                                }
                                Ok(Err(err)) => {
                                    let fetch_ms = fetch_t0.elapsed().as_millis();
                                    walletcore_log_line(
                                        id,
                                        snapshot.network,
                                        &format!(
                                            "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                            id, fetch_ms, err
                                        ),
                                    );
                                    wc_log_line_android_or_stdout(&format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                        id, fetch_ms, err
                                    ));
                                    return record_error(
                                        -16,
                                        format!(
                                            "wallet_refresh: contiguous_scannable_blocks failed: {}",
                                            err
                                        ),
                                    );
                                }
                                Err(msg) => {
                                    let fetch_ms = fetch_t0.elapsed().as_millis();
                                    let msg = format!(
                                        "wallet_refresh: contiguous_scannable_blocks timeout/disconnect ({}) after {}s for heights {}..{}",
                                        msg,
                                        CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                        start_bn_local,
                                        end_bn_inclusive_local
                                    );
                                    walletcore_log_line(
                                        id,
                                        snapshot.network,
                                        &format!(
                                            "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                            id, fetch_ms, msg
                                        ),
                                    );
                                    wc_log_line_android_or_stdout(&format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                        id, fetch_ms, msg
                                    ));
                                    return record_error(-16, msg);
                                }
                            }
                        }
                    } else {
                        let fetch_t0 = Instant::now();
                        let start_bn_local = start_bn;
                        let end_bn_inclusive_local = end_bn_inclusive;
                        batch_prefetch = "sync";

                        let fetch_res = {
                            // Clone outside the timeout closure so we don't move `rpc_client` into the closure
                            // (the closure must be 'static and would otherwise capture/move `rpc_client`,
                            // which is reused across loop iterations).
                            let rpc_client_for_timeout = rpc_client.clone();
                            let base_url_for_timeout = base_url.clone();
                            let bulk_fetch_mode_for_timeout = bulk_fetch_mode;
                            recv_timeout_block_on(
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                move || async move {
                                    match bulk_fetch_mode_for_timeout {
                                        BulkFetchMode::RangeBlocks => fetch_scannable_blocks_range_bin(
                                            &rpc_client_for_timeout,
                                            &base_url_for_timeout,
                                            start_bn_local,
                                            end_bn_inclusive_local,
                                        ),
                                        _ => rpc_client_for_timeout
                                            .contiguous_scannable_blocks(
                                                start_bn_local..=end_bn_inclusive_local,
                                            )
                                            .await
                                            .map_err(Into::into),
                                    }
                                },
                            )
                        };

                        match fetch_res {
                            Ok(Ok(v)) => {
                                let fetch_ms = fetch_t0.elapsed().as_millis();
                                batch_fetch_wait_ms = fetch_ms;
                                batch_fetch_rpc_ms = fetch_ms;
                                if log_batch_events {
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
                                    wc_log_line_android_or_stdout(&format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_ms={} blocks_per_s={:.2}",
                                        id,
                                        v.len(),
                                        fetch_ms,
                                        if fetch_ms > 0 {
                                            (v.len() as f64) / (fetch_ms as f64 / 1000.0)
                                        } else {
                                            0.0
                                        }
                                    ));
                                }
                                v
                            }
                            Ok(Err(err)) => {
                                let fetch_ms = fetch_t0.elapsed().as_millis();
                                walletcore_log_line(
                                    id,
                                    snapshot.network,
                                    &format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                        id, fetch_ms, err
                                    ),
                                );
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                    id, fetch_ms, err
                                ));
                                return record_error(
                                    -16,
                                    format!(
                                        "wallet_refresh: contiguous_scannable_blocks failed: {}",
                                        err
                                    ),
                                );
                            }
                            Err(msg) => {
                                let fetch_ms = fetch_t0.elapsed().as_millis();
                                let msg = format!(
                                    "wallet_refresh: contiguous_scannable_blocks timeout/disconnect ({}) after {}s for heights {}..{}",
                                    msg,
                                    CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                    start_bn_local,
                                    end_bn_inclusive_local
                                );
                                walletcore_log_line(
                                    id,
                                    snapshot.network,
                                    &format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                        id, fetch_ms, msg
                                    ),
                                );
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                    id, fetch_ms, msg
                                ));
                                return record_error(-16, msg);
                            }
                        }
                    }
                }

                #[cfg(target_os = "android")]
                {
                    // Prefer one-ahead prefetch when the pending range matches; otherwise drain and
                    // fetch synchronously. Hyper stays on the dedicated worker thread either way.
                    let fetch_wait_t0 = Instant::now();
                    let start_bn_local = start_bn;
                    let end_bn_inclusive_local = end_bn_inclusive;
                    let mut fetch_started_at = fetch_wait_t0;

                    let fetch_res = match android_next_prefetch.take() {
                        Some((pf_start, pf_end, pending))
                            if pf_start == start_bn_u64 && pf_end == end_bn_inclusive_u64 =>
                        {
                            batch_prefetch = "hit";
                            fetch_started_at = pending.started_at;
                            if log_batch_events {
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=android_prefetch_hit wallet_id={} range={}..={}",
                                    id, start_bn_local, end_bn_inclusive_local
                                ));
                            }
                            AndroidContiguousFetchWorker::wait_pending(
                                pending,
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                            )
                        }
                        Some((pf_start, pf_end, pending)) => {
                            batch_prefetch = "miss";
                            if log_batch_events {
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=android_prefetch_miss wallet_id={} wanted={}..={} pending={}..={}",
                                    id,
                                    start_bn_u64,
                                    end_bn_inclusive_u64,
                                    pf_start,
                                    pf_end
                                ));
                            }
                            let _ = AndroidContiguousFetchWorker::wait_pending(
                                pending,
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                            );
                            fetch_started_at = Instant::now();
                            android_fetch_worker.fetch_with_timeout(
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                start_bn_local,
                                end_bn_inclusive_local,
                            )
                        }
                        None => {
                            batch_prefetch = "sync";
                            fetch_started_at = Instant::now();
                            android_fetch_worker.fetch_with_timeout(
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                start_bn_local,
                                end_bn_inclusive_local,
                            )
                        }
                    };

                    match fetch_res {
                        Ok(Ok(v)) => {
                            let fetch_wait_ms = fetch_wait_t0.elapsed().as_millis();
                            let fetch_rpc_ms = fetch_started_at.elapsed().as_millis();
                            batch_fetch_wait_ms = fetch_wait_ms;
                            batch_fetch_rpc_ms = fetch_rpc_ms;
                            if log_batch_events {
                                walletcore_log_line(
                                    id,
                                    snapshot.network,
                                    &format!(
                                        "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_wait_ms={} fetch_rpc_ms={} prefetch={} blocks_per_s={:.2}",
                                        id,
                                        v.len(),
                                        fetch_wait_ms,
                                        fetch_rpc_ms,
                                        batch_prefetch,
                                        if fetch_wait_ms > 0 {
                                            (v.len() as f64) / (fetch_wait_ms as f64 / 1000.0)
                                        } else {
                                            0.0
                                        }
                                    ),
                                );
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_ok wallet_id={} blocks={} fetch_wait_ms={} fetch_rpc_ms={} prefetch={} blocks_per_s={:.2}",
                                    id,
                                    v.len(),
                                    fetch_wait_ms,
                                    fetch_rpc_ms,
                                    batch_prefetch,
                                    if fetch_wait_ms > 0 {
                                        (v.len() as f64) / (fetch_wait_ms as f64 / 1000.0)
                                    } else {
                                        0.0
                                    }
                                ));
                            }
                            v
                        }
                        Ok(Err(err)) => {
                            let fetch_ms = fetch_wait_t0.elapsed().as_millis();
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                    id, fetch_ms, err
                                ),
                            );
                            wc_log_line_android_or_stdout(&format!(
                                "🧭 wallet_refresh stage=contiguous_scannable_blocks_error wallet_id={} fetch_ms={} err={}",
                                id, fetch_ms, err
                            ));
                            return record_error(
                                -16,
                                format!(
                                    "wallet_refresh: contiguous_scannable_blocks failed: {}",
                                    err
                                ),
                            );
                        }
                        Err(msg) => {
                            let fetch_ms = fetch_wait_t0.elapsed().as_millis();
                            let msg = format!(
                                "wallet_refresh: contiguous_scannable_blocks timeout/disconnect ({}) after {}s for heights {}..{}",
                                msg,
                                CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                                start_bn_local,
                                end_bn_inclusive_local
                            );
                            walletcore_log_line(
                                id,
                                snapshot.network,
                                &format!(
                                    "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                    id, fetch_ms, msg
                                ),
                            );
                            wc_log_line_android_or_stdout(&format!(
                                "🧭 wallet_refresh stage=contiguous_scannable_blocks_timeout wallet_id={} fetch_ms={} err={}",
                                id, fetch_ms, msg
                            ));
                            return record_error(-16, msg);
                        }
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
            let actual_end_bn_inclusive =
                start_bn.saturating_add(scannables.len().saturating_sub(1));

            // Ensure prefetch depth (non-Android only).
            #[cfg(not(target_os = "android"))]
            {
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
                    let prefetch_base_url = base_url.clone();
                    let prefetch_mode = bulk_fetch_mode;
                    let handle = TOKIO_RUNTIME.spawn(async move {
                        let t0 = Instant::now();
                        let res = match prefetch_mode {
                            BulkFetchMode::RangeBlocks => fetch_scannable_blocks_range_bin(
                                &prefetch_client,
                                &prefetch_base_url,
                                next_start_bn,
                                next_end_bn,
                            ),
                            _ => prefetch_client
                                .contiguous_scannable_blocks(next_start_bn..=next_end_bn)
                                .await
                                .map_err(Into::into),
                        };
                        let prefetch_ms = t0.elapsed().as_millis();
                        (next_start, next_end_inclusive, prefetch_ms, res)
                    });
                    prefetch_in_flight.push_back(handle);

                    cursor_for_prefetch = next_end_exclusive;
                }
            }

            // Android one-ahead: start next fetch on the worker before scanning this batch.
            #[cfg(target_os = "android")]
            {
                if let Some((_, _, pending)) = android_next_prefetch.take() {
                    let _ = AndroidContiguousFetchWorker::wait_pending(
                        pending,
                        CONTIGUOUS_BLOCKS_TIMEOUT_SECS,
                    );
                }
                let next_start = (actual_end_bn_inclusive as u64).saturating_add(1);
                let next_end_exclusive = core::cmp::min(
                    daemon.height,
                    next_start.saturating_add(upstream_block_batch),
                );
                if next_end_exclusive > next_start {
                    let next_end_inclusive = next_end_exclusive.saturating_sub(1);
                    if let (Ok(next_start_bn), Ok(next_end_bn)) = (
                        usize::try_from(next_start),
                        usize::try_from(next_end_inclusive),
                    ) {
                        match android_fetch_worker.begin_fetch(next_start_bn, next_end_bn) {
                            Ok(pending) => {
                                if log_batch_events {
                                    wc_log_line_android_or_stdout(&format!(
                                        "🧭 wallet_refresh stage=android_prefetch_start wallet_id={} range={}..={}",
                                        id, next_start_bn, next_end_bn
                                    ));
                                }
                                android_next_prefetch =
                                    Some((next_start, next_end_inclusive, pending));
                            }
                            Err(msg) => {
                                wc_log_line_android_or_stdout(&format!(
                                    "🧭 wallet_refresh stage=android_prefetch_begin_failed wallet_id={} err={}",
                                    id, msg
                                ));
                            }
                        }
                    }
                }
            }

            // ---- Scan batch ----
            if log_batch_events {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧪 wallet_refresh stage=scan_start wallet_id={} range={}..={} blocks={}",
                        id,
                        start_bn,
                        actual_end_bn_inclusive,
                        scannables.len()
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "🧪 wallet_refresh stage=scan_start wallet_id={} range={}..={} blocks={}",
                    id,
                    start_bn,
                    actual_end_bn_inclusive,
                    scannables.len()
                ));
            }

            let scan_t0 = Instant::now();
            let mut outputs_added_in_batch: usize = 0;
            let blocks_in_batch: usize = scannables.len();

            // Android-only: scan heartbeat (detect if we're CPU-bound vs deadlocked inside scan).
            #[cfg(target_os = "android")]
            let mut last_scan_heartbeat = Instant::now();

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

                // Android-only: heartbeat every ~5s during scan loop.
                #[cfg(target_os = "android")]
                {
                    if log_batch_events && last_scan_heartbeat.elapsed().as_secs() >= 5 {
                        last_scan_heartbeat = Instant::now();
                        walletcore_log_line(
                            id,
                            snapshot.network,
                            &format!(
                                "🧪 wallet_refresh stage=scan_heartbeat wallet_id={} height={} range={}..={} outputs_added_so_far={}",
                                id, th, start_bn, actual_end_bn_inclusive, outputs_added_in_batch
                            ),
                        );
                        wc_log_line_android_or_stdout(&format!(
                            "🧪 wallet_refresh stage=scan_heartbeat wallet_id={} height={} range={}..={} outputs_added_so_far={}",
                            id, th, start_bn, actual_end_bn_inclusive, outputs_added_in_batch
                        ));
                    }
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
                                working_outputs[out_idx].spending_height = Some(th);
                                let e = spent_inputs_by_txid.entry(spend_txid).or_insert(0);
                                *e = e.saturating_add(spent_amount);

                                if walletcore_debug_spend_detect_enabled() {
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
                                    if walletcore_debug_spend_detect_enabled() {
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
                            }

                            if let Some(out_idx) = key_image_to_output_index.get(&ki_bytes).copied()
                            {
                                let spent_amount = working_outputs[out_idx].amount;
                                working_outputs[out_idx].spent = true;

                                if let Some(spend_txid) = spend_txid_opt {
                                    working_outputs[out_idx].spending_txid = Some(spend_txid);
                                    working_outputs[out_idx].spending_height = Some(th);
                                    let e = spent_inputs_by_txid.entry(spend_txid).or_insert(0);
                                    *e = e.saturating_add(spent_amount);

                                    if walletcore_debug_spend_detect_enabled() {
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
                                } else {
                                    working_outputs[out_idx].spending_txid = None;
                                    working_outputs[out_idx].spending_height = Some(th);
                                    if walletcore_debug_spend_detect_enabled() {
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
                    //
                    // IMPORTANT: We must use the exact same derivation as the send path; otherwise
                    // we cannot correlate daemon `is_key_image_spent` results back to tracked outputs,
                    // and sends can fail with confusing double_spend/invalid_input behavior.
                    //
                    // Use the shared helper (also used by send) to keep this consistent.
                    let key_image_bytes: [u8; 32] = derive_key_image_bytes(
                        &output,
                        master.spend_scalar,
                        master.view_scalar_ed,
                        major,
                        minor,
                    );

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
                        spending_height: None,
                    });

                    outputs_added_in_batch = outputs_added_in_batch.saturating_add(1);
                }

                th = th.saturating_add(1);
            }

            let scan_ms = scan_t0.elapsed().as_millis();
            refresh_scan_ms_total = refresh_scan_ms_total.saturating_add(scan_ms);
            refresh_fetch_wait_ms_total =
                refresh_fetch_wait_ms_total.saturating_add(batch_fetch_wait_ms);
            refresh_fetch_rpc_ms_total =
                refresh_fetch_rpc_ms_total.saturating_add(batch_fetch_rpc_ms);
            refresh_blocks_total = refresh_blocks_total.saturating_add(blocks_in_batch);
            refresh_outputs_added_total =
                refresh_outputs_added_total.saturating_add(outputs_added_in_batch);
            refresh_batches_total = refresh_batches_total.saturating_add(1);

            let overlapped_ms = batch_fetch_rpc_ms.saturating_sub(batch_fetch_wait_ms);
            let likely_bound = if batch_fetch_wait_ms >= scan_ms {
                "fetch"
            } else if scan_ms > batch_fetch_wait_ms.saturating_mul(2) {
                "scan"
            } else {
                "balanced"
            };
            let timing_line = format!(
                "⏱️ wallet_refresh stage=batch_timing wallet_id={} range={}..={} blocks={} prefetch={} fetch_wait_ms={} fetch_rpc_ms={} overlapped_ms={} scan_ms={} likely_bound={}",
                id,
                start_bn,
                actual_end_bn_inclusive,
                blocks_in_batch,
                batch_prefetch,
                batch_fetch_wait_ms,
                batch_fetch_rpc_ms,
                overlapped_ms,
                scan_ms,
                likely_bound
            );
            walletcore_log_line(id, snapshot.network, &timing_line);
            wc_log_line_android_or_stdout(&timing_line);

            if log_batch_events {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧪 wallet_refresh stage=scan_done wallet_id={} range={}..={} blocks={} outputs_added={} scan_ms={}",
                        id,
                        start_bn,
                        actual_end_bn_inclusive,
                        blocks_in_batch,
                        outputs_added_in_batch,
                        scan_ms
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "🧪 wallet_refresh stage=scan_done wallet_id={} range={}..={} blocks={} outputs_added={} scan_ms={}",
                    id,
                    start_bn,
                    actual_end_bn_inclusive,
                    blocks_in_batch,
                    outputs_added_in_batch,
                    scan_ms
                ));
            }

            if refresh_telemetry_enabled {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "🧪 scannable_completeness wallet_id={} range={}..={} blocks={} txs_total={} txs_v1={} txs_v2={} v2_proofs_some={} v2_proofs_none={} txs_extra_nonempty={} txs_outputs_nonzero={} outputs_total={}",
                        id,
                        start_bn,
                        actual_end_bn_inclusive,
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
                        actual_end_bn_inclusive,
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

            if log_batch_events {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "💾 wallet_refresh stage=persist_start wallet_id={} range={}..={} next_scan_cursor={}",
                        id, start_bn, actual_end_bn_inclusive, th
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "💾 wallet_refresh stage=persist_start wallet_id={} range={}..={} next_scan_cursor={}",
                    id, start_bn, actual_end_bn_inclusive, th
                ));
            }

            persist_span_start = Some(Instant::now());

            // Drain prefetch tasks into ready queue (only await enough to keep moving).
            // Non-Android only; Android prefetch pipeline is disabled.
            #[cfg(not(target_os = "android"))]
            while next_scannables_q.is_empty() {
                let Some(handle) = prefetch_in_flight.pop_front() else {
                    break;
                };

                let join_wait_t0 = Instant::now();
                match TOKIO_RUNTIME.block_on(handle) {
                    Ok((pf_start, pf_end, pf_ms, Ok(v))) => {
                        let _ = join_wait_t0.elapsed().as_millis();
                        next_scannables_q.push_back((pf_start, pf_end, pf_ms, v));
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

            // Close this persist span now that we've advanced the cursor (per-batch persist boundary).
            if let Some(p0) = persist_span_start.take() {
                let persist_ms = p0.elapsed().as_millis();
                refresh_persist_ms_total = refresh_persist_ms_total.saturating_add(persist_ms);

                if log_batch_events {
                    walletcore_log_line(
                        id,
                        snapshot.network,
                        &format!(
                            "💾 wallet_refresh stage=persist_done wallet_id={} range={}..={} persist_ms={} new_last_scanned={}",
                            id, start_bn, actual_end_bn_inclusive, persist_ms, scan_cursor
                        ),
                    );
                    wc_log_line_android_or_stdout(&format!(
                        "💾 wallet_refresh stage=persist_done wallet_id={} range={}..={} persist_ms={} new_last_scanned={}",
                        id, start_bn, actual_end_bn_inclusive, persist_ms, scan_cursor
                    ));
                }
            }

            if log_batch_events {
                walletcore_log_line(
                    id,
                    snapshot.network,
                    &format!(
                        "✅ wallet_refresh stage=cursor_advance wallet_id={} last_scanned={}",
                        id, scan_cursor
                    ),
                );
                wc_log_line_android_or_stdout(&format!(
                    "✅ wallet_refresh stage=cursor_advance wallet_id={} last_scanned={}",
                    id, scan_cursor
                ));
            }

            update_scan_progress(
                id,
                scan_cursor.min(daemon.height),
                daemon.height,
                daemon.top_block_timestamp,
                snapshot.restore_height,
            );
        }

        if scan_cursor < daemon.height {
            walletcore_log_line(
                id,
                snapshot.network,
                &format!(
                    "⚠️ wallet_refresh stopped before tip wallet_id={} last_scanned={} tip={}",
                    id, scan_cursor, daemon.height
                ),
            );
            wc_log_line_android_or_stdout(&format!(
                "⚠️ wallet_refresh stopped before tip wallet_id={} last_scanned={} tip={}",
                id, scan_cursor, daemon.height
            ));
        }
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
            .saturating_sub(refresh_persist_ms_total)
            .saturating_sub(refresh_fetch_wait_ms_total);

        let likely_bound = if refresh_fetch_wait_ms_total >= refresh_scan_ms_total {
            "fetch"
        } else if refresh_scan_ms_total > refresh_fetch_wait_ms_total.saturating_mul(2) {
            "scan"
        } else {
            "balanced"
        };

        walletcore_log_line(
            id,
            snapshot.network,
            &format!(
                "📈 wallet_refresh summary wallet_id={} status=ok total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} fetch_wait_ms_total={} fetch_rpc_ms_total={} persist_ms_total={} other_ms={} likely_bound={}",
                id,
                total_ms,
                refresh_batches_total,
                refresh_blocks_total,
                refresh_outputs_added_total,
                refresh_scan_ms_total,
                refresh_fetch_wait_ms_total,
                refresh_fetch_rpc_ms_total,
                refresh_persist_ms_total,
                other_ms,
                likely_bound
            ),
        );
        wc_log_line_android_or_stdout(&format!(
            "📈 wallet_refresh summary wallet_id={} status=ok total_ms={} batches={} blocks={} outputs_added={} scan_ms_total={} fetch_wait_ms_total={} fetch_rpc_ms_total={} persist_ms_total={} other_ms={} likely_bound={}",
            id,
            total_ms,
            refresh_batches_total,
            refresh_blocks_total,
            refresh_outputs_added_total,
            refresh_scan_ms_total,
            refresh_fetch_wait_ms_total,
            refresh_fetch_rpc_ms_total,
            refresh_persist_ms_total,
            other_ms,
            likely_bound
        ));

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
        let mut spent_by_spend_txid: HashMap<String, (u64, Option<u64>)> = HashMap::new();

        for o in &working_outputs {
            if o.spent {
                if let Some(spend_txid_bytes) = o.spending_txid {
                    let spend_txid = hex_lowercase(&spend_txid_bytes);
                    let entry = spent_by_spend_txid.entry(spend_txid).or_insert((0, None));
                    entry.0 = entry.0.saturating_add(o.amount);
                    if let Some(height) = o.spending_height {
                        entry.1 = Some(entry.1.unwrap_or(0).max(height));
                    }
                }
            }
        }

        for (spend_txid, (gross_amount, spending_height)) in spent_by_spend_txid {
            match computed_ledger.get_mut(&spend_txid) {
                Some(entry) => {
                    if entry.direction == "out" {
                        entry.amount = entry.amount.max(gross_amount);
                        if entry.height.is_none() || entry.height == Some(0) {
                            entry.height = spending_height;
                        }
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
                            height: spending_height,
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
            if entry.direction == "out" {
                if entry.is_pending {
                    entry.is_pending = false;
                }
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

    // Balances reflect currently spendable wallet value; spent outputs stay in history only.
    let mut total = 0u64;
    let mut unlocked = 0u64;
    for output in &working_outputs {
        if output.spent {
            continue;
        }
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

#[cfg(test)]
mod tests {
    use super::{decode_range_transaction, fetch_scannable_blocks_range_bin, RangeFetchError};
    use crate::support::RpcClient;
    use crate::BlockingRpcTransport;
    use monero_wallet::block::Block as MoneroBlock;
    use monero_wallet::transaction::{Input, Pruned, Timelock, Transaction, TransactionPrefix};

    fn synthetic_pruned_v2() -> (Vec<u8>, [u8; 32], [u8; 32]) {
        let transaction = Transaction::<Pruned>::V2 {
            prefix: TransactionPrefix {
                additional_timelock: Timelock::None,
                inputs: vec![Input::Gen(1)],
                outputs: vec![],
                extra: vec![],
            },
            proofs: None,
        };
        let prunable_hash = [0; 32];
        let transaction_hash = transaction
            .hash_with_prunable_hash(prunable_hash)
            .expect("v2 transaction should hash with a prunable hash");
        (transaction.serialize(), prunable_hash, transaction_hash)
    }

    #[test]
    fn decodes_and_verifies_pruned_range_transaction() {
        let (blob, prunable_hash, transaction_hash) = synthetic_pruned_v2();
        let decoded = decode_range_transaction(&blob, Some(prunable_hash), transaction_hash, 0, 0)
            .expect("valid pruned transaction should decode");

        assert_eq!(decoded.serialize(), blob);
    }

    #[test]
    fn requests_unpruned_retry_when_prunable_hash_is_missing() {
        let (blob, _, transaction_hash) = synthetic_pruned_v2();
        let result = decode_range_transaction(&blob, None, transaction_hash, 0, 0);

        assert!(matches!(result, Err(RangeFetchError::RetryUnpruned(_))));
    }

    #[test]
    fn rejects_pruned_transaction_hash_mismatch() {
        let (blob, prunable_hash, mut transaction_hash) = synthetic_pruned_v2();
        transaction_hash[0] ^= 1;
        let result = decode_range_transaction(&blob, Some(prunable_hash), transaction_hash, 0, 0);

        assert!(matches!(result, Err(RangeFetchError::Rpc(_))));
    }

    #[test]
    #[ignore = "requires a live wallet RPC node"]
    fn debug_live_range_fetch_against_local_daemon() {
        let base_url = std::env::var("WALLETCORE_TEST_NODE")
            .unwrap_or_else(|_| "http://127.0.0.1:18092".to_string());
        let client: RpcClient = crate::TOKIO_RUNTIME
            .block_on(async {
                monero_simple_request_rpc::SimpleRequestTransport::new(base_url.clone()).await
            })
            .expect("failed to build rpc client");

        let blocks = fetch_scannable_blocks_range_bin(&client, &base_url, 3_630_413, 3_630_437)
            .unwrap_or_else(|e| panic!("range fetch failed: {e}"));

        eprintln!("fetched scannable blocks={}", blocks.len());
        assert_eq!(blocks.len(), 25);
    }

    #[test]
    #[ignore = "requires a live wallet RPC node"]
    fn debug_live_range_fetch_ios_window_against_local_daemon() {
        let base_url = "http://127.0.0.1:18092";
        let client: RpcClient = crate::TOKIO_RUNTIME
            .block_on(async {
                monero_simple_request_rpc::SimpleRequestTransport::new(base_url.to_string()).await
            })
            .expect("failed to build rpc client");

        let blocks = fetch_scannable_blocks_range_bin(&client, base_url, 3_519_450, 3_519_474)
            .unwrap_or_else(|e| panic!("range fetch failed: {e}"));

        eprintln!("fetched iOS-window scannable blocks={}", blocks.len());
        assert_eq!(blocks.len(), 25);
    }

    #[test]
    #[ignore = "requires a live wallet RPC node"]
    fn debug_live_get_o_indexes_response_shape() {
        let base_url = "http://127.0.0.1:18092";
        let transport = BlockingRpcTransport::new(base_url).expect("transport init failed");
        let resp = transport
            .get_blocks_bin(3_630_413, 25, true)
            .expect("get_blocks_bin failed");

        let first = resp.blocks.first().expect("missing first block");
        let mut block_reader: &[u8] = first.block.as_slice();
        let block = MoneroBlock::read(&mut block_reader).expect("block decode failed");
        let tx_hash = *block.transactions.first().expect("missing first tx hash");

        let request = [
            b"\x01\x11\x01\x01\x01\x01\x02\x01".as_slice(),
            &[1u8],
            &[1 << 2],
            &[4u8],
            b"txid".as_slice(),
            &[10u8],
            &[32 << 2],
            &tx_hash,
        ]
        .concat();

        let tx_hash_hex: String = tx_hash.iter().map(|b| format!("{b:02x}")).collect();
        match ureq::post(&format!("{base_url}/get_o_indexes.bin"))
            .set("Content-Type", "application/octet-stream")
            .send(std::io::Cursor::new(&request))
        {
            Ok(response) => {
                let mut reader = response.into_reader();
                let mut response = Vec::new();
                std::io::Read::read_to_end(&mut reader, &mut response)
                    .expect("read response failed");

                std::fs::write("/tmp/get_o_indexes_first_tx.bin", &response)
                    .expect("write sample failed");
                eprintln!(
                    "get_o_indexes response bytes={} tx_hash={} prefix={}",
                    response.len(),
                    tx_hash_hex,
                    crate::support::bulk_bin::hex_dump_prefix(&response, 96)
                );
            }
            Err(ureq::Error::Status(code, response)) => {
                eprintln!(
                    "get_o_indexes unavailable status={} tx_hash={} url={}",
                    code,
                    tx_hash_hex,
                    response.get_url()
                );
                assert_eq!(code, 404);
            }
            Err(err) => panic!("get_o_indexes.bin request failed: {err}"),
        }
    }
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

    // Clear any stale cancellation request from a previous refresh before starting.
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
