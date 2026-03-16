import Foundation

#if canImport(MoneroWalletCore)
import MoneroWalletCore
#elseif canImport(CLibMoneroWalletCore)
import CLibMoneroWalletCore
#else
#error("MoneroWalletCoreFFI: Missing C module. Ensure the xcframework (Apple) or system library (Linux) is available.")
#endif

public extension WalletCoreFFIClient {
    static func version() -> String {
        guard let c = walletcore_version() else { return "unknown" }
        let s = String(cString: c)
        _ = walletcore_free_cstr(c)
        return s
    }

    static func openWalletFromMnemonic(
        walletId: String,
        mnemonic: String,
        restoreHeight: UInt64 = 0,
        mainnet: Bool = true
    ) throws {
        let rc = walletId.withCString { cId in
            mnemonic.withCString { cMn in
                wallet_open_from_mnemonic(cId, cMn, restoreHeight, mainnet ? 1 : 0)
            }
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_open_from_mnemonic")
    }

    static func setGapLimit(
        walletId: String,
        gapLimit: UInt32
    ) throws {
        let rc = walletId.withCString { cId in
            wallet_set_gap_limit(cId, gapLimit)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_set_gap_limit")
    }

    static func forceRescanFromHeight(
        walletId: String,
        fromHeight: UInt64
    ) throws {
        let rc = walletId.withCString { cId in
            wallet_force_rescan_from_height(cId, fromHeight)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_force_rescan_from_height")
    }

    static func resetTrackedOutputs(walletId: String) throws {
        let rc = walletId.withCString { cId in
            wallet_reset_tracked_outputs(cId)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_reset_tracked_outputs")
    }

    static func startZmqListener(endpoint: String) throws {
        let rc = endpoint.withCString { cEndpoint in
            wallet_start_zmq_listener(cEndpoint)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_start_zmq_listener")
    }

    static func stopZmqListener() throws {
        let rc = wallet_stop_zmq_listener()
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_stop_zmq_listener")
    }

    static func zmqSequence() throws -> UInt64 {
        var value: UInt64 = 0
        let rc = wallet_zmq_sequence(&value)
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_zmq_sequence")
        return value
    }

    static func refreshWallet(
        walletId: String,
        nodeURL: String? = nil
    ) throws -> UInt64 {
        var lastScanned: UInt64 = 0
        let rc: Int32 = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    wallet_refresh(cId, cNode, &lastScanned)
                }
            } else {
                return wallet_refresh(cId, nil, &lastScanned)
            }
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_refresh")
        return lastScanned
    }

    static func refreshWalletAsync(
        walletId: String,
        nodeURL: String? = nil
    ) throws {
        let rc: Int32 = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    wallet_refresh_async(cId, cNode)
                }
            } else {
                return wallet_refresh_async(cId, nil)
            }
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_refresh_async")
    }

    static func refreshCancel(walletId: String) throws {
        let rc: Int32 = walletId.withCString { cId in
            wallet_refresh_cancel(cId)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_refresh_cancel")
    }

    static func syncStatus(walletId: String) throws -> SyncStatus {
        var chainHeight: UInt64 = 0
        var chainTime: UInt64 = 0
        var lastRefreshTimestamp: UInt64 = 0
        var lastScanned: UInt64 = 0
        var restoreHeight: UInt64 = 0
        let rc: Int32 = walletId.withCString { cId in
            wallet_sync_status(cId, &chainHeight, &chainTime, &lastRefreshTimestamp, &lastScanned, &restoreHeight)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_sync_status")
        return SyncStatus(
            chainHeight: chainHeight,
            chainTime: chainTime,
            lastRefreshTimestamp: lastRefreshTimestamp,
            lastScanned: lastScanned,
            restoreHeight: restoreHeight
        )
    }

    static func getBalance(walletId: String) throws -> (total: UInt64, unlocked: UInt64) {
        var total: UInt64 = 0
        var unlocked: UInt64 = 0
        let rc = walletId.withCString { cId in
            wallet_get_balance(cId, &total, &unlocked)
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_get_balance")
        return (total: total, unlocked: unlocked)
    }

    static func getBalanceWithFilter(
        walletId: String,
        filter: [String: Any]? = nil
    ) throws -> (total: UInt64, unlocked: UInt64) {
        let filterJSON = try WalletCoreFFISupport.encodeOptionalJSONObject(filter, context: "wallet_get_balance_with_filter filter")

        var total: UInt64 = 0
        var unlocked: UInt64 = 0

        let rc: Int32 = walletId.withCString { cId in
            if let f = filterJSON {
                return f.withCString { cFilter in
                    wallet_get_balance_with_filter(cId, cFilter, &total, &unlocked)
                }
            } else {
                return wallet_get_balance_with_filter(cId, nil, &total, &unlocked)
            }
        }

        try WalletCoreFFISupport.checkRC(rc, context: "wallet_get_balance_with_filter")
        return (total: total, unlocked: unlocked)
    }
}
