//! TLS for the Postgres connection.
//!
//! The Rust driver ships no TLS of its own: `Client::connect` takes a connector
//! as an argument, and the `NoTls` dbdelve passed until now is a real type whose
//! whole behaviour is to refuse. That is why `sslmode` had to be rejected
//! rather than honoured — see the note on [`SslMode::parse`] for what a
//! connection string's `sslmode` actually buys you here.
//!
//! `rustls` rather than `native-tls`, because `rustls` and `rustls-native-certs`
//! were already in the dependency graph through gpui, and the latter reads the
//! platform trust store through `security-framework`, which dbdelve already
//! depends on for the Keychain. The choice costs one adapter crate instead of a
//! second TLS stack, and no OpenSSL on any platform.
//!
//! The driver's own `SslMode` has three rungs — disable, prefer, require — and
//! leaves *verification* entirely to the connector. So libpq's top two rungs are
//! `require` on the wire plus a stricter verifier in here, and the inversion
//! worth remembering is that `verify-full` is the cheap one: `rustls` checks the
//! chain and the hostname by default. It is `require` and `verify-ca`, the modes
//! that encrypt without fully establishing who answered, that need code written
//! for them.

use std::sync::Arc;

use rustls::{
    ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    client::verify_server_cert_signed_by_trust_anchor,
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::ParsedCertificate,
};
pub use tokio_postgres_rustls::MakeRustlsConnect;

/// What a connection asks of TLS, in libpq's vocabulary because that is the
/// vocabulary every connection string is written in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    /// libpq's default, and so dbdelve's: encrypt when the server offers it, and
    /// carry on in the clear when it does not.
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

impl SslMode {
    /// Weakest first, which is the order the form draws them in.
    pub const ALL: [Self; 5] = [
        Self::Disable,
        Self::Prefer,
        Self::Require,
        Self::VerifyCa,
        Self::VerifyFull,
    ];

    /// The spelling libpq uses, which is the spelling that goes back into a URL
    /// and into the profile on disk.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyCa => "verify-ca",
            Self::VerifyFull => "verify-full",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Disable => "Disable",
            Self::Prefer => "Prefer",
            Self::Require => "Require",
            Self::VerifyCa => "Verify CA",
            Self::VerifyFull => "Verify Full",
        }
    }

    /// What the mode actually promises, for the line under the selector. A
    /// person choosing between five words deserves to know that two of them
    /// encrypt without checking who answered.
    pub fn explanation(self) -> &'static str {
        match self {
            Self::Disable => "Never encrypted.",
            Self::Prefer => "Encrypted when the server offers it. The certificate is not checked.",
            Self::Require => "Always encrypted. The certificate is not checked.",
            Self::VerifyCa => {
                "The certificate must be signed by a trusted authority. Its host name is not checked."
            }
            Self::VerifyFull => {
                "The certificate must be signed by a trusted authority and issued for this host."
            }
        }
    }

    /// An empty value is the default rather than an error: that is what a URL
    /// with no `sslmode` at all means, and both spellings should land in the
    /// same place.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" => Ok(Self::default()),
            "disable" => Ok(Self::Disable),
            "prefer" => Ok(Self::Prefer),
            "require" => Ok(Self::Require),
            "verify-ca" => Ok(Self::VerifyCa),
            "verify-full" => Ok(Self::VerifyFull),
            // libpq tries plaintext first and TLS only if that fails. The
            // driver has no way to express that order, and treating it as
            // `prefer` would reverse it — quietly preferring the opposite
            // thing to the one that was asked for.
            "allow" => Err(
                "sslmode=allow is not supported: the driver cannot try plaintext before TLS."
                    .to_string(),
            ),
            other => Err(format!("sslmode={other} is not an SSL mode.")),
        }
    }

    /// What goes in the connection string. The driver decides only whether to
    /// negotiate and whether a refusal is fatal; everything above that is
    /// decided by the verifier this module builds.
    pub fn driver_mode(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require | Self::VerifyCa | Self::VerifyFull => "require",
        }
    }

    /// Whether a root certificate is consulted at all, and so whether naming
    /// one can change the outcome of a connection.
    pub fn checks_certificate(self) -> bool {
        matches!(self, Self::VerifyCa | Self::VerifyFull)
    }
}

/// The connector for a mode, or `None` for `disable`, which never negotiates.
///
/// Built per connection rather than once: the roots depend on the profile's own
/// certificate file, and a profile pointing at a private CA must not be able to
/// widen what another profile trusts.
pub fn connector(
    mode: SslMode,
    root_certificate: Option<&str>,
) -> Result<Option<MakeRustlsConnect>, String> {
    if mode == SslMode::Disable {
        return Ok(None);
    }

    // `ring`, matching the provider already in the tree. rustls defaults to
    // `aws-lc-rs`, which would drag in a second crypto library with a C and
    // assembly build — see the pin in Cargo.toml.
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let versions = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("Could not configure TLS: {error}"))?;

    let config = match mode {
        // The one mode that needs nothing written for it, and deliberately the
        // only one that does not go through `dangerous()`.
        SslMode::VerifyFull => versions
            .with_root_certificates(roots(root_certificate)?)
            .with_no_client_auth(),
        SslMode::VerifyCa => versions
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TrustChainOnly {
                roots: roots(root_certificate)?,
                provider,
            }))
            .with_no_client_auth(),
        // libpq's `prefer` and `require` encrypt without asking who answered.
        SslMode::Prefer | SslMode::Require => versions
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(TrustAnyServer { provider }))
            .with_no_client_auth(),
        SslMode::Disable => unreachable!("returned above"),
    };

    Ok(Some(MakeRustlsConnect::new(config)))
}

/// The roots a certificate is checked against.
///
/// A named file *replaces* the platform's trust store rather than adding to it,
/// which is what libpq's `sslrootcert` does and the only reading that makes a
/// cloud provider's bundle mean anything: the point of pinning RDS's authority
/// is that nothing else is accepted.
fn roots(root_certificate: Option<&str>) -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();

    let Some(path) = root_certificate else {
        let loaded = rustls_native_certs::load_native_certs();
        for certificate in loaded.certs {
            // A single unparsable certificate in the platform store is not a
            // reason to refuse every connection; an empty store is.
            let _ = store.add(certificate);
        }
        if store.is_empty() {
            return Err(match loaded.errors.first() {
                Some(error) => format!("Could not read the system certificates: {error}"),
                None => "The system holds no certificate authorities.".to_string(),
            });
        }
        return Ok(store);
    };

    let file = std::fs::File::open(crate::store::home_expanded(path))
        .map_err(|error| format!("Could not read {path}: {error}"))?;
    let mut reader = std::io::BufReader::new(file);
    for certificate in rustls_pemfile::certs(&mut reader) {
        let certificate = certificate.map_err(|error| format!("Could not read {path}: {error}"))?;
        store
            .add(certificate)
            .map_err(|error| format!("{path} does not hold a usable certificate: {error}"))?;
    }
    if store.is_empty() {
        return Err(format!("{path} holds no certificates."));
    }
    Ok(store)
}

/// libpq's `prefer` and `require`: encryption, and no question about who is on
/// the other end.
///
/// Every certificate check here is deliberately a no-op. The handshake
/// signature is still verified, because that is what makes the key exchange
/// work at all — it says the peer holds the key in the certificate it sent, not
/// that the certificate is anybody in particular.
#[derive(Debug)]
struct TrustAnyServer {
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for TrustAnyServer {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            certificate,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            certificate,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// libpq's `verify-ca`: the chain must reach a root dbdelve trusts, but the name
/// on the certificate is not compared to the host that was dialled.
///
/// Useful where one authority issues for a fleet and the host names are not
/// stable — and weaker than it looks, because anything that authority ever
/// signed will pass. `verify-full` is the one that says *this* server.
#[derive(Debug)]
struct TrustChainOnly {
    roots: RootCertStore,
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for TrustChainOnly {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let certificate = ParsedCertificate::try_from(end_entity)?;
        // The whole difference from `verify-full` is the call that is not here:
        // `rustls::client::verify_server_name`.
        verify_server_cert_signed_by_trust_anchor(
            &certificate,
            &self.roots,
            intermediates,
            now,
            self.provider.signature_verification_algorithms.all,
        )?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            certificate,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            certificate,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_mode_round_trips_through_the_spelling_it_is_stored_as() {
        for mode in SslMode::ALL {
            assert_eq!(SslMode::parse(mode.as_str()), Ok(mode), "{mode:?}");
        }
    }

    #[test]
    fn an_absent_sslmode_is_the_libpq_default_rather_than_an_error() {
        assert_eq!(SslMode::parse(""), Ok(SslMode::Prefer));
        assert_eq!(SslMode::parse("  "), Ok(SslMode::Prefer));
        assert_eq!(SslMode::parse("VERIFY-FULL"), Ok(SslMode::VerifyFull));
    }

    #[test]
    fn a_mode_dbdelve_cannot_honour_is_refused_by_name() {
        // Never silently downgraded to `prefer`: `allow` asks for plaintext
        // first, which is the opposite order.
        assert!(SslMode::parse("allow").is_err());
        assert!(SslMode::parse("verify").is_err());
    }

    #[test]
    fn the_driver_only_ever_sees_the_three_rungs_it_has() {
        // Everything above `require` is this module's job. Handing the driver
        // "verify-full" would fail its own parser as an invalid connection
        // string, naming neither the option nor the reason.
        for mode in SslMode::ALL {
            assert!(
                ["disable", "prefer", "require"].contains(&mode.driver_mode()),
                "{mode:?} sends {}",
                mode.driver_mode()
            );
        }
        assert_eq!(SslMode::VerifyFull.driver_mode(), "require");
        assert_eq!(SslMode::VerifyCa.driver_mode(), "require");
    }

    #[test]
    fn only_the_verifying_modes_consult_a_root_certificate() {
        assert!(!SslMode::Disable.checks_certificate());
        assert!(!SslMode::Prefer.checks_certificate());
        assert!(!SslMode::Require.checks_certificate());
        assert!(SslMode::VerifyCa.checks_certificate());
        assert!(SslMode::VerifyFull.checks_certificate());
    }

    #[test]
    fn disable_builds_no_connector_and_every_other_mode_builds_one() {
        assert!(connector(SslMode::Disable, None).unwrap().is_none());
        // These two never read a trust store, so they cannot fail on a machine
        // that has none.
        assert!(connector(SslMode::Prefer, None).unwrap().is_some());
        assert!(connector(SslMode::Require, None).unwrap().is_some());
    }

    #[test]
    fn a_named_root_certificate_that_is_not_there_is_an_error_not_a_fallback() {
        // Falling back to the platform store would trust a wider set than the
        // profile asked for, which is the one thing pinning a CA is meant to
        // prevent.
        let missing = connector(SslMode::VerifyFull, Some("/nonexistent/ca.pem"));
        assert!(missing.is_err());
        assert!(connector(SslMode::VerifyCa, Some("/nonexistent/ca.pem")).is_err());
    }
}
