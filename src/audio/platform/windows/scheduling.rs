use std::ffi::c_void;

type RegisterTask = unsafe extern "system" fn(*const u16, *mut u32) -> *mut c_void;
type RevertTask = unsafe extern "system" fn(*mut c_void) -> i32;

// 仅在创建它的音频线程上持有和释放, 不通过可跨线程的 Handle 包装.
pub(super) struct MmcssTask {
    handle: *mut c_void,
    revert: RevertTask,
}

impl MmcssTask {
    pub(super) fn register() -> Option<Self> {
        let task = Self::register_with(AvSetMmThreadCharacteristicsW, AvRevertMmThreadCharacteristics);
        if task.is_none() {
            tracing::warn!(error = %std::io::Error::last_os_error(), "注册 Pro Audio MMCSS 失败, 继续使用普通线程优先级");
        }
        task
    }

    fn register_with(register: RegisterTask, revert: RevertTask) -> Option<Self> {
        let task_name: [u16; 10] = [80, 114, 111, 32, 65, 117, 100, 105, 111, 0];
        let mut task_index = 0;
        let handle = unsafe { register(task_name.as_ptr(), &mut task_index) };
        if handle.is_null() {
            None
        } else {
            Some(Self { handle, revert })
        }
    }
}

impl Drop for MmcssTask {
    fn drop(&mut self) {
        if unsafe { (self.revert)(self.handle) } == 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "撤销 Pro Audio MMCSS 失败");
        }
    }
}

#[link(name = "avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: *const u16, task_index: *mut u32) -> *mut c_void;
    fn AvRevertMmThreadCharacteristics(task_handle: *mut c_void) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static REVERT_COUNT: Cell<usize> = const { Cell::new(0) };
    }

    unsafe extern "system" fn register_ok(name: *const u16, index: *mut u32) -> *mut c_void {
        let expected = [80, 114, 111, 32, 65, 117, 100, 105, 111, 0];
        assert_eq!(unsafe { std::slice::from_raw_parts(name, 10) }, expected);
        assert_eq!(unsafe { *index }, 0);
        std::ptr::dangling_mut::<u8>().cast()
    }

    unsafe extern "system" fn register_failed(_: *const u16, _: *mut u32) -> *mut c_void {
        std::ptr::null_mut()
    }

    unsafe extern "system" fn revert(handle: *mut c_void) -> i32 {
        assert_eq!(handle, std::ptr::dangling_mut::<u8>().cast());
        REVERT_COUNT.with(|count| count.set(count.get() + 1));
        1
    }

    #[test]
    fn registration_is_reverted_once_on_error_exit() {
        REVERT_COUNT.with(|count| count.set(0));
        fn operation() -> Result<(), ()> {
            let _task = MmcssTask::register_with(register_ok, revert).unwrap();
            Err(())
        }
        assert!(operation().is_err());
        REVERT_COUNT.with(|count| assert_eq!(count.get(), 1));
    }

    #[test]
    fn failed_registration_is_not_reverted() {
        REVERT_COUNT.with(|count| count.set(0));
        assert!(MmcssTask::register_with(register_failed, revert).is_none());
        REVERT_COUNT.with(|count| assert_eq!(count.get(), 0));
    }
}
