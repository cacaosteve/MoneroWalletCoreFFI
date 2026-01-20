//! Bulk binary (EPEE / portable_storage) decoding helpers.
//!
//! This module contains the “bulk bin decoding” utilities that were historically embedded
//! in `src/lib.rs` to support wallet2-style binary endpoints like `/getblocks.bin`.
//!
//! Design goals:
//! - Keep business logic out: these are decoding/inspection helpers only.
//! - Be defensive against malformed input (avoid panics, avoid unbounded loops).
//! - Keep APIs crate-internal (`pub(crate)`).
//!
//! Notes:
//! - We intentionally support only the marker shapes we observe from monerod in the wild.
//! - If we encounter new markers, fail fast with a clear error so we can extend safely.
//!
//! This file was created during refactoring to reduce the size of `src/lib.rs`.

use bytes::Buf;

/// One-time debug logging toggle for bulk binary decoding.
///
/// Enable via env var:
/// - `WALLETCORE_BULK_BIN_DEBUG=1`
///
/// This is cached at first successful “true” read to avoid repeated env reads on hot paths.
#[inline]
pub(crate) fn bulk_bin_debug_enabled() -> bool {
    // Intentionally not caching in a static here because this module is extracted from a larger
    // crate where the existing caching static may live elsewhere. If you want caching, wire a
    // shared AtomicBool in the crate root and call into it from here.
    //
    // For now, keep it simple and deterministic.
    std::env::var("WALLETCORE_BULK_BIN_DEBUG")
        .ok()
        .map(|s| s != "0")
        .unwrap_or(false)
}

/// Render a small hex dump of a byte prefix for diagnostics.
///
/// This is intentionally lightweight and does not allocate excessively beyond the output string.
pub(crate) fn hex_dump_prefix(bytes: &[u8], max_len: usize) -> String {
    let dump_len = std::cmp::min(max_len, bytes.len());
    let mut hex = String::new();
    for (i, b) in bytes[..dump_len].iter().enumerate() {
        if i > 0 {
            hex.push(' ');
        }
        // keep format stable for logs
        hex.push_str(&format!("{:02x}", b));
    }
    hex
}

/// Non-destructive peek of Monero portable_storage varint (LEB128-style) from a byte slice.
/// Returns `(value, bytes_used)` if the varint is well-formed and fits in `u64`.
pub(crate) fn peek_epee_varint_u64(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut out: u64 = 0;
    let mut shift: u32 = 0;

    for (i, &b) in bytes.iter().enumerate() {
        let low = (b & 0x7f) as u64;

        // Prevent overflow / nonsense shifts
        if shift >= 64 {
            return None;
        }

        out |= low.checked_shl(shift)? as u64;

        if (b & 0x80) == 0 {
            return Some((out, i + 1));
        }

        shift = shift.saturating_add(7);

        // Cap to a sane maximum number of bytes for u64.
        if i >= 9 {
            return None;
        }
    }

    None
}

/// Consume a portable_storage varint from a `Buf`.
pub(crate) fn skip_epee_varint_u64<B: Buf>(r: &mut B) -> cuprate_epee_encoding::error::Result<u64> {
    // Monero portable_storage uses a LEB128-style varint.
    let mut out: u64 = 0;
    let mut shift: u32 = 0;

    loop {
        if !r.has_remaining() {
            return Err(cuprate_epee_encoding::error::Error::Format(
                "skip_epee_varint_u64: EOF",
            ));
        }

        let b = r.get_u8();
        out |= u64::from(b & 0x7f) << shift;

        if (b & 0x80) == 0 {
            return Ok(out);
        }

        shift += 7;
        if shift >= 64 {
            return Err(cuprate_epee_encoding::error::Error::Format(
                "skip_epee_varint_u64: varint overflow",
            ));
        }
    }
}

/// Read a portable_storage field name (length-prefixed UTF-8 string).
pub(crate) fn read_epee_field_name<B: Buf>(
    r: &mut B,
) -> cuprate_epee_encoding::error::Result<String> {
    let name_len = skip_epee_varint_u64(r)?;
    let name_len_usize = usize::try_from(name_len).map_err(|_| {
        cuprate_epee_encoding::error::Error::Format("read_epee_field_name: name length overflow")
    })?;

    if r.remaining() < name_len_usize {
        return Err(cuprate_epee_encoding::error::Error::Format(
            "read_epee_field_name: EOF reading field name",
        ));
    }

    let bytes = r.copy_to_bytes(name_len_usize);
    let s = std::str::from_utf8(&bytes).map_err(|_| {
        cuprate_epee_encoding::error::Error::Format("read_epee_field_name: invalid UTF-8")
    })?;

    Ok(s.to_string())
}

/// Read a portable_storage length-prefixed byte sequence.
///
/// `ctx` is included in any error messages for easier debugging.
pub(crate) fn read_epee_len_prefixed_bytes<B: Buf>(
    r: &mut B,
    ctx: &'static str,
) -> cuprate_epee_encoding::error::Result<Vec<u8>> {
    let len = skip_epee_varint_u64(r)?;
    let len_usize = usize::try_from(len).map_err(|_| {
        cuprate_epee_encoding::error::Error::Format(Box::leak(
            format!("{ctx}: length overflow").into_boxed_str(),
        ))
    })?;

    if r.remaining() < len_usize {
        return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
            format!("{ctx}: EOF reading bytes").into_boxed_str(),
        )));
    }

    Ok(r.copy_to_bytes(len_usize).to_vec())
}

/// Portable_storage "string/blob-like" markers we have observed from monerod in the wild.
///
/// `0x0a` / `0x0b` are classic string/blob markers.
/// We also treat `0xba` / `0xcf` as blob-like based on observed tx blob encodings.
#[inline]
pub(crate) fn is_supported_blob_marker(marker: u8) -> bool {
    matches!(marker, 0x0a | 0x0b | 0xba | 0xcf)
}

// -------------------------
// Generic EPEE value skipping
// -------------------------
//
// `cuprate_epee_encoding` object builders call `add_field(name, reader)` for each field.
// If we encounter an unknown field, we MUST consume its value to keep the reader aligned.
// Otherwise subsequent reads can fail with "Marker does not match expected Marker".
//
// This helper implements a generic skipper for EPEE-encoded values.
//
// It is intentionally conservative and only supports the marker kinds we actually see from monerod.
// If we encounter an unsupported marker, we return a Format error so we can extend support safely.

pub(crate) fn skip_epee_value<B: Buf>(r: &mut B) -> cuprate_epee_encoding::error::Result<()> {
    if !r.has_remaining() {
        return Err(cuprate_epee_encoding::error::Error::Format(
            "skip_epee_value: unexpected EOF (no marker)",
        ));
    }

    let marker = r.get_u8();

    match marker {
        // Bool (1 byte)
        0x01 => {
            if r.remaining() < 1 {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value: EOF reading bool",
                ));
            }
            let _ = r.get_u8();
            Ok(())
        }

        // Fixed-width ints (best-effort; monerod frequently uses 8 byte ints)
        0x02 | 0x03 => {
            if r.remaining() < 8 {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value: EOF reading int64",
                ));
            }
            r.advance(8);
            Ok(())
        }

        // Strings/blobs: varint length + bytes
        0x0a | 0x0b | 0xba | 0xcf => {
            let len = skip_epee_varint_u64(r)?;
            let len_usize = usize::try_from(len).map_err(|_| {
                cuprate_epee_encoding::error::Error::Format("skip_epee_value: length overflow")
            })?;
            if r.remaining() < len_usize {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value: EOF reading bytes",
                ));
            }
            r.advance(len_usize);
            Ok(())
        }

        // Object: varint field count + repeated (name,value) pairs
        0x0c => {
            let fields = skip_epee_varint_u64(r)?;
            for _ in 0..fields {
                let name_len = skip_epee_varint_u64(r)?;
                let name_len_usize = usize::try_from(name_len).map_err(|_| {
                    cuprate_epee_encoding::error::Error::Format(
                        "skip_epee_value: name length overflow",
                    )
                })?;
                if r.remaining() < name_len_usize {
                    return Err(cuprate_epee_encoding::error::Error::Format(
                        "skip_epee_value: EOF reading field name",
                    ));
                }
                r.advance(name_len_usize);

                skip_epee_value(r)?;
            }
            Ok(())
        }

        // Array: element marker + varint length + elements.
        // Observed: `txs` can start with marker 0x8c; treat it array-like.
        0x0d | 0x8c => {
            if !r.has_remaining() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value: EOF reading array element marker",
                ));
            }
            let elem_marker = r.get_u8();
            let n = skip_epee_varint_u64(r)?;
            for _ in 0..n {
                skip_epee_value_with_known_marker(r, elem_marker)?;
            }
            Ok(())
        }

        _ => Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
            format!("skip_epee_value: unsupported marker=0x{marker:02x} (extend decoder)")
                .into_boxed_str(),
        ))),
    }
}

pub(crate) fn skip_epee_value_with_known_marker<B: Buf>(
    r: &mut B,
    marker: u8,
) -> cuprate_epee_encoding::error::Result<()> {
    match marker {
        0x01 => {
            if r.remaining() < 1 {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: EOF reading bool",
                ));
            }
            let _ = r.get_u8();
            Ok(())
        }

        0x02 | 0x03 => {
            if r.remaining() < 8 {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: EOF reading int64",
                ));
            }
            r.advance(8);
            Ok(())
        }

        0x0a | 0x0b | 0xba | 0xcf => {
            let len = skip_epee_varint_u64(r)?;
            let len_usize = usize::try_from(len).map_err(|_| {
                cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: length overflow",
                )
            })?;
            if r.remaining() < len_usize {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: EOF reading bytes",
                ));
            }
            r.advance(len_usize);
            Ok(())
        }

        0x0c => {
            let fields = skip_epee_varint_u64(r)?;
            for _ in 0..fields {
                let name_len = skip_epee_varint_u64(r)?;
                let name_len_usize = usize::try_from(name_len).map_err(|_| {
                    cuprate_epee_encoding::error::Error::Format(
                        "skip_epee_value_with_known_marker: name length overflow",
                    )
                })?;
                if r.remaining() < name_len_usize {
                    return Err(cuprate_epee_encoding::error::Error::Format(
                        "skip_epee_value_with_known_marker: EOF reading field name",
                    ));
                }
                r.advance(name_len_usize);

                skip_epee_value(r)?;
            }
            Ok(())
        }

        0x0d | 0x8c => {
            if !r.has_remaining() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: EOF reading nested array elem marker",
                ));
            }
            let elem_marker = r.get_u8();
            let n = skip_epee_varint_u64(r)?;
            for _ in 0..n {
                skip_epee_value_with_known_marker(r, elem_marker)?;
            }
            Ok(())
        }

        _ => {
            // Tolerant fallback: treat unknown markers as blob-like with a varint length.
            let len = skip_epee_varint_u64(r)?;
            let len_usize = usize::try_from(len).map_err(|_| {
                cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: length overflow (unknown marker)",
                )
            })?;
            if r.remaining() < len_usize {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "skip_epee_value_with_known_marker: EOF reading bytes (unknown marker)",
                ));
            }
            r.advance(len_usize);
            Ok(())
        }
    }
}

// -------------------------
// Typed-array parser for observed tx blob encoding (marker 0x8c)
// -------------------------

/// Spec-driven typed-array parser for the observed `txs` encoding in wallet2 `/getblocks.bin`.
///
/// Observed:
/// - marker 0x8c
/// - varint count
/// - schema header with element type name (e.g. `"blob"`)
/// - then N elements encoded as length-prefixed byte blobs
///
/// If we encounter an unexpected element type, we skip elements generically to keep cursor aligned.
///
/// Important: this helper focuses on maintaining cursor alignment and extracting bytes.
/// Interpretation of tx blobs is done elsewhere.
pub(crate) fn read_txs_typed_array_0x8c<B: Buf>(
    r: &mut B,
) -> cuprate_epee_encoding::error::Result<Vec<Vec<u8>>> {
    // Diagnostics: dump container start bytes (helps reverse-engineer layouts).
    if bulk_bin_debug_enabled() {
        let chunk0 = r.chunk();
        if !chunk0.is_empty() {
            println!(
                "🧩 txs(0x8c) dump@container_start bytes[0..{}]={}",
                std::cmp::min(64, chunk0.len()),
                hex_dump_prefix(chunk0, 64)
            );
        }
    }

    if !r.has_remaining() {
        return Err(cuprate_epee_encoding::error::Error::Format(
            "read_txs_typed_array_0x8c: EOF (missing marker)",
        ));
    }

    let marker = r.get_u8();
    if marker != 0x8c {
        return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
            format!("read_txs_typed_array_0x8c: unexpected marker=0x{marker:02x}").into_boxed_str(),
        )));
    }

    // 1) Element count
    let n_u64 = skip_epee_varint_u64(r)?;
    let n = usize::try_from(n_u64).map_err(|_| {
        cuprate_epee_encoding::error::Error::Format("read_txs_typed_array_0x8c: count overflow")
    })?;

    // 2) Typed-array schema header:
    // We observed bytes like: 08 04 'blob' ...
    // Interpret this as: <schema_marker:u8> <type_name_len:varint> <type_name_bytes>.
    if !r.has_remaining() {
        return Err(cuprate_epee_encoding::error::Error::Format(
            "read_txs_typed_array_0x8c: EOF (missing schema marker)",
        ));
    }

    let _schema_marker = r.get_u8();
    let type_name_len = skip_epee_varint_u64(r)?;
    let type_name_len_usize = usize::try_from(type_name_len).map_err(|_| {
        cuprate_epee_encoding::error::Error::Format(
            "read_txs_typed_array_0x8c: type name length overflow",
        )
    })?;

    if r.remaining() < type_name_len_usize {
        return Err(cuprate_epee_encoding::error::Error::Format(
            "read_txs_typed_array_0x8c: EOF reading type name",
        ));
    }

    let type_name_bytes = r.copy_to_bytes(type_name_len_usize);
    let elem_type = std::str::from_utf8(&type_name_bytes)
        .unwrap_or("")
        .to_string();

    if bulk_bin_debug_enabled() {
        let chunk1 = r.chunk();
        if !chunk1.is_empty() {
            println!(
                "🧩 txs(0x8c) dump@element_stream_start elem_type={:?} count={} bytes[0..{}]={}",
                elem_type,
                n,
                std::cmp::min(64, chunk1.len()),
                hex_dump_prefix(chunk1, 64)
            );
        }
    }

    // 3) Decode elements
    let mut out: Vec<Vec<u8>> = Vec::with_capacity(n);

    if elem_type == "blob" {
        // Fast path: attempt generic decode of Vec<Vec<u8>> without committing to consumption.
        // Note: this depends on cuprate's decoder behavior. If it fails, we fall back to manual parsing.
        if r.has_remaining() {
            let save = r.chunk();
            let mut tmp: &[u8] = save;
            if let Ok(v) = cuprate_epee_encoding::read_epee_value::<Vec<Vec<u8>>, _>(&mut tmp) {
                let consumed = save.len().saturating_sub(tmp.len());
                r.advance(consumed);
                return Ok(v);
            }
        }

        for _ in 0..n {
            if !r.has_remaining() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "read_txs_typed_array_0x8c(blob): EOF reading element",
                ));
            }

            let chunk = r.chunk();
            if chunk.is_empty() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "read_txs_typed_array_0x8c(blob): unable to peek element bytes",
                ));
            }

            // Accept both marker-present and markerless blob encodings; be tolerant to avoid desync.

            // Case A: [marker][varint_len][bytes...]
            if chunk.len() >= 2 {
                if let Some((len, used)) = peek_epee_varint_u64(&chunk[1..]) {
                    let rem_after_marker = r.remaining().saturating_sub(1);
                    if (used as u64) <= rem_after_marker as u64
                        && len <= rem_after_marker.saturating_sub(used) as u64
                    {
                        let _ = r.get_u8();
                        let b = read_epee_len_prefixed_bytes(
                            r,
                            "read_txs_typed_array_0x8c(blob,marker_any)",
                        )?;
                        out.push(b);
                        continue;
                    }
                }
            }

            // Case B: [varint_len][bytes...]
            if let Some((len, used)) = peek_epee_varint_u64(chunk) {
                let rem = r.remaining();

                if (used as u64) <= rem as u64 && len <= rem.saturating_sub(used) as u64 {
                    let b = read_epee_len_prefixed_bytes(
                        r,
                        "read_txs_typed_array_0x8c(blob,markerless)",
                    )?;
                    out.push(b);
                    continue;
                }

                // Tolerant path: if declared len is larger than remaining, consume what remains.
                if (used as u64) <= rem as u64 && len > rem.saturating_sub(used) as u64 {
                    r.advance(used);
                    let rem_now = r.remaining();
                    let mut b = Vec::with_capacity(rem_now);
                    b.extend_from_slice(r.copy_to_bytes(rem_now).as_ref());
                    out.push(b);
                    continue;
                }
            }

            // Tolerant fallback: consume marker if present, then treat the remaining bytes as one payload.
            if chunk.len() > 1 {
                let _ = r.get_u8();
            }
            let remaining = r.remaining();
            let b = if remaining > 0 {
                let mut v = Vec::with_capacity(remaining);
                v.extend_from_slice(r.copy_to_bytes(remaining).as_ref());
                v
            } else {
                Vec::new()
            };
            out.push(b);
        }
    } else {
        // Unknown element type: keep cursor aligned by skipping each element generically.
        for _ in 0..n {
            if !r.has_remaining() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "read_txs_typed_array_0x8c: EOF skipping element",
                ));
            }

            let chunk = r.chunk();
            if chunk.is_empty() {
                return Err(cuprate_epee_encoding::error::Error::Format(
                    "read_txs_typed_array_0x8c: unable to peek element marker",
                ));
            }

            let m = chunk[0];
            let _ = r.get_u8();
            skip_epee_value_with_known_marker(r, m)?;
            out.push(Vec::new());
        }
    }

    Ok(out)
}

/// Try to decode a `BlockCompleteEntry` object from a blob payload.
///
/// Some daemons appear to encode `blocks` as a typed array whose elements are *blobs*, where each blob
/// is itself a portable_storage object payload for `block_complete_entry`.
///
/// Returns:
/// - `Ok(Some(entry))` if the blob payload decodes as a `BlockCompleteEntry`
/// - `Ok(None)` if it does not look like a valid entry (so caller can treat payload as raw bytes)
/// - `Err(e)` only for hard format errors we want to surface
pub(crate) fn try_decode_block_complete_entry_from_blob_payload(
    payload: &[u8],
) -> cuprate_epee_encoding::error::Result<Option<crate::support::bulk_models::BlockCompleteEntry>> {
    if payload.is_empty() {
        return Ok(None);
    }

    // Attempt to decode as an object payload:
    // [field_count varint] then repeated [field_name][field_value]
    let mut r: &[u8] = payload;

    let fields = match skip_epee_varint_u64(&mut r) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    // Defensive: reject obviously insane field counts (avoid huge loops on garbage data).
    if fields > 1000 {
        return Ok(None);
    }

    let mut builder = crate::support::bulk_models::BlockCompleteEntryBuilder::default();

    for _ in 0..fields {
        let name = match read_epee_field_name(&mut r) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };

        match <crate::support::bulk_models::BlockCompleteEntryBuilder as cuprate_epee_encoding::EpeeObjectBuilder<
            crate::support::bulk_models::BlockCompleteEntry,
        >>::add_field(&mut builder, &name, &mut r)
        {
            Ok(true) => {}
            Ok(false) => {
                // Unknown field: we must still skip its value. The builder didn't consume it,
                // so consume it here by reading the marker and skipping the value.
                if !r.has_remaining() {
                    return Ok(None);
                }
                let marker = r.get_u8();
                skip_epee_value_with_known_marker(&mut r, marker)?;
            }
            Err(_) => return Ok(None),
        }
    }

    match <crate::support::bulk_models::BlockCompleteEntryBuilder as cuprate_epee_encoding::EpeeObjectBuilder<
        crate::support::bulk_models::BlockCompleteEntry,
    >>::finish(builder)
    {
        Ok(entry) => Ok(Some(entry)),
        Err(_) => Ok(None),
    }
}
