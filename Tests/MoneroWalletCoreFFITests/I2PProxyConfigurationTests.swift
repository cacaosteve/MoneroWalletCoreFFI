import XCTest
@testable import MoneroWalletCoreFFI

final class I2PProxyConfigurationTests: XCTestCase {
    func testNativeProxyConfigurationRejectsUnsupportedSchemes() throws {
        // No connection is made: this also verifies the shipped native symbol
        // and Swift wrapper agree, rather than only typechecking against a header.
        defer { try? WalletCoreFFIClient.setI2PHTTPProxy(nil) }
        try WalletCoreFFIClient.setI2PHTTPProxy("127.0.0.1:4444")
        XCTAssertThrowsError(try WalletCoreFFIClient.setI2PHTTPProxy("socks5://127.0.0.1:4447"))
        XCTAssertThrowsError(try WalletCoreFFIClient.setI2PHTTPProxy("https://127.0.0.1:4444"))
        try WalletCoreFFIClient.setI2PHTTPProxy(nil)
    }
}
