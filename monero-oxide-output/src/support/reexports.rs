//! Internal support module re-exports for extracted FFI submodules.
//!
//! Keep this file focused on re-exporting crate-local helpers/types so `src/ffi/*`
//! modules can depend on a small, stable surface (`crate::support::*`) without
//! importing a long list from the crate root.
//!
//! Avoid putting business logic here.

#![allow(unused_imports)]

pub(crate) use crate::{
    // Broadcast helper (uses /send_raw_transaction).
    broadcast_send_raw_transaction,
    // Refresh helpers / toggles.
    build_stamp,
    bulk_fetch_batch_from_env,
    bulk_fetch_mode_from_env,
    bulk_mode_str,
    // Error handling.
    clear_last_error,
    // Transfer listing / export helpers (used by transfer FFI module).
    confirmations_for_height,
    derive_address_string,
    // Wallet send/preview helpers.
    fee_rate_max_per_weight_cap,
    fingerprint32,
    // Hex formatting helpers.
    hex_lowercase,
    // Send/sweep error classification helpers.
    is_failed_send_raw_tx_error,
    is_invalid_input_send_raw_tx_error,
    // Binary-decoy provider constructor.
    make_bin_decoy_daemon,
    map_rpc_error,
    master_keys_from_mnemonic_str,
    // Recent hash tracking (wallet2-style).
    push_recent_block_hash,
    record_error,
    refresh_cancelled_for_wallet,
    set_refresh_cancel_for_wallet,
    spend_log_every_n_batches_from_env,
    spend_log_every_n_blocks_from_env,
    // Shared key image derivation helper (used by refresh + send).
    support::key_image::derive_key_image_bytes,

    update_scan_progress,
    // Debugging helpers.
    walletcore_debug_dump_tracked_outputs,
    walletcore_debug_input_dump_enabled,
    walletcore_debug_spend_detect_enabled,
    walletcore_debug_target_height,
    walletcore_debug_target_txid,
    walletcore_debug_target_window,
    // Env config helpers.
    walletcore_decoy_mode_bin16,
    walletcore_decoy_probe_enabled,
    // Preview/send env toggles.
    walletcore_disable_decoys,
    walletcore_fee_priority,
    walletcore_input_select_mode,
    // Logging (module-friendly function; prefer this from submodules).
    walletcore_log_line,
    walletcore_send_bisect_enabled,
    walletcore_send_bisect_on_failed_enabled,
    // Sweep knobs / toggles.
    walletcore_sweep_bisect_enabled,
    walletcore_sweep_min_input_piconero,
    watch_key_image_from_env,
    watch_txid_from_env,
    // Core shared types.
    DaemonStatus,
    InputSelectMode,
    LedgerEntry,
    ObservedOutput,
    ObservedOutputsEnvelope,
    ObservedTransfer,
    PendingOutgoingTx,
    // Cache persistence types.
    PersistedWallet,
    // Refresh worker types used by refresh logic.
    RefreshWorkerResult,
    RefreshWorkerSpend,
    // Address index type used with Scanner::register_subaddress.
    SubaddressIndex,
    TrackedOutput,
    // Refresh constants / globals used by refresh path.
    PANIC_HOOK_INSTALLED,
    // Runtime + state.
    TOKIO_RUNTIME,
    WALLETCORE_LOG_VERSION,
    WALLET_STORE,
};
