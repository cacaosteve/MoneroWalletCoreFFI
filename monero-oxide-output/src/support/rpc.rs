//! RPC/daemon client support types used across FFI modules.
//!
//! All daemon HTTP calls share the same explicit, fail-closed routing policy.

use super::response_limits::{
    read_response, read_response_detailed, MAX_BINARY_RESPONSE_BYTES, MAX_JSON_RESPONSE_BYTES,
};
use monero_daemon_rpc::{HttpTransport, MoneroDaemon};
use monero_interface::InterfaceError;
use std::sync::Arc;

#[derive(Clone)]
pub(crate) struct RpcTransport {
    agent: ureq::Agent,
    base_url: String,
    credentials: Option<(zeroize::Zeroizing<String>, zeroize::Zeroizing<String>)>,
    // Read the entire response before reusing the connection, as monerod requires.
    serial: Arc<tokio::sync::Mutex<()>>,
}

fn error(message: impl Into<String>) -> InterfaceError {
    InterfaceError::InterfaceError(message.into())
}

impl RpcTransport {
    pub(crate) fn for_url(raw: &str) -> Result<Self, InterfaceError> {
        let mut url = url::Url::parse(raw).map_err(|_| error("Invalid daemon URL"))?;
        let credentials = if !url.username().is_empty() {
            Some((
                zeroize::Zeroizing::new(url.username().to_owned()),
                zeroize::Zeroizing::new(url.password().unwrap_or("").to_owned()),
            ))
        } else {
            None
        };
        url.set_username("")
            .map_err(|_| error("Invalid daemon URL"))?;
        url.set_password(None)
            .map_err(|_| error("Invalid daemon URL"))?;
        // Snapshot the route once. Later settings changes cannot alter this client.
        let agent = super::routing::agent_for_url(url.as_str()).map_err(error)?;
        Ok(Self {
            agent,
            base_url: url.as_str().trim_end_matches('/').to_owned(),
            credentials,
            serial: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(crate) async fn new(raw: String) -> Result<RpcClient, InterfaceError> {
        MoneroDaemon::new(Self::for_url(&raw)?).await
    }

    fn post_blocking(
        &self,
        route: &str,
        body: &[u8],
        limit: usize,
    ) -> Result<Vec<u8>, InterfaceError> {
        // `route` comes from the daemon interface, never a redirect target.
        if route.contains("://") || route.starts_with("//") || route.contains(['\r', '\n']) {
            return Err(error("Invalid daemon RPC route"));
        }
        let url = format!("{}/{}", self.base_url, route.trim_start_matches('/'));
        let content_type = if route.ends_with(".bin") {
            "application/octet-stream"
        } else {
            "application/json"
        };
        let request = || self.agent.post(&url).set("Content-Type", content_type);
        let send = |request: ureq::Request| match request.send_bytes(body) {
            Ok(response) | Err(ureq::Error::Status(_, response)) => Ok(response),
            // Do not include URLs, credentials or transaction data in errors.
            Err(_) => Err(error(
                "Daemon transport failed; no direct-network fallback attempted",
            )),
        };
        let mut response = send(request())?;
        if response.status() == 401 {
            if let Some((username, password)) = &self.credentials {
                let challenge = response.header("WWW-Authenticate").unwrap_or("").to_owned();
                let _ = read_response(response, limit)
                    .map_err(|_| error("Invalid authentication response"))?;
                let mut challenge = digest_auth::parse(&challenge)
                    .map_err(|_| error("Invalid digest authentication challenge"))?;
                let uri = url::Url::parse(&url).map_err(|_| error("Invalid daemon URL"))?;
                let context = digest_auth::AuthContext::new_post(
                    username.as_str(),
                    password.as_str(),
                    uri.path(),
                    Some(body),
                );
                let header = challenge
                    .respond(&context)
                    .map_err(|_| error("Digest authentication failed"))?
                    .to_header_string();
                response = send(request().set("Authorization", &header))?;
            }
        }
        if (300..400).contains(&response.status()) {
            return Err(error(
                "Daemon redirects are forbidden; no network fallback attempted",
            ));
        }
        if response.status() == 401 {
            return Err(error("Daemon authentication failed"));
        }
        let status = response.status();
        let content_length = response
            .header("Content-Length")
            .map(|value| {
                value
                    .parse::<u64>()
                    .map(|length| length.to_string())
                    .unwrap_or_else(|_| "invalid".to_owned())
            })
            .unwrap_or_else(|| "absent".to_owned());
        let response_read_started = std::time::Instant::now();
        read_response_detailed(response, limit).map_err(|failure| {
            walletcore_diagnostic!(
                "❌ daemon response read failed route={} status={} content_length={} bytes_read={} limit={} elapsed_ms={} kind={:?} error={}",
                route,
                status,
                content_length,
                failure.bytes_read,
                limit,
                response_read_started.elapsed().as_millis(),
                failure.kind,
                failure.message,
            );
            error("Invalid or oversized daemon HTTP response")
        })
    }
}

impl HttpTransport for RpcTransport {
    async fn post(
        &self,
        route: &str,
        body: Vec<u8>,
        response_size_limit: Option<usize>,
    ) -> Result<Vec<u8>, InterfaceError> {
        let _serial = self.serial.lock().await;
        let transport = self.clone();
        let route = route.to_owned();
        let cap = if route.ends_with(".bin") {
            MAX_BINARY_RESPONSE_BYTES
        } else {
            MAX_JSON_RESPONSE_BYTES
        };
        let limit = response_size_limit.unwrap_or(cap).min(cap);
        // ureq is blocking; never block the async runtime's worker threads.
        tokio::task::spawn_blocking(move || transport.post_blocking(&route, &body, limit))
            .await
            .map_err(|_| error("Daemon HTTP worker failed"))?
    }
}

/// The concrete daemon client type used throughout walletcore.
///
/// All extracted modules use the same routed transport, including native scan workers.
pub(crate) type RpcClient = MoneroDaemon<RpcTransport>;

/// Common monero-interface traits we rely on for method resolution.
///
/// Note: traits must be in scope for method syntax (`client.fee_rate(...)`) to work.
pub(crate) use monero_interface::{
    ProvidesBlockchainMeta as _, ProvidesFeeRates as _, ProvidesScannableBlocks as _,
};

// Bring MoneroDaemon's JSON-RPC helper into scope for callers which need bespoke daemon methods
// (e.g. `is_key_image_spent` preflight).
pub(crate) use monero_daemon_rpc::MoneroDaemon as _;

#[cfg(test)]
#[path = "routing_tests.rs"]
mod routing_tests;
