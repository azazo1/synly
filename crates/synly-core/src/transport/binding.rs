//! 将第二条物理连接绑定到已经认证的逻辑会话, 不按蓝牙名称合并设备.

use super::routing::TransportKind;
use crate::{crypto, protocol::DeviceIdentity};
use anyhow::{Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{Duration, Instant};
use uuid::Uuid;

const CHALLENGE_TTL: Duration = Duration::from_secs(10);
const PROOF_LABEL: &[u8] = b"synly/session/physical-link/client/v1";
const SERVER_PROOF_LABEL: &[u8] = b"synly/session/physical-link/server/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkChallenge {
    pub session_id: Uuid,
    pub nonce: Uuid,
    pub transport: TransportKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkProof { pub challenge: LinkChallenge, pub mac: [u8; 32] }

/// 只能作为本机绑定凭据使用, 不从对端序列化数据恢复.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachedLink { session_id: Uuid, transport: TransportKind, id: Uuid }
impl AttachedLink {
    pub fn transport(self) -> TransportKind { self.transport }
    pub fn id(self) -> Uuid { self.id }
}

struct Pending { challenge: LinkChallenge, exporter: [u8; 32], created: Instant }
impl Drop for Pending { fn drop(&mut self) { self.exporter.fill(0); } }

/// 必须从已经通过应用授权的主会话建立. secret 只来自主 TLS 会话的专用 exporter.
/// candidate 身份须由应用认证提供, transport/exporter 须从本机物理连接提供, 不能采用远端声明.
pub struct SessionLinks {
    session_id: Uuid,
    peer: DeviceIdentity,
    secret: [u8; 32],
    primary: TransportKind,
    slots: [Option<Uuid>; 2],
    pending: Option<Pending>,
    closed: bool,
}
impl std::fmt::Debug for SessionLinks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SessionLinks").field("session_id", &self.session_id).field("peer_id", &self.peer.device_id).field("primary", &self.primary).field("closed", &self.closed).finish_non_exhaustive()
    }
}

fn index(transport: TransportKind) -> usize { match transport { TransportKind::Lan => 0, TransportKind::Bluetooth => 1 } }

fn mac_state(secret: &[u8; 32], challenge: LinkChallenge, peer: &DeviceIdentity, exporter: &[u8; 32], server: bool) -> Result<Hmac<Sha256>> {
    let key = STANDARD_NO_PAD.decode(peer.identity_public_key.trim())?;
    if key.len() != 32 { bail!("连接绑定身份公钥长度无效"); }
    let mut mac = Hmac::<Sha256>::new_from_slice(secret)?;
    mac.update(if server { SERVER_PROOF_LABEL } else { PROOF_LABEL });
    mac.update(challenge.session_id.as_bytes());
    mac.update(challenge.nonce.as_bytes());
    mac.update(&[index(challenge.transport) as u8]);
    mac.update(peer.device_id.as_bytes());
    mac.update(&key);
    mac.update(exporter);
    Ok(mac)
}

pub fn prove_link(secret: &[u8; 32], challenge: LinkChallenge, local: &DeviceIdentity, exporter: &[u8; 32]) -> Result<LinkProof> {
    crypto::verify_device_identity_material(local)?;
    let mac = mac_state(secret, challenge, local, exporter, false)?.finalize().into_bytes().into();
    Ok(LinkProof { challenge, mac })
}

impl SessionLinks {
    pub fn new(session_id: Uuid, peer: DeviceIdentity, primary: TransportKind, secret: [u8; 32]) -> Result<(Self, AttachedLink)> {
        if session_id.is_nil() { bail!("逻辑会话 ID 不能为空"); }
        crypto::verify_device_identity_material(&peer)?;
        let id = Uuid::new_v4();
        let mut slots = [None, None];
        slots[index(primary)] = Some(id);
        Ok((Self { session_id, peer, secret, primary, slots, pending: None, closed: false }, AttachedLink { session_id, transport: primary, id }))
    }

    pub fn challenge(&mut self, transport: TransportKind, exporter: [u8; 32], now: Instant) -> Result<LinkChallenge> {
        if self.closed { bail!("逻辑会话已关闭"); }
        if self.slots[index(transport)].is_some() { bail!("逻辑会话已经挂载此传输类型"); }
        if self.pending.as_ref().is_some_and(|pending| now.saturating_duration_since(pending.created) >= CHALLENGE_TTL) { self.pending = None; }
        if self.pending.is_some() { bail!("已有物理连接正在绑定"); }
        let challenge = LinkChallenge { session_id: self.session_id, nonce: Uuid::new_v4(), transport };
        self.pending = Some(Pending { challenge, exporter, created: now });
        Ok(challenge)
    }

    pub fn attach(&mut self, proof: LinkProof, authenticated_peer: &DeviceIdentity, transport: TransportKind, exporter: &[u8; 32], now: Instant) -> Result<AttachedLink> {
        self.attach_role(proof, authenticated_peer, transport, exporter, now, false)
    }

    pub(crate) fn attach_role(&mut self, proof: LinkProof, authenticated_peer: &DeviceIdentity, transport: TransportKind, exporter: &[u8; 32], now: Instant, server: bool) -> Result<AttachedLink> {
        if self.closed { bail!("逻辑会话已关闭"); }
        let Some(pending) = &self.pending else { bail!("没有待验证的物理连接挑战"); };
        if now.saturating_duration_since(pending.created) >= CHALLENGE_TTL {
            self.pending = None;
            bail!("物理连接挑战已过期");
        }
        if proof.challenge != pending.challenge || proof.challenge.session_id != self.session_id || proof.challenge.transport != transport || pending.exporter != *exporter {
            bail!("物理连接挑战不属于当前会话或 TLS 连接");
        }
        if self.slots[index(transport)].is_some() { bail!("重复的物理连接"); }
        if authenticated_peer.device_id != self.peer.device_id || !crypto::public_keys_match(&authenticated_peer.identity_public_key, &self.peer.identity_public_key) {
            bail!("物理连接不是已授权的同一个设备身份");
        }
        crypto::verify_device_identity_material(authenticated_peer)?;
        mac_state(&self.secret, proof.challenge, &self.peer, exporter, server)?.verify_slice(&proof.mac).map_err(|_| anyhow::anyhow!("物理连接会话绑定证明无效"))?;
        // 本机凭据独立随机, 不能采用对端 nonce, 防止重用 nonce 后迟到关闭误删新连接.
        let id = Uuid::new_v4();
        self.slots[index(transport)] = Some(id);
        self.pending = None;
        tracing::info!(session = %self.session_id, ?transport, "物理连接已绑定到现有逻辑会话");
        Ok(AttachedLink { session_id: self.session_id, transport, id })
    }

    pub(crate) fn adopt(&mut self, challenge: LinkChallenge, transport: TransportKind, exporter: [u8; 32], now: Instant) -> Result<()> {
        if self.closed || challenge.session_id != self.session_id || challenge.transport != transport || challenge.nonce.is_nil() { bail!("候选挑战不属于当前逻辑会话或物理传输"); }
        if self.slots[index(transport)].is_some() { bail!("重复的候选传输"); }
        if self.pending.as_ref().is_some_and(|pending| now.saturating_duration_since(pending.created) >= CHALLENGE_TTL) { self.pending = None; }
        if self.pending.is_some() { bail!("已有候选正在绑定"); }
        self.pending = Some(Pending { challenge, exporter, created: now });
        Ok(())
    }
    pub(crate) fn local_proof(&self, challenge: LinkChallenge, local: &DeviceIdentity, exporter: &[u8; 32], server: bool) -> Result<LinkProof> {
        if self.closed || challenge.session_id != self.session_id { bail!("本机会话已关闭或挑战属于其他会话"); }
        crypto::verify_device_identity_material(local)?;
        Ok(LinkProof { challenge, mac: mac_state(&self.secret, challenge, local, exporter, server)?.finalize().into_bytes().into() })
    }
    pub(crate) fn cancel_challenge(&mut self, challenge: LinkChallenge, exporter: &[u8; 32]) {
        if self.pending.as_ref().is_some_and(|pending| pending.challenge == challenge && &pending.exporter == exporter) { self.pending = None; }
    }
    pub fn session_id(&self) -> Uuid { self.session_id }
    pub fn contains(&self, transport: TransportKind) -> bool { !self.closed && self.slots[index(transport)].is_some() }
    pub fn primary(&self) -> TransportKind { self.primary }
    pub fn is_closed(&self) -> bool { self.closed }

    /// 旧连接的迟到关闭通知不能移除同类型的新连接. 主控制断开则结束整个会话.
    pub fn detach(&mut self, link: AttachedLink) -> bool {
        if link.session_id != self.session_id || self.slots[index(link.transport)] != Some(link.id) { return false; }
        self.slots[index(link.transport)] = None;
        if link.transport == self.primary { self.close(); }
        tracing::info!(session = %self.session_id, transport = ?link.transport, "物理连接已从逻辑会话移除");
        true
    }

    pub fn close(&mut self) { self.closed = true; self.slots = [None, None]; self.pending = None; self.secret.fill(0); }
}
impl Drop for SessionLinks {
    fn drop(&mut self) { self.secret.fill(0); }
}

pub fn export_link_master_from_client<T>(stream: &tokio_rustls::client::TlsStream<T>, session_id: Uuid) -> Result<[u8; 32]> {
    let mut output = [0; 32];
    stream.get_ref().1.export_keying_material(&mut output, b"synly/session/link-master/v1", Some(session_id.as_bytes()))?;
    Ok(output)
}
pub fn export_link_master_from_server<T>(stream: &tokio_rustls::server::TlsStream<T>, session_id: Uuid) -> Result<[u8; 32]> {
    let mut output = [0; 32];
    stream.get_ref().1.export_keying_material(&mut output, b"synly/session/link-master/v1", Some(session_id.as_bytes()))?;
    Ok(output)
}
pub fn export_candidate_from_client<T>(stream: &tokio_rustls::client::TlsStream<T>, session_id: Uuid) -> Result<[u8; 32]> {
    let mut output = [0; 32];
    stream.get_ref().1.export_keying_material(&mut output, b"synly/session/link-candidate/v1", Some(session_id.as_bytes()))?;
    Ok(output)
}
pub fn export_candidate_from_server<T>(stream: &tokio_rustls::server::TlsStream<T>, session_id: Uuid) -> Result<[u8; 32]> {
    let mut output = [0; 32];
    stream.get_ref().1.export_keying_material(&mut output, b"synly/session/link-candidate/v1", Some(session_id.as_bytes()))?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_peer() -> DeviceIdentity {
        let device = crate::identity::generate_device_config("测试设备".to_owned()).unwrap();
        DeviceIdentity { device_id: device.device_id, device_name: device.device_name.clone(), instance_name: None, identity_public_key: device.identity_public_key().unwrap().to_owned(), tls_root_certificate: crypto::device_tls_root_certificate(&device).unwrap() }
    }

    #[test]
    fn reused_remote_nonce_does_not_reuse_local_ticket_or_cancel_new_exporter() {
        let peer = test_peer(); let id = Uuid::new_v4(); let now = Instant::now();
        let (mut links, _) = SessionLinks::new(id, peer.clone(), TransportKind::Lan, [1; 32]).unwrap();
        let challenge = LinkChallenge { session_id: id, nonce: Uuid::new_v4(), transport: TransportKind::Bluetooth };
        links.adopt(challenge, TransportKind::Bluetooth, [2; 32], now).unwrap();
        let proof = links.local_proof(challenge, &peer, &[2; 32], true).unwrap();
        let old = links.attach_role(proof, &peer, TransportKind::Bluetooth, &[2; 32], now, true).unwrap();
        assert!(links.detach(old));
        links.adopt(challenge, TransportKind::Bluetooth, [3; 32], now).unwrap();
        links.cancel_challenge(challenge, &[2; 32]);
        let proof = links.local_proof(challenge, &peer, &[3; 32], true).unwrap();
        let current = links.attach_role(proof, &peer, TransportKind::Bluetooth, &[3; 32], now, true).unwrap();
        assert!(!links.detach(old)); assert!(links.contains(TransportKind::Bluetooth));
        assert!(links.detach(current));
    }

    #[test]
    fn link_proof_binds_identity_session_transport_nonce_secret_and_exporter() {
        let peer = test_peer();
        let session = Uuid::new_v4();
        let secret = [1; 32]; let exporter = [2; 32]; let now = Instant::now();
        let (mut links, _) = SessionLinks::new(session, peer.clone(), TransportKind::Lan, secret).unwrap();
        let challenge = links.challenge(TransportKind::Bluetooth, exporter, now).unwrap();
        let proof = prove_link(&secret, challenge, &peer, &exporter).unwrap();
        assert!(links.attach(proof, &peer, TransportKind::Bluetooth, &[3; 32], now).is_err());
        assert!(links.attach(proof, &peer, TransportKind::Lan, &exporter, now).is_err());
        let wrong = prove_link(&[4; 32], challenge, &peer, &exporter).unwrap();
        assert!(links.attach(wrong, &peer, TransportKind::Bluetooth, &exporter, now).is_err());
        let mut swapped = proof; swapped.challenge.session_id = Uuid::new_v4();
        assert!(links.attach(swapped, &peer, TransportKind::Bluetooth, &exporter, now).is_err());
        swapped = proof; swapped.challenge.nonce = Uuid::new_v4();
        assert!(links.attach(swapped, &peer, TransportKind::Bluetooth, &exporter, now).is_err());
        let mut impostor = peer.clone(); impostor.device_id = Uuid::new_v4();
        assert!(links.attach(proof, &impostor, TransportKind::Bluetooth, &exporter, now).is_err());
        let mut changed_key = test_peer(); changed_key.device_id = peer.device_id;
        assert!(links.attach(proof, &changed_key, TransportKind::Bluetooth, &exporter, now).is_err());
        links.attach(proof, &peer, TransportKind::Bluetooth, &exporter, now).unwrap();
        assert!(links.attach(proof, &peer, TransportKind::Bluetooth, &exporter, now).is_err());
        assert!(links.challenge(TransportKind::Bluetooth, exporter, now).is_err());
        assert_eq!(links.primary(), TransportKind::Lan);
    }

    #[test]
    fn expiry_and_stale_detach_do_not_revive_or_remove_new_links() {
        let peer = test_peer(); let secret = [1; 32]; let exporter = [2; 32]; let now = Instant::now();
        let (mut links, primary) = SessionLinks::new(Uuid::new_v4(), peer.clone(), TransportKind::Lan, secret).unwrap();
        let challenge = links.challenge(TransportKind::Bluetooth, exporter, now).unwrap();
        let expired = prove_link(&secret, challenge, &peer, &exporter).unwrap();
        assert!(links.attach(expired, &peer, TransportKind::Bluetooth, &exporter, now + CHALLENGE_TTL).is_err());
        let challenge = links.challenge(TransportKind::Bluetooth, exporter, now + CHALLENGE_TTL).unwrap();
        let proof = prove_link(&secret, challenge, &peer, &exporter).unwrap();
        let old = links.attach(proof, &peer, TransportKind::Bluetooth, &exporter, now + CHALLENGE_TTL).unwrap();
        assert!(links.detach(old));
        let challenge = links.challenge(TransportKind::Bluetooth, exporter, now + CHALLENGE_TTL).unwrap();
        let proof = prove_link(&secret, challenge, &peer, &exporter).unwrap();
        links.attach(proof, &peer, TransportKind::Bluetooth, &exporter, now + CHALLENGE_TTL).unwrap();
        assert!(!links.detach(old));
        assert!(links.contains(TransportKind::Bluetooth));
        assert!(links.detach(primary));
        assert!(links.is_closed());
        assert!(!links.contains(TransportKind::Bluetooth));
        assert!(links.challenge(TransportKind::Bluetooth, exporter, now + CHALLENGE_TTL).is_err());
    }
}
