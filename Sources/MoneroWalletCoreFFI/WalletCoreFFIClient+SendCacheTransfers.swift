import Foundation

#if canImport(MoneroWalletCore)
import MoneroWalletCore
#elseif canImport(CLibMoneroWalletCore)
import CLibMoneroWalletCore
#else
#error("MoneroWalletCoreFFI: Missing C module. Ensure the xcframework (Apple) or system library (Linux) is available.")
#endif

public extension WalletCoreFFIClient {
    static func prepareSend(
        walletId: String,
        toAddress: String,
        amountPiconero: UInt64,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> PreparedSend {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddress in
                        wallet_prepare_send(cId, cNode, cAddress, amountPiconero, ringLen)
                    }
                }
            }
            return toAddress.withCString { cAddress in
                wallet_prepare_send(cId, nil, cAddress, amountPiconero, ringLen)
            }
        }

        let string = try WalletCoreFFISupport.takeCString(raw, context: "wallet_prepare_send")
        guard let data = string.data(using: .utf8) else {
            throw WalletCoreFFIError.decode("wallet_prepare_send returned non-UTF8 data")
        }
        do {
            return try WalletCoreFFISupport.jsonDecoder.decode(PreparedSend.self, from: data)
        } catch {
            throw WalletCoreFFIError.decode("Unexpected wallet_prepare_send payload: \(string)")
        }
    }

    static func relayPrepared(
        walletId: String,
        prepared: PreparedSend,
        nodeURL: String? = nil
    ) throws -> RelayResult {
        let payloadData = try WalletCoreFFISupport.jsonEncoder.encode(prepared)
        guard let payload = String(data: payloadData, encoding: .utf8) else {
            throw WalletCoreFFIError.invalidArgument("Failed to encode prepared transaction")
        }

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    payload.withCString { cPayload in
                        wallet_relay_prepared(cId, cNode, cPayload)
                    }
                }
            }
            return payload.withCString { cPayload in
                wallet_relay_prepared(cId, nil, cPayload)
            }
        }

        let string = try WalletCoreFFISupport.takeCString(raw, context: "wallet_relay_prepared")
        guard let data = string.data(using: .utf8) else {
            throw WalletCoreFFIError.decode("wallet_relay_prepared returned non-UTF8 data")
        }
        do {
            return try WalletCoreFFISupport.jsonDecoder.decode(RelayResult.self, from: data)
        } catch {
            throw WalletCoreFFIError.decode("Unexpected wallet_relay_prepared payload: \(string)")
        }
    }

    static func previewFee(
        walletId: String,
        destinations: [Destination],
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> UInt64 {
        let jsonData = try WalletCoreFFISupport.jsonEncoder.encode(destinations)
        guard let jsonStr = String(data: jsonData, encoding: .utf8) else {
            throw WalletCoreFFIError.invalidArgument("Failed to encode destinations as UTF-8 JSON")
        }

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    jsonStr.withCString { cDest in
                        wallet_preview_fee(cId, cNode, cDest, ringLen)
                    }
                }
            } else {
                return jsonStr.withCString { cDest in
                    wallet_preview_fee(cId, nil, cDest, ringLen)
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_preview_fee")
        if let data = s.data(using: .utf8),
           let res = try? WalletCoreFFISupport.jsonDecoder.decode(FeeResult.self, from: data) {
            return res.fee
        }
        if let fee = UInt64(s.trimmingCharacters(in: .whitespacesAndNewlines)) {
            return fee
        }
        throw WalletCoreFFIError.decode("Unexpected preview_fee payload: \(s)")
    }

    static func previewFeeWithFilter(
        walletId: String,
        destinations: [Destination],
        filter: [String: Any]? = nil,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> UInt64 {
        let destData = try WalletCoreFFISupport.jsonEncoder.encode(destinations)
        guard let destJSON = String(data: destData, encoding: .utf8) else {
            throw WalletCoreFFIError.invalidArgument("Failed to encode destinations as UTF-8 JSON")
        }

        let filterJSON = try WalletCoreFFISupport.encodeOptionalJSONObject(filter, context: "wallet_preview_fee_with_filter filter")

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    return destJSON.withCString { cDest in
                        if let f = filterJSON {
                            return f.withCString { cFilter in
                                wallet_preview_fee_with_filter(cId, cNode, cDest, cFilter, ringLen)
                            }
                        } else {
                            return wallet_preview_fee_with_filter(cId, cNode, cDest, nil, ringLen)
                        }
                    }
                }
            } else {
                return destJSON.withCString { cDest in
                    if let f = filterJSON {
                        return f.withCString { cFilter in
                            wallet_preview_fee_with_filter(cId, nil, cDest, cFilter, ringLen)
                        }
                    } else {
                        return wallet_preview_fee_with_filter(cId, nil, cDest, nil, ringLen)
                    }
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_preview_fee_with_filter")
        if let data = s.data(using: .utf8),
           let res = try? WalletCoreFFISupport.jsonDecoder.decode(FeeResult.self, from: data) {
            return res.fee
        }
        if let fee = UInt64(s.trimmingCharacters(in: .whitespacesAndNewlines)) {
            return fee
        }
        throw WalletCoreFFIError.decode("Unexpected preview_fee_with_filter payload: \(s)")
    }

    static func previewSweep(
        walletId: String,
        toAddress: String,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (amount: UInt64, fee: UInt64) {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddr in
                        wallet_preview_sweep(cId, cNode, cAddr, ringLen)
                    }
                }
            } else {
                return toAddress.withCString { cAddr in
                    wallet_preview_sweep(cId, nil, cAddr, ringLen)
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_preview_sweep")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SweepPreviewResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected preview_sweep payload: \(s)")
        }
        return (amount: res.amount, fee: res.fee)
    }

    static func previewSweepWithFilter(
        walletId: String,
        toAddress: String,
        filter: [String: Any]? = nil,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (amount: UInt64, fee: UInt64) {
        let filterJSON = try WalletCoreFFISupport.encodeOptionalJSONObject(filter, context: "wallet_preview_sweep_with_filter filter")

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddr in
                        if let f = filterJSON {
                            return f.withCString { cFilter in
                                wallet_preview_sweep_with_filter(cId, cNode, cAddr, cFilter, ringLen)
                            }
                        } else {
                            return wallet_preview_sweep_with_filter(cId, cNode, cAddr, nil, ringLen)
                        }
                    }
                }
            } else {
                return toAddress.withCString { cAddr in
                    if let f = filterJSON {
                        return f.withCString { cFilter in
                            wallet_preview_sweep_with_filter(cId, nil, cAddr, cFilter, ringLen)
                        }
                    } else {
                        return wallet_preview_sweep_with_filter(cId, nil, cAddr, nil, ringLen)
                    }
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_preview_sweep_with_filter")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SweepPreviewResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected preview_sweep_with_filter payload: \(s)")
        }
        return (amount: res.amount, fee: res.fee)
    }

    static func sweep(
        walletId: String,
        toAddress: String,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (txid: String, amount: UInt64, fee: UInt64) {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddr in
                        wallet_sweep(cId, cNode, cAddr, ringLen)
                    }
                }
            } else {
                return toAddress.withCString { cAddr in
                    wallet_sweep(cId, nil, cAddr, ringLen)
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_sweep")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SweepSendResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected sweep payload: \(s)")
        }
        return (txid: res.txid, amount: res.amount, fee: res.fee)
    }

    static func sweepWithFilter(
        walletId: String,
        toAddress: String,
        filter: [String: Any]? = nil,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (txid: String, amount: UInt64, fee: UInt64) {
        let filterJSON = try WalletCoreFFISupport.encodeOptionalJSONObject(filter, context: "wallet_sweep_with_filter filter")

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddr in
                        if let f = filterJSON {
                            return f.withCString { cFilter in
                                wallet_sweep_with_filter(cId, cNode, cAddr, cFilter, ringLen)
                            }
                        } else {
                            return wallet_sweep_with_filter(cId, cNode, cAddr, nil, ringLen)
                        }
                    }
                }
            } else {
                return toAddress.withCString { cAddr in
                    if let f = filterJSON {
                        return f.withCString { cFilter in
                            wallet_sweep_with_filter(cId, nil, cAddr, cFilter, ringLen)
                        }
                    } else {
                        return wallet_sweep_with_filter(cId, nil, cAddr, nil, ringLen)
                    }
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_sweep_with_filter")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SweepSendResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected sweep_with_filter payload: \(s)")
        }
        return (txid: res.txid, amount: res.amount, fee: res.fee)
    }

    static func send(
        walletId: String,
        toAddress: String,
        amountPiconero: UInt64,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (txid: String, fee: UInt64) {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    toAddress.withCString { cAddr in
                        wallet_send(cId, cNode, cAddr, amountPiconero, ringLen)
                    }
                }
            } else {
                return toAddress.withCString { cAddr in
                    wallet_send(cId, nil, cAddr, amountPiconero, ringLen)
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_send")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SendResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected send payload: \(s)")
        }
        return (txid: res.txid, fee: res.fee)
    }

    static func sendWithFilter(
        walletId: String,
        destinations: [Destination],
        filter: [String: Any]? = nil,
        ringLen: UInt8 = 16,
        nodeURL: String? = nil
    ) throws -> (txid: String, fee: UInt64) {
        let destData = try WalletCoreFFISupport.jsonEncoder.encode(destinations)
        guard let destJSON = String(data: destData, encoding: .utf8) else {
            throw WalletCoreFFIError.invalidArgument("Failed to encode destinations as UTF-8 JSON")
        }

        let filterJSON = try WalletCoreFFISupport.encodeOptionalJSONObject(filter, context: "wallet_send_with_filter filter")

        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            if let node = nodeURL {
                return node.withCString { cNode in
                    return destJSON.withCString { cDest in
                        if let f = filterJSON {
                            return f.withCString { cFilter in
                                wallet_send_with_filter(cId, cNode, cDest, cFilter, ringLen)
                            }
                        } else {
                            return wallet_send_with_filter(cId, cNode, cDest, nil, ringLen)
                        }
                    }
                }
            } else {
                return destJSON.withCString { cDest in
                    if let f = filterJSON {
                        return f.withCString { cFilter in
                            wallet_send_with_filter(cId, nil, cDest, cFilter, ringLen)
                        }
                    } else {
                        return wallet_send_with_filter(cId, nil, cDest, nil, ringLen)
                    }
                }
            }
        }

        let s = try WalletCoreFFISupport.takeCString(raw, context: "wallet_send_with_filter")
        guard let data = s.data(using: .utf8),
              let res = try? WalletCoreFFISupport.jsonDecoder.decode(SendResult.self, from: data) else {
            throw WalletCoreFFIError.decode("Unexpected send_with_filter payload: \(s)")
        }
        return (txid: res.txid, fee: res.fee)
    }

    static func importCache(
        walletId: String,
        cacheBlob: Data
    ) throws {
        let rc: Int32 = cacheBlob.withUnsafeBytes { rawBuf in
            guard let base = rawBuf.bindMemory(to: UInt8.self).baseAddress else {
                return Int32(-11)
            }
            return walletId.withCString { cId in
                wallet_import_cache(cId, base, rawBuf.count)
            }
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_import_cache")
    }

    static func exportCache(walletId: String) throws -> Data? {
        var required: Int = 0
        let probeRC: Int32 = walletId.withCString { cId in
            wallet_export_cache(cId, nil, 0, &required)
        }
        if probeRC != 0 && probeRC != -12 {
            try WalletCoreFFISupport.checkRC(probeRC, context: "wallet_export_cache (probe)")
        }
        guard required > 0 else { return nil }

        var buffer = Data(count: required)
        var written: Int = 0
        let rc: Int32 = buffer.withUnsafeMutableBytes { rawBuf in
            guard let base = rawBuf.bindMemory(to: UInt8.self).baseAddress else {
                return Int32(-11)
            }
            return walletId.withCString { cId in
                wallet_export_cache(cId, base, rawBuf.count, &written)
            }
        }
        try WalletCoreFFISupport.checkRC(rc, context: "wallet_export_cache")
        guard written <= buffer.count else {
            throw WalletCoreFFIError.core("wallet_export_cache reported invalid length (\(written) > \(buffer.count))")
        }
        return buffer.prefix(written)
    }

    static func exportOutputsJSON(walletId: String) throws -> String {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            wallet_export_outputs_json(cId)
        }
        return try WalletCoreFFISupport.takeCString(raw, context: "wallet_export_outputs_json")
    }

    static func observedOutputs(walletId: String) throws -> WalletObservedOutputsEnvelope {
        let json = try exportOutputsJSON(walletId: walletId)
        guard let data = json.data(using: .utf8) else {
            throw WalletCoreFFIError.decode("wallet_export_outputs_json returned non-UTF8")
        }
        do {
            return try WalletCoreFFISupport.jsonDecoder.decode(WalletObservedOutputsEnvelope.self, from: data)
        } catch {
            throw WalletCoreFFIError.decode("Failed to decode observed outputs: \(error.localizedDescription)")
        }
    }

    static func exportTransfersJSON(walletId: String) throws -> String {
        let raw: UnsafeMutablePointer<CChar>? = walletId.withCString { cId in
            wallet_list_transfers_json(cId)
        }
        return try WalletCoreFFISupport.takeCString(raw, context: "wallet_list_transfers_json")
    }

    static func listTransfers(walletId: String) throws -> [Transfer] {
        let json = try exportTransfersJSON(walletId: walletId)
        guard let data = json.data(using: .utf8) else {
            throw WalletCoreFFIError.decode("wallet_list_transfers_json returned non-UTF8")
        }
        do {
            return try WalletCoreFFISupport.jsonDecoder.decode([Transfer].self, from: data)
        } catch {
            throw WalletCoreFFIError.decode("Failed to decode transfers: \(error.localizedDescription)")
        }
    }
}
