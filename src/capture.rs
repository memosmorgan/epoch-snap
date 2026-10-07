//! One epoch's descriptor, private image, scanner and failure cleanup.
use crate::{
    Error, Result,
    arena::CaptureMetrics,
    linux::{Fault, Registered},
    mapping::Mapping,
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering, compiler_fence},
        mpsc::{self, Receiver},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct Storage {
    pub image: Vec<u64>,
    pub pages: Vec<bool>,
}

struct Control {
    failure: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    saved: AtomicUsize,
    protection_started: AtomicBool,
}

pub(crate) struct Finished {
    pub storage: Storage,
    pub outcome: Result<Option<CaptureMetrics>>,
    pub poisoned: bool,
}

pub(crate) struct Worker {
    join: Option<JoinHandle<Finished>>,
    control: Arc<Control>,
    started: Instant,
}

impl Worker {
    // Spawn before transferring prepared buffers: a spawn error cannot lose
    // their ownership. The one handoff channel is used before owner resumption;
    // fault service never needs a lock held by the application owner.
    pub(crate) fn spawn(
        mapping: Arc<Mapping>,
        started: Instant,
        #[cfg(test)] mut hooks: Option<Hooks>,
    ) -> Result<(Self, mpsc::SyncSender<Storage>, Receiver<Result<Duration>>)> {
        let control = Arc::new(Control {
            failure: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
            saved: AtomicUsize::new(0),
            protection_started: AtomicBool::new(false),
        });
        let worker_control = Arc::clone(&control);
        let (send, receive) = mpsc::sync_channel::<Storage>(0);
        let (acknowledge, armed) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("epochsnap-capture".into())
            .spawn(move || {
                let Ok(mut storage) = receive.recv() else {
                    return Finished {
                        storage: Storage {
                            image: Vec::new(),
                            pages: Vec::new(),
                        },
                        outcome: Err(Error::Protocol("capture storage handoff abandoned")),
                        poisoned: false,
                    };
                };
                // Retain storage outside the unwind boundary so failed epochs
                // recover the same allocation; Registered's Drop publishes
                // failure BEFORE its final close releases a faulting store.
                let outcome = catch_unwind(AssertUnwindSafe(|| {
                    run(
                        mapping,
                        &mut storage,
                        &worker_control,
                        started,
                        &acknowledge,
                        #[cfg(test)]
                        &mut hooks,
                    )
                }))
                .unwrap_or(Err(Error::Protocol("capture worker panicked")));
                #[cfg(test)]
                if hooks.as_ref().is_some_and(|hooks| hooks.escape_panic) {
                    panic!("injected panic outside the worker unwind boundary");
                }
                if let Err(error) = &outcome {
                    if worker_control.protection_started.load(Ordering::Acquire) {
                        worker_control.failure.store(true, Ordering::Release);
                    }
                    // If arming failed, the caller is still waiting. A success
                    // ack already consumed by the owner is never readiness.
                    let _ = acknowledge.send(Err(error.clone()));
                }
                let poisoned = worker_control.failure.load(Ordering::Acquire);
                Finished {
                    storage,
                    outcome,
                    poisoned,
                }
            })
            .map_err(|error| Error::System {
                operation: "spawn capture worker",
                errno: error.raw_os_error().unwrap_or(libc::EAGAIN),
            })?;
        Ok((
            Self {
                join: Some(join),
                control,
                started,
            },
            send,
            armed,
        ))
    }
    pub(crate) fn failed(&self) -> bool {
        self.control.failure.load(Ordering::Acquire)
    }
    pub(crate) fn saved_pages(&self) -> usize {
        self.control.saved.load(Ordering::Acquire)
    }
    pub(crate) fn is_finished(&self) -> bool {
        self.join.as_ref().unwrap().is_finished()
    }
    pub(crate) fn cancel(&self) {
        self.control.cancel.store(true, Ordering::Release);
    }
    #[cfg(test)]
    #[allow(dead_code)] // Used by the direct-module integration schedule harness.
    pub(crate) fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.control.cancel)
    }
    pub(crate) fn finish(mut self) -> Finished {
        let mut finished = join_or_abort(self.join.take().unwrap());
        if let Ok(Some(metrics)) = &mut finished.outcome {
            metrics.ready_offset = self.started.elapsed();
        }
        finished
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            self.cancel();
            // Also handles owner unwind during setup/acknowledgement. Never
            // detach a live worker, whose context may be servicing blocked writes.
            let _ = join_or_abort(join);
        }
    }
}

fn join_or_abort(join: JoinHandle<Finished>) -> Finished {
    // All expected worker panics are contained inside the storage-owning unwind
    // boundary. An escape means safe cleanup cannot be established.
    join.join().unwrap_or_else(|_| std::process::abort())
}

fn run(
    mapping: Arc<Mapping>,
    storage: &mut Storage,
    control: &Control,
    started: Instant,
    acknowledge: &mpsc::SyncSender<Result<Duration>>,
    #[cfg(test)] hooks: &mut Option<Hooks>,
) -> Result<Option<CaptureMetrics>> {
    let cpu_start = thread_cpu()?;
    let mut context = Registered::with_failure(Arc::clone(&mapping), Arc::clone(&control.failure))?;
    #[cfg(test)]
    visit(hooks, Event::BeforeArm, Some(&context))?;
    // Both a full-range ioctl error and test-induced partial arming are active
    // failures. Registered owns rollback and publishes before releasing writes.
    control.protection_started.store(true, Ordering::Release);
    #[cfg(test)]
    if hooks
        .as_ref()
        .is_some_and(|hooks| hooks.partial_arm_failure)
    {
        context.protect_first_page()?;
        return Err(Error::Protocol("injected partial arming failure"));
    }
    context.protect()?;
    // Explicit compiler barriers bracket T and page release; WP supplies the
    // Linux write exclusion, while uniform atomics supply Rust race freedom.
    compiler_fence(Ordering::SeqCst);
    let boundary = Instant::now();
    let boundary_offset = boundary.duration_since(started);
    if acknowledge.send(Ok(boundary_offset)).is_err() {
        control.cancel.store(true, Ordering::Release);
    }
    let mut metrics = CaptureMetrics {
        fault_pages: 0,
        scan_pages: 0,
        copied_bytes: 0,
        boundary_offset,
        ready_offset: Duration::ZERO,
        worker_elapsed: None,
        worker_cpu: None,
    };
    let total_pages = storage.pages.len();
    let mut scan = 0;
    'capture: while control.saved.load(Ordering::Relaxed) < total_pages {
        if control.cancel.load(Ordering::Acquire) {
            break;
        }
        #[cfg(test)]
        visit(hooks, Event::BeforeDrain, Some(&context))?;
        if control.cancel.load(Ordering::Acquire) {
            break;
        }
        // Real notifications are demand hints. Internal tests may defer an
        // already-read real notification across a scanner save.
        #[cfg(test)]
        if let Some(fault) = hooks.as_mut().and_then(|hooks| hooks.deferred.take()) {
            service_fault(
                &mapping,
                &context,
                storage,
                control,
                &mut metrics,
                fault,
                hooks,
            )?;
        }
        while let Some(fault) = context.read_fault()? {
            if control.cancel.load(Ordering::Acquire) {
                break 'capture;
            }
            service_fault(
                &mapping,
                &context,
                storage,
                control,
                &mut metrics,
                fault,
                #[cfg(test)]
                hooks,
            )?;
        }
        while scan < total_pages && storage.pages[scan] {
            scan += 1;
        }
        if scan < total_pages {
            #[cfg(test)]
            visit(hooks, Event::BeforeScan(scan), Some(&context))?;
            if control.cancel.load(Ordering::Acquire) {
                break;
            }
            save_page(
                &mapping,
                &context,
                storage,
                control,
                &mut metrics,
                scan,
                false,
                #[cfg(test)]
                hooks,
            )?;
            scan += 1;
        }
    }
    let cancelled = control.cancel.load(Ordering::Acquire);
    // Success and intentional cancellation both confirm full unprotection
    // before final close. An ioctl error leaves armed=true for failure cleanup.
    context.unprotect()?;
    #[cfg(test)]
    visit(hooks, Event::BeforeClose, Some(&context))?;
    drop(context);
    let elapsed = boundary.elapsed();
    #[cfg(test)]
    visit(hooks, Event::Closed, None)?;
    let cpu = thread_cpu()?.saturating_sub(cpu_start);
    if cancelled {
        return Ok(None);
    }
    metrics.worker_elapsed = Some(elapsed);
    metrics.worker_cpu = Some(cpu);
    #[cfg(test)]
    visit(hooks, Event::BeforePublish, None)?;
    Ok(Some(metrics))
}

fn service_fault(
    mapping: &Mapping,
    context: &Registered,
    storage: &mut Storage,
    control: &Control,
    metrics: &mut CaptureMetrics,
    fault: Fault,
    #[cfg(test)] hooks: &mut Option<Hooks>,
) -> Result<()> {
    let start = mapping.address() as u64;
    if fault.address < start || fault.address - start >= mapping.len_bytes() as u64 {
        return Err(Error::Protocol("out-of-range fault address"));
    }
    let page = (fault.address - start) as usize / mapping.page_size();
    #[cfg(test)]
    visit(hooks, Event::Fault(page), Some(context))?;
    save_page(
        mapping,
        context,
        storage,
        control,
        metrics,
        page,
        true,
        #[cfg(test)]
        hooks,
    )
}

#[allow(clippy::too_many_arguments)]
fn save_page(
    mapping: &Mapping,
    context: &Registered,
    storage: &mut Storage,
    control: &Control,
    metrics: &mut CaptureMetrics,
    page: usize,
    fault: bool,
    #[cfg(test)] hooks: &mut Option<Hooks>,
) -> Result<()> {
    if storage.pages[page] {
        // A delayed fault must not cause an already-saved page to be copied again.
        return Ok(());
    }
    let words = mapping.page_size() / 8;
    mapping.copy_page(page, &mut storage.image[page * words..(page + 1) * words])?;
    storage.pages[page] = true;
    #[cfg(test)]
    visit(hooks, Event::Copied(page), Some(context))?;
    compiler_fence(Ordering::SeqCst);
    // Do not release the writer until the entire page has been saved in the image.
    context.unprotect_page(page)?;
    if fault {
        metrics.fault_pages += 1;
    } else {
        metrics.scan_pages += 1;
    }
    metrics.copied_bytes += mapping.page_size();
    control.saved.fetch_add(1, Ordering::Release);
    #[cfg(test)]
    visit(hooks, Event::Unprotected(page), Some(context))?;
    Ok(())
}

fn thread_cpu() -> Result<Duration> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: initialized writable timespec, correct clock, no arena memory.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return Err(Error::System {
            operation: "clock_gettime(CLOCK_THREAD_CPUTIME_ID)",
            errno: std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO),
        });
    }
    Ok(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    BeforeArm,
    BeforeDrain,
    BeforeScan(usize),
    Fault(usize),
    Copied(usize),
    Unprotected(usize),
    BeforeClose,
    Closed,
    BeforePublish,
}
#[cfg(test)]
type Callback = Box<dyn FnMut(Event, Option<&Registered>) -> Result<Option<Fault>> + Send>;
#[cfg(test)]
pub(crate) struct Hooks {
    callback: Callback,
    deferred: Option<Fault>,
    pub partial_arm_failure: bool,
    pub escape_panic: bool,
}
#[cfg(test)]
impl Hooks {
    #[allow(dead_code)] // Constructed by the direct-module integration harness.
    pub(crate) fn new(
        callback: impl FnMut(Event, Option<&Registered>) -> Result<Option<Fault>> + Send + 'static,
    ) -> Self {
        Self {
            callback: Box::new(callback),
            deferred: None,
            partial_arm_failure: false,
            escape_panic: false,
        }
    }
}
#[cfg(test)]
fn visit(hooks: &mut Option<Hooks>, event: Event, context: Option<&Registered>) -> Result<()> {
    if let Some(hooks) = hooks
        && let Some(fault) = (hooks.callback)(event, context)?
    {
        assert!(hooks.deferred.is_none());
        hooks.deferred = Some(fault);
    }
    Ok(())
}
