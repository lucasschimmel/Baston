//! One port, two protocols: plain HTTP and TLS, told apart by the first bytes.
//!
//! The game port has to speak both, and not by choice.
//!
//! The FiveM client sends some of its game-port requests as plain HTTP, and
//! `getConfiguration` hands out literal `http://…/files` URLs, so a TLS-only
//! listener answers them with `Received HTTP/0.9 when not allowed` and the
//! server stops working. But the CFX server list queries a listed server over
//! **HTTPS** — a live run against the real ingress reported
//! `http: server gave HTTP response to HTTPS client` on `/info.json`,
//! `/dynamic.json` and `/players.json` in turn — so a plain-only listener
//! cannot be listed.
//!
//! FXServer solves it by multiplexing, and this is the same trick from the
//! same observation (`HttpServerManager.cpp`): a TLS record starts with a
//! handshake byte and a ClientHello, and no HTTP method does.
//!
//! ```text
//! 0x16 ?? ?? ?? ?? 0x01     TLS handshake, ClientHello  -> TLS
//! "GET /info.json HTTP…"    anything else               -> plain HTTP
//! ```
//!
//! The bytes are read with `peek`, so they stay in the socket buffer and
//! whichever side takes the connection sees the stream from its first byte.

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::Router;
use hyper::body::Incoming;
use hyper::Request;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

/// Bytes needed to recognise a TLS ClientHello: the record type, then the
/// handshake type at offset 5.
const SNIFF_BYTES: usize = 6;

/// How long a connection may stay silent before it is dropped.
///
/// A peer that connects and sends nothing would otherwise hold a task forever,
/// which is a free denial of service on a public port.
const SNIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// Does this look like the start of a TLS connection?
///
/// `0x16` is the handshake content type and `0x01` at offset 5 is
/// `client_hello`. The same two-byte test FXServer uses, and it cannot collide
/// with HTTP: every method starts with an uppercase ASCII letter.
fn looks_like_tls(prefix: &[u8]) -> bool {
    prefix.len() >= SNIFF_BYTES && prefix[0] == 0x16 && prefix[5] == 1
}

/// A connection that may or may not have TLS under it.
///
/// An enum rather than a boxed trait object: there are exactly two cases, they
/// are known at accept time, and the read path is hot enough that a vtable per
/// poll would be a strange thing to pay for.
pub enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_flush(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Self::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Peek at the opening bytes without consuming them.
///
/// `peek` leaves the data in the socket buffer, which is the whole point: the
/// TLS handshake and the HTTP parser each need to see the stream from byte
/// zero, and neither would tolerate a prefix having been eaten.
async fn sniff(stream: &TcpStream) -> io::Result<bool> {
    let mut prefix = [0u8; SNIFF_BYTES];
    let deadline = tokio::time::Instant::now() + SNIFF_TIMEOUT;

    loop {
        // A short peek means the peer has not sent enough yet, not that it
        // never will — TLS records and HTTP request lines both arrive in
        // whatever pieces the network chose.
        let read = tokio::time::timeout_at(deadline, stream.peek(&mut prefix)).await??;
        if read >= SNIFF_BYTES {
            return Ok(looks_like_tls(&prefix));
        }
        if read == 0 {
            // Peer closed. Not TLS, and the connection is about to end anyway.
            return Ok(false);
        }
        stream.readable().await?;
    }
}

/// Serve `router` on `listener`, accepting plain HTTP and, when `tls` is
/// present, TLS on the same port.
///
/// Never returns while the listener lives. An error on one connection is that
/// connection's problem: it is logged and the loop continues, because a
/// malformed handshake from anywhere on the internet must not stop a server.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Option<TlsAcceptor>,
) -> io::Result<()> {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Per-connection errors (a peer that vanished between the SYN and
            // the accept) are not reasons to stop listening.
            Err(e) if is_connection_error(&e) => continue,
            Err(e) => return Err(e),
        };

        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            if let Err(e) = serve_connection(stream, peer, router, tls).await {
                tracing::debug!(target: "http", %peer, error = %e, "connection closed early");
            }
        });
    }
}

async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    router: Router,
    tls: Option<TlsAcceptor>,
) -> io::Result<()> {
    let io = match (sniff(&stream).await?, tls) {
        (true, Some(acceptor)) => MaybeTls::Tls(Box::new(acceptor.accept(stream).await?)),
        // A TLS hello with no acceptor configured: nothing useful can be said
        // back, and answering HTTP to it produces the exact "server gave HTTP
        // response to HTTPS client" that this module exists to stop.
        (true, None) => {
            tracing::debug!(target: "http", %peer, "TLS connection refused: no certificate");
            return Ok(());
        }
        (false, _) => MaybeTls::Plain(stream),
    };

    // What axum's own `serve` does per connection: hand the router the request
    // with the peer address attached, so a resource's HTTP handler sees
    // `request.address` the way FXServer reports it.
    let service = router
        .into_service::<Body>()
        .map_request(move |mut req: Request<Incoming>| {
            req.extensions_mut().insert(ConnectInfo(peer));
            req.map(Body::new)
        });

    Builder::new(TokioExecutor::new())
        .serve_connection_with_upgrades(TokioIo::new(io), TowerToHyperService::new(service))
        .await
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Whether an accept error concerns one connection rather than the listener.
fn is_connection_error(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
    )
}

/// A TLS acceptor for the game port.
///
/// With no `[tls]` section, a self-signed certificate is generated at boot.
/// That is what FXServer does (`server-tls.crt` is generated, not supplied),
/// and it is enough for the only thing that speaks TLS here: the CFX server
/// list, which reaches servers at arbitrary IPs and cannot be checking names
/// it could never have a certificate for.
pub fn acceptor(tls: Option<&baston_config::TlsConfig>) -> Result<TlsAcceptor, String> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let (certs, key) = match tls {
        Some(config) => load_pem(config)?,
        None => generate_self_signed()?,
    };

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| format!("TLS certificate rejected: {e}"))?;
    Ok(TlsAcceptor::from(Arc::new(server_config)))
}

type CertAndKey = (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
);

fn generate_self_signed() -> Result<CertAndKey, String> {
    let cert = rcgen::generate_simple_self_signed(vec!["baston".to_owned()])
        .map_err(|e| format!("could not generate a TLS certificate: {e}"))?;
    let key = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| format!("generated TLS key is unusable: {e}"))?;
    Ok((
        vec![rustls::pki_types::CertificateDer::from(cert.cert)],
        key,
    ))
}

fn load_pem(config: &baston_config::TlsConfig) -> Result<CertAndKey, String> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    };
    let certs = rustls_pemfile::certs(&mut read(&config.cert_pem)?.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", config.cert_pem.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "{} contains no certificate",
            config.cert_pem.display()
        ));
    }
    let key = rustls_pemfile::private_key(&mut read(&config.key_pem)?.as_slice())
        .map_err(|e| format!("{}: {e}", config.key_pem.display()))?
        .ok_or_else(|| format!("{} contains no private key", config.key_pem.display()))?;
    Ok((certs, key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact test FXServer makes: content type 0x16, handshake type 1.
    #[test]
    fn a_client_hello_is_recognised() {
        // TLS 1.2 record header + handshake type.
        assert!(looks_like_tls(&[0x16, 0x03, 0x01, 0x00, 0xa5, 0x01]));
        // TLS 1.0 hello, as an older client would send.
        assert!(looks_like_tls(&[0x16, 0x03, 0x00, 0x00, 0x2f, 0x01]));
    }

    /// Every HTTP method starts with an uppercase letter, so none of them can
    /// be mistaken for a handshake byte.
    #[test]
    fn http_requests_are_never_mistaken_for_tls() {
        for line in [
            "GET /info.json HTTP/1.1\r\n",
            "POST /client HTTP/1.1\r\n",
            "HEAD /files/x HTTP/1.1\r\n",
            "OPTIONS * HTTP/1.1\r\n",
        ] {
            assert!(!looks_like_tls(line.as_bytes()), "{line}");
        }
    }

    /// A handshake byte alone is not enough: the sixth byte decides, and
    /// deciding early on a short read would misroute the connection.
    #[test]
    fn a_short_prefix_is_never_enough_to_decide() {
        assert!(!looks_like_tls(&[]));
        assert!(!looks_like_tls(&[0x16]));
        assert!(!looks_like_tls(&[0x16, 0x03, 0x01, 0x00, 0xa5]));
    }

    /// 0x16 with something other than a ClientHello after it — a stray record
    /// or a binary protocol that happens to start the same way.
    #[test]
    fn a_handshake_byte_without_a_client_hello_is_not_tls() {
        assert!(!looks_like_tls(&[0x16, 0x03, 0x01, 0x00, 0xa5, 0x02]));
    }

    #[test]
    fn a_self_signed_acceptor_is_built_when_no_certificate_is_configured() {
        acceptor(None).expect("a boot-time certificate must always be available");
    }
}
