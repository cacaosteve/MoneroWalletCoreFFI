import XCTest
@testable import MoneroWalletCoreFFI

final class LifecycleErrorTests: XCTestCase {
    func testTimeoutIsTypedAndNotInferredFromDiagnosticText() {
        XCTAssertTrue(WalletCoreLifecycleError(code: -31, message: "busy").isTimeout)
        XCTAssertFalse(WalletCoreLifecycleError(code: -1, message: "rc=-31").isTimeout)
        XCTAssertEqual(WalletCoreLifecycleError(code: -31, message: "busy").localizedDescription, "busy")
    }
}
