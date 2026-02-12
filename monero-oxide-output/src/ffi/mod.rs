//! FFI submodules extracted from the historical mega-`lib.rs`.
//!
//! Each submodule should expose `#[no_mangle] pub extern "C"` functions that are
//! re-exported from the crate root (`lib.rs`) to preserve the public ABI.

pub(crate) mod cache;
pub(crate) mod mnemonic;
pub(crate) mod preview_fee;
pub(crate) mod refresh;
pub(crate) mod send;
pub(crate) mod sweep;
pub(crate) mod transfers;
