import XCTest
@testable import MoneroWalletCoreFFI

final class TransferHistoryDecodingTests: XCTestCase {
    private let row = #"{"txid":"abababababababababababababababababababababababababababababababab","direction":"in","amount":42,"fee":7,"height":3600000,"timestamp":1786000000,"confirmations":10,"is_pending":false,"subaddress_major":0,"subaddress_minor":1}"#

    func testLegacyArrayRemainsSupported() throws {
        let history = try WalletCoreFFIClient.decodeTransferHistoryJSON(
            "[\(row)]",
            expectedWalletId: "main_wallet"
        )

        XCTAssertEqual(history.schemaVersion, 0)
        XCTAssertNil(history.walletId)
        XCTAssertEqual(history.transfers.first?.fee, 7)
        XCTAssertEqual(history.transfers.first?.subaddressMinor, 1)
    }

    func testVersionOneAcceptsAdditiveFields() throws {
        let json = #"{"schema_version":1,"wallet_id":"main_wallet","last_scanned_height":3600010,"chain_height":3600020,"chain_time":1787000000,"future_metadata":true,"transfers":["# + row + "]}"
        let history = try WalletCoreFFIClient.decodeTransferHistoryJSON(
            json,
            expectedWalletId: "main_wallet"
        )

        XCTAssertEqual(history.schemaVersion, 1)
        XCTAssertEqual(history.lastScannedHeight, 3_600_010)
        XCTAssertEqual(history.transfers.count, 1)
    }

    func testFutureVersionIsRejected() {
        let json = #"{"schema_version":2,"wallet_id":"main_wallet","last_scanned_height":0,"chain_height":0,"chain_time":0,"transfers":[]}"#
        XCTAssertThrowsError(
            try WalletCoreFFIClient.decodeTransferHistoryJSON(json, expectedWalletId: "main_wallet")
        ) { error in
            XCTAssertTrue(error.localizedDescription.contains("schema version 2"))
        }
    }

    func testMismatchedWalletAndUnknownDirectionAreRejected() {
        let wrongWallet = #"{"schema_version":1,"wallet_id":"other","last_scanned_height":0,"chain_height":0,"chain_time":0,"transfers":[]}"#
        XCTAssertThrowsError(
            try WalletCoreFFIClient.decodeTransferHistoryJSON(wrongWallet, expectedWalletId: "main_wallet")
        )

        let unknownDirection = "[\(row.replacingOccurrences(of: "\"in\"", with: "\"sideways\""))]"
        XCTAssertThrowsError(
            try WalletCoreFFIClient.decodeTransferHistoryJSON(
                unknownDirection,
                expectedWalletId: "main_wallet"
            )
        )
    }
}
