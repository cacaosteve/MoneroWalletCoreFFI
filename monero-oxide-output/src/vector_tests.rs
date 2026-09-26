//! Tiny restore / address / checksum vectors.
//! Cake mnemonic→address is the only Cake fixture we keep.

use super::*;
use std::ffi::CString;

const CAKE_MNEMONIC: &str = "ability pockets lordship tomorrow gypsy match neutral uncle avatar \
    betting bicycle junk unzip pyramid lynx mammal edgy empty uneven knowledge juvenile wiring \
    paradise psychic betting";

const CAKE_PRIMARY: &str = "48tLyQXpcwt8w6uKHyb5Zs3vdnoDWAEKFQr1c198o7aX9dBzXP3BTSMVsDiuH3ozDCNqwojb4vNeQZf7xg6URimDLaNtGSN";

const STANDARD: &str =
    "4B33mFPMq6mKi7Eiyd5XuyKRVMGVZz1Rqb9ZTyGApXW5d1aT7UBDZ89ewmnWFkzJ5wPd2SFbn313vCT8a4E2Qf4KQH4pNey";

const INTEGRATED: &str =
    "4Ljin4CrSNHKi7Eiyd5XuyKRVMGVZz1Rqb9ZTyGApXW5d1aT7UBDZ89ewmnWFkzJ5wPd2SFbn313vCT8a4E2Qf4KbaTH6MnpXSn88oBX35";

const SUBADDRESS: &str =
    "8C5zHM5ud8nGC4hC2ULiBLSWx9infi8JUUmWEat4fcTf8J4H38iWYVdFmPCA9UmfLTZxD43RsyKnGEdZkoGij6csDeUnbEB";

#[test]
fn cake_english_25_word_mnemonic_to_primary_address() {
    let keys = master_keys_from_mnemonic_str(CAKE_MNEMONIC).expect("valid mnemonic");
    let addr = derive_address_string(&keys, 0, 0, MoneroNetwork::Mainnet);
    assert_eq!(addr, CAKE_PRIMARY);
}

#[test]
fn bad_mnemonic_wrong_checksum_rejected() {
    let without_last = CAKE_MNEMONIC.rsplit_once(' ').expect("words").0;
    let bad = format!("{without_last} ability");
    assert!(master_keys_from_mnemonic_str(&bad).is_err());
}

#[test]
fn bad_mnemonic_24_words_rejected() {
    let words: Vec<&str> = CAKE_MNEMONIC.split_whitespace().collect();
    assert_eq!(words.len(), 25);
    let twenty_four = words[..24].join(" ");
    assert!(
        master_keys_from_mnemonic_str(&twenty_four).is_err(),
        "truncated 24-word phrase must be rejected"
    );
}

#[test]
fn bad_mnemonic_unknown_word_rejected() {
    let bad = CAKE_MNEMONIC.replace("pockets", "notaword");
    assert!(master_keys_from_mnemonic_str(&bad).is_err());
}

#[test]
fn parse_primary_subaddress_and_integrated() {
    assert!(MoneroAddress::from_str(MoneroNetwork::Mainnet, STANDARD).is_ok());
    assert!(MoneroAddress::from_str(MoneroNetwork::Mainnet, SUBADDRESS).is_ok());
    assert!(MoneroAddress::from_str(MoneroNetwork::Mainnet, INTEGRATED).is_ok());
}

#[test]
fn parse_flipped_character_fails_oxide_checksum() {
    let mut chars: Vec<char> = STANDARD.chars().collect();
    let i = chars.len() / 2;
    chars[i] = if chars[i] == 'A' { 'B' } else { 'A' };
    let flipped: String = chars.into_iter().collect();
    assert_ne!(flipped, STANDARD);
    assert!(MoneroAddress::from_str(MoneroNetwork::Mainnet, &flipped).is_err());
}

#[test]
fn sealed_lifecycle_keeps_view_state_rewinds_new_outputs_and_closes() {
    let wallet_id = CString::new("sealed-lifecycle-test").unwrap();
    let mnemonic = CString::new(CAKE_MNEMONIC).unwrap();
    assert_eq!(
        wallet_open_from_mnemonic(wallet_id.as_ptr(), mnemonic.as_ptr(), 100, 1),
        0
    );

    {
        let mut wallets = WALLET_STORE.lock().unwrap();
        let wallet = wallets.get_mut("sealed-lifecycle-test").unwrap();
        wallet.last_scanned = 140;
        wallet.chain_height = 150;
        wallet.chain_time = 1_700_000_000;
        wallet.tracked_outputs = vec![
            TrackedOutput {
                tx_hash: [1; 32],
                index_in_tx: 0,
                key_image: [2; 32],
                amount: 7,
                block_height: 110,
                additional_timelock: Timelock::None,
                is_coinbase: false,
                subaddress_major: 0,
                subaddress_minor: 0,
                spent: true,
                spending_txid: Some([3; 32]),
                spending_height: Some(125),
            },
            TrackedOutput {
                tx_hash: [4; 32],
                index_in_tx: 0,
                key_image: [0; 32],
                amount: 11,
                block_height: 120,
                additional_timelock: Timelock::None,
                is_coinbase: false,
                subaddress_major: 0,
                subaddress_minor: 1,
                spent: false,
                spending_txid: None,
                spending_height: None,
            },
        ];
        wallet.seen_outpoints = HashSet::from([([1; 32], 0), ([4; 32], 0)]);
        wallet.total = 11;
        wallet.unlocked = 11;
        wallet.spend_rescan_from = Some(120);

        let persisted = PersistedWallet::from(&*wallet);
        let encoded = bincode::serialize(&persisted).unwrap();
        let decoded: PersistedWallet = bincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.spend_rescan_from, Some(120));
    }

    assert_eq!(wallet_seal(wallet_id.as_ptr(), 1_000), 0);
    let mut sealed = 0;
    assert_eq!(wallet_is_sealed(wallet_id.as_ptr(), &mut sealed), 0);
    assert_eq!(sealed, 1);

    // Read-only wallet state remains available while private spend authority is absent.
    let (mut total, mut unlocked) = (0, 0);
    assert_eq!(
        wallet_get_balance(wallet_id.as_ptr(), &mut total, &mut unlocked),
        0
    );
    assert_eq!((total, unlocked), (11, 11));

    // A different valid seed must not be allowed to rebind an already-open wallet.
    let mut generated = [0_i8; 512];
    let mut generated_len = 0;
    assert_eq!(
        wallet_generate_mnemonic_english(
            generated.as_mut_ptr(),
            generated.len(),
            &mut generated_len,
        ),
        0
    );
    assert_eq!(
        wallet_unseal_from_mnemonic(wallet_id.as_ptr(), generated.as_ptr(), 1_000),
        -16
    );
    assert_eq!(wallet_is_sealed(wallet_id.as_ptr(), &mut sealed), 0);
    assert_eq!(sealed, 1);

    assert_eq!(
        wallet_unseal_from_mnemonic(wallet_id.as_ptr(), mnemonic.as_ptr(), 1_000),
        0
    );
    assert_eq!(wallet_is_sealed(wallet_id.as_ptr(), &mut sealed), 0);
    assert_eq!(sealed, 0);

    {
        let wallets = WALLET_STORE.lock().unwrap();
        let wallet = &wallets["sealed-lifecycle-test"];
        assert_eq!(wallet.last_scanned, 120);
        assert_eq!(wallet.spend_rescan_from, None);
        assert_eq!(wallet.tracked_outputs.len(), 1);
        assert_eq!(wallet.tracked_outputs[0].block_height, 110);
        assert!(!wallet.tracked_outputs[0].spent);
        assert_eq!((wallet.total, wallet.unlocked), (7, 7));
    }

    assert_eq!(wallet_close(wallet_id.as_ptr(), 1_000), 0);
    assert_eq!(wallet_is_sealed(wallet_id.as_ptr(), &mut sealed), -13);
}

#[test]
fn sealed_spend_rescan_marker_never_advances_a_reorged_cursor() {
    let wallet_id = CString::new("sealed-reorg-marker-test").unwrap();
    let mnemonic = CString::new(CAKE_MNEMONIC).unwrap();
    assert_eq!(
        wallet_open_from_mnemonic(wallet_id.as_ptr(), mnemonic.as_ptr(), 100, 1),
        0
    );
    {
        let mut wallets = WALLET_STORE.lock().unwrap();
        let wallet = wallets.get_mut("sealed-reorg-marker-test").unwrap();
        wallet.last_scanned = 115;
        wallet.spend_rescan_from = Some(120);
    }

    assert_eq!(wallet_seal(wallet_id.as_ptr(), 1_000), 0);
    assert_eq!(
        wallet_unseal_from_mnemonic(wallet_id.as_ptr(), mnemonic.as_ptr(), 1_000),
        0
    );
    {
        let wallets = WALLET_STORE.lock().unwrap();
        let wallet = &wallets["sealed-reorg-marker-test"];
        assert_eq!(wallet.last_scanned, 115);
        assert_eq!(wallet.spend_rescan_from, None);
    }
    assert_eq!(wallet_close(wallet_id.as_ptr(), 1_000), 0);
}
