//! WASAPI endpoint 选择与默认跟随策略. 固定 ID 不回退到默认设备.
use super::{check_hresult, ComPtr, IMMDevice, IMMDeviceEnumerator, E_CONSOLE, E_RENDER, DEVICE_REBIND_POLL_MS};
use crate::audio::error::{Error, Result};
use std::ptr;
use std::time::{Duration, Instant};

const DEVICE_STATE_ACTIVE: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum EndpointSelection {
    Default,
    // 保留 Windows 的不透明 ID, 不 trim, 不把友好名称猜成 endpoint ID.
    Fixed(Vec<u16>),
}

impl EndpointSelection {
    pub(super) fn parse(device: Option<&str>) -> Result<Self> {
        match device {
            None => Ok(Self::Default),
            Some(id) if id.is_empty() || id.contains('\0') => {
                Err(Error::InvalidConfig("Windows endpoint ID 不能为空或包含 NUL"))
            }
            Some(id) => Ok(Self::Fixed(id.encode_utf16().chain(Some(0)).collect())),
        }
    }

    pub(super) fn follows_default(&self) -> bool { matches!(self, Self::Default) }
}

// 调用方持有 COM apartment 与 enumerator. 测试使用内存 vtable, 不创建真实设备.
pub(super) fn select(enumerator: &ComPtr<IMMDeviceEnumerator>, selection: &EndpointSelection) -> Result<ComPtr<IMMDevice>> {
    let mut raw = ptr::null_mut();
    let enumerator = enumerator.as_ptr();
    let (status, operation) = unsafe {
        match selection {
            EndpointSelection::Default => (
                ((*(*enumerator).lp_vtbl).get_default_audio_endpoint)(enumerator, E_RENDER, E_CONSOLE, &mut raw),
                "IMMDeviceEnumerator::GetDefaultAudioEndpoint",
            ),
            EndpointSelection::Fixed(id) => (
                ((*(*enumerator).lp_vtbl).get_device)(enumerator, id.as_ptr(), &mut raw),
                "IMMDeviceEnumerator::GetDevice",
            ),
        }
    };
    check_hresult(status, operation)?;
    let device = ComPtr::<IMMDevice>::from_raw(raw)?;
    let mut state = 0;
    unsafe {
        check_hresult(((*(*device.as_ptr()).lp_vtbl).get_state)(device.as_ptr(), &mut state), "IMMDevice::GetState")?;
    }
    // Sunshine get_sink_device:969-971 同样要求显式 endpoint 为 ACTIVE.
    if state != DEVICE_STATE_ACTIVE {
        return Err(Error::Backend(format!("Windows 音频 endpoint 当前不可用, state=0x{state:x}")));
    }
    Ok(device)
}

// 固定设备连默认 ID 都不查询, 包括默认设备消失/查询失败时. 当前设备自身的失效
// 仍由 WASAPI 错误触发重建, 重建继续使用同一个 EndpointSelection.
pub(super) fn should_rebind(
    bound_default_id: Option<&str>, last_check: &mut Instant, now: Instant,
    current: impl FnOnce() -> Result<String>,
) -> bool {
    let Some(bound) = bound_default_id else { return false; };
    if now.saturating_duration_since(*last_check) < Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS)) {
        return false;
    }
    *last_check = now;
    current().map_or(true, |id| id != bound)
}

#[cfg(test)]
mod tests;
