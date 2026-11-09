//! TLS for tokio-postgres query sessions, with the same `sslmode` semantics as sqlx.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::config::SslMode as PgSslMode;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::error::{AppError, AppResult};
use crate::model::SslMode;

/// tokio-postgres only knows disable/prefer/require; certificate checks for
/// verify-ca and verify-full live in the rustls verifier.
pub fn pg_ssl_mode(mode: &SslMode) -> PgSslMode {
    match mode {
        SslMode::Disable => PgSslMode::Disable,
        SslMode::Prefer => PgSslMode::Prefer,
        _ => PgSslMode::Require,
    }
}

/// Builds the rustls connector for `mode`. As in sqlx, `root_cert` (a PEM file
/// added to the native roots) is read only by verify-ca and verify-full; the
/// other modes never verify, so a stale path does not break them.
pub fn tls_connector(mode: &SslMode, root_cert: Option<&str>) -> AppResult<MakeRustlsConnect> {
    let provider = Arc::new(ring::default_provider());
    let root_cert = root_cert.filter(|p| !p.is_empty());
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        SslMode::Disable | SslMode::Prefer | SslMode::Require => Arc::new(NoVerify(provider.clone())),
        SslMode::VerifyCa => Arc::new(IgnoreHostname(webpki_verifier(root_cert, &provider)?)),
        SslMode::VerifyFull => webpki_verifier(root_cert, &provider)?,
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| AppError::Connection(format!("TLS setup failed: {e}")))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

fn read_ca_file(path: &str) -> AppResult<Vec<CertificateDer<'static>>> {
    let ca_error = |err: &dyn std::fmt::Display| AppError::Connection(format!("Could not read CA file {path}: {err}"));
    let pem = std::fs::read(path).map_err(|e| ca_error(&e))?;
    let certs = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| ca_error(&e))?;
    if certs.is_empty() {
        return Err(ca_error(&"no certificates found"));
    }
    Ok(certs)
}

/// The platform's trusted roots plus the certificates in `root_cert`.
fn webpki_verifier(root_cert: Option<&str>, provider: &Arc<CryptoProvider>) -> AppResult<Arc<WebPkiServerVerifier>> {
    let extra = match root_cert {
        Some(path) => read_ca_file(path)?,
        None => Vec::new(),
    };
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    for cert in extra {
        roots
            .add(cert)
            .map_err(|e| AppError::Connection(format!("Invalid CA certificate: {e}")))?;
    }
    WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .map_err(|e| AppError::Connection(format!("TLS setup failed: {e}")))
}

/// `prefer` / `require`: any certificate is accepted, but handshake signatures are
/// still checked, so the session is encrypted to whoever holds the key.
#[derive(Debug)]
struct NoVerify(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// `verify-ca`: the chain must verify; a hostname mismatch is accepted.
#[derive(Debug)]
struct IgnoreHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for IgnoreHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // WebPki checks the chain before the name, so a name error means the chain is valid.
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connector_builds_for_every_mode() {
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert!(tls_connector(&mode, None).is_ok(), "{mode:?}");
        }
    }

    #[test]
    fn missing_ca_file_is_connection_error() {
        let path = "/nonexistent/porthole-ca.pem";
        match tls_connector(&SslMode::VerifyFull, Some(path)) {
            Err(AppError::Connection(m)) => {
                assert!(m.starts_with(&format!("Could not read CA file {path}: ")), "{m}")
            }
            Err(other) => panic!("expected Connection, got {other:?}"),
            Ok(_) => panic!("verify-full built with a missing CA file"),
        }
    }

    #[test]
    fn non_verifying_modes_ignore_ca_file() {
        // As in sqlx: only verify-ca and verify-full read the CA file.
        let path = "/nonexistent/porthole-ca.pem";
        for mode in [SslMode::Disable, SslMode::Prefer, SslMode::Require] {
            assert!(tls_connector(&mode, Some(path)).is_ok(), "{mode:?}");
        }
    }

    #[test]
    fn ssl_modes_map_to_tokio_postgres() {
        assert_eq!(pg_ssl_mode(&SslMode::Disable), PgSslMode::Disable);
        assert_eq!(pg_ssl_mode(&SslMode::Prefer), PgSslMode::Prefer);
        for mode in [SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull] {
            assert_eq!(pg_ssl_mode(&mode), PgSslMode::Require);
        }
    }
}
