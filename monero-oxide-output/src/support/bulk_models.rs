//! Wallet2-style bulk binary (EPEE / portable_storage) request/response models.
//!
//! This module hosts the structs + `cuprate_epee_encoding` builders for monerod binary endpoints:
//! - `/get_blocks.bin` (range-based: start_height/count/prune)  + response
//! - `/get_blocks_by_height.bin` (heights/prune)               + response
//! - `COMMAND_RPC_GET_BLOCKS_FAST` (wallet2-style fast blocks) + response
//!
//! These types were extracted from the historical monolithic `src/lib.rs` to keep file sizes
//! manageable and isolate the schema/decoding concerns.
//!
//! Notes:
//! - These models are crate-internal (`pub(crate)`).
//! - Decoding helpers used for debug / unknown-field skipping live in `support::bulk_bin`.
//! - `BlockCompleteEntry` depends on `support::bulk_bin` utilities to keep cursor alignment when
//!   daemons add unknown fields.
//!
//! Important:
//! - Some fields (notably `block_ids` in `GetBlocksFastBinRequest`) must match Monero C++
//!   serialization (`KV_SERIALIZE_CONTAINER_POD_AS_BLOB`) i.e. a packed blob rather than an array.

use bytes::{Buf, BufMut};
use cuprate_epee_encoding::{write_field, EpeeObject};

use crate::support::bulk_bin::{
    bulk_bin_debug_enabled, hex_dump_prefix, is_supported_blob_marker, read_epee_field_name,
    read_epee_len_prefixed_bytes, read_txs_typed_array_0x8c, skip_epee_value, skip_epee_varint_u64,
    try_decode_block_complete_entry_from_blob_payload,
};

/// Wallet2-style `COMMAND_RPC_GET_BLOCKS_FAST` (`/get_blocks.bin`) request model.
///
/// This endpoint is what `wallet2`/Feather use for fast wallet sync: it returns both:
/// - `blocks` (block blobs + pruned tx blobs)
/// - `output_indices` (per-transaction output indices), eliminating the need for `/get_o_indexes.bin`
///
/// We implement only the subset we need for scanning.
///
/// NOTE: Monerod supports both `/get_blocks.bin` and `/getblocks.bin`.
/// This crate may call one or the other depending on how the rest of the transport is wired.
/// The request *body schema* is what distinguishes this request from range-based `get_blocks.bin`.
#[derive(Clone, Debug)]
pub(crate) struct GetBlocksFastBinRequest {
    /// `COMMAND_RPC_GET_BLOCKS_FAST::request_t::requested_info`
    pub(crate) requested_info: u8,

    /// IMPORTANT: In Monero C++ this is serialized with `KV_SERIALIZE_CONTAINER_POD_AS_BLOB(block_ids)`.
    /// That means it's encoded as a single blob of bytes (32 * N) rather than a normal EPEE array.
    /// We represent it as a packed blob to match daemon expectations and avoid HTTP 400.
    pub(crate) block_ids: Vec<u8>,

    pub(crate) start_height: u64,
    pub(crate) prune: bool,
    pub(crate) no_miner_tx: bool,
    pub(crate) pool_info_since: u64,
    pub(crate) max_block_count: u64,
}

#[derive(Default)]
pub(crate) struct GetBlocksFastBinRequestBuilder {
    requested_info: Option<u8>,
    block_ids: Option<Vec<u8>>,
    start_height: Option<u64>,
    prune: Option<bool>,
    no_miner_tx: Option<bool>,
    pool_info_since: Option<u64>,
    max_block_count: Option<u64>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksFastBinRequest>
    for GetBlocksFastBinRequestBuilder
{
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "requested_info" => {
                self.requested_info = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "block_ids" => {
                // Packed POD blob (32 * N bytes)
                self.block_ids = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "start_height" => {
                self.start_height = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "prune" => {
                self.prune = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "no_miner_tx" => {
                self.no_miner_tx = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "pool_info_since" => {
                self.pool_info_since = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "max_block_count" => {
                self.max_block_count = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksFastBinRequest> {
        Ok(GetBlocksFastBinRequest {
            requested_info: self.requested_info.unwrap_or(0),
            block_ids: self.block_ids.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field block_ids missing")
            })?,
            start_height: self.start_height.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field start_height missing")
            })?,
            prune: self.prune.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field prune missing")
            })?,
            no_miner_tx: self.no_miner_tx.unwrap_or(false),
            pool_info_since: self.pool_info_since.unwrap_or(0),
            max_block_count: self.max_block_count.unwrap_or(0),
        })
    }
}

impl EpeeObject for GetBlocksFastBinRequest {
    type Builder = GetBlocksFastBinRequestBuilder;

    fn number_of_fields(&self) -> u64 {
        7
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.requested_info, "requested_info", w)?;
        // Packed POD blob (32 * N bytes), matching KV_SERIALIZE_CONTAINER_POD_AS_BLOB(block_ids)
        write_field(self.block_ids, "block_ids", w)?;
        write_field(self.start_height, "start_height", w)?;
        write_field(self.prune, "prune", w)?;
        write_field(self.no_miner_tx, "no_miner_tx", w)?;
        write_field(self.pool_info_since, "pool_info_since", w)?;
        write_field(self.max_block_count, "max_block_count", w)?;
        Ok(())
    }
}

/// Per-tx output indices (for `get_o_indexes` avoidance).
#[derive(Clone, Debug)]
pub(crate) struct TxOutputIndices {
    pub(crate) indices: Vec<u64>,
}

#[derive(Default)]
pub(crate) struct TxOutputIndicesBuilder {
    indices: Option<Vec<u64>>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<TxOutputIndices> for TxOutputIndicesBuilder {
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "indices" => {
                self.indices = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<TxOutputIndices> {
        Ok(TxOutputIndices {
            indices: self.indices.unwrap_or_default(),
        })
    }
}

impl EpeeObject for TxOutputIndices {
    type Builder = TxOutputIndicesBuilder;

    fn number_of_fields(&self) -> u64 {
        1
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.indices, "indices", w)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BlockOutputIndices {
    pub(crate) indices: Vec<TxOutputIndices>,
}

#[derive(Default)]
pub(crate) struct BlockOutputIndicesBuilder {
    indices: Option<Vec<TxOutputIndices>>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<BlockOutputIndices> for BlockOutputIndicesBuilder {
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "indices" => {
                self.indices = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<BlockOutputIndices> {
        Ok(BlockOutputIndices {
            indices: self.indices.unwrap_or_default(),
        })
    }
}

impl EpeeObject for BlockOutputIndices {
    type Builder = BlockOutputIndicesBuilder;

    fn number_of_fields(&self) -> u64 {
        1
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.indices, "indices", w)?;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct GetBlocksFastBinResponse {
    pub(crate) blocks: Vec<BlockCompleteEntry>,
    pub(crate) start_height: u64,
    pub(crate) current_height: u64,
    pub(crate) output_indices: Vec<BlockOutputIndices>,
    pub(crate) daemon_time: Option<u64>,
    pub(crate) pool_info_extent: Option<u8>,
    pub(crate) status: Option<String>,
    pub(crate) untrusted: Option<bool>,
}

#[derive(Default)]
pub(crate) struct GetBlocksFastBinResponseBuilder {
    blocks: Option<Vec<BlockCompleteEntry>>,
    start_height: Option<u64>,
    current_height: Option<u64>,
    output_indices: Option<Vec<BlockOutputIndices>>,
    daemon_time: Option<u64>,
    pool_info_extent: Option<u8>,
    status: Option<String>,
    untrusted: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksFastBinResponse>
    for GetBlocksFastBinResponseBuilder
{
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        // Targeted schema debugging for `/getblocks.bin` response decoding.
        if bulk_bin_debug_enabled() {
            println!("🧩 getblocks.bin response: decoding field={:?}", name);
        }

        match name {
            "blocks" => {
                // Prefer generic schema-driven decode.
                if r.has_remaining() {
                    let save = r.chunk();
                    let mut tmp: &[u8] = save;
                    match cuprate_epee_encoding::read_epee_value::<Vec<BlockCompleteEntry>, _>(
                        &mut tmp,
                    ) {
                        Ok(v) => {
                            let consumed = save.len().saturating_sub(tmp.len());
                            r.advance(consumed);
                            if bulk_bin_debug_enabled() {
                                println!(
                                    "🧩 getblocks.bin blocks: generic decode ok (count={})",
                                    v.len()
                                );
                            }
                            self.blocks = Some(v);
                            return Ok(true);
                        }
                        Err(e) => {
                            if bulk_bin_debug_enabled() {
                                println!(
                                    "🧩 getblocks.bin blocks: generic decode failed; falling back to manual parser: {}",
                                    e
                                );
                            }
                        }
                    }
                }

                // ---- Manual instrumentation / legacy fallback path ----
                if !r.has_remaining() {
                    return Err(cuprate_epee_encoding::error::Error::Format(
                        "getblocks.bin decode failed in field 'blocks': EOF (missing container marker)",
                    ));
                }

                let container_marker = r.get_u8();

                // Determine element count and (optional) typed-array element type name.
                let (n, typed_elem_type): (u64, Option<String>) = match container_marker {
                    // Plain array: [0x0d][elem_marker][len][elements...]
                    0x0d => {
                        if !r.has_remaining() {
                            return Err(cuprate_epee_encoding::error::Error::Format(
                                "getblocks.bin decode failed in field 'blocks': EOF (missing element marker)",
                            ));
                        }
                        let elem_marker = r.get_u8();
                        if elem_marker != 0x0c {
                            return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                                format!(
                                    "getblocks.bin decode failed in field 'blocks': unexpected element marker=0x{elem_marker:02x} (expected object marker 0x0c)"
                                )
                                .into_boxed_str(),
                            )));
                        }

                        let n = skip_epee_varint_u64(r).map_err(|e| {
                            cuprate_epee_encoding::error::Error::Format(Box::leak(
                                format!(
                                    "getblocks.bin decode failed in field 'blocks': failed to read array length: {e}"
                                )
                                .into_boxed_str(),
                            ))
                        })?;

                        (n, None)
                    }

                    // Typed array: [0x8c][len][schema_marker][type_name_len][type_name_bytes][elem_marker][elements...]
                    0x8c => {
                        let n = skip_epee_varint_u64(r).map_err(|e| {
                            cuprate_epee_encoding::error::Error::Format(Box::leak(
                                format!(
                                    "getblocks.bin decode failed in field 'blocks': failed to read typed-array length: {e}"
                                )
                                .into_boxed_str(),
                            ))
                        })?;

                        if !r.has_remaining() {
                            return Err(cuprate_epee_encoding::error::Error::Format(
                                "getblocks.bin decode failed in field 'blocks': EOF (missing typed-array schema marker)",
                            ));
                        }
                        let _schema_marker = r.get_u8();

                        let type_name_len = skip_epee_varint_u64(r).map_err(|e| {
                            cuprate_epee_encoding::error::Error::Format(Box::leak(
                                format!(
                                    "getblocks.bin decode failed in field 'blocks': failed to read typed-array type name length: {e}"
                                )
                                .into_boxed_str(),
                            ))
                        })?;
                        let type_name_len_usize = usize::try_from(type_name_len).map_err(|_| {
                            cuprate_epee_encoding::error::Error::Format(
                                "getblocks.bin decode failed in field 'blocks': typed-array type name length overflow",
                            )
                        })?;
                        if r.remaining() < type_name_len_usize {
                            return Err(cuprate_epee_encoding::error::Error::Format(
                                "getblocks.bin decode failed in field 'blocks': EOF reading typed-array type name",
                            ));
                        }
                        let type_name_bytes = r.copy_to_bytes(type_name_len_usize);
                        let type_name = std::str::from_utf8(&type_name_bytes)
                            .unwrap_or("")
                            .to_string();

                        if !r.has_remaining() {
                            return Err(cuprate_epee_encoding::error::Error::Format(
                                "getblocks.bin decode failed in field 'blocks': EOF (missing typed-array element marker)",
                            ));
                        }
                        let elem_marker = r.get_u8();

                        // Some daemons appear to use a different object marker for typed-array elements.
                        if elem_marker != 0x0c && elem_marker != 0x10 {
                            return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                                format!(
                                    "getblocks.bin decode failed in field 'blocks': unexpected typed-array element marker=0x{elem_marker:02x} (expected object marker 0x0c or 0x10)"
                                )
                                .into_boxed_str(),
                            )));
                        }

                        (
                            n,
                            Some(format!("{type_name}|elem_marker=0x{elem_marker:02x}")),
                        )
                    }

                    _ => {
                        return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!(
                                "getblocks.bin decode failed in field 'blocks': unexpected container marker=0x{container_marker:02x} (expected 0x0d or 0x8c)"
                            )
                            .into_boxed_str(),
                        )));
                    }
                };

                if bulk_bin_debug_enabled() {
                    if let Some(ref ty) = typed_elem_type {
                        println!(
                            "🧩 getblocks.bin blocks container: typed_array marker=0x8c elem_type={:?} len={}",
                            ty, n
                        );

                        let chunk = r.chunk();
                        if !chunk.is_empty() {
                            let hex = hex_dump_prefix(chunk, 64);
                            println!(
                                "🧩 getblocks.bin blocks element_stream_start bytes[0..{}]={}",
                                std::cmp::min(64, chunk.len()),
                                hex
                            );
                        } else {
                            println!("🧩 getblocks.bin blocks element_stream_start: (unavailable)");
                        }
                    } else {
                        println!(
                            "🧩 getblocks.bin blocks container: plain_array marker=0x0d len={}",
                            n
                        );
                    }
                }

                // Decode elements (manual best-effort).
                let blocks_elem_marker: u8 = typed_elem_type
                    .as_deref()
                    .and_then(|s| s.split("|elem_marker=0x").nth(1))
                    .and_then(|hex| u8::from_str_radix(&hex[..2.min(hex.len())], 16).ok())
                    .unwrap_or(0x0a);

                let savepoint: &[u8] = r.chunk();

                // --- Attempt 1: decode as `BlockCompleteEntry` objects ---
                let mut reader_obj: &[u8] = savepoint;
                let mut obj_out: Vec<BlockCompleteEntry> = Vec::with_capacity(n as usize);
                let mut object_decode_ok = true;

                for i in 0..n {
                    if bulk_bin_debug_enabled() {
                        println!(
                            "🧩 getblocks.bin blocks[{}]: object-decode start (remaining={})",
                            i,
                            reader_obj.len()
                        );
                        if !reader_obj.is_empty() {
                            let hex = hex_dump_prefix(reader_obj, 32);
                            println!(
                                "🧩 getblocks.bin blocks[{}]: object-decode peek bytes[0..{}]={}",
                                i,
                                std::cmp::min(32, reader_obj.len()),
                                hex
                            );
                        }
                    }

                    let fields = match skip_epee_varint_u64(&mut reader_obj) {
                        Ok(v) => v,
                        Err(e) => {
                            object_decode_ok = false;
                            if bulk_bin_debug_enabled() {
                                println!(
                                    "🧩 getblocks.bin blocks[{}]: object-decode failed reading field_count: {}",
                                    i, e
                                );
                            }
                            break;
                        }
                    };

                    let mut builder = BlockCompleteEntryBuilder::default();
                    for _ in 0..fields {
                        let name = match read_epee_field_name(&mut reader_obj) {
                            Ok(v) => v,
                            Err(e) => {
                                object_decode_ok = false;
                                if bulk_bin_debug_enabled() {
                                    println!(
                                        "🧩 getblocks.bin blocks[{}]: object-decode failed reading field name: {}",
                                        i, e
                                    );
                                }
                                break;
                            }
                        };

                        if let Err(e) = builder.add_field(&name, &mut reader_obj) {
                            object_decode_ok = false;
                            if bulk_bin_debug_enabled() {
                                println!(
                                    "🧩 getblocks.bin blocks[{}]: object-decode add_field({:?}) failed: {}",
                                    i, name, e
                                );
                            }
                            break;
                        }
                    }

                    if !object_decode_ok {
                        break;
                    }

                    let entry = match builder.finish() {
                        Ok(v) => v,
                        Err(e) => {
                            object_decode_ok = false;
                            if bulk_bin_debug_enabled() {
                                println!(
                                    "🧩 getblocks.bin blocks[{}]: object-decode finish failed: {}",
                                    i, e
                                );
                            }
                            break;
                        }
                    };

                    if bulk_bin_debug_enabled() {
                        println!(
                            "🧩 getblocks.bin blocks[{}]: object-decode ok (block_bytes={} tx_blobs={} pruned={})",
                            i,
                            entry.block.len(),
                            entry.txs.len(),
                            entry.pruned
                        );
                    }

                    obj_out.push(entry);
                }

                if object_decode_ok && obj_out.len() == n as usize {
                    let consumed = savepoint.len().saturating_sub(reader_obj.len());
                    r.advance(consumed);
                    self.blocks = Some(obj_out);
                    return Ok(true);
                }

                if bulk_bin_debug_enabled() {
                    println!(
                        "🧩 getblocks.bin blocks: object-decode failed; attempting blob fallback from savepoint (remaining={})",
                        savepoint.len()
                    );
                }

                // --- Attempt 2: decode as length-prefixed blob bytes ---
                const MAX_BLOCK_BYTES: usize = 10 * 1024 * 1024;

                if !is_supported_blob_marker(blocks_elem_marker) {
                    return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                        format!(
                            "getblocks.bin decode failed in field 'blocks': unsupported typed-array elem_marker=0x{blocks_elem_marker:02x}"
                        )
                        .into_boxed_str(),
                    )));
                }

                let mut reader_blob: &[u8] = savepoint;
                let mut out: Vec<BlockCompleteEntry> = Vec::with_capacity(n as usize);

                for i in 0..n {
                    if reader_blob.is_empty() {
                        return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!(
                                "getblocks.bin decode failed in field 'blocks': blocks[{i}] EOF (missing element bytes)"
                            )
                            .into_boxed_str(),
                        )));
                    }

                    if !reader_blob.is_empty() && reader_blob[0] == blocks_elem_marker {
                        reader_blob = &reader_blob[1..];
                    }

                    let blob_payload = read_epee_len_prefixed_bytes(
                        &mut reader_blob,
                        "getblocks.bin blocks(blob_payload/shared_marker)",
                    )?;

                    if blob_payload.len() > MAX_BLOCK_BYTES {
                        return Err(cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!(
                                "getblocks.bin decode failed in field 'blocks': blocks[{i}] element too large (len={} > {MAX_BLOCK_BYTES})",
                                blob_payload.len()
                            )
                            .into_boxed_str(),
                        )));
                    }

                    if let Some(entry) =
                        try_decode_block_complete_entry_from_blob_payload(&blob_payload)?
                    {
                        out.push(entry);
                    } else {
                        out.push(BlockCompleteEntry {
                            block: blob_payload,
                            txs: Vec::new(),
                            pruned: true,
                        });
                    }
                }

                let consumed = savepoint.len().saturating_sub(reader_blob.len());
                r.advance(consumed);

                self.blocks = Some(out);
                return Ok(true);
            }

            "start_height" => {
                self.start_height =
                    Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                        cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!("getblocks.bin decode failed in field 'start_height': {e}")
                                .into_boxed_str(),
                        ))
                    })?);
            }
            "current_height" => {
                self.current_height =
                    Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                        cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!("getblocks.bin decode failed in field 'current_height': {e}")
                                .into_boxed_str(),
                        ))
                    })?);
            }
            "output_indices" => {
                self.output_indices =
                    Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                        cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!("getblocks.bin decode failed in field 'output_indices': {e}")
                                .into_boxed_str(),
                        ))
                    })?);
            }
            "daemon_time" => {
                self.daemon_time =
                    Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                        cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!("getblocks.bin decode failed in field 'daemon_time': {e}")
                                .into_boxed_str(),
                        ))
                    })?);
            }
            "pool_info_extent" => {
                self.pool_info_extent =
                    Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                        cuprate_epee_encoding::error::Error::Format(Box::leak(
                            format!("getblocks.bin decode failed in field 'pool_info_extent': {e}")
                                .into_boxed_str(),
                        ))
                    })?);
            }
            "status" => {
                self.status = Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                    cuprate_epee_encoding::error::Error::Format(Box::leak(
                        format!("getblocks.bin decode failed in field 'status': {e}")
                            .into_boxed_str(),
                    ))
                })?);
            }
            "untrusted" => {
                self.untrusted = Some(cuprate_epee_encoding::read_epee_value(r).map_err(|e| {
                    cuprate_epee_encoding::error::Error::Format(Box::leak(
                        format!("getblocks.bin decode failed in field 'untrusted': {e}")
                            .into_boxed_str(),
                    ))
                })?);
            }

            _ => return Ok(false),
        }

        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksFastBinResponse> {
        Ok(GetBlocksFastBinResponse {
            blocks: self.blocks.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'blocks'")
            })?,
            start_height: self.start_height.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'start_height'")
            })?,
            current_height: self.current_height.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'current_height'")
            })?,
            output_indices: self.output_indices.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'output_indices'")
            })?,
            daemon_time: self.daemon_time,
            pool_info_extent: self.pool_info_extent,
            status: self.status,
            untrusted: self.untrusted,
        })
    }
}

impl EpeeObject for GetBlocksFastBinResponse {
    type Builder = GetBlocksFastBinResponseBuilder;

    fn number_of_fields(&self) -> u64 {
        8
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.blocks, "blocks", w)?;
        write_field(self.start_height, "start_height", w)?;
        write_field(self.current_height, "current_height", w)?;
        write_field(self.output_indices, "output_indices", w)?;
        if let Some(daemon_time) = self.daemon_time {
            write_field(daemon_time, "daemon_time", w)?;
        }
        if let Some(pool_info_extent) = self.pool_info_extent {
            write_field(pool_info_extent, "pool_info_extent", w)?;
        }
        if let Some(status) = self.status {
            write_field(status, "status", w)?;
        }
        if let Some(untrusted) = self.untrusted {
            write_field(untrusted, "untrusted", w)?;
        }
        Ok(())
    }
}

/// Request for monerod `/get_blocks_by_height.bin` (portable_storage / EPEE encoded).
#[derive(Clone, Debug)]
pub(crate) struct GetBlocksByHeightBinRequest {
    pub(crate) heights: Vec<u64>,
    pub(crate) prune: bool,
}

#[derive(Default)]
pub(crate) struct GetBlocksByHeightBinRequestBuilder {
    heights: Option<Vec<u64>>,
    prune: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksByHeightBinRequest>
    for GetBlocksByHeightBinRequestBuilder
{
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "heights" => {
                self.heights = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "prune" => {
                self.prune = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksByHeightBinRequest> {
        Ok(GetBlocksByHeightBinRequest {
            heights: self.heights.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field heights missing")
            })?,
            prune: self.prune.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field prune missing")
            })?,
        })
    }
}

impl EpeeObject for GetBlocksByHeightBinRequest {
    type Builder = GetBlocksByHeightBinRequestBuilder;

    fn number_of_fields(&self) -> u64 {
        2
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.heights, "heights", w)?;
        write_field(self.prune, "prune", w)?;
        Ok(())
    }
}

/// Range request for monerod `/get_blocks.bin` (portable_storage / EPEE encoded).
///
/// Supported:
/// - start_height: u64
/// - count: u64
/// - prune: bool
#[derive(Clone, Debug)]
pub(crate) struct GetBlocksBinRequest {
    pub(crate) start_height: u64,
    pub(crate) count: u64,
    pub(crate) prune: bool,
}

#[derive(Default)]
pub(crate) struct GetBlocksBinRequestBuilder {
    start_height: Option<u64>,
    count: Option<u64>,
    prune: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksBinRequest> for GetBlocksBinRequestBuilder {
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "start_height" => {
                self.start_height = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "count" => {
                self.count = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "prune" => {
                self.prune = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksBinRequest> {
        Ok(GetBlocksBinRequest {
            start_height: self.start_height.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field start_height missing")
            })?,
            count: self.count.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field count missing")
            })?,
            prune: self.prune.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field prune missing")
            })?,
        })
    }
}

impl EpeeObject for GetBlocksBinRequest {
    type Builder = GetBlocksBinRequestBuilder;

    fn number_of_fields(&self) -> u64 {
        3
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.start_height, "start_height", w)?;
        write_field(self.count, "count", w)?;
        write_field(self.prune, "prune", w)?;
        Ok(())
    }
}

/// Shared tx entry for binary block responses.
///
/// Used by `BlockCompleteEntry` decoding when daemons provide tx blobs explicitly as objects.
#[derive(Clone, Debug)]
pub(crate) struct TxBlobEntry {
    pub(crate) blob: Vec<u8>,
    pub(crate) prunable_hash: Option<[u8; 32]>,
}

#[derive(Default)]
pub(crate) struct TxBlobEntryBuilder {
    blob: Option<Vec<u8>>,
    prunable_hash: Option<[u8; 32]>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<TxBlobEntry> for TxBlobEntryBuilder {
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "blob" => {
                self.blob = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "prunable_hash" => {
                self.prunable_hash = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<TxBlobEntry> {
        Ok(TxBlobEntry {
            blob: self.blob.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("Required field blob missing")
            })?,
            prunable_hash: self.prunable_hash,
        })
    }
}

impl EpeeObject for TxBlobEntry {
    type Builder = TxBlobEntryBuilder;

    fn number_of_fields(&self) -> u64 {
        let mut n = 1; // blob
        if self.prunable_hash.is_some() {
            n += 1;
        }
        n
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.blob, "blob", w)?;
        if let Some(hash) = self.prunable_hash {
            write_field(hash, "prunable_hash", w)?;
        }
        Ok(())
    }
}

/// Shared block entry for `/get_blocks_by_height.bin` and (typically) `/get_blocks.bin`.
#[derive(Clone, Debug)]
pub(crate) struct BlockCompleteEntry {
    pub(crate) block: Vec<u8>,
    /// Some daemons (or prune modes) omit tx blobs in certain responses.
    /// When omitted, we treat it as an empty list.
    pub(crate) txs: Vec<TxBlobEntry>,
    /// Daemons include whether the entry is pruned.
    pub(crate) pruned: bool,
}

#[derive(Default)]
pub(crate) struct BlockCompleteEntryBuilder {
    block: Option<Vec<u8>>,
    txs: Option<Vec<TxBlobEntry>>,
    pruned: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<BlockCompleteEntry> for BlockCompleteEntryBuilder {
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "block" => {
                if bulk_bin_debug_enabled() {
                    let rem_before = r.remaining();
                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='block' remaining_before={}",
                        rem_before
                    );
                }

                self.block = Some(cuprate_epee_encoding::read_epee_value(r)?);

                if bulk_bin_debug_enabled() {
                    let rem_after = r.remaining();
                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='block' remaining_after={}",
                        rem_after
                    );
                }
            }

            "txs" => {
                if bulk_bin_debug_enabled() {
                    let rem_before = r.remaining();
                    let chunk = r.chunk();

                    let peek_marker = if rem_before > 0 && !chunk.is_empty() {
                        format!("0x{:02x}", chunk[0])
                    } else if rem_before > 0 {
                        "(unavailable)".to_string()
                    } else {
                        "(eof)".to_string()
                    };

                    if rem_before > 0 && !chunk.is_empty() && chunk[0] == 0x8c {
                        let dump_len = std::cmp::min(16, chunk.len());
                        println!(
                            "🧩 get_blocks(.bin) block_complete_entry: field='txs' marker=0x8c leading_bytes[0..{}]={}",
                            dump_len,
                            hex_dump_prefix(&chunk[..dump_len], dump_len)
                        );
                    }

                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='txs' remaining_before={} next_marker={}",
                        rem_before, peek_marker
                    );
                }

                // Some daemons encode `txs` with a typed-array marker (observed 0x8c + element type name "blob").
                // Parse it keyed by embedded element type name; fall back to generic decoder otherwise.
                let txs_value: Vec<Vec<u8>> = {
                    if r.has_remaining() {
                        let save = r.chunk();
                        let mut tmp: &[u8] = save;
                        match cuprate_epee_encoding::read_epee_value::<Vec<Vec<u8>>, _>(&mut tmp) {
                            Ok(v) => {
                                let consumed = save.len().saturating_sub(tmp.len());
                                r.advance(consumed);
                                Ok(v)
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        Err(cuprate_epee_encoding::error::Error::Format(
                            "get_blocks(.bin) block_complete_entry: txs: empty buffer",
                        ))
                    }
                }
                .or_else(|_| {
                    let chunk = r.chunk();
                    if !chunk.is_empty() && chunk[0] == 0x8c {
                        read_txs_typed_array_0x8c(r)
                    } else {
                        cuprate_epee_encoding::read_epee_value(r)
                    }
                })?;

                let tx_entries: Vec<TxBlobEntry> = txs_value
                    .into_iter()
                    .map(|blob| TxBlobEntry {
                        blob,
                        prunable_hash: None,
                    })
                    .collect();

                self.txs = Some(tx_entries);

                if bulk_bin_debug_enabled() {
                    let rem_after = r.remaining();
                    let chunk = r.chunk();
                    let peek_marker = if rem_after > 0 && !chunk.is_empty() {
                        format!("0x{:02x}", chunk[0])
                    } else if rem_after > 0 {
                        "(unavailable)".to_string()
                    } else {
                        "(eof)".to_string()
                    };

                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='txs' remaining_after={} next_marker={}",
                        rem_after, peek_marker
                    );
                }
            }

            // Be permissive with common field name variants observed across daemons / implementations.
            "txs_blob" | "txs_blobs" | "txs_bytes" | "txs_byte" | "txs_data" | "transactions" => {
                if bulk_bin_debug_enabled() {
                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field={:?} (normalized to 'txs')",
                        name
                    );
                }
                self.txs = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }

            "pruned" => {
                if bulk_bin_debug_enabled() {
                    let rem_before = r.remaining();
                    let peek_marker = if rem_before > 0 {
                        let chunk = r.chunk();
                        if !chunk.is_empty() {
                            format!("0x{:02x}", chunk[0])
                        } else {
                            "(unavailable)".to_string()
                        }
                    } else {
                        "(eof)".to_string()
                    };

                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='pruned' remaining_before={} next_marker={}",
                        rem_before, peek_marker
                    );
                }

                self.pruned = Some(cuprate_epee_encoding::read_epee_value(r)?);

                if bulk_bin_debug_enabled() {
                    let rem_after = r.remaining();
                    let peek_marker = if rem_after > 0 {
                        let chunk = r.chunk();
                        if !chunk.is_empty() {
                            format!("0x{:02x}", chunk[0])
                        } else {
                            "(unavailable)".to_string()
                        }
                    } else {
                        "(eof)".to_string()
                    };

                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: field='pruned' remaining_after={} next_marker={}",
                        rem_after, peek_marker
                    );
                }
            }

            _ => {
                // IMPORTANT: we must consume unknown values to keep the reader aligned.
                if bulk_bin_debug_enabled() {
                    let rem_before = r.remaining();
                    let peek_marker = if rem_before > 0 {
                        let chunk = r.chunk();
                        if !chunk.is_empty() {
                            format!("0x{:02x}", chunk[0])
                        } else {
                            "(unavailable)".to_string()
                        }
                    } else {
                        "(eof)".to_string()
                    };

                    println!(
                        "🧩 get_blocks(.bin) block_complete_entry: skipping unknown field {:?} (next_marker={} remaining_before_skip={})",
                        name, peek_marker, rem_before
                    );
                }

                skip_epee_value(r)?;
            }
        }

        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<BlockCompleteEntry> {
        let block = self.block.ok_or_else(|| {
            cuprate_epee_encoding::error::Error::Format("block_complete_entry missing 'block'")
        })?;

        let txs = self.txs.unwrap_or_default();
        let pruned = self.pruned.unwrap_or(false);

        if bulk_bin_debug_enabled() {
            println!(
                "🧩 get_blocks(.bin) block_complete_entry: decoded block_bytes={} tx_blobs={} pruned={}",
                block.len(),
                txs.len(),
                pruned
            );
        }

        Ok(BlockCompleteEntry { block, txs, pruned })
    }
}

impl EpeeObject for BlockCompleteEntry {
    type Builder = BlockCompleteEntryBuilder;

    fn number_of_fields(&self) -> u64 {
        3
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.block, "block", w)?;
        write_field(self.txs, "txs", w)?;
        write_field(self.pruned, "pruned", w)?;
        Ok(())
    }
}

/// Minimal response model for monerod `/get_blocks_by_height.bin`.
#[derive(Clone, Debug)]
pub(crate) struct GetBlocksByHeightBinResponse {
    pub(crate) blocks: Vec<BlockCompleteEntry>,
    pub(crate) status: Option<String>,
    pub(crate) untrusted: Option<bool>,
}

#[derive(Default)]
pub(crate) struct GetBlocksByHeightBinResponseBuilder {
    blocks: Option<Vec<BlockCompleteEntry>>,
    status: Option<String>,
    untrusted: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksByHeightBinResponse>
    for GetBlocksByHeightBinResponseBuilder
{
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "blocks" => {
                self.blocks = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "status" => {
                self.status = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "untrusted" => {
                self.untrusted = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksByHeightBinResponse> {
        Ok(GetBlocksByHeightBinResponse {
            blocks: self.blocks.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'blocks'")
            })?,
            status: self.status,
            untrusted: self.untrusted,
        })
    }
}

impl EpeeObject for GetBlocksByHeightBinResponse {
    type Builder = GetBlocksByHeightBinResponseBuilder;

    fn number_of_fields(&self) -> u64 {
        3
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.blocks, "blocks", w)?;
        if let Some(status) = self.status {
            write_field(status, "status", w)?;
        }
        if let Some(untrusted) = self.untrusted {
            write_field(untrusted, "untrusted", w)?;
        }
        Ok(())
    }
}

/// Minimal response model for monerod `/get_blocks.bin` (range-based).
#[derive(Clone, Debug)]
pub(crate) struct GetBlocksBinResponse {
    pub(crate) blocks: Vec<BlockCompleteEntry>,
    pub(crate) status: Option<String>,
    pub(crate) untrusted: Option<bool>,
}

#[derive(Default)]
pub(crate) struct GetBlocksBinResponseBuilder {
    blocks: Option<Vec<BlockCompleteEntry>>,
    status: Option<String>,
    untrusted: Option<bool>,
}

impl cuprate_epee_encoding::EpeeObjectBuilder<GetBlocksBinResponse>
    for GetBlocksBinResponseBuilder
{
    fn add_field<B: Buf>(
        &mut self,
        name: &str,
        r: &mut B,
    ) -> cuprate_epee_encoding::error::Result<bool> {
        match name {
            "blocks" => {
                self.blocks = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "status" => {
                self.status = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            "untrusted" => {
                self.untrusted = Some(cuprate_epee_encoding::read_epee_value(r)?);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn finish(self) -> cuprate_epee_encoding::error::Result<GetBlocksBinResponse> {
        Ok(GetBlocksBinResponse {
            blocks: self.blocks.ok_or_else(|| {
                cuprate_epee_encoding::error::Error::Format("response missing 'blocks'")
            })?,
            status: self.status,
            untrusted: self.untrusted,
        })
    }
}

impl EpeeObject for GetBlocksBinResponse {
    type Builder = GetBlocksBinResponseBuilder;

    fn number_of_fields(&self) -> u64 {
        3
    }

    fn write_fields<B: BufMut>(self, w: &mut B) -> cuprate_epee_encoding::error::Result<()> {
        write_field(self.blocks, "blocks", w)?;
        if let Some(status) = self.status {
            write_field(status, "status", w)?;
        }
        if let Some(untrusted) = self.untrusted {
            write_field(untrusted, "untrusted", w)?;
        }
        Ok(())
    }
}
