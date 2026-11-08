// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher_ffi::*;
use super::dispatcher::{
    DispatcherOps, PeerHolder, PeerHolderMuClass, PeeredState, PeeredStateMuGuard,
    impl_peered_dispatcher_facade_with_state,
};
use super::handle::KernelHandle;
use super::message_packet::{MessagePacket, MessagePacketPtr};
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{AutoBlocked, Blocked, CurrentMessageWaiter, ThreadDispatcher};
use super::user_handles::msg_handle_refs;
use crate::counters::define_kcounter;
use crate::kconcurrent::{
    ChainLockTransaction, ChainLockTransactionToken, EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
    OwnerGuard, TransactionResult, assert_no_transaction_active, clt_tag,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::{OwnedWaitQueue, WakeOption};
use crate::kernel::scheduler;
use crate::kernel::thread::{
    AnnotatedAutoEagerReschedDisabler, PreemptDisabledToken, Thread, ThreadPtr,
    signal_policy_exception,
};
use crate::kernel::wait_queue::{ForceInheritance, Interruptible, ResourceOwnership};
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::ktrace_rs::{
    KTrace, category_enabled, duration_begin_timestamp, duration_end_timestamp,
    flow_begin_timestamp, flow_end_timestamp, flow_step_timestamp,
};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::mem::{self, MaybeUninit, align_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::{cmp, slice, str};
use fbl::{
    Canary, DefaultObjectTag, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    RefPtr, TrackingSize,
};
use ksync::{KMutex, PhantomMutex, guarded, kcell_init};
use object_constants_rs::{
    kChannelDispatcherStateAlign, kChannelDispatcherStateOffset, kChannelDispatcherStateSize,
};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::{
    ZX_CHANNEL_PEER_CLOSED, ZX_CHANNEL_READABLE, ZX_CHANNEL_WRITABLE, ZX_DEFAULT_CHANNEL_RIGHTS,
    ZX_EXCP_POLICY_CODE_CHANNEL_FULL_WRITE, ZX_OBJ_TYPE_CHANNEL, ZX_TASK_RETCODE_VDSO_KILL,
    ZX_USER_SIGNAL_ALL, zx_koid_t, zx_obj_type_t, zx_rights_t, zx_txid_t,
};

/// Signals that can be asserted on a ChannelDispatcher by userspace.
const ALLOWED_SIGNALS: u32 = ZX_USER_SIGNAL_ALL | ZX_CHANNEL_READABLE | ZX_CHANNEL_WRITABLE;

/// Kernel generated transaction IDs have the most significant bit set to distinguish them from
/// userspace generated transaction IDs.
// This value is part of the zx_channel_call contract.
const MIN_KERNEL_GENERATED_TXID: u32 = 0x80000000;

/// Maximum pending message count threshold for a channel before raising exceptions.
// Temporary hack to chase down bugs like https://fxbug.dev/42123699 where upwards of 250MB of ipc
// memory is consumed. The bet is that even if each message is at max size there should be one or
// two channels with thousands of messages. If so, this check adds no overhead to the existing code.
// See https://fxbug.dev/42124465.
// TODO(cpu): This limit can be lower but mojo's ChannelTest.PeerStressTest sends about 3K small
// messages. Switching to size limit is more reasonable.
const MAX_PENDING_MESSAGE_COUNT: usize = 3500;

/// Warning threshold for pending message count on a channel.
const WARN_PENDING_MESSAGE_COUNT: usize = MAX_PENDING_MESSAGE_COUNT / 2;

/// Maximum number of handles to include in channel message traces when body tracing is enabled.
const MAX_TRACE_HANDLES: usize = 4;

/// Randomly generated multilinear hash coefficients. These should be sufficient for non-user
/// builds where tracing syscalls are enabled. In the future, if we elect to enable tracing
/// facilities in user builds, this can be strengthened by generating the coefficients during
/// boot.
const HASH_COEFFICIENTS: [u64; 6] = [
    0xa573c3ccbd7e2010,
    0x165cbcf3a0de8544,
    0x8b975f576f025514,
    0xabc406ce862c9a1d,
    0xf292bea1a3fe6bed,
    0x1c7c06b8b02b4585,
];

define_kcounter!(CHANNEL_PACKET_DEPTH_1, "channel.depth.1", Sum);
define_kcounter!(CHANNEL_PACKET_DEPTH_4, "channel.depth.4", Sum);
define_kcounter!(CHANNEL_PACKET_DEPTH_16, "channel.depth.16", Sum);
define_kcounter!(CHANNEL_PACKET_DEPTH_64, "channel.depth.64", Sum);
define_kcounter!(CHANNEL_PACKET_DEPTH_256, "channel.depth.256", Sum);
define_kcounter!(CHANNEL_PACKET_DEPTH_UNBOUNDED, "channel.depth.unbounded", Sum);
define_kcounter!(CHANNEL_FULL, "channel.full", Sum);
define_kcounter!(DISPATCHER_CHANNEL_CREATE_COUNT, "dispatcher.channel.create", Sum);
define_kcounter!(DISPATCHER_CHANNEL_DESTROY_COUNT, "dispatcher.channel.destroy", Sum);

/// Operation code passed to tracing for channel message transfers.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum MessageOp {
    Write,
    Read,
    ChannelCallWriteRequest,
    ChannelCallReadResponse,
}

#[repr(C)]
#[derive(Copy, Clone, Default, FromBytes)]
struct FidlHeader {
    txid: zx_txid_t,
    flags: [u8; 3],
    magic: u8,
    ordinal: u64,
}

zr::static_assert!(size_of::<FidlHeader>() == 2 * size_of::<u64>());

#[repr(C)]
#[derive(Copy, Clone, IntoBytes, Immutable)]
struct RawInfoHandleBasic {
    koid: zx_koid_t,
    rights: zx_rights_t,
    type_: zx_obj_type_t,
    related_koid: zx_koid_t,
    reserved: u32,
    padding1: [u8; 4],
}

zr::static_assert_size_and_align!(
    RawInfoHandleBasic,
    size_of::<zx_types::zx_info_handle_basic_t>(),
    align_of::<zx_types::zx_info_handle_basic_t>(),
);

#[inline]
fn is_kernel_generated_txid(txid: zx_txid_t) -> bool {
    txid >= MIN_KERNEL_GENERATED_TXID
}

/// 64-bit to 32-bit hash using the multilinear hash family `ax + by + c`.
#[inline]
fn hash_value(a: u64, b: u64, c: u64, value: u64) -> u32 {
    let x = value as u32 as u64;
    let y = (value >> 32) as u32 as u64;
    ((a.wrapping_mul(x)).wrapping_add(b.wrapping_mul(y)).wrapping_add(c) >> 32) as u32
}

/// First hash function using randomly generated coefficients.
#[inline]
fn hash_a(value: u64) -> u32 {
    hash_value(HASH_COEFFICIENTS[0], HASH_COEFFICIENTS[1], HASH_COEFFICIENTS[2], value)
}

/// Second hash function using randomly generated coefficients.
#[inline]
fn hash_b(value: u64) -> u32 {
    hash_value(HASH_COEFFICIENTS[3], HASH_COEFFICIENTS[4], HASH_COEFFICIENTS[5], value)
}

#[inline]
fn hash_b_pair(high: u32, low: u32) -> u32 {
    hash_b(((high as u64) << 32) | (low as u64))
}

/// Generates a flow id using a universal hash function of the minimum endpoint koid and the txid
/// or message packet address, depending on whether the txid is kernel-generated.
///
/// In general, koids are guaranteed to be unique over the lifetime of a particular system boot.
/// Using the min endpoint koid ensures both endpoints use the same hash input. A txid shared
/// between sender and receiver is expected to be unique (guaranteed for kernel-generated txids)
/// among the set of txids for messages pending in a particular channel. Likewise, the message
/// packet address is shared between the sender and receiver and is guaranteed to be unique among
/// the set of pointers to pending messages.
///
/// Given that the `(koid, txid)` or `(koid, &msg)` pair is likely to be unique over the span of
/// the flow, the likelihood of id confusion is equivalent to the likelihood of hash collisions by
/// temporally overlapping flows.
fn channel_message_flow_id(
    msg: &MessagePacket,
    txid: zx_txid_t,
    channel: &ChannelDispatcher,
) -> u64 {
    let min_koid = cmp::min(channel.get_koid(), channel.get_related_koid());

    // Use the top bit of the message id to indicate whether the input was a txid, which can be
    // used to correlate a later response message, or a message pointer, which cannot. The 32-bit
    // txid is combined with the bottom 32 bits of the channel koid as inputs to `hash_b_pair` to
    // improve the uniqueness of the message id.
    let is_txid_mask = 1u32 << 31;
    let message_id = if !is_kernel_generated_txid(txid) {
        hash_b(ptr::from_ref(msg).addr() as u64) & !is_txid_mask
    } else {
        hash_b_pair(txid, min_koid as u32) | is_txid_mask
    };

    let high = hash_a(min_koid) as u64;
    let low = message_id as u64;
    (high << 32) | low
}

/// Internal state of a `ChannelDispatcher`, synchronized via `channel_lock` and peer holder mutex.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct ChannelDispatcherState {
    canary: Canary<{ fbl::magic(b"CHAN") }>,

    #[guarded_by(mu)]
    txid: u32,

    #[pin]
    peered: PeeredState<ChannelDispatcher>,

    // By using a dedicated lock to protect the fields accessed by `read`, we can avoid acquiring
    // the peer holder lock (`mu`) in the `read` path. Why do we care? The peer holder lock is held
    // when raising signals and notifying matching observers. And there can be a lot of matching
    // observers. By using two locks and never acquiring the peer holder lock in the `read` path we
    // can allow `read` to execute concurrently with the observer notification.
    //
    // When acquiring both the peer holder lock and `channel_lock` be sure to acquire the peer
    // holder lock first.
    #[mutex]
    channel_lock: KMutex<TrackedCriticalMutex>,

    #[guarded_by(channel_lock)]
    #[pin]
    messages: DoublyLinkedList<MessagePacketPtr, DefaultObjectTag, TrackingSize>,

    #[guarded_by(channel_lock)]
    max_message_count: u64,

    // Tracks the process that is allowed to issue calls, for example write to the opposite end.
    // Without it, one can see writes out of order with respect of the previous and current owner.
    // We avoid locking and updating `owner` if the new owner is kernel, which happens when the
    // endpoint is written into a channel or during process destruction.
    //
    // The locking protocol for this field is a little tricky. The `read` method, which only ever
    // acquires `channel_lock`, must read this field. The `write` method also needs to read this
    // field, however, it needs to do so before it would otherwise need to acquire `channel_lock`.
    // So to avoid having `write` prematurely acquire and release `channel_lock`, we instead require
    // that either the peer holder lock or `channel_lock` are held when reading this field and both
    // are held when writing it.
    owner: UnsafeCell<zx_koid_t>,

    #[guarded_by(mu)]
    #[pin]
    waiters: DoublyLinkedList<NonNull<MessageWaiter>>,

    // True if this object's peer has been closed. This field exists so that `read` can check for
    // peer closed without having to acquire the peer holder lock.
    #[guarded_by(channel_lock)]
    peer_has_closed: bool,

    #[mutex(PeerHolderMuClass<ChannelDispatcher>)]
    mu: KMutex<PhantomMutex>,
}

zr::static_assert_size_and_align!(
    ChannelDispatcherState,
    kChannelDispatcherStateSize,
    kChannelDispatcherStateAlign,
);

impl ChannelDispatcherState {
    /// In-place pinned initializer for `ChannelDispatcherState`.
    pub(super) fn init(
        holder: RefPtr<PeerHolder<ChannelDispatcher>>,
    ) -> impl PinInit<Self, Infallible> {
        DISPATCHER_CHANNEL_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            txid: 0.into(),
            peered <- PeeredState::init(holder),
            channel_lock <- KMutex::init(),
            messages <- kcell_init(DoublyLinkedList::new()),
            max_message_count: 0.into(),
            owner: UnsafeCell::new(zx_types::ZX_KOID_INVALID),
            waiters <- kcell_init(DoublyLinkedList::new()),
            peer_has_closed: false.into(),
            mu: KMutex::new(PhantomMutex),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for ChannelDispatcherState {
    fn drop(self: Pin<&mut Self>) {
        DISPATCHER_CHANNEL_DESTROY_COUNT.add(1);

        // At this point the other endpoint no longer holds a reference to us, so we can be sure
        // we're discarding any remaining messages safely.
        //
        // It's not possible to do this safely in on_zero_handles_locked().
        let this = self.project();
        this.messages.get_pinned_mut().clear();

        match *this.max_message_count.as_mut() {
            0..=1 => CHANNEL_PACKET_DEPTH_1.add(1),
            2..=4 => CHANNEL_PACKET_DEPTH_4.add(1),
            5..=16 => CHANNEL_PACKET_DEPTH_16.add(1),
            17..=64 => CHANNEL_PACKET_DEPTH_64.add(1),
            65..=256 => CHANNEL_PACKET_DEPTH_256.add(1),
            _ => CHANNEL_PACKET_DEPTH_UNBOUNDED.add(1),
        }
    }
}

impl_peered_dispatcher_facade_with_state!(
    pub struct ChannelDispatcher,
    ChannelDispatcherState,
    ZX_OBJ_TYPE_CHANNEL,
    kChannelDispatcherStateOffset,
    allowed_signals: ALLOWED_SIGNALS,
);

zr::static_assert!(size_of::<ChannelDispatcher>() == 0);

impl ChannelDispatcher {
    /// Returns the number of times a channel reached the max pending message count,
    /// [`MAX_PENDING_MESSAGE_COUNT`].
    pub(crate) fn get_channel_full_count() -> i64 {
        CHANNEL_FULL.sum_across_all_cpus()
    }

    /// Creates a new `ChannelDispatcher` pair and returns their kernel handles and rights.
    pub fn create() -> Result<(KernelHandle<Self>, KernelHandle<Self>, zx_rights_t), Status> {
        let holder0 = PeerHolder::<Self>::create().map_err(|_| Status::NO_MEMORY)?;
        let holder1 = holder0.clone();

        let create_single =
            |holder: RefPtr<PeerHolder<Self>>| -> Result<KernelHandle<Self>, Status> {
                let raw = RefPtr::into_raw(holder);
                // SAFETY: `raw` is transferred to `ChannelDispatcher` on success, or reclaimed on
                // failure.
                unsafe {
                    KernelHandle::create(|out| {
                        cpp_channel_dispatcher_create(raw.cast_mut().cast(), out)
                    })
                    .inspect_err(|_| drop(RefPtr::from_raw(raw)))
                }
            };

        let handle0 = create_single(holder0)?;
        let handle1 = create_single(holder1)?;

        handle0.dispatcher().init_peer(handle1.dispatcher().clone());
        handle1.dispatcher().init_peer(handle0.dispatcher().clone());

        Ok((handle0, handle1, ZX_DEFAULT_CHANNEL_RIGHTS))
    }

    /// Read from this endpoint's message queue. `owner` is the handle table koid of the process
    /// attempting to read from the channel. `msg_size` and `msg_handle_count` are in-out
    /// parameters. As input, they specify the maximum size and handle count, respectively. On `Ok`
    /// or `Err(Status::BUFFER_TOO_SMALL)`, they specify the actual size and handle count of the
    /// next message. The next message is returned on `Ok`, or popped and discarded on
    /// `Err(Status::BUFFER_TOO_SMALL)` when `may_discard` is set.
    // This method should never acquire the peer holder lock (`self.state().peered.lock()`). See
    // the comment at `channel_lock` for details.
    #[inline]
    pub fn read(
        &self,
        owner: zx_koid_t,
        msg_size: &mut u32,
        msg_handle_count: &mut u32,
        may_discard: bool,
    ) -> Result<MessagePacketPtr, Status> {
        let state = self.state();
        state.canary.assert();

        let max_size = *msg_size;
        let max_handle_count = *msg_handle_count;

        let (msg, too_small) = {
            ksync::lock!(let mut g = state.lock_channel_lock());

            // SAFETY: Holding `channel_lock` is sufficient to read `owner` (writing requires
            // holding both the peer holder lock and `channel_lock`).
            if owner != unsafe { *state.owner.get() } {
                return Err(Status::BAD_HANDLE);
            }

            let peer_has_closed = *g.peer_has_closed();
            let mut messages = g.as_mut().messages_mut();
            let Some(front) = messages.front() else {
                return Err(if peer_has_closed {
                    Status::PEER_CLOSED
                } else {
                    Status::SHOULD_WAIT
                });
            };

            *msg_size = front.data_size() as u32;
            *msg_handle_count = front.num_handles() as u32;
            let too_small = *msg_size > max_size || *msg_handle_count > max_handle_count;
            if too_small && !may_discard {
                return Err(Status::BUFFER_TOO_SMALL);
            }

            let msg = messages.as_mut().pop_front().unwrap();
            if messages.is_empty() {
                self.clear_signals(ZX_CHANNEL_READABLE);
            }
            (msg, too_small)
        };
        if too_small {
            return Err(Status::BUFFER_TOO_SMALL);
        }

        // If we reach here, we popped a non-empty message from `messages` with `Status::OK`.
        self.trace_message(&msg, MessageOp::Read);
        Ok(msg)
    }

    /// Write to the opposing endpoint's message queue. `owner` is the handle table koid of the
    /// process attempting to write to the channel, or `ZX_KOID_INVALID` if kernel is doing it.
    pub fn write(&self, owner: zx_koid_t, msg: MessagePacketPtr) -> Result<(), Status> {
        let state = self.state();
        state.canary.assert();

        let waiter_to_signal = {
            ksync::lock!(let mut guard = state.peered.lock());

            // Failing this test is only possible if this process has two threads racing: one thread
            // is issuing channel_write() and one thread is moving the handle to another process.
            // SAFETY: Holding the peer holder lock (`peered`) is sufficient to read `owner`
            // (writing requires holding both the peer holder lock and `channel_lock`).
            if owner != unsafe { *state.owner.get() } {
                core::hint::cold_path();
                return Err(Status::BAD_HANDLE);
            }

            let Some(peer) = self.peer(&guard) else {
                core::hint::cold_path();
                return Err(Status::PEER_CLOSED);
            };

            self.trace_message(&msg, MessageOp::Write);

            match peer.try_write_to_message_waiter(guard.as_mut().token_mut(), msg) {
                Ok(waiter) => Some((waiter, AnnotatedAutoEagerReschedDisabler::new())),
                Err(msg) => {
                    peer.write_self_locked(guard.as_mut().token_mut(), msg, None);
                    None
                }
            }
        };

        if let Some((waiter, eager_resched_disabler)) = waiter_to_signal {
            // Signal the waiter after releasing `peered.lock()` (`CriticalMutex`) so waking the
            // caller neither activates the 150us timeslice extension timer nor contends on
            // `peered.lock()` when the caller resumes.
            // SAFETY: `try_write_to_message_waiter` unlinked `waiter` from `peer`'s `waiters` list
            // and stored `Ok(msg)` in `waiter.result` while holding `peered.lock()`. `waiter`
            // remains valid until `signal` sets `waiter.signaled = true` and releases
            // `waiter.wait_queue.lock()` (any concurrent timeout or thread teardown that observes
            // `waiter` already unlinked from `waiters` waits in `wait_until_signaled()`).
            unsafe {
                MessageWaiter::signal(waiter, Some(eager_resched_disabler));
            }
        }

        Ok(())
    }

    /// Perform a transacted Write + Read. `owner` is the handle table koid of the process
    /// attempting to write to the channel, or `ZX_KOID_INVALID` if kernel is doing it.
    #[inline]
    pub fn call(
        channel: RefPtr<Self>,
        owner: zx_koid_t,
        mut msg: MessagePacketPtr,
        deadline: Deadline,
    ) -> Result<MessagePacketPtr, Status> {
        let this_ptr = RefPtr::as_ptr(&channel);
        let waiter = ThreadDispatcher::get_current_message_waiter();
        if let Err(channel) = waiter.begin_wait(channel) {
            core::hint::cold_path();
            return Err(Self::kill_vdso_violator(channel));
        }

        // SAFETY: `channel` was moved into `waiter.begin_wait` on the current thread, where it
        // remains alive until `waiter.end_wait_and_take_channel` returns it on the same thread: at
        // the end of `call` or `resume_interrupted_call` (directly or via
        // `cancel_or_complete_wait`), or via `cancel_pending_wait` in
        // `ThreadDispatcher::exiting_current` if the thread exits after an interrupted call.
        let this: &Self = unsafe { &*this_ptr };
        let state = this.state();
        state.canary.assert();

        let lock_res = 'lock: {
            // Use time limited preemption deferral while we hold this lock. If our server is
            // running with a deadline profile, (and we are not) then after we queue the message and
            // signal the server, it is possible that the server thread:
            //
            // 1) Gets assigned to our core.
            // 2) It reads the message we just sent.
            // 3) It processes the message and responds with a write to this channel before we get a
            //    chance to drop the lock.
            //
            // This will result in an undesirable thrash sequence where:
            //
            // 1) The server thread contests the lock we are holding.
            // 2) It suffers through the adaptive mutex spin (but it is on our CPU, so it will never
            //    discover that the lock is available)
            // 3) It will then drop into a block transmitting its profile pressure, and allowing us
            //    to run again.
            // 4) we will run for a very short time until we finish our notifications.
            // 5) As soon as we drop the lock, we will immediately bounce back to the server thread
            //    which will complete its operation.
            //
            // Hard disabling preemption helps to avoid this thrash, but comes with a caveat. It may
            // be that the observer list we need to notify is Very Long and takes a significant
            // amount of time to filter and signal. We _really_ do not want to be running with
            // preemption disabled for very long as it can hold off time critical tasks. So instead
            // of hard disabling preemption we use CriticalMutex and rely on it to provide
            // time-limited preemption deferral.
            //
            // TODO(johngro): Even with time-limited preemption deferral, this mitigation is not
            // ideal. We would much prefer an approach where we do something like move the
            // notification step outside of the lock, or break the locks protecting the two message
            // and waiter queues into two locks instead of a single shared lock, so that we never
            // have to defer preemption. Such a solution gets complicated however, owing to
            // lifecycle issues for the various SignalObservers, and the common locking structure of
            // PeeredDispatchers. See https://fxbug.dev/42050802. TL;DR - someday, when we have had
            // the time to carefully refactor the locking here, come back and remove the use of
            // CriticalMutex.
            ksync::lock!(let mut guard = state.peered.lock());

            // See write() for an explanation of this test.
            // SAFETY: Holding the peer holder lock (`peered`) is sufficient to read `owner`
            // (writing requires holding both the peer holder lock and `channel_lock`).
            if owner != unsafe { *state.owner.get() } {
                core::hint::cold_path();
                // SAFETY: Called on the owning thread while holding `peered.lock()`.
                let (channel_ref, _) = unsafe { waiter.end_wait_and_take_channel() };
                break 'lock Err((channel_ref, Status::BAD_HANDLE));
            }

            let Some(peer) = this.peer(&guard) else {
                core::hint::cold_path();
                // SAFETY: Called on the owning thread while holding `peered.lock()`.
                let (channel_ref, _) = unsafe { waiter.end_wait_and_take_channel() };
                break 'lock Err((channel_ref, Status::PEER_CLOSED));
            };

            let txid = this.allocate_txid_locked(guard.as_mut().token_mut());

            // Install our txid in the waiter and the outbound message.
            waiter.set_txid(txid);
            msg.set_txid(txid);

            this.trace_message(&msg, MessageOp::ChannelCallWriteRequest);

            // (0) Before writing the outbound message and waiting, add our waiter to the list.
            // SAFETY: `waiter` is embedded in the current `ThreadDispatcher`, which outlives this
            // call.
            unsafe {
                this.waiters_mut(guard.as_mut().token_mut()).push_back_raw(waiter.as_non_null());
            }

            // (1) Write outbound message to opposing endpoint.
            peer.write_self_locked(guard.as_mut().token_mut(), msg, Some(&waiter.wait_queue));

            // Disable eager rescheduling before releasing `peered.lock()` (`CriticalMutex`) so that
            // `clear_timeslice_extension` cannot prematurely preempt the caller before it enters
            // `waiter.wait()`.
            Ok(AnnotatedAutoEagerReschedDisabler::new())
        };
        let eager_resched_disabler = match lock_res {
            Ok(disabler) => disabler,
            Err((channel_ref, status)) => {
                core::hint::cold_path();
                drop(channel_ref);
                return Err(status);
            }
        };

        // Reuse the code from the half-call used for retrying a Call after thread suspend.
        let (channel_ref, end_status) =
            this.resume_interrupted_call_inner(&waiter, deadline, Some(eager_resched_disabler));
        drop(channel_ref);
        end_status
    }

    /// Kills the current process after a thread tried to begin a second wait, dropping `channel`,
    /// and returns the status for `call` to report.
    ///
    /// Kept out of line so that the `RefPtr` drop and process kill do not bloat the hot path of
    /// `call`.
    #[cold]
    #[inline(never)]
    fn kill_vdso_violator(channel: RefPtr<Self>) -> Status {
        drop(channel);
        // If a thread tries BeginWait'ing twice, the VDSO contract around retrying channel
        // calls has been violated. Shoot the misbehaving process.
        ProcessDispatcher::get_current().kill(ZX_TASK_RETCODE_VDSO_KILL);
        Status::BAD_STATE
    }

    /// Performs the wait-then-read half of `call`. This is meant for retrying after an interruption
    /// caused by suspending.
    ///
    /// Takes `&CurrentMessageWaiter` (rather than `&MessageWaiter`) so that only the current
    /// thread's own `MessageWaiter` can be passed, since `waiter.channel` and `waiter.result` are
    /// accessed without `peered.lock()` on the owning thread.
    #[inline]
    pub fn resume_interrupted_call(
        waiter: &CurrentMessageWaiter,
        deadline: Deadline,
    ) -> Result<MessagePacketPtr, Status> {
        // SAFETY: `waiter` is the current thread's `MessageWaiter`.
        let Some(this) = (unsafe { waiter.channel_ptr() }) else {
            core::hint::cold_path();
            return Err(Status::BAD_STATE);
        };
        // SAFETY: `waiter` is the current thread's `MessageWaiter` (`CurrentMessageWaiter` is
        // `!Send`/`!Sync`), and `waiter.channel` is only ever mutated on the current thread
        // (`begin_wait`, `end_wait_and_take_channel`, or `cancel_pending_wait` in
        // `ThreadDispatcher::exiting_current`). `waiter.channel` keeps `this` alive until
        // `resume_interrupted_call_inner` either moves the `RefPtr<ChannelDispatcher>` into
        // `channel_ref` (keeping `this` alive until `drop(channel_ref)` below) or leaves it in
        // `waiter.channel` on `INTERRUPTED_RETRY`.
        let (channel_ref, end_status) = unsafe {
            let this = this.as_ref();
            this.state().canary.assert();
            this.resume_interrupted_call_inner(waiter, deadline, None)
        };
        drop(channel_ref);
        end_status
    }

    #[inline]
    fn resume_interrupted_call_inner(
        &self,
        waiter: &CurrentMessageWaiter,
        deadline: Deadline,
        eager_resched_disabler: Option<AnnotatedAutoEagerReschedDisabler>,
    ) -> (Option<RefPtr<Self>>, Result<MessagePacketPtr, Status>) {
        // SAFETY: `waiter` is the current thread's `MessageWaiter`.
        debug_assert!(unsafe { waiter.channel_ptr() } == Some(NonNull::from(self)));
        // (2) Wait for notification via waiter's event or for the deadline to hit.
        let wait_res = {
            let _auto_blocked = AutoBlocked::new(Blocked::CHANNEL);
            // SAFETY: `waiter` is embedded in the calling thread's `ThreadDispatcher`, and the
            // calling thread holds no spinlocks or chain locks and is in a valid state to block.
            unsafe { waiter.wait(deadline, Interruptible::Yes, eager_resched_disabler) }
        };

        // (3) see (3A), (3B) in on_zero_handles_locked/on_peer_zero_handles_locked or (3C) in
        // try_write_to_message_waiter for paths where the waiter could be signaled and removed from
        // the list.
        //
        // If the deadline hits, the waiter is not removed from the list *but* another thread could
        // still cause (3A), (3B), or (3C) before the lock in `cancel_or_complete_wait`.
        let (channel_ref, end_status) = match wait_res {
            Ok(()) => {
                // Fast path: `signal()` already unlinked `waiter` from `waiters`, stored
                // `waiter.result`, cleared `waiter.wait_queue`'s owner, and released
                // `waiter.wait_queue.lock()` before waking us.
                // SAFETY: Called on the owning thread after `wait()` returned `Ok(())`.
                unsafe { waiter.end_wait_and_take_channel() }
            }
            Err(Status::INTERRUPTED_RETRY) => {
                core::hint::cold_path();
                // If we got interrupted, return out to usermode, but do not clear the waiter.
                return (None, Err(Status::INTERRUPTED_RETRY));
            }
            // SAFETY: `waiter` is the current thread's `MessageWaiter`, which was queued on `self`
            // by `call` (`waiter.channel` refers to `self`) and remains allocated while the current
            // thread executes. The current thread holds no spinlocks or chain locks and is in a
            // valid state to block.
            Err(_) => unsafe { self.cancel_or_complete_wait(waiter) },
        };

        if let Ok(ref packet) = end_status {
            self.trace_message(packet, MessageOp::ChannelCallReadResponse);
        }

        (channel_ref, end_status)
    }

    /// Attempt to deliver the message to a waiting `MessageWaiter`.
    ///
    /// On match, removes the waiter from `waiters`, writes `Ok(msg)` into `waiter.result`, and
    /// returns `Ok(waiter)` so the caller can invoke `waiter.signal()` after releasing
    /// `peered.lock()`. Otherwise returns `Err(msg)`.
    #[inline]
    fn try_write_to_message_waiter(
        &self,
        token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
        msg: MessagePacketPtr,
    ) -> Result<NonNull<MessageWaiter>, MessagePacketPtr> {
        let state = self.state();
        state.canary.assert();

        // SAFETY: `token` belongs to the `PeerHolder` mutex synchronizing `self`.
        let g = unsafe { state.guard_mu(token) };
        let waiters = g.waiters();
        if waiters.is_empty() {
            return Err(msg);
        }

        // If the far side has "call" waiters waiting for replies, see if this message's txid
        // matches one of them. If so, deliver it. Note, because callers use a kernel generated txid
        // we can skip checking the list if this message's txid isn't kernel generated.
        let txid = msg.get_txid();
        if !is_kernel_generated_txid(txid) {
            return Err(msg);
        }

        if let Some(waiter_ptr) = self.waiters_mut(token).erase_if(|w| w.get_txid() == txid) {
            // (3C) Deliver message to waiter. Signaling happens after `peered.lock()` is released
            // in `ChannelDispatcher::write`.
            // SAFETY: `waiter_ptr` was just removed from `waiters` while holding `token`, and has
            // not yet been delivered or canceled for the current wait.
            unsafe {
                MessageWaiter::deliver_locked(waiter_ptr, token, msg);
            }
            return Ok(waiter_ptr);
        }

        Err(msg)
    }

    /// Queues a message on this channel endpoint and notifies observers.
    ///
    /// Must be called with the peer holder lock held.
    #[inline]
    fn write_self_locked(
        &self,
        token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
        msg: MessagePacketPtr,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        let state = self.state();
        state.canary.assert();

        // Once we've acquired the channel_lock we're going to make a copy of the previously active
        // signals and raise the READABLE signal before dropping the lock. After we've dropped the
        // lock, we'll notify observers using the previously active signals plus READABLE.
        //
        // There are several things to note about this sequence:
        //
        // 1. We must hold channel_lock while updating the stored signals (raise_signals_locked) to
        // synchronize with thread adding, removing, or canceling observers otherwise we may create
        // a spurious READABLE signal (see NoSpuriousReadableSignalWhenRacing test).
        //
        // 2. We must release the channel_lock before notifying observers to ensure that Read can
        // execute concurrently with notify_observers_locked_with_queue, which is a potentially long
        // running call.
        //
        // 3. We can skip the call to notify_observers_locked_with_queue if the previously active
        // signals contained READABLE (because there can't be any observers still waiting for
        // READABLE if that signal is already active).
        let previous_signals = {
            ksync::lock!(let mut g = state.lock_channel_lock());
            let size = {
                let mut messages = g.as_mut().messages_mut();
                messages.as_mut().push_back(msg);
                messages.len()
            };
            let previous_signals = self.raise_signals_locked(token, ZX_CHANNEL_READABLE);
            let max_count = cmp::max(size as u64, *g.max_message_count());
            *g.as_mut().max_message_count_mut() = max_count;

            // TODO(cpu): Remove this hack. See comment in MAX_PENDING_MESSAGE_COUNT definition.
            if size >= WARN_PENDING_MESSAGE_COUNT {
                self.check_message_count(token, size);
            }
            previous_signals
        };

        // Don't bother waking observers if ZX_CHANNEL_READABLE was already active.
        if (previous_signals & ZX_CHANNEL_READABLE) == 0 {
            self.notify_observers_locked_with_queue(
                token,
                previous_signals | ZX_CHANNEL_READABLE,
                queue_to_own,
            );
        }
    }

    #[cold]
    fn check_message_count(
        &self,
        token: &ksync::LockToken<'_, PeerHolderMuClass<Self>>,
        size: usize,
    ) {
        // TODO(cpu): Remove this hack. See comment in MAX_PENDING_MESSAGE_COUNT definition.
        if size != WARN_PENDING_MESSAGE_COUNT && size <= MAX_PENDING_MESSAGE_COUNT {
            return;
        }
        let mut pname = [0u8; zx_types::ZX_MAX_NAME_LEN];
        debug_assert!(ProcessDispatcher::get_current().get_name(&mut pname).is_ok());
        let len = pname.iter().position(|&b| b == 0).unwrap_or(pname.len());
        let name = str::from_utf8(&pname[..len]).unwrap_or("<unknown>");
        // SAFETY: Holding the peer holder lock (`token`) is sufficient to read the peer's `owner`.
        let peer_owner = unsafe {
            *self.state().peered.guard_mu(token).peer().as_ref().unwrap().state().owner.get()
        };
        if size == WARN_PENDING_MESSAGE_COUNT {
            kprint::kprintln!(
                "KERN: warning! channel ({:u}) has {:u} messages ({:s}) (peer: {:u}) (write).",
                self.get_koid(),
                size,
                name,
                peer_owner
            );
        } else {
            kprint::kprintln!(
                "KERN: channel ({:u}) has {:u} messages ({:s}) (peer: {:u}) (write). Raising exception.",
                self.get_koid(),
                size,
                name,
                peer_owner
            );
            signal_policy_exception(ZX_EXCP_POLICY_CODE_CHANNEL_FULL_WRITE, 0);
            CHANNEL_FULL.add(1);
        }
    }

    /// Generate a unique txid to be used in a channel call.
    #[inline]
    fn allocate_txid_locked(
        &self,
        token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
    ) -> u32 {
        // SAFETY: `token` belongs to the `PeerHolder` mutex synchronizing `self`.
        let mut g = unsafe { self.state().guard_mu_mut(token) };
        let mut txid = *g.txid();
        loop {
            // Values 1..MIN_KERNEL_GENERATED_TXID are reserved for userspace.
            txid = txid.wrapping_add(1);
            let candidate = txid | MIN_KERNEL_GENERATED_TXID;
            // If there are waiting messages, ensure we have not allocated a txid that's already in
            // use. This is unlikely. It's atypical for multiple threads to be invoking
            // channel_call() on the same channel at once, so the waiter list is most commonly
            // empty.
            if g.waiters().forward_iter().all(|w| w.get_txid() != candidate) {
                *g.txid_mut() = txid;
                return candidate;
            }
        }
    }

    /// Cancels any channel_call message waiters waiting on this endpoint.
    pub(super) fn cancel_message_waiters(&self) {
        ksync::lock!(let mut guard = self.state().peered.lock());
        self.cancel_message_waiters_locked(guard.as_mut().token_mut(), Status::CANCELED);
    }

    /// Cancels (with `status`) any channel_call message waiters waiting on this endpoint.
    fn cancel_message_waiters_locked(
        &self,
        token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
        status: Status,
    ) {
        self.state().canary.assert();
        while let Some(waiter_ptr) = self.waiters_mut(token).pop_front() {
            // SAFETY: `waiter_ptr` was placed into `waiters` during `call()`, was just removed from
            // `waiters` while holding `token`, has not yet been delivered or canceled for the
            // current wait, and remains valid until `cancel_locked` sets `waiter.signaled = true`
            // and releases `waiter.wait_queue.lock()`. `status` is never `TIMED_OUT`.
            unsafe {
                MessageWaiter::cancel_locked(waiter_ptr, token, status);
            }
        }
    }

    /// Removes `waiter` from this channel's `waiters` list (if not already removed by delivery or
    /// cancellation), synchronizes with any in-flight `signal()`, and returns the taken channel
    /// reference and final status.
    ///
    /// # Safety
    ///
    /// - `waiter` must be the calling thread's own `MessageWaiter` (embedded in its
    ///   `ThreadDispatcher`), so that no other thread can concurrently take `waiter.channel` or
    ///   `waiter.result`, and `waiter` must remain allocated until this returns.
    /// - If `waiter.channel` is `Some`, it must refer to `self` (that is, `waiter` was queued on
    ///   `self`'s `waiters` by `ChannelDispatcher::call`, not on a different `ChannelDispatcher`).
    /// - The calling thread must hold no spinlocks or chain locks and be in a valid state to block,
    ///   since this may block in `wait_until_signaled()`.
    #[cold]
    unsafe fn cancel_or_complete_wait(
        &self,
        waiter: &MessageWaiter,
    ) -> (Option<RefPtr<Self>>, Result<MessagePacketPtr, Status>) {
        let state = self.state();
        state.canary.assert();
        let (channel_ref, end_status, was_in_waiters) = {
            ksync::lock!(let mut guard = state.peered.lock());

            // (4) If any of (3A), (3B), or (3C) have occurred, a concurrent writer or canceler
            // already removed `waiter` from the waiters list (and stored a non-TIMED_OUT status in
            // `waiter.result`) under `peered.lock()`. Otherwise, `waiter` is still linked in the
            // waiters list and it is our job to remove it.
            // SAFETY: Called on the owning thread while holding `peered.lock()`.
            let (channel_ref, end_status) = unsafe { waiter.end_wait_and_take_channel() };
            let was_in_waiters = waiter.node.in_container();
            if was_in_waiters {
                debug_assert!(matches!(end_status, Err(Status::TIMED_OUT)));
                // SAFETY: `waiter` was added to `self.waiters` during `call` (per the caller,
                // `waiter` was queued on `self`), and `waiter.node.in_container()` confirmed it is
                // still linked in `self.waiters` while holding `peered.lock()`.
                unsafe {
                    let _ = self.waiters_mut(guard.as_mut().token_mut()).erase(waiter);
                }
            }
            (channel_ref, end_status, was_in_waiters)
        };
        if channel_ref.is_some() {
            if was_in_waiters {
                // No `signal()` can be in flight because `waiter` was still linked in `waiters`
                // under `peered.lock()`. `INTERRUPTED_RETRY` returns before reaching
                // `cancel_or_complete_wait`, so this only runs once the wait has terminally ended
                // (timeout, kill, or thread exit), where resetting any residual owner is always
                // desired.
                waiter.wait_queue.reset_owner_if_no_waiters();
            } else {
                // `deliver_locked` and `cancel_locked` always overwrite `waiter.result` with a
                // non-`TIMED_OUT` status under `peered.lock()` right after unlinking `waiter`.
                debug_assert!(!matches!(end_status, Err(Status::TIMED_OUT)));
                // A concurrent writer or canceler already unlinked `waiter` from `waiters` under
                // `peered.lock()`. Wait until its `signal()` call has set `waiter.signaled = true`
                // and released `waiter.wait_queue.lock()` before allowing `waiter` to be reused or
                // dropped.
                // SAFETY: `waiter` remains allocated until this returns, the calling thread holds
                // no spinlocks or chain locks and is in a valid state to block (per the caller),
                // and the concurrent writer or canceler that unlinked `waiter` has called or will
                // call `signal()` for the current wait.
                unsafe { waiter.wait_until_signaled() };
            }
        }
        (channel_ref, end_status)
    }

    /// Sets the owning process KOID for this channel endpoint.
    pub(super) fn set_owner(&self, new_owner: zx_koid_t) {
        // Testing for ZX_KOID_INVALID is an optimization so we don't pay the cost of grabbing the
        // lock when the endpoint moves from the process to channel; the one that we must get right
        // is from channel to new owner.
        if new_owner == zx_types::ZX_KOID_INVALID {
            return;
        }

        let state = self.state();
        state.canary.assert();
        ksync::lock!(state.peered.lock());
        ksync::lock!(state.channel_lock.lock());
        // SAFETY: Both the peer holder lock (`peered`) and `channel_lock` are held, giving
        // exclusive access to `owner`.
        unsafe {
            *state.owner.get() = new_owner;
        }
    }

    /// Returns whether the peer endpoint has closed.
    ///
    /// Locking this endpoint's `channel_lock` is sufficient because `peer_has_closed` is set on
    /// this endpoint under `channel_lock` when the peer drops its handles.
    pub(super) fn peer_has_closed(&self) -> bool {
        let state = self.state();
        state.canary.assert();
        ksync::lock!(let guard = state.lock_channel_lock());
        *guard.peer_has_closed()
    }

    /// Returns the current and maximum pending message counts for this endpoint.
    pub(super) fn get_message_counts(&self) -> (usize, u64) {
        let state = self.state();
        state.canary.assert();
        ksync::lock!(let g = state.lock_channel_lock());
        (g.messages().len(), *g.max_message_count())
    }

    #[inline]
    fn peer<'a>(
        &'a self,
        guard: &PeeredStateMuGuard<'_, Self, TrackedCriticalMutex>,
    ) -> Option<&'a Self> {
        // SAFETY: `guard` holds the shared `PeerHolder` lock, so the peer `RefPtr` in `guard`
        // cannot be cleared or dropped while `guard` is alive.
        guard.peer().as_deref().map(|p| unsafe { &*(p as *const Self) })
    }

    #[inline]
    fn waiters_mut<'a>(
        &'a self,
        token: &'a mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
    ) -> Pin<&'a mut DoublyLinkedList<NonNull<MessageWaiter>>> {
        // SAFETY: `token` witnesses that `mu` is held, giving exclusive access to `waiters`. The
        // list is a pinned field of the pinned dispatcher state, so it is never moved.
        unsafe { self.state().waiters.get_pinned_mut_locked(token) }
    }

    // PeeredDispatcher implementation.
    fn on_zero_handles_locked(&self, token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>) {
        self.state().canary.assert();

        // (3A) Abort any waiting Call operations because we've been canceled by reason of our local
        // handle going away.
        self.cancel_message_waiters_locked(token, Status::CANCELED);
    }

    // This requires holding the shared channel lock. The thread analysis can reason about repeated
    // calls to get_lock() on the shared object, but cannot reason about the aliasing between
    // left->get_lock() and right->get_lock(), which occurs above in on_zero_handles.
    fn on_peer_zero_handles_locked(
        &self,
        token: &mut ksync::LockToken<'_, PeerHolderMuClass<Self>>,
    ) {
        self.state().canary.assert();
        {
            let state = self.state();
            ksync::lock!(let mut guard = state.lock_channel_lock());
            *guard.as_mut().peer_has_closed_mut() = true;
        }
        self.update_state_locked(token, ZX_CHANNEL_WRITABLE, ZX_CHANNEL_PEER_CLOSED);
        // (3B) Abort any waiting Call operations because we've been canceled by reason of the
        // opposing endpoint going away.
        self.cancel_message_waiters_locked(token, Status::PEER_CLOSED);
    }

    /// Emits `kernel:ipc` duration and flow trace events (`ChannelMessage` / `ChannelFlow`) for a
    /// channel message operation when IPC tracing is enabled.
    #[inline]
    fn trace_message(&self, msg: &MessagePacket, op: MessageOp) {
        if category_enabled!("kernel:ipc") {
            self.trace_message_slow(msg, op);
        }
    }

    #[cold]
    fn trace_message_slow(&self, msg: &MessagePacket, op: MessageOp) {
        // We emit these trace events non-standardly to work around some compatibility issues:
        //
        // 1) We partially inline the trace macro so that we can purposely emit 0-length durations.
        //
        //    chrome://tracing requires flow events to be contained in a duration. Perfetto requires
        //    flow events to be attached to a "slice". However, the Perfetto viewer treats instant
        //    events as 0-length slices. This means that we can assign flows to them, and they get a
        //    special easy to click on arrow instead of a tiny duration bar. Using a 0-length
        //    duration gets us nice instant events in the Perfetto viewer, while still supporting
        //    flows in chrome://tracing.
        //
        // 2) Even though we know exactly when the duration ends, we emit a Begin/End pair instead
        //    of using a duration-complete event.
        //
        //    Because we do so little work between creating the duration-complete scope and then
        //    emitting the flow event, if we emit a duration-complete event, the two events may be
        //    created with the same timestamp. Since the duration-complete event is only written
        //    when the scope ends, it is written _after_ the flow event in the trace, causing the
        //    flow to be associated with the previous event, not it. By using a Begin/End pair, we
        //    ensure that though the events have the same timestamp, they will be read in the
        //    correct order and the flow events will be associated correctly.
        let payload = msg.start_of_payload();
        let header = FidlHeader::read_from_prefix(payload).map(|(h, _)| h).unwrap_or_default();
        let txid = header.txid;
        let ordinal = header.ordinal;

        let ts = KTrace::timestamp();

        if cfg!(channel_message_body_tracing_enabled)
            && matches!(op, MessageOp::Write | MessageOp::ChannelCallWriteRequest)
        {
            let num_handles = cmp::min(msg.num_handles(), MAX_TRACE_HANDLES);
            let mut handle_info = [MaybeUninit::<RawInfoHandleBasic>::uninit(); MAX_TRACE_HANDLES];
            for (info, handle_ref) in handle_info.iter_mut().zip(msg_handle_refs(msg)) {
                let disp = handle_ref.dispatcher_ref();
                info.write(RawInfoHandleBasic {
                    koid: disp.get_koid(),
                    rights: handle_ref.rights(),
                    type_: disp.get_type(),
                    related_koid: disp.get_related_koid(),
                    reserved: 0,
                    padding1: [0; 4],
                });
            }
            // SAFETY: The first `num_handles` elements of `handle_info` were initialized above.
            let handle_info = unsafe {
                slice::from_raw_parts(
                    handle_info.as_ptr().cast::<RawInfoHandleBasic>(),
                    num_handles,
                )
            };
            let handle_bytes = IntoBytes::as_bytes(handle_info);
            // Record message body when sending.
            duration_begin_timestamp!(
                "kernel:ipc",
                "ChannelMessage",
                ts,
                "ordinal" => ordinal,
                "bytes" => payload,
                "handles" => handle_bytes,
            );
        } else {
            // Don't record message body when receiving.
            duration_begin_timestamp!("kernel:ipc", "ChannelMessage", ts, "ordinal" => ordinal);
        }

        // When the txid is kernel-generated, Read and Write message ops are just steps in the
        // overall flow that is bounded by ChannelCallWriteRequest and ChannelCallReadResponse
        // message ops.
        let flow_id = channel_message_flow_id(msg, txid, self);
        match op {
            MessageOp::Write | MessageOp::Read if is_kernel_generated_txid(txid) => {
                flow_step_timestamp!("kernel:ipc", "ChannelFlow", ts, flow_id);
            }
            MessageOp::Write | MessageOp::ChannelCallWriteRequest => {
                flow_begin_timestamp!("kernel:ipc", "ChannelFlow", ts, flow_id);
            }
            MessageOp::Read | MessageOp::ChannelCallReadResponse => {
                flow_end_timestamp!("kernel:ipc", "ChannelFlow", ts, flow_id);
            }
        }

        duration_end_timestamp!("kernel:ipc", "ChannelMessage", ts);
    }
}

/// Per-thread structure used while waiting in a `ChannelDispatcher::call`.
///
/// While enqueued in a `ChannelDispatcher`'s `waiters` list, `MessageWaiter`'s state is guarded by
/// the lock of the owning `ChannelDispatcher`, and `deliver_locked()` and `cancel_locked()` must
/// only be called under that lock after removing the waiter from `waiters`.
///
/// `MessageWaiter`s are embedded in `ThreadDispatcher`s, and the `channel` pointer can only be
/// manipulated by their thread (via `begin_wait()` or `end_wait_and_take_channel()`, including from
/// `cancel_pending_wait()` in `ThreadDispatcher::exiting_current`, which guarantees that `channel`
/// is `None` before the `ThreadDispatcher` is destroyed).
/// Once `signal()` sets `signaled = true` and releases `wait_queue.lock()` before unblocking the
/// waiting thread, no other thread holds a reference to the waiter, allowing
/// `end_wait_and_take_channel()` to run without reacquiring the `ChannelDispatcher`'s lock when
/// `wait()` returns `Ok(())`, and allowing `cancel_or_complete_wait()` to synchronize with an
/// in-flight `signal()` via `wait_until_signaled()`.
///
/// See also: comments in `ChannelDispatcher::call()`.
#[derive(DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct MessageWaiter {
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<MessageWaiter>,
    channel: UnsafeCell<Option<RefPtr<ChannelDispatcher>>>,
    result: UnsafeCell<Result<MessagePacketPtr, Status>>,
    // TODO(teisenbe/swetland): Investigate hoisting this outside to reduce userthread size.
    #[pin]
    wait_queue: OwnedWaitQueue,
    // Guarded by `wait_queue`'s lock (or accessed on the owning thread in `begin_wait` before
    // insertion).
    signaled: UnsafeCell<bool>,
    txid: UnsafeCell<zx_txid_t>,
}

impl MessageWaiter {
    /// In-place initialization for `MessageWaiter` called during `ThreadDispatcher` construction.
    pub(super) fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            node: DoublyLinkedListNode::new(),
            channel: UnsafeCell::new(None),
            result: UnsafeCell::new(Err(Status::BAD_STATE)),
            wait_queue <- OwnedWaitQueue::init(),
            signaled: UnsafeCell::new(false),
            txid: UnsafeCell::new(0),
        })
    }

    /// Begins a wait on `channel`.
    #[inline]
    fn begin_wait(
        &self,
        channel: RefPtr<ChannelDispatcher>,
    ) -> Result<(), RefPtr<ChannelDispatcher>> {
        // SAFETY: `begin_wait` is only called on the owning thread (`ThreadDispatcher`).
        // - `self.channel` is verified to be `None`, and `self.result` is always `Err` when
        //   `channel` is `None` (initialized to `Err(BAD_STATE)` and reset to `Err(BAD_STATE)` by
        //   `end_wait_and_take_channel`), so neither holds a droppable value.
        // - `self` is not yet in any channel's `waiters` list and its `wait_queue` has not been
        //   passed to `write_self_locked`, so no other thread can concurrently access
        //   `self.signaled` or `self.wait_queue`.
        unsafe {
            if (*self.channel.get()).is_some() {
                core::hint::cold_path();
                return Err(channel);
            }
            debug_assert!(!self.node.in_container());
            debug_assert!((*self.result.get()).is_err());

            ptr::write(self.result.get(), Err(Status::TIMED_OUT));
            ptr::write(self.channel.get(), Some(channel));
            *self.signaled.get() = false;
        }
        Ok(())
    }

    /// Ends the wait on the owning thread, returning the taken `channel` reference and the
    /// delivered message/status.
    ///
    /// # Safety
    ///
    /// Must be called on the owning thread (the thread whose `ThreadDispatcher` embeds `self`),
    /// either:
    /// - after `wait()` returned `Ok(())`, where `signal()` already unlinked `self` from `waiters`,
    ///   wrote `self.result`, and released `self.wait_queue.lock()` before waking the owning
    ///   thread, or
    /// - while holding the `peered.lock()` of the `ChannelDispatcher` that `self.channel` refers
    ///   to, which synchronizes access to `channel` and `result` with `deliver_locked` and
    ///   `cancel_locked`.
    #[inline]
    unsafe fn end_wait_and_take_channel(
        &self,
    ) -> (Option<RefPtr<ChannelDispatcher>>, Result<MessagePacketPtr, Status>) {
        // SAFETY: Forwarded from the caller (see `# Safety`).
        unsafe {
            let Some(channel) = (*self.channel.get()).take() else {
                core::hint::cold_path();
                return (None, Err(Status::BAD_STATE));
            };
            (Some(channel), mem::replace(&mut *self.result.get(), Err(Status::BAD_STATE)))
        }
    }

    /// Waits on the internal wait queue until deadline or signaled.
    ///
    /// Returns the outcome of waiting on the queue (such as `Ok(())`, or `Err(Status::TIMED_OUT)`).
    /// Note that `wait()` only returns the wait queue outcome, while `end_wait_and_take_channel()`
    /// retrieves the delivered status.
    ///
    /// # Safety
    ///
    /// - `self` and `self.wait_queue` must remain alive for the entire duration of the wait.
    /// - The calling thread must hold no spinlocks or chain locks and be in a valid state to block.
    #[inline]
    unsafe fn wait(
        &self,
        deadline: Deadline,
        interruptible: Interruptible,
        mut eager_resched_disabler: Option<AnnotatedAutoEagerReschedDisabler>,
    ) -> Result<(), Status> {
        assert_no_transaction_active();
        // TODO(https://fxbug.dev/477068635): Consider merging this logic back into OwnedWaitQueue.
        // SAFETY: Multi-threading is initialized when blocking a thread on a channel call.
        let current_thread: &Thread = unsafe { ThreadPtr::current().as_ref() };
        let do_transaction =
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                // Hand off eager-resched suppression to the transaction's state saver so that any
                // backoff (`relax()`) properly reenables preemption while spinning.
                drop(eager_resched_disabler.take());
                // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables eager rescheduling,
                // which disables preemption for the duration of this transaction.
                let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(&self.wait_queue, token);
                // SAFETY: `guard` holds `self.wait_queue.lock()`, which guards `self.signaled`.
                if unsafe { *self.signaled.get() } {
                    core::hint::cold_path();
                    guard.release();
                    return TransactionResult::Value(Ok(()));
                }
                let new_owner = guard.proof().owner_thread();
                // SAFETY: `new_owner` (if `Some`) is the live owner of `self.wait_queue`, which
                // cannot change or exit while `guard` holds `self.wait_queue.lock()`.
                let new_owner_ref = unsafe { new_owner.map(|t| t.as_ref()) };
                // SAFETY: `guard` holds `self.wait_queue.lock()`, the transaction is not finalized,
                // `current_thread` is the running thread, and no other chain locks are held.
                if let Some((details, mut current_thread_guard)) = unsafe {
                    guard.proof().try_lock_for_baao_operation_locked(
                        current_thread,
                        new_owner_ref,
                        token,
                    )
                } {
                    ChainLockTransaction::finalize(token);
                    // TODO(https://fxbug.dev/42182908): Once fair-to-fair priority inheritance is
                    // implemented, change this to `ForceInheritance::Yes`.
                    // SAFETY: The transaction is finalized with the BAAO locks held, and the caller
                    // guarantees that `current_thread` is in a valid state to block and
                    // `self.wait_queue` remains alive for the entire call.
                    let result = unsafe {
                        OwnedWaitQueue::block_and_assign_owner_locked(
                            guard,
                            &mut current_thread_guard.proof(),
                            &deadline,
                            &details,
                            ResourceOwnership::Normal,
                            interruptible,
                            ForceInheritance::No,
                            token,
                            &preempt_disabled_token,
                        )
                    };
                    current_thread_guard.release();
                    return TransactionResult::Value(result);
                }
                guard.release();
                core::hint::cold_path();
                TransactionResult::BACKOFF
            };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::Wait"),
            do_transaction,
        )
    }

    /// Signals this waiter and wakes any thread blocked in `wait()` or `wait_until_signaled()`.
    ///
    /// Takes `this: NonNull<Self>` rather than `&self` because the owning `ThreadDispatcher` may
    /// complete its wait (or finish `wait_until_signaled()` in `cancel_or_complete_wait`) and exit
    /// on another CPU as soon as `wait_queue.lock()` is released and/or the waiter thread is
    /// unblocked, before `signal` returns.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `MessageWaiter` that was just unlinked from its channel's
    /// `waiters` list after writing `this.result` (`deliver_locked` or `cancel_locked`), and has
    /// not yet had `signal` called for the current wait.
    unsafe fn signal(
        this: NonNull<Self>,
        mut eager_resched_disabler: Option<AnnotatedAutoEagerReschedDisabler>,
    ) {
        assert_no_transaction_active();
        // TODO(https://fxbug.dev/477068635): Consider merging this logic back into OwnedWaitQueue.
        let this_ptr = this.as_ptr();
        let mut wake_hooks = OwnedWaitQueue::default_wake_hooks();
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // Hand off eager-resched suppression to the transaction's state saver so that any
            // backoff (`relax()`) properly reenables preemption while spinning.
            drop(eager_resched_disabler.take());
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables eager rescheduling,
            // which disables preemption for the duration of this transaction.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            let waiter = {
                // Scope `wait_queue` and `guard` inside this block so no reference into `this` or
                // `OwnedWaitQueue` remains live after `dequeue_one_and_release_locked` releases
                // `wait_queue.lock()`.
                // SAFETY: `this_ptr` remains valid at least until `this.signaled` is set to `true`
                // and `wait_queue.lock()` is released by `dequeue_one_and_release_locked` below.
                let wait_queue: &OwnedWaitQueue = unsafe { &(*this_ptr).wait_queue };
                // SAFETY: The transaction is not finalized and holds no locks (so neither
                // `wait_queue.lock()` nor any lock in its downstream PI chain or waiter threads is
                // held), and no proof or guard for any waiter thread in `wait_queue` or any node in
                // its downstream PI chain is live. On success, `(guard, waiter)` are consumed by
                // `dequeue_one_and_release_locked` and `scheduler::unblock` below after finalizing
                // the transaction.
                let Some((guard, waiter)) =
                    (unsafe { wait_queue.lock_for_wake_one_or_backoff(&mut wake_hooks, token) })
                else {
                    core::hint::cold_path();
                    return TransactionResult::BACKOFF;
                };
                ChainLockTransaction::finalize(token);
                // SAFETY: `guard` holds `wait_queue.lock()`, which guards `signaled`.
                unsafe {
                    *(*this_ptr).signaled.get() = true;
                }
                // TODO(https://fxbug.dev/42182908): Once fair-to-fair priority inheritance is
                // implemented, change this to `ForceInheritance::Yes`.
                // SAFETY: `(guard, waiter)` is the unmodified result of
                // `lock_for_wake_one_or_backoff` in this transaction, and the transaction is
                // finalized. No proof or guard for `wait_queue`, its downstream PI chain, or
                // `waiter` is live, and `waiter` (if any) is unblocked below. `WakeOption::None` is
                // passed because `MessageWaiter` is per-thread (at most one waiter) and `signal()`
                // leaves `wait_queue` unowned.
                unsafe {
                    OwnedWaitQueue::dequeue_one_and_release_locked(
                        guard,
                        waiter,
                        &mut wake_hooks,
                        WakeOption::None,
                        ForceInheritance::No,
                        token,
                        &preempt_disabled_token,
                    );
                }
                waiter
            };
            // Note: `this_ptr` must not be dereferenced after this point, as another CPU may now
            // observe `signaled == true` and destroy the enclosing `ThreadDispatcher`.
            if let Some(t_ptr) = waiter {
                // SAFETY: `t_ptr`'s ChainLock is still held by the active transaction `token`, so
                // the thread cannot exit until `scheduler::unblock` releases its lock below, and
                // no other proof or guard for it is live.
                let t_guard = unsafe { OwnerGuard::adopt_unchecked(t_ptr.as_ref(), token) };
                // SAFETY: `t_ptr`'s thread was dequeued from `wait_queue` above and the active
                // transaction is finalized.
                unsafe { scheduler::unblock(t_guard, token) };
            }
            TransactionResult::DONE
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::Signal"),
            do_transaction,
        );
    }

    /// Waits until an in-flight `signal()` call sets `self.signaled = true` and releases
    /// `self.wait_queue.lock()`.
    ///
    /// The caller must ensure that a `signal()` call for the current wait has been or will be made
    /// (that is, `self` was already unlinked from `waiters` by a writer or canceler); otherwise
    /// this blocks forever.
    ///
    /// # Safety
    ///
    /// - `self` (including `self.wait_queue`) must remain allocated until this returns.
    /// - The calling thread must hold no spinlocks or chain locks and be in a valid state to block.
    #[cold]
    unsafe fn wait_until_signaled(&self) {
        // SAFETY: Forwarded from the caller.
        let res = unsafe { self.wait(Deadline::infinite(), Interruptible::No, None) };
        debug_assert_eq!(res, Ok(()));
    }

    /// Delivers a reply message to this waiter while holding `peered.lock()`.
    ///
    /// The caller must subsequently invoke `MessageWaiter::signal` after releasing `peered.lock()`.
    ///
    /// # Safety
    ///
    /// - `token` must belong to the `peered.lock()` of the `ChannelDispatcher` whose `waiters`
    ///   list `this` was queued on.
    /// - `this` must point to a valid `MessageWaiter` that was just removed from that `waiters`
    ///   list while holding `token`, and has not yet been delivered or canceled for the current
    ///   wait (so `this.result` holds `Err(Status::TIMED_OUT)` and no droppable value).
    #[inline]
    unsafe fn deliver_locked(
        this: NonNull<Self>,
        _token: &ksync::LockToken<'_, PeerHolderMuClass<ChannelDispatcher>>,
        msg: MessagePacketPtr,
    ) {
        let this_ptr = this.as_ptr();
        // SAFETY: Per the caller, `this` is valid, and holding `peered.lock()` after removing
        // `this` from `waiters` synchronizes access to `node`, `channel`, and `result`.
        // `this.result` was initialized to `Err(Status::TIMED_OUT)` in `begin_wait` and has not yet
        // been delivered/canceled, so it holds no droppable value.
        unsafe {
            debug_assert!(!(*this_ptr).node.in_container());
            debug_assert!((*(*this_ptr).channel.get()).is_some());
            debug_assert!((*(*this_ptr).result.get()).is_err());
            ptr::write((*this_ptr).result.get(), Ok(msg));
        }
    }

    /// Cancels this waiter with `status` and signals the waiting thread.
    ///
    /// # Safety
    ///
    /// - `token` must belong to the `peered.lock()` of the `ChannelDispatcher` whose `waiters`
    ///   list `this` was queued on.
    /// - `this` must point to a valid `MessageWaiter` that was just removed from that `waiters`
    ///   list while holding `token`, and has not yet been delivered or canceled for the current
    ///   wait.
    /// - `status` must not be `Status::TIMED_OUT`, which is reserved for a waiter that is still
    ///   linked in `waiters` (see `cancel_or_complete_wait`).
    #[cold]
    unsafe fn cancel_locked(
        this: NonNull<Self>,
        _token: &ksync::LockToken<'_, PeerHolderMuClass<ChannelDispatcher>>,
        status: Status,
    ) {
        assert!(status != Status::TIMED_OUT);
        let this_ptr = this.as_ptr();
        // SAFETY: Per the caller, `this` is valid, and holding `peered.lock()` after removing
        // `this` from `waiters` synchronizes access to `node`, `channel`, and `result`. `result`
        // was initialized to `Err(Status::TIMED_OUT)` in `begin_wait` and has not yet been
        // delivered/canceled, so it holds no droppable value.
        unsafe {
            debug_assert!(!(*this_ptr).node.in_container());
            debug_assert!((*(*this_ptr).channel.get()).is_some());
            debug_assert!((*(*this_ptr).result.get()).is_err());
            ptr::write((*this_ptr).result.get(), Err(status));
        }
        // SAFETY: `this` points to a valid `MessageWaiter` that was just unlinked from `waiters`
        // after writing `this.result` above, and `signal` has not yet been called for the current
        // wait (it is only called once, by whoever unlinked `this` from `waiters`).
        unsafe {
            Self::signal(this, None);
        }
    }

    /// Returns the transaction ID assigned to this waiter.
    #[inline]
    fn get_txid(&self) -> zx_txid_t {
        // SAFETY: Called under the owning `ChannelDispatcher`'s lock, which synchronizes access
        // with `set_txid`.
        unsafe { *self.txid.get() }
    }

    /// Sets the transaction ID for this waiter.
    #[inline]
    fn set_txid(&self, txid: zx_txid_t) {
        // SAFETY: Called on the owning thread under the owning `ChannelDispatcher`'s lock before
        // inserting `self` into `waiters`.
        unsafe {
            *self.txid.get() = txid;
        }
    }

    /// Returns a pointer to the channel associated with this waiter, if any, without cloning the
    /// `RefPtr`.
    ///
    /// The returned pointer remains valid as long as `self.channel` is not taken (by
    /// `end_wait_and_take_channel`).
    ///
    /// # Safety
    ///
    /// Must be called on the owning thread (the thread whose `ThreadDispatcher` embeds `self`),
    /// since `self.channel` is read without `peered.lock()`.
    #[inline]
    unsafe fn channel_ptr(&self) -> Option<NonNull<ChannelDispatcher>> {
        // SAFETY: `self.channel` is only ever mutated on the owning thread (`begin_wait` and
        // `end_wait_and_take_channel`), and the caller guarantees that this is the owning thread,
        // so reading it cannot race with any concurrent mutation.
        unsafe { (*self.channel.get()).as_ref().map(|c| NonNull::from(&**c)) }
    }

    /// If this waiter still has an active wait (e.g., an interrupted `zx_channel_call` that was
    /// never finished before the thread exited), removes it from the channel's `waiters` list and
    /// waits for any in-flight `signal()` to complete.
    ///
    /// # Safety
    ///
    /// Must only be called on the owning thread as it is exiting
    /// (`ThreadDispatcher::exiting_current`), while holding no spinlocks or chain locks and in a
    /// valid state to block.
    pub(super) unsafe fn cancel_pending_wait(&self) {
        // SAFETY: The caller guarantees this is the owning thread.
        if let Some(channel) = unsafe { self.channel_ptr() } {
            core::hint::cold_path();
            // SAFETY: The caller guarantees this is the owning (exiting) thread, so no other call
            // can take `self.channel`, and `self` remains allocated while it executes.
            // `self.channel` was set to `channel` by `begin_wait` in `ChannelDispatcher::call`,
            // which queued `self` on `channel`'s `waiters`. `channel` remains valid until
            // `cancel_or_complete_wait` returns the taken `RefPtr`. The caller guarantees no
            // spinlocks or chain locks are held and that blocking is allowed.
            let (channel_ref, result) = unsafe { channel.as_ref().cancel_or_complete_wait(self) };
            drop(result);
            drop(channel_ref);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for MessageWaiter {
    fn drop(self: Pin<&mut Self>) {
        // Any pending wait on a started user thread was already canceled in
        // `ThreadDispatcher::exiting_current` before `drop` gained `&mut Self`.
        //
        // `wait_queue` has no owner here either (as `OwnedWaitQueue::teardown` asserts): an owner
        // is only assigned via `write_self_locked(queue_to_own)` in `ChannelDispatcher::call`,
        // and every such wait ends in either `signal()` (whose `WakeOption::None` clears the owner
        // even when no thread is woken) or `reset_owner_if_no_waiters()` in
        // `cancel_or_complete_wait`.
        let this = self.project();
        assert!(this.channel.get_mut().is_none());
        debug_assert!(!this.node.in_container());
    }
}
