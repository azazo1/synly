//! 系统已配对设备记录与 Synly SDP 服务查询.

use super::socket;
use crate::bluetooth::{BluetoothPeer, SERVICE_UUID_BYTES};
use std::io;
use windows_sys::Win32::Devices::Bluetooth::*;
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_ITEMS, GetLastError, HANDLE};
use windows_sys::Win32::Networking::WinSock::*;

pub(super) const SERVICE_GUID: windows_sys::core::GUID =
    windows_sys::core::GUID::from_u128(uuid::Uuid::from_bytes(SERVICE_UUID_BYTES).as_u128());

pub(super) fn parse_address(address: &str) -> io::Result<u64> {
    u64::from_str_radix(&address.replace(':', ""), 16).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "无效的蓝牙地址"))
}

fn address_text(address: u64) -> io::Result<String> {
    if address == 0 || address >= (1u64 << 48) { return Err(io::Error::new(io::ErrorKind::InvalidData, "系统返回了无效的蓝牙地址")); }
    let hex = format!("{address:012X}");
    Ok((0..6).map(|index| &hex[index * 2..index * 2 + 2]).collect::<Vec<_>>().join(":"))
}

fn peer(info: &BLUETOOTH_DEVICE_INFO) -> io::Result<BluetoothPeer> {
    let address = address_text(unsafe { info.Address.Anonymous.ullLong })?;
    let name_len = info.szName.iter().position(|&c| c == 0).unwrap_or(info.szName.len());
    let name = String::from_utf16_lossy(&info.szName[..name_len]);
    Ok(BluetoothPeer { name: if name.is_empty() { address.clone() } else { name }, address })
}

struct RadioSearch(HBLUETOOTH_RADIO_FIND);
impl Drop for RadioSearch {
    fn drop(&mut self) { unsafe { BluetoothFindRadioClose(self.0); } }
}

struct RadioHandle(HANDLE);
impl Drop for RadioHandle {
    fn drop(&mut self) { unsafe { CloseHandle(self.0); } }
}

pub(super) fn require_paired(address: u64) -> io::Result<BluetoothPeer> {
    let parameters = BLUETOOTH_FIND_RADIO_PARAMS { dwSize: size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32 };
    let mut radio = std::ptr::null_mut();
    let find = unsafe { BluetoothFindFirstRadio(&parameters, &mut radio) };
    if find.is_null() { return Err(io::Error::from_raw_os_error(unsafe { GetLastError() } as i32)); }
    let _search = RadioSearch(find);
    loop {
        let _radio = RadioHandle(radio);
        let mut info = BLUETOOTH_DEVICE_INFO {
            dwSize: size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
            Address: BLUETOOTH_ADDRESS { Anonymous: BLUETOOTH_ADDRESS_0 { ullLong: address } },
            ..Default::default()
        };
        if unsafe { BluetoothGetDeviceInfo(radio, &mut info) } == 0 && info.fAuthenticated != 0 { return peer(&info); }
        if unsafe { BluetoothFindNextRadio(find, &mut radio) } == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_NO_MORE_ITEMS { return Err(io::Error::from_raw_os_error(code as i32)); }
            break;
        }
    }
    Err(io::Error::new(io::ErrorKind::PermissionDenied, "设备尚未系统配对, 或配对记录已被移除"))
}

struct DeviceSearch(HBLUETOOTH_DEVICE_FIND);
impl Drop for DeviceSearch {
    fn drop(&mut self) { unsafe { BluetoothFindDeviceClose(self.0); } }
}

pub(super) fn paired_devices() -> io::Result<Vec<BluetoothPeer>> {
    let parameters = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: 1,
        // 不发起 inquiry, 不把未配对或仅被记住的设备当作已授权设备.
        fIssueInquiry: 0,
        ..Default::default()
    };
    let mut info = BLUETOOTH_DEVICE_INFO { dwSize: size_of::<BLUETOOTH_DEVICE_INFO>() as u32, ..Default::default() };
    let handle = unsafe { BluetoothFindFirstDevice(&parameters, &mut info) };
    if handle.is_null() {
        let code = unsafe { GetLastError() };
        return if code == ERROR_NO_MORE_ITEMS { Ok(Vec::new()) } else { Err(io::Error::from_raw_os_error(code as i32)) };
    }
    let _search = DeviceSearch(handle);
    let mut peers = Vec::new();
    for _ in 0..256 {
        if info.fAuthenticated != 0 {
            if let Ok(peer) = peer(&info) { peers.push(peer); }
        }
        if unsafe { BluetoothFindNextDevice(handle, &mut info) } == 0 {
            let code = unsafe { GetLastError() };
            if code != ERROR_NO_MORE_ITEMS { return Err(io::Error::from_raw_os_error(code as i32)); }
            break;
        }
    }
    peers.sort_by(|a, b| a.address.cmp(&b.address));
    peers.dedup_by(|a, b| a.address == b.address);
    Ok(peers)
}

struct Lookup(HANDLE);
impl Drop for Lookup {
    fn drop(&mut self) { unsafe { WSALookupServiceEnd(self.0); } }
}

fn within(buffer: &[usize], pointer: *const u8, bytes: usize) -> bool {
    let start = buffer.as_ptr() as usize;
    let end = start.saturating_add(std::mem::size_of_val(buffer));
    let address = pointer as usize;
    address >= start && address.checked_add(bytes).is_some_and(|limit| limit <= end)
}

fn returned_channel(buffer: &[usize], address: u64) -> io::Result<Option<u8>> {
    let result = unsafe { std::ptr::read(buffer.as_ptr().cast::<WSAQUERYSETW>()) };
    let count = result.dwNumberOfCsAddrs as usize;
    if count == 0 { return Ok(None); }
    if count > 32 || !within(buffer, result.lpcsaBuffer.cast(), count * size_of::<CSADDR_INFO>()) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "SDP 查询返回了无效的地址数组"));
    }
    for index in 0..count {
        let record = unsafe { std::ptr::read_unaligned(result.lpcsaBuffer.add(index)) };
        // 微软服务查询文档使用 LocalAddr, 部分 provider 使用 RemoteAddr; 两者都核对真实对端地址.
        for socket in [record.LocalAddr, record.RemoteAddr] {
            if socket.iSockaddrLength < size_of::<SOCKADDR_BTH>() as i32 { continue; }
            if !within(buffer, socket.lpSockaddr.cast(), socket.iSockaddrLength as usize) {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "SDP 查询返回了缓冲区之外的 socket 地址"));
            }
            let found = unsafe { std::ptr::read_unaligned(socket.lpSockaddr.cast::<SOCKADDR_BTH>()) };
            let channel = found.port;
            if found.addressFamily == AF_BTH && found.btAddr == address && (1..=30).contains(&channel) {
                return Ok(Some(channel as u8));
            }
        }
    }
    Ok(None)
}

fn query_error(operation: &str, error: io::Error) -> io::Error {
    // 保留系统码的文字表示和失败 API, 不再把不同的 SDP 错误归为权限或离线.
    io::Error::new(error.kind(), format!("{operation}: {error}"))
}

pub(super) fn query_service(address: u64) -> io::Result<Option<u8>> {
    socket::initialize().map_err(|error| query_error("WSAStartup", error))?;
    require_paired(address).map_err(|error| query_error("系统配对记录", error))?;
    let target = SOCKADDR_BTH { addressFamily: AF_BTH, btAddr: address, ..Default::default() };
    let mut context = [0u16; 128];
    let mut context_len = context.len() as u32;
    if unsafe { WSAAddressToStringW((&target as *const SOCKADDR_BTH).cast(), size_of::<SOCKADDR_BTH>() as u32, std::ptr::null(), context.as_mut_ptr(), &mut context_len) } == SOCKET_ERROR {
        return Err(query_error("WSAAddressToStringW", socket::last_error()));
    }
    let mut service = SERVICE_GUID;
    let restriction = WSAQUERYSETW {
        dwSize: size_of::<WSAQUERYSETW>() as u32,
        lpServiceClassId: &mut service,
        dwNameSpace: NS_BTH,
        lpszContext: context.as_mut_ptr(),
        ..Default::default()
    };
    let mut handle = std::ptr::null_mut();
    if unsafe { WSALookupServiceBeginW(&restriction, LUP_FLUSHCACHE | LUP_RETURN_ADDR | LUP_RETURN_TYPE, &mut handle) } == SOCKET_ERROR {
        let error = socket::last_error();
        return if error.raw_os_error() == Some(WSASERVICE_NOT_FOUND) { Ok(None) } else { Err(query_error("WSALookupServiceBeginW", error)) };
    }
    let _lookup = Lookup(handle);
    let mut buffer = vec![0usize; 4096 / size_of::<usize>()];
    for _ in 0..128 {
        buffer.fill(0);
        let output = buffer.as_mut_ptr().cast::<WSAQUERYSETW>();
        unsafe { std::ptr::write(output, WSAQUERYSETW { dwSize: size_of::<WSAQUERYSETW>() as u32, ..Default::default() }) };
        let mut size = std::mem::size_of_val(buffer.as_slice()) as u32;
        if unsafe { WSALookupServiceNextW(handle, LUP_RETURN_ADDR, &mut size, output) } == 0 {
            if let Some(channel) = returned_channel(&buffer, address).map_err(|error| query_error("RFCOMM 通道解析", error))? { return Ok(Some(channel)); }
            continue;
        }
        let error = socket::last_error();
        match error.raw_os_error() {
            Some(WSA_E_NO_MORE | WSAENOMORE | WSASERVICE_NOT_FOUND) => return Ok(None),
            Some(WSAEFAULT) if size as usize > std::mem::size_of_val(buffer.as_slice()) && size <= 64 * 1024 => {
                buffer.resize((size as usize).div_ceil(size_of::<usize>()), 0);
            }
            _ => return Err(query_error("WSALookupServiceNextW", error)),
        }
    }
    Err(io::Error::new(io::ErrorKind::InvalidData, "SDP 查询返回了过多的服务记录"))
}

pub(super) struct Advertisement {
    local: SOCKADDR_BTH,
    registered: bool,
}

impl Advertisement {
    pub fn register(local: SOCKADDR_BTH) -> io::Result<Self> {
        let mut advertisement = Self { local, registered: false };
        advertisement.apply(RNRSERVICE_REGISTER)?;
        advertisement.registered = true;
        Ok(advertisement)
    }

    fn apply(&self, operation: WSAESETSERVICEOP) -> io::Result<()> {
        let mut local = self.local;
        let mut service = SERVICE_GUID;
        let mut name = "Synly".encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut endpoint = CSADDR_INFO {
            LocalAddr: SOCKET_ADDRESS { lpSockaddr: (&mut local as *mut SOCKADDR_BTH).cast(), iSockaddrLength: size_of::<SOCKADDR_BTH>() as i32 },
            iSocketType: SOCK_STREAM,
            iProtocol: BTHPROTO_RFCOMM as i32,
            ..Default::default()
        };
        let registration = WSAQUERYSETW {
            dwSize: size_of::<WSAQUERYSETW>() as u32,
            lpszServiceInstanceName: name.as_mut_ptr(),
            lpServiceClassId: &mut service,
            dwNameSpace: NS_BTH,
            dwNumberOfCsAddrs: 1,
            lpcsaBuffer: &mut endpoint,
            ..Default::default()
        };
        if unsafe { WSASetServiceW(&registration, operation, 0) } == SOCKET_ERROR { Err(socket::last_error()) } else { Ok(()) }
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if self.registered {
            if let Err(error) = self.apply(RNRSERVICE_DELETE) { tracing::warn!(%error, "注销 Synly 蓝牙 SDP 服务失败"); }
        }
    }
}
