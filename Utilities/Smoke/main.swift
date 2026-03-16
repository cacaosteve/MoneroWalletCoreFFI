import Foundation
import MoneroWalletCoreFFI

@main
struct Smoke {
    static func main() {
        let env = ProcessInfo.processInfo.environment

        // Inputs from environment or defaults
        let walletId = env["WALLET_ID"]?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmpty ?? "smoke_wallet"
        guard let mnemonic = env["WALLET_MNEMONIC"]?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmpty else {
            Smoke.printUsageAndExit("WALLET_MNEMONIC is required")
        }

        let restoreHeight: UInt64 = {
            if let s = env["WALLET_RESTORE_HEIGHT"]?.trimmingCharacters(in: .whitespacesAndNewlines), let v = UInt64(s) {
                return v
            }
            return 0
        }()

        let nodeURL = env["MONERO_URL"]?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmpty
        let destinationAddress = env["SMOKE_DEST_ADDRESS"]?.trimmingCharacters(in: .whitespacesAndNewlines).nonEmpty
        let runRefresh = envFlag("SMOKE_REFRESH", defaultValue: false)
        let runCancel = envFlag("SMOKE_CANCEL_REFRESH", defaultValue: false)

        let mainnet: Bool = {
            let st = env["STAGENET"]?.lowercased()
            switch st {
            case "1", "true", "yes": return false
            default: return true
            }
        }()

        print("==== MoneroWalletCoreFFI Smoke Test ====")
        print("- Version: \(WalletCoreFFIClient.version())")
        print("- Wallet ID: \(walletId)")
        print("- Network: \(mainnet ? "mainnet" : "stagenet")")
        print("- Restore height: \(restoreHeight)")
        print("- Refresh enabled: \(runRefresh)")
        print("- Cancel refresh enabled: \(runCancel)")
        if let nodeURL { print("- Node URL: \(nodeURL)") }
        if let destinationAddress { print("- Destination address: \(destinationAddress)") }

        do {
            let generatedMnemonic = try WalletCoreFFIClient.generateMnemonicEnglish()
            print("✔ generateMnemonicEnglish: OK (\(generatedMnemonic.split(separator: " ").count) words)")

            let primaryAddress = try WalletCoreFFIClient.derivePrimaryAddressFromMnemonic(mnemonic, mainnet: mainnet)
            print("✔ derivePrimaryAddressFromMnemonic: OK (\(primaryAddress.prefix(12))...)")

            let subaddress = try WalletCoreFFIClient.deriveSubaddressFromMnemonic(
                mnemonic,
                accountIndex: 0,
                subaddressIndex: 1,
                mainnet: mainnet
            )
            print("✔ deriveSubaddressFromMnemonic: OK (\(subaddress.prefix(12))...)")

            // 1) Open/register wallet
            try WalletCoreFFIClient.openWalletFromMnemonic(
                walletId: walletId,
                mnemonic: mnemonic,
                restoreHeight: restoreHeight,
                mainnet: mainnet
            )
            print("✔ openWalletFromMnemonic: OK")

            let initialStatus = try WalletCoreFFIClient.syncStatus(walletId: walletId)
            print("✔ syncStatus: chainHeight=\(initialStatus.chainHeight) lastScanned=\(initialStatus.lastScanned) restoreHeight=\(initialStatus.restoreHeight)")

            if let cache = try WalletCoreFFIClient.exportCache(walletId: walletId) {
                print("✔ exportCache: \(cache.count) bytes")
                try WalletCoreFFIClient.importCache(walletId: walletId, cacheBlob: cache)
                print("✔ importCache: OK")
            } else {
                print("• exportCache: no cache available yet")
            }

            if runRefresh {
                if runCancel {
                    try WalletCoreFFIClient.refreshWalletAsync(walletId: walletId, nodeURL: nodeURL)
                    print("✔ refreshWalletAsync: started")
                    try WalletCoreFFIClient.refreshCancel(walletId: walletId)
                    print("✔ refreshCancel: requested")
                    let cancelledStatus = try WalletCoreFFIClient.syncStatus(walletId: walletId)
                    print("✔ syncStatus after cancel: lastScanned=\(cancelledStatus.lastScanned)")
                } else {
                    let lastScanned = try WalletCoreFFIClient.refreshWallet(walletId: walletId, nodeURL: nodeURL)
                    print("✔ refreshWallet: OK (lastScanned=\(lastScanned))")
                }
            } else {
                print("• refreshWallet: skipped (set SMOKE_REFRESH=1 to enable)")
            }

            let (total, unlocked) = try WalletCoreFFIClient.getBalance(walletId: walletId)
            print("✔ getBalance: total=\(total) piconero, unlocked=\(unlocked) piconero")

            let (filteredTotal, filteredUnlocked) = try WalletCoreFFIClient.getBalanceWithFilter(
                walletId: walletId,
                filter: ["subaddress_minor": 0]
            )
            print("✔ getBalanceWithFilter: total=\(filteredTotal) piconero, unlocked=\(filteredUnlocked) piconero")

            do {
                _ = try WalletCoreFFIClient.getBalanceWithFilter(
                    walletId: walletId,
                    filter: ["invalid": Date()]
                )
                throw NSError(domain: "Smoke", code: 1, userInfo: [NSLocalizedDescriptionKey: "Expected invalid filter to throw"])
            } catch WalletCoreFFIError.invalidArgument {
                print("✔ invalid filter handling: rejected malformed filter input")
            }

            let transfers = try WalletCoreFFIClient.listTransfers(walletId: walletId)
            print("✔ listTransfers: \(transfers.count) rows")

            if let destinationAddress {
                let fee = try WalletCoreFFIClient.previewFee(
                    walletId: walletId,
                    destinations: [.init(address: destinationAddress, amount: 1)]
                )
                print("✔ previewFee: fee=\(fee)")

                let filteredFee = try WalletCoreFFIClient.previewFeeWithFilter(
                    walletId: walletId,
                    destinations: [.init(address: destinationAddress, amount: 1)],
                    filter: ["subaddress_minor": 0]
                )
                print("✔ previewFeeWithFilter: fee=\(filteredFee)")

                let sweepPreview = try WalletCoreFFIClient.previewSweep(
                    walletId: walletId,
                    toAddress: destinationAddress
                )
                print("✔ previewSweep: amount=\(sweepPreview.amount) fee=\(sweepPreview.fee)")

                let filteredSweepPreview = try WalletCoreFFIClient.previewSweepWithFilter(
                    walletId: walletId,
                    toAddress: destinationAddress,
                    filter: ["subaddress_minor": 0]
                )
                print("✔ previewSweepWithFilter: amount=\(filteredSweepPreview.amount) fee=\(filteredSweepPreview.fee)")
            } else {
                print("• send/sweep previews: skipped (set SMOKE_DEST_ADDRESS)")
            }

            print("==== Smoke Test Succeeded ====")
            exit(EXIT_SUCCESS)
        } catch {
            print("✖ Smoke Test Failed: \(error.localizedDescription)")
            if let err = error as? WalletCoreFFIError {
                switch err {
                case .core(let msg): print("Core error: \(msg)")
                case .nullPointer(let msg): print("Null pointer: \(msg)")
                case .decode(let msg): print("Decode error: \(msg)")
                case .invalidArgument(let msg): print("Invalid argument: \(msg)")
                }
            }
            exit(EXIT_FAILURE)
        }
    }

    private static func printUsageAndExit(_ message: String? = nil) -> Never {
        if let message { fputs("Error: \(message)\n", stderr) }
        let usage = """
        Usage (environment variables):
          WALLET_MNEMONIC         25-word mnemonic (required)
          WALLET_ID               Stable id for the wallet (default: "smoke_wallet")
          WALLET_RESTORE_HEIGHT   Starting scan height (default: 0)
          MONERO_URL              Daemon URL, e.g. http://127.0.0.1:18081 (optional)
          SMOKE_DEST_ADDRESS      Optional destination address for fee/sweep preview checks
          SMOKE_REFRESH           Set to 1/true/yes to perform refresh calls
          SMOKE_CANCEL_REFRESH    Set to 1/true/yes to test refresh async + cancel instead of full refresh
          STAGENET                If set to 1/true/yes, use stagenet; otherwise mainnet

        Example:
          WALLET_MNEMONIC=\"... 25 words ...\" \\
          WALLET_ID=smoke_wallet \\
          WALLET_RESTORE_HEIGHT=3000000 \\
          MONERO_URL=http://127.0.0.1:38081 \\
          STAGENET=1 \\
          swift run MoneroWalletCoreFFI_Smoke

        """
        fputs(usage + "\n", stderr)
        exit(EXIT_FAILURE)
    }
}

private extension String {
    var nonEmpty: String? { isEmpty ? nil : self }
}

private func envFlag(_ key: String, defaultValue: Bool) -> Bool {
    guard let raw = ProcessInfo.processInfo.environment[key]?.trimmingCharacters(in: .whitespacesAndNewlines).lowercased() else {
        return defaultValue
    }
    switch raw {
    case "1", "true", "yes", "y", "on":
        return true
    case "0", "false", "no", "n", "off":
        return false
    default:
        return defaultValue
    }
}
