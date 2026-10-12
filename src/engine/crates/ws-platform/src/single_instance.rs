//! Single-instance guard backed by a named mutex.

use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

const ENGINE_MUTEX_NAME: windows::core::PCWSTR = w!("Local\\WinSearch.Engine.v1");

/// Holds a named mutex for the process lifetime. `acquire` returns `None` when
/// another instance already holds it.
#[derive(Debug)]
pub struct SingleInstanceGuard {
    handle: HANDLE,
}

// The mutex handle is owned exclusively by this guard and only ever closed
// once at drop.
unsafe impl Send for SingleInstanceGuard {}

impl SingleInstanceGuard {
    pub fn acquire_engine() -> Option<Self> {
        Self::acquire(ENGINE_MUTEX_NAME)
    }

    pub fn acquire(name: windows::core::PCWSTR) -> Option<Self> {
        unsafe {
            let handle = match CreateMutexW(None, false, name) {
                Ok(h) => h,
                Err(_) => return None,
            };
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
