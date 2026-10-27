import Foundation

#if canImport(MoneroWalletCore)
import MoneroWalletCore
#elseif canImport(CLibMoneroWalletCore)
import CLibMoneroWalletCore
#else
#error("MoneroWalletCoreFFI: Missing C module. Ensure the xcframework (Apple) or system library (Linux) is available.")
#endif

/// Errors thrown by the Swift wrapper when the underlying FFI returns an error code or invalid data.
public enum WalletCoreFFIError: Error, LocalizedError {
    case core(String)
    case nullPointer(String)
    case decode(String)
    case invalidArgument(String)

    public var errorDescription: String? {
        switch self {
        case .core(let msg): return msg
        case .nullPointer(let msg): return msg
        case .decode(let msg): return msg
        case .invalidArgument(let msg): return msg
        }
    }
}

/// A timeout means cleanup has NOT completed. Keep the code separate from diagnostic text.
/// A separate type preserves the existing WalletCoreFFIError enum's cases for consumers.
public struct WalletCoreLifecycleError: Error, LocalizedError {
    public let code: Int32
    public let message: String
    public init(code: Int32, message: String) { self.code = code; self.message = message }
    public var isTimeout: Bool { code == -31 }
    public var errorDescription: String? { message }
}

/// Minimal Swift wrapper for the WalletCore C FFI.
/// This exposes a small, safe surface area for opening, refreshing, balance querying,
/// fee preview, and sending transactions.
///
/// Notes:
/// - All functions throw WalletCoreFFIError on failure.
/// - Functions that wrap C functions returning char* will free those pointers automatically.
/// - Destinations are encoded as JSON and handed to the core for fee preview and send.
public enum WalletCoreFFIClient {
    /// Configures only I2P clients. Missing/invalid settings cannot fall back to clearnet.
    public static func setI2PHTTPProxy(_ proxy: String?) throws {
        let rc = proxy.map { value in value.withCString { wallet_set_i2p_http_proxy($0) } }
            ?? wallet_set_i2p_http_proxy(nil)
        try WalletCoreFFISupport.checkRC(rc, context: "configure I2P HTTP proxy")
    }
    /// Reads the last error message from the core, if any.
    /// The returned pointer is owned by the core and must be freed with walletcore_free_cstr.
    public static func lastErrorMessage() -> String? {
        WalletCoreFFISupport.lastErrorMessage()
    }
}
