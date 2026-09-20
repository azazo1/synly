// 初始化完成后才发布引用, 最后一次退出完成前禁止重新初始化.
use crate::audio::error::{Error, Result};
use std::sync::Mutex;

pub(super) struct RuntimeUsers(Mutex<usize>);

impl RuntimeUsers {
    pub(super) const fn new() -> Self { Self(Mutex::new(0)) }

    pub(super) fn acquire(&self, init: impl FnOnce() -> Result<()>) -> Result<()> {
        let mut users = self.0.lock().map_err(|_| Error::BackendFatal("SDL2 生命周期锁已损坏".into()))?;
        let next = users.checked_add(1).ok_or_else(|| Error::BackendFatal("SDL2 引用计数溢出".into()))?;
        if *users == 0 { init()?; }
        *users = next;
        Ok(())
    }

    pub(super) fn release(&self, quit: impl FnOnce()) {
        // 即使此前发生 panic, 已经持有引用的设备仍需要释放子系统.
        let mut users = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(*users > 0, "SDL2 生命周期引用不平衡");
        *users -= 1;
        if *users == 0 { quit(); }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, atomic::{AtomicBool, AtomicUsize, Ordering}};

    #[test]
    fn failure_does_not_publish_and_last_release_holds_lock() {
        let users = RuntimeUsers::new();
        assert!(users.acquire(|| {
            assert!(users.0.try_lock().is_err());
            Err(Error::Backend("模拟初始化失败".into()))
        }).is_err());
        assert_eq!(*users.0.lock().unwrap(), 0);
        users.acquire(|| { assert!(users.0.try_lock().is_err()); Ok(()) }).unwrap();
        users.acquire(|| panic!("已有引用不应重复初始化")).unwrap();
        users.release(|| panic!("尚有引用不应退出"));
        users.release(|| assert!(users.0.try_lock().is_err()));
        assert_eq!(*users.0.lock().unwrap(), 0);
    }

    #[test]
    fn concurrent_users_never_observe_uninitialized_subsystem() {
        let users = RuntimeUsers::new();
        let live = AtomicBool::new(false);
        let starts = AtomicUsize::new(0);
        let stops = AtomicUsize::new(0);
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    barrier.wait();
                    for _ in 0..1000 {
                        users.acquire(|| {
                            assert!(!live.swap(true, Ordering::SeqCst));
                            starts.fetch_add(1, Ordering::SeqCst);
                            std::thread::yield_now();
                            Ok(())
                        }).unwrap();
                        assert!(live.load(Ordering::SeqCst));
                        std::thread::yield_now();
                        users.release(|| {
                            std::thread::yield_now();
                            assert!(live.swap(false, Ordering::SeqCst));
                            stops.fetch_add(1, Ordering::SeqCst);
                        });
                    }
                });
            }
        });
        assert!(!live.load(Ordering::SeqCst));
        assert!(starts.load(Ordering::SeqCst) > 0);
        assert_eq!(starts.load(Ordering::SeqCst), stops.load(Ordering::SeqCst));
        assert_eq!(*users.0.lock().unwrap(), 0);
    }
}
