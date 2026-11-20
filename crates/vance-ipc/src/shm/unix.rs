//! POSIX shared memory (macOS, Linux): `shm_open` + `mmap`.
//!
//! The descriptor is closed as soon as the object is mapped: the mapping
//! keeps the object alive on its own, and an open descriptor would only be
//! one more thing a child could inherit.

use super::Access;
use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

/// Owner read/write only: the decoder sidecar runs as the same user.
const MODE: libc::c_uint = 0o600;

/// A mapped shared-memory object, unmapped on drop.
pub(super) struct Mapping {
    ptr: *mut u8,
    len: usize,
}

impl Mapping {
    /// Create the object `name` (failing if it exists), size it to `len`
    /// bytes and map it.
    pub(super) fn create(name: &str, len: usize, access: Access) -> io::Result<Self> {
        let cname = c_name(name)?;
        let size = libc::off_t::try_from(len).map_err(|_| too_large(len))?;
        // SAFETY: `cname` is NUL-terminated and outlives the call; the
        // variadic mode argument is a `c_uint`, its promoted type.
        let fd = cvt(|| unsafe {
            libc::shm_open(
                cname.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                MODE,
            )
        })?;
        // SAFETY: shm_open just returned this descriptor; nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        // SAFETY: plain syscall on a descriptor we own.
        let mapped = cvt(|| unsafe { libc::ftruncate(fd.as_raw_fd(), size) })
            .and_then(|_| map(&fd, len, access));
        if mapped.is_err() {
            // Intentional: the error is the one worth reporting. We created
            // the name a moment ago and nobody can have attached yet, so
            // remove it rather than leave a stale object behind.
            let _ = unlink_c(&cname);
        }
        mapped
    }

    /// Map the existing object `name`, which must be exactly `len` bytes.
    pub(super) fn open(name: &str, len: usize, access: Access) -> io::Result<Self> {
        let cname = c_name(name)?;
        let oflag = match access {
            Access::ReadOnly => libc::O_RDONLY,
            Access::ReadWrite => libc::O_RDWR,
        };
        // SAFETY: as in `create`; the mode is ignored without O_CREAT.
        let fd = cvt(|| unsafe { libc::shm_open(cname.as_ptr(), oflag, MODE) })?;
        // SAFETY: shm_open just returned this descriptor; nothing else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut st = MaybeUninit::<libc::stat>::uninit();
        // SAFETY: `st` is valid for writes of one `stat`, and fstat fills it
        // completely when it succeeds.
        cvt(|| unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) })?;
        // SAFETY: fstat succeeded, so `st` is initialized.
        let size = unsafe { st.assume_init() }.st_size;
        // A smaller object would raise SIGBUS when the tail is touched; a
        // larger one means the descriptor and the object disagree.
        if u64::try_from(size).ok() != Some(len as u64) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("object is {size} bytes, the descriptor implies {len}"),
            ));
        }
        map(&fd, len, access)
    }

    /// Start of the mapping.
    pub(super) fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    /// Mapping length for the OS (tests only).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.len
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` are exactly what mmap returned. The owning
        // `ShmRegion` is being dropped, and every slice into the mapping
        // (`WriteSlot`, `Lease`) holds an `Arc` of that region, so none is
        // left.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// Remove the name `name`. Already gone is success.
pub(super) fn unlink(name: &str) -> io::Result<()> {
    unlink_c(&c_name(name)?)
}

/// Whether `/dev/shm` has room for `bytes` more. Linux shm objects are files
/// in that tmpfs: sizing one past its free space succeeds, and touching the
/// missing pages later raises SIGBUS. Unknown (no `/dev/shm`) counts as no.
#[cfg(target_os = "linux")]
pub(super) fn space_available(bytes: u64) -> bool {
    let mut st = MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: the path is a NUL-terminated literal and `st` is valid for
    // writes of one `statvfs`, which the call fills when it succeeds.
    if cvt(|| unsafe { libc::statvfs(c"/dev/shm".as_ptr(), st.as_mut_ptr()) }).is_err() {
        return false;
    }
    // SAFETY: statvfs succeeded, so `st` is initialized.
    let st = unsafe { st.assume_init() };
    // u128: the field widths vary by target, and the product can't overflow.
    let free = u128::from(st.f_bavail) * u128::from(st.f_frsize);
    u128::from(bytes) <= free
}

/// macOS backs POSIX shm with ordinary virtual memory: no filesystem limit.
#[cfg(not(target_os = "linux"))]
pub(super) fn space_available(_bytes: u64) -> bool {
    true
}

fn unlink_c(name: &CString) -> io::Result<()> {
    // SAFETY: `name` is NUL-terminated and outlives the call.
    match cvt(|| unsafe { libc::shm_unlink(name.as_ptr()) }) {
        Ok(_) => Ok(()),
        // Intentional: unlinking is idempotent by contract.
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Ok(()),
        Err(e) => Err(e),
    }
}

fn map(fd: &OwnedFd, len: usize, access: Access) -> io::Result<Mapping> {
    let prot = match access {
        Access::ReadOnly => libc::PROT_READ,
        Access::ReadWrite => libc::PROT_READ | libc::PROT_WRITE,
    };
    // SAFETY: a new mapping at an address the kernel picks (no MAP_FIXED),
    // so no existing memory is affected; `fd` is open and the object is at
    // least `len` bytes (created at that size, or checked by `open`).
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            len,
            prot,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(Mapping { ptr: p.cast(), len })
}

fn c_name(name: &str) -> io::Result<CString> {
    CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "shared-memory name contains a NUL byte",
        )
    })
}

fn too_large(len: usize) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("{len} bytes is too large for a shared-memory object"),
    )
}

/// Run a libc call that reports failure as -1 + errno, retrying on EINTR.
fn cvt(mut f: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        let r = f();
        if r != -1 {
            return Ok(r);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::{object_name, SLOT_ALIGN};
    use std::sync::atomic::{AtomicU32, Ordering};

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
        let first = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        let e = Mapping::create(&name, LEN, Access::ReadWrite)
            .err()
            .expect("second create of one name");
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists, "{e}");
        // The failed create must not have unlinked the first one's name.
        let again = Mapping::open(&name, LEN, Access::ReadOnly).unwrap();
        unlink(&name).unwrap();
        assert_eq!((first.len(), again.len()), (LEN, LEN));
    }

    #[test]
    fn unlink_is_idempotent_and_open_after_it_fails() {
        let name = test_name();
        let _m = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        unlink(&name).unwrap();
        unlink(&name).unwrap();
        let e = Mapping::open(&name, LEN, Access::ReadOnly)
            .err()
            .expect("open after unlink");
        assert_eq!(e.kind(), io::ErrorKind::NotFound, "{e}");
        unlink(&test_name()).unwrap();
    }

    #[test]
    fn open_checks_the_object_size() {
        let name = test_name();
        let _m = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        for wrong in [LEN - SLOT_ALIGN as usize, LEN + SLOT_ALIGN as usize] {
            let e = Mapping::open(&name, wrong, Access::ReadOnly)
                .err()
                .expect("size mismatch");
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{e}");
        }
        assert!(Mapping::open(&name, LEN, Access::ReadOnly).is_ok());
        unlink(&name).unwrap();
    }

    #[test]
    fn names_with_nul_are_rejected() {
        let e = Mapping::open("/vance\0x", LEN, Access::ReadOnly)
            .err()
            .unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }

    /// The read-only mapping is really PROT_READ. Probed without faulting:
    /// read(2) into a buffer the kernel may not write fails with EFAULT.
    #[test]
    fn read_only_mappings_are_not_writable() {
        let name = test_name();
        let rw = Mapping::create(&name, LEN, Access::ReadWrite).unwrap();
        let ro = Mapping::open(&name, LEN, Access::ReadOnly).unwrap();
        unlink(&name).unwrap();

        let mut fds = [0; 2];
        // SAFETY: `fds` has room for the two descriptors pipe() returns.
        cvt(|| unsafe { libc::pipe(fds.as_mut_ptr()) }).unwrap();
        // SAFETY: pipe() just returned these and nothing else owns them.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let feed = |bytes: &[u8]| {
            // SAFETY: `bytes` is valid for reads of its length.
            let n = unsafe { libc::write(w.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            assert_eq!(n, bytes.len() as isize);
        };
        // SAFETY: the kernel checks the destination; a non-writable one
        // yields EFAULT rather than a fault in this process.
        let read_into = |dst: *mut u8| unsafe { libc::read(r.as_raw_fd(), dst.cast(), 4) };
        // SAFETY: the mapping is LEN bytes, and nothing writes it while the
        // copy is taken.
        let head = |m: &Mapping| unsafe { std::slice::from_raw_parts(m.as_ptr(), 4) }.to_vec();

        feed(b"abcd");
        assert_eq!(read_into(rw.as_ptr()), 4, "control: rw mapping is writable");
        assert_eq!(head(&ro), b"abcd", "the read-only mapping sees the write");

        feed(b"efgh");
        assert_eq!(read_into(ro.as_ptr()), -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::EFAULT),
            "read-only mapping accepted a write"
        );
        assert_eq!(head(&ro), b"abcd");
    }

    #[test]
    fn space_check_is_sane() {
        assert!(space_available(0));
        if cfg!(target_os = "linux") {
            assert!(!space_available(u64::MAX));
        }
    }
}
