use crate::protocol::{ControlMessage, PairRequestPayload, SessionAgreement};
use crate::sync::WorkspaceSummary;
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hmac::{Hmac, Mac};
use rand::RngExt;
use rcgen::generate_simple_self_signed;
use ring::pbkdf2;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, ServerConfig, SignatureScheme};
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio_rustls::{TlsAcceptor, TlsConnector};

type HmacSha256 = Hmac<sha2::Sha256>;

const PBKDF2_ITERATIONS: u32 = 100_000;

#[derive(Debug)]
struct AcceptAnyServerVerifier {
    supported_schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for AcceptAnyServerVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported_schemes.clone()
    }
}

pub fn build_server_acceptor() -> Result<TlsAcceptor> {
    let certified =
        generate_simple_self_signed(vec!["synly.local".to_string(), "localhost".to_string()])?;
    let certs = vec![certified.cert.der().clone()];
    let key = PrivateKeyDer::from(certified.signing_key);

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)?;
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub fn build_client_connector() -> Result<TlsConnector> {
    let verifier = AcceptAnyServerVerifier {
        supported_schemes: rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes(),
    };
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsConnector::from(Arc::new(config)))
}

pub fn server_name() -> Result<ServerName<'static>> {
    Ok(ServerName::try_from("synly.local")?.to_owned())
}

pub fn export_keying_material_from_client<T>(
    stream: &tokio_rustls::client::TlsStream<T>,
    binding_id: &str,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        b"synly-pin-binding",
        Some(binding_id.as_bytes()),
    )?;
    Ok(output)
}

pub fn export_keying_material_from_server<T>(
    stream: &tokio_rustls::server::TlsStream<T>,
    binding_id: &str,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        b"synly-pin-binding",
        Some(binding_id.as_bytes()),
    )?;
    Ok(output)
}

pub fn random_pin() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000))
}

pub fn sign_pair_auth(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    payload: &PairRequestPayload,
) -> Result<String> {
    let payload_bytes = serde_json::to_vec(payload)?;
    Ok(sign_payload(
        exporter,
        request_id,
        pin,
        b"synly-client-proof",
        &payload_bytes,
    ))
}

pub fn verify_pair_auth(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    payload: &PairRequestPayload,
    proof: &str,
) -> Result<()> {
    let payload_bytes = serde_json::to_vec(payload)?;
    verify_payload(
        exporter,
        request_id,
        pin,
        b"synly-client-proof",
        &payload_bytes,
        proof,
    )
}

pub fn sign_pair_decision(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    accepted: bool,
    message: &str,
    agreement: &SessionAgreement,
    workspace: &WorkspaceSummary,
) -> Result<String> {
    let payload = serde_json::to_vec(&DecisionProofPayload {
        accepted,
        message,
        agreement,
        workspace,
    })?;
    Ok(sign_payload(
        exporter,
        request_id,
        pin,
        b"synly-server-proof",
        &payload,
    ))
}

pub fn verify_pair_decision(
    message: &ControlMessage,
    exporter: &[u8],
    request_id: &str,
    pin: &str,
) -> Result<()> {
    match message {
        ControlMessage::PairDecision {
            accepted,
            message,
            workspace,
            agreement,
            proof,
            ..
        } => {
            let payload = serde_json::to_vec(&DecisionProofPayload {
                accepted: *accepted,
                message,
                agreement,
                workspace,
            })?;
            verify_payload(
                exporter,
                request_id,
                pin,
                b"synly-server-proof",
                &payload,
                proof,
            )
        }
        _ => bail!("not a pair decision message"),
    }
}

#[derive(serde::Serialize)]
struct DecisionProofPayload<'a> {
    accepted: bool,
    message: &'a str,
    agreement: &'a SessionAgreement,
    workspace: &'a WorkspaceSummary,
}

fn sign_payload(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    label: &[u8],
    payload: &[u8],
) -> String {
    let key = derive_pin_key(request_id, pin);
    let mut mac = HmacSha256::new_from_slice(&key).expect("valid HMAC key");
    mac.update(label);
    mac.update(exporter);
    mac.update(payload);
    STANDARD_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify_payload(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    label: &[u8],
    payload: &[u8],
    proof: &str,
) -> Result<()> {
    let key = derive_pin_key(request_id, pin);
    let expected = STANDARD_NO_PAD.decode(proof.as_bytes())?;
    let mut mac = HmacSha256::new_from_slice(&key).expect("valid HMAC key");
    mac.update(label);
    mac.update(exporter);
    mac.update(payload);
    mac.verify_slice(&expected)?;
    Ok(())
}

fn derive_pin_key(request_id: &str, pin: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    let iterations = NonZeroU32::new(PBKDF2_ITERATIONS).expect("non zero iteration count");
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        request_id.as_bytes(),
        pin.trim().as_bytes(),
        &mut key,
    );
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::SyncMode;
    use crate::protocol::{DeviceIdentity, PairRequestPayload};
    use crate::sync::WorkspaceSummary;
    use uuid::Uuid;

    #[test]
    fn pair_auth_sign_and_verify_roundtrip() {
        let payload = PairRequestPayload {
            protocol_version: 1,
            client: DeviceIdentity {
                device_id: Uuid::new_v4(),
                device_name: "tester".into(),
            },
            requested_mode: SyncMode::Both,
            workspace: WorkspaceSummary {
                mode: SyncMode::Both,
                send_description: Some("demo".into()),
                send_layout: None,
                send_items: vec![],
                receive_root: Some("/tmp".into()),
            },
        };
        let exporter = [7u8; 32];
        let proof = sign_pair_auth(&exporter, "request", "123456", &payload).unwrap();
        verify_pair_auth(&exporter, "request", "123456", &payload, &proof).unwrap();
        assert!(verify_pair_auth(&exporter, "request", "654321", &payload, &proof).is_err());
    }
}
