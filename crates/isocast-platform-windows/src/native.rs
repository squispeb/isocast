use anyhow::Result;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, SetEvent};
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

pub(crate) struct Apartment;

impl Apartment {
    pub(crate) fn new() -> Result<Self> {
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }?;
        Ok(Self)
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe { RoUninitialize() };
    }
}

pub(crate) struct Event(HANDLE);

// SAFETY: an owned kernel event can be signalled/waited on from any thread;
// the handle remains open until all users of the enclosing Arc have left.
unsafe impl Send for Event {}
unsafe impl Sync for Event {}

impl Event {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None) }?))
    }

    pub(crate) fn handle(&self) -> HANDLE {
        self.0
    }

    pub(crate) fn signal(&self) {
        // This owned, unnamed event cannot be closed by another thread.
        let _ = unsafe { SetEvent(self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}
