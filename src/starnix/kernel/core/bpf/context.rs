// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::bpf::attachments::BpfSock;
use crate::power::EbpfSuspendGuard;
use crate::task::CurrentTask;
use ebpf_api::{
    BpfSockContext, CurrentTaskContext, Map, MapValueRef, MapsContext, RingBufferReservations,
};

use starnix_uapi::{gid_t, pid_t, uid_t};

enum SuspendLockState<'a> {
    NotLocked(),

    #[allow(dead_code)]
    Locked(EbpfSuspendGuard<'a>),
}

pub struct EbpfRunContextImpl<'a> {
    current_task: &'a CurrentTask,

    map_refs: Vec<MapValueRef<'a>>,

    ringbuf_reservations: RingBufferReservations<'a>,

    // Must be declared last so it is dropped after `map_refs`.
    suspend_lock_state: SuspendLockState<'a>,
}

impl<'a> EbpfRunContextImpl<'a> {
    pub fn new(current_task: &'a CurrentTask) -> Self {
        Self {
            current_task,
            map_refs: vec![],
            ringbuf_reservations: RingBufferReservations::default(),
            suspend_lock_state: SuspendLockState::NotLocked(),
        }
    }
}

impl<'a> MapsContext<'a> for EbpfRunContextImpl<'a> {
    fn on_map_access(&mut self, map: &Map) {
        if map.uses_locks() && matches!(self.suspend_lock_state, SuspendLockState::NotLocked()) {
            replace_with::replace_with(&mut self.suspend_lock_state, |state| {
                let SuspendLockState::NotLocked() = state else { unreachable!() };
                SuspendLockState::Locked(
                    self.current_task.kernel().suspend_resume_manager.acquire_ebpf_suspend_lock(),
                )
            });
        }
    }

    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>) {
        self.map_refs.push(map_ref)
    }

    fn ringbuf_reservations(&mut self) -> &mut RingBufferReservations<'a> {
        &mut self.ringbuf_reservations
    }
}

impl<'a> CurrentTaskContext for EbpfRunContextImpl<'a> {
    fn get_uid_gid(&self) -> (uid_t, gid_t) {
        let creds = self.current_task.current_creds();
        (creds.uid, creds.gid)
    }

    fn get_tid_tgid(&self) -> (pid_t, pid_t) {
        let task = &self.current_task.task;
        (task.get_tid(), task.get_pid())
    }
}

impl<'a> BpfSockContext for EbpfRunContextImpl<'a> {
    type BpfSockRef = &'a BpfSock<'a>;
}
