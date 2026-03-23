use crate::config::{DeviceConfig, TrustedDeviceConfig};
use crate::protocol::{
    ControlMessage, DeviceIdentity, PairAuthMethod, PairRequestPayload, SessionAgreement,
    encode_payload,
};
use crate::sync::WorkspaceSummary;
use anyhow::{Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD;
use hmac::{Hmac, Mac};
use rand::RngExt;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair as RcgenKeyPair, KeyUsagePurpose, PKCS_ED25519, SerialNumber,
};
use ring::agreement;
use ring::pbkdf2;
use ring::rand::SystemRandom;
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use rustls::RootCertStore;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, ServerConfig};
use sha2::{Digest, Sha256};
use spake2::{
    Ed25519Group as SpakeGroup, Identity as SpakeIdentity, Password as SpakePassword, Spake2,
};
use std::num::NonZeroU32;
use std::sync::Arc;
use tokio_rustls::{TlsAcceptor, TlsConnector};
use uuid::Uuid;
use x509_parser::certificate::X509Certificate;
use x509_parser::prelude::FromDer;

type HmacSha256 = Hmac<sha2::Sha256>;

const PBKDF2_ITERATIONS: u32 = 100_000;
const BOOTSTRAP_PUBLIC_KEY_LEN: usize = 32;
const RANDOMART_WIDTH: usize = 17;
const RANDOMART_HEIGHT: usize = 9;
const RANDOMART_SYMBOLS: &[u8] = b" .o+=*BOX@%&#/^";
const ED25519_PKCS8_PREFIX: [u8; 16] = [
    0x30, 0x51, 0x02, 0x01, 0x01, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20,
];
const ED25519_PKCS8_PUBLIC_KEY_PREFIX: [u8; 3] = [0x81, 0x21, 0x00];

pub struct BootstrapKeyMaterial {
    private_key: agreement::EphemeralPrivateKey,
    public_key: Vec<u8>,
}

pub struct BootstrapPakeState {
    state: Spake2<SpakeGroup>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintDisplay {
    pub short: String,
    pub randomart: String,
}

#[derive(Debug)]
struct BootstrapTlsMaterials {
    root_certificate: CertificateDer<'static>,
    client_materials: DeviceTlsMaterials,
    server_materials: DeviceTlsMaterials,
}

#[derive(Debug)]
struct DeviceTlsMaterials {
    cert_chain: Vec<CertificateDer<'static>>,
    private_key: PrivateKeyDer<'static>,
}

impl BootstrapKeyMaterial {
    pub fn public_key_encoded(&self) -> String {
        encode_bootstrap_public_key(&self.public_key)
    }

    fn derive_shared_secret(self, peer_public_key: &[u8]) -> Result<[u8; 32]> {
        let peer_public_key =
            agreement::UnparsedPublicKey::new(&agreement::X25519, peer_public_key);
        agreement::agree_ephemeral(self.private_key, &peer_public_key, |shared_secret| {
            let mut output = [0u8; 32];
            output.copy_from_slice(shared_secret);
            output
        })
        .map_err(|_| anyhow!("failed to derive bootstrap shared secret"))
    }
}

pub fn generate_bootstrap_key_material() -> Result<BootstrapKeyMaterial> {
    let rng = SystemRandom::new();
    let private_key = agreement::EphemeralPrivateKey::generate(&agreement::X25519, &rng)
        .map_err(|_| anyhow!("failed to generate bootstrap key material"))?;
    let public_key = private_key
        .compute_public_key()
        .map_err(|_| anyhow!("failed to derive bootstrap public key"))?
        .as_ref()
        .to_vec();
    Ok(BootstrapKeyMaterial {
        private_key,
        public_key,
    })
}

pub fn bootstrap_public_key_display(public_key: &str) -> Result<FingerprintDisplay> {
    let public_key = decode_bootstrap_public_key(public_key)?;
    Ok(fingerprint_display("bootstrap", &public_key))
}

pub fn bootstrap_session_display(
    request_id: &str,
    client_public_key: &str,
    server_public_key: &str,
) -> Result<FingerprintDisplay> {
    let client_public_key = decode_bootstrap_public_key(client_public_key)?;
    let server_public_key = decode_bootstrap_public_key(server_public_key)?;
    let mut context = Vec::with_capacity(
        request_id.len() + client_public_key.len() + server_public_key.len() + 16,
    );
    context.extend_from_slice(b"synly-bootstrap-session");
    context.extend_from_slice(request_id.as_bytes());
    context.extend_from_slice(&client_public_key);
    context.extend_from_slice(&server_public_key);
    Ok(fingerprint_display("session", &context))
}

pub fn start_bootstrap_pake_client(
    pin: &str,
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> Result<(BootstrapPakeState, String)> {
    let password = SpakePassword::new(pin.trim().as_bytes());
    let (id_a, id_b) = bootstrap_pake_identities(
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
    );
    let (state, outbound_message) = Spake2::<SpakeGroup>::start_a(&password, &id_a, &id_b);
    Ok((
        BootstrapPakeState { state },
        encode_bootstrap_message(&outbound_message),
    ))
}

pub fn start_bootstrap_pake_server(
    pin: &str,
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> Result<(BootstrapPakeState, String)> {
    let password = SpakePassword::new(pin.trim().as_bytes());
    let (id_a, id_b) = bootstrap_pake_identities(
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
    );
    let (state, outbound_message) = Spake2::<SpakeGroup>::start_b(&password, &id_a, &id_b);
    Ok((
        BootstrapPakeState { state },
        encode_bootstrap_message(&outbound_message),
    ))
}

pub fn finish_bootstrap_pake(state: BootstrapPakeState, inbound_message: &str) -> Result<Vec<u8>> {
    let inbound_message = decode_bootstrap_message(inbound_message)?;
    state
        .state
        .finish(&inbound_message)
        .map_err(|err| anyhow!("PAKE handshake failed: {err}"))
}

pub fn client_pake_confirm(
    pake_key: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> String {
    bootstrap_confirmation(
        pake_key,
        b"synly-pake-client-confirm",
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
    )
}

pub fn verify_client_pake_confirm(
    pake_key: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
    proof: &str,
) -> Result<()> {
    verify_bootstrap_confirmation(
        pake_key,
        b"synly-pake-client-confirm",
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
        proof,
    )
}

pub fn server_pake_confirm(
    pake_key: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> String {
    bootstrap_confirmation(
        pake_key,
        b"synly-pake-server-confirm",
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
    )
}

pub fn verify_server_pake_confirm(
    pake_key: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
    proof: &str,
) -> Result<()> {
    verify_bootstrap_confirmation(
        pake_key,
        b"synly-pake-server-confirm",
        request_id,
        client_bootstrap_public_key,
        server_bootstrap_public_key,
        proof,
    )
}

pub fn build_server_acceptor(
    device: &DeviceConfig,
    trusted_devices: &[TrustedDeviceConfig],
) -> Result<TlsAcceptor> {
    let tls_materials = build_device_tls_materials(device)?;
    let Some(roots) = trusted_client_roots(trusted_devices)? else {
        bail!("no trusted client roots configured");
    };
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(WebPkiClientVerifier::builder(Arc::new(roots)).build()?)
        .with_single_cert(tls_materials.cert_chain, tls_materials.private_key)?;
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
}

pub fn build_client_connector(
    device: &DeviceConfig,
    trusted_server_root_certificate: &str,
) -> Result<TlsConnector> {
    let mut roots = RootCertStore::empty();
    roots.add(decode_certificate_der(trusted_server_root_certificate)?)?;
    build_client_connector_with_roots(device, roots)
}

pub fn build_client_connector_for_trusted_devices(
    device: &DeviceConfig,
    trusted_devices: &[TrustedDeviceConfig],
) -> Result<TlsConnector> {
    let Some(roots) = trusted_client_roots(trusted_devices)? else {
        bail!("no trusted server roots configured");
    };
    build_client_connector_with_roots(device, roots)
}

pub fn build_bootstrap_client_connector(
    request_id: &str,
    pake_key: &[u8],
    client_bootstrap_key: BootstrapKeyMaterial,
    server_bootstrap_public_key: &str,
) -> Result<TlsConnector> {
    let client_public_key = client_bootstrap_key.public_key.clone();
    let server_public_key = decode_bootstrap_public_key(server_bootstrap_public_key)?;
    let shared_secret = client_bootstrap_key.derive_shared_secret(&server_public_key)?;
    let tls_materials = derive_bootstrap_tls_materials(
        &shared_secret,
        pake_key,
        request_id,
        &client_public_key,
        &server_public_key,
    )?;
    let mut roots = RootCertStore::empty();
    roots.add(tls_materials.root_certificate.clone())?;
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(
            tls_materials.client_materials.cert_chain,
            tls_materials.client_materials.private_key,
        )?;
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsConnector::from(Arc::new(config)))
}

pub fn build_bootstrap_server_acceptor(
    request_id: &str,
    pake_key: &[u8],
    server_bootstrap_key: BootstrapKeyMaterial,
    client_bootstrap_public_key: &str,
) -> Result<TlsAcceptor> {
    let server_public_key = server_bootstrap_key.public_key.clone();
    let client_public_key = decode_bootstrap_public_key(client_bootstrap_public_key)?;
    let shared_secret = server_bootstrap_key.derive_shared_secret(&client_public_key)?;
    let tls_materials = derive_bootstrap_tls_materials(
        &shared_secret,
        pake_key,
        request_id,
        &client_public_key,
        &server_public_key,
    )?;
    let mut roots = RootCertStore::empty();
    roots.add(tls_materials.root_certificate.clone())?;
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(WebPkiClientVerifier::builder(Arc::new(roots)).build()?)
        .with_single_cert(
            tls_materials.server_materials.cert_chain,
            tls_materials.server_materials.private_key,
        )?;
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsAcceptor::from(Arc::new(config)))
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

pub fn export_audio_master_secret_from_client<T>(
    stream: &tokio_rustls::client::TlsStream<T>,
    binding_id: &str,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        b"synly-audio-master",
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

pub fn export_audio_master_secret_from_server<T>(
    stream: &tokio_rustls::server::TlsStream<T>,
    binding_id: &str,
) -> Result<[u8; 32]> {
    let mut output = [0u8; 32];
    stream.get_ref().1.export_keying_material(
        &mut output,
        b"synly-audio-master",
        Some(binding_id.as_bytes()),
    )?;
    Ok(output)
}

pub fn random_pin() -> String {
    format!("{:06}", rand::rng().random_range(0..1_000_000))
}

#[cfg(test)]
pub fn sign_pair_auth(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    payload: &PairRequestPayload,
) -> Result<String> {
    let payload_bytes = encode_payload(payload)?;
    Ok(sign_payload(
        exporter,
        request_id,
        pin,
        b"synly-client-proof",
        &payload_bytes,
    ))
}

#[cfg(test)]
pub fn verify_pair_auth(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    payload: &PairRequestPayload,
    proof: &str,
) -> Result<()> {
    let payload_bytes = encode_payload(payload)?;
    verify_payload(
        exporter,
        request_id,
        pin,
        b"synly-client-proof",
        &payload_bytes,
        proof,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn sign_pair_decision(
    exporter: &[u8],
    request_id: &str,
    pin: &str,
    accepted: bool,
    message: &str,
    server: &DeviceIdentity,
    agreement: &SessionAgreement,
    workspace: &WorkspaceSummary,
    audio_mode: crate::cli::AudioMode,
    auth_method: PairAuthMethod,
    server_trusts_client: bool,
    trust_established: bool,
) -> Result<String> {
    let payload = encode_payload(&DecisionProofPayload {
        accepted,
        message,
        server,
        agreement,
        workspace,
        audio_mode,
        auth_method,
        server_trusts_client,
        trust_established,
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
            server,
            workspace,
            agreement,
            audio_mode,
            auth_method,
            server_trusts_client,
            proof,
            trust_established,
            ..
        } => {
            let payload = encode_payload(&DecisionProofPayload {
                accepted: *accepted,
                message,
                server,
                agreement,
                workspace,
                audio_mode: *audio_mode,
                auth_method: *auth_method,
                server_trusts_client: *server_trusts_client,
                trust_established: *trust_established,
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

pub fn sign_trusted_pair_auth(
    exporter: &[u8],
    private_key: &str,
    request_id: &str,
    payload: &PairRequestPayload,
) -> Result<String> {
    let payload_bytes = encode_payload(payload)?;
    sign_identity_payload(
        private_key,
        exporter,
        request_id,
        b"synly-trusted-client-proof",
        &payload_bytes,
    )
}

pub fn verify_trusted_pair_auth(
    exporter: &[u8],
    public_key: &str,
    request_id: &str,
    payload: &PairRequestPayload,
    proof: &str,
) -> Result<()> {
    let payload_bytes = encode_payload(payload)?;
    verify_identity_payload(
        public_key,
        exporter,
        request_id,
        b"synly-trusted-client-proof",
        &payload_bytes,
        proof,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn sign_trusted_pair_decision(
    private_key: &str,
    exporter: &[u8],
    request_id: &str,
    accepted: bool,
    message: &str,
    server: &DeviceIdentity,
    agreement: &SessionAgreement,
    workspace: &WorkspaceSummary,
    audio_mode: crate::cli::AudioMode,
    server_trusts_client: bool,
    trust_established: bool,
) -> Result<String> {
    let payload = encode_payload(&DecisionProofPayload {
        accepted,
        message,
        server,
        agreement,
        workspace,
        audio_mode,
        auth_method: PairAuthMethod::TrustedDevice,
        server_trusts_client,
        trust_established,
    })?;
    sign_identity_payload(
        private_key,
        exporter,
        request_id,
        b"synly-trusted-server-proof",
        &payload,
    )
}

pub fn verify_trusted_pair_decision(
    message: &ControlMessage,
    exporter: &[u8],
    request_id: &str,
    public_key: &str,
) -> Result<()> {
    match message {
        ControlMessage::PairDecision {
            accepted,
            message,
            server,
            workspace,
            agreement,
            audio_mode,
            auth_method,
            server_trusts_client,
            proof,
            trust_established,
            ..
        } => {
            let payload = encode_payload(&DecisionProofPayload {
                accepted: *accepted,
                message,
                server,
                agreement,
                workspace,
                audio_mode: *audio_mode,
                auth_method: *auth_method,
                server_trusts_client: *server_trusts_client,
                trust_established: *trust_established,
            })?;
            verify_identity_payload(
                public_key,
                exporter,
                request_id,
                b"synly-trusted-server-proof",
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
    server: &'a DeviceIdentity,
    agreement: &'a SessionAgreement,
    workspace: &'a WorkspaceSummary,
    audio_mode: crate::cli::AudioMode,
    auth_method: PairAuthMethod,
    server_trusts_client: bool,
    trust_established: bool,
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

fn sign_identity_payload(
    private_key: &str,
    exporter: &[u8],
    request_id: &str,
    label: &[u8],
    payload: &[u8],
) -> Result<String> {
    let key_pair = decode_identity_private_key(private_key)?;
    let message = identity_message(exporter, request_id, label, payload);
    Ok(STANDARD_NO_PAD.encode(key_pair.sign(&message).as_ref()))
}

fn verify_identity_payload(
    public_key: &str,
    exporter: &[u8],
    request_id: &str,
    label: &[u8],
    payload: &[u8],
    proof: &str,
) -> Result<()> {
    let verifying_key = decode_identity_public_key(public_key)?;
    let signature = STANDARD_NO_PAD.decode(proof.as_bytes())?;
    let message = identity_message(exporter, request_id, label, payload);
    UnparsedPublicKey::new(&ED25519, verifying_key)
        .verify(&message, &signature)
        .map_err(|_| anyhow!("identity signature verification failed"))
}

fn identity_message(exporter: &[u8], request_id: &str, label: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut message =
        Vec::with_capacity(label.len() + exporter.len() + request_id.len() + payload.len());
    message.extend_from_slice(label);
    message.extend_from_slice(exporter);
    message.extend_from_slice(request_id.as_bytes());
    message.extend_from_slice(payload);
    message
}

fn decode_identity_private_key(private_key: &str) -> Result<Ed25519KeyPair> {
    let pkcs8 = STANDARD_NO_PAD.decode(private_key.trim().as_bytes())?;
    Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| anyhow!("failed to parse device identity private key"))
}

fn decode_identity_public_key(public_key: &str) -> Result<Vec<u8>> {
    STANDARD_NO_PAD
        .decode(public_key.trim().as_bytes())
        .map_err(Into::into)
}

#[cfg(test)]
pub fn public_key_matches_private_key(private_key: &str, public_key: &str) -> Result<bool> {
    let key_pair = decode_identity_private_key(private_key)?;
    Ok(STANDARD_NO_PAD.encode(key_pair.public_key().as_ref()) == public_key.trim())
}

pub fn public_keys_match(left: &str, right: &str) -> bool {
    left.trim() == right.trim()
}

pub fn verify_device_identity(identity: &DeviceIdentity, expected_public_key: &str) -> Result<()> {
    verify_device_identity_material(identity)?;
    if !public_keys_match(&identity.identity_public_key, expected_public_key) {
        bail!("trusted public key does not match the peer's advertised identity key");
    }
    Ok(())
}

pub fn verify_device_identity_material(identity: &DeviceIdentity) -> Result<()> {
    verify_tls_root_certificate_matches_public_key(
        &identity.tls_root_certificate,
        &identity.identity_public_key,
    )
}

pub fn device_tls_root_certificate(device: &DeviceConfig) -> Result<String> {
    let certificate = build_device_root_certificate_der(device)?;
    Ok(STANDARD_NO_PAD.encode(certificate.as_ref()))
}

pub fn verify_tls_root_certificate_matches_public_key(
    root_certificate: &str,
    public_key: &str,
) -> Result<()> {
    let certificate = decode_certificate_der(root_certificate)?;
    let mut roots = RootCertStore::empty();
    roots.add(certificate.clone())?;
    let certificate_public_key =
        STANDARD_NO_PAD.encode(parse_certificate_public_key_bytes(certificate.as_ref())?);
    if !public_keys_match(&certificate_public_key, public_key) {
        bail!("TLS root certificate does not match the advertised identity public key");
    }
    Ok(())
}

pub fn short_identity_fingerprint(public_key: &str) -> Result<String> {
    let key_bytes = decode_identity_public_key(public_key)?;
    Ok(short_fingerprint_from_source(&key_bytes))
}

fn derive_bootstrap_tls_materials(
    shared_secret: &[u8],
    pake_key: &[u8],
    request_id: &str,
    client_public_key: &[u8],
    server_public_key: &[u8],
) -> Result<BootstrapTlsMaterials> {
    let bootstrap_secret = derive_bootstrap_secret(
        shared_secret,
        pake_key,
        request_id,
        client_public_key,
        server_public_key,
    );
    let root_seed = expand_bootstrap_secret(&bootstrap_secret, b"ca-seed");
    let client_seed = expand_bootstrap_secret(&bootstrap_secret, b"client-leaf-seed");
    let server_seed = expand_bootstrap_secret(&bootstrap_secret, b"server-leaf-seed");

    let root_key_pair = deterministic_rcgen_key_pair(&root_seed)?;
    let root_params =
        bootstrap_root_certificate_params(request_id, client_public_key, server_public_key);
    let root_issuer = CertifiedIssuer::self_signed(root_params, root_key_pair)?;
    let root_certificate = root_issuer.der().clone();

    let client_key_pair = deterministic_rcgen_key_pair(&client_seed)?;
    let client_certificate =
        bootstrap_client_leaf_certificate_params(request_id, client_public_key, server_public_key)?
            .signed_by(&client_key_pair, &root_issuer)?;
    let client_materials = DeviceTlsMaterials {
        cert_chain: vec![client_certificate.der().clone(), root_certificate.clone()],
        private_key: PrivateKeyDer::from(client_key_pair),
    };

    let server_key_pair = deterministic_rcgen_key_pair(&server_seed)?;
    let server_certificate =
        bootstrap_server_leaf_certificate_params(request_id, client_public_key, server_public_key)?
            .signed_by(&server_key_pair, &root_issuer)?;
    let server_materials = DeviceTlsMaterials {
        cert_chain: vec![server_certificate.der().clone(), root_certificate.clone()],
        private_key: PrivateKeyDer::from(server_key_pair),
    };

    Ok(BootstrapTlsMaterials {
        root_certificate,
        client_materials,
        server_materials,
    })
}

fn derive_bootstrap_secret(
    shared_secret: &[u8],
    pake_key: &[u8],
    request_id: &str,
    client_public_key: &[u8],
    server_public_key: &[u8],
) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(pake_key).expect("valid HMAC key");
    mac.update(b"synly-bootstrap-secret");
    mac.update(shared_secret);
    mac.update(request_id.as_bytes());
    mac.update(client_public_key);
    mac.update(server_public_key);
    let mut output = [0u8; 32];
    output.copy_from_slice(&mac.finalize().into_bytes());
    output
}

fn expand_bootstrap_secret(secret: &[u8; 32], label: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(secret).expect("valid HMAC key");
    mac.update(b"synly-bootstrap-expand");
    mac.update(label);
    let mut output = [0u8; 32];
    output.copy_from_slice(&mac.finalize().into_bytes());
    output
}

fn bootstrap_root_certificate_params(
    request_id: &str,
    client_public_key: &[u8],
    server_public_key: &[u8],
) -> CertificateParams {
    let mut params = CertificateParams::default();
    params.serial_number = Some(serial_number_from_slices(&[
        request_id.as_bytes(),
        client_public_key,
        server_public_key,
        b"synly-bootstrap-root",
    ]));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    params.distinguished_name = distinguished_name_for_device("synly-bootstrap-root", request_id);
    params
}

fn bootstrap_client_leaf_certificate_params(
    request_id: &str,
    client_public_key: &[u8],
    server_public_key: &[u8],
) -> Result<CertificateParams> {
    let mut params =
        CertificateParams::new(vec!["synly.local".to_string(), "localhost".to_string()])?;
    params.serial_number = Some(serial_number_from_slices(&[
        request_id.as_bytes(),
        client_public_key,
        server_public_key,
        b"synly-bootstrap-client",
    ]));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.distinguished_name = distinguished_name_for_device("synly-bootstrap-client", request_id);
    Ok(params)
}

fn bootstrap_server_leaf_certificate_params(
    request_id: &str,
    client_public_key: &[u8],
    server_public_key: &[u8],
) -> Result<CertificateParams> {
    let mut params =
        CertificateParams::new(vec!["synly.local".to_string(), "localhost".to_string()])?;
    params.serial_number = Some(serial_number_from_slices(&[
        request_id.as_bytes(),
        client_public_key,
        server_public_key,
        b"synly-bootstrap-server",
    ]));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    params.distinguished_name = distinguished_name_for_device("synly-bootstrap-server", request_id);
    Ok(params)
}

fn deterministic_rcgen_key_pair(seed: &[u8; 32]) -> Result<RcgenKeyPair> {
    let pkcs8 = ed25519_pkcs8_from_seed(seed)?;
    let private_key = PrivatePkcs8KeyDer::from(pkcs8);
    Ok(RcgenKeyPair::from_pkcs8_der_and_sign_algo(
        &private_key,
        &PKCS_ED25519,
    )?)
}

fn ed25519_pkcs8_from_seed(seed: &[u8; 32]) -> Result<Vec<u8>> {
    let key_pair = Ed25519KeyPair::from_seed_unchecked(seed)
        .map_err(|_| anyhow!("failed to derive deterministic Ed25519 key"))?;
    let mut pkcs8 = Vec::with_capacity(
        ED25519_PKCS8_PREFIX.len() + seed.len() + ED25519_PKCS8_PUBLIC_KEY_PREFIX.len() + 32,
    );
    pkcs8.extend_from_slice(&ED25519_PKCS8_PREFIX);
    pkcs8.extend_from_slice(seed);
    pkcs8.extend_from_slice(&ED25519_PKCS8_PUBLIC_KEY_PREFIX);
    pkcs8.extend_from_slice(key_pair.public_key().as_ref());
    Ok(pkcs8)
}

fn fingerprint_display(label: &str, source: &[u8]) -> FingerprintDisplay {
    let digest = Sha256::digest(source);
    FingerprintDisplay {
        short: short_fingerprint_from_digest(&digest),
        randomart: randomart_from_digest(label, &digest),
    }
}

fn short_fingerprint_from_source(source: &[u8]) -> String {
    let digest = Sha256::digest(source);
    short_fingerprint_from_digest(&digest)
}

fn short_fingerprint_from_digest(digest: &[u8]) -> String {
    let hex = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    hex.as_bytes()
        .chunks(4)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("-")
}

fn randomart_from_digest(label: &str, digest: &[u8]) -> String {
    let mut board = [[0u8; RANDOMART_WIDTH]; RANDOMART_HEIGHT];
    let mut x = (RANDOMART_WIDTH / 2) as isize;
    let mut y = (RANDOMART_HEIGHT / 2) as isize;
    let start_x = x;
    let start_y = y;

    for byte in digest {
        for shift in [0, 2, 4, 6] {
            let step = (byte >> shift) & 0x03;
            x += if step & 0x01 == 0 { -1 } else { 1 };
            y += if step & 0x02 == 0 { -1 } else { 1 };
            x = x.clamp(0, (RANDOMART_WIDTH - 1) as isize);
            y = y.clamp(0, (RANDOMART_HEIGHT - 1) as isize);
            board[y as usize][x as usize] = board[y as usize][x as usize].saturating_add(1);
        }
    }

    let end_x = x as usize;
    let end_y = y as usize;
    let mut output = String::new();
    output.push_str(&format!("+--[{label:^11.11}]--+\n"));
    for (row_index, row) in board.iter().enumerate() {
        output.push('|');
        for (col_index, value) in row.iter().enumerate() {
            let ch = if row_index == start_y as usize && col_index == start_x as usize {
                if row_index == end_y && col_index == end_x {
                    'E'
                } else {
                    'S'
                }
            } else if row_index == end_y && col_index == end_x {
                'E'
            } else {
                RANDOMART_SYMBOLS[(*value as usize).min(RANDOMART_SYMBOLS.len() - 1)] as char
            };
            output.push(ch);
        }
        output.push_str("|\n");
    }
    output.push_str("+-----------------+");
    output
}

fn bootstrap_pake_identities(
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> (SpakeIdentity, SpakeIdentity) {
    let id_a = SpakeIdentity::new(
        format!("synly-bootstrap-pake/client/{request_id}/{client_bootstrap_public_key}")
            .as_bytes(),
    );
    let id_b = SpakeIdentity::new(
        format!("synly-bootstrap-pake/server/{request_id}/{server_bootstrap_public_key}")
            .as_bytes(),
    );
    (id_a, id_b)
}

fn bootstrap_confirmation(
    pake_key: &[u8],
    label: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
) -> String {
    let mut mac = HmacSha256::new_from_slice(pake_key).expect("valid HMAC key");
    mac.update(label);
    mac.update(request_id.as_bytes());
    mac.update(client_bootstrap_public_key.as_bytes());
    mac.update(server_bootstrap_public_key.as_bytes());
    STANDARD_NO_PAD.encode(mac.finalize().into_bytes())
}

fn verify_bootstrap_confirmation(
    pake_key: &[u8],
    label: &[u8],
    request_id: &str,
    client_bootstrap_public_key: &str,
    server_bootstrap_public_key: &str,
    proof: &str,
) -> Result<()> {
    let expected = STANDARD_NO_PAD.decode(proof.as_bytes())?;
    let mut mac = HmacSha256::new_from_slice(pake_key).expect("valid HMAC key");
    mac.update(label);
    mac.update(request_id.as_bytes());
    mac.update(client_bootstrap_public_key.as_bytes());
    mac.update(server_bootstrap_public_key.as_bytes());
    mac.verify_slice(&expected)?;
    Ok(())
}

fn encode_bootstrap_message(message: &[u8]) -> String {
    STANDARD_NO_PAD.encode(message)
}

fn decode_bootstrap_message(message: &str) -> Result<Vec<u8>> {
    Ok(STANDARD_NO_PAD.decode(message.trim().as_bytes())?)
}

fn encode_bootstrap_public_key(public_key: &[u8]) -> String {
    STANDARD_NO_PAD.encode(public_key)
}

pub fn decode_bootstrap_public_key(public_key: &str) -> Result<Vec<u8>> {
    let public_key = STANDARD_NO_PAD.decode(public_key.trim().as_bytes())?;
    if public_key.len() != BOOTSTRAP_PUBLIC_KEY_LEN {
        bail!(
            "bootstrap public key must be {BOOTSTRAP_PUBLIC_KEY_LEN} bytes, got {}",
            public_key.len()
        );
    }
    Ok(public_key)
}

fn build_device_tls_materials(device: &DeviceConfig) -> Result<DeviceTlsMaterials> {
    let root_private_key = decode_identity_private_key_pkcs8(device.identity_private_key()?)?;
    let root_key_pair =
        RcgenKeyPair::from_pkcs8_der_and_sign_algo(&root_private_key, &PKCS_ED25519)?;
    let root_params = device_root_certificate_params(device)?;
    let root_issuer = CertifiedIssuer::self_signed(root_params, root_key_pair)?;
    let root_certificate = root_issuer.der().clone();

    let leaf_key_pair = RcgenKeyPair::generate_for(&PKCS_ED25519)?;
    let leaf_certificate =
        device_leaf_certificate_params(device)?.signed_by(&leaf_key_pair, &root_issuer)?;
    let cert_chain = vec![leaf_certificate.der().clone(), root_certificate.clone()];
    let private_key = PrivateKeyDer::from(leaf_key_pair);

    Ok(DeviceTlsMaterials {
        cert_chain,
        private_key,
    })
}

fn build_device_root_certificate_der(device: &DeviceConfig) -> Result<CertificateDer<'static>> {
    let root_private_key = decode_identity_private_key_pkcs8(device.identity_private_key()?)?;
    let root_key_pair =
        RcgenKeyPair::from_pkcs8_der_and_sign_algo(&root_private_key, &PKCS_ED25519)?;
    let root_certificate = device_root_certificate_params(device)?.self_signed(&root_key_pair)?;
    Ok(root_certificate.der().clone())
}

fn device_root_certificate_params(device: &DeviceConfig) -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    params.serial_number = Some(serial_number_from_slices(&[
        device.device_id.as_bytes(),
        device.identity_public_key()?.as_bytes(),
        b"synly-root",
    ]));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::DigitalSignature,
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
    ];
    params.distinguished_name =
        distinguished_name_for_device("synly-device-root", &device.device_id.to_string());
    Ok(params)
}

fn device_leaf_certificate_params(device: &DeviceConfig) -> Result<CertificateParams> {
    let leaf_nonce = Uuid::new_v4();
    let mut params =
        CertificateParams::new(vec!["synly.local".to_string(), "localhost".to_string()])?;
    params.serial_number = Some(serial_number_from_slices(&[
        device.device_id.as_bytes(),
        leaf_nonce.as_bytes(),
        b"synly-leaf",
    ]));
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::ClientAuth,
    ];
    params.distinguished_name =
        distinguished_name_for_device("synly-device-session", &device.device_id.to_string());
    Ok(params)
}

fn distinguished_name_for_device(prefix: &str, device_id: &str) -> rcgen::DistinguishedName {
    let mut name = rcgen::DistinguishedName::new();
    name.push(DnType::CommonName, format!("{prefix}-{device_id}"));
    name
}

fn serial_number_from_slices(parts: &[&[u8]]) -> SerialNumber {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let mut serial = hasher.finalize()[..16].to_vec();
    serial[0] &= 0x7f;
    if serial.iter().all(|byte| *byte == 0) {
        serial[0] = 1;
    }
    SerialNumber::from(serial)
}

fn trusted_client_roots(trusted_devices: &[TrustedDeviceConfig]) -> Result<Option<RootCertStore>> {
    let mut roots = RootCertStore::empty();
    let mut added = 0usize;
    for device in trusted_devices {
        if device.public_key.trim().is_empty() || device.tls_root_certificate.trim().is_empty() {
            continue;
        }
        roots.add(decode_certificate_der(&device.tls_root_certificate)?)?;
        added += 1;
    }
    if added == 0 {
        Ok(None)
    } else {
        Ok(Some(roots))
    }
}

fn build_client_connector_with_roots(
    device: &DeviceConfig,
    roots: RootCertStore,
) -> Result<TlsConnector> {
    let tls_materials = build_device_tls_materials(device)?;
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_client_auth_cert(tls_materials.cert_chain, tls_materials.private_key)?;
    config.alpn_protocols = vec![b"synly/1".to_vec()];

    Ok(TlsConnector::from(Arc::new(config)))
}

fn parse_certificate_public_key_bytes(certificate_der: &[u8]) -> Result<Vec<u8>> {
    let (_, certificate) = X509Certificate::from_der(certificate_der)
        .map_err(|err| anyhow!("failed to parse TLS certificate: {err}"))?;
    Ok(certificate.public_key().subject_public_key.data.to_vec())
}

fn decode_identity_private_key_pkcs8(private_key: &str) -> Result<PrivatePkcs8KeyDer<'static>> {
    let pkcs8 = STANDARD_NO_PAD.decode(private_key.trim().as_bytes())?;
    Ok(PrivatePkcs8KeyDer::from(pkcs8))
}

fn decode_certificate_der(certificate: &str) -> Result<CertificateDer<'static>> {
    let der = STANDARD_NO_PAD.decode(certificate.trim().as_bytes())?;
    Ok(CertificateDer::from(der))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{AudioMode, ClipboardMode, SyncMode};
    use crate::config::DeviceConfig;
    use crate::protocol::{
        ControlMessage, DeviceIdentity, PROTOCOL_VERSION, PairAuthMethod, PairRequestPayload,
        SessionAgreement,
    };
    use crate::sync::WorkspaceSummary;
    use ring::rand::SystemRandom;
    use ring::signature::KeyPair;
    use uuid::Uuid;

    fn sample_identity() -> (String, String) {
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let private_key = STANDARD_NO_PAD.encode(pkcs8.as_ref());
        let key_pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let public_key = STANDARD_NO_PAD.encode(key_pair.public_key().as_ref());
        (private_key, public_key)
    }

    fn sample_device() -> DeviceConfig {
        let (private_key, public_key) = sample_identity();
        sample_device_from_identity(private_key, public_key)
    }

    fn sample_device_from_identity(private_key: String, public_key: String) -> DeviceConfig {
        DeviceConfig {
            device_id: Uuid::new_v4(),
            device_name: "tester".into(),
            identity_private_key: Some(private_key),
            identity_public_key: Some(public_key),
        }
    }

    #[test]
    fn pair_auth_sign_and_verify_roundtrip() {
        let device = sample_device();
        let payload = PairRequestPayload {
            protocol_version: PROTOCOL_VERSION,
            client: DeviceIdentity {
                device_id: Uuid::new_v4(),
                device_name: "tester".into(),
                instance_name: Some("worker-a".into()),
                identity_public_key: device.identity_public_key().unwrap().to_string(),
                tls_root_certificate: device_tls_root_certificate(&device).unwrap(),
            },
            requested_mode: SyncMode::Both,
            workspace: WorkspaceSummary {
                mode: SyncMode::Both,
                send_description: Some("demo".into()),
                send_layout: None,
                send_items: vec![],
                receive_root: Some("/tmp".into()),
                max_folder_depth: None,
                clipboard_mode: ClipboardMode::Off,
            },
            audio_mode: AudioMode::Off,
            request_trust: false,
        };
        let exporter = [7u8; 32];
        let proof = sign_pair_auth(&exporter, "request", "123456", &payload).unwrap();
        verify_pair_auth(&exporter, "request", "123456", &payload, &proof).unwrap();
        assert!(verify_pair_auth(&exporter, "request", "654321", &payload, &proof).is_err());
    }

    #[test]
    fn trusted_pair_auth_sign_and_verify_roundtrip() {
        let (private_key, public_key) = sample_identity();
        let device = sample_device_from_identity(private_key.clone(), public_key.clone());
        let payload = PairRequestPayload {
            protocol_version: PROTOCOL_VERSION,
            client: DeviceIdentity {
                device_id: Uuid::new_v4(),
                device_name: "tester".into(),
                instance_name: Some("worker-a".into()),
                identity_public_key: public_key.clone(),
                tls_root_certificate: device_tls_root_certificate(&device).unwrap(),
            },
            requested_mode: SyncMode::Both,
            workspace: WorkspaceSummary {
                mode: SyncMode::Both,
                send_description: Some("demo".into()),
                send_layout: None,
                send_items: vec![],
                receive_root: Some("/tmp".into()),
                max_folder_depth: None,
                clipboard_mode: ClipboardMode::Off,
            },
            audio_mode: AudioMode::Off,
            request_trust: true,
        };
        let exporter = [9u8; 32];
        let proof = sign_trusted_pair_auth(&exporter, &private_key, "request", &payload).unwrap();
        verify_trusted_pair_auth(&exporter, &public_key, "request", &payload, &proof).unwrap();
        let (_, other_public_key) = sample_identity();
        assert!(
            verify_trusted_pair_auth(&exporter, &other_public_key, "request", &payload, &proof)
                .is_err()
        );
    }

    #[test]
    fn pair_decision_signature_covers_server_trust_choice() {
        let device = sample_device();
        let exporter = [5u8; 32];
        let request_id = "request";
        let pin = "123456";
        let message = "accepted";
        let server = DeviceIdentity {
            device_id: device.device_id,
            device_name: device.device_name.clone(),
            instance_name: Some("worker-a".into()),
            identity_public_key: device.identity_public_key().unwrap().to_string(),
            tls_root_certificate: device_tls_root_certificate(&device).unwrap(),
        };
        let agreement = SessionAgreement {
            host_to_client: true,
            client_to_host: false,
        };
        let workspace = WorkspaceSummary {
            mode: SyncMode::Both,
            send_description: Some("demo".into()),
            send_layout: None,
            send_items: vec![],
            receive_root: Some("/tmp".into()),
            max_folder_depth: None,
            clipboard_mode: ClipboardMode::Off,
        };
        let proof = sign_pair_decision(
            &exporter,
            request_id,
            pin,
            true,
            message,
            &server,
            &agreement,
            &workspace,
            AudioMode::Off,
            PairAuthMethod::Pin,
            true,
            false,
        )
        .unwrap();
        let decision = ControlMessage::PairDecision {
            accepted: true,
            message: message.into(),
            server: server.clone(),
            workspace: workspace.clone(),
            agreement: agreement.clone(),
            audio_mode: AudioMode::Off,
            auth_method: PairAuthMethod::Pin,
            server_trusts_client: true,
            proof: proof.clone(),
            trust_established: false,
        };
        verify_pair_decision(&decision, &exporter, request_id, pin).unwrap();

        let tampered_decision = ControlMessage::PairDecision {
            accepted: true,
            message: message.into(),
            server,
            workspace,
            agreement,
            audio_mode: AudioMode::Send,
            auth_method: PairAuthMethod::Pin,
            server_trusts_client: false,
            proof,
            trust_established: false,
        };
        assert!(verify_pair_decision(&tampered_decision, &exporter, request_id, pin).is_err());
    }

    #[test]
    fn trusted_pair_decision_signature_covers_server_trust_choice() {
        let (private_key, public_key) = sample_identity();
        let device = sample_device_from_identity(private_key.clone(), public_key.clone());
        let exporter = [6u8; 32];
        let request_id = "request";
        let message = "accepted";
        let server = DeviceIdentity {
            device_id: device.device_id,
            device_name: device.device_name.clone(),
            instance_name: Some("worker-a".into()),
            identity_public_key: public_key.clone(),
            tls_root_certificate: device_tls_root_certificate(&device).unwrap(),
        };
        let agreement = SessionAgreement {
            host_to_client: true,
            client_to_host: true,
        };
        let workspace = WorkspaceSummary {
            mode: SyncMode::Both,
            send_description: Some("demo".into()),
            send_layout: None,
            send_items: vec![],
            receive_root: Some("/tmp".into()),
            max_folder_depth: None,
            clipboard_mode: ClipboardMode::Both,
        };
        let proof = sign_trusted_pair_decision(
            &private_key,
            &exporter,
            request_id,
            true,
            message,
            &server,
            &agreement,
            &workspace,
            AudioMode::Off,
            true,
            false,
        )
        .unwrap();
        let decision = ControlMessage::PairDecision {
            accepted: true,
            message: message.into(),
            server: server.clone(),
            workspace: workspace.clone(),
            agreement: agreement.clone(),
            audio_mode: AudioMode::Off,
            auth_method: PairAuthMethod::TrustedDevice,
            server_trusts_client: true,
            proof: proof.clone(),
            trust_established: false,
        };
        verify_trusted_pair_decision(&decision, &exporter, request_id, &public_key).unwrap();

        let tampered_decision = ControlMessage::PairDecision {
            accepted: true,
            message: message.into(),
            server,
            workspace,
            agreement,
            audio_mode: AudioMode::Send,
            auth_method: PairAuthMethod::TrustedDevice,
            server_trusts_client: false,
            proof,
            trust_established: false,
        };
        assert!(
            verify_trusted_pair_decision(&tampered_decision, &exporter, request_id, &public_key)
                .is_err()
        );
    }

    #[test]
    fn public_key_matches_private_key_roundtrip() {
        let (private_key, public_key) = sample_identity();
        assert!(public_key_matches_private_key(&private_key, &public_key).unwrap());
    }

    #[test]
    fn tls_root_certificate_matches_device_identity_key() {
        let device = sample_device();
        let certificate = device_tls_root_certificate(&device).unwrap();
        verify_tls_root_certificate_matches_public_key(
            &certificate,
            device.identity_public_key().unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn deterministic_ed25519_pkcs8_roundtrip() {
        let seed = [7u8; 32];
        let pkcs8 = ed25519_pkcs8_from_seed(&seed).unwrap();
        let from_pkcs8 = Ed25519KeyPair::from_pkcs8(&pkcs8).unwrap();
        let from_seed = Ed25519KeyPair::from_seed_unchecked(&seed).unwrap();
        assert_eq!(
            from_pkcs8.public_key().as_ref(),
            from_seed.public_key().as_ref()
        );
    }

    #[test]
    fn bootstrap_session_display_is_deterministic() {
        let client_public_key = STANDARD_NO_PAD.encode([1u8; 32]);
        let server_public_key = STANDARD_NO_PAD.encode([2u8; 32]);
        let left =
            bootstrap_session_display("session-1", &client_public_key, &server_public_key).unwrap();
        let right =
            bootstrap_session_display("session-1", &client_public_key, &server_public_key).unwrap();
        assert_eq!(left, right);
        assert!(left.randomart.contains("session"));
    }

    #[test]
    fn bootstrap_tls_materials_are_deterministic() {
        let shared_secret = [9u8; 32];
        let pake_key = [8u8; 32];
        let client_public_key = [1u8; 32];
        let server_public_key = [2u8; 32];
        let left = derive_bootstrap_tls_materials(
            &shared_secret,
            &pake_key,
            "request-1",
            &client_public_key,
            &server_public_key,
        )
        .unwrap();
        let right = derive_bootstrap_tls_materials(
            &shared_secret,
            &pake_key,
            "request-1",
            &client_public_key,
            &server_public_key,
        )
        .unwrap();

        assert_eq!(
            left.root_certificate.as_ref(),
            right.root_certificate.as_ref()
        );
        assert_eq!(
            left.client_materials.cert_chain[0].as_ref(),
            right.client_materials.cert_chain[0].as_ref()
        );
        assert_eq!(
            left.server_materials.cert_chain[0].as_ref(),
            right.server_materials.cert_chain[0].as_ref()
        );
        assert_ne!(
            left.client_materials.cert_chain[0].as_ref(),
            left.server_materials.cert_chain[0].as_ref()
        );
    }

    #[test]
    fn bootstrap_pake_roundtrip_and_confirmation() {
        let client_public_key = STANDARD_NO_PAD.encode([3u8; 32]);
        let server_public_key = STANDARD_NO_PAD.encode([4u8; 32]);
        let (client_state, client_message) = start_bootstrap_pake_client(
            "123456",
            "request-2",
            &client_public_key,
            &server_public_key,
        )
        .unwrap();
        let (server_state, server_message) = start_bootstrap_pake_server(
            "123456",
            "request-2",
            &client_public_key,
            &server_public_key,
        )
        .unwrap();
        let client_key = finish_bootstrap_pake(client_state, &server_message).unwrap();
        let server_key = finish_bootstrap_pake(server_state, &client_message).unwrap();
        assert_eq!(client_key, server_key);

        let client_confirm = client_pake_confirm(
            &client_key,
            "request-2",
            &client_public_key,
            &server_public_key,
        );
        verify_client_pake_confirm(
            &server_key,
            "request-2",
            &client_public_key,
            &server_public_key,
            &client_confirm,
        )
        .unwrap();
        let server_confirm = server_pake_confirm(
            &server_key,
            "request-2",
            &client_public_key,
            &server_public_key,
        );
        verify_server_pake_confirm(
            &client_key,
            "request-2",
            &client_public_key,
            &server_public_key,
            &server_confirm,
        )
        .unwrap();
    }
}
