// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::fmt;
use core::ptr::NonNull;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::{Arc, Weak};

use fuchsia_async::{PacketReceiver, ReceiverRegistration};
use libasync_sys::{async_dispatcher_t, async_irq_t};
use zx::sys::{ZX_ERR_INVALID_ARGS, ZX_OK, zx_packet_interrupt_t, zx_status_t};
use zx::{Packet, Status};

use crate::ScopeDispatcher;

/// The representation of a C `async_irq_t` pointer.
#[derive(Eq, PartialEq, Hash, Copy, Clone)]
pub struct Irq {
    ptr: NonNull<async_irq_t>,
}

// SAFETY: We own the async_irq_t struct's contents once it's been passed into the dispatcher,
// and we do not modify the contents.
unsafe impl Send for Irq {}
unsafe impl Sync for Irq {}

impl Irq {
    /// Runs the interrupt handler with the appropriate arguments.
    pub fn run(
        &self,
        dispatcher: Arc<ScopeDispatcher>,
        signal: *const zx_packet_interrupt_t,
        status: Result<(), Status>,
    ) {
        // SAFETY: `self.ptr` is a valid pointer by construction, and we assert that the handler
        // is a valid pointer when we accept it for binding and only run handlers that have been
        // bound.
        let callback = unsafe { self.ptr.as_ref().handler.unwrap_unchecked() };

        // SAFETY: The registrant is expected to provide a valid function with the correct signature.
        unsafe {
            callback(
                dispatcher.as_ptr() as *mut _,
                self.ptr.as_ptr(),
                Status::result_into_raw(status),
                signal,
            )
        };
    }
}

impl fmt::Debug for Irq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let async_irq_t { state, handler, object } = unsafe { self.ptr.as_ref() };
        write!(
            f,
            "Irq {{ async_irq@{:?} {{ object: {object:?}, handler: {handler:?}, state: {state:?} }} }}",
            self.ptr
        )
    }
}

type IrqRegistration = ReceiverRegistration<PendingIrq>;

/// Collection of pending IRQs registered on the dispatcher.
#[derive(Default, Debug)]
pub struct PendingIrqs {
    irqs: HashMap<Irq, IrqRegistration>,
}

impl PendingIrqs {
    /// Starts waiting on the given irq
    fn bind(&mut self, dispatcher: &Arc<ScopeDispatcher>, irq: Irq) -> Result<(), Status> {
        if dispatcher.is_shutting_down() {
            return Err(Status::BAD_STATE);
        }

        let Entry::Vacant(entry) = self.irqs.entry(irq) else {
            return Err(Status::ALREADY_BOUND);
        };

        let registration = dispatcher
            .global_handle()
            .register_receiver(PendingIrq { irq, dispatcher: Arc::downgrade(dispatcher) });

        // SAFETY: the irq object is provided by registrant and registration.port() is valid.
        Status::ok(unsafe {
            zx::sys::zx_interrupt_bind(
                irq.ptr.as_ref().object,
                registration.port().raw_handle(),
                registration.key(),
                zx::sys::ZX_INTERRUPT_BIND,
            )
        })
        // We're expected to return NOT_SUPPORTED if the port doesn't have irqs enabled.
        .map_err(|err| if err == Status::WRONG_TYPE { Status::NOT_SUPPORTED } else { err })?;

        entry.insert(registration);
        Ok(())
    }

    /// Unbinds the irq
    fn unbind(&mut self, dispatcher: &ScopeDispatcher, irq: Irq) -> Result<(), Status> {
        if dispatcher.is_shutting_down() {
            return Err(Status::BAD_STATE);
        }

        let Some(bound) = self.irqs.remove(&irq) else {
            return Err(Status::NOT_FOUND);
        };

        // SAFETY: Interrupt and port handles are valid.
        let status = Status::ok(unsafe {
            zx::sys::zx_interrupt_bind(
                irq.ptr.as_ref().object,
                bound.port().raw_handle(),
                0,
                zx::sys::ZX_INTERRUPT_UNBIND,
            )
        });

        if let Err(Status::CANCELED) = status {
            return Ok(());
        }
        status
    }

    /// Drains and unbinds all registered IRQs for shutdown cancellation.
    pub fn drain_all(&mut self) -> Vec<Irq> {
        self.irqs
            .drain()
            .map(|(irq, bound)| {
                // SAFETY: Objects passed to be unbound are valid by construction.
                unsafe {
                    // We don't care if the unbind fails because that would just mean it was already
                    // unbound somehow.
                    zx::sys::zx_interrupt_bind(
                        irq.ptr.as_ref().object,
                        bound.port().raw_handle(),
                        0,
                        zx::sys::ZX_INTERRUPT_UNBIND,
                    )
                };
                irq
            })
            .collect()
    }
}

#[derive(Debug)]
struct PendingIrq {
    irq: Irq,
    dispatcher: Weak<ScopeDispatcher>,
}

impl PacketReceiver for PendingIrq {
    fn receive_packet(&self, packet: Packet) {
        let zx::PacketContents::Interrupt(interrupt) = packet.contents() else {
            panic!("unexpected packet type waiting for interrupt packet");
        };

        let Some(dispatcher) = self.dispatcher.upgrade() else { return };
        let pending = dispatcher.pending_irqs.lock();

        if dispatcher.is_shutting_down() {
            return;
        }

        if !pending.irqs.contains_key(&self.irq) {
            return;
        }

        // we don't want to call the callback with this lock held in case unbind is called
        // inside it.
        drop(pending);

        // Construct standard C interrupt packet for callback.
        let mut packet_signal = zx_packet_interrupt_t::default();
        packet_signal.timestamp = interrupt.timestamp();

        self.irq.run(dispatcher, &packet_signal, Ok(()));
    }
}

/// Begins asynchronously waiting on an IRQ specified in `irq_ptr`.
///
/// The irq's handler will be invoked once for each interrupt packet received,
/// until the irq object is unbound with `unbind_irq` or the dispatcher is shut down.
///
/// # Safety
///
/// The caller must ensure that `dispatcher_ptr` and `irq_ptr` are valid, non-null
/// pointers. The `async_irq_t` structure must remain alive and valid until it is
/// successfully unbound or canceled via dispatcher shutdown.
pub unsafe extern "C" fn bind_irq(
    dispatcher_ptr: *mut async_dispatcher_t,
    irq_ptr: *mut async_irq_t,
) -> zx_status_t {
    let Some(irq_ptr) = NonNull::new(irq_ptr) else {
        return ZX_ERR_INVALID_ARGS;
    };
    // SAFETY: The irq pointer has been asserted valid by the caller.
    let handler = unsafe { irq_ptr.as_ref().handler };
    if handler.is_none() {
        return ZX_ERR_INVALID_ARGS;
    }
    // SAFETY: The caller guarantees `dispatcher_ptr` originates from `ScopeDispatcher::as_ptr`.
    let dispatcher = unsafe { ScopeDispatcher::arc_from_ptr(dispatcher_ptr) };
    let irq = Irq { ptr: irq_ptr };

    if let Err(err) = dispatcher.pending_irqs.lock().bind(&dispatcher, irq) {
        return err.into_raw();
    }
    ZX_OK
}

/// Unbinds the IRQ associated with `irq_ptr`.
///
/// # Safety
///
/// The caller must ensure that `dispatcher_ptr` and `irq_ptr` are valid, non-null
/// pointers.
pub unsafe extern "C" fn unbind_irq(
    dispatcher_ptr: *mut async_dispatcher_t,
    irq_ptr: *mut async_irq_t,
) -> zx_status_t {
    let Some(irq_ptr) = NonNull::new(irq_ptr) else {
        return ZX_ERR_INVALID_ARGS;
    };
    // SAFETY: The caller guarantees `dispatcher_ptr` originates from `ScopeDispatcher::as_ptr`.
    let dispatcher = unsafe { ScopeDispatcher::from_ptr(dispatcher_ptr) };
    let irq = Irq { ptr: irq_ptr };

    if let Err(err) = dispatcher.pending_irqs.lock().unbind(dispatcher, irq) {
        return err.into_raw();
    }
    ZX_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use core::task::Poll;
    use fuchsia_async::TestExecutorBuilder;
    use fuchsia_sync::Mutex;
    use futures::{StreamExt, poll};
    use libasync::{AsAsyncDispatcherRef, DispatcherInterruptExt};
    use libasync_sys::async_state_t;
    use zx::{BootInstant, VirtualInterrupt};

    unsafe extern "C" fn never_call_handler(
        _dispatcher: *mut async_dispatcher_t,
        _irq: *mut async_irq_t,
        _status: zx_status_t,
        _signal: *const zx_packet_interrupt_t,
    ) {
        panic!("this handler should never be called");
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_bind_and_trigger_irq() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        // Trigger IRQ
        let trigger_time = BootInstant::from_nanos(12345);
        stream.interrupt().unwrap().trigger(trigger_time).unwrap();

        let res = stream.next().await;
        assert_eq!(res, Some(Ok(trigger_time)));

        stream.ack().unwrap();
        drop(stream);

        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_multi_shot_irq() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        for i in 1..=5 {
            let trigger_time = BootInstant::from_nanos(i * 1000);
            stream.interrupt().unwrap().trigger(trigger_time).unwrap();

            let res = stream.next().await;
            assert_eq!(res, Some(Ok(trigger_time)));

            stream.ack().unwrap();
        }

        drop(stream);
        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_unbind_stops_delivery() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let trigger_irq = v_irq.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        // Dropping stream unbinds from the dispatcher.
        drop(stream);

        // Triggering after unbind should not cause any issue or delivery.
        trigger_irq.trigger(BootInstant::from_nanos(9999)).unwrap();
        fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
            zx::MonotonicDuration::from_millis(20),
        ))
        .await;

        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_take_interrupt() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        let v_irq = stream.take_interrupt().unwrap();
        assert_eq!(poll!(stream.next()), Poll::Ready(None));

        // Reclaimed interrupt can still be triggered directly.
        v_irq.trigger(BootInstant::from_nanos(12345)).unwrap();

        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_self_unbind_in_handler() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();

        static UNBIND_RESULT: Mutex<Option<zx_status_t>> = Mutex::new(None);
        static HANDLER_INVOKED: AtomicU32 = AtomicU32::new(0);

        unsafe extern "C" fn self_unbinding_cb(
            dispatcher: *mut async_dispatcher_t,
            irq: *mut async_irq_t,
            status: zx_status_t,
            _signal: *const zx_packet_interrupt_t,
        ) {
            HANDLER_INVOKED.fetch_add(1, Ordering::SeqCst);
            if status == ZX_OK {
                // SAFETY: Self unbind call with matching pointers.
                let res = unsafe { unbind_irq(dispatcher, irq) };
                *UNBIND_RESULT.lock() = Some(res);
            }
        }

        let mut irq = async_irq_t {
            state: async_state_t::default(),
            handler: Some(self_unbinding_cb),
            object: v_irq.raw_handle(),
        };

        // SAFETY: Passing valid stack-allocated async_irq_t pointer.
        let status = unsafe { bind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) };
        assert_eq!(status, ZX_OK);

        v_irq.trigger(BootInstant::from_nanos(1111)).unwrap();
        while HANDLER_INVOKED.load(Ordering::SeqCst) == 0 {
            fuchsia_async::Timer::new(fuchsia_async::MonotonicInstant::after(
                zx::MonotonicDuration::from_millis(5),
            ))
            .await;
        }

        assert_eq!(HANDLER_INVOKED.load(Ordering::SeqCst), 1);
        assert_eq!(*UNBIND_RESULT.lock(), Some(ZX_OK));

        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_shutdown_cancels_bound_irq() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        // Shutdown while IRQ is still bound
        scope_dispatcher.shutdown().await;

        let res = stream.next().await;
        assert_eq!(res, Some(Err(Status::CANCELED)));
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_bind_and_unbind_after_shutdown() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();

        scope_dispatcher.shutdown().await;

        // Binding via OnInterrupt after shutdown returns BAD_STATE
        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);
        assert_eq!(poll!(stream.next()), Poll::Ready(Some(Err(Status::BAD_STATE))));

        // Direct unbind after shutdown returns BAD_STATE
        let mut irq = async_irq_t {
            state: async_state_t::default(),
            handler: Some(never_call_handler),
            object: zx::sys::ZX_HANDLE_INVALID,
        };
        // SAFETY: Passing valid pointer to stack async_irq_t.
        let status =
            Status::ok(unsafe { unbind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) });
        assert_eq!(status, Err(Status::BAD_STATE));
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_unbind_not_found() {
        let scope_dispatcher = ScopeDispatcher::new();
        let mut irq = async_irq_t {
            state: async_state_t::default(),
            handler: Some(never_call_handler),
            object: zx::sys::ZX_HANDLE_INVALID,
        };

        // SAFETY: Passing valid stack-allocated async_irq_t pointer.
        let status =
            Status::ok(unsafe { unbind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) });
        assert_eq!(status, Err(Status::NOT_FOUND));

        scope_dispatcher.shutdown().await;
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_already_bound() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();
        let mut irq = async_irq_t {
            state: async_state_t::default(),
            handler: Some(never_call_handler),
            object: v_irq.raw_handle(),
        };

        // SAFETY: Passing valid stack-allocated async_irq_t pointer.
        let status = unsafe { bind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) };
        assert_eq!(status, ZX_OK);

        // Attempting to bind the same struct again should fail
        // SAFETY: Passing valid stack-allocated async_irq_t pointer.
        let status =
            Status::ok(unsafe { bind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) });
        assert_eq!(status, Err(Status::ALREADY_BOUND));

        // SAFETY: Passing valid stack-allocated async_irq_t pointer.
        let status = unsafe { unbind_irq(scope_dispatcher.as_ptr().cast_mut(), &mut irq) };
        assert_eq!(status, ZX_OK);

        scope_dispatcher.shutdown().await;
    }

    #[test]
    fn test_unsupported_executor_port() {
        let mut test_executor = TestExecutorBuilder::new().allow_interrupts(false).build();
        let scope_dispatcher =
            ScopeDispatcher::new_on_executor(test_executor.global_handle().clone());
        let v_irq = VirtualInterrupt::create_virtual().unwrap();

        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(
            test_executor.run_until_stalled(&mut stream.next()),
            Poll::Ready(Some(Err(Status::NOT_SUPPORTED)))
        );

        assert_eq!(
            test_executor.run_until_stalled(&mut scope_dispatcher.shutdown()),
            Poll::Ready(())
        );
    }

    #[fuchsia::test(allow_interrupts = true)]
    async fn test_destroyed_irq_unbind() {
        let scope_dispatcher = ScopeDispatcher::new();
        let v_irq = VirtualInterrupt::create_virtual().unwrap();

        let mut stream = scope_dispatcher.as_async_dispatcher_ref().on_interrupt(v_irq);

        assert_eq!(poll!(stream.next()), Poll::Pending);

        // Destroy the interrupt object via syscall without closing handle
        let raw_handle = stream.interrupt().unwrap().raw_handle();
        // SAFETY: raw_handle is a valid handle to a live virtual interrupt.
        let res = unsafe { zx::sys::zx_interrupt_destroy(raw_handle) };
        assert_eq!(res, ZX_OK);

        // Dropping the stream unbinds cleanly even when interrupt was destroyed.
        drop(stream);

        scope_dispatcher.shutdown().await;
    }
}
