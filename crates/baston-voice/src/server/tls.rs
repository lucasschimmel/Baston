//! TLS acceptor for the Mumble control channel.
//!
//! The stock FiveM client does not validate the server certificate (Mumble's
//! model is trust-on-first-use, and FiveM skips even that), so a fresh
//! self-signed certificate per boot is exactly what FXServer's embedded
//! umurmur does too. Nothing is persisted.

use std::sync::Arc;

use tokio_rustls::TlsAcceptor;

/// Build a TLS acceptor around a boot-time self-signed certificate.
pub fn make_acceptor() -> Result<TlsAcceptor, Box<dyn std::error::Error + Send + Sync>> {
    // The gateway installs the process-default provider at boot; make this
    // self-sufficient for tests and tolerate the double install.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cert = rcgen::generate_simple_self_signed(vec!["baston-voice".to_owned()])?;
    let cert_der = rustls::pki_types::CertificateDer::from(cert.cert);
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
        .map_err(|e| format!("voice key encoding: {e}"))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}

/// Fill `buf` with cryptographically secure random bytes via the installed
/// rustls provider (no extra RNG dependency).
pub fn secure_random(buf: &mut [u8]) -> Result<(), rustls::crypto::GetRandomFailed> {
    let provider =
        rustls::crypto::CryptoProvider::get_default().expect("provider installed in make_acceptor");
    provider.secure_random.fill(buf)
}

#[cfg(test)]
mod tests {
    #[test]
    fn acceptor_builds_and_random_fills() {
        super::make_acceptor().expect("acceptor");
        let mut a = [0u8; 16];
        let mut b = [0u8; 16];
        super::secure_random(&mut a).unwrap();
        super::secure_random(&mut b).unwrap();
        assert_ne!(a, b, "two random draws must differ");
    }
}
