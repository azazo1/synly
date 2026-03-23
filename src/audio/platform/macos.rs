use crate::audio::capture::{AudioInput, CaptureStatus};
use crate::audio::config::{CaptureConfig, PlaybackConfig, StreamParams};
use crate::audio::error::{Error, Result};
use crate::audio::playback::AudioOutput;
use std::ffi::CStr;
use std::os::raw::{c_char, c_int, c_void};
use std::ptr::NonNull;
use std::time::Duration;

pub fn open_input(config: &CaptureConfig, stream: &StreamParams) -> Result<Box<dyn AudioInput>> {
    if config.device_name.is_some() {
        return Err(Error::Backend(
            "macOS capture now uses system audio tap; selecting a specific device is not implemented yet"
                .into(),
        ));
    }
    if stream.channels != 2 {
        return Err(Error::UnsupportedPlatform(
            "macOS system audio capture is currently limited to stereo",
        ));
    }

    let handle = unsafe {
        ar_macos_capture_create(
            std::ptr::null(),
            stream.sample_rate,
            2,
            stream.frame_size() as u32,
        )
    };
    let handle = NonNull::new(handle).ok_or_else(last_backend_error)?;
    Ok(Box::new(MacosInput { handle }))
}

pub fn open_output(
    _config: &PlaybackConfig,
    stream: &StreamParams,
) -> Result<Box<dyn AudioOutput>> {
    let handle = unsafe {
        ar_macos_playback_create(
            stream.sample_rate,
            stream.channels as u32,
            stream.frame_size() as u32,
        )
    };
    let handle = NonNull::new(handle).ok_or_else(last_backend_error)?;
    Ok(Box::new(MacosOutput { handle }))
}

struct MacosInput {
    handle: NonNull<c_void>,
}

unsafe impl Send for MacosInput {}

impl Drop for MacosInput {
    fn drop(&mut self) {
        unsafe { ar_macos_capture_destroy(self.handle.as_ptr()) };
    }
}

impl AudioInput for MacosInput {
    fn read_frame(&mut self, frame: &mut [f32], timeout: Duration) -> Result<CaptureStatus> {
        let result = unsafe {
            ar_macos_capture_read(
                self.handle.as_ptr(),
                frame.as_mut_ptr(),
                frame.len() as u32,
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };
        match result {
            0 => Ok(CaptureStatus::Ok),
            1 => Ok(CaptureStatus::Timeout),
            _ => Err(last_backend_error()),
        }
    }
}

struct MacosOutput {
    handle: NonNull<c_void>,
}

unsafe impl Send for MacosOutput {}

impl Drop for MacosOutput {
    fn drop(&mut self) {
        unsafe { ar_macos_playback_destroy(self.handle.as_ptr()) };
    }
}

impl AudioOutput for MacosOutput {
    fn submit_frame(&mut self, frame: &[f32], timeout: Duration) -> Result<()> {
        let result = unsafe {
            ar_macos_playback_submit(
                self.handle.as_ptr(),
                frame.as_ptr(),
                frame.len() as u32,
                timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(last_backend_error())
        }
    }
}

fn last_backend_error() -> Error {
    unsafe {
        let message = ar_macos_last_error();
        if message.is_null() {
            Error::Backend("unknown macOS audio backend error".into())
        } else {
            Error::Backend(CStr::from_ptr(message).to_string_lossy().into_owned())
        }
    }
}

unsafe extern "C" {
    fn ar_macos_last_error() -> *const c_char;

    fn ar_macos_capture_create(
        device_name: *const c_char,
        sample_rate: u32,
        channels: u32,
        frame_size: u32,
    ) -> *mut c_void;

    fn ar_macos_capture_destroy(handle: *mut c_void);

    fn ar_macos_capture_read(
        handle: *mut c_void,
        out_samples: *mut f32,
        sample_count: u32,
        timeout_ms: u32,
    ) -> c_int;

    fn ar_macos_playback_create(sample_rate: u32, channels: u32, frame_size: u32) -> *mut c_void;

    fn ar_macos_playback_destroy(handle: *mut c_void);

    fn ar_macos_playback_submit(
        handle: *mut c_void,
        samples: *const f32,
        sample_count: u32,
        timeout_ms: u32,
    ) -> c_int;
}
