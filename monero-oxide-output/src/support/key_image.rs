//! Shared key image derivation helpers (signer-aligned).
//!
//! Why this exists
//! --------------
//! We need a single, consistent key image derivation used across:
//! - refresh-time output bookkeeping (`TrackedOutput.key_image`),
//! - send-time spendability checks,
//! - mapping daemon `/is_key_image_spent` results back to tracked outpoints.
//!
//! The authoritative behavior is what the transaction signer uses. In the monero-oxide
//! wallet signer (`monero_wallet::send::SignableTransaction::sign`), the per-input secret key is:
//!
//!   x = a + key_offset
//!
//! and the key image is:
//!
//!   I = x * Hp(P)
//!
//! where `a` is the wallet's private spend scalar, `key_offset` comes from the owned output,
//! and `P` is the one-time public key for the output.
//!
//! IMPORTANT
//! ---------
//! This module intentionally mirrors the signer semantics using dalek scalar arithmetic for `x`,
//! to avoid mismatches caused by differing scalar representations or operator support.
//!
//! API notes
//! ---------
//! Callers still pass `view_scalar_ed` and subaddress indices for compatibility with older call
//! sites; they are intentionally unused because signer semantics do not include any `m` term at
//! this stage.

use monero_wallet::{ed25519, WalletOutput};

/// Derive the key image bytes for a `WalletOutput` using signer-aligned semantics.
///
/// Authoritative formula (monero-oxide signer):
/// - `x = a + key_offset`
/// - `I = x * Hp(P)`
///
/// Inputs:
/// - `wallet_out`: The owned output being tracked/spent.
/// - `spend_scalar_dalek`: The wallet's private spend scalar `a` as a dalek scalar (stored in core state).
/// - `view_scalar_ed`: Unused (kept for API compatibility).
/// - `subaddress_major`/`subaddress_minor`: Unused (kept for API compatibility).
///
/// Returns:
/// - `[u8; 32]` key image bytes.
pub(crate) fn derive_key_image_bytes(
    wallet_out: &WalletOutput,
    spend_scalar_dalek: curve25519_dalek::Scalar,
    _view_scalar_ed: ed25519::Scalar,
    _subaddress_major: u32,
    _subaddress_minor: u32,
) -> [u8; 32] {
    // key_offset: monero_wallet::ed25519::Scalar -> [u8; 32] -> dalek scalar
    let ko_bytes: [u8; 32] = <[u8; 32]>::from(wallet_out.key_offset());
    let ko_dalek = curve25519_dalek::Scalar::from_canonical_bytes(ko_bytes)
        .into_option()
        .unwrap_or(curve25519_dalek::Scalar::ZERO);

    // Signer semantics: x = a + key_offset
    let x = spend_scalar_dalek + ko_dalek;

    // Hp(P): biased_hash(compressed P) (monero-oxide ed25519 point hashing), then convert to dalek point.
    let p = wallet_out.key();
    let p_bytes = p.compress().to_bytes();
    let hp_p = ed25519::Point::biased_hash(p_bytes);
    let hp_p_bytes = hp_p.compress().to_bytes();

    use curve25519_dalek::traits::Identity as _;
    let hp_p_dalek = curve25519_dalek::edwards::CompressedEdwardsY(hp_p_bytes)
        .decompress()
        .unwrap_or(curve25519_dalek::EdwardsPoint::identity());

    // I = x * Hp(P)
    (hp_p_dalek * x).compress().to_bytes()
}
