use crate::audio::capture::{AudioInput, CaptureStatus};
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;
use std::slice;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const SUPPORTED_CHANNELS: u16 = 2;
const CAPTURE_RING_SECONDS: usize = 2;
const PLAYBACK_RING_SECONDS: usize = 1;
const SHARED_BUFFER_DURATION_HNS: i64 = 1_000_000;

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
const AUDCLNT_BUFFERFLAGS_SILENT: u32 = 0x2;
const AUDCLNT_E_DEVICE_INVALIDATED: i32 = 0x8889_0004u32 as i32;
const AUDCLNT_E_ENDPOINT_CREATE_FAILED: i32 = 0x8889_000Fu32 as i32;
const AUDCLNT_E_SERVICE_NOT_RUNNING: i32 = 0x8889_0010u32 as i32;
const AUDCLNT_E_RESOURCES_INVALIDATED: i32 = 0x8889_0026u32 as i32;

const E_RENDER: u32 = 0;
const E_CONSOLE: u32 = 0;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const PRO_AUDIO_TASK_NAME: [u16; 10] = [80, 114, 111, 32, 65, 117, 100, 105, 111, 0];

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
    validate_stream(
        "Windows WASAPI loopback capture",
        config.device_name.as_deref(),
        stream,
    )?;

    let ring = Arc::new(SharedSampleRing::new(
        stream.sample_rate as usize * stream.channels as usize * CAPTURE_RING_SECONDS,
        "windows capture backend has been closed",
    ));
    let stop_event = OwnedHandle::create_manual_reset(false)?;
    let thread = spawn_capture_thread(
        stop_event.raw(),
        Arc::clone(&ring),
        WasapiSpec::from_stream(stream),
    )?;

    Ok(Box::new(WindowsInput {
        continuous_audio: config.continuous_audio,
        ring,
        stop_event,
        thread: Some(thread),
    }))
}

pub fn open_output(config: &PlaybackConfig, stream: &StreamParams) -> Result<Box<dyn AudioOutput>> {
    validate_stream(
        "Windows WASAPI playback",
        config.device_name.as_deref(),
        stream,
    )?;

    let ring = Arc::new(SharedSampleRing::new(
        stream.sample_rate as usize * stream.channels as usize * PLAYBACK_RING_SECONDS,
        "windows playback backend has been closed",
    ));
    let stop_event = OwnedHandle::create_manual_reset(false)?;
    let thread = spawn_playback_thread(
        stop_event.raw(),
        Arc::clone(&ring),
        WasapiSpec::from_stream(stream),
    )?;

    Ok(Box::new(WindowsOutput {
        ring,
        stop_event,
        thread: Some(thread),
    }))
}

fn validate_stream(kind: &str, device_name: Option<&str>, stream: &StreamParams) -> Result<()> {
    if device_name.is_some() {
        return Err(Error::Backend(format!(
            "{kind} does not support selecting a specific device yet"
        )));
    }

    if stream.channels != SUPPORTED_CHANNELS as u8 {
        return Err(Error::UnsupportedPlatform(
            "Windows audio backend currently supports stereo only",
        ));
    }

    Ok(())
}

struct WindowsInput {
    continuous_audio: bool,
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
        if self.continuous_audio {
            self.ring.read_exact_or_silence(frame, timeout)?;
            return Ok(CaptureStatus::Ok);
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

    fn wave_format(&self) -> WaveFormatEx {
        let block_align = self.channels * size_of::<f32>() as u16;
        WaveFormatEx {
            w_format_tag: WAVE_FORMAT_IEEE_FLOAT,
            n_channels: self.channels,
            n_samples_per_sec: self.sample_rate,
            n_avg_bytes_per_sec: self.sample_rate * block_align as u32,
            n_block_align: block_align,
            w_bits_per_sample: 32,
            cb_size: 0,
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
) -> Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("audio-relay-win-capture".into())
        .spawn(move || {
            capture_thread_main(stop_event, ring, spec, ready_tx);
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
) -> Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name("audio-relay-win-playback".into())
        .spawn(move || {
            playback_thread_main(stop_event, ring, spec, ready_tx);
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
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) {
    let _mmcss = MmcssTask::enter_pro_audio();
    let mut ready_sent = false;
    loop {
        match CaptureThreadContext::start(spec) {
            Ok(context) => {
                if !ready_sent {
                    let _ = ready_tx.send(Ok(()));
                    ready_sent = true;
                }

                match context.run(stop_event, &ring) {
                    Ok(ThreadRunState::Stop) => return,
                    Ok(ThreadRunState::Restart) => continue,
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
    ready_tx: mpsc::Sender<std::result::Result<(), String>>,
) {
    let _mmcss = MmcssTask::enter_pro_audio();
    let mut ready_sent = false;
    loop {
        match PlaybackThreadContext::start(spec) {
            Ok(context) => {
                if !ready_sent {
                    let _ = ready_tx.send(Ok(()));
                    ready_sent = true;
                }

                match context.run(stop_event, &ring) {
                    Ok(ThreadRunState::Stop) => return,
                    Ok(ThreadRunState::Restart) => continue,
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

struct CaptureThreadContext {
    _com: ComApartment,
    audio_client: ComPtr<IAudioClient>,
    capture_client: ComPtr<IAudioCaptureClient>,
    capture_event: OwnedHandle,
    endpoint_id: String,
    channels: usize,
}

impl CaptureThreadContext {
    fn start(spec: WasapiSpec) -> Result<Self> {
        let com = ComApartment::new()?;
        let activated = activate_default_audio_client()?;
        let audio_client = activated.audio_client;
        let capture_event = OwnedHandle::create_manual_reset(false)?;
        let format = spec.wave_format();
        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK
            | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        unsafe {
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).initialize)(
                    audio_client.as_ptr(),
                    AUDCLNT_SHAREMODE_SHARED,
                    stream_flags,
                    SHARED_BUFFER_DURATION_HNS,
                    0,
                    &format,
                    ptr::null(),
                ),
                "IAudioClient::Initialize(loopback)",
            )?;
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).set_event_handle)(
                    audio_client.as_ptr(),
                    capture_event.raw(),
                ),
                "IAudioClient::SetEventHandle(loopback)",
            )?;
        }

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
            _com: com,
            audio_client,
            capture_client,
            capture_event,
            endpoint_id: activated.endpoint_id,
            channels: spec.channels as usize,
        })
    }

    fn run(self, stop_event: Handle, ring: &SharedSampleRing) -> Result<ThreadRunState> {
        let result = capture_loop(
            self.capture_client.as_ptr(),
            self.capture_event.raw(),
            stop_event,
            ring,
            &self.endpoint_id,
            self.channels,
        );

        unsafe {
            let _ = ((*(*self.audio_client.as_ptr()).lp_vtbl).stop)(self.audio_client.as_ptr());
        }

        result
    }
}

struct PlaybackThreadContext {
    _com: ComApartment,
    audio_client: ComPtr<IAudioClient>,
    render_client: ComPtr<IAudioRenderClient>,
    render_event: OwnedHandle,
    endpoint_id: String,
    channels: usize,
    buffer_frames: u32,
}

impl PlaybackThreadContext {
    fn start(spec: WasapiSpec) -> Result<Self> {
        let com = ComApartment::new()?;
        let activated = activate_default_audio_client()?;
        let audio_client = activated.audio_client;
        let render_event = OwnedHandle::create_manual_reset(false)?;
        let format = spec.wave_format();
        let stream_flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_NOPERSIST
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        unsafe {
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).initialize)(
                    audio_client.as_ptr(),
                    AUDCLNT_SHAREMODE_SHARED,
                    stream_flags,
                    SHARED_BUFFER_DURATION_HNS,
                    0,
                    &format,
                    ptr::null(),
                ),
                "IAudioClient::Initialize(render)",
            )?;
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).set_event_handle)(
                    audio_client.as_ptr(),
                    render_event.raw(),
                ),
                "IAudioClient::SetEventHandle(render)",
            )?;
        }

        let mut buffer_frames = 0u32;
        unsafe {
            check_hresult(
                ((*(*audio_client.as_ptr()).lp_vtbl).get_buffer_size)(
                    audio_client.as_ptr(),
                    &mut buffer_frames,
                ),
                "IAudioClient::GetBufferSize",
            )?;
        }

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
            _com: com,
            audio_client,
            render_client,
            render_event,
            endpoint_id: activated.endpoint_id,
            channels: spec.channels as usize,
            buffer_frames,
        })
    }

    fn run(self, stop_event: Handle, ring: &SharedSampleRing) -> Result<ThreadRunState> {
        let result = playback_loop(
            self.audio_client.as_ptr(),
            self.render_client.as_ptr(),
            self.render_event.raw(),
            stop_event,
            ring,
            &self.endpoint_id,
            self.channels,
            self.buffer_frames,
        );

        unsafe {
            let _ = ((*(*self.audio_client.as_ptr()).lp_vtbl).stop)(self.audio_client.as_ptr());
        }

        result
    }
}

fn capture_loop(
    capture_client: *mut IAudioCaptureClient,
    capture_event: Handle,
    stop_event: Handle,
    ring: &SharedSampleRing,
    endpoint_id: &str,
    channels: usize,
) -> Result<ThreadRunState> {
    let handles = [stop_event, capture_event];
    let mut last_endpoint_check =
        Instant::now() - Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS));
    loop {
        if should_rebind_default_render_endpoint(endpoint_id, &mut last_endpoint_check)? {
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

fn playback_loop(
    audio_client: *mut IAudioClient,
    render_client: *mut IAudioRenderClient,
    render_event: Handle,
    stop_event: Handle,
    ring: &SharedSampleRing,
    endpoint_id: &str,
    channels: usize,
    buffer_frames: u32,
) -> Result<ThreadRunState> {
    let handles = [stop_event, render_event];
    let mut last_endpoint_check =
        Instant::now() - Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS));
    loop {
        if should_rebind_default_render_endpoint(endpoint_id, &mut last_endpoint_check)? {
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
}

impl SharedSampleRing {
    fn new(capacity: usize, closed_message: &'static str) -> Self {
        Self {
            state: Mutex::new(RingState {
                buffer: vec![0.0; capacity.max(1)],
                read_pos: 0,
                write_pos: 0,
                len: 0,
                closed: false,
                error: None,
            }),
            readable: Condvar::new(),
            writable: Condvar::new(),
            closed_message,
        }
    }

    fn read_exact(&self, out: &mut [f32], timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state()?;
        while state.len < out.len() && !state.closed {
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_state, timed_out) = self.wait_for_readable(state, remaining)?;
            state = next_state;
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

    fn read_exact_or_silence(&self, out: &mut [f32], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state()?;
        while state.len < out.len() && !state.closed {
            let now = Instant::now();
            if now >= deadline {
                break;
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_state, _) = self.wait_for_readable(state, remaining)?;
            state = next_state;
        }

        if state.closed {
            return Err(self.closed_error(&state));
        }

        let count = state.len.min(out.len());
        if count > 0 {
            read_ring_prefix(&mut state, out, count);
        }
        if count < out.len() {
            out[count..].fill(0.0);
        }
        self.writable.notify_all();
        Ok(())
    }

    fn write_blocking(&self, samples: &[f32], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.lock_state()?;
        while state.available() < samples.len() && !state.closed {
            let now = Instant::now();
            if now >= deadline {
                return Err(Error::Backend(
                    "Windows playback ring buffer timed out".into(),
                ));
            }

            let remaining = deadline.saturating_duration_since(now);
            let (next_state, timed_out) = self.wait_for_writable(state, remaining)?;
            state = next_state;
            if timed_out && state.available() < samples.len() {
                return Err(Error::Backend(
                    "Windows playback ring buffer timed out".into(),
                ));
            }
        }

        if state.closed {
            return Err(self.closed_error(&state));
        }

        write_ring(&mut state, samples);
        self.readable.notify_all();
        Ok(())
    }

    fn write_overwrite(&self, samples: &[f32]) {
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(_) => return,
        };
        if state.closed {
            return;
        }

        let samples = if samples.len() >= state.buffer.len() {
            &samples[samples.len() - state.buffer.len()..]
        } else {
            samples
        };

        let needed = samples.len().saturating_sub(state.available());
        if needed > 0 {
            discard_oldest(&mut state, needed);
        }

        write_ring(&mut state, samples);
        self.readable.notify_all();
    }

    fn write_silence_overwrite(&self, sample_count: usize) {
        let silence = vec![0.0; sample_count];
        self.write_overwrite(&silence);
    }

    fn read_partial_zero_fill(&self, out: &mut [f32]) -> usize {
        let mut state = match self.lock_state() {
            Ok(state) => state,
            Err(_) => {
                out.fill(0.0);
                return 0;
            }
        };

        let count = state.len.min(out.len());
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
    error: Option<String>,
}

impl RingState {
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
    endpoint_id: String,
}

fn activate_default_audio_client() -> Result<ActivatedAudioClient> {
    let endpoint = get_default_render_endpoint()?;
    let endpoint_id = get_device_id(endpoint.device.as_ptr())?;
    let device = endpoint.device;

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
        endpoint_id,
    })
}

struct DefaultRenderEndpoint {
    device: ComPtr<IMMDevice>,
}

fn get_default_render_endpoint() -> Result<DefaultRenderEndpoint> {
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

    let mut device = ptr::null_mut();
    unsafe {
        check_hresult(
            ((*(*enumerator.as_ptr()).lp_vtbl).get_default_audio_endpoint)(
                enumerator.as_ptr(),
                E_RENDER,
                E_CONSOLE,
                &mut device,
            ),
            "IMMDeviceEnumerator::GetDefaultAudioEndpoint",
        )?;
    }
    let device = ComPtr::<IMMDevice>::from_raw(device)?;
    Ok(DefaultRenderEndpoint { device })
}

fn current_default_render_endpoint_id() -> Result<String> {
    let endpoint = get_default_render_endpoint()?;
    get_device_id(endpoint.device.as_ptr())
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

fn has_default_render_endpoint_changed(bound_endpoint_id: &str) -> Result<bool> {
    match current_default_render_endpoint_id() {
        Ok(current) => Ok(current != bound_endpoint_id),
        Err(_) => Ok(true),
    }
}

fn should_rebind_default_render_endpoint(
    bound_endpoint_id: &str,
    last_check: &mut Instant,
) -> Result<bool> {
    let now = Instant::now();
    if now.duration_since(*last_check) < Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS)) {
        return Ok(false);
    }

    *last_check = now;
    has_default_render_endpoint_changed(bound_endpoint_id)
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
    if result < WAIT_OBJECT_0 || result >= WAIT_OBJECT_0 + handles.len() as u32 {
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

struct MmcssTask(Handle);

impl MmcssTask {
    fn enter_pro_audio() -> Option<Self> {
        let mut task_index = 0u32;
        let handle =
            unsafe { AvSetMmThreadCharacteristicsW(PRO_AUDIO_TASK_NAME.as_ptr(), &mut task_index) };
        (!handle.is_null()).then_some(Self(handle))
    }
}

impl Drop for MmcssTask {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.0);
        }
    }
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
        let handle = unsafe { CreateEventW(ptr::null_mut(), 1, initial_state as i32, ptr::null()) };
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

#[repr(C)]
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

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> Handle;
    fn AvRevertMmThreadCharacteristics(task_handle: Handle) -> i32;
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
