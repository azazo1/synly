use super::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, Error, Handle, IAudioClient,
    Result, WasapiSpec, check_hresult,
};
use std::ptr;
use super::{CoTaskMemFree, WaveFormatEx, format::{EXTENSIBLE, WaveFormatExtensible}};

struct MixFormat(*mut WaveFormatEx);
impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(self.0.cast()); }
    }
}

fn device_format(client: *mut IAudioClient, spec: WasapiSpec) -> Result<WaveFormatExtensible> {
    let mut format = spec.wave_format()?;
    let mut mix = MixFormat(ptr::null_mut());
    unsafe {
        check_hresult(((*(*client).lp_vtbl).get_mix_format)(client, &mut mix.0), "IAudioClient::GetMixFormat")?;
        if mix.0.is_null() {
            return Err(Error::Backend("IAudioClient::GetMixFormat 返回空接口".into()));
        }
        apply_mix_format(&mut format, mix.0);
    }
    Ok(format)
}

// 指针及 cbSize 声明的存储由 WASAPI 保证有效. 非扩展或短 header 不读取扩展字段.
unsafe fn apply_mix_format(format: &mut WaveFormatExtensible, mix: *const WaveFormatEx) {
    let header = unsafe { mix.read_unaligned() };
    if header.w_format_tag == EXTENSIBLE && header.cb_size >= 22 {
        let mask = unsafe { ptr::addr_of!((*mix.cast::<WaveFormatExtensible>()).channel_mask).read_unaligned() };
        if !format.prefer_native_mask(header.n_channels, mask) {
            tracing::debug!(native_mask = mask, "保留标准声道布局, 由 Windows 音频引擎转换");
        }
    }
}

// shared event 模式要求两个时长参数均为 0, 实际缓冲由音频引擎周期决定.
// 先绑定 event 再查询参数, 避免后续查询失败时销毁客户端等待未设置的 event.
pub(super) fn initialize_shared_client(
    client: *mut IAudioClient,
    flags: u32,
    event: Handle,
    spec: WasapiSpec,
    direction: &'static str,
) -> Result<u32> {
    if flags & AUDCLNT_STREAMFLAGS_EVENTCALLBACK == 0 {
        return Err(Error::Backend("WASAPI 初始化缺少事件回调标志".into()));
    }
    let format = device_format(client, spec)?;
    let mut buffer_frames = 0;
    let mut period_hns = 0;
    let mut latency_hns = 0;
    unsafe {
        let vtbl = &*(*client).lp_vtbl;
        check_hresult(
            (vtbl.initialize)(client, AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &format.format, ptr::null()),
            "IAudioClient::Initialize(shared event)",
        )?;
        check_hresult((vtbl.set_event_handle)(client, event), "IAudioClient::SetEventHandle")?;
        check_hresult((vtbl.get_buffer_size)(client, &mut buffer_frames), "IAudioClient::GetBufferSize")?;
        check_hresult(
            (vtbl.get_device_period)(client, &mut period_hns, ptr::null_mut()),
            "IAudioClient::GetDevicePeriod",
        )?;
        check_hresult((vtbl.get_stream_latency)(client, &mut latency_hns), "IAudioClient::GetStreamLatency")?;
    }
    if buffer_frames == 0 || period_hns <= 0 || latency_hns < 0 {
        return Err(Error::Backend(format!(
            "WASAPI 返回无效缓冲参数: frames={buffer_frames}, period_hns={period_hns}, latency_hns={latency_hns}"
        )));
    }
    let channel_mask = format.channel_mask;
    tracing::info!(
        direction,
        channels = spec.channels,
        channel_mask,
        sample_rate = spec.sample_rate,
        buffer_frames,
        buffer_ms = f64::from(buffer_frames) * 1000.0 / f64::from(spec.sample_rate),
        period_ms = period_hns as f64 / 10_000.0,
        latency_ms = latency_hns as f64 / 10_000.0,
        "已初始化 WASAPI 共享事件流"
    );
    Ok(buffer_frames)
}

#[cfg(test)]
mod tests;
