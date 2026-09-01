//! One port answering both protocols — over real sockets, with a real TLS
//! handshake.
//!
//! The unit tests decide what a ClientHello looks like. This decides whether
//! the server actually serves both, which is the whole claim: the FiveM client
//! needs plain HTTP on the game port, and the CFX server list queries it over
//! HTTPS.

use std::time::Duration;

use axum::routing::get;
use axum::Router;
use baston_gateway::http::multiplex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Start the multiplexer on an ephemeral port and return it.
async fn start(tls: bool) -> u16 {
    let router = Router::new().route("/info.json", get(|| async { "{\"name\":\"BASTON\"}" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = tls.then(|| multiplex::acceptor(None).expect("self-signed acceptor"));

    tokio::spawn(async move {
        let _ = multiplex::serve(listener, router, acceptor).await;
    });
    // Let the accept loop reach its first await.
    tokio::time::sleep(Duration::from_millis(100)).await;
    port
}

/// A plain HTTP/1.1 request, written by hand — the shape the FiveM client
/// sends to the game port.
async fn plain_request(port: u16) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(b"GET /info.json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn one_port_answers_plain_http_and_https() {
    let port = start(true).await;

    let plain = plain_request(port).await;
    assert!(plain.starts_with("HTTP/1.1 200"), "{plain}");
    assert!(plain.contains("BASTON"), "{plain}");

    // A real TLS client against the same port. The certificate is self-signed,
    // exactly as FXServer's is, so verification is off — which is what the CFX
    // checker must also do, since it reaches servers at arbitrary addresses.
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let response = client
        .get(format!("https://127.0.0.1:{port}/info.json"))
        .send()
        .await
        .expect("HTTPS on the game port must answer");

    assert_eq!(response.status(), 200);
    assert!(response.text().await.unwrap().contains("BASTON"));
}

/// Without a certificate the server is plain-only, and a TLS client gets a
/// closed connection rather than an HTTP response — which is the failure the
/// CFX ingress reported as "server gave HTTP response to HTTPS client".
#[tokio::test]
async fn without_a_certificate_a_tls_client_is_not_answered_in_plaintext() {
    let port = start(false).await;

    assert!(plain_request(port).await.starts_with("HTTP/1.1 200"));

    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let result = client
        .get(format!("https://127.0.0.1:{port}/info.json"))
        .send()
        .await;
    assert!(
        result.is_err(),
        "a plain-only server must refuse TLS, not answer it with HTTP"
    );
}

/// A connection that says nothing must not hold a task open forever: the port
/// is public, and an idle socket is otherwise free to keep.
#[tokio::test]
async fn a_silent_connection_does_not_wedge_the_listener() {
    let port = start(true).await;
    let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    // The next client is served while the first one is still saying nothing.
    let plain = plain_request(port).await;
    assert!(plain.starts_with("HTTP/1.1 200"), "{plain}");
}

/// The bytes are peeked, not consumed, so the HTTP parser still sees the
/// request line from its first character. A request split across two writes
/// is the case that would expose a consumed prefix.
#[tokio::test]
async fn the_sniffed_bytes_are_left_in_the_stream() {
    let port = start(true).await;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

    stream.write_all(b"GET /info").await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    stream
        .write_all(b".json HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("BASTON"), "{response}");
}
