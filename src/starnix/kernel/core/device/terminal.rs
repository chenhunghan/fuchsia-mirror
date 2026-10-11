// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mutable_state::{state_accessor, state_implementation};
use crate::task::{EventHandler, Pid, WaitCanceler, WaitQueue, Waiter};
use crate::vfs::buffers::{InputBuffer, InputBufferExt as _, OutputBuffer};
use crate::vfs::{DirEntryHandle, FsString, Mounts};
use derivative::Derivative;
pub use line_discipline::TerminalSide;
use line_discipline::{LineDiscipline, PendingSignals};
use macro_rules_attribute::apply;
use starnix_sync::{DeviceTerminalsLock, LockDepMutex, LockDepRwLock, PtsIdsSetLock};
use starnix_uapi::auth::FsCred;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::{SIGCONT, SIGHUP, Signal};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{error, pid_t, uapi};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Weak};

const DEVPTS_FIRST_MAJOR: u32 = 136;
const DEVPTS_MAJOR_COUNT: u32 = 4;
pub const DEVPTS_COUNT: u32 = DEVPTS_MAJOR_COUNT * 256;

// Construct the DeviceId associated with the given pts replicas.
pub fn get_device_type_for_pts(id: u32) -> DeviceId {
    DeviceId::new(DEVPTS_FIRST_MAJOR + id / 256, id % 256)
}

/// Global state of the devpts filesystem.
pub struct TtyState {
    /// The terminal objects indexed by their identifier.
    pub terminals: LockDepRwLock<HashMap<u32, Weak<Terminal>>, DeviceTerminalsLock>,

    /// The set of allocated terminal identifiers.
    pts_ids_set: LockDepMutex<PtsIdsSet, PtsIdsSetLock>,
}

impl TtyState {
    /// Allocates a new terminal and returns it.
    pub fn get_next_terminal(
        self: &Arc<Self>,
        dev_pts_root: DirEntryHandle,
        creds: FsCred,
    ) -> Result<Arc<Terminal>, Errno> {
        let id = self.pts_ids_set.lock().acquire()?;
        let terminal = Terminal::new(self.clone(), dev_pts_root, creds, id);
        assert!(self.terminals.write().insert(id, Arc::downgrade(&terminal)).is_none());
        Ok(terminal)
    }

    /// Release the terminal identifier into the set of available identifier.
    pub fn release_terminal(&self, id: u32) -> Result<(), Errno> {
        // Remove this terminal id from the set of terminals before releasing the
        // identifier. Otherwise, a new terminal might reuse the id and get removed
        // instead.
        assert!(self.terminals.write().remove(&id).is_some());
        self.pts_ids_set.lock().release(id);
        Ok(())
    }
}

impl Default for TtyState {
    fn default() -> Self {
        Self { terminals: Default::default(), pts_ids_set: PtsIdsSet::new(DEVPTS_COUNT).into() }
    }
}

#[derive(Derivative)]
#[derivative(Default)]
#[derivative(Debug)]
pub struct TerminalMutableState {
    pub line_discipline: LineDiscipline,

    /// Wait queue for the main side of the terminal.
    main_wait_queue: WaitQueue,

    /// Wait queue for the replica side of the terminal.
    replica_wait_queue: WaitQueue,

    /// The controlling session and foreground process group of the terminal.
    pub controlling_session: Option<ControllingSession>,
}

/// State of a given terminal. This object handles both the main and the replica terminal.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Terminal {
    /// Weak self to allow cloning.
    weak_self: Weak<Self>,

    /// The global devpts state.
    #[derivative(Debug = "ignore")]
    state: Arc<TtyState>,

    /// The root of the devpts fs responsible for this terminal.
    pub dev_pts_root: DirEntryHandle,

    /// The owner of the terminal.
    pub fscred: FsCred,

    /// The identifier of the terminal.
    pub id: u32,

    /// The mutable state of the Terminal.
    mutable_state:
        starnix_sync::LockDepRwLock<TerminalMutableState, starnix_sync::TerminalMutableStateLock>,
}

impl Terminal {
    pub fn new(
        state: Arc<TtyState>,
        dev_pts_root: DirEntryHandle,
        fscred: FsCred,
        id: u32,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            weak_self: weak_self.clone(),
            state,
            dev_pts_root,
            fscred,
            id,
            mutable_state: Default::default(),
        })
    }

    pub fn to_owned(&self) -> Arc<Terminal> {
        self.weak_self.upgrade().expect("This should never be called while releasing the terminal")
    }

    /// Sets the terminal configuration.
    pub fn set_termios(&self, termios: uapi::termios2) {
        let signals = self.write().set_termios(termios);
        self.send_signals(signals.signals());
    }

    pub fn flush(&self, is_main: bool, arg: u32) -> Result<(), Errno> {
        self.write().flush(is_main, arg)
    }

    /// `close` implementation of the main side of the terminal.
    pub fn main_close(&self) {
        // Remove the entry in the file system.
        let id = FsString::from(self.id.to_string());
        // The child is not a directory, the mount doesn't matter.
        self.dev_pts_root.remove_child(id.as_ref(), &Mounts::new());
        self.write().main_close();
    }

    /// Called when a new reference to the main side of this terminal is made.
    pub fn main_open(&self) {
        self.write().main_open();
    }

    /// `wait_async` implementation of the main side of the terminal.
    pub fn main_wait_async(
        &self,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.read().main_wait_async(waiter, events, handler)
    }

    /// `query_events` implementation of the main side of the terminal.
    pub fn main_query_events(&self) -> FdEvents {
        self.read().main_query_events()
    }

    /// `read` implementation of the main side of the terminal.
    pub fn main_read(&self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        self.write().main_read(data)
    }

    /// `write` implementation of the main side of the terminal.
    pub fn main_write(&self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        let (bytes, signals) = self.write().main_write(data)?;
        self.send_signals(signals.signals());
        Ok(bytes)
    }

    /// `close` implementation of the replica side of the terminal.
    pub fn replica_close(&self) {
        self.write().replica_close();
    }

    /// Called when a new reference to the replica side of this terminal is made.
    pub fn replica_open(&self) {
        self.write().replica_open();
    }

    /// `wait_async` implementation of the replica side of the terminal.
    pub fn replica_wait_async(
        &self,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.read().replica_wait_async(waiter, events, handler)
    }

    /// `query_events` implementation of the replica side of the terminal.
    pub fn replica_query_events(&self) -> FdEvents {
        self.read().replica_query_events()
    }

    /// `read` implementation of the replica side of the terminal.
    pub fn replica_read(&self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        let (bytes, signals) = self.write().replica_read(data)?;
        self.send_signals(signals.signals());
        Ok(bytes)
    }

    /// `write` implementation of the replica side of the terminal.
    pub fn replica_write(&self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        self.write().replica_write(data)
    }

    /// Disassociates the controlling session from the terminal.
    ///
    /// Clears the controlling session, clears `controlling_terminal` on all member
    /// thread groups in the session, and sends SIGHUP and SIGCONT to the foreground
    /// process group outside of any terminal locks.
    pub fn disassociate_controlling_session(&self) {
        self.disassociate_controlling_session_if(None, None);
    }

    /// Disassociates the controlling session from the terminal if `expected_session` matches
    /// (or is `None`), clearing matching member `controlling_terminal` references and sending
    /// `SIGHUP` and `SIGCONT` to the foreground process group outside of any terminal locks.
    pub fn disassociate_controlling_session_if(
        &self,
        expected_session: Option<&Pid>,
        side: Option<TerminalSide>,
    ) {
        let controlling_session = {
            let mut terminal_state = self.write();
            if expected_session.is_none_or(|sid| terminal_state.controlling_session() == Some(sid))
            {
                terminal_state.controlling_session.take()
            } else {
                None
            }
        };
        if let Some(controlling_session) = controlling_session {
            controlling_session.disassociate(self, side);
        }
    }

    /// Sends the specified signals to the foreground process group of this terminal.
    ///
    /// The terminal state lock is held only to extract the foreground PID and is dropped
    /// before signal dispatch to prevent lock inversion with ThreadGroup locks.
    pub fn send_signals(&self, signals: &[Signal]) {
        if signals.is_empty() {
            return;
        }
        let foreground_pg = {
            let state = self.read();
            state.controlling_session.as_ref().map(|cs| cs.foreground_process_group.clone())
        };
        if let Some(pgid) = foreground_pg {
            pgid.send_signals_to_pgid(signals);
        }
    }

    pub fn device(&self) -> DeviceId {
        get_device_type_for_pts(self.id)
    }

    state_accessor!(Terminal, mutable_state);
}

struct InputBufferWrapper<'a>(&'a mut dyn crate::vfs::buffers::InputBuffer);

impl<'a> line_discipline::InputBuffer for InputBufferWrapper<'a> {
    fn available(&self) -> usize {
        self.0.available()
    }
    fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
        self.0.read_to_vec_exact(size)
    }
}

struct OutputBufferWrapper<'a>(&'a mut dyn crate::vfs::buffers::OutputBuffer);

impl<'a> line_discipline::OutputBuffer for OutputBufferWrapper<'a> {
    fn available(&self) -> usize {
        self.0.available()
    }
    fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
        self.0.write(data)
    }
}

#[apply(state_implementation!)]
impl TerminalMutableState<Base = Terminal> {
    /// Returns the controlling session PID if one is associated.
    pub fn controlling_session(&self) -> Option<&Pid> {
        self.controlling_session.as_ref().map(|cs| &cs.session)
    }

    /// Returns the terminal configuration.
    pub fn termios(&self) -> &uapi::termios2 {
        self.line_discipline.termios()
    }

    pub fn set_packet_mode(&mut self, enabled: bool) {
        if enabled != self.line_discipline.is_packet_mode_enabled() {
            self.line_discipline.set_packet_mode(enabled);
            self.notify_waiters();
        }
    }

    /// Returns the number of available bytes to read from the side of the terminal described by
    /// `is_main`.
    pub fn get_available_read_size(&self, is_main: bool) -> usize {
        self.line_discipline.get_available_read_size(TerminalSide::from(is_main))
    }

    /// Sets the terminal configuration.
    fn set_termios(&mut self, termios: uapi::termios2) -> PendingSignals {
        let signals = self.line_discipline.set_termios(termios);
        self.notify_waiters();
        signals
    }

    pub fn flush(&mut self, is_main: bool, arg: u32) -> Result<(), Errno> {
        self.line_discipline.flush(TerminalSide::from(is_main), arg)?;
        self.notify_waiters();
        Ok(())
    }

    /// `close` implementation of the main side of the terminal.
    pub fn main_close(&mut self) {
        self.line_discipline.main_close();
        self.notify_waiters();
    }

    /// Called when a new reference to the main side of this terminal is made.
    pub fn main_open(&mut self) {
        self.line_discipline.main_open();
    }

    pub fn is_main_closed(&self) -> bool {
        self.line_discipline.is_main_closed()
    }

    /// `wait_async` implementation of the main side of the terminal.
    fn main_wait_async(
        &self,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.main_wait_queue.wait_async_fd_events(waiter, events, handler)
    }

    /// `query_events` implementation of the main side of the terminal.
    fn main_query_events(&self) -> FdEvents {
        self.line_discipline.main_query_events()
    }

    /// `read` implementation of the main side of the terminal.
    fn main_read(&mut self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        let mut wrapper = OutputBufferWrapper(data);
        let result = self.line_discipline.main_read(&mut wrapper)?;
        self.notify_waiters();
        Ok(result)
    }

    /// `write` implementation of the main side of the terminal.
    ///
    /// Returns the number of bytes written and any signals generated while processing the input.
    fn main_write(&mut self, data: &mut dyn InputBuffer) -> Result<(usize, PendingSignals), Errno> {
        let mut wrapper = InputBufferWrapper(data);
        let (result, signals) = self.line_discipline.main_write(&mut wrapper)?;
        self.notify_waiters();
        Ok((result, signals))
    }

    /// `close` implementation of the replica side of the terminal.
    pub fn replica_close(&mut self) {
        self.line_discipline.replica_close();
        self.notify_waiters();
    }

    /// Called when a new reference to the replica side of this terminal is made.
    pub fn replica_open(&mut self) {
        self.line_discipline.replica_open();
    }

    /// `wait_async` implementation of the replica side of the terminal.
    fn replica_wait_async(
        &self,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> WaitCanceler {
        self.replica_wait_queue.wait_async_fd_events(waiter, events, handler)
    }

    /// `query_events` implementation of the replica side of the terminal.
    fn replica_query_events(&self) -> FdEvents {
        self.line_discipline.replica_query_events()
    }

    /// `read` implementation of the replica side of the terminal.
    ///
    /// Returns the number of bytes read and any signals generated while draining waiting input
    /// buffers.
    fn replica_read(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        let mut wrapper = OutputBufferWrapper(data);
        let (result, signals) = self.line_discipline.replica_read(&mut wrapper)?;
        self.notify_waiters();
        Ok((result, signals))
    }

    /// `write` implementation of the replica side of the terminal.
    fn replica_write(&mut self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        let mut wrapper = InputBufferWrapper(data);
        let result = self.line_discipline.replica_write(&mut wrapper)?;
        self.notify_waiters();
        Ok(result)
    }

    /// Notify any waiters if the state of the terminal changes.
    fn notify_waiters(&mut self) {
        let main_events = self.line_discipline.main_query_events();
        if main_events.bits() != 0 {
            self.main_wait_queue.notify_fd_events(main_events);
        }
        let replica_events = self.line_discipline.replica_query_events();
        if replica_events.bits() != 0 {
            self.replica_wait_queue.notify_fd_events(replica_events);
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.state.release_terminal(self.id).unwrap()
    }
}

/// The controlling terminal of a process.
#[derive(Clone, Debug)]
pub struct ControllingTerminal {
    /// The controlling terminal.
    pub terminal: Arc<Terminal>,
    /// The side of the terminal associated with the session.
    pub side: TerminalSide,
}

impl ControllingTerminal {
    pub fn new(terminal: &Terminal, side: TerminalSide) -> Self {
        Self { terminal: terminal.to_owned(), side }
    }

    pub fn matches(&self, terminal: &Terminal, side: TerminalSide) -> bool {
        std::ptr::eq(terminal, Arc::as_ptr(&self.terminal)) && side == self.side
    }
}

/// The controlling session and foreground process group of a terminal.
#[derive(Clone, Debug)]
pub struct ControllingSession {
    /// The controlling session leader PID.
    pub session: Pid,

    /// The foreground process group PID.
    pub foreground_process_group: Pid,
}

impl ControllingSession {
    pub fn new(session: &Pid, foreground_pgrp: &Pid) -> Self {
        Self { session: Arc::clone(session), foreground_process_group: Arc::clone(foreground_pgrp) }
    }

    pub fn foreground_process_group(&self) -> &Pid {
        &self.foreground_process_group
    }

    pub fn foreground_pgid(&self) -> pid_t {
        self.foreground_process_group.id
    }

    pub fn set_foreground_process_group(&mut self, pgid: Pid) {
        self.foreground_process_group = pgid;
    }

    /// Clears matching member `controlling_terminal` references and sends `SIGHUP` and `SIGCONT`
    /// to the foreground process group after this session has been detached from `terminal`.
    pub fn disassociate(self, terminal: &Terminal, side: Option<TerminalSide>) {
        self.session.clear_controlling_terminal(Some((terminal, side)));
        self.foreground_process_group.send_signals_to_pgid(&[SIGHUP, SIGCONT]);
    }
}

#[derive(Debug)]
struct PtsIdsSet {
    pts_count: u32,
    next_id: u32,
    reclaimed_ids: BTreeSet<u32>,
}

impl PtsIdsSet {
    fn new(pts_count: u32) -> Self {
        Self { pts_count, next_id: 0, reclaimed_ids: BTreeSet::new() }
    }

    fn release(&mut self, id: u32) {
        assert!(self.reclaimed_ids.insert(id))
    }

    fn acquire(&mut self) -> Result<u32, Errno> {
        match self.reclaimed_ids.iter().next() {
            Some(e) => {
                let value = *e;
                self.reclaimed_ids.remove(&value);
                Ok(value)
            }
            None => {
                if self.next_id < self.pts_count {
                    let id = self.next_id;
                    self.next_id += 1;
                    Ok(id)
                } else {
                    error!(ENOSPC)
                }
            }
        }
    }
}
