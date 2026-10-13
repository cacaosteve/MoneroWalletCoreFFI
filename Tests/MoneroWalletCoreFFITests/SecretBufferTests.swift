import XCTest
@testable import MoneroWalletCoreFFI

final class SecretBufferTests: XCTestCase {
    func testOwnedBuffersAreZeroedIncludingEmptyBuffers() {
        var bytes: [UInt8] = [1, 2, 3, 255]
        WalletCoreFFISupport.wipe(&bytes)
        XCTAssertEqual(bytes, [0, 0, 0, 0])
        var empty = [UInt8]()
        WalletCoreFFISupport.wipe(&empty)
        XCTAssertTrue(empty.isEmpty)
    }
    func testSecretCStringPassesTerminatedUTF8AndPropagatesErrors() throws {
        XCTAssertEqual(WalletCoreFFISupport.withSecretCString("synthetic phrase") { String(cString: $0) }, "synthetic phrase")
        enum Expected: Error { case failure }
        XCTAssertThrowsError(try WalletCoreFFISupport.withSecretCString("synthetic phrase") { _ in throw Expected.failure })
    }
}
