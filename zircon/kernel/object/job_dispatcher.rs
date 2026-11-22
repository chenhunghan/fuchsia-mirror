// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::Name;
use super::channel_dispatcher::ChannelDispatcher;
use super::dispatcher::DispatcherOps;
use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::job_dispatcher_ffi::cpp_job_dispatcher_alloc;
use super::job_policy::JobPolicy;
use super::process_dispatcher::ProcessDispatcher;
use super::root_job_observer::RootJobObserver;
use super::singleton::get_root_job_dispatcher;
use crate::counters::define_kcounter;
use crate::kernel::deadline::{DurationUnknown, SlackMode, TimerSlack};
use crate::kernel::task_runtime_stats::TaskRuntimeStats;
use crate::ksync_kernel::{CriticalMutexOrderedPolicy, TrackedCriticalMutex};
use core::cmp::Ordering;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{
    Array, Canary, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    HasRefCount, HasRefCountUpgradeable, InlineArray, IsOpaqueRefCounted, Recyclable, RefPtr,
    SinglyLinkedList, SinglyLinkedListContainable, SinglyLinkedListNode, TrackingSize, ref_counted,
};
use ksync::{KMutex, LOCK_FLAGS_NESTABLE, guarded};
use object_constants_rs as object_constants;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init, stack_try_pin_init};
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_JOB_RIGHTS, ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT,
    ZX_EXCEPTION_CHANNEL_TYPE_JOB, ZX_EXCEPTION_CHANNEL_TYPE_JOB_DEBUGGER, ZX_JOB_NO_JOBS,
    ZX_JOB_NO_PROCESSES, ZX_JOB_TERMINATED, ZX_MAX_NAME_LEN, ZX_OBJ_TYPE_JOB, ZX_OBJECT_SIGNAL_6,
    ZX_POL_OVERRIDE_DENY, ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL, ZX_TASK_RETCODE_OOM_KILL,
    ZX_TIMER_SLACK_CENTER, ZX_TIMER_SLACK_EARLY, ZX_TIMER_SLACK_LATE, zx_info_job_t,
    zx_info_task_runtime_t, zx_koid_t, zx_policy_basic_v1_t, zx_policy_basic_v2_t,
    zx_policy_timer_slack_t, zx_rights_t, zx_signals_t,
};

/// Job signal active when a job has no child jobs and no child processes.
///
/// TODO(https://fxbug.dev/42131457): This is a temporary signal that we don't want userspace using
/// (yet?). Either expose this signal to userspace in "zircon/types.h", or remove this signal.
pub const ZX_JOB_NO_CHILDREN: zx_signals_t = ZX_OBJECT_SIGNAL_6;

zr::static_assert!(core::mem::size_of::<zx_policy_basic_v1_t>() == 8);
zr::static_assert!(core::mem::align_of::<zx_policy_basic_v1_t>() == 4);
zr::static_assert!(core::mem::size_of::<zx_policy_basic_v2_t>() == 12);
zr::static_assert!(core::mem::align_of::<zx_policy_basic_v2_t>() == 4);
zr::static_assert!(core::mem::size_of::<zx_policy_timer_slack_t>() == 16);
zr::static_assert!(core::mem::align_of::<zx_policy_timer_slack_t>() == 8);

define_kcounter!(DISPATCHER_JOB_CREATE_COUNT, "dispatcher.job.create", Sum);
define_kcounter!(DISPATCHER_JOB_DESTROY_COUNT, "dispatcher.job.destroy", Sum);

/// The starting max_height value of the root job.
const ROOT_JOB_MAX_HEIGHT: u32 = 32;

/// The name assigned to the root job by [`JobDispatcher::create_root_job`].
const ROOT_JOB_NAME: &[u8] = b"root";

/// Number of debug exceptionates snapshotted without a heap allocation in
/// [`JobDispatcher::for_each_debug_exceptionate`].
const DEBUG_EXCEPTIONATE_INLINE_COUNT: usize = 4;

/// Assume the typical set-policy call has 8 items or less.
const POLICY_BASIC_INLINE_COUNT: usize = 8;

/// Capacity of an [`OomBitJobArray`].
const OOM_BIT_JOB_ARRAY_SIZE: usize = 8;

/// Interface for walking a job/process tree.
///
/// # Execution Context
///
/// [`JobDispatcher::enumerate_children`] invokes `on_job` and `on_process` without holding the
/// job's lock so callbacks may perform user copies, whereas
/// [`JobDispatcher::enumerate_children_recursive`] invokes them while holding all ancestor
/// `JobDispatcher` `CriticalMutex` locks so callbacks must not fault on user copies or acquire
/// unordered ancestor job locks.
pub trait JobEnumerator {
    /// Visits a job. If `on_job` returns false, the enumeration stops.
    fn on_job(&mut self, _job: &JobDispatcher) -> bool {
        true
    }

    /// Visits a process. If `on_process` returns false, the enumeration stops.
    fn on_process(&mut self, _proc: &ProcessDispatcher) -> bool {
        true
    }
}

/// Tag for a `JobDispatcher`'s parent's raw job list; exclusively used while holding the parent
/// job's `lock`.
struct JobDispatcherRawListTag;
/// Tag for a `JobDispatcher`'s parent's job list; exclusively used by the single `kill` call that
/// transitions the parent job out of [`State::Ready`] under `lock`.
struct JobDispatcherListTag;

/// Lifecycle state of a [`JobDispatcher`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Accepting children; the only state in which policy can be set.
    Ready,
    /// [`JobDispatcher::kill`] has run; no new children are accepted while the existing ones
    /// are torn down.
    Killing,
    /// Every child is gone and `ZX_JOB_TERMINATED` has been raised.
    Dead,
}

/// References to the live children of a job, collected under the job lock and released by the
/// caller once the lock has been dropped. See [`JobDispatcher::for_each_child_in_locked`].
struct LiveRefsArray<T: HasRefCount + Recyclable> {
    /// Uninitialized-capable backing allocation; elements in `0..len` are initialized.
    buf: Array<MaybeUninit<RefPtr<T>>>,
    /// Number of initialized elements in `buf[..len]`; always `<= buf.len()`.
    len: usize,
}

impl<T: HasRefCount + Recyclable> LiveRefsArray<T> {
    /// Creates an empty `LiveRefsArray` without allocating.
    const fn new() -> Self {
        Self { buf: Array::new(), len: 0 }
    }

    /// Allocates uninitialized storage for up to `capacity` `RefPtr<T>`s.
    fn try_with_capacity(capacity: usize) -> Result<Self, kalloc::AllocError> {
        Ok(Self { buf: Array::<RefPtr<T>>::try_new_uninit_slice(capacity)?, len: 0 })
    }

    /// Appends `item` into the next uninitialized slot without running drop glue.
    ///
    /// # Safety
    ///
    /// `self.len` must be strictly less than `self.buf.len()`.
    #[inline]
    unsafe fn push_unchecked(&mut self, item: RefPtr<T>) {
        debug_assert!(self.len < self.buf.len());
        // SAFETY: The caller guarantees `self.len < self.buf.len()`.
        let slot = unsafe { self.buf.get_unchecked_mut(self.len) };
        slot.write(item);
        self.len += 1;
    }
}

impl<T: HasRefCount + Recyclable> Default for LiveRefsArray<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: HasRefCount + Recyclable> core::ops::Deref for LiveRefsArray<T> {
    type Target = [RefPtr<T>];

    #[inline]
    fn deref(&self) -> &[RefPtr<T>] {
        // SAFETY: `self.len <= self.buf.len()` and every element in `0..self.len` was initialized
        // by `push_unchecked`.
        unsafe { self.buf.get_unchecked(..self.len).assume_init_ref() }
    }
}

impl<T: HasRefCount + Recyclable> Drop for LiveRefsArray<T> {
    fn drop(&mut self) {
        // SAFETY: `self.len <= self.buf.len()` and every element in `0..self.len` was initialized
        // by `push_unchecked` and has not yet been dropped.
        unsafe { self.buf.get_unchecked_mut(..self.len).assume_init_drop() };
    }
}

/// A job's raw (non-owning) list of child jobs.
type RawJobList = DoublyLinkedList<NonNull<JobDispatcher>, JobDispatcherRawListTag, TrackingSize>;
/// Owning list of jobs still to be killed, used by [`JobDispatcher::kill`].
type JobList = SinglyLinkedList<RefPtr<JobDispatcher>, JobDispatcherListTag>;

struct ProcessDispatcherRawJobListTag;
struct ProcessDispatcherJobListTag;

impl DoublyLinkedListContainable<ProcessDispatcher, ProcessDispatcherRawJobListTag>
    for ProcessDispatcher
{
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<ProcessDispatcher> {
        // SAFETY: `ProcessDispatcherRawJobListTag` is private to `job_dispatcher.rs`, where all
        // list/node accesses hold the parent job's `JobDispatcherState::lock`.
        unsafe { self.raw_job_list_node() }
    }
}

impl SinglyLinkedListContainable<ProcessDispatcher, ProcessDispatcherJobListTag>
    for ProcessDispatcher
{
    #[inline]
    fn get_node(&self) -> &SinglyLinkedListNode<ProcessDispatcher> {
        // SAFETY: `ProcessDispatcherJobListTag` is private to `job_dispatcher.rs`, where only the
        // unique `kill` caller transitioning the parent job out of `State::Ready` uses the list.
        unsafe { self.job_list_node() }
    }
}

/// A job's raw (non-owning) list of child processes.
type RawProcessList =
    DoublyLinkedList<NonNull<ProcessDispatcher>, ProcessDispatcherRawJobListTag, TrackingSize>;
/// Owning list of processes still to be killed, used by [`JobDispatcher::kill`].
type ProcessList = SinglyLinkedList<RefPtr<ProcessDispatcher>, ProcessDispatcherJobListTag>;

/// Fixed-capacity collection of OOM-kill candidates; see
/// [`JobDispatcher::collect_jobs_with_oom_bit`].
struct OomBitJobArray {
    /// Uninitialized backing slots; elements in `0..count` are initialized.
    jobs: [MaybeUninit<RefPtr<JobDispatcher>>; OOM_BIT_JOB_ARRAY_SIZE],
    /// Number of initialized elements in `jobs[..count]`; always `<= OOM_BIT_JOB_ARRAY_SIZE`.
    count: usize,
}

impl OomBitJobArray {
    /// Creates an empty `OomBitJobArray` without initializing its backing slots.
    const fn new() -> Self {
        Self { jobs: [const { MaybeUninit::uninit() }; OOM_BIT_JOB_ARRAY_SIZE], count: 0 }
    }

    /// Returns whether `self` has reached its fixed capacity.
    #[inline]
    const fn is_full(&self) -> bool {
        self.count >= OOM_BIT_JOB_ARRAY_SIZE
    }

    /// Appends `job` into the next uninitialized slot without running drop glue.
    ///
    /// # Safety
    ///
    /// `self.count` must be strictly less than [`OOM_BIT_JOB_ARRAY_SIZE`].
    #[inline]
    unsafe fn push_unchecked(&mut self, job: RefPtr<JobDispatcher>) {
        debug_assert!(self.count < OOM_BIT_JOB_ARRAY_SIZE);
        // SAFETY: The caller guarantees `self.count < OOM_BIT_JOB_ARRAY_SIZE`.
        let slot = unsafe { self.jobs.get_unchecked_mut(self.count) };
        slot.write(job);
        self.count += 1;
    }
}

impl Default for OomBitJobArray {
    fn default() -> Self {
        Self::new()
    }
}

impl core::ops::Deref for OomBitJobArray {
    type Target = [RefPtr<JobDispatcher>];

    #[inline]
    fn deref(&self) -> &[RefPtr<JobDispatcher>] {
        // SAFETY: `self.count <= OOM_BIT_JOB_ARRAY_SIZE` and every element in `0..self.count` was
        // initialized by `push_unchecked`.
        unsafe { self.jobs.get_unchecked(..self.count).assume_init_ref() }
    }
}

impl core::ops::DerefMut for OomBitJobArray {
    #[inline]
    fn deref_mut(&mut self) -> &mut [RefPtr<JobDispatcher>] {
        // SAFETY: `self.count <= OOM_BIT_JOB_ARRAY_SIZE` and every element in `0..self.count` was
        // initialized by `push_unchecked`.
        unsafe { self.jobs.get_unchecked_mut(..self.count).assume_init_mut() }
    }
}

impl Drop for OomBitJobArray {
    fn drop(&mut self) {
        // SAFETY: `self.count <= OOM_BIT_JOB_ARRAY_SIZE` and every element in `0..self.count` was
        // initialized by `push_unchecked` and has not yet been dropped.
        unsafe { self.jobs.get_unchecked_mut(..self.count).assume_init_drop() };
    }
}

/// Returns whether `obj` is currently linked into the list identified by `Tag`.
fn in_container<T, Tag>(obj: &T) -> bool
where
    T: DoublyLinkedListContainable<T, Tag>,
{
    DoublyLinkedListContainable::<T, Tag>::get_node(obj).in_container()
}

/// Sorts `slice` in-place using `compare`, preserving the relative order of equal elements.
fn stable_sort_by<T, F>(slice: &mut [T], mut compare: F)
where
    F: FnMut(&T, &T) -> Ordering,
{
    for i in 1..slice.len() {
        let mut j = i;
        while j > 0 && compare(&slice[j - 1], &slice[j]) == Ordering::Greater {
            slice.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// `DebugExceptionate` is a specialization for [`Exceptionate`] because we allow multiple debug
/// exception channels on a job. Rather than being statically owned, its lifecycle is dynamically
/// managed by [`JobDispatcher::create_debug_exceptionate`].
#[ref_counted]
#[derive(Recyclable, DoublyLinkedListContainable)]
#[pin_data]
#[repr(C)]
struct DebugExceptionate {
    /// The `ZX_EXCEPTION_CHANNEL_TYPE_JOB_DEBUGGER` exceptionate itself.
    #[pin]
    exceptionate: Exceptionate,
    /// Node through which the owning job links this entry into its `debug_exceptionates` list.
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<DebugExceptionate>,
}

impl DebugExceptionate {
    /// Allocates a new `DebugExceptionate` of the given exception channel type.
    fn try_create(exception_type: u32) -> Result<RefPtr<Self>, kalloc::AllocError> {
        fbl::pin_make_ref_counted!(DebugExceptionate {
            exceptionate <- Exceptionate::new(exception_type),
            node: DoublyLinkedListNode::new(),
        })
    }
}

impl core::ops::Deref for DebugExceptionate {
    type Target = Exceptionate;

    /// Lets a `DebugExceptionate` be used wherever a plain [`Exceptionate`] is expected.
    #[inline]
    fn deref(&self) -> &Exceptionate {
        &self.exceptionate
    }
}

/// Internal state storage for [`JobDispatcher`].
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct JobDispatcherState {
    /// Canary value verifying a live `JobDispatcherState`.
    canary: Canary<{ fbl::magic(b"JOBD") }>,

    /// Maximum depth of the job tree below this job; one less than the parent's, and
    /// [`ROOT_JOB_MAX_HEIGHT`] for the root job. A job with `max_height == 0` cannot have children.
    max_height: u32,

    /// The parent job, or `None` for the root job. Set once at construction.
    parent: Option<RefPtr<JobDispatcher>>,

    /// The user-friendly job name. For debug purposes only. That
    /// is, there is no mechanism to mint a handle to a job via this name.
    #[pin]
    name: Name<ZX_MAX_NAME_LEN>,

    /// The code passed to [`JobDispatcher::kill`], reported through `ZX_INFO_JOB`.
    #[guarded_by(lock)]
    return_code: i64,
    /// Lifecycle state; only ever advances `Ready` -> `Killing` -> `Dead`.
    #[guarded_by(lock)]
    state: State,
    /// Whether this job volunteers to be killed by [`JobDispatcher::kill_job_with_kill_on_oom`].
    ///
    /// TODO(cpu): The OOM kill system is incomplete, see https://fxbug.dev/42107677 for details.
    #[guarded_by(lock)]
    kill_on_oom: bool,

    /// Access to the pointers in these lists, especially any promotions to
    /// `RefPtr`, must be handled very carefully, because the children can die
    /// even when `lock` is held. See [`JobDispatcher::for_each_child_in_locked`] for more details
    /// and for a safe way to enumerate them.
    #[pin]
    #[guarded_by(lock)]
    jobs: RawJobList,
    /// Raw child processes; see `jobs`.
    #[pin]
    #[guarded_by(lock)]
    procs: RawProcessList,

    /// The policy inherited by every child; frozen once the job has children.
    #[guarded_by(lock)]
    policy: JobPolicy,

    /// The `ZX_EXCEPTION_CHANNEL_TYPE_JOB` exceptionate, which has its own lock.
    #[pin]
    exceptionate: Exceptionate,
    /// The debugger exceptionates created by [`JobDispatcher::create_debug_exceptionate`],
    /// pruned of disconnected entries on every walk.
    #[pin]
    #[guarded_by(lock)]
    debug_exceptionates: DoublyLinkedList<RefPtr<DebugExceptionate>>,

    /// Aggregated runtime stats for processes that have exited.
    #[guarded_by(lock)]
    exited_process_runtime_stats: TaskRuntimeStats,

    /// Node through which the parent job links this job into its raw `jobs` list; exclusively
    /// accessed under the parent job's `lock`.
    #[pin]
    raw_list_node: DoublyLinkedListNode<JobDispatcher>,
    /// Node through which [`JobDispatcher::kill`] links this job into its transient [`JobList`];
    /// exclusively accessed by the single `kill` call that transitions the parent job out of
    /// [`State::Ready`] under the parent job's `lock`.
    list_node: SinglyLinkedListNode<JobDispatcher>,

    /// Nestable because every job shares one lock class and the tree walks
    /// ([`JobDispatcher::collect_jobs_with_oom_bit`],
    /// [`JobDispatcher::enumerate_children_recursive`]) hold a parent's lock while acquiring a
    /// child's, ordered by [`JobDispatcher::lock_order`].
    #[mutex(flags = LOCK_FLAGS_NESTABLE)]
    lock: KMutex<TrackedCriticalMutex>,
}

zr::static_assert_size_and_align!(
    JobDispatcherState,
    object_constants::kJobDispatcherStateSize,
    object_constants::kJobDispatcherStateAlign,
);

impl JobDispatcherState {
    /// Initializes the `JobDispatcherState` in-place.
    ///
    /// `parent` is `None` only for the root job; otherwise it must have `max_height() > 0`
    /// ([`JobDispatcher::create`] rejects deeper nesting with `ZX_ERR_OUT_OF_RANGE` before
    /// allocating).
    pub(super) fn init(
        parent: Option<RefPtr<JobDispatcher>>,
        policy: JobPolicy,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        let max_height = parent.as_ref().map_or(ROOT_JOB_MAX_HEIGHT, |p| {
            p.max_height().checked_sub(1).expect("parent job cannot have children")
        });
        pin_init!(Self {
            canary: Canary::new(),
            max_height,
            parent,
            name <- Name::init(),
            return_code: 0.into(),
            state: State::Ready.into(),
            kill_on_oom: false.into(),
            jobs <- ksync::kcell_init(DoublyLinkedList::new()),
            procs <- ksync::kcell_init(DoublyLinkedList::new()),
            policy: policy.into(),
            exceptionate <- Exceptionate::new(ZX_EXCEPTION_CHANNEL_TYPE_JOB),
            debug_exceptionates <- ksync::kcell_init(DoublyLinkedList::new()),
            exited_process_runtime_stats: TaskRuntimeStats::default().into(),
            raw_list_node: DoublyLinkedListNode::new(),
            list_node: SinglyLinkedListNode::new(),
            lock <- KMutex::init(),
            _: {
                // The creation counter is bumped only when the initializer is applied: a
                // `PinInit` dropped without being applied would otherwise count a create with
                // no matching destroy.
                DISPATCHER_JOB_CREATE_COUNT.add(1);
            },
        })
    }
}

#[pinned_drop]
impl PinnedDrop for JobDispatcherState {
    fn drop(self: Pin<&mut Self>) {
        // By the time the state is dropped, `JobDispatcher::leave_job_tree` has already unlinked
        // the job from its parent, so nothing else can reach this state any more.
        DISPATCHER_JOB_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Implements the Job object kernel interface. Each Job has a parent
    /// Job and zero or more child Jobs and zero or more Child processes. This
    /// creates a DAG (tree) that connects every living task in the system.
    /// This is critically important because of the bottoms up refcount nature of
    /// the system in which the scheduler keeps alive the thread and the thread keeps
    /// alive the process, so without the Job it would not be possible to enumerate
    /// or control the tasks in the system for which there are no outstanding handles.
    ///
    /// The second important job of the Job is to apply policies that cannot otherwise
    /// be easily enforced by capabilities, for example kernel object creation.
    ///
    /// The third one is to support exception propagation from the leaf tasks to
    /// the root tasks.
    ///
    /// Obviously there is a special case for the 'root' Job whose parent is `None`
    /// and in the current implementation will call platform_halt() when its process
    /// and job count reaches zero. The root job is not exposed to user mode, instead
    /// the single child Job of the root job is given to the userboot process.
    pub struct JobDispatcher,
    JobDispatcherState,
    ZX_OBJ_TYPE_JOB,
    object_constants::kJobDispatcherStateOffset
);

zr::static_assert!(core::mem::size_of::<JobDispatcher>() == 0);

// `JobDispatcher` reference counts live in the C++ `Dispatcher` base, which derives from
// `fbl::RefCountedUpgradeable`, so raw child pointers can be upgraded under the parent's lock.
impl HasRefCountUpgradeable for JobDispatcher {}

impl DoublyLinkedListContainable<JobDispatcher, JobDispatcherRawListTag> for JobDispatcher {
    /// The node through which the parent's raw `jobs` list links this job.
    fn get_node(&self) -> &DoublyLinkedListNode<JobDispatcher> {
        &self.state().raw_list_node
    }
}

impl SinglyLinkedListContainable<JobDispatcher, JobDispatcherListTag> for JobDispatcher {
    /// The node through which a [`JobList`] links this job.
    fn get_node(&self) -> &SinglyLinkedListNode<JobDispatcher> {
        &self.state().list_node
    }
}

impl JobDispatcher {
    /// Returns the default rights for a JobDispatcher handle.
    pub const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_JOB_RIGHTS
    }

    /// Unlinks this job from its parent and, if that was the parent's last child, finishes the
    /// parent's dead transition.
    ///
    /// Called from the C++ destructor before [`JobDispatcherState`] is dropped. The job must still
    /// be a complete, shared object at that point: until it has left the parent's `jobs` list, a
    /// tree walk holding the parent's lock can reach it (and, in
    /// [`Self::collect_jobs_with_oom_bit`], lock it) and only then learn from the failed refcount
    /// upgrade that it is dying.
    ///
    /// # Safety
    ///
    /// Must only be called once, from `JobDispatcher::~JobDispatcher()` (`ref_count == 0`) prior
    /// to `rust_job_dispatcher_state_destroy`, with `self.state().lock` not held by the calling
    /// thread. No un-upgraded `RefPtr` (`RefPtr::from_ref`) may be formed from `self`.
    pub(super) unsafe fn leave_job_tree(&self) {
        if let Some(parent) = self.remove_from_job_trees_unlocked() {
            parent.finish_dead_transition_unlocked();
        }
    }

    /// To come up with an order on our recursive locks we take advantage of the fact that our
    /// max_height reduces from parent to child. As we acquire locks from parent->child we can
    /// build an increasing counter by inverting the max_height. We add 1 to the counter just so
    /// the order value of 0 is reserved for the default order when the lock is acquired without an
    /// order being specified.
    fn lock_order(&self) -> usize {
        (ROOT_JOB_MAX_HEIGHT - self.max_height() + 1) as usize
    }

    /// Calls the provided `func` on all live elements of `children`, which must be one of `jobs`
    /// or `procs`. Stops iterating early if `func` returns an error, returning that error from
    /// this method. `lock` must be held when calling this method, and it will still be held while
    /// the callback is called.
    ///
    /// The returned [`LiveRefsArray`] needs to be destructed when `lock` is not held anymore. The
    /// recommended pattern is:
    ///
    /// ```rust
    ///  let refs: LiveRefsArray<_>;
    ///  {
    ///      ksync::lock!(let guard = self.state().lock_lock());
    ///      (refs, result) = for_each_child_in_locked(guard.jobs(), ...);
    ///  }
    /// ```
    ///
    /// # Safety
    ///
    /// `children` must be the `jobs` or `procs` list of a `JobDispatcherState` whose `lock` the
    /// caller holds (projected from a live guard of that lock), and `func` must keep that `lock`
    /// continuously held and must not mutate `children`.
    unsafe fn for_each_child_in_locked<T, Tag>(
        children: &DoublyLinkedList<NonNull<T>, Tag, TrackingSize>,
        mut func: impl FnMut(&RefPtr<T>) -> Result<(), Status>,
    ) -> (LiveRefsArray<T>, Result<(), Status>)
    where
        T: HasRefCount
            + Recyclable
            + HasRefCountUpgradeable
            + IsOpaqueRefCounted
            + DoublyLinkedListContainable<T, Tag>,
    {
        let count = children.len();

        if count == 0 {
            return (LiveRefsArray::default(), Ok(()));
        }

        let Ok(mut refs) = LiveRefsArray::<T>::try_with_capacity(count) else {
            return (LiveRefsArray::default(), Err(Status::NO_MEMORY));
        };

        let visit = |cref: RefPtr<T>| {
            let result = func(&cref);
            // As part of our contract with take_each_child_locked we must not
            // destroy the `cref` we were given, as it might be the last reference to
            // the object. Therefore we keep the reference alive in the `refs` array
            // and pass the responsibility of releasing them outside the lock to the
            // caller.
            // SAFETY: `count` was read from `children.len()` (`TrackingSize`) under the `lock`
            // that guards `children`, and `for_each_child_in_locked`'s safety contract requires
            // `func` to keep that `lock` continuously held without mutating `children`, so
            // `take_each_child_locked` invokes `visit` at most `count == refs.buf.len()` times and
            // `refs.len < refs.buf.len()` holds on every call.
            unsafe { refs.push_unchecked(cref) };
            result
        };
        // SAFETY: The caller guarantees that `children` is projected from a held `lock`, and
        // `visit` retains every `RefPtr` in `refs` without mutating `children`.
        let result = unsafe { Self::take_each_child_locked(children, visit) };
        (refs, result)
    }

    /// Calls the provided `func` on all live elements of `children`, which must be one of `jobs`
    /// or `procs`. The callback must retain the `RefPtr` and not destroy it until after this
    /// method returns and the lock is dropped. Stops iterating early if `func` returns an error,
    /// returning that error from this method.
    ///
    /// # Safety
    ///
    /// `children` must be the `jobs` or `procs` list of a `JobDispatcherState` whose `lock` the
    /// caller holds (projected from a live guard of that lock), and `func` must keep that `lock`
    /// continuously held, must not mutate `children`, and must retain each `RefPtr` passed to it
    /// until after that `lock` is released.
    unsafe fn take_each_child_locked<T, Tag>(
        children: &DoublyLinkedList<NonNull<T>, Tag, TrackingSize>,
        mut func: impl FnMut(RefPtr<T>) -> Result<(), Status>,
    ) -> Result<(), Status>
    where
        T: HasRefCount
            + Recyclable
            + HasRefCountUpgradeable
            + IsOpaqueRefCounted
            + DoublyLinkedListContainable<T, Tag>,
    {
        const { assert!(core::mem::size_of::<T>() == 0) };

        // Convert child raw pointers into RefPtrs. This is tricky and requires
        // special logic on the RefPtr class to handle a ref count that can be
        // zero.
        //
        // The main requirement is that `lock` is both controlling child
        // list lookup and also making sure that the child destructor cannot
        // make progress when doing so. In other words, when inspecting the
        // `children` list we can be sure that a given child process or child
        // job is either
        //   - alive, with refcount > 0
        //   - in destruction process but blocked, refcount == 0

        for craw in children.forward_iter() {
            // SAFETY: `craw` is linked into `children`, and a child only unlinks itself (in
            // `remove_child_job` / `remove_child_process`) under the `lock` the caller holds, so
            // its memory stays valid for the duration of this call even if its reference count
            // has already dropped to zero. `T` is a zero-sized `OpaqueRefCountedFacade` wrapping
            // `UnsafeCell<()>`, so `craw: &T` carries zero `Freeze` bytes and preserves provenance.
            let cref = unsafe { RefPtr::make_ref_ptr_upgrade_from_raw(core::ptr::from_ref(craw)) };
            let Some(cref) = cref else {
                continue;
            };

            // `cref` might be the last reference at this point. If so,
            // when we drop it in the next iteration the object dtor
            // would be called here with the `lock` held. To avoid that we pass
            // ownership of the refptr to the callback who is required to keep it alive.
            func(cref)?;
        }

        Ok(())
    }

    /// Creates the root job, which has no parent and the root policy.
    pub fn create_root_job() -> RefPtr<JobDispatcher> {
        let Some(job) = Self::alloc(None, &JobPolicy::create_root_policy()) else {
            panic!("root-job: failed to allocate");
        };
        let status = job.set_name(ROOT_JOB_NAME);
        debug_assert!(status.is_ok());
        job
    }

    /// Creates a child `JobDispatcher` under `parent`.
    pub fn create(
        _flags: u32,
        parent: RefPtr<JobDispatcher>,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        if parent.max_height() == 0 {
            // The parent job cannot have children.
            return Err(Status::OUT_OF_RANGE);
        }

        let policy = parent.get_policy();
        let new_handle =
            KernelHandle::new(Self::alloc(Some(parent.clone()), &policy).ok_or(Status::NO_MEMORY)?);

        // SAFETY: `new_handle` was just allocated with `Some(parent.clone())` as its parent and
        // has never been linked into any `jobs` list.
        if !unsafe { parent.add_child_job(new_handle.dispatcher()) } {
            return Err(Status::BAD_STATE);
        }

        Ok((new_handle, Self::default_rights()))
    }

    /// Allocates and constructs a `JobDispatcher` (the C++ constructor), returning `None` if
    /// memory is exhausted.
    fn alloc(parent: Option<RefPtr<JobDispatcher>>, policy: &JobPolicy) -> Option<RefPtr<Self>> {
        let parent_raw =
            parent.map_or(core::ptr::null_mut(), |parent| RefPtr::into_raw(parent).cast_mut());
        // SAFETY: `parent_raw` is null or carries an acquired reference count that
        // `cpp_job_dispatcher_alloc` takes ownership of (even on allocation failure), `policy`
        // outlives the call, and `cpp_job_dispatcher_alloc` returns null or a pointer exported
        // with an acquired reference count.
        unsafe { RefPtr::try_from_raw(cpp_job_dispatcher_alloc(parent_raw, policy)) }
    }

    /// Returns the koid of the parent job, or 0 for the root job.
    pub fn get_related_koid(&self) -> zx_koid_t {
        self.parent().map_or(0, |parent| parent.get_koid())
    }

    /// Returns the parent of this job, or `None` for the root job.
    pub fn parent(&self) -> Option<&RefPtr<JobDispatcher>> {
        self.state().parent.as_ref()
    }

    /// Returns the maximum depth of the job tree below this job.
    fn max_height(&self) -> u32 {
        self.state().max_height
    }

    /// Registers `process` as a child of this job. Returns false if the job no longer accepts
    /// children (it is killing or dead).
    ///
    /// # Safety
    ///
    /// `process`'s parent job must be `self` and `process` must not yet be linked into any `procs`
    /// list.
    pub(super) unsafe fn add_child_process(&self, process: &RefPtr<ProcessDispatcher>) -> bool {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.state().lock_lock());
        if *guard.state() != State::Ready {
            return false;
        }
        debug_assert!(!in_container::<_, ProcessDispatcherRawJobListTag>(&**process));
        // SAFETY: `RefPtr::as_ptr` returns a non-null pointer.
        let proc_ptr = unsafe { NonNull::new_unchecked(RefPtr::as_ptr(process).cast_mut()) };
        // SAFETY: The caller guarantees `process`'s parent is `self` and `process` is not yet
        // linked into any `procs` list, and `process`'s destructor unlinks itself under this
        // `lock` in `remove_child_process` before its memory is released.
        unsafe { guard.as_mut().procs_mut().push_back_raw(proc_ptr) };
        self.update_signals_locked(&guard.as_token_guard());
        true
    }

    /// Registers `job` as a child of this job. Returns false if the job no longer accepts
    /// children (it is killing or dead).
    ///
    /// # Safety
    ///
    /// `job`'s parent job must be `self` and `job` must not yet be linked into any `jobs` list.
    unsafe fn add_child_job(&self, job: &RefPtr<JobDispatcher>) -> bool {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.state().lock_lock());

        if *guard.state() != State::Ready {
            return false;
        }

        // Put the new job after our next-youngest child, or us if we have none.
        //
        // We try to make older jobs closer to the root (both hierarchically and
        // temporally) show up earlier in enumeration.

        // This can only be called once, the job should not already be part
        // of any job tree.
        debug_assert!(!in_container::<_, JobDispatcherRawListTag>(&**job));
        debug_assert!({
            let neighbor: &JobDispatcher = guard.jobs().back().unwrap_or(self);
            !core::ptr::eq(neighbor, RefPtr::as_ptr(job))
        });

        // SAFETY: `RefPtr::as_ptr` returns a non-null pointer.
        let job_ptr = unsafe { NonNull::new_unchecked(RefPtr::as_ptr(job).cast_mut()) };
        // SAFETY: The caller guarantees `job`'s parent is `self` and `job` is not in any `jobs`
        // list yet, and `job`'s destructor unlinks itself under this `lock` in `remove_child_job`
        // before its memory is released.
        unsafe { guard.as_mut().jobs_mut().push_back_raw(job_ptr) };
        self.update_signals_locked(&guard.as_token_guard());
        true
    }

    /// Removes `process` from this job's child list. Safe to call more than once for the same
    /// process.
    ///
    /// # Safety
    ///
    /// `process`'s parent job must be `self` (so that if `process` is linked into a child process
    /// list it is `self.state().procs` protected by `self.state().lock`). `process` may already
    /// have `ref_count == 0` when called from `~ProcessDispatcher()`, so no un-upgraded `RefPtr`
    /// (`RefPtr::from_ref`) may be formed from `process`.
    pub(super) unsafe fn remove_child_process(&self, process: &ProcessDispatcher) {
        self.state().canary.assert();

        let should_die;
        {
            ksync::lock!(let mut guard = self.state().lock_lock());
            // The process dispatcher can call us in its destructor, kill(),
            // or remove_thread().
            if !in_container::<_, ProcessDispatcherRawJobListTag>(process) {
                return;
            }
            // SAFETY: The caller guarantees `process`'s parent job is `self`, so `in_container`
            // checked above under `guard` proves `process` is linked into `self.state().procs`.
            let _ = unsafe { guard.as_mut().procs_mut().erase(process) };

            // Aggregate runtime stats from exiting process.
            *guard.as_mut().exited_process_runtime_stats_mut() += process.get_task_runtime_stats();
            let token_guard = guard.as_token_guard();
            self.update_signals_locked(&token_guard);
            should_die = self.is_ready_for_dead_transition_locked(&token_guard);
        }

        if should_die {
            self.finish_dead_transition_unlocked();
        }
    }

    /// Removes the given child job from `self`. Returns
    /// [`Self::is_ready_for_dead_transition_locked`] and the caller is responsible for calling
    /// [`Self::finish_dead_transition_unlocked`] if true.
    ///
    /// # Safety
    ///
    /// `job`'s parent job must be `self`, so that if `job` is linked into a child job list it is
    /// `self.state().jobs` protected by `self.state().lock`.
    #[must_use]
    unsafe fn remove_child_job(&self, job: &JobDispatcher) -> bool {
        self.state().canary.assert();

        ksync::lock!(let mut guard = self.state().lock_lock());
        if !in_container::<_, JobDispatcherRawListTag>(job) {
            return false;
        }

        // SAFETY: The caller guarantees `job`'s parent job is `self`, so `in_container` checked
        // above under `guard` proves `job` is linked into `self.state().jobs`.
        let _ = unsafe { guard.as_mut().jobs_mut().erase(job) };
        self.update_signals_locked(&guard.as_token_guard());
        self.is_ready_for_dead_transition_locked(&guard.as_token_guard())
    }

    /// Returns the current lifecycle state, taking the lock to read it.
    fn get_state(&self) -> State {
        ksync::lock!(let guard = self.state().lock_lock());
        *guard.state()
    }

    /// Remove this job from its parent's job list, either when the job was killed or its last
    /// reference was dropped. It's safe to call this multiple times.
    ///
    /// Returns `Some(parent)` if we both had a parent, and our removal caused that parent to return
    /// true for [`Self::is_ready_for_dead_transition_locked`], at which point the caller is
    /// responsible for calling [`Self::finish_dead_transition_unlocked`] on `parent`. Returns
    /// `None` otherwise.
    ///
    /// We cannot be holding our lock when we call this because it requires
    /// locking our parent, and we only nest locks down the tree.
    #[must_use]
    fn remove_from_job_trees_unlocked(&self) -> Option<&RefPtr<JobDispatcher>> {
        self.state().canary.assert();

        self.parent().filter(|parent| {
            // SAFETY: `parent` is `self.parent()`, so if `self` is linked into a `jobs` list it is
            // `parent.state().jobs`.
            unsafe { parent.remove_child_job(self) }
        })
    }

    /// Helper to transition into the [`State::Dead`] state.
    ///
    /// The check for whether we should transition needs to be done under the
    /// lock, but actually moving into the dead state has to be done after
    /// releasing the lock.
    ///
    /// Because `State::Ready -> State::Killing` is irreversible under `lock` and no new children
    /// can be added once `State::Killing` is entered, at most one critical section ever observes
    /// `true` and invokes [`Self::finish_dead_transition_unlocked`].
    fn is_ready_for_dead_transition_locked(
        &self,
        guard: &JobDispatcherStateLockTokenGuard<'_, '_>,
    ) -> bool {
        self.state().canary.assert();
        *guard.state() == State::Killing && guard.jobs().is_empty() && guard.procs().is_empty()
    }

    /// Completes the transition into the [`State::Dead`] state; see
    /// [`Self::is_ready_for_dead_transition_locked`].
    fn finish_dead_transition_unlocked(&self) {
        self.state().canary.assert();

        // Dead transition happens in a loop, since at every step we could be causing a parent to
        // need to finish its dead transition, and this loop allows us to avoid an unbounded
        // recursion.
        let mut current = self;
        loop {
            // Make sure we're killing from the bottom of the tree up or else parent
            // jobs could die before their children.
            //
            // In particular, this means we have to finish dying before leaving the job
            // trees, since the last child leaving the tree can trigger its parent to
            // finish dying.
            debug_assert!(current.parent().is_none_or(|parent| parent.get_state() != State::Dead));
            {
                ksync::lock!(let mut guard = current.state().lock_lock());
                debug_assert_eq!(*guard.state(), State::Killing);
                *guard.as_mut().state_mut() = State::Dead;
                current.state().exceptionate.shutdown();
                for debug_exceptionate in guard.debug_exceptionates().forward_iter() {
                    debug_exceptionate.shutdown();
                }
                current.update_state_locked(guard.token(), 0, ZX_JOB_TERMINATED);
            }

            let Some(parent) = current.remove_from_job_trees_unlocked() else {
                break;
            };
            current = &**parent;
        }
    }

    /// Set or clear the signals of this job to reflect its current state.
    fn update_signals_locked(&self, guard: &JobDispatcherStateLockTokenGuard<'_, '_>) {
        // Every job shares one lock class, so `guard`'s type does not prove which job's lock is
        // held; assert per-instance identity here (constant-folded at all call sites).
        assert!(core::ptr::eq(guard.parent(), self.state()));

        // Clear all signals, and mark the appropriate ones active.
        //
        // The active signals take precedence over the clear signals.
        let clear: zx_signals_t = ZX_JOB_NO_JOBS | ZX_JOB_NO_PROCESSES | ZX_JOB_NO_CHILDREN;

        // Removing jobs or processes.
        let mut set: zx_signals_t = 0;
        if guard.procs().is_empty() {
            set |= ZX_JOB_NO_PROCESSES;
        }
        if guard.jobs().is_empty() {
            set |= ZX_JOB_NO_JOBS;
        }
        if guard.jobs().is_empty() && guard.procs().is_empty() {
            set |= ZX_JOB_NO_CHILDREN;
        }

        self.update_state_locked(guard.token(), clear, set);
    }

    /// Returns a copy of this job's policy.
    ///
    /// Must not be called with this job's lock held.
    pub fn get_policy(&self) -> JobPolicy {
        // This may be called on a child job from inside an `enumerate_children_recursive`
        // callback, with the ancestors' (nestable) locks already held, so acquire our lock with
        // its position in the tree as the lock order.
        ksync::lock!(
            let guard = self.state().lock_lock_policy_with::<CriticalMutexOrderedPolicy>(
                self.lock_order()
            )
        );
        *guard.policy()
    }

    /// Kills its lowest child job that has `kill_on_oom` set.
    /// Returns false if no alive child job had `kill_on_oom` set.
    pub fn kill_job_with_kill_on_oom(&self) -> bool {
        // Get list of jobs with kill bit set.
        let mut oom_jobs = OomBitJobArray::new();
        self.collect_jobs_with_oom_bit(&mut oom_jobs);
        if oom_jobs.is_empty() {
            kprint::kprintln!("OOM: no jobs with kill_on_oom found");
            return false;
        }

        // Sort `oom_jobs` in descending order by max height.
        stable_sort_by(&mut oom_jobs, |a, b| b.max_height().cmp(&a.max_height()));

        // Kill lowest to highest until we find something to kill.
        for job in oom_jobs.iter().rev() {
            if job.kill(ZX_TASK_RETCODE_OOM_KILL) {
                let mut name = [0u8; ZX_MAX_NAME_LEN];
                let status = job.get_name(&mut name);
                debug_assert!(status.is_ok());
                kprint::kprintln!("OOM: killing {:u} '{:s}'", job.get_koid(), &name[..]);
                return true;
            }
        }

        kprint::kprintln!("OOM: no job found to kill");
        false
    }

    /// Collects all jobs with `kill_on_oom` set up to the maximum fixed size of an
    /// [`OomBitJobArray`]. `RefPtr`s stored in `into` must be released once the corresponding job
    /// lock has been released.
    fn collect_jobs_with_oom_bit(&self, into: &mut OomBitJobArray) {
        // As collect_jobs_with_oom_bit will recurse we need to give a lock order to the guard.
        ksync::lock!(
            let guard = self.state().lock_lock_policy_with::<CriticalMutexOrderedPolicy>(
                self.lock_order()
            )
        );

        if *guard.kill_on_oom() {
            if into.is_full() {
                kprint::kprintln!("OOM: skipping some jobs, exceeded max count");
                return;
            }

            // SAFETY: `self` is either the job the caller holds a reference to or a child reached
            // through its parent's `jobs` list while holding the parent's lock, which the child's
            // destructor must acquire (in `remove_child_job`) before its memory is released.
            let cref = unsafe { RefPtr::make_ref_ptr_upgrade_from_raw(core::ptr::from_ref(self)) };
            let Some(cref) = cref else {
                return;
            };
            // SAFETY: `into.is_full()` returned `false` above and `into` has not been modified
            // since, so `into.count < OOM_BIT_JOB_ARRAY_SIZE`.
            unsafe { into.push_unchecked(cref) };
        }

        for job in guard.jobs().forward_iter() {
            job.collect_jobs_with_oom_bit(into);
        }
    }

    /// Terminate the child processes and jobs. Returns `false` if the job is already
    /// in the process of killing, or the children are already terminated. Regardless
    /// of return value, the job now will not accept new children and eventually
    /// transitions to [`State::Dead`]. `return_code` can be obtained via `ZX_INFO_JOB`.
    pub fn kill(&self, return_code: i64) -> bool {
        self.state().canary.assert();

        let mut jobs_to_kill = JobList::default();
        let mut procs_to_kill = ProcessList::default();

        // First transition our 'root' job into the `State::Killing` state. If this cannot be done
        // then we consider `kill` to have failed, otherwise it succeeds and we clean all the
        // children up.
        if !self.kill_one(return_code, &mut jobs_to_kill, &mut procs_to_kill) {
            debug_assert!(jobs_to_kill.is_empty());
            debug_assert!(procs_to_kill.is_empty());
            return false;
        }

        // Since we kill the child jobs first we have a depth-first massacre.
        while let Some(job) = jobs_to_kill.pop_front() {
            let _ = job.kill_one(return_code, &mut jobs_to_kill, &mut procs_to_kill);
        }

        while let Some(proc) = procs_to_kill.pop_front() {
            proc.kill(return_code);
        }

        true
    }

    /// Helper that transitions `self` to [`State::Killing`] and adds the children to `*_to_kill`
    /// lists for further cleanup.
    fn kill_one(
        &self,
        return_code: i64,
        jobs_to_kill: &mut JobList,
        procs_to_kill: &mut ProcessList,
    ) -> bool {
        let should_die;
        {
            ksync::lock!(let mut guard = self.state().lock_lock());
            if *guard.state() != State::Ready {
                return false;
            }

            *guard.as_mut().return_code_mut() = return_code;
            *guard.as_mut().state_mut() = State::Killing;

            // Gather the refs for our children. We can use `take_each_child_locked` since we will
            // be recording and keeping alive the RefPtrs in the callback in the *_to_kill lists.
            // SAFETY: `guard` holds `lock`, from which `jobs` and `procs` are projected.
            let result = unsafe {
                Self::take_each_child_locked(guard.jobs(), |job| {
                    jobs_to_kill.push_front(job);
                    Ok(())
                })
            };
            debug_assert!(result.is_ok());
            // SAFETY: As above.
            let result = unsafe {
                Self::take_each_child_locked(guard.procs(), |proc| {
                    procs_to_kill.push_front(proc);
                    Ok(())
                })
            };
            debug_assert!(result.is_ok());

            should_die = self.is_ready_for_dead_transition_locked(&guard.as_token_guard());
        }

        if should_die {
            self.finish_dead_transition_unlocked();
        }
        true
    }

    /// Called instead of `kill(ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL)`
    /// for the case of critical-to-job process death.
    #[cold]
    pub fn critical_process_kill(&self, dead_process: &ProcessDispatcher) {
        let mut proc_name = [0u8; ZX_MAX_NAME_LEN];
        let status = dead_process.get_name(&mut proc_name);
        debug_assert!(status.is_ok());

        let mut job_name = [0u8; ZX_MAX_NAME_LEN];
        let status = self.get_name(&mut job_name);
        debug_assert!(status.is_ok());

        kprint::kprintln!(
            "critical-process: process '{:s}' ({:u}) died, killing job '{:s}' ({:u})",
            &proc_name,
            dead_process.get_koid(),
            &job_name,
            self.get_koid()
        );

        if core::ptr::eq(self, RefPtr::as_ptr(get_root_job_dispatcher())) {
            RootJobObserver::critical_process_kill(dead_process);
        }

        self.kill(ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL);
    }

    /// Returns whether the job that `guard` locks may have its policy changed.
    fn can_set_policy_locked(guard: &JobDispatcherStateLockTokenGuard<'_, '_>) -> bool {
        // Can't set policy when there are active processes or jobs. This constraint ensures that
        // a process's policy cannot change over its lifetime.  Because a process's policy cannot
        // change, the risk of TOCTOU bugs is reduced and we are free to apply policy at the
        // ProcessDispatcher without having to walk up the tree to its containing job.
        guard.procs().is_empty() && guard.jobs().is_empty()
    }

    /// Set basic policy. `mode` is either ZX_JOB_POL_RELATIVE or ZX_JOB_POL_ABSOLUTE and
    /// `in_policy` is an array of policies.
    ///
    /// It is an error to set policy on a non-empty job, i.e. a job with one or more sub-jobs or
    /// processes.
    ///
    /// V1 flavor (on its way out)
    #[inline(never)]
    pub fn set_basic_policy_v1(
        &self,
        mode: u32,
        in_policy: &[zx_policy_basic_v1_t],
    ) -> Result<(), Status> {
        let policy_init =
            InlineArray::<zx_policy_basic_v2_t, POLICY_BASIC_INLINE_COUNT>::init_from_slice_with(
                in_policy,
                |input| zx_policy_basic_v2_t {
                    condition: input.condition,
                    action: input.policy,
                    flags: ZX_POL_OVERRIDE_DENY,
                },
            );
        stack_try_pin_init!(let policy = policy_init);
        let policy = policy.map_err(|_| Status::NO_MEMORY)?.into_ref();

        self.set_basic_policy_v2(mode, &policy)
    }

    /// Set basic policy; see [`Self::set_basic_policy_v1`].
    ///
    /// V2 flavor (on its way in)
    pub fn set_basic_policy_v2(
        &self,
        mode: u32,
        in_policy: &[zx_policy_basic_v2_t],
    ) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.state().lock_lock());

        if !Self::can_set_policy_locked(&guard.as_token_guard()) {
            return Err(Status::BAD_STATE);
        }
        guard.as_mut().policy_mut().add_basic_policy(mode, in_policy)
    }

    /// Set timer slack policy.
    ///
    /// `policy.min_slack` must be >= 0.
    ///
    /// `policy.default_mode` must be one of ZX_TIMER_SLACK_CENTER, ZX_TIMER_SLACK_EARLY,
    /// ZX_TIMER_SLACK_LATE.
    ///
    /// It is an error to set policy on a non-empty job, i.e. a job with one or more sub-jobs or
    /// processes.
    pub fn set_timer_slack_policy(&self, policy: &zx_policy_timer_slack_t) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.state().lock_lock());

        if !Self::can_set_policy_locked(&guard.as_token_guard()) {
            return Err(Status::BAD_STATE);
        }

        // Is the policy valid?
        if policy.min_slack < 0 {
            return Err(Status::INVALID_ARGS);
        }
        let new_mode = match policy.default_mode {
            ZX_TIMER_SLACK_CENTER => SlackMode::Center,
            ZX_TIMER_SLACK_EARLY => SlackMode::Early,
            ZX_TIMER_SLACK_LATE => SlackMode::Late,
            _ => return Err(Status::INVALID_ARGS),
        };

        let old_slack = guard.policy().get_timer_slack();
        let new_amount = old_slack.amount().0.max(policy.min_slack);
        let new_slack = TimerSlack::new(DurationUnknown(new_amount), new_mode);

        guard.as_mut().policy_mut().set_timer_slack(new_slack);

        Ok(())
    }

    /// Enumerates the direct (non recursive) children and invokes `je` methods on
    /// each node. The `je` methods are invoked without the lock of this
    /// JobDispatcher held, however a consistent atomic snapshot of children will
    /// be given. Returns false if any methods of `je` return false; returns true
    /// otherwise.
    pub fn enumerate_children(&self, je: &mut dyn JobEnumerator) -> bool {
        self.state().canary.assert();

        // Snapshot references are returned out of the lock block so they drop after `guard`.
        let (proc_refs, jobs_refs, result) = {
            ksync::lock!(let guard = self.state().lock_lock());

            // SAFETY: `guard` holds `lock`, from which `procs` is projected.
            let (proc_refs, status) =
                unsafe { Self::for_each_child_in_locked(guard.procs(), |_proc| Ok(())) };
            if status.is_ok() {
                // SAFETY: `guard` holds `lock`, from which `jobs` is projected.
                let (jobs_refs, status) =
                    unsafe { Self::for_each_child_in_locked(guard.jobs(), |_job| Ok(())) };
                (proc_refs, jobs_refs, status)
            } else {
                (proc_refs, LiveRefsArray::new(), status)
            }
        };

        // With the processes and jobs collected into their respective LiveRefsArrays, we can now
        // invoke the JobEnumerator callbacks on them. We perform this here, outside the lock,
        // instead of directly in the for_each_child_in_locked callbacks so that the callbacks are
        // permitted to perform user copies, or generally do actions that are not permitted whilst
        // locks are held.
        proc_refs.iter().all(|process| je.on_process(process))
            && jobs_refs.iter().all(|job| je.on_job(job))
            && result.is_ok()
    }

    /// Recursively walks the job/process tree and invokes `je` methods on each
    /// node. The `je` methods are invoked with all parent `JobDispatcher` object
    /// locks held. Returns false if any methods of `je` return false; returns true
    /// otherwise.
    ///
    /// Must not be called with this job's lock held.
    pub fn enumerate_children_recursive(&self, je: &mut dyn JobEnumerator) -> bool {
        self.state().canary.assert();

        // Snapshot references are returned out of the lock block so they drop after `guard`.
        let (proc_refs, jobs_refs, result) = {
            // As enumerate_children_recursive will recurse we need to give a lock order to the
            // guard.
            ksync::lock!(
                let guard = self.state().lock_lock_policy_with::<CriticalMutexOrderedPolicy>(
                    self.lock_order()
                )
            );

            // SAFETY: `guard` holds `lock`, from which `procs` is projected.
            let (proc_refs, status) = unsafe {
                Self::for_each_child_in_locked(guard.procs(), |proc| {
                    if je.on_process(proc) { Ok(()) } else { Err(Status::STOP) }
                })
            };
            if status.is_ok() {
                // SAFETY: `guard` holds `lock`, from which `jobs` is projected.
                let (jobs_refs, status) = unsafe {
                    Self::for_each_child_in_locked(guard.jobs(), |job| {
                        if !je.on_job(job) {
                            return Err(Status::STOP);
                        }
                        // TODO(kulakowski): This recursive call can overflow the stack.
                        if job.enumerate_children_recursive(je) {
                            Ok(())
                        } else {
                            Err(Status::STOP)
                        }
                    })
                };
                (proc_refs, jobs_refs, status)
            } else {
                (proc_refs, LiveRefsArray::new(), status)
            }
        };

        drop((proc_refs, jobs_refs));
        result.is_ok()
    }

    /// Looks up a direct child process of this job given its koid.
    pub fn lookup_process_by_id(&self, koid: zx_koid_t) -> Option<RefPtr<ProcessDispatcher>> {
        self.state().canary.assert();

        // Declared before the lock scope so that the references are released after the guard.
        let proc_refs: LiveRefsArray<ProcessDispatcher>;

        let mut found_proc = None;
        {
            ksync::lock!(let guard = self.state().lock_lock());

            // SAFETY: `guard` holds `lock`, from which `procs` is projected.
            let (refs, _result) = unsafe {
                Self::for_each_child_in_locked(guard.procs(), |proc| {
                    if proc.get_koid() == koid {
                        found_proc = Some(proc.clone());
                        return Err(Status::STOP);
                    }
                    Ok(())
                })
            };
            proc_refs = refs;
        }
        drop(proc_refs);
        found_proc // None if not found.
    }

    /// Looks up a direct child job of this job given its koid.
    pub fn lookup_job_by_id(&self, koid: zx_koid_t) -> Option<RefPtr<JobDispatcher>> {
        self.state().canary.assert();

        // Declared before the lock scope so that the references are released after the guard.
        let jobs_refs: LiveRefsArray<JobDispatcher>;

        let mut found_job = None;
        {
            ksync::lock!(let guard = self.state().lock_lock());

            // SAFETY: `guard` holds `lock`, from which `jobs` is projected.
            let (refs, _result) = unsafe {
                Self::for_each_child_in_locked(guard.jobs(), |job| {
                    if job.get_koid() == koid {
                        found_job = Some(job.clone());
                        return Err(Status::STOP);
                    }
                    Ok(())
                })
            };
            jobs_refs = refs;
        }
        drop(jobs_refs);
        found_job // None if not found.
    }

    /// Copies the job name into `out_name`.
    pub fn get_name(&self, out_name: &mut [u8; ZX_MAX_NAME_LEN]) -> Result<(), Status> {
        self.state().canary.assert();

        self.state().name.get(out_name);
        Ok(())
    }

    /// Sets the job name.
    pub fn set_name(&self, name: &[u8]) -> Result<(), Status> {
        self.state().canary.assert();

        self.state().name.set(name);
        Ok(())
    }

    /// Returns the job's normal `Exceptionate`.
    ///
    /// The exceptionate is only used by the threads under the current job, which keep the job
    /// alive, so handing out a reference bounded by `self` is sufficient.
    #[inline]
    pub fn exceptionate(&self) -> &Exceptionate {
        self.state().canary.assert();
        &self.state().exceptionate
    }

    /// Prunes the current [`Exceptionate`]s in `debug_exceptionates` of any invalid channels,
    /// moving removed entries into `pruned` so the caller can drop them after releasing the job
    /// lock. Returns the new count of `debug_exceptionates` after all removals.
    fn prune_debug_exceptionates_locked(
        debug_exceptionates: Pin<&mut DoublyLinkedList<RefPtr<DebugExceptionate>>>,
        mut pruned: Pin<&mut DoublyLinkedList<RefPtr<DebugExceptionate>>>,
    ) -> usize {
        let mut count = 0;
        // Remove disconnected exceptionates and get the count.
        let mut cursor = debug_exceptionates.cursor_front_mut();
        while let Some(debug_exceptionate) = cursor.get() {
            if !debug_exceptionate.has_valid_channel() {
                if let Some(removed) = cursor.erase() {
                    pruned.as_mut().push_back(removed);
                }
            } else {
                cursor.move_next();
                count += 1;
            }
        }
        count
    }

    /// Enumerates over a snapshot of debug exceptionates. `func` will be called without the lock
    /// held.
    #[inline]
    pub fn for_each_debug_exceptionate(
        &self,
        mut func: impl FnMut(&Exceptionate),
    ) -> Result<(), Status> {
        self.for_each_debug_exceptionate_dyn(&mut func)
    }

    /// Type-erased implementation of [`Self::for_each_debug_exceptionate`].
    fn for_each_debug_exceptionate_dyn(
        &self,
        func: &mut dyn FnMut(&Exceptionate),
    ) -> Result<(), Status> {
        let mut snapshot =
            InlineArray::<RefPtr<DebugExceptionate>, DEBUG_EXCEPTIONATE_INLINE_COUNT>::new();
        let res = {
            // MUST remain declared before `guard` so any disconnected `DebugExceptionate`s unlinked
            // during pruning are dropped only after `guard` is released.
            stack_pin_init!(let pruned = DoublyLinkedList::new());
            {
                ksync::lock!(let mut guard = self.state().lock_lock());
                let count = Self::prune_debug_exceptionates_locked(
                    guard.as_mut().debug_exceptionates_mut(),
                    pruned.as_mut(),
                );
                if count == 0 {
                    Ok(())
                } else {
                    // Perform the snapshot.
                    let mut live = guard.debug_exceptionates().forward_iter();
                    match InlineArray::try_new_with(count, || {
                        let next = live.next();
                        debug_assert!(next.is_some());
                        // SAFETY: `count` is the exact number of entries remaining in
                        // `debug_exceptionates` after pruning above while `guard` stayed
                        // continuously held, so `try_new_with(count, ...)` invokes this closure at
                        // most `count` times and `live.next()` yields `Some` on every call.
                        RefPtr::from_ref(unsafe { next.unwrap_unchecked() })
                    }) {
                        Ok(items) => {
                            // SAFETY: `snapshot` is still the empty `InlineArray::new()` (`count ==
                            // 0`, inline storage), whose drop is a no-op; `ptr::write` avoids
                            // emitting dead `InlineArray::drop` glue on this assignment.
                            unsafe { core::ptr::write(&mut snapshot, items) };
                            Ok(())
                        }
                        Err(_) => Err(Status::NO_MEMORY),
                    }
                }
            }
        };

        for exceptionate in snapshot.iter() {
            func(exceptionate);
        }
        drop(snapshot);
        res
    }

    /// Create a new debug exceptionate. Possible errors are ZX_ERR_NO_MEMORY and
    /// ZX_ERR_ALREADY_BOUND. Instead of returning an exceptionate, this function calls
    /// `set_channel` directly because it needs to be protected by our lock.
    pub fn create_debug_exceptionate(
        &self,
        channel_handle: KernelHandle<ChannelDispatcher>,
        thread_rights: zx_rights_t,
        process_rights: zx_rights_t,
    ) -> Result<(), Status> {
        // MUST remain declared before the lock block (and `unconsumed_channel` /
        // `failed_exceptionate` are returned out of the block) so no `DebugExceptionate` or
        // `ChannelDispatcher` handle is dropped while holding `self.state().lock`.
        stack_pin_init!(let pruned = DoublyLinkedList::new());
        let (res, unconsumed_channel, failed_exceptionate) = {
            ksync::lock!(let mut guard = self.state().lock_lock());
            // Remove disconnected exceptionates.
            let count = Self::prune_debug_exceptionates_locked(
                guard.as_mut().debug_exceptionates_mut(),
                pruned.as_mut(),
            );
            if count >= ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT as usize {
                (Err(Status::ALREADY_BOUND), Some(channel_handle), None)
            } else {
                match DebugExceptionate::try_create(ZX_EXCEPTION_CHANNEL_TYPE_JOB_DEBUGGER) {
                    Err(_) => (Err(Status::NO_MEMORY), Some(channel_handle), None),
                    Ok(exceptionate) => {
                        match exceptionate.set_channel(
                            channel_handle,
                            thread_rights,
                            process_rights,
                        ) {
                            Err(status) => (Err(status), None, Some(exceptionate)),
                            Ok(()) => {
                                guard.as_mut().debug_exceptionates_mut().push_back(exceptionate);
                                (Ok(()), None, None)
                            }
                        }
                    }
                }
            }
        };
        drop((failed_exceptionate, unconsumed_channel));
        res
    }

    /// Sets whether this job should be killed during out-of-memory events.
    pub fn set_kill_on_oom(&self, value: bool) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        *guard.as_mut().kill_on_oom_mut() = value;
    }

    /// Returns information about the job.
    pub fn get_info(&self) -> zx_info_job_t {
        self.state().canary.assert();

        ksync::lock!(let guard = self.state().lock_lock());
        zx_info_job_t {
            return_code: *guard.return_code(),
            exited: u8::from(*guard.state() == State::Dead || *guard.state() == State::Killing),
            kill_on_oom: u8::from(*guard.kill_on_oom()),
            debugger_attached: u8::from(!guard.debug_exceptionates().is_empty()),
            padding1: Default::default(),
        }
    }

    /// Aggregate the runtime for all processes that were previously running or are currently
    /// running as children of this job.
    ///
    /// This includes runtime for threads that previously ran under those processes, but it does
    /// not include runtime for child jobs.
    fn get_task_runtime_stats(&self) -> TaskRuntimeStats {
        self.state().canary.assert();
        ksync::lock!(let guard = self.state().lock_lock());
        // Sample the exited process runtime stats inside the lock to ensure consistency with
        // destructing process dispatchers that are still in the process list.
        let mut stats = *guard.exited_process_runtime_stats();
        for proc in guard.procs().forward_iter() {
            stats += proc.get_task_runtime_stats();
        }
        stats
    }

    /// Returns `self.get_task_runtime_stats()` converted to `zx_info_task_runtime_t`.
    pub fn get_runtime_stats(&self) -> zx_info_task_runtime_t {
        self.get_task_runtime_stats().into()
    }
}

/// JobDispatcher unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "job_dispatcher_tests")]
mod job_dispatcher_tests {
    use super::super::channel_dispatcher::ChannelDispatcher;
    use super::super::handle::KernelHandle;
    use super::{
        DEBUG_EXCEPTIONATE_INLINE_COUNT, JobDispatcher, JobEnumerator, OOM_BIT_JOB_ARRAY_SIZE,
        OomBitJobArray, POLICY_BASIC_INLINE_COUNT, ROOT_JOB_MAX_HEIGHT, ZX_JOB_NO_CHILDREN,
        stable_sort_by,
    };
    use crate::arch_rs::UserEntryState;
    use crate::kernel::deadline::SlackMode;
    use crate::kernel::task_runtime_stats::TaskRuntimeStats;
    use crate::object::{
        Dispatcher, ProcessDispatcher, ProcessState, ThreadDispatcher, get_root_job_dispatcher,
    };
    use fbl::RefPtr;
    use unittest::{
        assert_false, assert_gt, assert_true, expect_eq, expect_err, expect_false, expect_ok,
        expect_true, unwrap_ok,
    };
    use zx_status::Status;
    use zx_types::{
        ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT, ZX_JOB_NO_JOBS, ZX_JOB_NO_PROCESSES,
        ZX_JOB_POL_ABSOLUTE, ZX_JOB_POL_RELATIVE, ZX_JOB_TERMINATED, ZX_KOID_INVALID,
        ZX_POL_ACTION_ALLOW, ZX_POL_ACTION_DENY, ZX_POL_NEW_CHANNEL, ZX_POL_NEW_EVENT,
        ZX_POL_OVERRIDE_ALLOW, ZX_POL_OVERRIDE_DENY, ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL,
        ZX_TASK_RETCODE_OOM_KILL, ZX_TASK_TERMINATED, ZX_THREAD_SUSPENDED, ZX_TIMER_SLACK_CENTER,
        ZX_TIMER_SLACK_EARLY, ZX_TIMER_SLACK_LATE, zx_info_task_runtime_t, zx_koid_t,
        zx_policy_basic_v1_t, zx_policy_basic_v2_t, zx_policy_timer_slack_t, zx_rights_t,
        zx_signals_t,
    };

    /// Upper bound on the number of tree nodes `test_job_enumerator` records. Arbitrary.
    const INDEX_LIMIT: usize = 20;

    /// Creates a job under a private root job so that nothing here is visible from the real job
    /// tree.
    fn make_job() -> RefPtr<JobDispatcher> {
        let root_job = JobDispatcher::create_root_job();
        unwrap_child(JobDispatcher::create(0, root_job))
    }

    /// Creates a child of `parent`.
    fn make_child(parent: &RefPtr<JobDispatcher>) -> RefPtr<JobDispatcher> {
        unwrap_child(JobDispatcher::create(0, parent.clone()))
    }

    /// Unwraps the result of [`JobDispatcher::create`] into the bare job, dropping the rights.
    fn unwrap_child(
        created: Result<(KernelHandle<JobDispatcher>, zx_rights_t), Status>,
    ) -> RefPtr<JobDispatcher> {
        created.expect("job created").0.release()
    }

    /// Sleeps briefly until all `signals` are asserted on `dispatcher`.
    fn wait_for_signals(dispatcher: &Dispatcher, signals: zx_signals_t) {
        while (dispatcher.poll_signals() & signals) != signals {
            let _ = crate::kernel::thread::sleep_relative(
                crate::platform_rs::timer::DurationMono::from_millis(1),
            );
        }
    }

    /// Creates, pre-suspends, and starts an initial thread in `process`, waiting until it has
    /// parked in `THREAD_SUSPENDED` so its runtime stats are stationary.
    fn start_suspended_thread(
        process: &RefPtr<ProcessDispatcher>,
        name: &[u8],
    ) -> KernelHandle<ThreadDispatcher> {
        let (thread_handle, _rights) =
            ThreadDispatcher::create(process.clone(), 0, name).expect("thread created");
        let thread = thread_handle.dispatcher();
        // SAFETY: `thread` was just created above and has not started running, and no chain lock
        // transaction is active on this test thread.
        unsafe { thread.initialize() }.expect("thread initialized");
        thread.suspend().expect("thread suspended");
        let entry = UserEntryState { pc: 0, sp: 0, arg1: 0, arg2: 0, tp: 0, abi_reg: 0 };
        thread.start(entry, true).expect("thread started");
        wait_for_signals(thread, ZX_THREAD_SUSPENDED);
        while {
            let before = process.get_task_runtime_stats();
            let _ = crate::kernel::thread::sleep_relative(
                crate::platform_rs::timer::DurationMono::from_millis(1),
            );
            before != process.get_task_runtime_stats()
        } {}
        thread_handle
    }

    /// One visited task and the job it hangs off.
    #[derive(Clone, Copy, Default)]
    struct Entry {
        /// The task's koid.
        koid: zx_koid_t,
        /// The koid of the task's parent job (`ZX_KOID_INVALID` for the root entry).
        parent_koid: zx_koid_t,
    }

    /// Records the first `INDEX_LIMIT` tasks of a recursive walk and any contract violations.
    struct TestJobEnumerator {
        /// A task without a parent was visited.
        badness: bool,
        /// A callback ran after one of them returned `false`.
        called_after_stop: bool,
        /// Number of populated `entries`.
        index: usize,
        /// Visited tasks, parents before children.
        entries: [Entry; INDEX_LIMIT],
    }

    impl TestJobEnumerator {
        /// Creates an empty `TestJobEnumerator`.
        fn new() -> Self {
            Self {
                badness: false,
                called_after_stop: false,
                index: 0,
                entries: [Entry::default(); INDEX_LIMIT],
            }
        }

        /// Records a task; returns `false` once the table is full to stop the walk.
        fn add_entry(&mut self, koid: zx_koid_t, parent_koid: zx_koid_t) -> bool {
            if self.index >= INDEX_LIMIT {
                // When index >= INDEX_LIMIT a value of false is returned (below) which means that
                // no further on_job or on_process calls should be made. If this is called after
                // that state, then the enumerate_children_recursive code didn't honor the contract
                // about halting on a `return false`.
                self.called_after_stop = true;
                return false;
            }
            self.entries[self.index] = Entry { koid, parent_koid };
            self.index += 1;
            self.index < INDEX_LIMIT
        }
    }

    impl JobEnumerator for TestJobEnumerator {
        fn on_job(&mut self, job: &JobDispatcher) -> bool {
            let Some(parent) = job.parent() else {
                self.badness = true;
                return false;
            };
            self.add_entry(job.get_koid(), parent.get_koid())
        }

        fn on_process(&mut self, proc: &ProcessDispatcher) -> bool {
            self.add_entry(proc.get_koid(), proc.job().get_koid())
        }
    }

    /// JobDispatcherJobEnumerator
    #[test]
    fn test_job_enumerator() {
        let mut job_enumerator = TestJobEnumerator::new();
        let root_job = get_root_job_dispatcher();
        // Enumerating the children will not add the root job itself. Add it explicitly.
        job_enumerator.add_entry(root_job.get_koid(), ZX_KOID_INVALID);
        root_job.enumerate_children_recursive(&mut job_enumerator);

        assert_false!(job_enumerator.badness, "A pointer was unexpectedly null.");
        assert_false!(job_enumerator.called_after_stop, "Return false didn't halt Enumeration.");

        // There should be at least one job.
        assert_gt!(job_enumerator.index, 0usize, "At least one job");

        // Check that all nodes have a path to the root node.
        for i in 1..job_enumerator.index {
            let mut found_root = false;
            let mut current = job_enumerator.entries[i].parent_koid;
            // All the parents are expected to be in the list prior to the child.
            for k in (1..=i).rev() {
                if current == job_enumerator.entries[k - 1].koid {
                    if k == 1 && current == job_enumerator.entries[0].koid {
                        // The last step should always be at index 0 (the root).
                        found_root = true;
                    }
                    current = job_enumerator.entries[k - 1].parent_koid;
                }
            }
            assert_true!(found_root, "Find root");
        }
    }

    /// JobNoChildrenSignal
    #[test]
    fn test_job_no_children_signal() {
        // Create a new job.
        let (root, _rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()));

        // Ensure all three NO_{JOBS,PROCESSES,CHILDREN} signals are active.
        expect_eq!(
            root.dispatcher().poll_signals(),
            ZX_JOB_NO_PROCESSES | ZX_JOB_NO_JOBS | ZX_JOB_NO_CHILDREN
        );

        // Create a child job.
        let (child_job, _rights) = unwrap_ok!(JobDispatcher::create(0, root.dispatcher().clone()));

        // Ensure the NO_CHILDREN and NO_JOBS signals have cleared.
        expect_eq!(root.dispatcher().poll_signals(), ZX_JOB_NO_PROCESSES);

        // Create a child process.
        let (child_process, _process_rights, _vmar, _vmar_rights) =
            unwrap_ok!(ProcessDispatcher::create(root.dispatcher().clone(), b"test-process", 0));

        // Ensure the NO_PROCESSES signal has cleared.
        expect_eq!(root.dispatcher().poll_signals(), 0u32);

        // Kill the child job. Ensure NO_JOBS is active again.
        child_job.dispatcher().kill(0);
        expect_eq!(root.dispatcher().poll_signals(), ZX_JOB_NO_JOBS);

        // Kill the child process. Ensure all three signals are active again.
        child_process.dispatcher().kill(0);
        expect_eq!(
            root.dispatcher().poll_signals(),
            ZX_JOB_NO_PROCESSES | ZX_JOB_NO_JOBS | ZX_JOB_NO_CHILDREN
        );

        root.dispatcher().kill(0);
    }

    /// GetPolicyDuringEnumeration
    #[test]
    fn test_get_policy_during_enumeration() {
        let (parent_job, _rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()));
        let (child_job, _rights) =
            unwrap_ok!(JobDispatcher::create(0, parent_job.dispatcher().clone()));

        let policy = [zx_policy_basic_v2_t {
            condition: ZX_POL_NEW_EVENT,
            action: ZX_POL_ACTION_DENY,
            flags: ZX_POL_OVERRIDE_DENY,
        }];
        expect_ok!(child_job.dispatcher().set_basic_policy_v2(ZX_JOB_POL_RELATIVE, &policy));

        /// Queries the `ZX_POL_NEW_EVENT` policy of the job with `target_koid` while the walk
        /// holds its ancestors' locks.
        struct PolicyJobEnumerator {
            /// The job whose policy is queried.
            target_koid: zx_koid_t,
            /// Whether the target job was visited.
            visited: bool,
            /// The `ZX_POL_NEW_EVENT` action observed on the target job.
            action: u32,
        }

        impl JobEnumerator for PolicyJobEnumerator {
            fn on_job(&mut self, job: &JobDispatcher) -> bool {
                if job.get_koid() == self.target_koid {
                    self.visited = true;
                    self.action = job.get_policy().query_basic_policy(ZX_POL_NEW_EVENT);
                }
                true
            }
        }

        let mut enumerator = PolicyJobEnumerator {
            target_koid: child_job.dispatcher().get_koid(),
            visited: false,
            action: ZX_POL_ACTION_ALLOW,
        };

        expect_true!(parent_job.dispatcher().enumerate_children_recursive(&mut enumerator));
        expect_true!(enumerator.visited);
        expect_eq!(enumerator.action, ZX_POL_ACTION_DENY);

        parent_job.dispatcher().kill(0);
    }

    /// The job tree depth is bounded by the root job's max height.
    #[test]
    fn test_create_depth_limit() {
        // Keep every level in `chain` to verify `max_height` at each depth down to the leaf.
        let mut chain = [const { None }; ROOT_JOB_MAX_HEIGHT as usize + 1];
        let root = JobDispatcher::create_root_job();
        expect_eq!(root.max_height(), ROOT_JOB_MAX_HEIGHT);
        chain[0] = Some(root);
        for level in 1..chain.len() {
            let job = make_child(chain[level - 1].as_ref().unwrap());
            expect_eq!(job.max_height(), ROOT_JOB_MAX_HEIGHT - level as u32);
            chain[level] = Some(job);
        }
        // The leaf-most job cannot have children.
        let leaf = chain[ROOT_JOB_MAX_HEIGHT as usize].as_ref().unwrap();
        expect_eq!(leaf.max_height(), 0);
        expect_err!(JobDispatcher::create(0, leaf.clone()), Status::OUT_OF_RANGE);
        expect_eq!(leaf.get_related_koid(), leaf.parent().unwrap().get_koid());

        for job in chain.iter_mut().rev() {
            *job = None;
        }
    }

    /// kill() and critical_process_kill() report through get_info() and reject new children.
    #[test]
    fn test_kill_reports_and_rejects_children() {
        let root = make_job();
        let child = make_child(&root);
        expect_eq!(root.get_info().exited, 0);

        expect_true!(root.kill(42));
        let info = root.get_info();
        expect_eq!(info.exited, 1);
        expect_eq!(info.return_code, 42);
        expect_eq!(child.get_info().exited, 1);
        expect_eq!(child.get_info().return_code, 42);
        // A second kill reports failure and keeps the original return code.
        expect_false!(root.kill(7));
        expect_eq!(root.get_info().return_code, 42);

        // A dead job accepts neither child jobs nor child processes.
        expect_err!(JobDispatcher::create(0, root.clone()), Status::BAD_STATE);
        expect_err!(ProcessDispatcher::create(root.clone(), b"late", 0), Status::BAD_STATE);

        // Killing a parent job while a direct child process and a nested child job's started
        // process are still alive stages both processes through `procs_to_kill`
        // (`ProcessDispatcherJobListTag`) and cascades `finish_dead_transition_unlocked()` from
        // `tree_child` up to `tree_parent`.
        let tree_parent = make_job();
        let tree_child = make_child(&tree_parent);
        let (direct_proc, _rights, _vmar, _vmar_rights) = unwrap_ok!(
            ProcessDispatcher::create(tree_parent.clone(), b"kill-direct", 0),
            "created"
        );
        let (child_proc, _rights, _vmar, _vmar_rights) =
            unwrap_ok!(ProcessDispatcher::create(tree_child.clone(), b"kill-nested", 0), "created");
        let _child_thread = start_suspended_thread(child_proc.dispatcher(), b"kill-nested-t");

        expect_true!(tree_parent.kill(99));
        wait_for_signals(&tree_parent, ZX_JOB_TERMINATED);
        expect_eq!(tree_child.poll_signals() & ZX_JOB_TERMINATED, ZX_JOB_TERMINATED);
        expect_eq!(
            direct_proc.dispatcher().poll_signals() & ZX_TASK_TERMINATED,
            ZX_TASK_TERMINATED
        );
        expect_eq!(child_proc.dispatcher().poll_signals() & ZX_TASK_TERMINATED, ZX_TASK_TERMINATED);
        expect_true!(direct_proc.dispatcher().process_state() == ProcessState::Dead);
        expect_eq!(direct_proc.dispatcher().get_info().return_code, 99);
        expect_true!(child_proc.dispatcher().process_state() == ProcessState::Dead);
        expect_eq!(child_proc.dispatcher().get_info().return_code, 99);
        expect_eq!(tree_child.get_info().exited, 1);
        expect_eq!(tree_child.get_info().return_code, 99);
        expect_eq!(tree_parent.get_info().exited, 1);
        expect_eq!(tree_parent.get_info().return_code, 99);

        // A critical process configured with `retcode_nonzero = true` leaves its job alive on a
        // zero exit code, and kills a non-root job with ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL on a
        // non-zero exit code.
        let crit_job = make_job();
        let (ok_proc, _rights, _vmar, _vmar_rights) =
            unwrap_ok!(ProcessDispatcher::create(crit_job.clone(), b"crit-ok", 0), "created");
        expect_ok!(ok_proc.dispatcher().set_critical_to_job(crit_job.clone(), true));
        ok_proc.dispatcher().kill(0);
        expect_eq!(crit_job.get_info().exited, 0);

        let (fail_proc, _rights, _vmar, _vmar_rights) =
            unwrap_ok!(ProcessDispatcher::create(crit_job.clone(), b"crit-fail", 0), "created");
        expect_ok!(fail_proc.dispatcher().set_critical_to_job(crit_job.clone(), true));
        fail_proc.dispatcher().kill(1);
        let crit_info = crit_job.get_info();
        expect_eq!(crit_info.exited, 1);
        expect_eq!(crit_info.return_code, ZX_TASK_RETCODE_CRITICAL_PROCESS_KILL);
    }

    /// Policies can only be set on childless jobs and validate their arguments.
    #[test]
    fn test_set_policy() {
        let job = make_job();
        let slack = zx_policy_timer_slack_t {
            min_slack: 1000,
            default_mode: ZX_TIMER_SLACK_LATE,
            _padding: [0; 4],
        };
        expect_err!(
            job.set_timer_slack_policy(&zx_policy_timer_slack_t { min_slack: -1, ..slack }),
            Status::INVALID_ARGS
        );
        expect_err!(
            job.set_timer_slack_policy(&zx_policy_timer_slack_t { default_mode: 7, ..slack }),
            Status::INVALID_ARGS
        );
        expect_ok!(job.set_timer_slack_policy(&slack));
        expect_eq!(job.get_policy().get_timer_slack().amount().0, 1000);
        expect_true!(job.get_policy().get_timer_slack().mode() == SlackMode::Late);
        // A smaller minimum does not shrink the inherited slack, but the mode is replaced.
        expect_ok!(job.set_timer_slack_policy(&zx_policy_timer_slack_t {
            min_slack: 10,
            default_mode: ZX_TIMER_SLACK_CENTER,
            _padding: [0; 4],
        }));
        expect_eq!(job.get_policy().get_timer_slack().amount().0, 1000);
        expect_true!(job.get_policy().get_timer_slack().mode() == SlackMode::Center);
        // A larger minimum raises the slack and switches to ZX_TIMER_SLACK_EARLY.
        expect_ok!(job.set_timer_slack_policy(&zx_policy_timer_slack_t {
            min_slack: 2000,
            default_mode: ZX_TIMER_SLACK_EARLY,
            _padding: [0; 4],
        }));
        expect_eq!(job.get_policy().get_timer_slack().amount().0, 2000);
        expect_true!(job.get_policy().get_timer_slack().mode() == SlackMode::Early);

        let basic_v1 =
            [zx_policy_basic_v1_t { condition: ZX_POL_NEW_EVENT, policy: ZX_POL_ACTION_DENY };
                POLICY_BASIC_INLINE_COUNT + 1];
        expect_ok!(job.set_basic_policy_v1(ZX_JOB_POL_RELATIVE, &basic_v1[..1]));
        expect_ok!(job.set_basic_policy_v1(ZX_JOB_POL_RELATIVE, &basic_v1));
        expect_eq!(job.get_policy().query_basic_policy(ZX_POL_NEW_EVENT), ZX_POL_ACTION_DENY);

        let basic = [zx_policy_basic_v2_t {
            condition: ZX_POL_NEW_CHANNEL,
            action: ZX_POL_ACTION_DENY,
            flags: ZX_POL_OVERRIDE_DENY,
        }];
        expect_ok!(job.set_basic_policy_v2(ZX_JOB_POL_ABSOLUTE, &basic));
        expect_eq!(job.get_policy().query_basic_policy(ZX_POL_NEW_CHANNEL), ZX_POL_ACTION_DENY);

        // Once a child exists the policy is frozen.
        let child = make_child(&job);
        expect_err!(job.set_basic_policy_v2(ZX_JOB_POL_ABSOLUTE, &basic), Status::BAD_STATE);
        expect_err!(job.set_timer_slack_policy(&slack), Status::BAD_STATE);
        // The child inherited the parent's policy, and v1's ZX_POL_OVERRIDE_DENY prevents a child
        // from overriding ZX_POL_NEW_EVENT.
        expect_eq!(child.get_policy().get_timer_slack().amount().0, 2000);
        expect_true!(child.get_policy().get_timer_slack().mode() == SlackMode::Early);
        expect_eq!(child.get_policy().query_basic_policy(ZX_POL_NEW_CHANNEL), ZX_POL_ACTION_DENY);
        expect_eq!(child.get_policy().query_basic_policy(ZX_POL_NEW_EVENT), ZX_POL_ACTION_DENY);
        let override_attempt = [zx_policy_basic_v2_t {
            condition: ZX_POL_NEW_EVENT,
            action: 0,
            flags: ZX_POL_OVERRIDE_ALLOW,
        }];
        expect_err!(
            child.set_basic_policy_v2(ZX_JOB_POL_ABSOLUTE, &override_attempt),
            Status::ALREADY_EXISTS
        );
        job.kill(0);
    }

    /// Direct children can be found by koid; other koids are not.
    #[test]
    fn test_lookup_by_id() {
        let root = make_job();
        let child = make_child(&root);
        let grandchild = make_child(&child);
        let (process, _rights, _vmar, _vmar_rights) =
            unwrap_ok!(ProcessDispatcher::create(root.clone(), b"lookup", 0), "created");

        let found = root.lookup_job_by_id(child.get_koid());
        expect_true!(found.is_some_and(|found| RefPtr::ptr_eq(&found, &child)));
        expect_true!(root.lookup_job_by_id(grandchild.get_koid()).is_none());
        expect_true!(root.lookup_job_by_id(process.dispatcher().get_koid()).is_none());

        let found = root.lookup_process_by_id(process.dispatcher().get_koid());
        expect_true!(found.is_some_and(|found| RefPtr::ptr_eq(&found, process.dispatcher())));
        expect_true!(root.lookup_process_by_id(child.get_koid()).is_none());
        expect_true!(child.lookup_process_by_id(process.dispatcher().get_koid()).is_none());

        let proc_koid = process.dispatcher().get_koid();
        let child_koid = child.get_koid();
        process.dispatcher().kill(0);
        expect_true!(root.lookup_process_by_id(proc_koid).is_none());
        child.kill(0);
        expect_true!(root.lookup_job_by_id(child_koid).is_none());
        root.kill(0);
    }

    /// Debug exceptionates are bounded, counted while connected, and pruned once disconnected.
    #[test]
    fn test_debug_exceptionates() {
        let job = make_job();
        expect_eq!(job.get_info().debugger_attached, 0);

        let mut peers = [const { None }; ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT as usize];
        for (idx, peer) in peers.iter_mut().enumerate() {
            let (kernel_handle, user_handle, _rights) =
                unwrap_ok!(ChannelDispatcher::create(), "channel");
            expect_ok!(job.create_debug_exceptionate(kernel_handle, 0, 0));
            *peer = Some(user_handle);
            if idx + 1 == DEBUG_EXCEPTIONATE_INLINE_COUNT {
                let mut inline_count = 0;
                expect_ok!(job.for_each_debug_exceptionate(|exceptionate| {
                    if exceptionate.has_valid_channel() {
                        inline_count += 1;
                    }
                }));
                expect_eq!(inline_count, DEBUG_EXCEPTIONATE_INLINE_COUNT);
            }
        }
        expect_eq!(job.get_info().debugger_attached, 1);

        let mut count = 0;
        expect_ok!(job.for_each_debug_exceptionate(|exceptionate| {
            if exceptionate.has_valid_channel() {
                count += 1;
            }
        }));
        expect_eq!(count, ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT as usize);

        // The limit is enforced against live channels only.
        let (kernel_handle, user_handle, _rights) =
            unwrap_ok!(ChannelDispatcher::create(), "channel");
        expect_err!(job.create_debug_exceptionate(kernel_handle, 0, 0), Status::ALREADY_BOUND);
        drop(user_handle);

        // Closing a peer disconnects its exceptionate, which the next walk prunes.
        peers[0] = None;
        count = 0;
        expect_ok!(job.for_each_debug_exceptionate(|_exceptionate| count += 1));
        expect_eq!(count, ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT as usize - 1);
        let (kernel_handle, user_handle, _rights) =
            unwrap_ok!(ChannelDispatcher::create(), "channel");
        expect_ok!(job.create_debug_exceptionate(kernel_handle, 0, 0));
        drop(user_handle);

        // `for_each_debug_exceptionate` prunes the disconnected entry from above and invokes
        // callbacks without holding the job lock, so `get_info()` can acquire it cleanly inside
        // the callback.
        let mut visited = 0;
        let mut all_valid = true;
        let mut attached_in_callback = 0;
        expect_ok!(job.for_each_debug_exceptionate(|exceptionate| {
            all_valid &= exceptionate.has_valid_channel();
            if visited == 0 {
                attached_in_callback = job.get_info().debugger_attached;
            }
            visited += 1;
        }));
        expect_true!(all_valid);
        expect_eq!(attached_in_callback, 1);
        expect_eq!(visited, ZX_EXCEPTION_CHANNEL_JOB_DEBUGGER_MAX_COUNT as usize - 1);

        // With every peer closed, the next walk prunes all entries and clears `debugger_attached`.
        drop(peers);
        count = 0;
        expect_ok!(job.for_each_debug_exceptionate(|_exceptionate| count += 1));
        expect_eq!(count, 0);
        expect_eq!(job.get_info().debugger_attached, 0);
        job.kill(0);
    }

    /// Runtime stats sum active and exited direct child processes without double-counting.
    #[test]
    fn test_get_task_runtime_stats() {
        let root = make_job();
        let child_job = make_child(&root);
        expect_true!(root.get_task_runtime_stats() == TaskRuntimeStats::default());

        let (proc_a, _rights_a, _vmar_a, _vmar_rights_a) =
            unwrap_ok!(ProcessDispatcher::create(root.clone(), b"stats-a", 0), "created");
        let (proc_b, _rights_b, _vmar_b, _vmar_rights_b) =
            unwrap_ok!(ProcessDispatcher::create(root.clone(), b"stats-b", 0), "created");
        let (proc_c, _rights_c, _vmar_c, _vmar_rights_c) =
            unwrap_ok!(ProcessDispatcher::create(child_job.clone(), b"stats-c", 0), "created");

        let thread_a = start_suspended_thread(proc_a.dispatcher(), b"stats-a-t");
        let thread_b = start_suspended_thread(proc_b.dispatcher(), b"stats-b-t");
        let thread_c = start_suspended_thread(proc_c.dispatcher(), b"stats-c-t");

        thread_a.dispatcher().add_page_fault_ticks(1_100_000);
        thread_a.dispatcher().add_lock_contention_ticks(3_300_000);
        thread_b.dispatcher().add_page_fault_ticks(2_500_000);
        thread_b.dispatcher().add_lock_contention_ticks(7_700_000);
        thread_c.dispatcher().add_page_fault_ticks(5_000_000);
        thread_c.dispatcher().add_lock_contention_ticks(11_000_000);

        let stats_a = proc_a.dispatcher().get_task_runtime_stats();
        let stats_b = proc_b.dispatcher().get_task_runtime_stats();
        let stats_c = proc_c.dispatcher().get_task_runtime_stats();
        expect_eq!(stats_a.page_fault_ticks, 1_100_000);
        expect_true!(stats_a.lock_contention_ticks >= 3_300_000);
        expect_eq!(stats_b.page_fault_ticks, 2_500_000);
        expect_true!(stats_b.lock_contention_ticks >= 7_700_000);
        expect_eq!(stats_c.page_fault_ticks, 5_000_000);
        expect_true!(stats_c.lock_contention_ticks >= 11_000_000);
        expect_true!(child_job.get_task_runtime_stats() == stats_c);

        let mut expected_before = stats_a;
        expected_before += stats_b;
        let before_exit = root.get_task_runtime_stats();
        expect_true!(before_exit == expected_before);
        expect_eq!(before_exit.page_fault_ticks, 3_600_000);
        expect_true!(before_exit.lock_contention_ticks >= 11_000_000);

        // Killing `proc_a` unlinks it and rolls its runtime stats into the parent job; repeating
        // `remove_child_process` is a no-op and must not double-count, and `child_job`'s process
        // stats remain excluded.
        let proc_a_koid = proc_a.dispatcher().get_koid();
        proc_a.dispatcher().kill(0);
        wait_for_signals(proc_a.dispatcher(), ZX_TASK_TERMINATED);
        while root.lookup_process_by_id(proc_a_koid).is_some() {
            crate::kernel::thread::r#yield();
        }

        let mut expected_after = proc_a.dispatcher().get_task_runtime_stats();
        expected_after += stats_b;
        let after_a = root.get_task_runtime_stats();
        expect_true!(after_a == expected_after);
        expect_eq!(after_a.page_fault_ticks, before_exit.page_fault_ticks);
        expect_true!(after_a.lock_contention_ticks >= before_exit.lock_contention_ticks);
        expect_true!(after_a.cpu_ticks >= before_exit.cpu_ticks);
        expect_true!(after_a.queue_ticks >= before_exit.queue_ticks);

        // SAFETY: `root` is `proc_a`'s parent job.
        unsafe { root.remove_child_process(proc_a.dispatcher()) };
        expect_true!(root.get_task_runtime_stats() == after_a);

        let expected_info = zx_info_task_runtime_t::from(after_a);
        let actual_info = root.get_runtime_stats();
        expect_true!(actual_info == expected_info);
        expect_true!(actual_info.page_fault_time > 0);
        expect_true!(actual_info.lock_contention_time > 0);

        root.kill(0);
        wait_for_signals(proc_b.dispatcher(), ZX_TASK_TERMINATED);
        wait_for_signals(proc_c.dispatcher(), ZX_TASK_TERMINATED);
        wait_for_signals(&child_job, ZX_JOB_TERMINATED);
        wait_for_signals(&root, ZX_JOB_TERMINATED);
    }

    /// Recursive enumeration visits processes before child jobs and honors early-stop returns.
    #[test]
    fn test_enumerate_children_recursive() {
        /// Records visited koids in callback order and stops at the configured koid.
        struct EnumState {
            /// Visited job koids in callback order.
            jobs: [zx_koid_t; 8],
            /// Number of entries written to `jobs`.
            job_count: usize,
            /// Visited process koids in callback order.
            procs: [zx_koid_t; 8],
            /// Number of entries written to `procs`.
            proc_count: usize,
            /// Job koid at which `on_job` returns `false` (`ZX_KOID_INVALID` to never stop).
            stop_job_koid: zx_koid_t,
            /// Process koid at which `on_process` returns `false` (`ZX_KOID_INVALID` to never
            /// stop).
            stop_proc_koid: zx_koid_t,
            /// Set once a callback returns `false`.
            stopped: bool,
            /// Set if any callback runs after `stopped` became `true`.
            called_after_stop: bool,
        }

        impl EnumState {
            /// Creates a fresh `EnumState` configured with `stop_job_koid` and `stop_proc_koid`.
            fn new(stop_job_koid: zx_koid_t, stop_proc_koid: zx_koid_t) -> Self {
                Self {
                    jobs: [ZX_KOID_INVALID; 8],
                    job_count: 0,
                    procs: [ZX_KOID_INVALID; 8],
                    proc_count: 0,
                    stop_job_koid,
                    stop_proc_koid,
                    stopped: false,
                    called_after_stop: false,
                }
            }
        }

        impl JobEnumerator for EnumState {
            fn on_job(&mut self, job: &JobDispatcher) -> bool {
                if self.stopped {
                    self.called_after_stop = true;
                    return false;
                }
                self.jobs[self.job_count] = job.get_koid();
                self.job_count += 1;
                if job.get_koid() == self.stop_job_koid {
                    self.stopped = true;
                    return false;
                }
                true
            }

            fn on_process(&mut self, proc: &ProcessDispatcher) -> bool {
                if self.stopped {
                    self.called_after_stop = true;
                    return false;
                }
                self.procs[self.proc_count] = proc.get_koid();
                self.proc_count += 1;
                if proc.get_koid() == self.stop_proc_koid {
                    self.stopped = true;
                    return false;
                }
                true
            }
        }

        let root = make_job();
        let (root_proc1, _r1, _v1, _vr1) =
            unwrap_ok!(ProcessDispatcher::create(root.clone(), b"enum-p1", 0), "created");
        let (root_proc2, _r2, _v2, _vr2) =
            unwrap_ok!(ProcessDispatcher::create(root.clone(), b"enum-p2", 0), "created");
        let child_job1 = make_child(&root);
        let child_job2 = make_child(&root);
        let (child1_proc, _r3, _v3, _vr3) =
            unwrap_ok!(ProcessDispatcher::create(child_job1.clone(), b"enum-c1-p", 0), "created");
        let grandchild_job = make_child(&child_job1);

        let p1_koid = root_proc1.dispatcher().get_koid();
        let p2_koid = root_proc2.dispatcher().get_koid();
        let c1_p_koid = child1_proc.dispatcher().get_koid();
        let j1_koid = child_job1.get_koid();
        let j2_koid = child_job2.get_koid();
        let gj_koid = grandchild_job.get_koid();

        // Full walk visits processes before child jobs at each level and returns `true`.
        let mut full = EnumState::new(ZX_KOID_INVALID, ZX_KOID_INVALID);
        expect_true!(root.enumerate_children_recursive(&mut full));
        expect_false!(full.called_after_stop);
        expect_eq!(full.proc_count, 3);
        expect_true!(full.procs[..3] == [p1_koid, p2_koid, c1_p_koid]);
        expect_eq!(full.job_count, 3);
        expect_true!(full.jobs[..3] == [j1_koid, gj_koid, j2_koid]);

        // Returning `false` from `on_process` on a direct child stops before sibling processes and
        // child jobs.
        let mut stop_direct_proc = EnumState::new(ZX_KOID_INVALID, p1_koid);
        expect_false!(root.enumerate_children_recursive(&mut stop_direct_proc));
        expect_false!(stop_direct_proc.called_after_stop);
        expect_eq!(stop_direct_proc.proc_count, 1);
        expect_true!(stop_direct_proc.procs[..1] == [p1_koid]);
        expect_eq!(stop_direct_proc.job_count, 0);

        // Returning `false` from `on_process` inside a child job propagates out without visiting
        // deeper jobs or remaining sibling jobs.
        let mut stop_nested_proc = EnumState::new(ZX_KOID_INVALID, c1_p_koid);
        expect_false!(root.enumerate_children_recursive(&mut stop_nested_proc));
        expect_false!(stop_nested_proc.called_after_stop);
        expect_eq!(stop_nested_proc.proc_count, 3);
        expect_true!(stop_nested_proc.procs[..3] == [p1_koid, p2_koid, c1_p_koid]);
        expect_eq!(stop_nested_proc.job_count, 1);
        expect_true!(stop_nested_proc.jobs[..1] == [j1_koid]);

        // Returning `false` from `on_job` skips recursing into that job and skips sibling jobs.
        let mut stop_job = EnumState::new(j1_koid, ZX_KOID_INVALID);
        expect_false!(root.enumerate_children_recursive(&mut stop_job));
        expect_false!(stop_job.called_after_stop);
        expect_eq!(stop_job.proc_count, 2);
        expect_true!(stop_job.procs[..2] == [p1_koid, p2_koid]);
        expect_eq!(stop_job.job_count, 1);
        expect_true!(stop_job.jobs[..1] == [j1_koid]);

        root.kill(0);
        wait_for_signals(&root, ZX_JOB_TERMINATED);
    }

    /// `stable_sort_by` orders keys and preserves the relative order of equal keys.
    #[test]
    fn test_stable_sort_by() {
        let mut empty: [(i32, usize); 0] = [];
        stable_sort_by(&mut empty, |a, b| a.0.cmp(&b.0));

        let mut single = [(42, 0usize)];
        stable_sort_by(&mut single, |a, b| a.0.cmp(&b.0));
        expect_true!(single == [(42, 0)]);

        let mut reverse = [5, 4, 3, 2, 1];
        stable_sort_by(&mut reverse, i32::cmp);
        expect_true!(reverse == [1, 2, 3, 4, 5]);
        stable_sort_by(&mut reverse, i32::cmp);
        expect_true!(reverse == [1, 2, 3, 4, 5]);

        let mut all_equal = [(7, 0usize), (7, 1), (7, 2), (7, 3)];
        stable_sort_by(&mut all_equal, |a, b| b.0.cmp(&a.0));
        expect_true!(all_equal == [(7, 0), (7, 1), (7, 2), (7, 3)]);

        let mut items =
            [(20, 0usize), (30, 1), (10, 2), (30, 3), (20, 4), (10, 5), (30, 6), (20, 7)];
        stable_sort_by(&mut items, |a, b| b.0.cmp(&a.0));
        expect_true!(
            items == [(30, 1), (30, 3), (30, 6), (20, 0), (20, 4), (20, 7), (10, 2), (10, 5),]
        );
    }

    /// `OomBitJobArray` caps overflow and drops initialized `RefPtr`s without leaking.
    #[test]
    fn test_oom_bit_job_array_capacity() {
        /// Total OOM-marked jobs created to exceed `OOM_BIT_JOB_ARRAY_SIZE`.
        const TOTAL_OOM_JOBS: usize = OOM_BIT_JOB_ARRAY_SIZE + 2;

        let empty = OomBitJobArray::default();
        expect_true!(empty.is_empty());
        expect_false!(empty.is_full());
        drop(empty);

        let root = make_job();
        let mut koids = [ZX_KOID_INVALID; TOTAL_OOM_JOBS];
        let mut children: [Option<RefPtr<JobDispatcher>>; TOTAL_OOM_JOBS] =
            [const { None }; TOTAL_OOM_JOBS];
        for i in 0..TOTAL_OOM_JOBS {
            let child = make_child(&root);
            child.set_kill_on_oom(true);
            koids[i] = child.get_koid();
            children[i] = Some(child);
        }

        let mut oom_jobs = OomBitJobArray::new();
        root.collect_jobs_with_oom_bit(&mut oom_jobs);
        expect_eq!(oom_jobs.count, OOM_BIT_JOB_ARRAY_SIZE);
        expect_eq!(oom_jobs.len(), OOM_BIT_JOB_ARRAY_SIZE);
        expect_true!(oom_jobs.is_full());
        for i in 0..OOM_BIT_JOB_ARRAY_SIZE {
            expect_eq!(oom_jobs[i].get_koid(), koids[i]);
        }

        // Exercise `DerefMut` across all `OOM_BIT_JOB_ARRAY_SIZE` slots.
        oom_jobs.reverse();
        for i in 0..OOM_BIT_JOB_ARRAY_SIZE {
            expect_eq!(oom_jobs[i].get_koid(), koids[OOM_BIT_JOB_ARRAY_SIZE - 1 - i]);
        }

        // Dropping the caller handles destroys the two overflow jobs immediately, while the
        // `OOM_BIT_JOB_ARRAY_SIZE` collected jobs remain live until `oom_jobs` is dropped.
        drop(children);
        for &koid in &koids[OOM_BIT_JOB_ARRAY_SIZE..] {
            expect_true!(root.lookup_job_by_id(koid).is_none());
        }
        for &koid in &koids[..OOM_BIT_JOB_ARRAY_SIZE] {
            expect_true!(root.lookup_job_by_id(koid).is_some());
        }
        expect_eq!(root.poll_signals() & ZX_JOB_NO_JOBS, 0);

        drop(oom_jobs);
        for &koid in &koids[..OOM_BIT_JOB_ARRAY_SIZE] {
            expect_true!(root.lookup_job_by_id(koid).is_none());
        }
        expect_eq!(root.poll_signals() & ZX_JOB_NO_JOBS, ZX_JOB_NO_JOBS);

        // `kill_job_with_kill_on_oom` also handles hierarchies larger than
        // `OOM_BIT_JOB_ARRAY_SIZE`, sorting a full `OomBitJobArray` to kill the deepest job first
        // and draining the remaining jobs across subsequent calls.
        let mut live_children: [Option<RefPtr<JobDispatcher>>; TOTAL_OOM_JOBS - 1] =
            [const { None }; TOTAL_OOM_JOBS - 1];
        for slot in &mut live_children {
            let child = make_child(&root);
            child.set_kill_on_oom(true);
            *slot = Some(child);
        }
        let first_child = live_children[0].as_ref().expect("first child");
        let deep_grandchild = make_child(first_child);
        deep_grandchild.set_kill_on_oom(true);

        expect_true!(root.kill_job_with_kill_on_oom());
        expect_eq!(deep_grandchild.get_info().exited, 1);
        expect_eq!(deep_grandchild.get_info().return_code, ZX_TASK_RETCODE_OOM_KILL);

        for _ in 0..live_children.len() {
            expect_true!(root.kill_job_with_kill_on_oom());
        }
        expect_false!(root.kill_job_with_kill_on_oom());
        for child in live_children.iter().flatten() {
            expect_eq!(child.get_info().exited, 1);
            expect_eq!(child.get_info().return_code, ZX_TASK_RETCODE_OOM_KILL);
        }

        root.kill(0);
    }
}

/// Tests for job OOM kill behavior.
#[cfg(ktest)]
#[unittest::suite(name = "job")]
mod job_tests {
    use super::{JobDispatcher, JobEnumerator};
    use crate::object::get_root_job_dispatcher;
    use fbl::StringBuffer;
    use unittest::{assert_eq, expect_false, expect_ok, expect_true, unwrap_ok};
    use zx_types::ZX_MAX_NAME_LEN;

    /// Appends one line per job in the tree under `root` to `buf`, indented by depth and suffixed
    /// with ` KILL` when the job has `kill_on_oom` set.
    fn tree_to_string(root: &JobDispatcher, indent: usize, buf: &mut StringBuffer<512>) {
        for _ in 0..indent {
            buf.append_char(b' ');
        }

        let mut name = [0u8; ZX_MAX_NAME_LEN];
        root.get_name(&mut name).expect("get_name failed");
        let name_len = name.iter().position(|&b| b == 0).unwrap_or(ZX_MAX_NAME_LEN);
        buf.append(&name[..name_len]);

        if root.get_info().kill_on_oom != 0 {
            buf.append(b" KILL");
        }

        buf.append_char(b'\n');

        /// Recursively formats each direct child job with two additional spaces of indentation.
        struct Walker<'a> {
            /// Current indentation width in spaces.
            indent: usize,
            /// Output buffer receiving the formatted job lines.
            buf: &'a mut StringBuffer<512>,
        }
        impl JobEnumerator for Walker<'_> {
            fn on_job(&mut self, job: &JobDispatcher) -> bool {
                tree_to_string(job, self.indent + 2, self.buf);
                true
            }
        }
        let mut walker = Walker { indent, buf };
        root.enumerate_children(&mut walker);
    }

    /// Fills `per_level_count[d]` with the number of jobs at depth `d` below (and including)
    /// `root`.
    fn count_nodes_per_level(root: &JobDispatcher, per_level_count: &mut [i32]) {
        per_level_count.fill(0);

        /// Counts `node` at `level` and recurses into its direct child jobs.
        fn step(node: &JobDispatcher, level: usize, per_level_count: &mut [i32]) {
            per_level_count[level] += 1;
            /// Recurses into each direct child job at `level + 1`.
            struct Walker<'a> {
                /// Depth of `node`.
                level: usize,
                /// Per-depth job counter slice.
                per_level_count: &'a mut [i32],
            }
            impl JobEnumerator for Walker<'_> {
                fn on_job(&mut self, job: &JobDispatcher) -> bool {
                    step(job, self.level + 1, self.per_level_count);
                    true
                }
            }
            let mut walker = Walker { level, per_level_count };
            node.enumerate_children(&mut walker);
        }

        step(root, 0, per_level_count);
    }

    /// test trivial oom kill
    #[test]
    fn oom_job_kill_trivial() {
        let (root, _rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()));
        expect_ok!(root.dispatcher().set_name(b"root"));
        root.dispatcher().set_kill_on_oom(true);

        let mut buf = StringBuffer::<512>::new();
        tree_to_string(root.dispatcher(), 0, &mut buf);
        expect_true!(&buf[..] == b"root KILL\n", "incorrect start state");

        expect_true!(root.dispatcher().kill_job_with_kill_on_oom(), "killed");
    }

    /// test ordering of oom kill
    #[test]
    fn oom_job_kill_ordering() {
        // Given this tree (KILL indicating jobs that have ZX_PROP_JOB_KILL_ON_OOM):
        //
        //   root
        //     child1
        //       gchild1
        //       gchild2 KILL
        //       gchild3
        //     child2 KILL
        //       gchild4 KILL
        //       gchild5 KILL
        //
        let (root, _rights) =
            unwrap_ok!(JobDispatcher::create(0, get_root_job_dispatcher().clone()));
        expect_ok!(root.dispatcher().set_name(b"root"));

        let (child1, _rights) = unwrap_ok!(JobDispatcher::create(0, root.dispatcher().clone()));
        expect_ok!(child1.dispatcher().set_name(b"child1"));

        let (child2, _rights) = unwrap_ok!(JobDispatcher::create(0, root.dispatcher().clone()));
        expect_ok!(child2.dispatcher().set_name(b"child2"));
        child2.dispatcher().set_kill_on_oom(true);

        let (gchild1, _rights) = unwrap_ok!(JobDispatcher::create(0, child1.dispatcher().clone()));
        expect_ok!(gchild1.dispatcher().set_name(b"gchild1"));

        let (gchild2, _rights) = unwrap_ok!(JobDispatcher::create(0, child1.dispatcher().clone()));
        gchild2.dispatcher().set_kill_on_oom(true);
        expect_ok!(gchild2.dispatcher().set_name(b"gchild2"));

        let (gchild3, _rights) = unwrap_ok!(JobDispatcher::create(0, child1.dispatcher().clone()));
        expect_ok!(gchild3.dispatcher().set_name(b"gchild3"));

        let (gchild4, _rights) = unwrap_ok!(JobDispatcher::create(0, child2.dispatcher().clone()));
        gchild4.dispatcher().set_kill_on_oom(true);
        expect_ok!(gchild4.dispatcher().set_name(b"gchild4"));

        let (gchild5, _rights) = unwrap_ok!(JobDispatcher::create(0, child2.dispatcher().clone()));
        gchild5.dispatcher().set_kill_on_oom(true);
        expect_ok!(gchild5.dispatcher().set_name(b"gchild5"));

        let mut buf = StringBuffer::<512>::new();
        let mut per_level_count = [0i32; 3];

        buf.clear();
        tree_to_string(root.dispatcher(), 0, &mut buf);
        const EXPECTED_START: &[u8] = b"root\n  child1\n    gchild1\n    gchild2 KILL\n    \
            gchild3\n  child2 KILL\n    gchild4 KILL\n    gchild5 KILL\n";
        expect_true!(&buf[..] == EXPECTED_START, "incorrect start state");

        // The next 3 kills should kill 1 of (gchild2, gchild4, gchild5) each.
        // The order is an implementation detail.
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 2);
        assert_eq!(per_level_count[2], 5);

        expect_true!(root.dispatcher().kill_job_with_kill_on_oom(), "killed");
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 2);
        assert_eq!(per_level_count[2], 4);

        expect_true!(root.dispatcher().kill_job_with_kill_on_oom(), "killed");
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 2);
        assert_eq!(per_level_count[2], 3);

        expect_true!(root.dispatcher().kill_job_with_kill_on_oom(), "killed");
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 2);
        assert_eq!(per_level_count[2], 2);

        // No more killable oom jobs at level three, so child2 should be killed next at level 1.
        expect_true!(root.dispatcher().kill_job_with_kill_on_oom(), "killed");
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 1);
        assert_eq!(per_level_count[2], 2);

        // No more oom killable jobs in this hierarchy, so no jobs should be killed.
        expect_false!(root.dispatcher().kill_job_with_kill_on_oom(), "no kill");
        count_nodes_per_level(root.dispatcher(), &mut per_level_count);
        assert_eq!(per_level_count[0], 1);
        assert_eq!(per_level_count[1], 1);
        assert_eq!(per_level_count[2], 2);

        // Check end state.
        buf.clear();
        tree_to_string(root.dispatcher(), 0, &mut buf);
        expect_true!(
            &buf[..] == b"root\n  child1\n    gchild1\n    gchild3\n",
            "subsequent kills should have no effect"
        );

        // Clean up.
        root.dispatcher().kill(0);
    }
}
