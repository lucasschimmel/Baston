//! Live probe: logs into a running baston-voice server as a FiveM-style
//! Mumble client and reports the handshake.
//!
//! ```sh
//! cargo run -p baston-voice --example probe -- 127.0.0.1:30121 7
//! ```

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

use baston_voice::framing::{self, MessageType, PREAMBLE_SIZE};
use baston_voice::proto;

#[derive(Debug)]
struct NoVerify(rustls::crypto::WebPkiSupportedAlgorithms);

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.supported_schemes()
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let addr = args.next().unwrap_or_else(|| "127.0.0.1:30121".to_owned());
    let netid: u32 = args.next().as_deref().unwrap_or("7").parse()?;

    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let provider = rustls::crypto::CryptoProvider::get_default().expect("provider");
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify(
            provider.signature_verification_algorithms,
        )))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));

    let tcp = tokio::net::TcpStream::connect(&addr).await?;
    let mut stream = connector
        .connect("baston-voice".try_into().unwrap(), tcp)
        .await?;
    println!("[probe] TLS established with {addr}");

    let version = framing::encode(
        MessageType::Version,
        &proto::Version {
            version: Some(baston_voice::PROTOCOL_VERSION),
            release: Some("baston-probe".into()),
            os: None,
            os_version: None,
        },
    )?;
    stream.write_all(&version).await?;
    let auth = framing::encode(
        MessageType::Authenticate,
        &proto::Authenticate {
            username: Some(format!("[{netid}]")),
            password: None,
            tokens: vec![],
            celt_versions: vec![],
            opus: Some(true),
        },
    )?;
    stream.write_all(&auth).await?;

    let deadline = std::time::Duration::from_secs(5);
    loop {
        let mut pre = [0u8; PREAMBLE_SIZE];
        tokio::time::timeout(deadline, stream.read_exact(&mut pre)).await??;
        let p = framing::Preamble::parse(&pre).expect("preamble");
        let mut payload = vec![0u8; p.len as usize];
        tokio::time::timeout(deadline, stream.read_exact(&mut payload)).await??;
        let Some(ty) = MessageType::from_u16(p.ty) else {
            continue;
        };
        match ty {
            MessageType::CryptSetup => {
                let cs: proto::CryptSetup = framing::decode(ty, &payload)?;
                println!(
                    "[probe] CryptSetup: key {}B, nonces {}B/{}B",
                    cs.key.map_or(0, |k| k.len()),
                    cs.client_nonce.map_or(0, |n| n.len()),
                    cs.server_nonce.map_or(0, |n| n.len()),
                );
            }
            MessageType::ChannelState => {
                let ch: proto::ChannelState = framing::decode(ty, &payload)?;
                println!(
                    "[probe] ChannelState: id={:?} name={:?}",
                    ch.channel_id, ch.name
                );
            }
            MessageType::UserState => {
                let us: proto::UserState = framing::decode(ty, &payload)?;
                println!(
                    "[probe] UserState: session={:?} name={:?} channel={:?}",
                    us.session, us.name, us.channel_id
                );
            }
            MessageType::ServerSync => {
                let sync: proto::ServerSync = framing::decode(ty, &payload)?;
                println!(
                    "[probe] ServerSync: session={:?} bandwidth={:?} welcome={:?}",
                    sync.session, sync.max_bandwidth, sync.welcome_text
                );
                assert_eq!(sync.session, Some(netid), "session must equal netId");
                println!("[probe] OK — full Mumble login as [{netid}] succeeded");
                return Ok(());
            }
            other => println!("[probe] {other:?} ({} bytes)", payload.len()),
        }
    }
}
