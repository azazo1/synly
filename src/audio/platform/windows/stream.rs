use super::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, Error, Handle, IAudioClient,
    Result, WasapiSpec, check_hresult,
};
use std::ptr;

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
    let format = spec.wave_format();
    let mut buffer_frames = 0;
    let mut period_hns = 0;
    let mut latency_hns = 0;
    unsafe {
        let vtbl = &*(*client).lp_vtbl;
        check_hresult(
            (vtbl.initialize)(client, AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &format, ptr::null()),
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
    tracing::info!(
        direction,
        sample_rate = spec.sample_rate,
        buffer_frames,
        buffer_ms = f64::from(buffer_frames) * 1000.0 / f64::from(spec.sample_rate),
        period_ms = period_hns as f64 / 10_000.0,
        latency_ms = latency_hns as f64 / 10_000.0,
        "已初始化 WASAPI 共享事件流"
    );
    Ok(buffer_frames)
}
