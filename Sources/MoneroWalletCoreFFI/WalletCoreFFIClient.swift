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

/// Minimal Swift wrapper for the WalletCore C FFI.
/// This exposes a small, safe surface area for opening, refreshing, balance querying,
/// fee preview, and sending transactions.
///
/// Notes:
/// - All functions throw WalletCoreFFIError on failure.
/// - Functions that wrap C functions returning char* will free those pointers automatically.
/// - Destinations are encoded as JSON and handed to the core for fee preview and send.
public enum WalletCoreFFIClient {
    /// Reads the last error message from the core, if any.
    /// The returned pointer is owned by the core and must be freed with walletcore_free_cstr.
    public static func lastErrorMessage() -> String? {
        WalletCoreFFISupport.lastErrorMessage()
    }
}
