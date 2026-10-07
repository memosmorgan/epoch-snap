//! EpochSnap: Linux capability diagnostics and an owner-facing atomic-slot arena.

#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
compile_error!("EpochSnap supports Linux/x86-64 only");

mod arena;
pub use arena::{Arena, CaptureMetrics, CaptureMode, CheckpointStatus, Epoch};
mod capture;

// The integration harness compiles private modules directly; one-page test
// helpers and ABI layout types are intentionally absent from the owner API.
#[allow(dead_code)]
mod linux;
#[allow(dead_code)]
mod mapping;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    System {
        operation: &'static str,
        errno: i32,
    },
    Unsupported {
        missing: &'static str,
        observed: u64,
    },
    InvalidSize,
    Bounds,
    Allocation(&'static str),
    Busy,
    InvalidEpoch,
    EpochExhausted,
    Pending,
    Protocol(&'static str),
    Timeout,
    Poisoned,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::System { operation, errno } => write!(
                f,
                "{operation}: {} (errno={errno})",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Self::Unsupported { missing, observed } => {
                write!(f, "missing {missing} (observed mask={observed:#x})")
            }
            Self::InvalidSize => f.write_str("invalid or overflowing mapping size"),
            Self::Bounds => f.write_str("slot/page/image outside mapping bounds"),
            Self::Allocation(storage) => write!(f, "could not allocate {storage}"),
            Self::Busy => f.write_str("discard the retained checkpoint before another capture"),
            Self::InvalidEpoch => {
                f.write_str("checkpoint epoch is stale or belongs to another arena")
            }
            Self::EpochExhausted => f.write_str("arena identity or epoch generation exhausted"),
            Self::Pending => {
                f.write_str("checkpoint is pending; wait before inspecting or restoring")
            }
            Self::Protocol(message) => write!(f, "userfaultfd protocol: {message}"),
            Self::Timeout => f.write_str("timed out waiting for a userfaultfd event"),
            Self::Poisoned => f.write_str("fault service failed before releasing the writer"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy)]
pub struct Capabilities {
    pub page_size: usize,
    pub features: u64,
    pub ioctls: u64,
    pub range_ioctls: u64,
}

/// Negotiate synchronous WP and check registration of a populated anonymous page.
/// This does not exercise a protected write; run the explicit kernel tests too.
pub fn probe_userfaultfd() -> Result<Capabilities> {
    let mapping = std::sync::Arc::new(mapping::Mapping::new(1)?);
    let registered = linux::Registered::new(mapping)?;
    Ok(registered.capabilities)
}
