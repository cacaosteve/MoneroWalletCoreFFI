import Foundation

#if canImport(MoneroWalletCore)
import MoneroWalletCore
#elseif canImport(CLibMoneroWalletCore)
import CLibMoneroWalletCore
#else
#error("MoneroWalletCoreFFI: Missing C module. Ensure the xcframework (Apple) or system library (Linux) is available.")
#endif

enum WalletCoreFFISupport {
    static func lastErrorMessage() -> String? {
        guard let cstr = walletcore_last_error_message() else { return nil }
        let s = String(cString: cstr)
        _ = walletcore_free_cstr(cstr)
        return s
    }

    static func takeCString(_ ptr: UnsafeMutablePointer<CChar>?, context: String) throws -> String {
        guard let p = ptr else {
            let reason = lastErrorMessage() ?? "FFI returned null string (\(context))"
            throw WalletCoreFFIError.nullPointer(reason)
        }
        defer { _ = walletcore_free_cstr(p) }
        return String(cString: p)
    }

    @inline(__always)
    static func checkRC(_ rc: Int32, context: String) throws {
        guard rc == 0 else {
            let reason = lastErrorMessage() ?? "FFI error in \(context) (rc=\(rc))"
            throw WalletCoreFFIError.core(reason)
        }
    }

    static let jsonEncoder: JSONEncoder = {
        let enc = JSONEncoder()
        enc.outputFormatting = [.withoutEscapingSlashes]
        return enc
    }()

    static let jsonDecoder: JSONDecoder = {
        JSONDecoder()
    }()

    static func encodeOptionalJSONObject(
        _ value: [String: Any]?,
        context: String
    ) throws -> String? {
        guard let value else { return nil }
        guard JSONSerialization.isValidJSONObject(value) else {
            throw WalletCoreFFIError.invalidArgument("Invalid JSON object for \(context)")
        }
        let data = try JSONSerialization.data(withJSONObject: value, options: [])
        guard let json = String(data: data, encoding: .utf8) else {
            throw WalletCoreFFIError.invalidArgument("Failed to encode \(context) as UTF-8 JSON")
        }
        return json
    }

    static func data(fromHex hex: String) -> Data? {
        let s = hex.trimmingCharacters(in: .whitespacesAndNewlines)
        let len = s.count
        if len == 0 { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(len / 2)

        var index = s.startIndex
        func val(_ c: Character) -> UInt8? {
            switch c {
            case "0"..."9": return UInt8(c.asciiValue! - Character("0").asciiValue!)
            case "a"..."f": return 10 + UInt8(c.asciiValue! - Character("a").asciiValue!)
            case "A"..."F": return 10 + UInt8(c.asciiValue! - Character("A").asciiValue!)
            default: return nil
            }
        }

        while index < s.endIndex {
            let next = s.index(after: index)
            guard next < s.endIndex else { return nil }
            let c1 = s[index], c2 = s[next]
            guard let v1 = val(c1), let v2 = val(c2) else { return nil }
            bytes.append((v1 << 4) | v2)
            index = s.index(next, offsetBy: 1)
        }
        return Data(bytes)
    }
}
