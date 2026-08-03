use anyhow::{Context, Result, bail};
use std::io::{Error, ErrorKind};
use std::ptr;
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
    GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, OPEN_EXISTING,
    PIPE_ACCESS_DUPLEX, PIPE_ACCESS_INBOUND, PIPE_ACCESS_OUTBOUND, ReadFile, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};

const PIPE_BUFFER_SIZE: u32 = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) enum PipeDirection {
    ServerToClient,
    ClientToServer,
    Duplex,
}

pub(crate) struct NativePipe {
    handle: HANDLE,
    event: HANDLE,
    server: bool,
}

unsafe impl Send for NativePipe {}

impl NativePipe {
    pub(crate) fn create_server(
        name: &str,
        direction: PipeDirection,
        security: *const SECURITY_ATTRIBUTES,
    ) -> Result<Self> {
        let name = wide(name);
        let access = match direction {
            PipeDirection::ServerToClient => PIPE_ACCESS_OUTBOUND,
            PipeDirection::ClientToServer => PIPE_ACCESS_INBOUND,
            PipeDirection::Duplex => PIPE_ACCESS_DUPLEX,
        } | FILE_FLAG_OVERLAPPED
            | FILE_FLAG_FIRST_PIPE_INSTANCE;
        let mode = PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS;
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                access,
                mode,
                1,
                PIPE_BUFFER_SIZE,
                PIPE_BUFFER_SIZE,
                0,
                security,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::last_os_error()).context("failed to create Windows input agent pipe");
        }
        Self::from_handle(handle, true)
    }

    pub(crate) fn connect_client(
        name: &str,
        direction: PipeDirection,
        timeout: Duration,
    ) -> Result<Self> {
        let name = wide(name);
        let access = match direction {
            PipeDirection::ServerToClient => GENERIC_READ,
            PipeDirection::ClientToServer => GENERIC_WRITE,
            PipeDirection::Duplex => GENERIC_READ | GENERIC_WRITE,
        };
        let deadline = Instant::now() + timeout;
        loop {
            let handle = unsafe {
                CreateFileW(
                    name.as_ptr(),
                    access,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            if handle != INVALID_HANDLE_VALUE {
                return Self::from_handle(handle, false);
            }
            let error = Error::last_os_error();
            let retryable = matches!(
                error.raw_os_error(),
                Some(code)
                    if code == ERROR_FILE_NOT_FOUND as i32 || code == ERROR_PIPE_BUSY as i32
            );
            if !retryable || Instant::now() >= deadline {
                return Err(error).context("failed to connect Windows input agent pipe");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait_ms = duration_millis(remaining.min(Duration::from_millis(100)));
            unsafe {
                WaitNamedPipeW(name.as_ptr(), wait_ms);
            }
        }
    }

    fn from_handle(handle: HANDLE, server: bool) -> Result<Self> {
        let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
        if event.is_null() {
            unsafe {
                CloseHandle(handle);
            }
            return Err(Error::last_os_error()).context("failed to create Windows pipe event");
        }
        Ok(Self {
            handle,
            event,
            server,
        })
    }

    pub(crate) fn connect_server(&mut self, timeout: Duration) -> Result<()> {
        self.reset_event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        let connected = unsafe { ConnectNamedPipe(self.handle, &mut overlapped) };
        if connected != 0 {
            return Ok(());
        }
        let error = Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == ERROR_PIPE_CONNECTED as i32 => Ok(()),
            Some(code) if code == ERROR_IO_PENDING as i32 => {
                self.wait_overlapped(&mut overlapped, timeout).map(|_| ())
            }
            _ => Err(error).context("failed to connect Windows input agent server pipe"),
        }
    }

    pub(crate) fn read_exact(&mut self, bytes: &mut [u8], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut offset = 0usize;
        while offset < bytes.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Windows input agent pipe read timed out");
            }
            let transferred = self.read_once(&mut bytes[offset..], remaining)?;
            if transferred == 0 {
                return Err(Error::new(ErrorKind::UnexpectedEof, "Windows input agent pipe closed"))
                    .context("failed to read Windows input agent pipe");
            }
            offset = offset.saturating_add(transferred);
        }
        Ok(())
    }

    pub(crate) fn write_all(&mut self, bytes: &[u8], timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let mut offset = 0usize;
        while offset < bytes.len() {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                bail!("Windows input agent pipe write timed out");
            }
            let transferred = self.write_once(&bytes[offset..], remaining)?;
            if transferred == 0 {
                return Err(Error::new(ErrorKind::WriteZero, "Windows input agent pipe closed"))
                    .context("failed to write Windows input agent pipe");
            }
            offset = offset.saturating_add(transferred);
        }
        Ok(())
    }

    fn read_once(&mut self, bytes: &mut [u8], timeout: Duration) -> Result<usize> {
        self.reset_event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        let mut immediate = 0u32;
        let result = unsafe {
            ReadFile(
                self.handle,
                bytes.as_mut_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut immediate,
                &mut overlapped,
            )
        };
        if result != 0 {
            return Ok(immediate as usize);
        }
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error).context("failed to read Windows input agent pipe");
        }
        self.wait_overlapped(&mut overlapped, timeout)
    }

    fn write_once(&mut self, bytes: &[u8], timeout: Duration) -> Result<usize> {
        self.reset_event()?;
        let mut overlapped = OVERLAPPED {
            hEvent: self.event,
            ..Default::default()
        };
        let mut immediate = 0u32;
        let result = unsafe {
            WriteFile(
                self.handle,
                bytes.as_ptr(),
                bytes.len().min(u32::MAX as usize) as u32,
                &mut immediate,
                &mut overlapped,
            )
        };
        if result != 0 {
            return Ok(immediate as usize);
        }
        let error = Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_IO_PENDING as i32) {
            return Err(error).context("failed to write Windows input agent pipe");
        }
        self.wait_overlapped(&mut overlapped, timeout)
    }

    fn wait_overlapped(&mut self, overlapped: &mut OVERLAPPED, timeout: Duration) -> Result<usize> {
        match unsafe { WaitForSingleObject(self.event, duration_millis(timeout)) } {
            WAIT_OBJECT_0 => {
                let mut transferred = 0u32;
                if unsafe { GetOverlappedResult(self.handle, overlapped, &mut transferred, 0) } == 0
                {
                    return Err(Error::last_os_error())
                        .context("Windows input agent pipe operation failed");
                }
                Ok(transferred as usize)
            }
            WAIT_TIMEOUT => {
                unsafe {
                    CancelIoEx(self.handle, overlapped);
                }
                let mut transferred = 0u32;
                unsafe {
                    GetOverlappedResult(self.handle, overlapped, &mut transferred, 1);
                }
                Err(Error::new(ErrorKind::TimedOut, "Windows input agent pipe operation timed out"))
                    .context("Windows input agent pipe operation timed out")
            }
            _ => Err(Error::last_os_error()).context("failed to wait for Windows pipe operation"),
        }
    }

    fn reset_event(&self) -> Result<()> {
        if unsafe { ResetEvent(self.event) } == 0 {
            return Err(Error::last_os_error()).context("failed to reset Windows pipe event");
        }
        Ok(())
    }

    pub(crate) fn raw_handle(&self) -> HANDLE {
        self.handle
    }
}

impl Drop for NativePipe {
    fn drop(&mut self) {
        unsafe {
            CancelIoEx(self.handle, ptr::null());
            if self.server {
                DisconnectNamedPipe(self.handle);
            }
            CloseHandle(self.event);
            CloseHandle(self.handle);
        }
    }
}

fn duration_millis(duration: Duration) -> u32 {
    duration.as_millis().clamp(1, u128::from(u32::MAX - 1)) as u32
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
