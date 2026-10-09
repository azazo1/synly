//! LAN 免 PIN 候选连接的临时准入, 主会话关闭后授权立即失效.
use super::*;

pub(crate) struct LanAdmission {
    pub active: Vec<synly_core::transport::logical::LogicalSession>,
    pub full: bool,
}
impl LanAdmission {
    fn existing(&self, id: Uuid) -> Option<&synly_core::transport::logical::LogicalSession> {
        self.active.iter().find(|session| session.peer().device_id == id && session.is_open())
    }
    pub fn trust_devices(&self, saved: &[TrustedDeviceConfig]) -> Vec<TrustedDeviceConfig> {
        let mut trusted = saved.to_vec();
        for session in &self.active {
            if !session.is_open() { continue; }
            let peer = session.peer(); trusted.retain(|known| known.device_id != peer.device_id);
            if session.has(TransportKind::Lan) { continue; }
            trusted.push(TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name.clone(), public_key: peer.identity_public_key.clone(), tls_root_certificate: peer.tls_root_certificate.clone(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 });
        }
        trusted
    }
    /// 返回 trusted 及是否只能加入原会话. 已在线身份不允许另一个密钥或重复 LAN.
    pub fn resolve(&self, peer: &DeviceIdentity, saved: &[TrustedDeviceConfig]) -> Result<(TrustedDeviceConfig, bool)> {
        if let Some(existing) = self.existing(peer.device_id) {
            if !existing.matches(peer) || existing.has(TransportKind::Lan) { bail!("在线设备身份不匹配或 LAN 已经挂载"); }
            let trusted = self.trust_devices(&[]).into_iter().find(|known| known.device_id == peer.device_id).context("候选主会话授权已关闭")?;
            return Ok((trusted, true));
        }
        // TLS 建立后主会话已经关闭时, 不把握手开始时的临时根用于建立主会话.
        if self.active.iter().any(|session| session.peer().device_id == peer.device_id) { bail!("候选主会话已经结束, 临时授权无效"); }
        if self.full { bail!("host 会话已满, 仅允许现有设备候选连接"); }
        let known = saved.iter().find(|known| known.device_id == peer.device_id && crypto::public_keys_match(&known.public_key, &peer.identity_public_key)).context("设备没有可用的持久 mTLS 信任")?;
        Ok((known.clone(), false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity(name: &str) -> DeviceIdentity {
        let device = synly_core::identity::generate_device_config(name.to_owned()).unwrap();
        device_identity(&device, None)
    }
    fn session(primary: TransportKind) -> synly_core::transport::logical::LogicalSession {
        synly_core::transport::logical::LogicalSession::new(Uuid::new_v4(), identity("host"), identity("client"), primary, [7; 32]).unwrap()
    }
    fn saved(peer: &DeviceIdentity) -> TrustedDeviceConfig {
        TrustedDeviceConfig { device_id: peer.device_id, device_name: peer.device_name.clone(), public_key: peer.identity_public_key.clone(), tls_root_certificate: peer.tls_root_certificate.clone(), trusted_at_ms: 0, last_seen_ms: 0, successful_sessions: 0 }
    }
    #[test]
    fn full_host_allows_only_same_live_bluetooth_identity_and_does_not_persist_it() {
        let logical = session(TransportKind::Bluetooth); let owner = logical.owner().unwrap();
        let admission = LanAdmission { active: vec![logical.clone()], full: true };
        let saved = vec![];
        let (trust, candidate) = admission.resolve(logical.peer(), &saved).unwrap();
        assert!(candidate); assert_eq!(trust.public_key, logical.peer().identity_public_key); assert!(saved.is_empty());
        assert!(admission.resolve(&identity("unrelated"), &saved).is_err());
        let mut changed = identity("changed"); changed.device_id = logical.peer().device_id;
        assert!(admission.resolve(&changed, &saved).is_err());
        drop(owner);
        assert!(admission.trust_devices(&saved).is_empty());
        assert!(admission.resolve(logical.peer(), &saved).is_err());
    }
    #[test]
    fn temporary_authorization_cannot_become_primary_even_when_capacity_is_free() {
        let logical = session(TransportKind::Bluetooth); let owner = logical.owner().unwrap();
        let admission = LanAdmission { active: vec![logical.clone()], full: false };
        let persistent = vec![saved(logical.peer())];
        assert!(admission.resolve(logical.peer(), &persistent).unwrap().1);
        drop(owner);
        assert!(admission.resolve(logical.peer(), &persistent).is_err());
        // 新的准入快照可以按持久信任建立新主会话, 旧握手快照不可以.
        let fresh = LanAdmission { active: vec![], full: false };
        assert!(!fresh.resolve(logical.peer(), &persistent).unwrap().1);
    }
    #[test]
    fn duplicate_lan_is_excluded_from_tls_roots_and_cannot_enter_a_second_session() {
        let logical = session(TransportKind::Lan); let _owner = logical.owner().unwrap();
        let persistent = vec![saved(logical.peer())];
        let admission = LanAdmission { active: vec![logical.clone()], full: false };
        assert!(admission.trust_devices(&persistent).is_empty());
        assert!(admission.resolve(logical.peer(), &persistent).is_err());
    }
}
