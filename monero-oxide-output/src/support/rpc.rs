//! RPC/daemon client support types used across FFI modules.
//!
//! This module intentionally contains only small aliases and trait imports that
//! improve ergonomics for extracted FFI submodules (`crate::ffi::*`).
//! Keep business logic out of here.

/// The concrete daemon client type used throughout walletcore.
///
/// `SimpleRequestTransport::new(...)` returns a `MoneroDaemon<SimpleRequestTransport>` (the daemon
/// wrapper implements the monero-interface traits). We alias it so extracted modules don’t need to
/// know the generic parameters.
pub(crate) type RpcClient =
    monero_daemon_rpc::MoneroDaemon<monero_simple_request_rpc::SimpleRequestTransport>;

/// Common monero-interface traits we rely on for method resolution.
///
/// Note: traits must be in scope for method syntax (`client.fee_rate(...)`) to work.
pub(crate) use monero_interface::{
    ProvidesBlockchainMeta as _, ProvidesFeeRates as _, ProvidesScannableBlocks as _,
};

// Bring MoneroDaemon's JSON-RPC helper into scope for callers which need bespoke daemon methods
// (e.g. `is_key_image_spent` preflight).
pub(crate) use monero_daemon_rpc::MoneroDaemon as _;
