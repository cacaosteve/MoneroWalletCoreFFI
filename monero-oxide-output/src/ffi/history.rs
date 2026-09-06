//! Local, bounded history queries. No RPC and no persistent cache schema change.
use crate::*;
use std::ptr;
use std::sync::atomic::AtomicU64;

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HistoryFilter {
    #[default]
    All,
    Received,
    Sent,
    Pending,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HistoryQuery {
    pub limit: usize,
    pub offset: usize,
    pub revision: Option<String>,
    pub filter: HistoryFilter,
    pub search: String,
    /// Inclusive UTC seconds. Unknown timestamps do not match a date range.
    pub from_timestamp: Option<u64>,
    pub to_timestamp: Option<u64>,
    /// Exact detail lookup (independent of pagination).
    pub txid: Option<String>,
    /// Locate this row in a new revision and return its containing page (reload only).
    pub anchor_txid: Option<String>,
}
impl Default for HistoryQuery {
    fn default() -> Self {
        Self {
            limit: 50,
            offset: 0,
            revision: None,
            filter: HistoryFilter::All,
            search: String::new(),
            from_timestamp: None,
            to_timestamp: None,
            txid: None,
            anchor_txid: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryPage {
    pub schema_version: u32,
    pub wallet_id: String,
    pub revision: String,
    pub total_count: usize,
    pub matching_count: usize,
    pub pending_count: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub anchor_offset: Option<usize>,
    pub last_scanned_height: u64,
    pub chain_height: u64,
    pub chain_time: u64,
    pub transfers: Vec<api::Transfer>,
}

/// Shared by refresh snapshots using Arc. Rebuilt only when ledger content/identity changes,
/// never merely because confirmations or the scan cursor advanced. A content comparison is
/// deliberate: all existing send, cache-import, and reorg mutation paths are covered.
pub(crate) struct HistoryIndex {
    identity: String,
    revision: String,
    ledger: HashMap<String, LedgerEntry>,
    ordered: Vec<String>,
    pending: usize,
}
impl HistoryIndex {
    fn new(identity: String, ledger: &HashMap<String, LedgerEntry>) -> Self {
        let mut ordered: Vec<_> = ledger.keys().cloned().collect();
        ordered.sort_by(|a, b| {
            let (a, b) = (&ledger[a], &ledger[b]);
            b.is_pending
                .cmp(&a.is_pending)
                .then_with(|| b.height.unwrap_or(0).cmp(&a.height.unwrap_or(0)))
                .then_with(|| b.timestamp.unwrap_or(0).cmp(&a.timestamp.unwrap_or(0)))
                .then_with(|| a.txid.cmp(&b.txid))
        });
        Self {
            identity,
            revision: NEXT_REVISION.fetch_add(1, Ordering::Relaxed).to_string(),
            ledger: ledger.clone(),
            ordered,
            pending: ledger.values().filter(|t| t.is_pending).count(),
        }
    }

    fn query(
        &self,
        wallet_id: &str,
        query: &HistoryQuery,
        scanned: u64,
        height: u64,
        time: u64,
    ) -> Result<HistoryPage, String> {
        if !(1..=200).contains(&query.limit)
            || query.search.len() > 256
            || query.txid.as_ref().is_some_and(|s| s.len() > 64)
            || query.anchor_txid.as_ref().is_some_and(|s| s.len() > 64)
        {
            return Err(
                "invalid_history_query: limit must be 1..200; search must be bounded".into(),
            );
        }
        if query
            .from_timestamp
            .zip(query.to_timestamp)
            .is_some_and(|(a, b)| a > b)
        {
            return Err("invalid_history_query: date range is reversed".into());
        }
        if query.revision.as_ref().is_some_and(|r| r != &self.revision) {
            return Err("stale_history_cursor: history changed; reload the query".into());
        }
        if query.offset > 0 && query.revision.is_none() {
            return Err("invalid_history_query: continuation requires revision".into());
        }
        let search = query.search.trim().to_ascii_lowercase();
        let matches = |entry: &&LedgerEntry| {
            let t = *entry;
            (match query.filter {
                HistoryFilter::All => true,
                HistoryFilter::Received => t.direction == "in",
                HistoryFilter::Sent => t.direction == "out",
                HistoryFilter::Pending => t.is_pending,
            }) && t.txid.to_ascii_lowercase().contains(&search)
                && query
                    .txid
                    .as_ref()
                    .is_none_or(|id| t.txid.eq_ignore_ascii_case(id))
                && query
                    .from_timestamp
                    .is_none_or(|from| t.timestamp.is_some_and(|ts| ts >= from))
                && query
                    .to_timestamp
                    .is_none_or(|to| t.timestamp.is_some_and(|ts| ts <= to))
        };
        let anchor_offset = query.anchor_txid.as_ref().and_then(|anchor| {
            self.ordered
                .iter()
                .map(|id| &self.ledger[id])
                .filter(matches)
                .position(|entry| entry.txid.eq_ignore_ascii_case(anchor))
        });
        let offset = if query.anchor_txid.is_some() {
            anchor_offset.unwrap_or(0) / query.limit * query.limit
        } else {
            query.offset
        };
        let mut count = 0;
        let mut transfers = Vec::with_capacity(query.limit);
        // Only the returned page is cloned/serialized; never a full response hidden behind a UI slice.
        for entry in self
            .ordered
            .iter()
            .map(|id| &self.ledger[id])
            .filter(matches)
        {
            if count >= offset && transfers.len() < query.limit {
                transfers.push(api::Transfer {
                    txid: entry.txid.clone(),
                    direction: entry.direction.clone(),
                    amount: entry.amount,
                    fee: entry.fee,
                    height: entry.height,
                    timestamp: entry.timestamp,
                    confirmations: if entry.is_pending {
                        0
                    } else {
                        confirmations_for_height(height, entry.height.unwrap_or(0))
                    },
                    is_pending: entry.is_pending,
                    subaddress_major: None,
                    subaddress_minor: None,
                });
            }
            count += 1;
        }
        let end = offset.saturating_add(transfers.len());
        Ok(HistoryPage {
            schema_version: 1,
            wallet_id: wallet_id.into(),
            revision: self.revision.clone(),
            total_count: self.ledger.len(),
            matching_count: count,
            pending_count: self.pending,
            offset,
            anchor_offset,
            next_offset: (end < count).then_some(end),
            last_scanned_height: scanned,
            chain_height: height,
            chain_time: time,
            transfers,
        })
    }
}

pub(crate) fn query_history(wallet_id: &str, query: &HistoryQuery) -> Result<HistoryPage, String> {
    let (index, scanned, height, time) = {
        let mut wallets = WALLET_STORE
            .lock()
            .map_err(|_| "wallet store unavailable")?;
        let state = wallets.get_mut(wallet_id).ok_or("wallet not opened")?;
        let identity = wallet_cache_binding(state);
        if state
            .history_index
            .as_ref()
            .is_none_or(|i| i.identity != identity || i.ledger != state.tx_ledger)
        {
            state.history_index = Some(Arc::new(HistoryIndex::new(identity, &state.tx_ledger)));
        }
        (
            state.history_index.as_ref().unwrap().clone(),
            state.last_scanned,
            state.chain_height,
            state.chain_time,
        )
    };
    // Filter and serialize outside the wallet lock, so scrolling doesn't block scanning.
    index.query(wallet_id, query, scanned, height, time)
}

#[no_mangle]
pub extern "C" fn wallet_query_transfers_json(
    wallet_id: *const c_char,
    query_json: *const c_char,
) -> *mut c_char {
    clear_last_error();
    let result = (|| -> Result<String, String> {
        if wallet_id.is_null() || query_json.is_null() {
            return Err("invalid_history_query: null argument".into());
        }
        let id = unsafe { CStr::from_ptr(wallet_id) }
            .to_str()
            .map_err(|_| "invalid wallet id")?;
        let query = unsafe { CStr::from_ptr(query_json) }
            .to_str()
            .map_err(|_| "invalid query utf8")?;
        if query.len() > 4096 {
            return Err("invalid_history_query: request too large".into());
        }
        let query: HistoryQuery =
            serde_json::from_str(query).map_err(|e| format!("invalid_history_query: {e}"))?;
        serde_json::to_string(&query_history(id, &query)?).map_err(|e| e.to_string())
    })();
    match result {
        Ok(json) => CString::new(json).unwrap().into_raw(),
        Err(error) => {
            record_error(-16, error);
            ptr::null_mut()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ledger(count: usize) -> HashMap<String, LedgerEntry> {
        (0..count)
            .map(|i| {
                let txid = format!("{i:064x}");
                (
                    txid.clone(),
                    LedgerEntry {
                        txid,
                        direction: if i % 2 == 0 { "in" } else { "out" }.into(),
                        amount: i as u64,
                        fee: Some(7),
                        height: Some(100 + (i / 3) as u64),
                        timestamp: Some(1000 + i as u64),
                        is_pending: i == 0,
                        is_coinbase: false,
                    },
                )
            })
            .collect()
    }
    #[test]
    fn every_row_once_at_all_scales_and_bounded_pages() {
        for count in [0, 1, 10, 50, 100, 1000, 10000] {
            let index = HistoryIndex::new("test".into(), &ledger(count));
            let mut query = HistoryQuery::default();
            let mut ids = HashSet::new();
            loop {
                let page = index.query("test", &query, 20000, 20000, 0).unwrap();
                assert_eq!(page.total_count, count);
                assert!(page.transfers.len() <= 50);
                for row in page.transfers {
                    assert!(ids.insert(row.txid));
                }
                match page.next_offset {
                    Some(offset) => {
                        query.offset = offset;
                        query.revision = Some(page.revision);
                    }
                    None => break,
                }
            }
            assert_eq!(ids.len(), count);
        }
    }
    #[test]
    fn whole_ledger_search_filter_dates_detail_and_empty_are_distinct() {
        let index = HistoryIndex::new("test".into(), &ledger(10000));
        let mut q = HistoryQuery {
            search: format!("{:064x}", 4321),
            ..Default::default()
        };
        let page = index.query("test", &q, 20000, 20000, 0).unwrap();
        assert_eq!(
            (page.total_count, page.matching_count, page.transfers.len()),
            (10000, 1, 1)
        );
        q.filter = HistoryFilter::Received;
        assert_eq!(index.query("test", &q, 0, 0, 0).unwrap().matching_count, 0);
        q = HistoryQuery {
            from_timestamp: Some(1000),
            to_timestamp: Some(1009),
            ..Default::default()
        };
        assert_eq!(index.query("test", &q, 0, 0, 0).unwrap().matching_count, 10);
        q = HistoryQuery {
            txid: Some(format!("{:064x}", 2)),
            ..Default::default()
        };
        assert_eq!(
            index.query("test", &q, 0, 0, 0).unwrap().transfers[0].amount,
            2
        );
        q = HistoryQuery {
            filter: HistoryFilter::Pending,
            ..Default::default()
        };
        assert_eq!(index.query("test", &q, 0, 0, 0).unwrap().matching_count, 1);
    }
    #[test]
    fn stale_cursor_rejects_append_confirmation_reorg_and_wallet_replacement() {
        let original = ledger(100);
        let index = HistoryIndex::new("a".into(), &original);
        let query = HistoryQuery {
            offset: 50,
            revision: Some(index.revision.clone()),
            ..Default::default()
        };
        for variant in 0..4 {
            let mut changed = original.clone();
            match variant {
                0 => changed.extend(ledger(101)),
                1 => changed.get_mut(&format!("{:064x}", 0)).unwrap().is_pending = false,
                2 => {
                    changed.remove(&format!("{:064x}", 30));
                }
                _ => {}
            }
            let new_index = HistoryIndex::new("b".into(), &changed);
            assert!(new_index
                .query("test", &query, 0, 0, 0)
                .unwrap_err()
                .contains("stale_history_cursor"));
        }
        // Merely moving the tip preserves page identity; confirmations are calculated at read time.
        assert!(index.query("test", &query, 20000, 20001, 0).is_ok());
    }

    #[test]
    fn reload_anchor_survives_new_transactions_and_disappearing_rows() {
        let mut entries = ledger(10000);
        let before = HistoryIndex::new("test".into(), &entries);
        let anchor = before.ordered[4351].clone();
        entries.extend(ledger(10003));
        let after = HistoryIndex::new("test".into(), &entries);
        let q = HistoryQuery {
            anchor_txid: Some(anchor.clone()),
            ..Default::default()
        };
        let page = after.query("test", &q, 0, 0, 0).unwrap();
        let position = page.anchor_offset.unwrap();
        assert_eq!(position, 4354);
        assert_eq!(page.offset, position / 50 * 50);
        assert_eq!(page.transfers[position - page.offset].txid, anchor);
        entries.remove(&anchor);
        let after = HistoryIndex::new("test".into(), &entries);
        let missing = after.query("test", &q, 0, 0, 0).unwrap();
        assert_eq!((missing.offset, missing.anchor_offset), (0, None));
    }

    #[test]
    fn native_page_ffi_cache_import_and_full_export_share_one_ledger() {
        // Public test-only seed, never a real wallet.
        let id = CString::new("history-ffi-fixture").unwrap();
        let seed = CString::new("ability pockets lordship tomorrow gypsy match neutral uncle avatar betting bicycle junk unzip pyramid lynx mammal edgy empty uneven knowledge juvenile wiring paradise psychic betting").unwrap();
        assert_eq!(
            crate::wallet_open_from_mnemonic(id.as_ptr(), seed.as_ptr(), 100, 1),
            0
        );
        {
            let mut store = WALLET_STORE.lock().unwrap();
            let wallet = store.get_mut("history-ffi-fixture").unwrap();
            wallet.tx_ledger = ledger(123);
            wallet.tracked_outputs = (0u64..123)
                .map(|i| {
                    let mut tx_hash = [0; 32];
                    tx_hash[24..].copy_from_slice(&i.to_be_bytes());
                    TrackedOutput {
                        tx_hash,
                        index_in_tx: 0,
                        key_image: [0; 32],
                        amount: i + 1,
                        block_height: 100 + i / 3,
                        additional_timelock: Timelock::None,
                        is_coinbase: false,
                        subaddress_major: 0,
                        subaddress_minor: 0,
                        spent: false,
                        spending_txid: None,
                        spending_height: None,
                    }
                })
                .collect();
            wallet.last_scanned = 500;
            wallet.chain_height = 600;
        }
        let request = CString::new(r#"{"limit":10}"#).unwrap();
        let raw = wallet_query_transfers_json(id.as_ptr(), request.as_ptr());
        assert!(!raw.is_null());
        let page: HistoryPage =
            serde_json::from_str(unsafe { CStr::from_ptr(raw) }.to_str().unwrap()).unwrap();
        unsafe {
            drop(CString::from_raw(raw));
        }
        assert_eq!((page.total_count, page.transfers.len()), (123, 10));
        assert_eq!(
            api::list_transfers("history-ffi-fixture").unwrap().len(),
            123
        );
        let mut written = 0;
        assert_eq!(
            crate::wallet_export_cache(id.as_ptr(), ptr::null_mut(), 0, &mut written),
            -12
        );
        let mut cache = vec![0; written];
        assert_eq!(
            crate::wallet_export_cache(id.as_ptr(), cache.as_mut_ptr(), cache.len(), &mut written),
            0
        );
        WALLET_STORE
            .lock()
            .unwrap()
            .get_mut("history-ffi-fixture")
            .unwrap()
            .tx_ledger
            .clear();
        let empty = query_history("history-ffi-fixture", &HistoryQuery::default()).unwrap();
        assert_eq!(empty.total_count, 0);
        assert_ne!(empty.revision, page.revision);
        assert_eq!(
            crate::wallet_import_cache(id.as_ptr(), cache.as_ptr(), written),
            0
        );
        let restored = query_history("history-ffi-fixture", &HistoryQuery::default()).unwrap();
        assert_eq!(restored.total_count, 123);
        assert_ne!(restored.revision, empty.revision);
        let old = HistoryQuery {
            offset: 50,
            revision: Some(empty.revision),
            ..Default::default()
        };
        assert!(query_history("history-ffi-fixture", &old)
            .unwrap_err()
            .contains("stale_history_cursor"));
        WALLET_STORE.lock().unwrap().remove("history-ffi-fixture");
        assert!(query_history("history-ffi-fixture", &HistoryQuery::default()).is_err());
    }

    #[test]
    fn invalid_queries_and_same_height_order() {
        let index = HistoryIndex::new("test".into(), &ledger(100));
        for limit in [0, 201, usize::MAX] {
            assert!(index
                .query(
                    "test",
                    &HistoryQuery {
                        limit,
                        ..Default::default()
                    },
                    0,
                    0,
                    0
                )
                .is_err());
        }
        assert!(index
            .query(
                "test",
                &HistoryQuery {
                    offset: 50,
                    ..Default::default()
                },
                0,
                0,
                0
            )
            .is_err());
        let mut entries = ledger(3);
        for entry in entries.values_mut() {
            entry.is_pending = false;
            entry.height = Some(100);
            entry.timestamp = Some(1000);
        }
        let index = HistoryIndex::new("test".into(), &entries);
        assert_eq!(
            index.ordered,
            (0..3).map(|i| format!("{i:064x}")).collect::<Vec<_>>()
        );
    }
}
