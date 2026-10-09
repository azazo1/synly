//! 安全 RFCOMM 上的临时 TLS 与长期身份签名, 不请求应用 PIN.

use super::*;
use crate::bluetooth::BluetoothConnection;
use serde::Serialize;

// 这是协议域分隔符, 不是密码或伪造的 PAKE 密钥. 临时 TLS 的秘密仍由 X25519 交换产生.
const RFCOMM_DOMAIN: &[u8] = b"synly/system-authenticated-rfcomm/authorization/v1";

#[derive(Clone, Copy)]
pub(crate) enum Purpose { Request, Decision, Confirmation, Ready }
impl Purpose {
    fn label(self) -> &'static [u8] {
        match self {
            Self::Request => b"synly/bluetooth/identity-request/v1",
            Self::Decision => b"synly/bluetooth/identity-decision/v1",
            Self::Confirmation => b"synly/bluetooth/identity-confirmation/v1",
            Self::Ready => b"synly/bluetooth/identity-ready/v1",
        }
    }
}

pub(crate) fn client_connector(_connection: &BluetoothConnection, request_id: &str, key: BootstrapKeyMaterial, remote_public_key: &str) -> Result<TlsConnector> {
    build_bootstrap_client_connector(request_id, RFCOMM_DOMAIN, key, remote_public_key)
}
pub(crate) fn server_acceptor(_connection: &BluetoothConnection, request_id: &str, key: BootstrapKeyMaterial, remote_public_key: &str) -> Result<TlsAcceptor> {
    build_bootstrap_server_acceptor(request_id, RFCOMM_DOMAIN, key, remote_public_key)
}

pub(crate) fn sign<T: Serialize>(purpose: Purpose, local: &DeviceConfig, exporter: &[u8; 32], request_id: &str, payload: &T) -> Result<String> {
    sign_identity_payload(local.identity_private_key()?, exporter, request_id, purpose.label(), &encode_payload(payload)?)
}
pub(crate) fn verify<T: Serialize>(purpose: Purpose, peer: &DeviceIdentity, exporter: &[u8; 32], request_id: &str, payload: &T, signature: &str) -> Result<()> {
    verify_device_identity_material(peer)?;
    verify_identity_payload(&peer.identity_public_key, exporter, request_id, purpose.label(), &encode_payload(payload)?, signature)
}
