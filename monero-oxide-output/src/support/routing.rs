//! Explicit, fail-closed I2P routing. No process proxy environment variables.
use std::sync::RwLock;

use once_cell::sync::Lazy;
use url::Url;

fn anonymous_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "i2p" || host.ends_with(".i2p") || host == "onion" || host.ends_with(".onion")
}

/// Defense in depth: an anonymous destination may never reach the system resolver,
/// even if a future HTTP-library change introduces a proxy bypass path.
pub(crate) fn resolve_direct(netloc: &str) -> std::io::Result<Vec<std::net::SocketAddr>> {
    use std::net::ToSocketAddrs;
    let host = netloc
        .rsplit_once(':')
        .map(|(host, _)| host)
        .unwrap_or(netloc);
    if anonymous_host(host) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Direct DNS for anonymity-network destinations is forbidden",
        ));
    }
    netloc.to_socket_addrs().map(Iterator::collect)
}

static I2P_PROXY: Lazy<RwLock<Result<Option<String>, String>>> =
    Lazy::new(|| RwLock::new(Ok(None)));

pub(crate) fn configure_i2p_proxy(raw: Option<&str>) -> Result<(), String> {
    let parsed = raw
        .filter(|s| !s.trim().is_empty())
        .map(parse_proxy)
        .transpose();
    // Invalid new settings must invalidate the route, never reuse a previous proxy.
    *I2P_PROXY
        .write()
        .map_err(|_| "I2P proxy configuration unavailable")? = parsed.clone();
    parsed.map(|_| ())
}

pub(crate) fn parse_proxy(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let text = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let url = Url::parse(&text).map_err(|_| "Invalid I2P HTTP proxy")?;
    if url.host_str().is_some_and(anonymous_host) {
        return Err("The local HTTP proxy cannot itself be an I2P/Tor destination".into());
    }
    if url.scheme() != "http"
        || url.host_str().is_none()
        || url.port_or_known_default().is_none()
        || url.port() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("I2P HTTP proxy must be http://host:port (not SOCKS or HTTPS)".into());
    }
    Ok(url.to_string())
}

pub(crate) fn proxy_for_url(raw_url: &str) -> Result<Option<String>, String> {
    let url = Url::parse(raw_url).map_err(|_| "Invalid daemon URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || url.fragment().is_some()
        || url.query().is_some()
        || url.port() == Some(0)
    {
        return Err("Daemon URL must use HTTP or HTTPS and have no fragment".into());
    }
    let host = url
        .host_str()
        .unwrap()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "i2p" || host.ends_with(".i2p") {
        if url.scheme() != "http" {
            return Err("Use http:// for the I2P RPC destination".into());
        }
        return I2P_PROXY
            .read()
            .map_err(|_| "I2P proxy configuration unavailable")?
            .clone()?
            .map(Some)
            .ok_or_else(|| {
                "An I2P HTTP proxy is required; direct connections are forbidden".into()
            });
    }
    // Never resolve other anonymity-network names through ordinary DNS either.
    if host == "onion" || host.ends_with(".onion") {
        return Err("Tor daemon routing is not configured".into());
    }
    Ok(None)
}

pub(crate) fn agent_for_url(raw_url: &str) -> Result<ureq::Agent, String> {
    let proxy = proxy_for_url(raw_url)?;
    let mut builder = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        // A daemon must not redirect the wallet to a clearnet URL or another host.
        .redirects(0)
        .resolver(resolve_direct)
        .try_proxy_from_env(false);
    if let Some(proxy) = proxy {
        builder = builder.proxy(ureq::Proxy::new(proxy).map_err(|_| "Invalid I2P HTTP proxy")?);
    }
    Ok(builder.build())
}
