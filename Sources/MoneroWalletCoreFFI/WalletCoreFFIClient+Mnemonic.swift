import Foundation

#if canImport(MoneroWalletCore)
import MoneroWalletCore
#elseif canImport(CLibMoneroWalletCore)
import CLibMoneroWalletCore
#else
#error("MoneroWalletCoreFFI: Missing C module. Ensure the xcframework (Apple) or system library (Linux) is available.")
#endif

public extension WalletCoreFFIClient {
    static func derivePrimaryAddressFromSeed(seedData: Data, mainnet: Bool = true) throws -> String {
        var buffer = Array<CChar>(repeating: 0, count: 192)
        var written: Int = 0
        let rc: Int32 = seedData.withUnsafeBytes { rawBuf in
            guard let base = rawBuf.bindMemory(to: UInt8.self).baseAddress else { return Int32(-11) }
            return wallet_primary_address_from_seed(
                base,
                rawBuf.count,
                mainnet ? 1 : 0,
                &buffer,
                buffer.count,
                &written
            )
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_primary_address_from_seed")
        buffer[min(written, buffer.count - 1)] = 0
        let addrBytes = buffer.prefix(min(written, buffer.count - 1)).map { UInt8(bitPattern: $0) }
        return String(decoding: addrBytes, as: UTF8.self)
    }

    static func derivePrimaryAddressFromSeedHex(seedHex: String, mainnet: Bool = true) throws -> String {
        guard let seed = WalletCoreFFISupport.data(fromHex: seedHex) else {
            throw WalletCoreFFIError.invalidArgument("Seed hex is invalid")
        }
        return try derivePrimaryAddressFromSeed(seedData: seed, mainnet: mainnet)
    }

    static func derivePrimaryAddressFromMnemonic(_ phrase: String, mainnet: Bool = true) throws -> String {
        var buffer = Array<CChar>(repeating: 0, count: 192)
        var written: Int = 0
        let rc: Int32 = phrase.withCString { cstr in
            wallet_primary_address_from_mnemonic(
                cstr,
                mainnet ? 1 : 0,
                &buffer,
                buffer.count,
                &written
            )
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_primary_address_from_mnemonic")
        buffer[min(written, buffer.count - 1)] = 0
        let addrBytes = buffer.prefix(min(written, buffer.count - 1)).map { UInt8(bitPattern: $0) }
        return String(decoding: addrBytes, as: UTF8.self)
    }

    static func generateMnemonicEnglish() throws -> String {
        var buffer = Array<CChar>(repeating: 0, count: 512)
        var written: Int = 0
        let rc: Int32 = wallet_generate_mnemonic_english(
            &buffer,
            buffer.count,
            &written
        )
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_generate_mnemonic_english")
        buffer[min(written, buffer.count - 1)] = 0
        let bytes = buffer.prefix(min(written, buffer.count - 1)).map { UInt8(bitPattern: $0) }
        return String(decoding: bytes, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
    }

    static func deriveSubaddressFromMnemonic(
        _ phrase: String,
        accountIndex: UInt32 = 0,
        subaddressIndex: UInt32,
        mainnet: Bool = true
    ) throws -> String {
        var buffer = Array<CChar>(repeating: 0, count: 192)
        var written: Int = 0
        let rc: Int32 = phrase.withCString { cstr in
            wallet_derive_subaddress_from_mnemonic(
                cstr,
                accountIndex,
                subaddressIndex,
                mainnet ? 1 : 0,
                &buffer,
                buffer.count,
                &written
            )
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_derive_subaddress_from_mnemonic")
        buffer[min(written, buffer.count - 1)] = 0
        let addrBytes = buffer.prefix(min(written, buffer.count - 1)).map { UInt8(bitPattern: $0) }
        return String(decoding: addrBytes, as: UTF8.self)
    }

    static func deriveAddressFromSeed(
        seedData: Data,
        accountIndex: UInt32,
        subaddressIndex: UInt32,
        mainnet: Bool = true
    ) throws -> String {
        var buffer = Array<CChar>(repeating: 0, count: 192)
        var written: Int = 0
        let rc: Int32 = seedData.withUnsafeBytes { rawBuf in
            guard let base = rawBuf.bindMemory(to: UInt8.self).baseAddress else { return Int32(-11) }
            return wallet_derive_address_from_seed(
                base,
                rawBuf.count,
                mainnet ? 1 : 0,
                accountIndex,
                subaddressIndex,
                &buffer,
                buffer.count,
                &written
            )
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_derive_address_from_seed")
        buffer[min(written, buffer.count - 1)] = 0
        let addrBytes = buffer.prefix(min(written, buffer.count - 1)).map { UInt8(bitPattern: $0) }
        return String(decoding: addrBytes, as: UTF8.self)
    }

    static func deriveAddressFromSeedHex(
        seedHex: String,
        accountIndex: UInt32,
        subaddressIndex: UInt32,
        mainnet: Bool = true
    ) throws -> String {
        guard let seed = WalletCoreFFISupport.data(fromHex: seedHex) else {
            throw WalletCoreFFIError.invalidArgument("Seed hex is invalid")
        }
        return try deriveAddressFromSeed(
            seedData: seed,
            accountIndex: accountIndex,
            subaddressIndex: subaddressIndex,
            mainnet: mainnet
        )
    }
}
