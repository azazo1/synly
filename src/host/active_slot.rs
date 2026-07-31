use crate::host::SessionCapabilityProfile;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const PROMOTION_TIMEOUT: Duration = Duration::from_secs(30);

/// host 侧唯一活跃槽位的状态机.
///
/// 活跃会话承载文件, 音频与输入能力, 其余会话仅同步剪贴板.
/// 配对流程串行化, 因此 reserve/claim/release 的临界区都很短.
pub struct ActiveSlot {
    active: Option<Uuid>,
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
    pub fn new() -> Self {
        Self {
            active: None,
            reserved: None,
            pending: None,
        }
    }

    pub fn active(&self) -> Option<Uuid> {
        self.active
    }

    /// 配对确认设备身份后调用, 返回该连接可用的能力档位.
    ///
    /// 只有无活跃会话, 无预留且无提升目标 (或该设备就是提升目标) 时,
    /// 才能获得全量能力.
    pub fn reserve(&mut self, device_id: Uuid) -> SessionCapabilityProfile {
        let is_pending_target = self
            .pending
            .as_ref()
            .is_some_and(|(target, _)| *target == device_id);
        let slot_free =
            self.active.is_none() && self.reserved.is_none() && self.pending.is_none();
        if is_pending_target || slot_free {
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
        } else {
            None
        };
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
    pub fn on_session_end(
        &mut self,
        device_id: Uuid,
        trusted_candidates: &[Uuid],
    ) -> Option<Uuid> {
        if self.active == Some(device_id) {
            self.active = None;
            if let Some(candidate) = trusted_candidates.first() {
                let candidate = *candidate;
                self.pending = Some((candidate, Instant::now() + PROMOTION_TIMEOUT));
                return Some(candidate);
            }
        } else if self.reserved == Some(device_id) {
            self.reserved = None;
        }
        None
    }

    /// 手动切换活跃会话: 目标设备下次重连时认领槽位并降级旧活跃会话.
    pub fn request_switch(&mut self, target: Uuid) {
        self.pending = Some((target, Instant::now() + PROMOTION_TIMEOUT));
    }

    /// 提升等待超时后开放槽位, 返回是否发生过期.
    pub fn pending_expired(&mut self, now: Instant) -> bool {
        if let Some((_, deadline)) = self.pending
            && now >= deadline
        {
            self.pending = None;
            return true;
        }
        false
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
        ActiveSlot::new()
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
    fn release_frees_reservation_for_next_device() {
        let mut slot = slot();
        let first = Uuid::new_v4();
        slot.reserve(first);
        slot.release(first);
        let second = Uuid::new_v4();
        assert_eq!(slot.reserve(second), SessionCapabilityProfile::Full);
    }

    #[test]
    fn active_session_end_promotes_earliest_trusted_candidate() {
        let mut slot = slot();
        let active = Uuid::new_v4();
        slot.reserve(active);
        slot.claim(active);

        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let promoted = slot.on_session_end(active, &[first, second]);

        assert_eq!(promoted, Some(first));
        assert_eq!(slot.active(), None);
        assert_eq!(slot.reserve(first), SessionCapabilityProfile::Full);
        assert_eq!(
            slot.reserve(second),
            SessionCapabilityProfile::ClipboardOnly
        );
        assert_eq!(slot.claim(first), None);
        assert_eq!(slot.active(), Some(first));
    }

    #[test]
    fn active_session_end_without_candidate_leaves_slot_open() {
        let mut slot = slot();
        let active = Uuid::new_v4();
        slot.reserve(active);
        slot.claim(active);

        assert_eq!(slot.on_session_end(active, &[]), None);
        let next = Uuid::new_v4();
        assert_eq!(slot.reserve(next), SessionCapabilityProfile::Full);
    }

    #[test]
    fn manual_switch_reserves_slot_for_target_and_demotes_old_active_on_claim() {
        let mut slot = slot();
        let old = Uuid::new_v4();
        slot.reserve(old);
        slot.claim(old);

        let target = Uuid::new_v4();
        slot.request_switch(target);
        assert_eq!(slot.reserve(target), SessionCapabilityProfile::Full);
        assert_eq!(
            slot.reserve(Uuid::new_v4()),
            SessionCapabilityProfile::ClipboardOnly
        );
        assert_eq!(slot.claim(target), Some(old));
        assert_eq!(slot.active(), Some(target));
    }

    #[test]
    fn pending_expiry_opens_slot_to_other_devices() {
        let mut slot = slot();
        let active = Uuid::new_v4();
        slot.reserve(active);
        slot.claim(active);

        let candidate = Uuid::new_v4();
        assert_eq!(slot.on_session_end(active, &[candidate]), Some(candidate));
        let now = Instant::now();
        assert!(slot.pending_expired(now + PROMOTION_TIMEOUT + Duration::from_secs(1)));

        let newcomer = Uuid::new_v4();
        assert_eq!(slot.reserve(newcomer), SessionCapabilityProfile::Full);
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
        let slot = Arc::new(Mutex::new(ActiveSlot::new()));
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
