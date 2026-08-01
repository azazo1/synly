use crate::host::SessionCapabilityProfile;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROMOTION_TIMEOUT: Duration = Duration::from_secs(30);
/// 首选活跃设备离线后的回归保留期, 与其他提升等待同值.
pub const PREFERRED_HOLD_TIMEOUT: Duration = Duration::from_secs(30);

/// 提升/保留等待超时后槽位需要执行的动作.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExpiryAction {
    /// 没有等待中的目标.
    None,
    /// 等待结束, 槽位开放给任一设备.
    SlotOpened,
    /// 等首选设备回归的保留期结束, 应提升一个临时活跃会话.
    PromoteStandIn,
}

/// host 侧唯一活跃槽位的状态机.
///
/// 活跃会话承载文件, 音频与输入能力, 其余会话仅同步剪贴板.
/// 首选设备 (preferred) 的身份跨重连与 host 重启保留: 其离线时槽位短暂保留,
/// 超时后其他设备可临时接管, 但首选设备回归时夺回活跃.
/// 配对流程串行化, 因此 reserve/claim/release 的临界区都很短.
pub struct ActiveSlot {
    active: Option<Uuid>,
    /// 粘性首选设备, 首次认领或手动切换时确立, 可持久化恢复.
    preferred: Option<Uuid>,
    reserved: Option<Uuid>,
    pending: Option<(Uuid, Instant)>,
}

pub struct ActiveSlotReserver {
    slot: Arc<Mutex<ActiveSlot>>,
}

/// 配对阶段获得的槽位预留, 会话真正建立时 claim, 配对失败时自动释放.
pub struct SlotReservation {
    slot: Arc<Mutex<ActiveSlot>>,
    device_id: Uuid,
    profile: SessionCapabilityProfile,
    claimed: bool,
}

impl ActiveSlot {
    /// 从持久化状态恢复首选设备.
    pub fn with_preferred(preferred: Option<Uuid>) -> Self {
        Self {
            active: None,
            preferred,
            reserved: None,
            pending: None,
        }
    }

    pub fn active(&self) -> Option<Uuid> {
        self.active
    }

    pub fn preferred(&self) -> Option<Uuid> {
        self.preferred
    }

    /// 配对确认设备身份后调用, 返回该连接可用的能力档位.
    ///
    /// 无活跃会话/预留/提升目标, 或设备就是提升目标或首选设备时,
    /// 才能获得全量能力.
    pub fn reserve(&mut self, device_id: Uuid) -> SessionCapabilityProfile {
        let is_pending_target = self
            .pending
            .as_ref()
            .is_some_and(|(target, _)| *target == device_id);
        let is_preferred = self.preferred == Some(device_id) && self.active != Some(device_id);
        let slot_free =
            self.active.is_none() && self.reserved.is_none() && self.pending.is_none();
        if is_pending_target || is_preferred || slot_free {
            self.reserved = Some(device_id);
            SessionCapabilityProfile::Full
        } else {
            SessionCapabilityProfile::ClipboardOnly
        }
    }

    /// 会话建立时确认预留, 返回需要降级的旧活跃会话 (如有).
    pub fn claim(&mut self, device_id: Uuid) -> Option<Uuid> {
        if self.reserved != Some(device_id) {
            return None;
        }
        self.reserved = None;
        let demote = if self
            .pending
            .as_ref()
            .is_some_and(|(target, _)| *target == device_id)
        {
            self.pending = None;
            self.active.take()
        } else if self.preferred == Some(device_id) {
            // 首选设备回归, 降级临时活跃会话.
            self.active.take()
        } else {
            None
        };
        if self.preferred.is_none() {
            self.preferred = Some(device_id);
        }
        self.active = Some(device_id);
        demote
    }

    /// 配对失败或连接被拒绝时释放预留.
    pub fn release(&mut self, device_id: Uuid) {
        if self.reserved == Some(device_id) {
            self.reserved = None;
        }
    }

    /// 会话结束时调用, 返回需要提升 (发送 Goodbye 促其重连) 的候选设备.
    ///
    /// 结束的恰是首选设备时, 槽位为其保留等待回归, 不提升其他设备.
    pub fn on_session_end(
        &mut self,
        device_id: Uuid,
        trusted_candidates: &[Uuid],
    ) -> Option<Uuid> {
        if self.active == Some(device_id) {
            self.active = None;
            if self.preferred == Some(device_id) {
                self.pending = Some((device_id, Instant::now() + PREFERRED_HOLD_TIMEOUT));
                return None;
            }
            if self.pending.is_none()
                && let Some(candidate) = trusted_candidates.first()
            {
                let candidate = *candidate;
                self.pending = Some((candidate, Instant::now() + PROMOTION_TIMEOUT));
                return Some(candidate);
            }
        } else if self.reserved == Some(device_id) {
            self.reserved = None;
        }
        None
    }

    /// 手动切换活跃会话: 目标设备成为新首选, 下次重连时认领槽位并降级旧活跃会话.
    pub fn request_switch(&mut self, target: Uuid) {
        self.preferred = Some(target);
        self.pending = Some((target, Instant::now() + PROMOTION_TIMEOUT));
    }

    /// 清除首选身份 (如撤销信任), 并取消为其保留的回归等待.
    pub fn clear_preferred(&mut self) {
        let old = self.preferred.take();
        if let Some((target, _)) = self.pending
            && old == Some(target)
        {
            self.pending = None;
        }
    }

    /// 提升/保留等待超时, 返回超时后需要执行的动作.
    pub fn pending_expired(&mut self, now: Instant) -> ExpiryAction {
        let Some((target, deadline)) = self.pending else {
            return ExpiryAction::None;
        };
        if now < deadline {
            return ExpiryAction::None;
        }
        self.pending = None;
        if self.active.is_none() && self.preferred == Some(target) {
            ExpiryAction::PromoteStandIn
        } else {
            ExpiryAction::SlotOpened
        }
    }
}

impl ActiveSlotReserver {
    pub fn new(slot: Arc<Mutex<ActiveSlot>>) -> Self {
        Self { slot }
    }

    pub fn reserve(&self, device_id: Uuid) -> SlotReservation {
        let profile = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .reserve(device_id);
        SlotReservation {
            slot: Arc::clone(&self.slot),
            device_id,
            profile,
            claimed: false,
        }
    }
}

impl SlotReservation {
    pub fn profile(&self) -> SessionCapabilityProfile {
        self.profile
    }

    pub fn claim(mut self) -> Option<Uuid> {
        self.claimed = true;
        self.slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .claim(self.device_id)
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        if !self.claimed {
            self.slot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .release(self.device_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> ActiveSlot {
        ActiveSlot::with_preferred(None)
    }

    fn make_active(slot: &mut ActiveSlot, device: Uuid) {
        assert_eq!(slot.reserve(device), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(device), None);
    }

    #[test]
    fn first_reservation_claims_full_profile_and_second_is_clipboard_only() {
        let mut slot = slot();
        assert_eq!(slot.reserve(Uuid::new_v4()), SessionCapabilityProfile::Full);
        assert_eq!(
            slot.reserve(Uuid::new_v4()),
            SessionCapabilityProfile::ClipboardOnly
        );
    }

    #[test]
    fn claim_promotes_reserved_device_to_active() {
        let mut slot = slot();
        let device = Uuid::new_v4();
        slot.reserve(device);
        assert_eq!(slot.claim(device), None);
        assert_eq!(slot.active(), Some(device));
    }

    #[test]
    fn first_claim_establishes_preferred_device() {
        let mut slot = slot();
        let device = Uuid::new_v4();
        slot.reserve(device);
        assert_eq!(slot.claim(device), None);
        assert_eq!(slot.preferred(), Some(device));
    }

    #[test]
    fn with_preferred_seeds_persisted_identity() {
        let preferred = Uuid::new_v4();
        let mut slot = ActiveSlot::with_preferred(Some(preferred));
        let stand_in = Uuid::new_v4();
        // 首选设备未在线时, 其他设备可以临时认领.
        assert_eq!(slot.reserve(stand_in), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(stand_in), None);
        assert_eq!(slot.preferred(), Some(preferred));
        // 首选设备回归时夺回活跃并降级临时设备.
        assert_eq!(slot.reserve(preferred), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(preferred), Some(stand_in));
        assert_eq!(slot.active(), Some(preferred));
    }

    #[test]
    fn duplicate_connection_for_active_preferred_stays_clipboard_only() {
        let mut slot = slot();
        let device = Uuid::new_v4();
        make_active(&mut slot, device);
        assert_eq!(slot.reserve(device), SessionCapabilityProfile::ClipboardOnly);
    }

    #[test]
    fn release_frees_reservation_for_next_device() {
        let mut slot = slot();
        let first = Uuid::new_v4();
        slot.reserve(first);
        slot.release(first);
        let second = Uuid::new_v4();
        assert_eq!(slot.reserve(second), SessionCapabilityProfile::Full);
    }

    #[test]
    fn preferred_device_reconnect_within_hold_avoids_takeover() {
        let mut slot = slot();
        let ama = Uuid::new_v4();
        let k60 = Uuid::new_v4();
        make_active(&mut slot, ama);
        // ama 离线: 槽位保留等待回归, 不提升 k60.
        assert_eq!(slot.on_session_end(ama, &[k60]), None);
        assert_eq!(slot.reserve(k60), SessionCapabilityProfile::ClipboardOnly);
        // ama 在保留期内回归: 直接恢复全量.
        assert_eq!(slot.reserve(ama), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(ama), None);
        assert_eq!(slot.active(), Some(ama));
        assert_eq!(slot.preferred(), Some(ama));
    }

    #[test]
    fn hold_expiry_promotes_stand_in_and_preferred_reclaims() {
        let mut slot = slot();
        let ama = Uuid::new_v4();
        let k60 = Uuid::new_v4();
        make_active(&mut slot, ama);
        assert_eq!(slot.on_session_end(ama, &[k60]), None);
        let now = Instant::now();
        assert_eq!(
            slot.pending_expired(now + PREFERRED_HOLD_TIMEOUT + Duration::from_secs(1)),
            ExpiryAction::PromoteStandIn
        );
        // 保留期结束后, 其他设备可以临时接管.
        assert_eq!(slot.reserve(k60), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(k60), None);
        assert_eq!(slot.active(), Some(k60));
        assert_eq!(slot.preferred(), Some(ama));
        // ama 回归时夺回活跃, k60 被降级.
        assert_eq!(slot.reserve(ama), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(ama), Some(k60));
        assert_eq!(slot.active(), Some(ama));
    }

    #[test]
    fn stand_in_session_end_promotes_earliest_trusted_candidate() {
        let mut slot = slot();
        let preferred = Uuid::new_v4();
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        make_active(&mut slot, preferred);
        assert_eq!(slot.on_session_end(preferred, &[first, second]), None);
        let now = Instant::now();
        assert_eq!(
            slot.pending_expired(now + PREFERRED_HOLD_TIMEOUT + Duration::from_secs(1)),
            ExpiryAction::PromoteStandIn
        );
        assert_eq!(slot.reserve(first), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(first), None);
        assert_eq!(slot.active(), Some(first));
        // 临时活跃会话结束: 提升最早的受信候选.
        assert_eq!(slot.on_session_end(first, &[second]), Some(second));
        assert_eq!(slot.active(), None);
        assert_eq!(slot.reserve(second), SessionCapabilityProfile::Full);
        assert_eq!(
            slot.reserve(Uuid::new_v4()),
            SessionCapabilityProfile::ClipboardOnly
        );
        assert_eq!(slot.claim(second), None);
        assert_eq!(slot.active(), Some(second));
    }

    #[test]
    fn preferred_session_end_without_candidate_holds_then_opens_slot() {
        let mut slot = slot();
        let active = Uuid::new_v4();
        make_active(&mut slot, active);
        assert_eq!(slot.on_session_end(active, &[]), None);
        let next = Uuid::new_v4();
        // 保留期内其他设备不能认领.
        assert_eq!(slot.reserve(next), SessionCapabilityProfile::ClipboardOnly);
        let now = Instant::now();
        assert_eq!(
            slot.pending_expired(now + PREFERRED_HOLD_TIMEOUT + Duration::from_secs(1)),
            ExpiryAction::PromoteStandIn
        );
        // 保留期结束后槽位开放.
        assert_eq!(slot.reserve(next), SessionCapabilityProfile::Full);
    }

    #[test]
    fn manual_switch_reserves_slot_for_target_and_demotes_old_active_on_claim() {
        let mut slot = slot();
        let old = Uuid::new_v4();
        make_active(&mut slot, old);

        let target = Uuid::new_v4();
        slot.request_switch(target);
        assert_eq!(slot.preferred(), Some(target));
        assert_eq!(slot.reserve(target), SessionCapabilityProfile::Full);
        assert_eq!(
            slot.reserve(Uuid::new_v4()),
            SessionCapabilityProfile::ClipboardOnly
        );
        assert_eq!(slot.claim(target), Some(old));
        assert_eq!(slot.active(), Some(target));
        // 旧活跃设备回归只能拿剪贴板档位.
        assert_eq!(slot.reserve(old), SessionCapabilityProfile::ClipboardOnly);
    }

    #[test]
    fn session_end_during_switch_preserves_pending_target() {
        let mut slot = slot();
        let old = Uuid::new_v4();
        make_active(&mut slot, old);
        let target = Uuid::new_v4();
        slot.request_switch(target);
        // 旧活跃会话在目标认领前断开, 不覆盖切换等待.
        assert_eq!(slot.on_session_end(old, &[Uuid::new_v4()]), None);
        assert_eq!(slot.reserve(target), SessionCapabilityProfile::Full);
        assert_eq!(slot.claim(target), None);
        assert_eq!(slot.active(), Some(target));
    }

    #[test]
    fn switch_pending_expiry_keeps_current_active() {
        let mut slot = slot();
        let old = Uuid::new_v4();
        make_active(&mut slot, old);
        let target = Uuid::new_v4();
        slot.request_switch(target);
        let now = Instant::now();
        assert_eq!(
            slot.pending_expired(now + PROMOTION_TIMEOUT + Duration::from_secs(1)),
            ExpiryAction::SlotOpened
        );
        assert_eq!(slot.active(), Some(old));
    }

    #[test]
    fn clear_preferred_removes_identity_and_pending_hold() {
        let mut slot = slot();
        let preferred = Uuid::new_v4();
        make_active(&mut slot, preferred);
        assert_eq!(slot.on_session_end(preferred, &[]), None);
        slot.clear_preferred();
        assert_eq!(slot.preferred(), None);
        let other = Uuid::new_v4();
        assert_eq!(slot.reserve(other), SessionCapabilityProfile::Full);
    }

    #[test]
    fn non_active_session_end_does_not_disturb_slot() {
        let mut slot = slot();
        let active = Uuid::new_v4();
        slot.reserve(active);
        slot.claim(active);

        let bystander = Uuid::new_v4();
        assert_eq!(slot.on_session_end(bystander, &[]), None);
        assert_eq!(slot.active(), Some(active));
    }

    #[test]
    fn reservation_guard_releases_on_drop() {
        let slot = Arc::new(Mutex::new(ActiveSlot::with_preferred(None)));
        let reserver = ActiveSlotReserver::new(Arc::clone(&slot));
        let device = Uuid::new_v4();
        {
            let reservation = reserver.reserve(device);
            assert_eq!(reservation.profile(), SessionCapabilityProfile::Full);
        }
        let other = Uuid::new_v4();
        assert_eq!(
            slot.lock().unwrap().reserve(other),
            SessionCapabilityProfile::Full
        );
    }
}
