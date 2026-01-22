//! Shared key image derivation helpers.
//!
//! Rationale
//! ---------
//! We need `TrackedOutput.key_image` (persisted during refresh) to match the key images used when
//! constructing and signing transactions. If these differ, we cannot reliably:
//! - correlate daemon `is_key_image_spent` results back to wallet outputs,
//! - detect spends correctly,
//! - avoid false `double_spend` / `invalid_input` send failures.
//!
//! This module provides a single, shared derivation used by both refresh and send paths.
//!
//! Notes
//! -----
//! - This helper is aligned with the `monero-oxide` signer semantics used by
//!   `monero_wallet::send::SignableTransaction::sign`.
//! - In the signer, the input secret key is derived as: `x = a + key_offset`
//!   (no additional subaddress `m` term is added at this stage).
//! - `subaddress_major`/`subaddress_minor` are accepted for API compatibility but are not used.

use monero_wallet::{ed25519, WalletOutput};

/// Derive the key image bytes for a `WalletOutput`.
///
/// This is aligned with the signer implementation in `monero-oxide`:
/// the input secret key is `x = a + key_offset`, and the key image is:
/// `I = x * Hp(P)` where `P` is the one-time public key.
///
/// Inputs:
/// - `wallet_out`: The output being tracked/spent.
/// - `spend_scalar_dalek`: The wallet's private spend scalar `a` as a dalek scalar.
/// - `view_scalar_ed`: Kept for API compatibility; not used by this derivation.
/// - `subaddress_major`/`subaddress_minor`: Kept for API compatibility; not used by this derivation.
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
    // key_offset: monero_wallet::ed25519::Scalar -> [u8;32] -> dalek scalar
    let ko_bytes: [u8; 32] = <[u8; 32]>::from(wallet_out.key_offset());
    let ko_dalek = curve25519_dalek::Scalar::from_canonical_bytes(ko_bytes)
        .into_option()
        .unwrap_or(curve25519_dalek::Scalar::ZERO);

    // Signer semantics:
    // x = a + key_offset
    let x = spend_scalar_dalek + ko_dalek;

    // Hp(P)
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
