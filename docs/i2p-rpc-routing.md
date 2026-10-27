# Explicit I2P wallet RPC routing (0.1.20)

I2P wallet RPC and the node's I2P P2P transaction zone are independent. RPC can
carry wallet scans as well as transaction submissions; Cuprate's block P2P sync
can remain on clearnet. RPC submissions do not count as I2P P2P receipts.

Before an I2P operation, configure the HTTP proxy explicitly:

```swift
try WalletCoreFFIClient.setI2PHTTPProxy("127.0.0.1:4444")
```

Rust hosts use `api::set_i2p_http_proxy(Some("127.0.0.1:4444"))`; C/JNI hosts
use `wallet_set_i2p_http_proxy`. Use the **HTTP** proxy, not SOCKS `:4447`.
`127.0.0.1` refers to the device running the app. No I2P router is embedded.

Every RPC client captures its route at construction. `.i2p` hosts require a valid
configured HTTP proxy and are never resolved directly. Missing/invalid proxy
settings, connection failures and redirects fail closed. Clearnet RPC is direct;
ambient HTTP_PROXY, ALL_PROXY and NO_PROXY do not override either route. HTTPS
and SOCKS proxy schemes are rejected rather than silently interpreted as HTTP.
HTTP I2P RPC streams are carried by I2P; a LAN hop to a remote HTTP proxy is not
itself encrypted. Use a local proxy or a trusted/VPN-protected connection.

Setting an invalid new proxy invalidates creation of new I2P clients. Previously
created clients retain their captured route, even when settings change. An app
requiring an immediate routing change must first stop its existing operations.
Setting NULL/empty disables new I2P clients; it does not enable a direct fallback.

Hosts must also validate the selected policy: I2P-only requires an I2P scan and
broadcast endpoint; hybrid requires an I2P broadcast endpoint. Reject a clearnet
URL entered in an I2P field, including a saved pending-send endpoint which no
longer matches the chosen policy. Never silently substitute the clearnet default.
Optional fiat/explorer services are separate from wallet-node routing.

The async daemon transport and blocking bulk/metadata transport share the same
agent factory. Responses are bounded; async blocking HTTP work runs off the
Tokio worker threads, and each async client serializes complete response reads.
Digest authentication is retained for password-protected nodes. Node redirects
are not followed. No wallet cache or blockchain format change is involved.

Regression fixtures use real loopback HTTP servers/proxies and synthetic bytes,
not live transactions. They cover RPC scanning, binary RPC and raw transaction
submission paths; missing/invalid/dead proxies; redirects and response bounds;
concurrent direct/I2P clients and ambient proxy bypass variables. The opt-in live
test only reads height and verifies private diagnostic methods are blocked.

Apple/JNI consumers must rebuild against the new native artifact and matching
header. Updating Swift/Kotlin source alone does not fix an installed app.
