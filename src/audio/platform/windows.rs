use crate::audio::capture::{AudioInput, CaptureStatus};
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use std::ffi::c_void;
use std::ptr;
use std::slice;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

mod budget;
mod diagnostics;
mod endpoint;
mod format;
mod scheduling;
mod stream;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod queue_tests;

use budget::{MAX_PLAYBACK_WAIT, QueueBudget};
use diagnostics::CaptureDiagnostics;
use endpoint::EndpointSelection;
use scheduling::MmcssTask;

const CLSCTX_ALL: u32 = 23;
const COINIT_MULTITHREADED: u32 = 0;
const WAIT_OBJECT_0: u32 = 0;
const WAIT_FAILED: u32 = 0xFFFF_FFFF;
const WAIT_TIMEOUT: u32 = 258;
const FALSE: i32 = 0;
const DEVICE_REBIND_POLL_MS: u32 = 500;
const DEVICE_RETRY_BACKOFF_MS: u32 = 500;

const AUDCLNT_SHAREMODE_SHARED: u32 = 0;
const AUDCLNT_STREAMFLAGS_LOOPBACK: u32 = 0x0002_0000;
const AUDCLNT_STREAMFLAGS_EVENTCALLBACK: u32 = 0x0004_0000;
const AUDCLNT_STREAMFLAGS_NOPERSIST: u32 = 0x0008_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY: u32 = 0x1;
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;
const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000Fu32 as i32;
const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010u32 as i32;
const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026u32 as i32;

const E_RENDER: u32 = 0;
const E_CONSOLE: u32 = 0;

const CLSID_MMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xBCDE_0395,
    0xE52F,
    0x467C,
    [0x8E, 0x3D, 0xC4, 0x57, 0x92, 0x91, 0x69, 0x2E],
);
const IID_IMMDEVICE_ENUMERATOR: Guid = Guid::new(
    0xA956_64D2,
    0x9614,
    0x4F35,
    [0xA7, 0x46, 0xDE, 0x8D, 0xB6, 0x36, 0x17, 0xE6],
);
const IID_IAUDIO_CLIENT: Guid = Guid::new(
    0x1CB9_AD4C,
    0xDBFA,
    0x4C32,
    [0xB1, 0x78, 0xC2, 0xF5, 0x68, 0xA7, 0x03, 0xB2],
);
const IID_IAUDIO_RENDER_CLIENT: Guid = Guid::new(
    0xF294_ACFC,
    0x3146,
    0x4483,
    [0xA7, 0xBF, 0xAD, 0xDC, 0xA7, 0xC2, 0x60, 0xE2],
);
const IID_IAUDIO_CAPTURE_CLIENT: Guid = Guid::new(
    0xC8AD_BD64,
    0xE71E,
    0x48A0,
    [0xA4, 0xDE, 0x18, 0x5C, 0x39, 0x5C, 0xD3, 0x17],
);

pub fn open_input(config: &CaptureConfig, stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    let endpoint = EndpointSelection::parse(config.device_name.as_deref())?;
    validate_stream(stream)?;

    let budget = QueueBudget::from_stream(stream)?;
    let ring = Arc::new(SharedSampleRing::new(
        budget.capture_samples,
        budget.channels,
        budget.frame_samples,
        budget.capture_samples,
        "windows capture backend has been closed",
    )?);
    let stop_event = OwnedHandle::create_manual_reset(false)?;
    let thread = spawn_capture_thread(
        stop_event.raw(),
        Arc::clone(&ring),
        WasapiSpec::from_stream(stream),
        endpoint,
    )?;

    Ok(Box::new(WindowsInput {
        ring,
        stop_event,
        thread: Some(thread),
    }))
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    let endpoint = EndpointSelection::parse(config.device_name.as_deref())?;
    validate_stream(stream)?;

    let budget = QueueBudget::from_stream(stream)?;
    let ring = Arc::new(SharedSampleRing::new(
        budget.playback_samples,
        budget.channels,
        budget.frame_samples,
        budget.playback_watermark,
        "windows playback backend has been closed",
    )?);
    let stop_event = OwnedHandle::create_manual_reset(false)?;
    let thread = spawn_playback_thread(
        stop_event.raw(),
        Arc::clone(&ring),
        WasapiSpec::from_stream(stream),
        endpoint,
    )?;

    Ok(Box::new(WindowsOutput {
        ring,
        stop_event,
        thread: Some(thread),
    }))
}

fn validate_stream(stream: &StreamParams) -> Result<()> {
    WasapiSpec::from_stream(stream).wave_format()?;
    Ok(())
}

struct WindowsInput {
    ring: Arc<SharedSampleRing>,
    stop_event: OwnedHandle,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WindowsInput {
    fn drop(&mut self) {
        let _ = self.stop_event.set();
        self.ring.close(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioInput for WindowsInput {
    fn read_frame(&mut self, frame: &mut [f32], timeout: Duration) -> Result<CaptureStatus> {
        if frame.len() != self.ring.frame_samples {
            return Err(Error::Backend("Windows 捕获缓冲必须为完整协商帧".into()));
        }
        if self.ring.read_exact(frame, timeout)? {
            Ok(CaptureStatus::Ok)
        } else {
            Ok(CaptureStatus::Timeout)
        }
    }
}

struct WindowsOutput {
    ring: Arc<SharedSampleRing>,
    stop_event: OwnedHandle,
    thread: Option<JoinHandle<()>>,
}

impl Drop for WindowsOutput {
    fn drop(&mut self) {
        let _ = self.stop_event.set();
        self.ring.close(None);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl AudioOutput for WindowsOutput {
    fn submit_frame(&mut self, frame: &[f32], timeout: Duration) -> Result<()> {
        self.ring.write_blocking(frame, timeout)
    }
}

#[derive(Clone, Copy)]
struct WasapiSpec {
    sample_rate: u32,
    channels: u16,
}

impl WasapiSpec {
    fn from_stream(stream: &StreamParams) -> Self {
        Self {
            sample_rate: stream.sample_rate,
            channels: stream.channels as u16,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ThreadRunState {
    Stop,
    Restart,
}

fn spawn_capture_thread(
    stop_event: Handle,
    ring: Arc<SharedSampleRing>,
    spec: WasapiSpec,
    endpoint: EndpointSelection,
) -> Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("audio-relay-win-capture".into())
        .spawn(move || {
            capture_thread_main(stop_event, ring, spec, endpoint, ready_tx);
        })
        .map_err(|err| Error::Backend(format!("failed to spawn Windows capture thread: {err}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(message)) => {
            let _ = thread.join();
            Err(Error::Backend(message))
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Backend(
                "Windows capture thread exited before backend startup completed".into(),
            ))
        }
    }
}

fn spawn_playback_thread(
    stop_event: Handle,
    ring: Arc<SharedSampleRing>,
    spec: WasapiSpec,
    endpoint: EndpointSelection,
) -> Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("audio-relay-win-playback".into())
        .spawn(move || {
            playback_thread_main(stop_event, ring, spec, endpoint, ready_tx);
        })
        .map_err(|err| Error::Backend(format!("failed to spawn Windows playback thread: {err}")))?;

    match ready_rx.recv() {
        Ok(Ok(())) => Ok(thread),
        Ok(Err(message)) => {
            let _ = thread.join();
            Err(Error::Backend(message))
        }
        Err(_) => {
            let _ = thread.join();
            Err(Error::Backend(
                "Windows playback thread exited before backend startup completed".into(),
            ))
        }
    }
}

fn capture_thread_main(
    stop_event: Handle,
    ring: Arc<SharedSampleRing>,
    spec: WasapiSpec,
    endpoint: EndpointSelection,
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) {
    let _mmcss = MmcssTask::register();
    let mut diagnostics = CaptureDiagnostics::default();
    let mut ready_sent = false;
    loop {
        match CaptureThreadContext::start(spec, &endpoint) {
            Ok(context) => {
                if let Err(err) = ring.configure_capture_packet(context.buffer_frames, spec.sample_rate) {
                    let message = err.to_string();
                    if !ready_sent {
                        let _ = ready_tx.send(Err(message.clone()));
                    }
                    ring.close(Some(message));
                    return;
                }
                if ready_sent {
                    if let Err(err) = ring.finish_recovery() {
                        ring.close(Some(err.to_string()));
                        return;
                    }
                    tracing::info!("Windows 音频捕获设备已重建, 旧音频已丢弃");
                }
                if !ready_sent {
                    let _ = ready_tx.send(Ok(()));
                    ready_sent = true;
                }

                match context.run(stop_event, &ring, &mut diagnostics) {
                    Ok(ThreadRunState::Stop) => return,
                    Ok(ThreadRunState::Restart) => {
                        match ring.begin_recovery() {
                            Ok(discarded_samples) => {
                                tracing::info!(discarded_samples, "Windows 音频设备需要重建, 清空队列并暂停接收音频");
                            }
                            Err(err) => {
                                ring.close(Some(err.to_string()));
                                return;
                            }
                        }
                        continue;
                    }
                    Err(err) => {
                        if !ready_sent {
                            let message = err.to_string();
                            let _ = ready_tx.send(Err(message.clone()));
                            ring.close(Some(message));
                            return;
                        }
                        ring.close(Some(err.to_string()));
                        return;
                    }
                }
            }
            Err(err) => {
                if !ready_sent {
                    let message = err.to_string();
                    let _ = ready_tx.send(Err(message.clone()));
                    ring.close(Some(message));
                    return;
                }

                match wait_for_stop_or_timeout(stop_event, DEVICE_RETRY_BACKOFF_MS) {
                    Ok(true) => return,
                    Ok(false) => continue,
                    Err(wait_err) => {
                        ring.close(Some(wait_err.to_string()));
                        return;
                    }
                }
            }
        }
    }
}

fn playback_thread_main(
    stop_event: Handle,
    ring: Arc<SharedSampleRing>,
    spec: WasapiSpec,
    endpoint: EndpointSelection,
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) {
    let _mmcss = MmcssTask::register();
    match PlaybackThreadContext::start(spec, &endpoint) {
        Ok(context) => {
            let _ = ready_tx.send(Ok(()));
            finish_playback_stream(context.run(stop_event, &ring), &ring);
        }
        Err(error) => {
            let message = error.to_string();
            let _ = ready_tx.send(Err(message.clone()));
            ring.close(Some(message));
        }
    }
}

fn finish_playback_stream(result: Result<ThreadRunState>, ring: &SharedSampleRing) {
    // 播放设备的整个生命周期由 runtime/render 负责, 不能在这里偷偷重建,
    // 否则上层会保留旧 Opus 解码状态, 也不会执行恢复丢帧窗口.
    match result {
        Ok(ThreadRunState::Stop) => ring.close(None),
        Ok(ThreadRunState::Restart) => ring.close(Some("Windows 播放设备请求重建".into())),
        Err(error) => ring.close(Some(error.to_string())),
    }
}

struct CaptureThreadContext {
    audio_client: ComPtr<IAudioClient>,
    capture_client: ComPtr<IAudioCaptureClient>,
    capture_event: OwnedHandle,
    endpoint_id: Option<String>,
    channels: usize,
    buffer_frames: u32,
    // Rust 按字段声明顺序析构, COM apartment 必须晚于所有接口释放.
    _com: ComApartment,
}

impl CaptureThreadContext {
    fn start(spec: WasapiSpec, endpoint: &EndpointSelection) -> Result<Self> {
        let com = ComApartment::new()?;
        let activated = activate_audio_client(endpoint)?;
        let audio_client = activated.audio_client;
        let capture_event = OwnedHandle::create_auto_reset(false)?;
        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        let buffer_frames = stream::initialize_shared_client(
            audio_client.as_ptr(),
            stream_flags,
            capture_event.raw(),
            spec,
            "capture",
        )?;

        let capture_client = get_service::<IAudioCaptureClient>(
            audio_client.as_ptr(),
            &IID_IAUDIO_CAPTURE_CLIENT,
            "IAudioClient::GetService(IAudioCaptureClient)",
        )?;

        unsafe {
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).start)(audio_client.as_ptr()),
                "IAudioClient::Start(loopback)",
            )?;
        }

        Ok(Self {
            audio_client,
            capture_client,
            capture_event,
            endpoint_id: activated.endpoint_id,
            channels: spec.channels as usize,
            buffer_frames,
            _com: com,
        })
    }

    fn run(
        self,
        stop_event: Handle,
        ring: &SharedSampleRing,
        diagnostics: &mut CaptureDiagnostics,
    ) -> Result<ThreadRunState> {
        capture_loop(
            self.capture_client.as_ptr(),
            self.capture_event.raw(),
            stop_event,
            ring,
            self.endpoint_id.as_deref(),
            self.channels,
            diagnostics,
        )
    }
}

impl Drop for CaptureThreadContext {
    fn drop(&mut self) {
        unsafe {
            let _ = ((*(*self.audio_client.as_ptr()).lp_vtbl).stop)(self.audio_client.as_ptr());
        }
    }
}

struct PlaybackThreadContext {
    audio_client: ComPtr<IAudioClient>,
    render_client: ComPtr<IAudioRenderClient>,
    render_event: OwnedHandle,
    endpoint_id: Option<String>,
    channels: usize,
    buffer_frames: u32,
    // Rust 按字段声明顺序析构, COM apartment 必须晚于所有接口释放.
    _com: ComApartment,
}

impl PlaybackThreadContext {
    fn start(spec: WasapiSpec, endpoint: &EndpointSelection) -> Result<Self> {
        let com = ComApartment::new()?;
        let activated = activate_audio_client(endpoint)?;
        let audio_client = activated.audio_client;
        let render_event = OwnedHandle::create_auto_reset(false)?;
        let stream_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        let buffer_frames = stream::initialize_shared_client(
            audio_client.as_ptr(),
            stream_flags,
            render_event.raw(),
            spec,
            "render",
        )?;

        let render_client = get_service::<IAudioRenderClient>(
            audio_client.as_ptr(),
            &IID_IAUDIO_RENDER_CLIENT,
            "IAudioClient::GetService(IAudioRenderClient)",
        )?;

        prime_render_buffer(
            render_client.as_ptr(),
            buffer_frames,
            spec.channels as usize,
        )?;

        unsafe {
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).start)(audio_client.as_ptr()),
                "IAudioClient::Start(render)",
            )?;
        }

        Ok(Self {
            audio_client,
            render_client,
            render_event,
            endpoint_id: activated.endpoint_id,
            channels: spec.channels as usize,
            buffer_frames,
            _com: com,
        })
    }

    fn run(self, stop_event: Handle, ring: &SharedSampleRing) -> Result<ThreadRunState> {
        playback_loop(
            self.audio_client.as_ptr(),
            self.render_client.as_ptr(),
            self.render_event.raw(),
            stop_event,
            ring,
            self.endpoint_id.as_deref(),
            self.channels,
            self.buffer_frames,
        )
    }
}

impl Drop for PlaybackThreadContext {
    fn drop(&mut self) {
        unsafe {
            let _ = ((*(*self.audio_client.as_ptr()).lp_vtbl).stop)(self.audio_client.as_ptr());
        }
    }
}

fn capture_loop(
    capture_client: *mut IAudioCaptureClient,
    capture_event: Handle,
    stop_event: Handle,
    ring: &SharedSampleRing,
    endpoint_id: Option<&str>,
    channels: usize,
    diagnostics: &mut CaptureDiagnostics,
) -> Result<ThreadRunState> {
    let handles = [stop_event, capture_event];
    let mut last_endpoint_check =
        Instant::now() - Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS));
    loop {
        if endpoint::should_rebind(endpoint_id, &mut last_endpoint_check, Instant::now(), current_default_render_endpoint_id) {
            return Ok(ThreadRunState::Restart);
        }

        match wait_for_multiple_objects_timeout(&handles, DEVICE_REBIND_POLL_MS)? {
            Some(0) => return Ok(ThreadRunState::Stop),
            Some(1) => {}
            None => continue,
            index => {
                return Err(Error::Backend(format!(
                    "unexpected wait result from loopback capture thread: {index:?}"
                )));
            }
        }

        loop {
            let mut packet_frames = 0u32;
            let hr = unsafe {
                ((*(*capture_client).lp_vtbl).get_next_packet_size)(
                    capture_client,
                    &mut packet_frames,
                )
            };
            if should_restart_audio_client(hr) {
                return Ok(ThreadRunState::Restart);
            }
            check_hresult(hr, "IAudioCaptureClient::GetNextPacketSize")?;
            if packet_frames == 0 {
                break;
            }

            let mut data_ptr = ptr::null_mut();
            let mut frames_available = packet_frames;
            let mut flags = 0u32;
            let hr = unsafe {
                ((*(*capture_client).lp_vtbl).get_buffer)(
                    capture_client,
                    &mut data_ptr,
                    &mut frames_available,
                    &mut flags,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if should_restart_audio_client(hr) {
                return Ok(ThreadRunState::Restart);
            }
            check_hresult(hr, "IAudioCaptureClient::GetBuffer")?;

            let sample_count = frames_available as usize * channels;
            if flags & AUDCLNT_BUFFERFLAGS_SILENT != 0 {
                ring.write_silence_overwrite(sample_count);
            } else if !data_ptr.is_null() && sample_count > 0 {
                let samples =
                    unsafe { slice::from_raw_parts(data_ptr as *const f32, sample_count) };
                ring.write_overwrite(samples);
            }

            let hr = unsafe {
                ((*(*capture_client).lp_vtbl).release_buffer)(capture_client, frames_available)
            };
            if should_restart_audio_client(hr) {
                return Ok(ThreadRunState::Restart);
            }
            check_hresult(hr, "IAudioCaptureClient::ReleaseBuffer")?;
            if let Some(report) = diagnostics.observe(
                flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY != 0,
                Instant::now(),
            ) {
                tracing::warn!(
                    total = report.total,
                    since_last_report = report.since_last_report,
                    "WASAPI 捕获音频不连续"
                );
            }
        }
    }
}

fn prime_render_buffer(
    render_client: *mut IAudioRenderClient,
    buffer_frames: u32,
    channels: usize,
) -> Result<()> {
    let mut data_ptr = ptr::null_mut();
    unsafe {
        check_hresult(
            ((*(*render_client).lp_vtbl).get_buffer)(render_client, buffer_frames, &mut data_ptr),
            "IAudioRenderClient::GetBuffer(prime)",
        )?;
    }

    if !data_ptr.is_null() {
        let sample_count = buffer_frames as usize * channels;
        let out = unsafe { slice::from_raw_parts_mut(data_ptr as *mut f32, sample_count) };
        out.fill(0.0);
    }

    unsafe {
        check_hresult(
            ((*(*render_client).lp_vtbl).release_buffer)(
                render_client,
                buffer_frames,
                AUDCLNT_BUFFERFLAGS_SILENT,
            ),
            "IAudioRenderClient::ReleaseBuffer(prime)",
        )?;
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn playback_loop(
    audio_client: *mut IAudioClient,
    render_client: *mut IAudioRenderClient,
    render_event: Handle,
    stop_event: Handle,
    ring: &SharedSampleRing,
    endpoint_id: Option<&str>,
    channels: usize,
    buffer_frames: u32,
) -> Result<ThreadRunState> {
    let handles = [stop_event, render_event];
    let mut last_endpoint_check =
        Instant::now() - Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS));
    loop {
        if endpoint::should_rebind(endpoint_id, &mut last_endpoint_check, Instant::now(), current_default_render_endpoint_id) {
            return Ok(ThreadRunState::Restart);
        }

        match wait_for_multiple_objects_timeout(&handles, DEVICE_REBIND_POLL_MS)? {
            Some(0) => return Ok(ThreadRunState::Stop),
            Some(1) => {}
            None => continue,
            index => {
                return Err(Error::Backend(format!(
                    "unexpected wait result from render thread: {index:?}"
                )));
            }
        }

        let mut padding = 0u32;
        let hr =
            unsafe { ((*(*audio_client).lp_vtbl).get_current_padding)(audio_client, &mut padding) };
        if should_restart_audio_client(hr) {
            return Ok(ThreadRunState::Restart);
        }
        check_hresult(hr, "IAudioClient::GetCurrentPadding")?;
        let frames_available = buffer_frames.saturating_sub(padding);
        if frames_available == 0 {
            continue;
        }

        let mut data_ptr = ptr::null_mut();
        let hr = unsafe {
            ((*(*render_client).lp_vtbl).get_buffer)(render_client, frames_available, &mut data_ptr)
        };
        if should_restart_audio_client(hr) {
            return Ok(ThreadRunState::Restart);
        }
        check_hresult(hr, "IAudioRenderClient::GetBuffer")?;

        if !data_ptr.is_null() {
            let sample_count = frames_available as usize * channels;
            let out = unsafe { slice::from_raw_parts_mut(data_ptr as *mut f32, sample_count) };
            ring.read_partial_zero_fill(out);
        }

        let hr = unsafe {
            ((*(*render_client).lp_vtbl).release_buffer)(render_client, frames_available, 0)
        };
        if should_restart_audio_client(hr) {
            return Ok(ThreadRunState::Restart);
        }
        check_hresult(hr, "IAudioRenderClient::ReleaseBuffer")?;
    }
}

struct SharedSampleRing {
    state: Mutex<RingState>,
    readable: Condvar,
    writable: Condvar,
    closed_message: &'static str,
    channels: usize,
    frame_samples: usize,
    playback_watermark: usize,
}

impl SharedSampleRing {
    fn new(
        capacity: usize,
        channels: usize,
        frame_samples: usize,
        playback_watermark: usize,
        closed_message: &'static str,
    ) -> Result<Self> {
        if channels == 0 || frame_samples == 0 || capacity < frame_samples
            || !capacity.is_multiple_of(channels) || !frame_samples.is_multiple_of(channels)
            || !playback_watermark.is_multiple_of(channels) || playback_watermark > capacity
        {
            return Err(Error::Backend("Windows 音频队列边界无效".into()));
        }
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(capacity)
            .map_err(|err| Error::Backend(format!("分配 Windows 音频队列失败: {err}")))?;
        buffer.resize(capacity, 0.0);
        Ok(Self {
            state: Mutex::new(RingState {
                buffer,
                read_pos: 0,
                write_pos: 0,
                len: 0,
                closed: false,
                recovering: false,
                generation: 0,
                dropped_samples: 0,
                high_water_samples: 0,
                #[cfg(test)]
                writer_wait_hook: None,
                error: None,
            }),
            readable: Condvar::new(),
            writable: Condvar::new(),
            closed_message,
            channels,
            frame_samples,
            playback_watermark,
        })
    }

    // 仅在捕获生产循环启动前调用, 初始化与设备恢复都需要重新读取实际单包上界.
    fn configure_capture_packet(&self, buffer_frames: u32, sample_rate: u32) -> Result<()> {
        if buffer_frames == 0 || sample_rate == 0 {
            return Err(Error::Backend("Windows 捕获设备包或采样率无效".into()));
        }
        let packet_samples = usize::try_from(buffer_frames).ok()
            .and_then(|frames| frames.checked_mul(self.channels))
            .ok_or_else(|| Error::Backend("Windows 捕获设备包样本数溢出".into()))?;
        let floor_frames = usize::try_from(sample_rate).ok()
            .and_then(|rate| rate.checked_mul(30))
            .ok_or_else(|| Error::Backend("Windows 捕获时间预算溢出".into()))?;
        let floor_samples = floor_frames.div_ceil(1000).checked_mul(self.channels)
            .ok_or_else(|| Error::Backend("Windows 捕获时间预算样本数溢出".into()))?;
        let capacity = self.frame_samples.checked_add(packet_samples)
            .ok_or_else(|| Error::Backend("Windows 捕获拼帧容量溢出".into()))?
            .max(floor_samples);
        let mut state = self.lock_state()?;
        if state.closed {
            return Err(self.closed_error(&state));
        }
        // 分配成功后才替换旧状态, 不改变恢复标志或流代次.
        let mut buffer = Vec::new();
        buffer.try_reserve_exact(capacity)
            .map_err(|err| Error::Backend(format!("分配 Windows 捕获拼帧队列失败: {err}")))?;
        buffer.resize(capacity, 0.0);
        let discarded = state.len;
        state.record_drop(discarded);
        state.buffer = buffer;
        state.read_pos = 0;
        state.write_pos = 0;
        state.len = 0;
        self.readable.notify_all();
        self.writable.notify_all();
        tracing::debug!(capacity, packet_samples, "已按设备最大单包调整 Windows 捕获队列");
        Ok(())
    }

    fn read_exact(&self, out: &mut [f32], timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state()?;
        if state.closed {
            return Err(self.closed_error(&state));
        }
        if !out.len().is_multiple_of(self.channels) || out.len() > state.buffer.len() {
            return Err(Error::Backend("Windows 捕获读取未按声道帧对齐或超出容量".into()));
        }
        while state.len < out.len() && !state.closed {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_state, timed_out) = self.wait_for_readable(state, remaining)?;
            state = next_state;
            if state.closed {
                return Err(self.closed_error(&state));
            }
            if timed_out && state.len < out.len() {
                return Ok(false);
            }
        }

        if state.closed {
            return Err(self.closed_error(&state));
        }

        read_ring(&mut state, out);
        self.writable.notify_all();
        Ok(true)
    }

    fn write_blocking(&self, samples: &[f32], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout.min(MAX_PLAYBACK_WAIT);
        let mut state = self.lock_state()?;
        let generation = state.generation;
        loop {
            if state.closed {
                return Err(self.closed_error(&state));
            }
            if samples.len() != self.frame_samples {
                return Err(Error::Backend("Windows 播放提交必须为完整协商帧".into()));
            }
            // 恢复期间的提交以及跨越恢复边界的旧提交均丢弃, 不加入新设备队列.
            if !state.accepts_write(generation) {
                state.record_drop(samples.len());
                return Ok(());
            }
            // 水位仅计算软件队列, WASAPI 设备缓冲由独立诊断报告.
            if state.len <= self.playback_watermark && state.available() >= samples.len() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                state.record_drop(samples.len());
                return Err(Error::Backend(
                    "Windows playback ring buffer timed out".into(),
                ));
            }
            let remaining = deadline.saturating_duration_since(now);
            #[cfg(test)]
            if let Some(hook) = state.writer_wait_hook.take() {
                let _ = hook.send(remaining);
            }
            let (next_state, _) = self.wait_for_writable(state, remaining)?;
            state = next_state;
        }

        write_ring(&mut state, samples);
        self.readable.notify_all();
        Ok(())
    }

    fn begin_recovery(&self) -> Result<usize> {
        let mut state = self.lock_state()?;
        if state.closed {
            return Err(self.closed_error(&state));
        }
        let discarded = state.len;
        state.generation = state.generation.wrapping_add(1);
        state.recovering = true;
        discard_oldest(&mut state, discarded);
        self.readable.notify_all();
        self.writable.notify_all();
        Ok(discarded)
    }

    fn finish_recovery(&self) -> Result<()> {
        let mut state = self.lock_state()?;
        if state.closed {
            return Err(self.closed_error(&state));
        }
        let discarded = state.len;
        discard_oldest(&mut state, discarded);
        state.recovering = false;
        self.readable.notify_all();
        self.writable.notify_all();
        Ok(())
    }

    fn write_overwrite(&self, samples: &[f32]) {
        self.write_capture(Some(samples), samples.len());
    }

    fn write_silence_overwrite(&self, sample_count: usize) {
        self.write_capture(None, sample_count);
    }

    fn write_capture(&self, samples: Option<&[f32]>, sample_count: usize) {
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.closed {
            return;
        }
        if state.recovering || !sample_count.is_multiple_of(self.channels) {
            state.record_drop(sample_count);
            return;
        }
        let retained = sample_count.min(state.buffer.len());
        let input_skipped = sample_count - retained;
        state.record_drop(input_skipped);
        let needed = retained.saturating_sub(state.available());
        discard_oldest(&mut state, needed);
        // 容量与输入都按完整声道帧对齐, 保留最新音频不会交换左右声道.
        match samples {
            Some(samples) => write_ring(&mut state, &samples[input_skipped..]),
            None => {
                for _ in 0..retained {
                    let position = state.write_pos;
                    state.buffer[position] = 0.0;
                    state.write_pos = (position + 1) % state.buffer.len();
                }
                state.len += retained;
                state.high_water_samples = state.high_water_samples.max(state.len);
            }
        }
        self.readable.notify_all();
    }

    fn read_partial_zero_fill(&self, out: &mut [f32]) -> usize {
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(_) => {
                out.fill(0.0);
                return 0;
            }
        };

        let aligned_len = out.len() - out.len() % self.channels;
        let count = state.len.min(aligned_len);
        if count > 0 {
            read_ring_prefix(&mut state, out, count);
        }
        if count < out.len() {
            out[count..].fill(0.0);
        }
        self.writable.notify_all();
        count
    }

    fn close(&self, error: Option<String>) {
        if let Ok(mut state) = self.lock_state() {
            if let Some(error) = error {
                state.error.get_or_insert(error);
            }
            if !state.closed {
                tracing::debug!(
                    backend = self.closed_message,
                    dropped_samples = state.dropped_samples,
                    high_water_samples = state.high_water_samples,
                    "Windows 音频软件队列已关闭"
                );
            }
            state.closed = true;
            self.readable.notify_all();
            self.writable.notify_all();
        }
    }

    fn lock_state(&self) -> Result<MutexGuard<'_, RingState>> {
        self.state
            .lock()
            .map_err(|_| Error::Backend("Windows audio ring buffer was poisoned".into()))
    }

    fn wait_for_readable<'a>(
        &self,
        state: MutexGuard<'a, RingState>,
        timeout: Duration,
    ) -> Result<(MutexGuard<'a, RingState>, bool)> {
        self.readable
            .wait_timeout(state, timeout)
            .map(|(guard, wait)| (guard, wait.timed_out()))
            .map_err(|_| Error::Backend("Windows audio ring buffer was poisoned".into()))
    }

    fn wait_for_writable<'a>(
        &self,
        state: MutexGuard<'a, RingState>,
        timeout: Duration,
    ) -> Result<(MutexGuard<'a, RingState>, bool)> {
        self.writable
            .wait_timeout(state, timeout)
            .map(|(guard, wait)| (guard, wait.timed_out()))
            .map_err(|_| Error::Backend("Windows audio ring buffer was poisoned".into()))
    }

    fn closed_error(&self, state: &RingState) -> Error {
        Error::Backend(
            state
                .error
                .clone()
                .unwrap_or_else(|| self.closed_message.to_string()),
        )
    }
}

struct RingState {
    buffer: Vec<f32>,
    read_pos: usize,
    write_pos: usize,
    len: usize,
    closed: bool,
    recovering: bool,
    generation: u64,
    dropped_samples: u64,
    high_water_samples: usize,
    #[cfg(test)]
    writer_wait_hook: Option<mpsc::Sender<Duration>>,
    error: Option<String>,
}

impl RingState {
    fn record_drop(&mut self, samples: usize) {
        self.dropped_samples = self.dropped_samples.saturating_add(
            u64::try_from(samples).unwrap_or(u64::MAX),
        );
    }

    fn accepts_write(&self, generation: u64) -> bool {
        !self.recovering && self.generation == generation
    }

    fn available(&self) -> usize {
        self.buffer.len() - self.len
    }
}

fn write_ring(state: &mut RingState, samples: &[f32]) {
    for &sample in samples {
        state.buffer[state.write_pos] = sample;
        state.write_pos = (state.write_pos + 1) % state.buffer.len();
    }
    state.len += samples.len();
    state.high_water_samples = state.high_water_samples.max(state.len);
}

fn read_ring(state: &mut RingState, out: &mut [f32]) {
    read_ring_prefix(state, out, out.len());
}

fn read_ring_prefix(state: &mut RingState, out: &mut [f32], count: usize) {
    for slot in &mut out[..count] {
        *slot = state.buffer[state.read_pos];
        state.read_pos = (state.read_pos + 1) % state.buffer.len();
    }
    state.len -= count;
}

fn discard_oldest(state: &mut RingState, count: usize) {
    state.record_drop(count.min(state.len));
    if count >= state.len {
        state.read_pos = state.write_pos;
        state.len = 0;
        return;
    }

    state.read_pos = (state.read_pos + count) % state.buffer.len();
    state.len -= count;
}

struct ActivatedAudioClient {
    audio_client: ComPtr<IAudioClient>,
    endpoint_id: Option<String>,
}

fn activate_audio_client(selection: &EndpointSelection) -> Result<ActivatedAudioClient> {
    let device = get_render_endpoint(selection)?;
    let endpoint_id = get_device_id(device.as_ptr())?;
    tracing::info!(follows_default = selection.follows_default(), "已选择 Windows 音频 endpoint");

    let mut audio_client = ptr::null_mut();
    unsafe {
        check_hresult(
            ((*(*device.as_ptr()).lp_vtbl).activate)(
                device.as_ptr(),
                &IID_IAUDIO_CLIENT,
                CLSCTX_ALL,
                ptr::null_mut(),
                &mut audio_client,
            ),
            "IMMDevice::Activate(IAudioClient)",
        )?;
    }

    Ok(ActivatedAudioClient {
        audio_client: ComPtr::from_raw(audio_client.cast())?,
        endpoint_id: selection.follows_default().then_some(endpoint_id),
    })
}

fn get_render_endpoint(selection: &EndpointSelection) -> Result<ComPtr<IMMDevice>> {
    let mut enumerator = ptr::null_mut();
    unsafe {
        check_hresult(
            CoCreateInstance(
                &CLSID_MMDEVICE_ENUMERATOR,
                ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IMMDEVICE_ENUMERATOR,
                &mut enumerator,
            ),
            "CoCreateInstance(MMDeviceEnumerator)",
        )?;
    }
    let enumerator = ComPtr::<IMMDeviceEnumerator>::from_raw(enumerator.cast())?;

    endpoint::select(&enumerator, selection)
}

fn current_default_render_endpoint_id() -> Result<String> {
    let device = get_render_endpoint(&EndpointSelection::Default)?;
    get_device_id(device.as_ptr())
}

fn get_device_id(device: *mut IMMDevice) -> Result<String> {
    let mut wide_ptr = ptr::null_mut();
    unsafe {
        check_hresult(
            ((*(*device).lp_vtbl).get_id)(device, &mut wide_ptr),
            "IMMDevice::GetId",
        )?;
    }

    if wide_ptr.is_null() {
        return Err(Error::Backend("IMMDevice::GetId returned null".into()));
    }

    let id = unsafe {
        let mut len = 0usize;
        while *wide_ptr.add(len) != 0 {
            len += 1;
        }
        let slice = slice::from_raw_parts(wide_ptr, len);
        String::from_utf16_lossy(slice)
    };

    unsafe { CoTaskMemFree(wide_ptr.cast()) };
    Ok(id)
}

fn should_restart_audio_client(hr: i32) -> bool {
    matches!(
        hr,
        AUDCLNT_E_DEVICE_INVALIDATED
            | AUDCLNT_E_ENDPOINT_CREATE_FAILED
            | AUDCLNT_E_SERVICE_NOT_RUNNING
            | AUDCLNT_E_RESOURCES_INVALIDATED
    )
}

fn get_service<T>(audio_client: *mut IAudioClient, iid: &Guid, action: &str) -> Result<ComPtr<T>> {
    let mut service = ptr::null_mut();
    unsafe {
        check_hresult(
            ((*(*audio_client).lp_vtbl).get_service)(audio_client, iid, &mut service),
            action,
        )?;
    }
    ComPtr::from_raw(service.cast())
}

fn wait_for_multiple_objects_timeout(handles: &[Handle], timeout_ms: u32) -> Result<Option<usize>> {
    let result = unsafe {
        WaitForMultipleObjects(handles.len() as u32, handles.as_ptr(), FALSE, timeout_ms)
    };
    if result == WAIT_TIMEOUT {
        return Ok(None);
    }
    if result == WAIT_FAILED {
        return Err(last_os_error("WaitForMultipleObjects"));
    }
    if result >= WAIT_OBJECT_0 + handles.len() as u32 {
        return Err(Error::Backend(format!(
            "WaitForMultipleObjects returned unexpected value 0x{result:08X}"
        )));
    }
    Ok(Some((result - WAIT_OBJECT_0) as usize))
}

fn wait_for_stop_or_timeout(stop_event: Handle, timeout_ms: u32) -> Result<bool> {
    Ok(wait_for_multiple_objects_timeout(&[stop_event], timeout_ms)?.is_some())
}

fn check_hresult(hr: i32, action: &str) -> Result<()> {
    if hr >= 0 {
        Ok(())
    } else {
        Err(Error::Backend(format!(
            "{action} failed with HRESULT {}",
            format_hresult(hr)
        )))
    }
}

fn format_hresult(hr: i32) -> String {
    format!("0x{:08X}", hr as u32)
}

fn last_os_error(action: &str) -> Error {
    Error::Backend(format!(
        "{action} failed: {}",
        std::io::Error::last_os_error()
    ))
}

struct ComApartment;

impl ComApartment {
    fn new() -> Result<Self> {
        unsafe {
            check_hresult(
                CoInitializeEx(ptr::null_mut(), COINIT_MULTITHREADED),
                "CoInitializeEx",
            )?;
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

struct ComPtr<T> {
    ptr: *mut T,
}

impl<T> ComPtr<T> {
    fn from_raw(ptr: *mut T) -> Result<Self> {
        if ptr.is_null() {
            Err(Error::Backend("COM activation returned null".into()))
        } else {
            Ok(Self { ptr })
        }
    }

    fn as_ptr(&self) -> *mut T {
        self.ptr
    }
}

impl<T> Drop for ComPtr<T> {
    fn drop(&mut self) {
        unsafe {
            if !self.ptr.is_null() {
                release_com(self.ptr.cast());
            }
        }
    }
}

struct OwnedHandle(Handle);

impl OwnedHandle {
    fn create_manual_reset(initial_state: bool) -> Result<Self> {
        Self::create(true, initial_state)
    }

    fn create_auto_reset(initial_state: bool) -> Result<Self> {
        Self::create(false, initial_state)
    }

    fn create(manual_reset: bool, initial_state: bool) -> Result<Self> {
        let handle = unsafe {
            CreateEventW(
                ptr::null_mut(),
                manual_reset as i32,
                initial_state as i32,
                ptr::null(),
            )
        };
        if handle.is_null() {
            Err(last_os_error("CreateEventW"))
        } else {
            Ok(Self(handle))
        }
    }

    fn raw(&self) -> Handle {
        self.0
    }

    fn set(&self) -> Result<()> {
        let ok = unsafe { SetEvent(self.0) };
        if ok == 0 {
            Err(last_os_error("SetEvent"))
        } else {
            Ok(())
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            if !self.0.is_null() {
                CloseHandle(self.0);
            }
        }
    }
}

unsafe fn release_com(ptr: *mut c_void) {
    let unknown = ptr as *mut IUnknown;
    if !unknown.is_null() {
        unsafe {
            ((*(*unknown).lp_vtbl).release)(unknown);
        }
    }
}

#[repr(transparent)]
#[derive(Clone, Copy)]
struct Handle(*mut c_void);

impl Handle {
    fn is_null(self) -> bool {
        self.0.is_null()
    }
}

unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

impl Guid {
    const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

#[repr(C, packed(1))]
#[derive(Clone, Copy)]
struct WaveFormatEx {
    w_format_tag: u16,
    n_channels: u16,
    n_samples_per_sec: u32,
    n_avg_bytes_per_sec: u32,
    n_block_align: u16,
    w_bits_per_sample: u16,
    cb_size: u16,
}

#[repr(C)]
struct PropVariant {
    _private: [u8; 0],
}

#[repr(C)]
struct IUnknown {
    lp_vtbl: *const IUnknownVtbl,
}

#[repr(C)]
struct IUnknownVtbl {
    query_interface: unsafe extern "system" fn(*mut IUnknown, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IUnknown) -> u32,
    release: unsafe extern "system" fn(*mut IUnknown) -> u32,
}

#[repr(C)]
struct IMMDeviceEnumerator {
    lp_vtbl: *const IMMDeviceEnumeratorVtbl,
}

#[repr(C)]
struct IMMDeviceEnumeratorVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IMMDeviceEnumerator) -> u32,
    release: unsafe extern "system" fn(*mut IMMDeviceEnumerator) -> u32,
    enum_audio_endpoints:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, u32, u32, *mut *mut c_void) -> i32,
    get_default_audio_endpoint:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, u32, u32, *mut *mut IMMDevice) -> i32,
    get_device:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, *const u16, *mut *mut IMMDevice) -> i32,
    register_endpoint_notification_callback:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, *mut c_void) -> i32,
    unregister_endpoint_notification_callback:
        unsafe extern "system" fn(*mut IMMDeviceEnumerator, *mut c_void) -> i32,
}

#[repr(C)]
struct IMMDevice {
    lp_vtbl: *const IMMDeviceVtbl,
}

#[repr(C)]
struct IMMDeviceVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IMMDevice, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IMMDevice) -> u32,
    release: unsafe extern "system" fn(*mut IMMDevice) -> u32,
    activate: unsafe extern "system" fn(
        *mut IMMDevice,
        *const Guid,
        u32,
        *mut PropVariant,
        *mut *mut c_void,
    ) -> i32,
    open_property_store: unsafe extern "system" fn(*mut IMMDevice, u32, *mut *mut c_void) -> i32,
    get_id: unsafe extern "system" fn(*mut IMMDevice, *mut *mut u16) -> i32,
    get_state: unsafe extern "system" fn(*mut IMMDevice, *mut u32) -> i32,
}

#[repr(C)]
struct IAudioClient {
    lp_vtbl: *const IAudioClientVtbl,
}

#[repr(C)]
struct IAudioClientVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IAudioClient, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IAudioClient) -> u32,
    release: unsafe extern "system" fn(*mut IAudioClient) -> u32,
    initialize: unsafe extern "system" fn(
        *mut IAudioClient,
        u32,
        u32,
        i64,
        i64,
        *const WaveFormatEx,
        *const Guid,
    ) -> i32,
    get_buffer_size: unsafe extern "system" fn(*mut IAudioClient, *mut u32) -> i32,
    get_stream_latency: unsafe extern "system" fn(*mut IAudioClient, *mut i64) -> i32,
    get_current_padding: unsafe extern "system" fn(*mut IAudioClient, *mut u32) -> i32,
    is_format_supported: unsafe extern "system" fn(
        *mut IAudioClient,
        u32,
        *const WaveFormatEx,
        *mut *mut WaveFormatEx,
    ) -> i32,
    get_mix_format: unsafe extern "system" fn(*mut IAudioClient, *mut *mut WaveFormatEx) -> i32,
    get_device_period: unsafe extern "system" fn(*mut IAudioClient, *mut i64, *mut i64) -> i32,
    start: unsafe extern "system" fn(*mut IAudioClient) -> i32,
    stop: unsafe extern "system" fn(*mut IAudioClient) -> i32,
    reset: unsafe extern "system" fn(*mut IAudioClient) -> i32,
    set_event_handle: unsafe extern "system" fn(*mut IAudioClient, Handle) -> i32,
    get_service: unsafe extern "system" fn(*mut IAudioClient, *const Guid, *mut *mut c_void) -> i32,
}

#[repr(C)]
struct IAudioRenderClient {
    lp_vtbl: *const IAudioRenderClientVtbl,
}

#[repr(C)]
struct IAudioRenderClientVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IAudioRenderClient, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IAudioRenderClient) -> u32,
    release: unsafe extern "system" fn(*mut IAudioRenderClient) -> u32,
    get_buffer: unsafe extern "system" fn(*mut IAudioRenderClient, u32, *mut *mut u8) -> i32,
    release_buffer: unsafe extern "system" fn(*mut IAudioRenderClient, u32, u32) -> i32,
}

#[repr(C)]
struct IAudioCaptureClient {
    lp_vtbl: *const IAudioCaptureClientVtbl,
}

#[repr(C)]
struct IAudioCaptureClientVtbl {
    query_interface:
        unsafe extern "system" fn(*mut IAudioCaptureClient, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut IAudioCaptureClient) -> u32,
    release: unsafe extern "system" fn(*mut IAudioCaptureClient) -> u32,
    get_buffer: unsafe extern "system" fn(
        *mut IAudioCaptureClient,
        *mut *mut u8,
        *mut u32,
        *mut u32,
        *mut u64,
        *mut u64,
    ) -> i32,
    release_buffer: unsafe extern "system" fn(*mut IAudioCaptureClient, u32) -> i32,
    get_next_packet_size: unsafe extern "system" fn(*mut IAudioCaptureClient, *mut u32) -> i32,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateEventW(
        event_attributes: *mut c_void,
        manual_reset: i32,
        initial_state: i32,
        name: *const u16,
    ) -> Handle;
    fn SetEvent(handle: Handle) -> i32;
    fn WaitForMultipleObjects(
        count: u32,
        handles: *const Handle,
        wait_all: i32,
        milliseconds: u32,
    ) -> u32;
    fn CloseHandle(handle: Handle) -> i32;
}

#[link(name = "ole32")]
unsafe extern "system" {
    fn CoInitializeEx(reserved: *mut c_void, coinit: u32) -> i32;
    fn CoUninitialize();
    fn CoTaskMemFree(memory: *mut c_void);
    fn CoCreateInstance(
        clsid: *const Guid,
        outer: *mut c_void,
        cls_context: u32,
        iid: *const Guid,
        object: *mut *mut c_void,
    ) -> i32;
}
