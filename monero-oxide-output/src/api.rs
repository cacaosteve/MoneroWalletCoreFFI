//! Safe Rust wrappers around the C ABI.
//!
//! iOS and Android keep calling `extern "C"`. Desktop (GPUI) uses this module
//! so it does not re-implement pointer/buffer plumbing.

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_int},
    ptr,
};

use serde::Deserialize;

use crate::{
    wallet_derive_subaddress_from_mnemonic, wallet_export_cache, wallet_generate_mnemonic_english,
    wallet_get_balance, wallet_get_balance_with_filter, wallet_import_cache,
    wallet_list_transfers_json, wallet_open_from_mnemonic, wallet_prepare_send,
    wallet_prepare_send_with_filter, wallet_prepare_sweep, wallet_prepare_sweep_with_filter,
    wallet_preview_fee, wallet_preview_fee_with_filter, wallet_preview_sweep,
    wallet_preview_sweep_with_filter, wallet_primary_address_from_mnemonic, wallet_refresh_async,
    wallet_refresh_cancel, wallet_relay_prepared, wallet_reset_tracked_outputs, wallet_send,
    wallet_set_gap_limit, wallet_sweep, wallet_sync_status, walletcore_free_cstr,
    walletcore_last_error_message, walletcore_version,
};

pub use crate::ffi::refresh::RefreshJob;

#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub total_piconero: u64,
    pub unlocked_piconero: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncStatus {
    pub chain_height: u64,
    pub chain_time: u64,
    pub last_refresh_timestamp: u64,
    pub last_scanned: u64,
    pub restore_height: u64,
}

const DEFAULT_RING_LEN: u8 = 16;

#[derive(Debug, Clone, Deserialize)]
pub struct SendResult {
    pub txid: String,
    pub fee: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SweepPreview {
    pub amount: u64,
    pub fee: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SweepResult {
    pub txid: String,
    pub amount: u64,
    pub fee: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayResult {
    pub txid: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PreparedTx {
    pub txid: String,
    pub amount: u64,
    pub fee: u64,
}

#[derive(Deserialize)]
struct FeeOnly {
    fee: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Transfer {
    pub txid: String,
    pub direction: String,
    pub amount: u64,
    pub fee: Option<u64>,
    pub height: Option<u64>,
    pub timestamp: Option<u64>,
    #[serde(default)]
    pub confirmations: u64,
    #[serde(default)]
    pub is_pending: bool,
}

fn fail(code: i32) -> Error {
    Error {
        code,
        message: take_last_error().unwrap_or_else(|| format!("walletcore error {code}")),
    }
}

fn check(code: c_int) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(fail(code))
    }
}

fn cstr(value: &str) -> Result<CString> {
    CString::new(value).map_err(|_| Error {
        code: -10,
        message: "argument contained an interior NUL".into(),
    })
}

fn take_last_error() -> Option<String> {
    take_string(walletcore_last_error_message())
}

pub fn last_error() -> Option<String> {
    take_last_error()
}

fn take_string(ptr: *mut c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    let value = unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    let _ = walletcore_free_cstr(ptr);
    Some(value)
}

fn write_into_buf(mut call: impl FnMut(*mut c_char, usize, *mut usize) -> c_int) -> Result<String> {
    let mut buf = vec![0u8; 1024];
    let mut written = 0usize;
    let rc = call(buf.as_mut_ptr() as *mut c_char, buf.len(), &mut written);
    if rc == -12 {
        buf.resize(written.saturating_add(2).max(2048), 0);
        written = 0;
        check(call(
            buf.as_mut_ptr() as *mut c_char,
            buf.len(),
            &mut written,
        ))?;
    } else {
        check(rc)?;
    }
    let end = written.min(buf.len().saturating_sub(1));
    let text = CStr::from_bytes_until_nul(&buf[..=end]).map_err(|_| Error {
        code: -16,
        message: "walletcore output was not NUL-terminated".into(),
    })?;
    Ok(text.to_string_lossy().into_owned())
}

pub fn version() -> String {
    take_string(walletcore_version()).unwrap_or_else(|| "walletcore".into())
}

pub fn generate_mnemonic_english() -> Result<String> {
    write_into_buf(|buf, len, written| wallet_generate_mnemonic_english(buf, len, written))
}

pub fn open_from_mnemonic(
    wallet_id: &str,
    mnemonic: &str,
    restore_height: u64,
    mainnet: bool,
) -> Result<()> {
    let id = cstr(wallet_id.trim())?;
    let seed = cstr(mnemonic.trim())?;
    check(wallet_open_from_mnemonic(
        id.as_ptr(),
        seed.as_ptr(),
        restore_height,
        u8::from(mainnet),
    ))
}

pub fn set_gap_limit(wallet_id: &str, gap_limit: u32) -> Result<()> {
    let id = cstr(wallet_id.trim())?;
    check(wallet_set_gap_limit(id.as_ptr(), gap_limit))
}

pub fn primary_address_from_mnemonic(mnemonic: &str, mainnet: bool) -> Result<String> {
    let seed = cstr(mnemonic.trim())?;
    write_into_buf(|buf, len, written| {
        wallet_primary_address_from_mnemonic(seed.as_ptr(), u8::from(mainnet), buf, len, written)
    })
}

pub fn derive_subaddress_from_mnemonic(
    mnemonic: &str,
    account_index: u32,
    subaddress_index: u32,
    mainnet: bool,
) -> Result<String> {
    let seed = cstr(mnemonic.trim())?;
    write_into_buf(|buf, len, written| {
        wallet_derive_subaddress_from_mnemonic(
            seed.as_ptr(),
            account_index,
            subaddress_index,
            u8::from(mainnet),
            buf,
            len,
            written,
        )
    })
}

pub fn refresh_async(wallet_id: &str, node_url: &str) -> Result<()> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    check(wallet_refresh_async(id.as_ptr(), node.as_ptr()))
}

pub fn refresh_job(wallet_id: &str) -> RefreshJob {
    crate::ffi::refresh::refresh_job(wallet_id.trim())
}

pub fn refresh_cancel(wallet_id: &str) -> Result<()> {
    let id = cstr(wallet_id.trim())?;
    check(wallet_refresh_cancel(id.as_ptr()))
}

pub fn sync_status(wallet_id: &str) -> Result<SyncStatus> {
    let id = cstr(wallet_id.trim())?;
    let mut status = SyncStatus {
        chain_height: 0,
        chain_time: 0,
        last_refresh_timestamp: 0,
        last_scanned: 0,
        restore_height: 0,
    };
    check(wallet_sync_status(
        id.as_ptr(),
        &mut status.chain_height,
        &mut status.chain_time,
        &mut status.last_refresh_timestamp,
        &mut status.last_scanned,
        &mut status.restore_height,
    ))?;
    Ok(status)
}

pub fn get_balance(wallet_id: &str) -> Result<Balance> {
    let id = cstr(wallet_id.trim())?;
    let mut total = 0u64;
    let mut unlocked = 0u64;
    check(wallet_get_balance(id.as_ptr(), &mut total, &mut unlocked))?;
    Ok(Balance {
        total_piconero: total,
        unlocked_piconero: unlocked,
    })
}

pub fn get_balance_for_subaddress(wallet_id: &str, subaddress_minor: u32) -> Result<Balance> {
    let id = cstr(wallet_id.trim())?;
    let filter = cstr(&filter_json(subaddress_minor))?;
    let mut total = 0u64;
    let mut unlocked = 0u64;
    check(wallet_get_balance_with_filter(
        id.as_ptr(),
        filter.as_ptr(),
        &mut total,
        &mut unlocked,
    ))?;
    Ok(Balance {
        total_piconero: total,
        unlocked_piconero: unlocked,
    })
}

pub fn list_transfers(wallet_id: &str) -> Result<Vec<Transfer>> {
    let id = cstr(wallet_id.trim())?;
    let json = take_string(wallet_list_transfers_json(id.as_ptr())).ok_or_else(|| fail(-13))?;
    serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("transfer JSON: {err}"),
    })
}

pub fn import_cache(wallet_id: &str, cache: &[u8]) -> Result<()> {
    if cache.is_empty() {
        return Err(Error {
            code: -11,
            message: "cache was empty".into(),
        });
    }
    let id = cstr(wallet_id.trim())?;
    check(wallet_import_cache(
        id.as_ptr(),
        cache.as_ptr(),
        cache.len(),
    ))
}

pub fn export_cache(wallet_id: &str) -> Result<Vec<u8>> {
    let id = cstr(wallet_id.trim())?;
    let mut needed = 0usize;
    let probe = wallet_export_cache(id.as_ptr(), ptr::null_mut(), 0, &mut needed);
    if probe != -12 && probe != 0 {
        return Err(fail(probe));
    }
    if needed == 0 {
        return Ok(Vec::new());
    }
    let mut buf = vec![0u8; needed];
    let mut written = 0usize;
    check(wallet_export_cache(
        id.as_ptr(),
        buf.as_mut_ptr(),
        buf.len(),
        &mut written,
    ))?;
    buf.truncate(written);
    Ok(buf)
}

pub fn reset_tracked_outputs(wallet_id: &str) -> Result<()> {
    let id = cstr(wallet_id.trim())?;
    check(wallet_reset_tracked_outputs(id.as_ptr()))
}

fn take_required(ptr: *mut c_char) -> Result<String> {
    take_string(ptr).ok_or_else(|| fail(-16))
}

pub fn has_unlocked_for_exact_send(amount: u64, fee: u64, unlocked: u64) -> bool {
    amount <= unlocked && fee <= unlocked.saturating_sub(amount)
}

fn dests_json(to_address: &str, amount_piconero: u64) -> Result<CString> {
    cstr(&format!(
        r#"[{{"address":{},"amount":{}}}]"#,
        serde_json::to_string(to_address.trim()).map_err(|err| Error {
            code: -10,
            message: format!("destination JSON: {err}"),
        })?,
        amount_piconero
    ))
}

fn filter_json(subaddress_minor: u32) -> String {
    format!(r#"{{"subaddress_minor":{subaddress_minor}}}"#)
}

pub fn preview_fee(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    amount_piconero: u64,
) -> Result<u64> {
    preview_fee_filtered(wallet_id, node_url, to_address, amount_piconero, None)
}

pub fn preview_fee_filtered(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    amount_piconero: u64,
    from_subaddress: Option<u32>,
) -> Result<u64> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let dests = dests_json(to_address, amount_piconero)?;
    let json = if let Some(minor) = from_subaddress {
        let filter = cstr(&filter_json(minor))?;
        take_required(wallet_preview_fee_with_filter(
            id.as_ptr(),
            node.as_ptr(),
            dests.as_ptr(),
            filter.as_ptr(),
            DEFAULT_RING_LEN,
        ))?
    } else {
        take_required(wallet_preview_fee(
            id.as_ptr(),
            node.as_ptr(),
            dests.as_ptr(),
            DEFAULT_RING_LEN,
        ))?
    };
    let parsed: FeeOnly = serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("fee JSON: {err}"),
    })?;
    Ok(parsed.fee)
}

pub fn preview_sweep(wallet_id: &str, node_url: &str, to_address: &str) -> Result<SweepPreview> {
    preview_sweep_filtered(wallet_id, node_url, to_address, None)
}

pub fn preview_sweep_filtered(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    from_subaddress: Option<u32>,
) -> Result<SweepPreview> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let dest = cstr(to_address.trim())?;
    let json = if let Some(minor) = from_subaddress {
        let filter = cstr(&filter_json(minor))?;
        take_required(wallet_preview_sweep_with_filter(
            id.as_ptr(),
            node.as_ptr(),
            dest.as_ptr(),
            filter.as_ptr(),
            DEFAULT_RING_LEN,
        ))?
    } else {
        take_required(wallet_preview_sweep(
            id.as_ptr(),
            node.as_ptr(),
            dest.as_ptr(),
            DEFAULT_RING_LEN,
        ))?
    };
    serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("sweep preview JSON: {err}"),
    })
}

pub fn send(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    amount_piconero: u64,
) -> Result<SendResult> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let dest = cstr(to_address.trim())?;
    let json = take_required(wallet_send(
        id.as_ptr(),
        node.as_ptr(),
        dest.as_ptr(),
        amount_piconero,
        DEFAULT_RING_LEN,
    ))?;
    serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("send JSON: {err}"),
    })
}

pub fn sweep(wallet_id: &str, node_url: &str, to_address: &str) -> Result<SweepResult> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let dest = cstr(to_address.trim())?;
    let json = take_required(wallet_sweep(
        id.as_ptr(),
        node.as_ptr(),
        dest.as_ptr(),
        DEFAULT_RING_LEN,
    ))?;
    serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("sweep JSON: {err}"),
    })
}

pub fn prepare_send(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    amount_piconero: u64,
) -> Result<String> {
    prepare_send_filtered(wallet_id, node_url, to_address, amount_piconero, None)
}

pub fn prepare_send_filtered(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    amount_piconero: u64,
    from_subaddress: Option<u32>,
) -> Result<String> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    if let Some(minor) = from_subaddress {
        let dests = dests_json(to_address, amount_piconero)?;
        let filter = cstr(&filter_json(minor))?;
        take_required(wallet_prepare_send_with_filter(
            id.as_ptr(),
            node.as_ptr(),
            dests.as_ptr(),
            filter.as_ptr(),
            DEFAULT_RING_LEN,
        ))
    } else {
        let dest = cstr(to_address.trim())?;
        take_required(wallet_prepare_send(
            id.as_ptr(),
            node.as_ptr(),
            dest.as_ptr(),
            amount_piconero,
            DEFAULT_RING_LEN,
        ))
    }
}

pub fn prepare_sweep(wallet_id: &str, node_url: &str, to_address: &str) -> Result<String> {
    prepare_sweep_filtered(wallet_id, node_url, to_address, None)
}

pub fn prepare_sweep_filtered(
    wallet_id: &str,
    node_url: &str,
    to_address: &str,
    from_subaddress: Option<u32>,
) -> Result<String> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let dest = cstr(to_address.trim())?;
    if let Some(minor) = from_subaddress {
        let filter = cstr(&filter_json(minor))?;
        take_required(wallet_prepare_sweep_with_filter(
            id.as_ptr(),
            node.as_ptr(),
            dest.as_ptr(),
            filter.as_ptr(),
            DEFAULT_RING_LEN,
        ))
    } else {
        take_required(wallet_prepare_sweep(
            id.as_ptr(),
            node.as_ptr(),
            dest.as_ptr(),
            DEFAULT_RING_LEN,
        ))
    }
}

pub fn parse_prepared(json: &str) -> Result<PreparedTx> {
    serde_json::from_str(json).map_err(|err| Error {
        code: -16,
        message: format!("prepared JSON: {err}"),
    })
}

pub fn relay_prepared(wallet_id: &str, node_url: &str, prepared_json: &str) -> Result<RelayResult> {
    let id = cstr(wallet_id.trim())?;
    let node = cstr(node_url.trim())?;
    let payload = cstr(prepared_json)?;
    let json = take_required(wallet_relay_prepared(
        id.as_ptr(),
        node.as_ptr(),
        payload.as_ptr(),
    ))?;
    serde_json::from_str(&json).map_err(|err| Error {
        code: -16,
        message: format!("relay JSON: {err}"),
    })
}
