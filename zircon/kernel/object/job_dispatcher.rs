// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_void;
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::{
    ZX_DEFAULT_JOB_RIGHTS, ZX_OBJECT_SIGNAL_6, zx_policy_basic_v1_t, zx_policy_basic_v2_t,
    zx_policy_timer_slack_t, zx_rights_t, zx_signals_t,
};

use super::channel_dispatcher::ChannelDispatcher;
use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use super::job_dispatcher_ffi::{
    cpp_job_dispatcher_create, cpp_job_dispatcher_create_debug_exceptionate,
    cpp_job_dispatcher_create_root_job, cpp_job_dispatcher_enumerate_children,
    cpp_job_dispatcher_enumerate_children_recursive, cpp_job_dispatcher_exceptionate,
    cpp_job_dispatcher_get_info, cpp_job_dispatcher_get_policy,
    cpp_job_dispatcher_get_runtime_stats, cpp_job_dispatcher_kill,
    cpp_job_dispatcher_kill_job_with_kill_on_oom, cpp_job_dispatcher_lookup_job_by_id,
    cpp_job_dispatcher_lookup_process_by_id, cpp_job_dispatcher_parent,
    cpp_job_dispatcher_set_basic_policy_v1, cpp_job_dispatcher_set_basic_policy_v2,
    cpp_job_dispatcher_set_kill_on_oom, cpp_job_dispatcher_set_timer_slack_policy,
};
use super::job_policy::JobPolicy;
use super::process_dispatcher::ProcessDispatcher;

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

crate::object::dispatcher::impl_dispatcher_facade!(
    pub struct JobDispatcher,
    zx_types::ZX_OBJ_TYPE_JOB
);

zr::static_assert!(core::mem::size_of::<JobDispatcher>() == 0);

/// Interface for walking a job/process tree.
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

impl JobDispatcher {
    /// Returns the default rights for a JobDispatcher handle.
    pub fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_JOB_RIGHTS
    }

    /// Creates a child `JobDispatcher` under `parent`.
    pub fn create(
        flags: u32,
        parent: fbl::RefPtr<JobDispatcher>,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        // SAFETY: `parent` transfers an acquired reference count into C++, and
        // `cpp_job_dispatcher_create` initializes `handle` and `rights` on success.
        unsafe {
            KernelHandle::create_with_rights(|handle_out, rights_out| {
                cpp_job_dispatcher_create(
                    flags,
                    fbl::RefPtr::into_raw(parent) as *mut _,
                    handle_out,
                    rights_out,
                )
            })
        }
    }

    /// Sets basic policy (v1) on this job.
    pub fn set_basic_policy_v1(
        &self,
        mode: u32,
        policy: &[zx_policy_basic_v1_t],
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `JobDispatcher` and `policy` points to `policy.len()` elements.
        let status = unsafe {
            cpp_job_dispatcher_set_basic_policy_v1(
                self as *const _ as *mut _,
                mode,
                policy.as_ptr(),
                policy.len(),
            )
        };
        Status::ok(status)
    }

    /// Sets basic policy (v2) on this job.
    pub fn set_basic_policy_v2(
        &self,
        mode: u32,
        policy: &[zx_policy_basic_v2_t],
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `JobDispatcher` and `policy` points to `policy.len()` elements.
        let status = unsafe {
            cpp_job_dispatcher_set_basic_policy_v2(
                self as *const _ as *mut _,
                mode,
                policy.as_ptr(),
                policy.len(),
            )
        };
        Status::ok(status)
    }

    /// Sets timer slack policy on this job.
    pub fn set_timer_slack_policy(&self, policy: &zx_policy_timer_slack_t) -> Result<(), Status> {
        // SAFETY: `self` is a valid `JobDispatcher` and `policy` points to a valid struct.
        let status = unsafe {
            cpp_job_dispatcher_set_timer_slack_policy(
                self as *const _ as *mut _,
                policy as *const _,
            )
        };
        Status::ok(status)
    }

    /// Creates and returns a new root job dispatcher.
    pub fn create_root_job() -> fbl::RefPtr<JobDispatcher> {
        // SAFETY: `cpp_job_dispatcher_create_root_job` returns a raw pointer carrying an acquired
        // refcount.
        unsafe { fbl::RefPtr::from_raw(cpp_job_dispatcher_create_root_job()) }
    }

    /// Terminates this job and all child jobs and child processes.
    pub fn kill(&self, return_code: i64) -> bool {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        unsafe { cpp_job_dispatcher_kill(self as *const _ as *mut _, return_code) }
    }

    /// Sets whether this job should be killed during out-of-memory events.
    pub fn set_kill_on_oom(&self, value: bool) {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        unsafe { cpp_job_dispatcher_set_kill_on_oom(self as *const _ as *mut _, value) }
    }

    /// Kills the lowest child job that has `kill_on_oom` set.
    pub fn kill_job_with_kill_on_oom(&self) -> bool {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        unsafe { cpp_job_dispatcher_kill_job_with_kill_on_oom(self as *const _ as *mut _) }
    }

    /// Returns information about the job.
    pub fn get_info(&self) -> zx_types::zx_info_job_t {
        let mut info = MaybeUninit::<zx_types::zx_info_job_t>::uninit();
        // SAFETY: `self` is valid and `info` points to uninitialized stack memory.
        unsafe {
            cpp_job_dispatcher_get_info(self as *const _, info.as_mut_ptr());
            info.assume_init()
        }
    }

    /// Returns aggregate runtime stats for this job and child processes.
    pub fn get_runtime_stats(&self) -> zx_types::zx_info_task_runtime_t {
        // SAFETY: self is a valid JobDispatcher reference.
        unsafe { cpp_job_dispatcher_get_runtime_stats(self as *const _) }
    }

    /// Enumerates child jobs or child processes into user buffer and returns
    /// `(actual_count, available_count)`.
    pub fn enumerate_children(
        &self,
        user_koids: crate::user_copy::UserOutPtr<zx_types::zx_koid_t>,
        max: usize,
        is_jobs: bool,
    ) -> Result<(usize, usize), Status> {
        let mut count = 0usize;
        let mut avail = 0usize;
        // SAFETY: self is a valid JobDispatcher reference and user_koids is a user pointer.
        let status = unsafe {
            cpp_job_dispatcher_enumerate_children(
                self as *const _,
                user_koids.as_ptr(),
                max,
                is_jobs,
                &mut count,
                &mut avail,
            )
        };
        Status::ok(status)?;
        Ok((count, avail))
    }

    /// Returns the job's normal `Exceptionate`.
    #[inline]
    pub fn exceptionate(&self) -> &Exceptionate {
        // SAFETY: `self` is a valid `JobDispatcher`, which owns `exceptionate_` for its entire
        // lifetime.
        unsafe { &*cpp_job_dispatcher_exceptionate(self as *const _ as *mut _) }
    }

    /// Creates a debug `Exceptionate` on this job backed by `channel_handle`.
    pub fn create_debug_exceptionate(
        &self,
        mut channel_handle: KernelHandle<ChannelDispatcher>,
        thread_rights: zx_rights_t,
        process_rights: zx_rights_t,
    ) -> Result<(), Status> {
        // SAFETY: `self` and `channel_handle` are valid.
        Status::ok(unsafe {
            cpp_job_dispatcher_create_debug_exceptionate(
                self as *const _ as *mut _,
                &mut channel_handle,
                thread_rights,
                process_rights,
            )
        })
    }

    /// Looks up a direct child job of this job given its koid.
    #[inline]
    pub fn lookup_job_by_id(
        &self,
        koid: zx_types::zx_koid_t,
    ) -> Option<fbl::RefPtr<JobDispatcher>> {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        let ptr = unsafe { cpp_job_dispatcher_lookup_job_by_id(self as *const _ as *mut _, koid) };
        // SAFETY: `ptr` is exported via `fbl::ExportToRawPtr` with an acquired refcount, or null.
        unsafe { fbl::RefPtr::try_from_raw(ptr) }
    }

    /// Looks up a direct child process of this job given its koid.
    #[inline]
    pub fn lookup_process_by_id(
        &self,
        koid: zx_types::zx_koid_t,
    ) -> Option<fbl::RefPtr<ProcessDispatcher>> {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        let ptr =
            unsafe { cpp_job_dispatcher_lookup_process_by_id(self as *const _ as *mut _, koid) };
        // SAFETY: `ptr` is exported via `fbl::ExportToRawPtr` with an acquired refcount, or null.
        unsafe { fbl::RefPtr::try_from_raw(ptr) }
    }

    /// Returns the parent job of this job, or `None` if this is the root job.
    #[inline]
    pub fn parent(&self) -> Option<fbl::RefPtr<JobDispatcher>> {
        // SAFETY: `self` is a valid `JobDispatcher` reference.
        let ptr = unsafe { cpp_job_dispatcher_parent(core::ptr::from_ref(self)) };
        // SAFETY: `ptr` is exported via `fbl::ExportToRawPtr` with an acquired refcount, or null.
        unsafe { fbl::RefPtr::try_from_raw(ptr) }
    }

    /// Returns a copy of this job's policy.
    ///
    /// Must not be called with this job's lock held.
    #[inline]
    pub fn get_policy(&self) -> JobPolicy {
        let mut policy = MaybeUninit::<JobPolicy>::uninit();
        // SAFETY: `self` is a valid `JobDispatcher` reference and `&raw mut policy` points to
        // valid, aligned stack storage that `cpp_job_dispatcher_get_policy` unconditionally
        // initializes before returning.
        unsafe {
            cpp_job_dispatcher_get_policy(core::ptr::from_ref(self), &raw mut policy);
            policy.assume_init()
        }
    }

    /// Recursively walks the job/process tree and invokes `je` methods on each
    /// node. The `je` methods are invoked with all parent `JobDispatcher` object
    /// locks held. Returns false if any methods of `je` return false; returns true
    /// otherwise.
    ///
    /// Must not be called with this job's lock held.
    pub fn enumerate_children_recursive<E: JobEnumerator>(&self, je: &mut E) -> bool {
        let ctx_ptr: *mut c_void = core::ptr::from_mut(je).cast();
        // SAFETY: `self` is a valid `JobDispatcher` reference, `ctx_ptr` points to a valid `E`
        // that can be exclusively borrowed for each callback invocation, and the C++ recursive
        // enumeration holds the parent job locks keeping each visited `JobDispatcher` and
        // `ProcessDispatcher` alive for the duration of the callback.
        unsafe {
            cpp_job_dispatcher_enumerate_children_recursive(
                core::ptr::from_ref(self).cast_mut(),
                ctx_ptr,
                Self::on_job_shim::<E>,
                Self::on_process_shim::<E>,
            )
        }
    }

    /// Enumerates the direct (non-recursive) children and invokes `je` methods on each node.
    ///
    /// The `je` methods are invoked without the lock of this `JobDispatcher` held, however a
    /// consistent atomic snapshot of children will be given. Returns false if any methods of `je`
    /// return false; returns true otherwise.
    #[cfg(ktest)]
    pub fn enumerate_direct_children<E: JobEnumerator>(&self, je: &mut E) -> bool {
        use super::job_dispatcher_ffi::cpp_job_dispatcher_enumerate_direct_children;

        let ctx_ptr: *mut c_void = core::ptr::from_mut(je).cast();
        // SAFETY: `self` is a valid `JobDispatcher` reference, `ctx_ptr` points to a valid `E`
        // that can be exclusively borrowed for each callback invocation, and the C++ direct
        // enumeration holds a `LiveRefsArray` snapshot keeping each visited `JobDispatcher` and
        // `ProcessDispatcher` alive for the duration of the callback.
        unsafe {
            cpp_job_dispatcher_enumerate_direct_children(
                core::ptr::from_ref(self).cast_mut(),
                ctx_ptr,
                Self::on_job_shim::<E>,
                Self::on_process_shim::<E>,
            )
        }
    }

    /// # Safety
    ///
    /// `ctx` must point to a valid `E` that can be exclusively borrowed for the duration of the
    /// callback, and `job` must point to a live `JobDispatcher` for the duration of the callback.
    unsafe extern "C" fn on_job_shim<E: JobEnumerator>(
        ctx: *mut c_void,
        job: *mut JobDispatcher,
    ) -> bool {
        // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
        let (je, job) = unsafe { (&mut *ctx.cast::<E>(), &*job) };
        je.on_job(job)
    }

    /// # Safety
    ///
    /// `ctx` must point to a valid `E` that can be exclusively borrowed for the duration of the
    /// callback, and `proc` must point to a live `ProcessDispatcher` for the duration of the
    /// callback.
    unsafe extern "C" fn on_process_shim<E: JobEnumerator>(
        ctx: *mut c_void,
        proc: *mut ProcessDispatcher,
    ) -> bool {
        // SAFETY: Guaranteed by the caller per this function's `# Safety` contract.
        let (je, proc) = unsafe { (&mut *ctx.cast::<E>(), &*proc) };
        je.on_process(proc)
    }
}

/// JobDispatcher unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "job_dispatcher_tests")]
mod job_dispatcher_tests {
    use super::{JobDispatcher, JobEnumerator, ZX_JOB_NO_CHILDREN};
    use crate::object::{ProcessDispatcher, get_root_job_dispatcher};
    use unittest::{
        assert_false, assert_gt, assert_true, expect_eq, expect_ok, expect_true, unwrap_ok,
    };
    use zx_types::{ZX_JOB_NO_JOBS, ZX_JOB_NO_PROCESSES, ZX_KOID_INVALID, zx_koid_t};

    const INDEX_LIMIT: usize = 20;

    #[derive(Clone, Copy, Default)]
    struct Entry {
        koid: zx_koid_t,
        parent_koid: zx_koid_t,
    }

    struct TestJobEnumerator {
        badness: bool,
        called_after_stop: bool,
        index: usize,
        entries: [Entry; INDEX_LIMIT],
    }

    impl TestJobEnumerator {
        fn new() -> Self {
            Self {
                badness: false,
                called_after_stop: false,
                index: 0,
                entries: [Entry::default(); INDEX_LIMIT],
            }
        }

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

        let policy = [zx_types::zx_policy_basic_v2_t {
            condition: zx_types::ZX_POL_NEW_EVENT,
            action: zx_types::ZX_POL_ACTION_DENY,
            flags: zx_types::ZX_POL_OVERRIDE_DENY,
        }];
        expect_ok!(
            child_job.dispatcher().set_basic_policy_v2(zx_types::ZX_JOB_POL_RELATIVE, &policy)
        );

        struct PolicyJobEnumerator {
            target_koid: zx_koid_t,
            visited: bool,
            action: u32,
        }

        impl JobEnumerator for PolicyJobEnumerator {
            fn on_job(&mut self, job: &JobDispatcher) -> bool {
                if job.get_koid() == self.target_koid {
                    self.visited = true;
                    self.action = job.get_policy().query_basic_policy(zx_types::ZX_POL_NEW_EVENT);
                }
                true
            }
        }

        let mut enumerator = PolicyJobEnumerator {
            target_koid: child_job.dispatcher().get_koid(),
            visited: false,
            action: zx_types::ZX_POL_ACTION_ALLOW,
        };

        expect_true!(parent_job.dispatcher().enumerate_children_recursive(&mut enumerator));
        expect_true!(enumerator.visited);
        expect_eq!(enumerator.action, zx_types::ZX_POL_ACTION_DENY);

        parent_job.dispatcher().kill(0);
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

        struct Walker<'a> {
            indent: usize,
            buf: &'a mut StringBuffer<512>,
        }
        impl JobEnumerator for Walker<'_> {
            fn on_job(&mut self, job: &JobDispatcher) -> bool {
                tree_to_string(job, self.indent + 2, self.buf);
                true
            }
        }
        let mut walker = Walker { indent, buf };
        root.enumerate_direct_children(&mut walker);
    }

    fn count_nodes_per_level(root: &JobDispatcher, per_level_count: &mut [i32]) {
        per_level_count.fill(0);

        fn step(node: &JobDispatcher, level: usize, per_level_count: &mut [i32]) {
            per_level_count[level] += 1;
            struct Walker<'a> {
                level: usize,
                per_level_count: &'a mut [i32],
            }
            impl JobEnumerator for Walker<'_> {
                fn on_job(&mut self, job: &JobDispatcher) -> bool {
                    step(job, self.level + 1, self.per_level_count);
                    true
                }
            }
            let mut walker = Walker { level, per_level_count };
            node.enumerate_direct_children(&mut walker);
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
