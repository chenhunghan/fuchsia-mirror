// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel lock dependency tracking runtime, diagnostics, and console commands.

mod lock_dep_ffi;

use crate::backtrace::Backtrace;
use crate::kernel::event::Event;
use crate::kernel::percpu::PerCpu;
use crate::kernel::relaxed_atomic::RelaxedAtomicBool;
use crate::kernel::thread::{self, Interruptible, LOW_PRIORITY, Thread};
use crate::ksync_kernel::TrackedMutex;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::ffi::{CStr, c_void};
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::Ordering;
use debug::kernel_oops;
use init::{LK_INIT_LEVEL_EARLIEST, LK_INIT_LEVEL_THREADING, LkInitLevel};
use kprint::{kprint, kprintln};
use ksync::KMutex;
use lazy_init::{LazyInit, NoCheck};
use lock_dep_ffi::cpp_lockdep_thread_lock_state_init;
use lockdep::{LOCK_FLAGS_IRQ_SAFE, LockFlags};
use pin_init::pin_init;
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::ZX_MAX_NAME_LEN;

const MAX_LOCK_DEPENDENCIES: usize = 193;

/// Atomic flag used to indicate that a loop detection pass needs to be performed.
///
/// Relaxed ordering suffices because `LockDependencySet` updates use relaxed atomics and the
/// polling loop-detector thread does not depend on any non-atomic stores published via this flag.
static LOOP_DETECTION_GRAPH_IS_DIRTY: RelaxedAtomicBool = RelaxedAtomicBool::new(false);

/// Event to wait on the completion of a triggered loop detection pass. This is
/// primarily to bound the async loop detection report when testing.
static DETECTION_COMPLETE_EVENT: LazyInit<Event, NoCheck> = LazyInit::uninit();

/// Synchronizes the access to the loop detection completion event.
#[ksync::guarded]
struct DetectionCompleteLock {
    #[mutex]
    lock: KMutex<TrackedMutex>,
}

static DETECTION_COMPLETE_LOCK: LazyInit<DetectionCompleteLock, NoCheck> = LazyInit::uninit();

/// Opaque representation of C++ `lockdep::ValidatorLockClassState`.
#[repr(C, align(8))]
pub struct ValidatorLockClassState {
    _facade: OpaqueFacade,
}

impl ValidatorLockClassState {
    /// Returns the `ValidatorLockClassState` instance for the given lock class ID
    /// (`ValidatorLockClassState::Get(id)`).
    #[inline]
    pub fn get(id: &Self) -> &Self {
        id
    }

    /// Returns the name of this lock class (`ValidatorLockClassState::name()`).
    pub fn name(&self) -> &CStr {
        // SAFETY: `self` is a valid `ValidatorLockClassState` whose name is a static C string.
        unsafe { CStr::from_ptr(lock_dep_ffi::cpp_lockdep_state_name(self)) }
    }

    /// Returns the type name of the lock class for the given lock class ID
    /// (`ValidatorLockClassState::GetName(id)`).
    pub fn get_name(id: &Self) -> &CStr {
        Self::get(id).name()
    }

    /// Returns the root of the connected set containing `self`
    /// (`ValidatorLockClassState::connected_set()`).
    pub fn connected_set(&self) -> *const Self {
        // SAFETY: `self` is a valid `ValidatorLockClassState`; `OpaqueFacade` wraps `UnsafeCell`,
        // permitting interior mutation of the relaxed-atomic disjoint-set parent in `FindSet`.
        unsafe {
            lock_dep_ffi::cpp_lockdep_state_connected_set(ptr::from_ref(self).cast_mut())
                .cast_const()
        }
    }

    /// Returns an iterator over all registered `ValidatorLockClassState` instances
    /// (`ValidatorLockClassState::Iter()`).
    pub fn iter() -> impl Iterator<Item = &'static Self> {
        // SAFETY: `cpp_lockdep_iter_begin` returns the head of the static linked list of lock
        // classes (or null).
        let mut curr = unsafe { lock_dep_ffi::cpp_lockdep_iter_begin() };
        core::iter::from_fn(move || {
            let state = curr;
            if state.is_null() {
                None
            } else {
                // SAFETY: `state` is a valid node in the static linked list of lock classes.
                unsafe {
                    curr = lock_dep_ffi::cpp_lockdep_iter_next(state);
                    Some(&*state)
                }
            }
        })
    }

    /// Returns an iterator over the dependency set of `self`
    /// (`ValidatorLockClassState::dependency_set()`).
    pub fn dependency_set(&self) -> impl Iterator<Item = &'static Self> + '_ {
        // SAFETY: `self` is a valid `ValidatorLockClassState`.
        let set = unsafe { lock_dep_ffi::cpp_lockdep_state_dependency_set(self) };
        set.iter().filter_map(|entry| {
            let ptr = entry.load(Ordering::Relaxed);
            // SAFETY: Any non-null entry in `LockDependencySet` points to a valid `'static`
            // `ValidatorLockClassState`.
            unsafe { ptr.as_ref() }
        })
    }
}

/// Opaque representation of C++ `lockdep::AcquiredLockEntry`.
#[repr(C, align(8))]
pub struct AcquiredLockEntry {
    _facade: OpaqueFacade,
}

impl AcquiredLockEntry {
    /// Returns the lock class ID for this acquired lock entry (`AcquiredLockEntry::id()`).
    ///
    /// # Safety
    ///
    /// `self` must have a non-null lock class ID pointing to a valid [`ValidatorLockClassState`].
    pub unsafe fn id(&self) -> &ValidatorLockClassState {
        // SAFETY: `self` is a valid `AcquiredLockEntry`, and the caller guarantees its lock class
        // ID points to a valid `ValidatorLockClassState`.
        unsafe { lock_dep_ffi::cpp_lockdep_entry_id(self) }
    }

    /// Returns the external/address order recorded for this acquired lock entry
    /// (`AcquiredLockEntry::order()`).
    pub fn order(&self) -> usize {
        // SAFETY: `self` is a valid `AcquiredLockEntry`.
        unsafe { lock_dep_ffi::cpp_lockdep_entry_order(self) }
    }
}

/// Representation of C++ `lockdep::LockResult` (`enum class LockResult : uint8_t`).
#[repr(transparent)]
#[derive(Clone, Copy)]
pub struct LockResult(u8);

impl LockResult {
    /// Returns a string representation of the given `LockResult` (`lockdep::ToString(result)`).
    pub fn to_cstr(self) -> &'static CStr {
        // SAFETY: `cpp_lockdep_result_to_string` returns a static NUL-terminated string literal
        // for every `u8` value.
        unsafe { CStr::from_ptr(lock_dep_ffi::cpp_lockdep_result_to_string(self)) }
    }
}

/// An opaque type representing the C++ `lockdep::ThreadLockState` class.
#[repr(transparent)]
pub struct ThreadLockState(zr::Opaque<[usize; 2]>);

impl ThreadLockState {
    /// Constructs a default `ThreadLockState` in place.
    #[inline]
    pub fn init() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        // SAFETY: `cpp_lockdep_thread_lock_state_init` placement-constructs a valid
        // `lockdep::ThreadLockState` in place in the provided uninitialized slot.
        zr::pin_init_ffi!(cpp_lockdep_thread_lock_state_init)
    }

    /// Returns a raw mutable pointer to the underlying `ThreadLockState` storage.
    #[inline]
    const fn as_mut_ptr(&self) -> *mut Self {
        self.0.get().cast()
    }
}

impl Drop for ThreadLockState {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `self` holds an initialized C++ `lockdep::ThreadLockState` being destroyed
        // exactly once with exclusive access.
        unsafe { lock_dep_ffi::cpp_lockdep_thread_lock_state_destroy(self.as_mut_ptr()) };
    }
}

/// Loop detection thread. Traverses the lock dependency graph to find circular
/// lock dependencies.
extern "C" fn lock_dep_thread(_arg: *mut c_void) -> i32 {
    loop {
        // Check to see if our graph has been flagged as dirty once every 2 seconds.
        let _ = thread::sleep_relative(DurationMono::from_seconds(2));

        if LOOP_DETECTION_GRAPH_IS_DIRTY.load() {
            LOOP_DETECTION_GRAPH_IS_DIRTY.store(false);
            // SAFETY: Called solely from the single dedicated `lockdep` loop-detection thread
            // (spawned at `LK_INIT_LEVEL_THREADING`, after `lock_dep_event_init` at
            // `LK_INIT_LEVEL_EARLIEST`), satisfying `lockdep::LoopDetectionPass`'s non-reentrancy
            // requirement and guaranteeing `DETECTION_COMPLETE_EVENT` is initialized.
            unsafe {
                lock_dep_ffi::cpp_lockdep_loop_detection_pass();
                DETECTION_COMPLETE_EVENT.get_unchecked().signal();
            }
        }
    }
}

fn lock_dep_event_init(_level: LkInitLevel) {
    // SAFETY: `LK_INIT_LEVEL_EARLIEST` runs once on CPU 0 in `lk_main` with interrupts disabled
    // before `bootstrap2`, kernel threads, or secondary CPUs exist.
    unsafe {
        let Ok(()) = Pin::static_ref(&DETECTION_COMPLETE_EVENT).init_pin(Event::init_unsignaled());
        let Ok(()) = Pin::static_ref(&DETECTION_COMPLETE_LOCK)
            .init_pin(pin_init!(DetectionCompleteLock { lock <- KMutex::init() }));
    }
}

init::lk_init_hook!(lockdep_event, lock_dep_event_init, LK_INIT_LEVEL_EARLIEST);

fn lock_dep_init(_level: LkInitLevel) {
    // SAFETY: `c"lockdep"` is a valid NUL-terminated C string, `lock_dep_thread` is safe to run on
    // a new kernel thread with a null argument, and `LK_INIT_LEVEL_THREADING` runs in blockable
    // thread context with no active `ChainLockTransaction` and before `t` can be started elsewhere.
    unsafe {
        let t = thread::create_with_priority(
            c"lockdep".as_ptr(),
            lock_dep_thread,
            ptr::null_mut(),
            LOW_PRIORITY,
        )
        .expect("failed to create lockdep thread");
        t.detach_and_resume().expect("failed to resume lockdep thread");
    }
}

init::lk_init_hook!(lockdep, lock_dep_init, LK_INIT_LEVEL_THREADING);

/// Waits for a loop detection pass to complete, or times out. This is exposed for tests.
///
/// # Safety
///
/// Must only be called in blockable thread context after `LK_INIT_LEVEL_EARLIEST`
/// (`lock_dep_event_init`) and threading initialization have completed.
unsafe fn trigger_and_wait_for_loop_detection(deadline: InstantMono) -> Result<(), Status> {
    // SAFETY: Caller guarantees `LK_INIT_LEVEL_EARLIEST` (`lock_dep_event_init`) has completed.
    ksync::lock!(let _guard = unsafe { DETECTION_COMPLETE_LOCK.get_unchecked() }.lock_lock());
    // SAFETY: As above.
    let _ = unsafe { DETECTION_COMPLETE_EVENT.get_unchecked() }.unsignal();
    system_trigger_loop_detection();
    // SAFETY: As above.
    unsafe { DETECTION_COMPLETE_EVENT.get_unchecked() }.wait_deadline(deadline, Interruptible::Yes)
}

/// Prints a kernel oops when a normal lock order violation is detected
/// (`lockdep::SystemLockValidationError`).
///
/// # Safety
///
/// - `lock_entry` and `conflicting_entry` must have non-null lock class IDs pointing to valid
///   [`ValidatorLockClassState`] instances.
/// - The calling CPU must have a valid, initialized current `Thread`.
/// - `caller_frame` must either lie outside the calling thread's stack bounds (such as null) or be
///   a valid, aligned, initialized frame pointer on the calling thread's stack.
#[cold]
unsafe fn system_lock_validation_error(
    lock_entry: &AcquiredLockEntry,
    conflicting_entry: &AcquiredLockEntry,
    _state: &ThreadLockState,
    caller_address: *const c_void,
    caller_frame: *const c_void,
    result: LockResult,
) {
    // SAFETY: Caller guarantees the calling CPU has a valid, initialized current `Thread`.
    let current_thread = unsafe { &*thread::current_get() };

    let mut owner_name = [0u8; ZX_MAX_NAME_LEN];
    current_thread.owner_name(&mut owner_name);
    let owner_name = thread::name_buf_as_bytes(&owner_name);

    let pid = current_thread.pid();
    let tid = current_thread.tid();

    let thread_name = current_thread.name();
    kernel_oops!(
        "Lock validation failed for thread {:p} pid {:u} tid {:u} ({:s}:{:s}):\n",
        ptr::from_ref(current_thread),
        pid,
        tid,
        owner_name,
        thread_name.as_bytes(),
    );
    kprintln!("Reason: {:s}", result.to_cstr());
    // SAFETY: Caller guarantees `lock_entry` and `conflicting_entry` have valid lock class IDs.
    let (bad_id, conflicting_id) = unsafe { (lock_entry.id(), conflicting_entry.id()) };
    kprintln!(
        "Bad lock: name={:s} order={:u}",
        ValidatorLockClassState::get_name(bad_id),
        lock_entry.order(),
    );
    kprintln!(
        "Conflict: name={:s} order={:u}",
        ValidatorLockClassState::get_name(conflicting_id),
        conflicting_entry.order(),
    );
    kprintln!("caller={:p} frame={:p}", caller_address, caller_frame);

    let mut bt = Backtrace::new();
    // SAFETY: Caller guarantees `caller_frame` is either outside stack bounds or a valid, aligned,
    // initialized frame pointer on the calling thread's stack.
    unsafe { thread::current_get_backtrace_with_fp(caller_frame.expose_provenance(), &mut bt) };
    bt.print();
    kprint!("\n");
}

/// Issues a kernel panic when a fatal lock order violation is detected
/// (`lockdep::SystemLockValidationFatal`).
///
/// # Safety
///
/// `lock_entry` must have a non-null lock class ID pointing to a valid
/// [`ValidatorLockClassState`].
#[cold]
unsafe fn system_lock_validation_fatal(
    lock_entry: &AcquiredLockEntry,
    _state: &ThreadLockState,
    caller_address: *const c_void,
    caller_frame: *const c_void,
    result: LockResult,
) -> ! {
    // SAFETY: Caller guarantees `lock_entry` has a valid lock class ID.
    let lock_id = unsafe { lock_entry.id() };
    panic!(
        "Fatal lock violation detected! name={}, reason={}, pc={:p}, stack frame={:p}",
        zr::from_utf8_lossy(ValidatorLockClassState::get_name(lock_id).to_bytes()),
        zr::from_utf8_lossy(result.to_cstr().to_bytes()),
        caller_address,
        caller_frame,
    );
}

/// Prints a kernel oops when a circular lock dependency is detected
/// (`lockdep::SystemCircularLockDependencyDetected`).
#[cold]
fn system_circular_lock_dependency_detected(connected_set_root: &ValidatorLockClassState) {
    kernel_oops!("Circular lock dependency detected:\n");

    for node in ValidatorLockClassState::iter() {
        if ptr::eq(node.connected_set(), connected_set_root) {
            for dependency_id in node.dependency_set() {
                let dependency = ValidatorLockClassState::get(dependency_id);
                if ptr::eq(dependency.connected_set(), connected_set_root) {
                    kprintln!("  {:s} -> {:s}", dependency.name(), node.name());
                }
            }
        }
    }

    kprint!("\n");
}

/// Returns a pointer to the [`ThreadLockState`] instance for the current thread when in thread
/// context or for the current CPU when in irq context (`lockdep::SystemGetThreadLockState`).
///
/// # Safety
///
/// - When `(lock_flags & LOCK_FLAGS_IRQ_SAFE) != 0`, the current CPU's `PerCpu` pointer must be
///   initialized.
/// - Otherwise, the calling CPU must have a valid, initialized current `Thread`.
#[inline]
unsafe fn system_get_thread_lock_state(lock_flags: LockFlags) -> *mut ThreadLockState {
    if (lock_flags & LOCK_FLAGS_IRQ_SAFE) != 0 {
        PerCpu::get_current().lock_state.as_mut_ptr()
    } else {
        // SAFETY: Caller guarantees the calling CPU has a valid, initialized current `Thread`.
        unsafe { Thread::lock_state_ptr(thread::current_get()) }
    }
}

/// Initializes an instance of [`ThreadLockState`] (`lockdep::SystemInitThreadLockState`).
#[inline]
pub fn system_init_thread_lock_state(_state: &ThreadLockState) {}

/// There is no explicit event based triggering mechanism for lockdep when used
/// in the kernel. The loop detection thread will simply poll the "dirty" flag
/// once every 2 seconds, clearing the flag and performing a check if the flag
/// is set.
#[inline]
fn system_trigger_loop_detection() {
    LOOP_DETECTION_GRAPH_IS_DIRTY.store(true);
}

#[cfg(console_enabled)]
mod console {
    use super::{ValidatorLockClassState, system_trigger_loop_detection};
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use core::ffi::c_int;
    use core::ptr;
    use kprint::kprintln;

    /// Dumps the state of the lock dependency graph.
    fn dump_lock_class_state() {
        kprintln!("Lock class states:");
        for state in ValidatorLockClassState::iter() {
            kprintln!("  {:s} {{", state.name());
            for id in state.dependency_set() {
                kprintln!("    {:s}", ValidatorLockClassState::get_name(id));
            }
            kprintln!("  }}");
        }
        kprintln!("\nConnected sets:");
        for state in ValidatorLockClassState::iter() {
            // Only handle root nodes in the outer loop. The nested loop will pick
            // up all of the child nodes under each parent node.
            if ptr::eq(state.connected_set(), state) {
                kprintln!("{{");
                for other_state in ValidatorLockClassState::iter() {
                    if ptr::eq(other_state.connected_set(), state) {
                        kprintln!("  {:s}", other_state.name());
                    }
                }
                kprintln!("}}");
            }
        }
    }

    /// Top-level lockdep command.
    ///
    /// # Safety
    ///
    /// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where
    /// `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
    unsafe extern "C" fn command_lock_dep(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return -1;
        }
        // SAFETY: `argc > 0` and `argv` is non-null; caller guarantees `argv` is valid for `argc`
        // elements.
        let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
        let usage = || -> c_int {
            kprintln!("{:s} dump              : dump lock classes", argv[0].as_str());
            kprintln!("{:s} loop              : trigger loop detection pass", argv[0].as_str());
            -1
        };
        if argc < 2 {
            kprintln!("Not enough arguments:");
            return usage();
        }

        if argv[1].as_str() == "dump" {
            dump_lock_class_state();
        } else if argv[1].as_str() == "loop" {
            kprintln!("Triggering loop detection pass:");
            system_trigger_loop_detection();
        } else {
            kprintln!("Unrecognized subcommand: '{:s}'", argv[1].as_str());
            return usage();
        }

        0
    }

    static_command!(
        CMD_LOCKDEP,
        c"lockdep".as_ptr(),
        c"kernel lock diagnostics".as_ptr(),
        command_lock_dep,
        CMD_AVAIL_NORMAL
    );
}
