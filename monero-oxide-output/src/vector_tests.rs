//! Tiny restore / address / checksum vectors.
//! Cake mnemonic→address is the only Cake fixture we keep.

use super::*;

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
