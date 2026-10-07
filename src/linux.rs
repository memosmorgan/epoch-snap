use crate::mapping::Mapping;
use crate::{Capabilities, Error, Result};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Call {
    Read,
    Protect,
    Unprotect,
    Close,
}

#[cfg(test)]
#[derive(Default)]
struct Injection {
    errors: Vec<(Call, i32)>,
    message: Option<([u8; 32], i64)>,
}

// Required subset of Linux v6.6 include/uapi/linux/userfaultfd.h.
const API_REQUEST: libc::c_ulong = 0xc018_aa3f;
const REGISTER_REQUEST: libc::c_ulong = 0xc020_aa00;
const WP_REQUEST: libc::c_ulong = 0xc018_aa06;
const REQUIRED_FEATURES: u64 = (1 << 0) | (1 << 13);

#[repr(C)]
#[derive(Default)]
struct Api {
    api: u64,
    features: u64,
    ioctls: u64,
}
#[repr(C)]
#[derive(Default)]
struct Range {
    start: u64,
    len: u64,
}
#[repr(C)]
#[derive(Default)]
struct Register {
    range: Range,
    mode: u64,
    ioctls: u64,
}
#[repr(C)]
#[derive(Default)]
struct WriteProtect {
    range: Range,
    mode: u64,
}
#[repr(C, packed)]
struct Message {
    event: u8,
    reserved: [u8; 7],
    arg: [u8; 24],
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fault {
    pub address: u64,
}

fn decode(bytes: &[u8; 32]) -> Result<Fault> {
    let flags = u64::from_ne_bytes(bytes[8..16].try_into().unwrap());
    if bytes[0] != 0x12 || flags != 3 {
        return Err(Error::Protocol("expected a synchronous WP write fault"));
    }
    Ok(Fault {
        address: u64::from_ne_bytes(bytes[16..24].try_into().unwrap()),
    })
}

fn validate_api(api: &Api) -> Result<()> {
    for (bit, name) in [(1, "anonymous WP"), (1 << 13, "WP_UNPOPULATED")] {
        if api.features & bit == 0 {
            return Err(Error::Unsupported {
                missing: name,
                observed: api.features,
            });
        }
    }
    if api.api != 0xaa {
        return Err(Error::Protocol("unexpected API version"));
    }
    if api.ioctls & 3 != 3 {
        return Err(Error::Unsupported {
            missing: "REGISTER/UNREGISTER ioctls",
            observed: api.ioctls,
        });
    }
    Ok(())
}

fn system_result(operation: &'static str, result: i64) -> Result<()> {
    if result < 0 {
        Err(Error::System {
            operation,
            errno: std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        })
    } else {
        Ok(())
    }
}

fn retry(operation: &'static str, eagain: bool, mut call: impl FnMut() -> i64) -> Result<i64> {
    loop {
        let result = call();
        match system_result(operation, result) {
            Ok(()) => return Ok(result),
            Err(Error::System { errno, .. })
                if errno == libc::EINTR || (eagain && errno == libc::EAGAIN) => {}
            Err(error) => return Err(error),
        }
    }
}

fn open() -> Result<OwnedFd> {
    let raw = retry("userfaultfd(UFFD_USER_MODE_ONLY)", false, || {
        // SAFETY: the x86-64 syscall has exactly one integer flags argument.
        unsafe {
            libc::syscall(
                libc::SYS_userfaultfd,
                libc::O_CLOEXEC | libc::O_NONBLOCK | 1,
            )
        }
    })? as libc::c_int;
    // SAFETY: successful syscall returns a fresh fd. This is its sole owner;
    // no duplicate fd is made, so dropping it closes the final UFFD reference.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn negotiate(fd: &OwnedFd, features: u64) -> Result<Api> {
    let mut api = Api {
        api: 0xaa,
        features,
        ioctls: 0,
    };
    retry("UFFDIO_API", false, || {
        // SAFETY: request exactly matches this initialized C-layout buffer;
        // fd is live for the call, and kernel output fits the 24-byte object.
        unsafe { libc::ioctl(fd.as_raw_fd(), API_REQUEST, &mut api as *mut Api) as i64 }
    })?;
    validate_api(&api)?;
    Ok(api)
}

pub(crate) struct Registered {
    fd: Option<OwnedFd>,
    mapping: Arc<Mapping>,
    failure: Arc<AtomicBool>,
    armed: bool,
    #[cfg(test)]
    injection: std::cell::RefCell<Injection>,
    pub(crate) capabilities: Capabilities,
}
impl Registered {
    pub(crate) fn new(mapping: Arc<Mapping>) -> Result<Self> {
        Self::with_failure(mapping, Arc::new(AtomicBool::new(false)))
    }
    pub(crate) fn with_failure(mapping: Arc<Mapping>, failure: Arc<AtomicBool>) -> Result<Self> {
        let probe = open()?;
        negotiate(&probe, 0)?;
        drop(probe);
        let fd = open()?;
        let api = negotiate(&fd, REQUIRED_FEATURES)?;
        let mut register = Register {
            range: Range {
                start: mapping.address() as u64,
                len: mapping.len_bytes() as u64,
            },
            mode: 2,
            ioctls: 0,
        };
        retry("UFFDIO_REGISTER(WP)", false, || {
            // SAFETY: request/32-byte C buffer match. The mapping is aligned,
            // owned, populated and kept alive by Arc through registration/release.
            unsafe {
                libc::ioctl(
                    fd.as_raw_fd(),
                    REGISTER_REQUEST,
                    &mut register as *mut Register,
                ) as i64
            }
        })?;
        if register.ioctls & (1 << 6) == 0 {
            return Err(Error::Unsupported {
                missing: "range WRITEPROTECT ioctl",
                observed: register.ioctls,
            });
        }
        let capabilities = Capabilities {
            page_size: crate::mapping::page_size()?,
            features: api.features,
            ioctls: api.ioctls,
            range_ioctls: register.ioctls,
        };
        Ok(Self {
            fd: Some(fd),
            mapping,
            failure,
            armed: false,
            #[cfg(test)]
            injection: Default::default(),
            capabilities,
        })
    }
    pub(crate) fn protect(&mut self) -> Result<()> {
        // An ioctl failure may have partially armed a range. Mark ownership first
        // so error/unwind cleanup publishes failure before descriptor release.
        self.armed = true;
        self.change_protection(1)
    }
    pub(crate) fn unprotect(&mut self) -> Result<()> {
        self.change_protection(0)?;
        self.armed = false;
        Ok(())
    }
    fn change_protection(&self, mode: u64) -> Result<()> {
        self.change_range(0, self.mapping.len_bytes(), mode)
    }
    fn change_range(&self, offset: usize, bytes: usize, mode: u64) -> Result<()> {
        let mut wp = WriteProtect {
            range: Range {
                start: (self.mapping.address() + offset) as u64,
                len: bytes as u64,
            },
            mode,
        };
        retry("UFFDIO_WRITEPROTECT", true, || {
            #[cfg(test)]
            if self.injected_error(if mode == 1 {
                Call::Protect
            } else {
                Call::Unprotect
            }) {
                return -1;
            }
            // SAFETY: correct request and initialized 24-byte buffer; sole fd
            // remains owned and the registered mapping remains alive.
            unsafe {
                libc::ioctl(
                    self.fd.as_ref().unwrap().as_raw_fd(),
                    WP_REQUEST,
                    &mut wp as *mut WriteProtect,
                ) as i64
            }
        })?;
        Ok(())
    }
    pub(crate) fn unprotect_page(&self, page: usize) -> Result<()> {
        if page >= self.mapping.len_bytes() / self.mapping.page_size() {
            return Err(Error::Bounds);
        }
        self.change_range(page * self.mapping.page_size(), self.mapping.page_size(), 0)
    }
    #[cfg(test)]
    pub(crate) fn protect_first_page(&mut self) -> Result<()> {
        self.armed = true;
        self.change_range(0, self.mapping.page_size(), 1)
    }
    pub(crate) fn read_fault(&self) -> Result<Option<Fault>> {
        let mut bytes = [0u8; 32];
        let count = match retry("read(userfaultfd)", false, || {
            #[cfg(test)]
            if self.injected_error(Call::Read) {
                return -1;
            }
            #[cfg(test)]
            if let Some((message, count)) = self.injection.borrow_mut().message.take() {
                bytes = message;
                return count;
            }
            // SAFETY: writable initialized stack buffer of exactly one packed
            // message. No references to packed fields are ever constructed.
            unsafe {
                libc::read(
                    self.fd.as_ref().unwrap().as_raw_fd(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                ) as i64
            }
        }) {
            Err(Error::System {
                errno: libc::EAGAIN,
                ..
            }) => return Ok(None),
            result => result?,
        };
        if count != 32 {
            return Err(Error::Protocol("short/EOF fault message"));
        }
        let fault = decode(&bytes)?;
        let start = self.mapping.address() as u64;
        if fault.address < start || fault.address - start >= self.mapping.len_bytes() as u64 {
            return Err(Error::Protocol("out-of-range fault address"));
        }
        Ok(Some(fault))
    }
    pub(crate) fn wait_fault(&self, timeout: Duration) -> Result<Fault> {
        let deadline = Instant::now().checked_add(timeout).ok_or(Error::Timeout)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            if let Some(fault) = self.read_fault()? {
                return Ok(fault);
            }
            let millis = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            let mut poll = libc::pollfd {
                fd: self.fd.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd, live fd and bounded integer timeout.
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if let Err(error) = system_result("poll(userfaultfd)", result as i64) {
                if matches!(
                    error,
                    Error::System {
                        errno: libc::EINTR,
                        ..
                    }
                ) {
                    continue;
                }
                return Err(error);
            }
            if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(Error::Protocol("poll reports failed descriptor"));
            }
        }
    }
    #[cfg(test)]
    pub(crate) fn wait_readable(&self, timeout: Duration) -> Result<()> {
        let deadline = Instant::now().checked_add(timeout).ok_or(Error::Timeout)?;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(Error::Timeout);
            }
            let mut poll = libc::pollfd {
                fd: self.fd.as_ref().unwrap().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = remaining
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32;
            // SAFETY: one initialized pollfd, a live owned fd, bounded timeout.
            let result = unsafe { libc::poll(&mut poll, 1, millis) };
            if let Err(error) = system_result("poll(userfaultfd)", result as i64) {
                if matches!(
                    error,
                    Error::System {
                        errno: libc::EINTR,
                        ..
                    }
                ) {
                    continue;
                }
                return Err(error);
            }
            if poll.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(Error::Protocol("poll reports failed descriptor"));
            }
            if poll.revents & libc::POLLIN != 0 {
                return Ok(());
            }
        }
    }
    pub(crate) fn failure(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.failure)
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the direct-module integration harness.
    pub(crate) fn inject_errors(&self, call: Call, errors: &[i32]) {
        self.injection
            .borrow_mut()
            .errors
            .extend(errors.iter().map(|&errno| (call, errno)));
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the direct-module integration harness.
    pub(crate) fn pending_errors(&self) -> usize {
        self.injection.borrow().errors.len()
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the direct-module integration harness.
    pub(crate) fn inject_message(&self, message: [u8; 32], count: i64) {
        self.injection.borrow_mut().message = Some((message, count));
    }
    #[cfg(test)]
    fn injected_error(&self, call: Call) -> bool {
        let mut injection = self.injection.borrow_mut();
        let Some(index) = injection
            .errors
            .iter()
            .position(|&(operation, _)| operation == call)
        else {
            return false;
        };
        let (_, errno) = injection.errors.remove(index);
        // SAFETY: test-only injection writes this worker thread's errno; it
        // bypasses one syscall attempt, then exercises the real retry/cleanup.
        unsafe {
            *libc::__errno_location() = errno;
        }
        true
    }
}
impl Drop for Registered {
    fn drop(&mut self) {
        // MUST close only after terminal failure publication. On Linux final
        // close clears UFFD WP and wakes waiters, including during unwinding.
        // Mapping Arc is still held here and its field drops after fd release.
        #[cfg(test)]
        let injected = self.injected_error(Call::Close);
        #[cfg(not(test))]
        let injected = false;
        release_armed(self.armed, &self.failure, || {
            if let Some(fd) = self.fd.take() {
                let raw = fd.into_raw_fd();
                let result = if injected {
                    -1
                } else {
                    // SAFETY: sole descriptor, consumed exactly once. Never
                    // retry: Linux may already have released the fd on error.
                    unsafe { libc::close(raw) }
                };
                if result != 0 {
                    std::process::abort();
                }
            }
        });
    }
}

fn release_armed(armed: bool, failure: &AtomicBool, close: impl FnOnce()) {
    if armed {
        failure.store(true, Ordering::Release);
    }
    close();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    #[test]
    fn linux_66_uapi_layout_and_requests() {
        assert_eq!(size_of::<Api>(), 24);
        assert_eq!(offset_of!(Api, features), 8);
        assert_eq!(offset_of!(Api, ioctls), 16);
        assert_eq!(size_of::<Range>(), 16);
        assert_eq!(offset_of!(Range, len), 8);
        assert_eq!(size_of::<Register>(), 32);
        assert_eq!(offset_of!(Register, mode), 16);
        assert_eq!(offset_of!(Register, ioctls), 24);
        assert_eq!(size_of::<WriteProtect>(), 24);
        assert_eq!(offset_of!(WriteProtect, mode), 16);
        assert_eq!(size_of::<Message>(), 32);
        assert_eq!(align_of::<Message>(), 1);
        assert_eq!(offset_of!(Message, arg), 8);
        assert_eq!(API_REQUEST, 0xc018_aa3f);
        assert_eq!(REGISTER_REQUEST, 0xc020_aa00);
        assert_eq!(WP_REQUEST, 0xc018_aa06);
    }

    #[test]
    fn packed_fault_decodes_and_rejects_unexpected_flags() {
        let mut bytes = [0; 32];
        bytes[0] = 0x12;
        bytes[8] = 3;
        bytes[16..24].copy_from_slice(&0x1234_5678_9000u64.to_ne_bytes());
        assert_eq!(decode(&bytes).unwrap().address, 0x1234_5678_9000);
        bytes[8] = 1;
        assert!(matches!(decode(&bytes), Err(Error::Protocol(_))));
        bytes[0] = 0x16;
        assert!(matches!(decode(&bytes), Err(Error::Protocol(_))));
    }

    #[test]
    fn missing_features_and_ioctl_support_are_errors() {
        let mut api = Api {
            api: 0xaa,
            features: REQUIRED_FEATURES,
            ioctls: 3,
        };
        assert!(validate_api(&api).is_ok());
        api.features &= !(1 << 13);
        assert!(matches!(
            validate_api(&api),
            Err(Error::Unsupported {
                missing: "WP_UNPOPULATED",
                ..
            })
        ));
        api.features = REQUIRED_FEATURES;
        api.ioctls = 0;
        assert!(matches!(validate_api(&api), Err(Error::Unsupported { .. })));
    }

    #[test]
    fn syscall_error_preserves_operation_and_errno() {
        // SAFETY: writes this thread's libc errno solely to inject a syscall outcome.
        unsafe {
            *libc::__errno_location() = libc::EPERM;
        }
        assert_eq!(
            system_result("userfaultfd", -1),
            Err(Error::System {
                operation: "userfaultfd",
                errno: libc::EPERM
            })
        );
        assert!(system_result("ioctl", 0).is_ok());
    }

    #[test]
    fn retries_interrupted_calls_and_only_requested_eagain() {
        let mut calls = 0;
        let value = retry("writeprotect", true, || {
            calls += 1;
            if calls < 3 {
                // SAFETY: injects this thread's syscall errno outcome.
                unsafe {
                    *libc::__errno_location() = if calls == 1 {
                        libc::EINTR
                    } else {
                        libc::EAGAIN
                    };
                }
                -1
            } else {
                0
            }
        });
        assert_eq!(value, Ok(0));
        assert_eq!(calls, 3);
        assert_eq!(
            retry("read", false, || {
                // SAFETY: injects this thread's syscall errno outcome.
                unsafe {
                    *libc::__errno_location() = libc::EAGAIN;
                }
                -1
            }),
            Err(Error::System {
                operation: "read",
                errno: libc::EAGAIN
            })
        );
    }

    #[test]
    fn terminal_failure_is_visible_before_release() {
        let failure = AtomicBool::new(false);
        release_armed(true, &failure, || {
            assert!(
                failure.load(Ordering::Acquire),
                "release must observe terminal failure already published"
            );
        });
        failure.store(false, Ordering::Relaxed);
        release_armed(false, &failure, || {
            assert!(!failure.load(Ordering::Acquire))
        });
    }
}
