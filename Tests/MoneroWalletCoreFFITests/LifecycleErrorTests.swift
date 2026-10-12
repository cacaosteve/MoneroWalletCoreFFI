import XCTest
import Foundation
@testable import MoneroWalletCoreFFI

final class LifecycleErrorTests: XCTestCase {
    func testExpectedReleaseBinaryVersionAndCacheSchema() throws {
        guard let expected = ProcessInfo.processInfo.environment["NEXAWAL_EXPECTED_NATIVE_VERSION"] else {
            throw XCTSkip("Set NEXAWAL_EXPECTED_NATIVE_VERSION when verifying a release artifact")
        }
        XCTAssertEqual(WalletCoreFFIClient.version(), expected)
        let walletId = "release-artifact-probe-\(UUID().uuidString)"
        let mnemonic = "ability pockets lordship tomorrow gypsy match neutral uncle avatar betting bicycle junk unzip pyramid lynx mammal edgy empty uneven knowledge juvenile wiring paradise psychic betting"
        try WalletCoreFFIClient.openWalletFromMnemonic(walletId: walletId, mnemonic: mnemonic,
            restoreHeight: 100, mainnet: true)
        defer { try? WalletCoreFFIClient.closeWallet(walletId: walletId) }
        let cache = try XCTUnwrap(WalletCoreFFIClient.exportCache(walletId: walletId))
        XCTAssertEqual(Array(cache.prefix(4)), [5, 0, 0, 0])
        try WalletCoreFFIClient.sealWallet(walletId: walletId)
        XCTAssertTrue(try WalletCoreFFIClient.isWalletSealed(walletId: walletId))
    }

    func testTimeoutIsTypedAndNotInferredFromDiagnosticText() {
        XCTAssertTrue(WalletCoreLifecycleError(code: -31, message: "busy").isTimeout)
        XCTAssertFalse(WalletCoreLifecycleError(code: -1, message: "rc=-31").isTimeout)
        XCTAssertEqual(WalletCoreLifecycleError(code: -31, message: "busy").localizedDescription, "busy")
    }
}
