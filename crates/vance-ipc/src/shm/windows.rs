//! Windows shared memory: a named section backed by the paging file
//! (`CreateFileMappingW` on `INVALID_HANDLE_VALUE`) and a view of it.
//!
//! A section has no name to remove. It lives until its last handle and view
//! are gone, so `unlink` is a no-op. Windows commits the whole size when the
//! section is created, so creation fails up front when memory is short.

use super::Access;
use std::io;
use std::mem::MaybeUninit;
use std::ptr;
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, ERROR_ALREADY_EXISTS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP,
    FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS,
    PAGE_READWRITE,
};

/// A section handle and a view of it, both released on drop.
pub(super) struct Mapping {
    view: *mut u8,
    section: HANDLE,
}

impl Mapping {
    /// Create the section `name` (failing if it exists) with `len` bytes and
    /// map all of it.
    pub(super) fn create(name: &str, len: usize, access: Access) -> io::Result<Self> {
        let wname = wide(name)?;
        let size = len as u64;
        // SAFETY: no preconditions. Cleared so the ERROR_ALREADY_EXISTS
        // check below can't read a value left by an earlier call.
        unsafe { SetLastError(0) };
        // SAFETY: INVALID_HANDLE_VALUE asks for a section backed by the
        // paging file; null attributes give the default security descriptor
        // and a non-inheritable handle; `wname` is NUL-terminated and
        // outlives the call.
        let section = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                (size >> 32) as u32,
                size as u32,
                wname.as_ptr(),
            )
        };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        // A name that already exists still "succeeds", returning the
        // existing section at its own size. That's someone else's object.
        // SAFETY: no preconditions.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            // SAFETY: we own the handle just returned and close it once.
            unsafe { CloseHandle(section) };
            return Err(io::Error::from_raw_os_error(ERROR_ALREADY_EXISTS as i32));
        }
        map(section, len, access)
    }

    /// Map the existing section `name`, which must be exactly `len` bytes.
    pub(super) fn open(name: &str, len: usize, access: Access) -> io::Result<Self> {
        let wname = wide(name)?;
        // SAFETY: `wname` is NUL-terminated and outlives the call; 0 (FALSE)
        // makes the handle non-inheritable.
        let section = unsafe { OpenFileMappingW(view_access(access), 0, wname.as_ptr()) };
        if section.is_null() {
            return Err(io::Error::last_os_error());
        }
        // Length 0 maps the whole section, so its real size can be checked.
        let m = map(section, 0, access)?;
        let size = m.query()?.RegionSize;
        if size != len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("section is {size} bytes, the descriptor implies {len}"),
            ));
        }
        Ok(m)
    }

    /// Start of the view.
    pub(super) fn as_ptr(&self) -> *mut u8 {
        self.view
    }

    /// The view's pages as the memory manager sees them. A view is one
    /// allocation with one protection, so `RegionSize` is the view's size:
    /// the section's size in whole pages (region sizes are whole pages).
    fn query(&self) -> io::Result<MEMORY_BASIC_INFORMATION> {
        let mut info = MaybeUninit::<MEMORY_BASIC_INFORMATION>::uninit();
        // SAFETY: `info` is valid for writes of the length we pass.
        let n = unsafe {
            VirtualQuery(
                self.view.cast(),
                info.as_mut_ptr(),
                size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: VirtualQuery returned nonzero, so it filled `info`.
        Ok(unsafe { info.assume_init() })
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `view` came from MapViewOfFile and `section` from
        // Create/OpenFileMappingW; each is released exactly once, here. The
        // owning `ShmRegion` is being dropped, and every slice into the view
        // (`WriteSlot`, `Lease`) holds an `Arc` of that region, so none is
        // left.
        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.cast(),
            });
            CloseHandle(self.section);
        }
    }
}

/// Sections have no name to remove (see the module docs).
pub(super) fn unlink(_name: &str) -> io::Result<()> {
    Ok(())
}

/// Windows commits a section's memory when it is created, so an oversized
/// region fails cleanly at creation instead of faulting later.
pub(super) fn space_available(_bytes: u64) -> bool {
    true
}

fn view_access(access: Access) -> FILE_MAP {
    match access {
        Access::ReadOnly => FILE_MAP_READ,
        Access::ReadWrite => FILE_MAP_READ | FILE_MAP_WRITE,
    }
}

/// Map `len` bytes of `section` from its start (0 = all of it). Takes the
/// handle over: the `Mapping` keeps it, or it is closed on failure.
fn map(section: HANDLE, len: usize, access: Access) -> io::Result<Mapping> {
    // SAFETY: `section` is a valid section handle we own; offset 0 and
    // `len` (the section's own size, or 0) stay within it.
    let view = unsafe { MapViewOfFile(section, view_access(access), 0, 0, len) };
    if view.Value.is_null() {
        let e = io::Error::last_os_error();
        // SAFETY: we own the handle and close it once.
        unsafe { CloseHandle(section) };
        return Err(e);
    }
    Ok(Mapping {
        view: view.Value.cast(),
        section,
    })
}

fn wide(name: &str) -> io::Result<Vec<u16>> {
    if name.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared-memory name contains a NUL character",
        ));
    }
    Ok(name.encode_utf16().chain(std::iter::once(0)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::{object_name, SLOT_ALIGN};
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows_sys::Win32::System::Memory::PAGE_READONLY;

    const LEN: usize = 2 * SLOT_ALIGN as usize;

    /// A name no real region uses (epoch from a range the client never
    /// reaches).
    fn test_name() -> String {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        object_name(std::process::id(), 0xF000_0000 | n, 0x7e57_0000 | n)
    }

    #[test]
    fn creating_an_existing_name_fails_and_leaves_it_alone() {
        let name = test_name();
        let _first = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        let e = Mapping::create(&name, LEN, Access::ReadWrite)
            .err()
            .expect("second create of one name");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        assert!(Mapping::open(&name, LEN, Access::ReadOnly).is_ok());
    }

    #[test]
    fn open_checks_the_section_size() {
        let name = test_name();
        let _m = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        for wrong in [LEN - SLOT_ALIGN as usize, LEN + SLOT_ALIGN as usize] {
            let e = Mapping::open(&name, wrong, Access::ReadOnly)
                .err()
                .expect("size mismatch");
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        }
        assert!(Mapping::open(&name, LEN, Access::ReadOnly).is_ok());
    }

    #[test]
    fn read_only_views_are_read_only() {
        let name = test_name();
        let rw = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        let ro = Mapping::open(&name, LEN, Access::ReadOnly).unwrap();
        assert_eq!(rw.query().unwrap().Protect, PAGE_READWRITE);
        assert_eq!(ro.query().unwrap().Protect, PAGE_READONLY);
        // SAFETY: both views are LEN bytes of one section; the read-only
        // view is only read.
        unsafe {
            rw.as_ptr().write_bytes(0xA5, LEN);
            assert_eq!(*ro.as_ptr().add(LEN - 1), 0xA5);
        }
    }

    #[test]
    fn the_section_dies_with_its_last_handle() {
        let name = test_name();
        let m = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        unlink(&name).unwrap();
        assert!(
            Mapping::open(&name, LEN, Access::ReadOnly).is_ok(),
            "a name outlives unlink"
        );
        drop(m);
        let e = Mapping::open(&name, LEN, Access::ReadOnly)
            .err()
            .expect("open after the last handle closed");
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
    }
}
