use super::*;
use std::cell::Cell;
use std::mem::size_of;

thread_local! {
    static MOCK_STAGE: Cell<u32> = const { Cell::new(0) };
    static MOCK_PERIOD_FAILS: Cell<bool> = const { Cell::new(false) };
    static MOCK_CHANNELS: Cell<u16> = const { Cell::new(2) };
}

unsafe extern "system" fn mock_initialize(
    _: *mut IAudioClient,
    mode: u32,
    flags: u32,
    duration: i64,
    periodicity: i64,
    format: *const WaveFormatEx,
    _: *const Guid,
) -> i32 {
    assert_eq!(mode, AUDCLNT_SHAREMODE_SHARED);
    assert_ne!(flags & AUDCLNT_STREAMFLAGS_EVENTCALLBACK, 0);
    assert_eq!((duration, periodicity), (0, 0));
    let header = unsafe { format.read_unaligned() };
    let rate = header.n_samples_per_sec;
    let channels = header.n_channels;
    let tag = header.w_format_tag;
    let size = header.cb_size;
    assert_eq!(rate, 48_000);
    assert_eq!(channels, MOCK_CHANNELS.with(Cell::get));
    assert_eq!((tag, size), (0xfffe, 22));
    let mask = unsafe { ptr::addr_of!((*format.cast::<format::WaveFormatExtensible>()).channel_mask).read_unaligned() };
    assert_eq!(mask, match channels { 2 => 0x3, 6 => 0x60f, 8 => 0x63f, _ => unreachable!() });
    MOCK_STAGE.with(|stage| { assert_eq!(stage.get(), 0); stage.set(1); });
    0
}

unsafe extern "system" fn mock_set_event(_: *mut IAudioClient, _: Handle) -> i32 {
    MOCK_STAGE.with(|stage| { assert_eq!(stage.get(), 1); stage.set(2); });
    0
}

unsafe extern "system" fn mock_buffer_size(_: *mut IAudioClient, frames: *mut u32) -> i32 {
    MOCK_STAGE.with(|stage| { assert_eq!(stage.get(), 2); stage.set(3); });
    unsafe { *frames = 480; }
    0
}

unsafe extern "system" fn mock_period(_: *mut IAudioClient, period: *mut i64, _: *mut i64) -> i32 {
    MOCK_STAGE.with(|stage| { assert_eq!(stage.get(), 3); stage.set(4); });
    if MOCK_PERIOD_FAILS.with(Cell::get) {
        return AUDCLNT_E_DEVICE_INVALIDATED;
    }
    unsafe { *period = 100_000; }
    0
}

unsafe extern "system" fn mock_latency(_: *mut IAudioClient, latency: *mut i64) -> i32 {
    MOCK_STAGE.with(|stage| { assert_eq!(stage.get(), 4); stage.set(5); });
    unsafe { *latency = 100_000; }
    0
}

unsafe extern "system" fn unused_query(_: *mut IAudioClient, _: *const Guid, _: *mut *mut c_void) -> i32 { -1 }
unsafe extern "system" fn unused_ref(_: *mut IAudioClient) -> u32 { 1 }
unsafe extern "system" fn unused_status(_: *mut IAudioClient) -> i32 { -1 }
unsafe extern "system" fn unused_padding(_: *mut IAudioClient, _: *mut u32) -> i32 { -1 }
unsafe extern "system" fn unused_format(_: *mut IAudioClient, _: u32, _: *const WaveFormatEx, _: *mut *mut WaveFormatEx) -> i32 { -1 }
#[link(name = "ole32")]
unsafe extern "system" {
    fn CoTaskMemAlloc(size: usize) -> *mut c_void;
}
unsafe extern "system" fn mock_mix(_: *mut IAudioClient, output: *mut *mut WaveFormatEx) -> i32 {
    let channels = MOCK_CHANNELS.with(Cell::get);
    let mut mix = WasapiSpec { sample_rate: 44_100, channels }.wave_format().unwrap();
    if channels == 6 { mix.channel_mask = 0x60f; }
    let raw = unsafe { CoTaskMemAlloc(size_of::<format::WaveFormatExtensible>()) }.cast::<format::WaveFormatExtensible>();
    assert!(!raw.is_null());
    unsafe { raw.write_unaligned(mix); *output = raw.cast(); }
    0
}
unsafe extern "system" fn failed_mix(_: *mut IAudioClient, _: *mut *mut WaveFormatEx) -> i32 { AUDCLNT_E_DEVICE_INVALIDATED }
unsafe extern "system" fn null_mix(_: *mut IAudioClient, _: *mut *mut WaveFormatEx) -> i32 { 0 }

fn mock_vtbl() -> IAudioClientVtbl {
    IAudioClientVtbl {
        query_interface: unused_query,
        add_ref: unused_ref,
        release: unused_ref,
        initialize: mock_initialize,
        get_buffer_size: mock_buffer_size,
        get_stream_latency: mock_latency,
        get_current_padding: unused_padding,
        is_format_supported: unused_format,
        get_mix_format: mock_mix,
        get_device_period: mock_period,
        start: unused_status,
        stop: unused_status,
        reset: unused_status,
        set_event_handle: mock_set_event,
        get_service: unused_query,
    }
}

#[test]
fn shared_capture_and_render_query_actual_buffer_after_binding_event() {
    let vtbl = mock_vtbl();
    let mut client = IAudioClient { lp_vtbl: &vtbl };
    let spec = WasapiSpec { sample_rate: 48_000, channels: 2 };
    MOCK_PERIOD_FAILS.with(|fails| fails.set(false));
    for extra_flags in [0, AUDCLNT_STREAMFLAGS_LOOPBACK] {
        MOCK_STAGE.with(|stage| stage.set(0));
        let actual_frames = stream::initialize_shared_client(
            &mut client,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK | extra_flags,
            Handle(ptr::null_mut()),
            spec,
            "test",
        ).unwrap();
        assert_eq!(actual_frames, 480);
        MOCK_STAGE.with(|stage| assert_eq!(stage.get(), 5));
    }
}

#[test]
fn failed_period_query_propagates_without_starting_stream() {
    let vtbl = mock_vtbl();
    let mut client = IAudioClient { lp_vtbl: &vtbl };
    MOCK_STAGE.with(|stage| stage.set(0));
    MOCK_PERIOD_FAILS.with(|fails| fails.set(true));
    let result = stream::initialize_shared_client(
        &mut client,
        AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        Handle(ptr::null_mut()),
        WasapiSpec { sample_rate: 48_000, channels: 2 },
        "test",
    );
    assert!(result.is_err());
    MOCK_STAGE.with(|stage| assert_eq!(stage.get(), 4));
    MOCK_PERIOD_FAILS.with(|fails| fails.set(false));
}

#[test]
fn all_layouts_reach_capture_and_playback_initialize_with_native_51_mask() {
    let vtbl = mock_vtbl();
    let mut client = IAudioClient { lp_vtbl: &vtbl };
    for channels in [2, 6, 8] {
        MOCK_CHANNELS.with(|value| value.set(channels));
        for extra_flags in [0, AUDCLNT_STREAMFLAGS_LOOPBACK] {
            MOCK_STAGE.with(|value| value.set(0));
            let frames = stream::initialize_shared_client(&mut client,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK | extra_flags, Handle(ptr::null_mut()),
                WasapiSpec { sample_rate: 48_000, channels }, "test").unwrap();
            assert_eq!(frames, 480);
            MOCK_STAGE.with(|value| assert_eq!(value.get(), 5));
        }
    }
    MOCK_CHANNELS.with(|value| value.set(2));
}

#[test]
fn failed_or_null_mix_format_never_initializes_stream() {
    for get_mix_format in [failed_mix as unsafe extern "system" fn(_, _) -> _, null_mix] {
        let mut vtbl = mock_vtbl();
        vtbl.get_mix_format = get_mix_format;
        let mut client = IAudioClient { lp_vtbl: &vtbl };
        MOCK_STAGE.with(|value| value.set(0));
        let result = stream::initialize_shared_client(&mut client, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            Handle(ptr::null_mut()), WasapiSpec { sample_rate: 48_000, channels: 2 }, "test");
        assert!(matches!(result, Err(Error::Backend(_))));
        MOCK_STAGE.with(|value| assert_eq!(value.get(), 0));
    }
}

fn ring() -> SharedSampleRing {
    SharedSampleRing::new(8, 2, 2, 6, "测试音频队列已关闭").unwrap()
}

#[test]
fn recovery_discards_wrapped_samples_and_accepts_only_new_audio() {
    let ring = ring();
    ring.write_overwrite(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    let mut consumed = [0.0; 4];
    assert!(ring.read_exact(&mut consumed, Duration::ZERO).unwrap());
    ring.write_overwrite(&[7.0, 8.0, 9.0, 10.0]);
    assert_eq!(ring.begin_recovery().unwrap(), 6);
    ring.finish_recovery().unwrap();
    ring.write_blocking(&[11.0, 12.0], Duration::ZERO).unwrap();
    let mut output = [0.0; 4];
    assert_eq!(ring.read_partial_zero_fill(&mut output), 2);
    assert_eq!(output, [11.0, 12.0, 0.0, 0.0]);
}

#[test]
fn submissions_during_device_outage_do_not_accumulate() {
    let ring = ring();
    ring.begin_recovery().unwrap();
    for _ in 0..100 {
        ring.write_blocking(&[1.0, 2.0], Duration::ZERO).unwrap();
        ring.write_overwrite(&[3.0, 4.0]);
        ring.write_silence_overwrite(2);
    }
    ring.finish_recovery().unwrap();
    let mut output = [9.0; 2];
    assert_eq!(ring.read_partial_zero_fill(&mut output), 0);
    assert_eq!(output, [0.0, 0.0]);
}

#[test]
fn old_generation_stays_invalid_after_recovery_finishes() {
    let ring = ring();
    let old_generation = ring.lock_state().unwrap().generation;
    ring.begin_recovery().unwrap();
    ring.finish_recovery().unwrap();
    let state = ring.lock_state().unwrap();
    assert!(!state.accepts_write(old_generation));
    assert!(state.accepts_write(state.generation));
}

#[test]
fn recovery_does_not_reopen_a_closed_queue() {
    let ring = ring();
    ring.begin_recovery().unwrap();
    ring.close(Some("终止".into()));
    assert!(ring.finish_recovery().is_err());
    assert!(ring.begin_recovery().is_err());
    assert!(ring.write_blocking(&[1.0, 2.0], Duration::ZERO).is_err());
    assert!(ring.read_exact(&mut [0.0; 2], Duration::ZERO).is_err());
}

#[test]
fn oversized_frame_fails_without_waiting_for_impossible_capacity() {
    assert!(ring().write_blocking(&[0.0; 10], Duration::from_secs(1)).is_err());
}

#[test]
fn audio_events_auto_reset_but_stop_events_remain_signaled() {
    let audio = OwnedHandle::create_auto_reset(false).unwrap();
    let stop = OwnedHandle::create_manual_reset(false).unwrap();
    audio.set().unwrap();
    stop.set().unwrap();
    assert_eq!(wait_for_multiple_objects_timeout(&[audio.raw()], 0).unwrap(), Some(0));
    assert_eq!(wait_for_multiple_objects_timeout(&[audio.raw()], 0).unwrap(), None);
    assert_eq!(wait_for_multiple_objects_timeout(&[stop.raw()], 0).unwrap(), Some(0));
    assert_eq!(wait_for_multiple_objects_timeout(&[stop.raw()], 0).unwrap(), Some(0));
}
