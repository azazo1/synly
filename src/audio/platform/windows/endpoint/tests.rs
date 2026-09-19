use super::*;
use super::super::{Guid, IMMDeviceEnumeratorVtbl, IMMDeviceVtbl, PropVariant};
use std::cell::{Cell, RefCell};
use std::ffi::c_void;

#[repr(C)]
struct FakeDevice {
    interface: IMMDevice,
    state: Cell<u32>,
    state_result: Cell<i32>,
    state_queries: Cell<usize>,
    releases: Cell<usize>,
}
#[repr(C)]
struct FakeEnumerator {
    interface: IMMDeviceEnumerator,
    device: Box<FakeDevice>,
    lookup_result: Cell<i32>,
    return_null: Cell<bool>,
    default_queries: Cell<usize>,
    fixed_queries: Cell<usize>,
    selected_id: RefCell<Vec<u16>>,
    releases: Cell<usize>,
}

unsafe extern "system" fn enum_query(_: *mut IMMDeviceEnumerator, _: *const Guid, _: *mut *mut c_void) -> i32 { -1 }
unsafe extern "system" fn enum_addref(_: *mut IMMDeviceEnumerator) -> u32 { 1 }
unsafe extern "system" fn enum_release(raw: *mut IMMDeviceEnumerator) -> u32 {
    let fake = unsafe { &*raw.cast::<FakeEnumerator>() };
    fake.releases.set(fake.releases.get() + 1);
    0
}
unsafe extern "system" fn enumerate(_: *mut IMMDeviceEnumerator, _: u32, _: u32, _: *mut *mut c_void) -> i32 { -1 }
unsafe fn return_device(fake: &FakeEnumerator, output: *mut *mut IMMDevice) -> i32 {
    if fake.lookup_result.get() < 0 { return fake.lookup_result.get(); }
    unsafe {
        *output = if fake.return_null.get() { ptr::null_mut() }
                  else { (&fake.device.interface as *const IMMDevice).cast_mut() };
    }
    0
}
unsafe extern "system" fn default_device(raw: *mut IMMDeviceEnumerator, flow: u32, role: u32, output: *mut *mut IMMDevice) -> i32 {
    assert_eq!(flow, E_RENDER);
    assert_eq!(role, E_CONSOLE);
    let fake = unsafe { &*raw.cast::<FakeEnumerator>() };
    fake.default_queries.set(fake.default_queries.get() + 1);
    unsafe { return_device(fake, output) }
}
unsafe extern "system" fn fixed_device(raw: *mut IMMDeviceEnumerator, id: *const u16, output: *mut *mut IMMDevice) -> i32 {
    let fake = unsafe { &*raw.cast::<FakeEnumerator>() };
    fake.fixed_queries.set(fake.fixed_queries.get() + 1);
    let mut selected = fake.selected_id.borrow_mut();
    selected.clear();
    for index in 0..4096 {
        let code = unsafe { *id.add(index) };
        selected.push(code);
        if code == 0 { break; }
    }
    assert_eq!(selected.last(), Some(&0));
    unsafe { return_device(fake, output) }
}
unsafe extern "system" fn notify(_: *mut IMMDeviceEnumerator, _: *mut c_void) -> i32 { -1 }
static ENUMERATOR_VTABLE: IMMDeviceEnumeratorVtbl = IMMDeviceEnumeratorVtbl {
    query_interface: enum_query, add_ref: enum_addref, release: enum_release,
    enum_audio_endpoints: enumerate, get_default_audio_endpoint: default_device, get_device: fixed_device,
    register_endpoint_notification_callback: notify, unregister_endpoint_notification_callback: notify,
};
unsafe extern "system" fn device_query(_: *mut IMMDevice, _: *const Guid, _: *mut *mut c_void) -> i32 { -1 }
unsafe extern "system" fn device_addref(_: *mut IMMDevice) -> u32 { 1 }
unsafe extern "system" fn device_release(raw: *mut IMMDevice) -> u32 {
    let fake = unsafe { &*raw.cast::<FakeDevice>() };
    fake.releases.set(fake.releases.get() + 1);
    0
}
unsafe extern "system" fn activate(_: *mut IMMDevice, _: *const Guid, _: u32, _: *mut PropVariant, _: *mut *mut c_void) -> i32 { -1 }
unsafe extern "system" fn properties(_: *mut IMMDevice, _: u32, _: *mut *mut c_void) -> i32 { -1 }
unsafe extern "system" fn device_id(_: *mut IMMDevice, _: *mut *mut u16) -> i32 { -1 }
unsafe extern "system" fn device_state(raw: *mut IMMDevice, output: *mut u32) -> i32 {
    let fake = unsafe { &*raw.cast::<FakeDevice>() };
    fake.state_queries.set(fake.state_queries.get() + 1);
    if fake.state_result.get() < 0 { return fake.state_result.get(); }
    unsafe { *output = fake.state.get(); }
    0
}
static DEVICE_VTABLE: IMMDeviceVtbl = IMMDeviceVtbl {
    query_interface: device_query, add_ref: device_addref, release: device_release,
    activate, open_property_store: properties, get_id: device_id, get_state: device_state,
};
fn fixture() -> Box<FakeEnumerator> {
    Box::new(FakeEnumerator {
        interface: IMMDeviceEnumerator { lp_vtbl: &ENUMERATOR_VTABLE },
        device: Box::new(FakeDevice {
            interface: IMMDevice { lp_vtbl: &DEVICE_VTABLE }, state: Cell::new(DEVICE_STATE_ACTIVE),
            state_result: Cell::new(0), state_queries: Cell::new(0), releases: Cell::new(0),
        }),
        lookup_result: Cell::new(0), return_null: Cell::new(false),
        default_queries: Cell::new(0), fixed_queries: Cell::new(0),
        selected_id: RefCell::new(Vec::new()), releases: Cell::new(0),
    })
}
fn enumerator(fake: &FakeEnumerator) -> ComPtr<IMMDeviceEnumerator> {
    ComPtr::from_raw((&fake.interface as *const IMMDeviceEnumerator).cast_mut()).unwrap()
}

#[test]
fn selection_preserves_opaque_utf16_id_and_rejects_empty_or_nul() {
    assert!(EndpointSelection::parse(None).unwrap().follows_default());
    for invalid in ["", "a\0b", "\0"] {
        assert!(matches!(EndpointSelection::parse(Some(invalid)), Err(Error::InvalidConfig(_))));
    }
    let id = " {0.0.0.00000000}.{设备-🎵} ";
    let fixed = EndpointSelection::parse(Some(id)).unwrap();
    assert!(!fixed.follows_default());
    assert_eq!(fixed, EndpointSelection::Fixed(id.encode_utf16().chain(Some(0)).collect()));
}
#[test]
fn default_and_fixed_use_distinct_com_methods_and_release_once() {
    let fake = fixture();
    let enumerator = enumerator(&fake);
    drop(select(&enumerator, &EndpointSelection::Default).unwrap());
    let selection = EndpointSelection::parse(Some("{设备-id}")).unwrap();
    drop(select(&enumerator, &selection).unwrap());
    assert_eq!(fake.default_queries.get(), 1);
    assert_eq!(fake.fixed_queries.get(), 1);
    assert_eq!(*fake.selected_id.borrow(), "{设备-id}".encode_utf16().chain(Some(0)).collect::<Vec<_>>());
    assert_eq!(fake.device.state_queries.get(), 2);
    assert_eq!(fake.device.releases.get(), 2);
    drop(enumerator);
    assert_eq!(fake.releases.get(), 1);
}
#[test]
fn missing_fixed_endpoint_never_falls_back_to_default() {
    let fake = fixture();
    fake.lookup_result.set(-1);
    let selection = EndpointSelection::parse(Some("missing-id")).unwrap();
    assert!(matches!(select(&enumerator(&fake), &selection), Err(Error::Backend(_))));
    assert_eq!(fake.fixed_queries.get(), 1);
    assert_eq!(fake.default_queries.get(), 0);
    assert_eq!(fake.device.state_queries.get(), 0);
    assert_eq!(fake.device.releases.get(), 0);
}
#[test]
fn inactive_endpoints_are_rejected_without_leaking_returned_interface() {
    for state in [0, 2, 4, 8, 15] {
        let fake = fixture();
        fake.device.state.set(state);
        let fixed = EndpointSelection::parse(Some("offline-id")).unwrap();
        assert!(matches!(select(&enumerator(&fake), &fixed), Err(Error::Backend(_))));
        assert_eq!(fake.device.releases.get(), 1);
        assert_eq!(fake.default_queries.get(), 0);
    }
}
#[test]
fn state_query_failure_releases_device() {
    let fake = fixture();
    fake.device.state_result.set(-1);
    assert!(matches!(select(&enumerator(&fake), &EndpointSelection::Default), Err(Error::Backend(_))));
    assert_eq!(fake.device.releases.get(), 1);
}
#[test]
fn successful_lookup_with_null_interface_is_rejected() {
    let fake = fixture();
    fake.return_null.set(true);
    assert!(matches!(select(&enumerator(&fake), &EndpointSelection::Default), Err(Error::Backend(_))));
    assert_eq!(fake.device.state_queries.get(), 0);
}
#[test]
fn reopening_fixed_selection_uses_original_id_after_unavailability() {
    let fake = fixture();
    let selection = EndpointSelection::parse(Some("fixed-id")).unwrap();
    drop(select(&enumerator(&fake), &selection).unwrap());
    fake.device.state.set(8);
    assert!(select(&enumerator(&fake), &selection).is_err());
    fake.device.state.set(DEVICE_STATE_ACTIVE);
    drop(select(&enumerator(&fake), &selection).unwrap());
    assert_eq!(fake.fixed_queries.get(), 3);
    assert_eq!(fake.default_queries.get(), 0);
    assert_eq!(*fake.selected_id.borrow(), "fixed-id".encode_utf16().chain(Some(0)).collect::<Vec<_>>());
}
#[test]
fn fixed_binding_does_not_query_or_follow_default_device() {
    let mut last = Instant::now();
    let original = last;
    for seconds in [0, 1, 100, 10000] {
        assert!(!should_rebind(None, &mut last, original + Duration::from_secs(seconds), || panic!("固定 endpoint 不应查询默认设备")));
        assert_eq!(last, original);
    }
}
#[test]
fn default_binding_polls_on_schedule_and_recovers_from_query_failure() {
    let now = Instant::now();
    let interval = Duration::from_millis(u64::from(DEVICE_REBIND_POLL_MS));
    let mut last = now;
    assert!(!should_rebind(Some("a"), &mut last, now + interval - Duration::from_nanos(1), || panic!("尚未到轮询时刻")));
    assert!(!should_rebind(Some("a"), &mut last, now + interval, || Ok("a".into())));
    assert_eq!(last, now + interval);
    assert!(should_rebind(Some("a"), &mut last, now + interval * 2, || Ok("b".into())));
    assert!(should_rebind(Some("a"), &mut last, now + interval * 3, || Err(Error::Backend("默认设备不可用".into()))));
    assert!(!should_rebind(Some("a"), &mut last, now, || panic!("倒退时间不应触发轮询")));
}
