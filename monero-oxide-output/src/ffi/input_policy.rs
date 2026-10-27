//! Shared eligibility for exact sends, fee previews and sweeps. Quarantine affects
//! availability, not historical ownership/amounts; only a caught-up unspent response
//! releases an entry. The existing cache format remains unchanged.
use crate::support::RpcClient;
use crate::{wallet_cache_binding, StoredWallet, TrackedOutput, WALLET_STORE};

pub(crate) fn unlocked_balance(wallet: &StoredWallet, minor: Option<u32>) -> u64 {
    wallet
        .tracked_outputs
        .iter()
        .filter(|o| {
            !o.spent
                && o.is_unlocked(wallet.chain_height, wallet.chain_time)
                && !wallet
                    .invalid_input_quarantine
                    .contains(&(o.tx_hash, o.index_in_tx))
                && minor.is_none_or(|m| o.subaddress_major == 0 && o.subaddress_minor == m)
        })
        .fold(0u64, |sum, o| sum.saturating_add(o.amount))
}

fn apply_statuses(
    wallet: &mut StoredWallet,
    outputs: &[TrackedOutput],
    statuses: &[u8],
    daemon_height: u64,
    allow_release: bool,
) -> Result<(), String> {
    if statuses.len() != outputs.len() || statuses.iter().any(|status| *status > 2) {
        return Err("invalid key-image spent-status response".into());
    }
    let caught_up = allow_release && daemon_height > 0 && wallet.last_scanned >= daemon_height;
    let current: std::collections::HashSet<_> = wallet
        .tracked_outputs
        .iter()
        .filter(|o| !o.spent)
        .map(|o| (o.tx_hash, o.index_in_tx, o.key_image))
        .collect();
    for (output, status) in outputs.iter().zip(statuses) {
        // Ignore an output that has changed since the request; never alter ledger/spent flags
        // based on an untrusted daemon's report alone.
        if !current.contains(&(output.tx_hash, output.index_in_tx, output.key_image)) {
            continue;
        }
        let key = (output.tx_hash, output.index_in_tx);
        match status {
            0 if caught_up => {
                wallet.invalid_input_quarantine.remove(&key);
            }
            1 | 2 => {
                wallet.invalid_input_quarantine.insert(key);
            }
            _ => {}
        }
    }
    wallet.unlocked = unlocked_balance(wallet, None);
    Ok(())
}

/// Query all eligible tracked outputs, including previously quarantined ones. Unsupported
/// endpoints retain existing quarantine; they do not silently make blocked inputs spendable.
pub(crate) fn spendable(
    id: &str,
    snapshot: &mut StoredWallet,
    rpc: &RpcClient,
    height: u64,
    time: u64,
    allow_release: bool,
) -> Result<Vec<TrackedOutput>, String> {
    let outputs: Vec<_> = snapshot
        .tracked_outputs
        .iter()
        .filter(|o| !o.spent && o.is_unlocked(height, time))
        .cloned()
        .collect();
    reconcile_with(id, snapshot, &outputs, height, allow_release, |images| {
        super::send::rpc_is_key_image_spent(rpc, images)
    })?;
    Ok(outputs
        .into_iter()
        .filter(|o| {
            !snapshot
                .invalid_input_quarantine
                .contains(&(o.tx_hash, o.index_in_tx))
        })
        .collect())
}

pub(crate) fn revalidate_after_refresh(
    id: &str,
    rpc: &RpcClient,
    height: u64,
) -> Result<(), String> {
    let mut snapshot = {
        let store = WALLET_STORE
            .lock()
            .map_err(|_| "wallet store unavailable")?;
        let state = store.get(id).ok_or("wallet not open")?;
        if state.invalid_input_quarantine.is_empty() {
            return Ok(());
        }
        state.clone()
    };
    let outputs: Vec<_> = snapshot
        .tracked_outputs
        .iter()
        .filter(|o| {
            !o.spent
                && o.key_image != [0; 32]
                && snapshot
                    .invalid_input_quarantine
                    .contains(&(o.tx_hash, o.index_in_tx))
        })
        .cloned()
        .collect();
    reconcile_with(id, &mut snapshot, &outputs, height, true, |images| {
        super::send::rpc_is_key_image_spent(rpc, images)
    })
}

fn reconcile_with(
    id: &str,
    snapshot: &mut StoredWallet,
    outputs: &[TrackedOutput],
    height: u64,
    allow_release: bool,
    query: impl FnOnce(&[[u8; 32]]) -> Result<Vec<u8>, String>,
) -> Result<(), String> {
    if outputs.is_empty() {
        return Ok(());
    }
    let images: Vec<_> = outputs.iter().map(|o| o.key_image).collect();
    if let Ok(statuses) = query(&images) {
        let mut store = WALLET_STORE
            .lock()
            .map_err(|_| "wallet store unavailable")?;
        let state = store.get_mut(id).ok_or("wallet not open")?;
        if wallet_cache_binding(state) != wallet_cache_binding(snapshot) {
            return Err("wallet changed during spent-status request".into());
        }
        apply_statuses(state, outputs, &statuses, height, allow_release)?;
        snapshot.invalid_input_quarantine = state.invalid_input_quarantine.clone();
        snapshot.unlocked = state.unlocked;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, ffi::CString};

    #[test]
    fn real_spent_status_transport_uses_the_shared_quarantine_policy() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            thread,
            time::{Duration, Instant},
        };
        let id = "quarantine-local-rpc-fixture";
        let mut snapshot = fixture(id);
        WALLET_STORE
            .lock()
            .unwrap()
            .insert(id.into(), snapshot.clone());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let mut connection = None;
            // MoneroDaemon first probes optional batch JSON-RPC support.
            for status in [None, Some(2), Some(2), Some(0)] {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut stream = if let Some(stream) = connection.take() {
                    stream
                } else {
                    loop {
                        match listener.accept() {
                            Ok((stream, _)) => break stream,
                            Err(error)
                                if error.kind() == std::io::ErrorKind::WouldBlock
                                    && Instant::now() < deadline =>
                            {
                                thread::sleep(Duration::from_millis(2))
                            }
                            Err(error) => panic!("fixture HTTP accept failed: {error}"),
                        }
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 1024];
                    let count = stream.read(&mut bytes).unwrap();
                    assert!(count > 0 && request.len() + count <= 8192);
                    request.extend_from_slice(&bytes[..count]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let header = String::from_utf8_lossy(&request[..end]);
                        let length = header
                            .lines()
                            .find_map(|line| {
                                let (key, value) = line.split_once(':')?;
                                key.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().unwrap())
                            })
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let body = match status {
                    Some(status) => {
                        assert!(String::from_utf8_lossy(&request).contains("/is_key_image_spent"));
                        format!(r#"{{"spent_status":[{status}],"status":"OK"}}"#)
                    }
                    None => {
                        assert!(String::from_utf8_lossy(&request).contains("/json_rpc"));
                        r#"{"error":{"code":-32600,"message":"fixture batch unsupported"}}"#.into()
                    }
                };
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n{body}", body.len()).unwrap();
                connection = Some(stream);
            }
        });
        let rpc = crate::TOKIO_RUNTIME
            .block_on(crate::support::rpc::RpcTransport::new(
                endpoint,
            ))
            .unwrap();
        assert_eq!(
            super::super::send::rpc_is_key_image_spent(&rpc, &[[2; 32]]).unwrap(),
            vec![2]
        );
        assert!(spendable(id, &mut snapshot, &rpc, 200, 1_700_000_000, true)
            .unwrap()
            .is_empty());
        assert_eq!(snapshot.total, 100);
        assert_eq!(snapshot.unlocked, 0);
        assert_eq!(
            spendable(id, &mut snapshot, &rpc, 200, 1_700_000_000, true)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(snapshot.unlocked, 100);
        WALLET_STORE.lock().unwrap().remove(id);
        server.join().unwrap();
    }
    fn fixture(id: &str) -> StoredWallet {
        let id_c = CString::new(id).unwrap();
        let seed = CString::new("ability pockets lordship tomorrow gypsy match neutral uncle avatar betting bicycle junk unzip pyramid lynx mammal edgy empty uneven knowledge juvenile wiring paradise psychic betting").unwrap();
        assert_eq!(
            crate::wallet_open_from_mnemonic(id_c.as_ptr(), seed.as_ptr(), 0, 1),
            0
        );
        let mut wallet = WALLET_STORE.lock().unwrap().remove(id).unwrap();
        wallet.chain_height = 200;
        wallet.last_scanned = 200;
        wallet.chain_time = 1_700_000_000;
        wallet.tracked_outputs = vec![TrackedOutput {
            tx_hash: [1; 32],
            index_in_tx: 0,
            public_key: [1; 32],
            key_image: [2; 32],
            amount: 100,
            block_height: 100,
            additional_timelock: crate::Timelock::None,
            is_coinbase: false,
            subaddress_major: 0,
            subaddress_minor: 1,
            spent: false,
            spending_txid: None,
            spending_height: None,
        }];
        wallet.total = 100;
        wallet.unlocked = 100;
        wallet.block_timestamps = HashMap::from([(100, 1_699_999_900)]);
        wallet.tx_ledger = crate::rebuild_transfer_ledger(
            &wallet.tracked_outputs,
            &[],
            &HashMap::from([(crate::hex_lowercase(&[1; 32]), 9)]),
            wallet.chain_time,
            &wallet.block_timestamps,
        );
        wallet
    }
    #[test]
    fn pool_spend_and_drop_preserve_history_but_change_available_balance() {
        let mut wallet = fixture("pool-drop-fixture");
        let outputs = wallet.tracked_outputs.clone();
        let history = serde_json::to_string(&wallet.tx_ledger).unwrap();
        apply_statuses(&mut wallet, &outputs, &[2], 200, true).unwrap();
        assert_eq!(unlocked_balance(&wallet, None), 0);
        assert_eq!(unlocked_balance(&wallet, Some(1)), 0);
        assert_eq!(wallet.total, 100);
        assert!(!wallet.tracked_outputs[0].spent);
        // A stale scan cannot release the entry just because a node reports unspent.
        apply_statuses(&mut wallet, &outputs, &[0], 201, true).unwrap();
        assert_eq!(wallet.unlocked, 0);
        wallet.last_scanned = 201;
        wallet.chain_height = 201;
        apply_statuses(&mut wallet, &outputs, &[0], 201, true).unwrap();
        assert_eq!(wallet.unlocked, 100);
        assert!(wallet.invalid_input_quarantine.is_empty());
        assert_eq!(serde_json::to_string(&wallet.tx_ledger).unwrap(), history);
    }
    #[test]
    fn malformed_statuses_and_stale_outputs_do_not_change_quarantine() {
        let mut wallet = fixture("bad-status-fixture");
        let outputs = wallet.tracked_outputs.clone();
        let key = (outputs[0].tx_hash, outputs[0].index_in_tx);
        wallet.invalid_input_quarantine.insert(key);
        assert!(apply_statuses(&mut wallet, &outputs, &[], 200, true).is_err());
        assert!(apply_statuses(&mut wallet, &outputs, &[3], 200, true).is_err());
        let mut stale = outputs;
        stale[0].key_image = [9; 32];
        apply_statuses(&mut wallet, &stale, &[0], 200, true).unwrap();
        assert!(wallet.invalid_input_quarantine.contains(&key));
    }
    #[test]
    fn newly_rejected_inputs_are_not_reenabled_by_a_same_call_retry() {
        let mut wallet = fixture("retry-quarantine-fixture");
        let outputs = wallet.tracked_outputs.clone();
        wallet
            .invalid_input_quarantine
            .insert((outputs[0].tx_hash, outputs[0].index_in_tx));
        apply_statuses(&mut wallet, &outputs, &[0], 200, false).unwrap();
        assert_eq!(wallet.unlocked, 0);
        apply_statuses(&mut wallet, &outputs, &[0], 200, true).unwrap();
        assert_eq!(wallet.unlocked, 100);
    }
    #[test]
    fn unsupported_preflight_keeps_blocked_inputs_and_cache_round_trip_keeps_history() {
        let id = "quarantine-cache-fixture";
        let mut wallet = fixture(id);
        let outputs = wallet.tracked_outputs.clone();
        apply_statuses(&mut wallet, &outputs, &[2], 200, true).unwrap();
        let bytes = bincode::serialize(&crate::PersistedWallet::from(&wallet)).unwrap();
        let history = serde_json::to_string(&wallet.tx_ledger).unwrap();
        WALLET_STORE
            .lock()
            .unwrap()
            .insert(id.into(), wallet.clone());
        let id_c = CString::new(id).unwrap();
        assert_eq!(
            crate::ffi::cache::wallet_import_cache(id_c.as_ptr(), bytes.as_ptr(), bytes.len()),
            0
        );
        let mut snapshot = WALLET_STORE.lock().unwrap().get(id).unwrap().clone();
        reconcile_with(id, &mut snapshot, &outputs, 200, true, |_| {
            Err("unsupported".into())
        })
        .unwrap();
        assert_eq!(unlocked_balance(&snapshot, None), 0);
        let mut total = 0;
        let mut unlocked = 0;
        assert_eq!(
            crate::wallet_get_balance(id_c.as_ptr(), &mut total, &mut unlocked),
            0
        );
        assert_eq!((total, unlocked), (100, 0));
        let filter = CString::new(r#"{"subaddress_minor":1}"#).unwrap();
        assert_eq!(
            crate::wallet_get_balance_with_filter(
                id_c.as_ptr(),
                filter.as_ptr(),
                &mut total,
                &mut unlocked
            ),
            0
        );
        assert_eq!((total, unlocked), (100, 0));
        // A subsequent supported response releases a dropped pool transaction without a rescan.
        reconcile_with(id, &mut snapshot, &outputs, 200, true, |_| Ok(vec![0])).unwrap();
        assert_eq!(unlocked_balance(&snapshot, None), 100);
        let restored = WALLET_STORE.lock().unwrap().remove(id).unwrap();
        assert_eq!(serde_json::to_string(&restored.tx_ledger).unwrap(), history);
    }
    #[test]
    fn no_candidates_means_no_extra_rpc_and_foreign_wallet_results_are_rejected() {
        let id = "quarantine-identity-fixture";
        let mut wallet = fixture(id);
        reconcile_with(id, &mut wallet, &[], 200, true, |_| {
            panic!("unexpected RPC")
        })
        .unwrap();
        let outputs = wallet.tracked_outputs.clone();
        let mut replacement = wallet.clone();
        replacement.network = crate::MoneroNetwork::Stagenet;
        WALLET_STORE.lock().unwrap().insert(id.into(), replacement);
        assert!(reconcile_with(id, &mut wallet, &outputs, 200, true, |_| Ok(vec![2])).is_err());
        WALLET_STORE.lock().unwrap().remove(id);
    }
}
