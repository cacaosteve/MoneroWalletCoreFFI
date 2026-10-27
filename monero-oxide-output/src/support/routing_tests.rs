//! Real loopback HTTP fixtures: never spend funds or contact a live node.
use super::*;
use crate::support::routing::{configure_i2p_proxy, parse_proxy};
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::Mutex,
    time::{Duration, Instant},
};

static SERIAL: Mutex<()> = Mutex::new(());
struct Reset;
impl Drop for Reset {
    fn drop(&mut self) {
        let _ = configure_i2p_proxy(None);
    }
}

fn fixture(
    count: usize,
    status: &str,
    extra: &str,
    body: &[u8],
) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    fixture_responses(vec![
        (status.to_owned(), extra.to_owned(), body.to_vec());
        count
    ])
}

fn fixture_responses(
    responses: Vec<(String, String, Vec<u8>)>,
) -> (String, std::thread::JoinHandle<Vec<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let task = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = vec![];
        while requests.len() < responses.len() {
            match listener.accept() {
                Ok((mut socket, _)) => {
                    socket
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut request = vec![];
                    while !request.ends_with(b"\r\n\r\n") {
                        let mut byte = [0];
                        socket.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                        assert!(request.len() < 64 * 1024);
                    }
                    let header = String::from_utf8_lossy(&request);
                    let len = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    let offset = request.len();
                    request.resize(offset + len, 0);
                    socket.read_exact(&mut request[offset..]).unwrap();
                    let (status, extra, body) = &responses[requests.len()];
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).unwrap();
                    socket.write_all(body).unwrap();
                    requests.push(request);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "expected request was never sent to its configured proxy"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(err) => panic!("fixture accept: {err}"),
            }
        }
        requests
    });
    (format!("http://{address}"), task)
}

#[test]
fn digest_authentication_keeps_credentials_out_of_proxy_request_urls() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    let challenge = r#"Digest realm="monero-rpc", nonce="test-nonce", qop="auth", algorithm=MD5"#;
    let (proxy, task) = fixture_responses(vec![
        (
            "401 Unauthorized".into(),
            format!("WWW-Authenticate: {challenge}\r\n"),
            b"{}".to_vec(),
        ),
        ("200 OK".into(), String::new(), b"{}".to_vec()),
    ]);
    configure_i2p_proxy(Some(&proxy)).unwrap();
    let transport = RpcTransport::for_url("http://wallet:password@authenticated.i2p").unwrap();
    crate::TOKIO_RUNTIME
        .block_on(transport.post("json_rpc", b"{}".to_vec(), Some(1024)))
        .unwrap();
    let requests = task.join().unwrap();
    for request in &requests {
        assert!(request.starts_with(b"POST http://authenticated.i2p/json_rpc HTTP/1.1\r\n"));
        assert!(!String::from_utf8_lossy(request).contains("password"));
    }
    assert!(!String::from_utf8_lossy(&requests[0])
        .to_ascii_lowercase()
        .contains("authorization:"));
    let second = String::from_utf8_lossy(&requests[1]);
    let header = second
        .lines()
        .find_map(|line| line.strip_prefix("Authorization: "))
        .unwrap();
    let actual = digest_auth::AuthorizationHeader::parse(header).unwrap();
    assert_eq!(actual.username, "wallet");
    assert_eq!(actual.uri, "/json_rpc");
    let mut context =
        digest_auth::AuthContext::new_post("wallet", "password", "/json_rpc", Some(b"{}"));
    context.cnonce = actual.cnonce.clone().map(Into::into);
    let expected = digest_auth::parse(challenge)
        .unwrap()
        .respond(&context)
        .unwrap();
    assert_eq!(actual.response, expected.response);
}

#[test]
fn proxy_configuration_and_anonymous_names_fail_closed() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    configure_i2p_proxy(None).unwrap();
    assert_eq!(
        crate::support::routing::resolve_direct("EXAMPLE.I2P.:80")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        crate::support::routing::resolve_direct("example.onion:80")
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    for url in [
        "http://example.i2p",
        "http://EXAMPLE.I2P./",
        "http://example.onion",
        "http://example.i2p:0",
        "ftp://example.i2p",
    ] {
        assert!(
            RpcTransport::for_url(url).is_err(),
            "must never resolve directly: {url}"
        );
        assert!(crate::BlockingRpcTransport::new(url).is_err());
    }
    for proxy in [
        "socks5://127.0.0.1:4447",
        "https://127.0.0.1:4444",
        "http://127.0.0.1:4444/path",
        "http://user:pass@127.0.0.1:4444",
        "http://127.0.0.1:0",
        "://bad",
    ] {
        assert!(
            parse_proxy(proxy).is_err(),
            "invalid proxy accepted: {proxy}"
        );
    }
    assert!(parse_proxy("[::1]:4444").is_ok());
    configure_i2p_proxy(Some("127.0.0.1:4444")).unwrap();
    assert!(configure_i2p_proxy(Some("socks5://127.0.0.1:4447")).is_err());
    assert!(
        RpcTransport::for_url("http://example.i2p").is_err(),
        "invalid new settings must not reuse old settings"
    );
    assert!(
        RpcTransport::for_url("http://127.0.0.1:18092").is_ok(),
        "invalid I2P settings must not change a direct client"
    );
}

#[test]
fn scan_binary_and_transaction_rpc_all_use_the_proxy() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    let (proxy, task) = fixture(
        4,
        "200 OK",
        "",
        br#"{"error":{"code":-32600},"status":"OK"}"#,
    );
    configure_i2p_proxy(Some(&proxy)).unwrap();
    crate::TOKIO_RUNTIME.block_on(async {
        let daemon = RpcTransport::new("http://does-not-resolve.b32.i2p".into())
            .await
            .unwrap();
        daemon.rpc_call("get_height", None, 1024).await.unwrap();
        // Invalid synthetic payload sent ONLY to our mock HTTP proxy, not a live node.
        daemon
            .rpc_call(
                "send_raw_transaction",
                Some(r#"{"tx_as_hex":"00"}"#.into()),
                1024,
            )
            .await
            .unwrap();
    });
    crate::BlockingRpcTransport::new("http://does-not-resolve.b32.i2p")
        .unwrap()
        .post_bytes("get_blocks.bin", vec![1, 2, 3])
        .unwrap();
    let requests = task.join().unwrap();
    for request in &requests {
        let text = String::from_utf8_lossy(request);
        assert!(text.starts_with("POST http://does-not-resolve.b32.i2p/"));
    }
    assert!(String::from_utf8_lossy(&requests[2]).contains("/send_raw_transaction"));
    assert!(String::from_utf8_lossy(&requests[3]).contains("/get_blocks.bin"));
}

#[test]
fn snapshots_keep_i2p_sends_on_their_proxy_during_clearnet_scanning() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    let (proxy, proxy_task) = fixture(2, "200 OK", "", b"{}");
    let (direct, direct_task) = fixture(1, "200 OK", "", b"{}");
    configure_i2p_proxy(Some(&proxy)).unwrap();
    let transport = RpcTransport::for_url("http://snapshot.i2p").unwrap();
    let blocking = crate::BlockingRpcTransport::new("http://snapshot.i2p").unwrap();
    configure_i2p_proxy(None).unwrap();
    crate::TOKIO_RUNTIME.block_on(async {
        let direct = RpcTransport::for_url(&direct).unwrap();
        let (a, b) = futures::join!(
            transport.post("send_raw_transaction", vec![], Some(1024)),
            direct.post("get_height", vec![], Some(1024))
        );
        a.unwrap();
        b.unwrap();
    });
    blocking.post_bytes("get_height", vec![]).unwrap();
    assert_eq!(proxy_task.join().unwrap().len(), 2);
    assert_eq!(direct_task.join().unwrap().len(), 1);
    assert!(RpcTransport::for_url("http://snapshot.i2p").is_err());
}

#[test]
fn unavailable_proxy_never_falls_back_to_a_direct_destination() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = dead.local_addr().unwrap();
    drop(dead);
    configure_i2p_proxy(Some(&format!("http://{address}"))).unwrap();
    let transport = RpcTransport::for_url("http://no-fallback.i2p").unwrap();
    let result =
        crate::TOKIO_RUNTIME.block_on(transport.post("send_raw_transaction", vec![], Some(1024)));
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("no direct-network fallback"));
    assert!(crate::BlockingRpcTransport::new("http://no-fallback.i2p")
        .unwrap()
        .post_bytes("get_height", vec![])
        .is_err());
}

#[test]
fn ambient_proxy_and_no_proxy_variables_cannot_change_routes() {
    if std::env::var("WALLETCORE_ROUTE_CHILD").as_deref() != Ok("1") {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "support::rpc::routing_tests::ambient_proxy_and_no_proxy_variables_cannot_change_routes", "--nocapture"])
            .env("WALLETCORE_ROUTE_CHILD", "1")
            .env("HTTP_PROXY", "http://127.0.0.1:1").env("http_proxy", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1").env("all_proxy", "http://127.0.0.1:1")
            .env("NO_PROXY", "*").env("no_proxy", "*")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    // Child environment is isolated: no races with other process-wide test settings.
    snapshots_keep_i2p_sends_on_their_proxy_during_clearnet_scanning();
}

#[test]
fn redirects_cannot_escape_i2p_and_responses_are_bounded() {
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    let escaped = TcpListener::bind("127.0.0.1:0").unwrap();
    escaped.set_nonblocking(true).unwrap();
    let (proxy, task) = fixture(
        1,
        "302 Found",
        &format!(
            "Location: http://{}/escape\r\n",
            escaped.local_addr().unwrap()
        ),
        b"{}",
    );
    configure_i2p_proxy(Some(&proxy)).unwrap();
    let transport = RpcTransport::for_url("http://redirect.i2p").unwrap();
    assert!(crate::TOKIO_RUNTIME
        .block_on(transport.post("get_height", vec![], Some(1024)))
        .unwrap_err()
        .to_string()
        .contains("redirects are forbidden"));
    task.join().unwrap();
    assert_eq!(
        escaped.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    let (proxy, task) = fixture(1, "200 OK", "", b"123456789");
    configure_i2p_proxy(Some(&proxy)).unwrap();
    let transport = RpcTransport::for_url("http://bounded.i2p").unwrap();
    assert!(crate::TOKIO_RUNTIME
        .block_on(transport.post("get_height", vec![], Some(8)))
        .is_err());
    task.join().unwrap();
}

#[test]
#[ignore = "requires an explicitly configured live I2P HTTP proxy; read-only, never submits transactions"]
fn live_i2p_rpc_is_reachable_and_admin_methods_remain_private() {
    let proxy = std::env::var("WALLETCORE_TEST_I2P_PROXY").unwrap();
    let url = std::env::var("WALLETCORE_TEST_I2P_RPC").unwrap();
    let _lock = SERIAL.lock().unwrap();
    let _reset = Reset;
    configure_i2p_proxy(Some(&proxy)).unwrap();
    crate::TOKIO_RUNTIME.block_on(async {
        let daemon = RpcTransport::new(url).await.unwrap();
        let height = daemon.rpc_call("get_height", None, 4096).await.unwrap();
        let height: serde_json::Value = serde_json::from_str(&height).unwrap();
        assert_eq!(height["status"], "OK");
        // Read one scannable block: proves wallet data can travel over RPC/I2P,
        // independently of Cuprate's clearnet-only node block synchronization.
        use monero_interface::ProvidesScannableBlocks as _;
        daemon.scannable_block_by_number(0).await.unwrap();
        for method in ["get_transaction_counters", "get_connections"] {
            let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":method}).to_string();
            let result = daemon.rpc_call("json_rpc", Some(body), 4096).await.unwrap();
            let result: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(result["error"]["code"], -32601);
            assert!(result.get("result").is_none());
        }
        println!(
            "I2P RPC height={} and private methods blocked",
            height["height"]
        );
    });
}
