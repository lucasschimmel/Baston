//! End-to-end voice server tests: real TLS control handshake, tunneled voice
//! delivery, and the encrypted UDP path with a client-side `CryptState`.

use std::sync::Arc;

use prost::Message;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::client::TlsStream;
use tokio_rustls::TlsConnector;

use baston_voice::crypto::CryptState;
use baston_voice::framing::{self, MessageType, PREAMBLE_SIZE};
use baston_voice::pds::write_u64;
use baston_voice::proto;
use baston_voice::server::{spawn, VoiceServerConfig};
use baston_voice::{VoicePacketType, VoiceTargetKind};

/// A rustls verifier that accepts anything — the FiveM client doesn't
/// validate the voice certificate either.
#[derive(Debug)]
struct NoVerify(rustls::crypto::WebPkiSupportedAlgorithms);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_schemes()
    }
}

type Stream = TlsStream<tokio::net::TcpStream>;

async fn tls_connect(port: u16) -> Stream {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let provider = rustls::crypto::CryptoProvider::get_default().expect("provider installed");
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(
            provider.signature_verification_algorithms,
        )))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("tcp connect");
    connector
        .connect("baston-voice".try_into().unwrap(), tcp)
        .await
        .expect("tls connect")
}

async fn send_frame<M: Message>(stream: &mut Stream, ty: MessageType, msg: &M) {
    let frame = framing::encode(ty, msg).expect("encode");
    stream.write_all(&frame).await.expect("write");
}

async fn read_frame(stream: &mut Stream) -> (u16, Vec<u8>) {
    let mut pre = [0u8; PREAMBLE_SIZE];
    stream.read_exact(&mut pre).await.expect("preamble");
    let p = framing::Preamble::parse(&pre).expect("parse preamble");
    let mut payload = vec![0u8; p.len as usize];
    stream.read_exact(&mut payload).await.expect("payload");
    (p.ty, payload)
}

/// Read frames until `ty` shows up (skipping the others). Panics after a cap.
async fn read_until(stream: &mut Stream, ty: MessageType) -> Vec<u8> {
    for _ in 0..64 {
        let (t, payload) = read_frame(stream).await;
        if t == ty.tag() {
            return payload;
        }
    }
    panic!("never received {ty:?}");
}

/// Run the full login: Version + Authenticate, then collect CryptSetup and
/// wait for ServerSync. Returns the stream and the client-side crypt state.
async fn login(port: u16, netid: u32) -> (Stream, CryptState) {
    let mut stream = tls_connect(port).await;
    send_frame(
        &mut stream,
        MessageType::Version,
        &proto::Version {
            version: Some(baston_voice::PROTOCOL_VERSION),
            release: Some("test-client".into()),
            os: None,
            os_version: None,
        },
    )
    .await;
    send_frame(
        &mut stream,
        MessageType::Authenticate,
        &proto::Authenticate {
            username: Some(format!("[{netid}]")),
            password: None,
            tokens: vec![],
            celt_versions: vec![],
            opus: Some(true),
        },
    )
    .await;

    let cs: proto::CryptSetup = framing::decode(
        MessageType::CryptSetup,
        &read_until(&mut stream, MessageType::CryptSetup).await,
    )
    .expect("cryptsetup");
    let key: [u8; 16] = cs.key.expect("key").try_into().expect("16-byte key");
    let client_nonce: [u8; 16] = cs
        .client_nonce
        .expect("client nonce")
        .try_into()
        .expect("16-byte nonce");
    let server_nonce: [u8; 16] = cs
        .server_nonce
        .expect("server nonce")
        .try_into()
        .expect("16-byte nonce");
    // Client side: encrypt with the client nonce, decrypt with the server's.
    let crypt = CryptState::new(&key, &client_nonce, &server_nonce);

    let sync: proto::ServerSync = framing::decode(
        MessageType::ServerSync,
        &read_until(&mut stream, MessageType::ServerSync).await,
    )
    .expect("serversync");
    assert_eq!(sync.session, Some(netid), "session id == netId");

    (stream, crypt)
}

/// A minimal Opus voice datagram (plaintext form): header byte, sequence
/// varint, one frame.
fn opus_datagram(audio: &[u8]) -> Vec<u8> {
    let mut p = Vec::new();
    p.push((VoicePacketType::Opus as u8) << 5 | VoiceTargetKind::Normal.to_bits());
    write_u64(&mut p, 1); // sequence
    write_u64(&mut p, audio.len() as u64);
    p.extend_from_slice(audio);
    p
}

#[tokio::test]
async fn two_clients_handshake_and_tunneled_voice_is_routed() {
    let handle = spawn(VoiceServerConfig {
        bind: "127.0.0.1".parse().unwrap(),
        port: 0,
    })
    .await
    .expect("spawn");

    let (mut c1, _crypt1) = login(handle.port(), 1).await;
    let (mut c2, _crypt2) = login(handle.port(), 2).await;

    // Client 1 speaks through the TCP tunnel (UDP "blocked"); client 2 has no
    // UDP flow either, so delivery must come back as a UDPTunnel frame.
    let datagram = opus_datagram(&[0xAB; 20]);
    send_frame(
        &mut c1,
        MessageType::UdpTunnel,
        &proto::UdpTunnel {
            packet: datagram.clone(),
        },
    )
    .await;

    let tunneled: proto::UdpTunnel = framing::decode(
        MessageType::UdpTunnel,
        &tokio::time::timeout(
            std::time::Duration::from_secs(5),
            read_until(&mut c2, MessageType::UdpTunnel),
        )
        .await
        .expect("voice delivery timed out"),
    )
    .expect("tunnel decode");

    // The delivered packet carries the speaker session (1) prepended.
    let parsed = baston_voice::voice::parse(&tunneled.packet).expect("outbound parses");
    assert_eq!(parsed.codec, VoicePacketType::Opus);
    let mut r = baston_voice::pds::PdsReader::new(&tunneled.packet[1..]);
    assert_eq!(r.read_u64(), Some(1), "speaker session prepended");
}

#[tokio::test]
async fn udp_voice_roundtrips_through_ocb2() {
    let handle = spawn(VoiceServerConfig {
        bind: "127.0.0.1".parse().unwrap(),
        port: 0,
    })
    .await
    .expect("spawn");

    let (_c1, mut crypt1) = login(handle.port(), 1).await;
    let (_c2, mut crypt2) = login(handle.port(), 2).await;

    let sock1 = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let sock2 = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind");
    sock1
        .connect(("127.0.0.1", handle.port()))
        .await
        .expect("connect");
    sock2
        .connect(("127.0.0.1", handle.port()))
        .await
        .expect("connect");

    // Bind client 2's UDP flow with an encrypted ping (type=Ping, seq varint).
    let mut ping = Vec::new();
    ping.push((VoicePacketType::Ping as u8) << 5);
    write_u64(&mut ping, 99);
    sock2.send(&crypt2.encrypt(&ping)).await.expect("send ping");
    let mut buf = [0u8; 1024];
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock2.recv(&mut buf))
        .await
        .expect("ping echo timed out")
        .expect("recv");
    let echoed = crypt2.decrypt(&buf[..n]).expect("echo decrypts");
    assert_eq!(echoed, ping, "encrypted ping echoes verbatim");

    // Client 1 speaks over UDP; client 2 must receive an encrypted datagram.
    let datagram = opus_datagram(&[0x5A; 32]);
    sock1
        .send(&crypt1.encrypt(&datagram))
        .await
        .expect("send voice");
    let n = tokio::time::timeout(std::time::Duration::from_secs(5), sock2.recv(&mut buf))
        .await
        .expect("voice delivery timed out")
        .expect("recv");
    let plain = crypt2.decrypt(&buf[..n]).expect("voice decrypts");
    let mut r = baston_voice::pds::PdsReader::new(&plain[1..]);
    assert_eq!(r.read_u64(), Some(1), "speaker session is client 1");
    let parsed = baston_voice::voice::parse(&plain).expect("outbound voice parses");
    assert_eq!(parsed.codec, VoicePacketType::Opus);
}
