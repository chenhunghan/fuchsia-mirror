// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use derivative::Derivative;
use starnix_uapi::errors::Errno;
use starnix_uapi::signals::{SIGINT, SIGQUIT, SIGTSTP, Signal};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    CREAD, CS8, CSIZE, ECHO, ECHOCTL, ECHOE, ECHOK, ECHOKE, ECHONL, ECHOPRT, EXTPROC, ICANON,
    ICRNL, IEXTEN, IGNCR, INLCR, ISIG, ISTRIP, IUCLC, IUTF8, IXANY, IXON, NOFLSH, OCRNL, OLCUC,
    ONLCR, ONLRET, ONOCR, OPOST, PARENB, PARMRK, TABDLY, VEOF, VEOL, VEOL2, VERASE, VINTR, VKILL,
    VLNEXT, VQUIT, VREPRINT, VSTART, VSTOP, VSUSP, VWERASE, XTABS, cc_t, error, tcflag_t, uapi,
};
use std::collections::VecDeque;

// CANON_MAX_BYTES is the number of bytes that fit into a single line of
// terminal input in canonical mode. See https://github.com/google/gvisor/blob/master/pkg/sentry/fs/tty/line_discipline.go
const CANON_MAX_BYTES: usize = 4096;

// NON_CANON_MAX_BYTES is the maximum number of bytes that can be read at
// a time in non canonical mode.
const NON_CANON_MAX_BYTES: usize = CANON_MAX_BYTES - 1;

// WAIT_BUFFER_MAX_BYTES is the maximum size of a wait buffer. It is based on
// https://github.com/google/gvisor/blob/master/pkg/sentry/fsimpl/devpts/queue.go
const WAIT_BUFFER_MAX_BYTES: usize = 131072;

const SPACES_PER_TAB: usize = 8;

// DISABLED_CHAR is used to indicate that a control character is disabled.
const DISABLED_CHAR: u8 = 0;

const BACKSPACE_CHAR: u8 = 8; // \b

/// The offset in ASCII between a control character and it's character name.
/// For example, typing CTRL-C on a keyboard generates the value
/// b'C' - CONTROL_OFFSET
const CONTROL_OFFSET: u8 = 0x40;

#[derive(Derivative)]
#[derivative(Default)]
#[derivative(Debug)]
pub struct LineDiscipline {
    /// |true| is the terminal is locked.
    #[derivative(Default(value = "true"))]
    pub locked: bool,

    /// |true| if the output is stopped (due to IXON).
    #[derivative(Default(value = "false"))]
    pub stopped: bool,

    /// Terminal size.
    pub window_size: uapi::winsize,

    /// Terminal configuration.
    #[derivative(Default(value = "get_default_termios()"))]
    termios: uapi::termios2,

    /// True if the terminal is currently in the middle of an erase sequence (ECHOPRT).
    #[derivative(Default(value = "false"))]
    erasing: bool,

    /// True if the next character should be treated literally.
    #[derivative(Default(value = "false"))]
    lnext: bool,

    /// Location in a row of the cursor. Needed to handle certain special characters like
    /// backspace.
    column: usize,

    /// Column where the current canonical input line started.
    canon_column: usize,

    /// Echoes generated while processing input. Flushed when output is not stopped.
    pending_echoes: Vec<EchoOp>,

    /// Packet mode state (TIOCPKT).
    #[derivative(Default(value = "false"))]
    packet_mode_enabled: bool,

    /// Packet mode pending events.
    #[derivative(Default(value = "0"))]
    packet_mode_pending_events: u8,

    /// The number of active references to the main part of the terminal. Starts as `None`. The
    /// main part of the terminal is considered closed when this is `Some(0)`.
    main_references: Option<u32>,

    /// The number of active references to the replica part of the terminal. Starts as `None`. The
    /// replica part of the terminal is considered closed when this is `Some(0)`.
    replica_references: Option<u32>,

    /// Input queue of the terminal. Data flow from the main side to the replica side.
    #[derivative(Default(value = "Queue::input_queue()"))]
    input_queue: Option<Queue>,

    /// Output queue of the terminal. Data flow from the replica side to the main side.
    #[derivative(Default(value = "Queue::output_queue()"))]
    output_queue: Option<Queue>,
}

/// Helper trait for input/output buffers.
pub trait InputBuffer {
    fn available(&self) -> usize;
    fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno>;
}

pub trait OutputBuffer {
    fn available(&self) -> usize;
    fn write(&mut self, data: &[u8]) -> Result<usize, Errno>;
}

/// Macro to help working with the terminal queues.
macro_rules! with_queue {
    ($self_:tt . $name:ident . $fn:ident ( $($param:expr),*$(,)?)) => {
        {
        let mut queue = $self_.$name . take().unwrap();
        let result = queue.$fn( $($param),* );
        $self_.$name = Some(queue);
        result
        }
    };
}

/// Keep track of the signals to send when handling terminal content.
#[derive(Debug, PartialEq)]
#[must_use]
pub struct PendingSignals {
    signals: Vec<Signal>,
}

impl PendingSignals {
    pub fn new() -> Self {
        Self { signals: vec![] }
    }

    /// Add the given signal to the list of signal to send to the associate process group.
    fn add(&mut self, signal: Signal) {
        self.signals.push(signal);
    }

    /// Append all pending signals in `other` to `self`.
    fn append(&mut self, mut other: Self) {
        self.signals.append(&mut other.signals);
    }

    /// Returns a slice of the pending signals.
    pub fn signals(&self) -> &[Signal] {
        &self.signals[..]
    }
}

/// Represents the type of erase operation that can be performed on terminal input.
#[derive(Debug, PartialEq)]
enum EraseType {
    /// Erase a single character (typically triggered by backspace)
    Character,
    /// Erase a word (typically triggered by Ctrl+W)
    Word,
    /// Erase the entire line (typically triggered by Ctrl+U)
    Line,
}

/// Identifies one of the two ends of a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalSide {
    Main,
    Replica,
}

impl From<bool> for TerminalSide {
    fn from(is_main: bool) -> Self {
        if is_main { Self::Main } else { Self::Replica }
    }
}

/// Represents a deferred echo operation queued during input processing and executed in
/// `commit_echoes` when output is not stopped.
#[derive(Debug, Clone, Copy, PartialEq)]
enum EchoOp {
    /// Output a single raw echo byte through `do_output_char`.
    Byte(RawByte),
    /// Record the current cursor `column` as `canon_column` (start of canonical input line).
    SetCanonColumn,
    /// Erase a tab character (`ECHOE`), computing the number of backspaces from `num_chars` and
    /// `canon_column` (if `!after_tab`).
    EraseTab { num_chars: usize, after_tab: bool },
}

impl LineDiscipline {
    /// Returns the terminal configuration.
    pub fn termios(&self) -> &uapi::termios2 {
        &self.termios
    }

    fn is_canon_enabled(&self) -> bool {
        self.termios.has_local_flags(ICANON) && !self.is_extproc_enabled()
    }

    fn is_extproc_enabled(&self) -> bool {
        self.termios.has_local_flags(EXTPROC)
    }

    pub fn is_packet_mode_enabled(&self) -> bool {
        self.packet_mode_enabled
    }

    pub fn set_packet_mode(&mut self, enabled: bool) {
        self.packet_mode_enabled = enabled;
        if !enabled {
            self.packet_mode_pending_events = 0;
        }
    }

    /// Returns the number of available bytes to read from `side`.
    pub fn get_available_read_size(&self, side: TerminalSide) -> usize {
        let queue = match side {
            TerminalSide::Main => self.output_queue(),
            TerminalSide::Replica => self.input_queue(),
        };
        queue.readable_size()
    }

    /// Resets transient terminal input state (`erasing`, `lnext`, and `pending_echoes`).
    fn reset_input_state(&mut self) {
        self.erasing = false;
        self.lnext = false;
        self.pending_echoes.clear();
    }

    /// Sets the terminal configuration.
    pub fn set_termios(&mut self, mut termios: uapi::termios2) -> PendingSignals {
        // Pseudo-terminals always force CS8 | CREAD and clear PARENB.
        termios.c_cflag &= !(CSIZE | PARENB);
        termios.c_cflag |= CS8 | CREAD;

        let old_ixon = self.termios.has_input_flags(IXON);
        let new_ixon = termios.has_input_flags(IXON);
        let old_flow = old_ixon
            && self.termios.c_cc[VSTOP as usize] == get_control_character('S')
            && self.termios.c_cc[VSTART as usize] == get_control_character('Q');
        let new_flow = new_ixon
            && termios.c_cc[VSTOP as usize] == get_control_character('S')
            && termios.c_cc[VSTART as usize] == get_control_character('Q');
        let extproc_active = ((self.termios.c_lflag | termios.c_lflag) & EXTPROC) != 0;
        let canon_or_extproc_changed =
            ((self.termios.c_lflag ^ termios.c_lflag) & (ICANON | EXTPROC)) != 0;

        self.termios = termios;

        if self.packet_mode_enabled {
            if old_flow != new_flow {
                self.packet_mode_pending_events &=
                    !((uapi::TIOCPKT_DOSTOP | uapi::TIOCPKT_NOSTOP) as u8);
                let event = if new_flow { uapi::TIOCPKT_DOSTOP } else { uapi::TIOCPKT_NOSTOP };
                self.packet_mode_pending_events |= event as u8;
            }
            if extproc_active {
                self.packet_mode_pending_events |= uapi::TIOCPKT_IOCTL as u8;
            }
        }

        if old_ixon && !new_ixon {
            self.start_tty();
        }

        if canon_or_extproc_changed {
            self.erasing = false;
            self.lnext = false;
            with_queue!(self.input_queue.on_canon_mode_changed(self))
        } else {
            PendingSignals::new()
        }
    }

    /// Flushes queues according to `queue_selector` (TCIFLUSH, TCOFLUSH, TCIOFLUSH).
    pub fn flush(&mut self, side: TerminalSide, queue_selector: u32) -> Result<(), Errno> {
        let flush_terminal_input = side == TerminalSide::Replica
            && matches!(queue_selector, uapi::TCIFLUSH | uapi::TCIOFLUSH);

        // We can receive a flush request from either the main or the replica which switch what the
        // input and output queues are referring to.
        let (input_queue, output_queue) = match side {
            TerminalSide::Main => {
                (self.output_queue.as_mut().unwrap(), self.input_queue.as_mut().unwrap())
            }
            TerminalSide::Replica => {
                (self.input_queue.as_mut().unwrap(), self.output_queue.as_mut().unwrap())
            }
        };

        let event;
        // For input flushes, we discard all data in the pipeline. Data that's already been
        // delivered to us, and data that's waiting to be delivered.
        //
        // For output flushes, we want to only discard data that has been sent, but not yet
        // delivered to the other side's read_queue. Since this we're a pty and sending is just a
        // memcpy rather than actually going across a wire, the only time this will happen when the
        // read_queue fills up and there is backpressure due to full buffers.
        match queue_selector {
            uapi::TCIFLUSH => {
                input_queue.flush();
                event = uapi::TIOCPKT_FLUSHREAD;
            }
            uapi::TCOFLUSH => {
                output_queue.flush_unprocessed();
                event = uapi::TIOCPKT_FLUSHWRITE;
            }
            uapi::TCIOFLUSH => {
                input_queue.flush();
                output_queue.flush_unprocessed();
                event = uapi::TIOCPKT_FLUSHREAD | uapi::TIOCPKT_FLUSHWRITE;
            }
            _ => return error!(EINVAL),
        };

        if flush_terminal_input {
            self.reset_input_state();
        }

        if side == TerminalSide::Replica && self.packet_mode_enabled {
            self.packet_mode_pending_events |= event as u8;
        }

        Ok(())
    }

    /// `close` implementation of the main side of the terminal.
    pub fn main_close(&mut self) {
        self.main_references = self.main_references.map(|v| v - 1);
        if self.is_main_closed() {
            let _ = self.flush(TerminalSide::Replica, uapi::TCIFLUSH);
            let _ = self.flush(TerminalSide::Main, uapi::TCIFLUSH);
        }
    }

    /// Called when a new reference to the main side of this terminal is made.
    pub fn main_open(&mut self) {
        self.main_references = Some(self.main_references.unwrap_or(0) + 1);
    }

    pub fn is_main_closed(&self) -> bool {
        matches!(self.main_references, Some(0))
    }

    /// `query_events` implementation of the main side of the terminal.
    pub fn main_query_events(&self) -> FdEvents {
        let mut events =
            self.output_queue().read_readiness() | self.input_queue().write_readiness();
        if self.packet_mode_enabled && self.packet_mode_pending_events != 0 {
            events |= FdEvents::POLLIN | FdEvents::POLLPRI;
        }
        if self.is_replica_closed() {
            events |= FdEvents::POLLHUP;
        }
        events
    }

    /// `read` implementation of the main side of the terminal.
    pub fn main_read(&mut self, data: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        if self.packet_mode_enabled && self.packet_mode_pending_events != 0 {
            if data.available() == 0 {
                return Ok(0);
            }
            let event = self.packet_mode_pending_events;
            let written = data.write(&[event])?;
            if written > 0 {
                self.packet_mode_pending_events = 0;
            }
            return Ok(written);
        }
        if self.is_replica_closed() && self.output_queue().readable_size() == 0 {
            return error!(EIO);
        }
        if self.packet_mode_enabled {
            if self.output_queue().readable_size() == 0 {
                return error!(EAGAIN);
            }
            if data.available() == 0 {
                return Ok(0);
            }
            let written = data.write(&[0])?;
            if written == 0 {
                return Ok(0);
            }
            let res = with_queue!(self.output_queue.read(self, data));
            match res {
                Ok((n, signals)) => {
                    assert!(signals.signals().is_empty());
                    return Ok(n + 1);
                }
                Err(_) => return Ok(1),
            }
        }
        let (n, signals) = with_queue!(self.output_queue.read(self, data))?;
        assert!(signals.signals().is_empty());
        Ok(n)
    }

    /// `write` implementation of the main side of the terminal.
    ///
    /// Returns the number of bytes written and any signals generated while processing the input.
    pub fn main_write(
        &mut self,
        data: &mut dyn InputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        with_queue!(self.input_queue.write(self, data))
    }

    /// `close` implementation of the replica side of the terminal.
    pub fn replica_close(&mut self) {
        self.replica_references = self.replica_references.map(|v| v - 1);
    }

    /// Called when a new reference to the replica side of this terminal is made.
    pub fn replica_open(&mut self) {
        self.replica_references = Some(self.replica_references.unwrap_or(0) + 1);
    }

    pub fn is_replica_closed(&self) -> bool {
        matches!(self.replica_references, Some(0))
    }

    /// `query_events` implementation of the replica side of the terminal.
    pub fn replica_query_events(&self) -> FdEvents {
        if self.is_main_closed() {
            return FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLERR | FdEvents::POLLHUP;
        }
        let mut events = self.input_queue().read_readiness();
        if !(self.stopped && self.termios.has_input_flags(IXON)) {
            events |= self.output_queue().write_readiness();
        }
        events
    }

    /// `read` implementation of the replica side of the terminal.
    ///
    /// Returns the number of bytes read and any signals generated while draining waiting input
    /// buffers.
    pub fn replica_read(
        &mut self,
        data: &mut dyn OutputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        if self.is_main_closed() {
            return Ok((0, PendingSignals::new()));
        }
        with_queue!(self.input_queue.read(self, data))
    }

    /// `write` implementation of the replica side of the terminal.
    pub fn replica_write(&mut self, data: &mut dyn InputBuffer) -> Result<usize, Errno> {
        if self.is_main_closed() {
            return error!(EIO);
        }
        if self.stopped && self.termios.has_input_flags(IXON) {
            return error!(EAGAIN);
        }
        let (read_from_userspace, signals) = with_queue!(self.output_queue.write(self, data))?;
        // Writing to the replica side never generates signals.
        assert!(signals.signals().is_empty());
        Ok(read_from_userspace)
    }

    /// Returns the input queue.
    fn input_queue(&self) -> &Queue {
        self.input_queue.as_ref().unwrap()
    }

    /// Returns the output_queue. The Option is always filled.
    fn output_queue(&self) -> &Queue {
        self.output_queue.as_ref().unwrap()
    }

    /// Return whether a signal must be send when receiving `byte`, and if yes, which.
    fn handle_signals(&mut self, byte: RawByte) -> Option<Signal> {
        if !self.termios.has_local_flags(ISIG) {
            return None;
        }
        self.termios.signal(byte)
    }

    fn stop_tty(&mut self) {
        if !self.stopped {
            self.stopped = true;
            if self.packet_mode_enabled {
                self.packet_mode_pending_events &= !(uapi::TIOCPKT_START as u8);
                self.packet_mode_pending_events |= uapi::TIOCPKT_STOP as u8;
            }
        }
    }

    fn start_tty(&mut self) {
        if self.stopped {
            self.stopped = false;
            if self.packet_mode_enabled {
                self.packet_mode_pending_events &= !(uapi::TIOCPKT_STOP as u8);
                self.packet_mode_pending_events |= uapi::TIOCPKT_START as u8;
            }
            self.commit_echoes();
            let signals = with_queue!(self.output_queue.drain_waiting_buffer(self));
            assert!(signals.signals().is_empty());
        }
    }

    fn do_output_char(&mut self, mut c: RawByte, out: &mut Vec<RawByte>) {
        if !self.termios.has_output_flags(OPOST) {
            out.push(c);
            return;
        }
        match c {
            b'\n' => {
                if self.termios.has_output_flags(ONLRET) {
                    self.column = 0;
                }
                if self.termios.has_output_flags(ONLCR) {
                    self.canon_column = 0;
                    self.column = 0;
                    out.extend_from_slice(b"\r\n");
                    return;
                }
                self.canon_column = self.column;
            }
            b'\r' => {
                if self.termios.has_output_flags(ONOCR) && self.column == 0 {
                    return;
                }
                if self.termios.has_output_flags(OCRNL) {
                    c = b'\n';
                    if self.termios.has_output_flags(ONLRET) {
                        self.canon_column = 0;
                        self.column = 0;
                    }
                } else {
                    self.canon_column = 0;
                    self.column = 0;
                }
            }
            b'\t' => {
                let spaces = SPACES_PER_TAB - (self.column % SPACES_PER_TAB);
                self.column += spaces;
                if self.termios.c_oflag & TABDLY == XTABS {
                    out.extend(std::iter::repeat_n(b' ', spaces));
                    return;
                }
            }
            BACKSPACE_CHAR => {
                if self.column > 0 {
                    self.column -= 1;
                }
            }
            _ => {
                if !is_cntrl(c) {
                    if self.termios.has_output_flags(OLCUC) {
                        c.make_ascii_uppercase();
                    }
                    if !is_utf8_continuation(c, &self.termios) {
                        self.column += 1;
                    }
                }
            }
        }
        out.push(c);
    }

    fn echo_raw_byte(&mut self, c: RawByte) {
        self.pending_echoes.push(EchoOp::Byte(c));
    }

    fn echo_set_canon_col(&mut self) {
        self.pending_echoes.push(EchoOp::SetCanonColumn);
    }

    fn echo_char(&mut self, c: RawByte) {
        if self.termios.has_local_flags(ECHOCTL) && is_cntrl(c) && c != b'\t' {
            self.echo_raw_byte(b'^');
            self.echo_raw_byte(c ^ CONTROL_OFFSET);
        } else {
            self.echo_raw_byte(c);
        }
    }

    fn finish_erasing(&mut self) {
        if self.erasing {
            self.echo_raw_byte(b'/');
            self.erasing = false;
        }
    }

    fn commit_echoes(&mut self) {
        if self.stopped && self.termios.has_input_flags(IXON) {
            return;
        }
        if !self.pending_echoes.is_empty() {
            let ops = std::mem::take(&mut self.pending_echoes);
            let mut echoes = Vec::with_capacity(ops.len());
            for op in ops {
                match op {
                    EchoOp::Byte(c) => self.do_output_char(c, &mut echoes),
                    EchoOp::SetCanonColumn => {
                        self.canon_column = self.column;
                    }
                    EchoOp::EraseTab { mut num_chars, after_tab } => {
                        if !after_tab {
                            num_chars += self.canon_column;
                        }
                        let num_bs = SPACES_PER_TAB - (num_chars % SPACES_PER_TAB);
                        for _ in 0..num_bs {
                            self.do_output_char(BACKSPACE_CHAR, &mut echoes);
                        }
                    }
                }
            }
            if !echoes.is_empty() {
                if let Some(ref mut output_queue) = self.output_queue {
                    output_queue.read_queue.push_back(ReadPacket { data: echoes, has_eof: false });
                }
            }
        }
    }

    fn eraser(&mut self, queue: &mut Queue, c: RawByte) {
        if queue.line_buffer.is_empty() {
            return;
        }

        let erase_type = if self.termios.is_erase(c) {
            EraseType::Character
        } else if self.termios.is_werase(c) {
            EraseType::Word
        } else {
            if !self.termios.has_local_flags(ECHO) {
                queue.line_buffer.clear();
                return;
            }
            if !self.termios.has_local_flags(ECHOK)
                || !self.termios.has_local_flags(ECHOKE)
                || !self.termios.has_local_flags(ECHOE)
            {
                queue.line_buffer.clear();
                self.finish_erasing();
                self.echo_char(c);
                if self.termios.has_local_flags(ECHOK) {
                    self.echo_raw_byte(b'\n');
                }
                return;
            }
            EraseType::Line
        };

        let mut seen_alnums = 0;
        while !queue.line_buffer.is_empty() {
            let mut pos = queue.line_buffer.len();
            while pos > 0 {
                pos -= 1;
                if !is_utf8_continuation(queue.line_buffer[pos], &self.termios) {
                    break;
                }
            }
            let first_byte = queue.line_buffer[pos];
            if is_utf8_continuation(first_byte, &self.termios) {
                // Do not partially erase an incomplete/stray UTF-8 continuation sequence.
                break;
            }
            if erase_type == EraseType::Word {
                if is_linux_alnum_or_underscore(first_byte) {
                    seen_alnums += 1;
                } else if seen_alnums > 0 {
                    break;
                }
            }
            let erased_char: Vec<RawByte> = queue.line_buffer.drain(pos..).collect();
            if self.termios.has_local_flags(ECHO) {
                if self.termios.has_local_flags(ECHOPRT) {
                    if !self.erasing {
                        self.echo_raw_byte(b'\\');
                        self.erasing = true;
                    }
                    for &b in &erased_char {
                        self.echo_char(b);
                    }
                } else if erase_type == EraseType::Character && !self.termios.has_local_flags(ECHOE)
                {
                    self.echo_char(self.termios.c_cc[VERASE as usize]);
                } else if first_byte == b'\t' {
                    let mut num_chars = 0;
                    let mut after_tab = false;
                    for &b in queue.line_buffer.iter().rev() {
                        if b == b'\t' {
                            after_tab = true;
                            break;
                        } else if is_cntrl(b) {
                            if self.termios.has_local_flags(ECHOCTL) {
                                num_chars += 2;
                            }
                        } else if !is_utf8_continuation(b, &self.termios) {
                            num_chars += 1;
                        }
                    }
                    self.pending_echoes.push(EchoOp::EraseTab { num_chars, after_tab });
                } else {
                    if is_cntrl(first_byte) && self.termios.has_local_flags(ECHOCTL) {
                        self.echo_raw_byte(BACKSPACE_CHAR);
                        self.echo_raw_byte(b' ');
                        self.echo_raw_byte(BACKSPACE_CHAR);
                    }
                    if !is_cntrl(first_byte) || self.termios.has_local_flags(ECHOCTL) {
                        self.echo_raw_byte(BACKSPACE_CHAR);
                        self.echo_raw_byte(b' ');
                        self.echo_raw_byte(BACKSPACE_CHAR);
                    }
                }
            }
            if erase_type == EraseType::Character {
                break;
            }
        }
        if queue.line_buffer.is_empty() && self.termios.has_local_flags(ECHO) {
            self.finish_erasing();
        }
    }

    fn transform(
        &mut self,
        is_input: bool,
        queue: &mut Queue,
        buffer: &[RawByte],
    ) -> (usize, PendingSignals) {
        if is_input {
            self.transform_input(queue, buffer)
        } else {
            (self.transform_output(queue, buffer), PendingSignals::new())
        }
    }

    fn transform_output(&mut self, queue: &mut Queue, original_buffer: &[RawByte]) -> usize {
        if self.stopped && self.termios.has_input_flags(IXON) {
            return 0;
        }
        let mut buffer = original_buffer;

        // transform_output is effectively always in noncanonical mode, as the
        // main termios never has ICANON set.

        if !self.termios.has_output_flags(OPOST) {
            let limit = CANON_MAX_BYTES.saturating_sub(queue.readable_size());
            if limit == 0 {
                return 0;
            }
            let to_write = std::cmp::min(limit, buffer.len());
            queue
                .read_queue
                .push_back(ReadPacket { data: buffer[..to_write].to_vec(), has_eof: false });
            return to_write;
        }

        let mut return_value = 0;
        while !buffer.is_empty()
            && queue.readable_size() + queue.line_buffer.len() < CANON_MAX_BYTES
        {
            let c = buffer[0];
            return_value += 1;
            buffer = &buffer[1..];
            self.do_output_char(c, &mut queue.line_buffer);
        }
        if !queue.line_buffer.is_empty() {
            queue.flush_line_buffer();
        }
        return_value
    }

    fn transform_input(
        &mut self,
        queue: &mut Queue,
        original_buffer: &[RawByte],
    ) -> (usize, PendingSignals) {
        let mut buffer = original_buffer;

        let mut return_value = 0;
        let mut signals = PendingSignals::new();
        while !buffer.is_empty()
            && queue.buffer_len()
                < if self.is_canon_enabled() && queue.read_queue.is_empty() {
                    CANON_MAX_BYTES
                } else {
                    NON_CANON_MAX_BYTES
                }
        {
            let mut c = buffer[0];

            if self.termios.has_input_flags(ISTRIP) {
                c &= 0x7f;
            }
            if self.termios.has_input_flags(IUCLC) && self.termios.has_local_flags(IEXTEN) {
                c.make_ascii_lowercase();
            }

            // Step 1: Handle literal-next (VLNEXT) active from previous character.
            if self.lnext {
                self.lnext = false;
                if self.stopped
                    && self.termios.has_input_flags(IXON)
                    && self.termios.has_input_flags(IXANY)
                {
                    self.start_tty();
                }
                let parmrk_double = c == 0xff && self.termios.has_input_flags(PARMRK);
                let pushed_len = if parmrk_double { 2 } else { 1 };
                if queue.buffer_len() + pushed_len > NON_CANON_MAX_BYTES {
                    if self.is_canon_enabled() && queue.read_queue.is_empty() {
                        buffer = &buffer[1..];
                        return_value += 1;
                        continue;
                    }
                    break;
                }
                if self.termios.has_local_flags(ECHO) {
                    self.finish_erasing();
                    if queue.line_buffer.is_empty() {
                        self.echo_set_canon_col();
                    }
                    self.echo_char(c);
                }
                // Note: Linux `n_tty` pushes both `0xff` bytes directly into `read_buf` when
                // `PARMRK` is enabled (so canonical `eraser` and `VREPRINT` operate on each `0xff`
                // byte in `line_buffer`).
                if parmrk_double {
                    queue.line_buffer.extend_from_slice(&[0xff, 0xff]);
                } else {
                    queue.line_buffer.push(c);
                }
                buffer = &buffer[1..];
                return_value += 1;
                continue;
            }

            // Step 2: EXTPROC bypasses all special character and echo processing.
            if self.is_extproc_enabled() {
                if queue.buffer_len() + 1 > NON_CANON_MAX_BYTES {
                    break;
                }
                queue.line_buffer.push(c);
                buffer = &buffer[1..];
                return_value += 1;
                continue;
            }

            // Step 3: IXON flow control (VSTART / VSTOP) takes precedence over ISIG and ICANON.
            if self.termios.has_input_flags(IXON) {
                if c == self.termios.c_cc[VSTART as usize]
                    && self.termios.c_cc[VSTART as usize] != DISABLED_CHAR
                {
                    self.start_tty();
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }
                if c == self.termios.c_cc[VSTOP as usize]
                    && self.termios.c_cc[VSTOP as usize] != DISABLED_CHAR
                {
                    self.stop_tty();
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }
            }

            // Step 4: ISIG signal characters (VINTR, VQUIT, VSUSP).
            if let Some(signal) = self.handle_signals(c) {
                signals.add(signal);
                if !self.termios.has_local_flags(NOFLSH) {
                    queue.flush_buffers();
                    self.reset_input_state();
                    if let Some(ref mut output_queue) = self.output_queue {
                        output_queue.flush();
                    }
                    if self.packet_mode_enabled {
                        self.packet_mode_pending_events |=
                            (uapi::TIOCPKT_FLUSHREAD | uapi::TIOCPKT_FLUSHWRITE) as u8;
                    }
                }
                if self.termios.has_input_flags(IXON) {
                    self.start_tty();
                }
                if self.termios.has_local_flags(ECHO) {
                    self.echo_char(c);
                    self.commit_echoes();
                }
                buffer = &buffer[1..];
                return_value += 1;
                continue;
            }

            // Step 5: IXANY restarts output on any character not consumed above.
            if self.stopped
                && self.termios.has_input_flags(IXON)
                && self.termios.has_input_flags(IXANY)
            {
                self.start_tty();
            }

            // Step 6: CR/NL input translations (IGNCR, ICRNL, INLCR).
            match c {
                b'\r' => {
                    if self.termios.has_input_flags(IGNCR) {
                        buffer = &buffer[1..];
                        return_value += 1;
                        continue;
                    }
                    if self.termios.has_input_flags(ICRNL) {
                        c = b'\n';
                    }
                }
                b'\n' => {
                    if self.termios.has_input_flags(INLCR) {
                        c = b'\r';
                    }
                }
                _ => {}
            }

            // Step 7: Canonical mode special characters.
            if self.is_canon_enabled() {
                if self.termios.is_erase(c) || self.termios.is_kill(c) || self.termios.is_werase(c)
                {
                    self.eraser(queue, c);
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }

                if self.termios.has_local_flags(IEXTEN) {
                    if c == self.termios.c_cc[VLNEXT as usize]
                        && self.termios.c_cc[VLNEXT as usize] != DISABLED_CHAR
                    {
                        self.lnext = true;
                        if self.termios.has_local_flags(ECHO) {
                            self.finish_erasing();
                            if self.termios.has_local_flags(ECHOCTL) {
                                self.echo_raw_byte(b'^');
                                self.echo_raw_byte(BACKSPACE_CHAR);
                            }
                        }
                        buffer = &buffer[1..];
                        return_value += 1;
                        continue;
                    }

                    if c == self.termios.c_cc[VREPRINT as usize]
                        && self.termios.c_cc[VREPRINT as usize] != DISABLED_CHAR
                        && self.termios.has_local_flags(ECHO)
                    {
                        self.finish_erasing();
                        self.echo_char(c);
                        self.echo_raw_byte(b'\n');
                        for &b in &queue.line_buffer {
                            self.echo_char(b);
                        }
                        buffer = &buffer[1..];
                        return_value += 1;
                        continue;
                    }
                }

                if c == b'\n' {
                    if self.termios.has_local_flags(ECHO) || self.termios.has_local_flags(ECHONL) {
                        self.echo_raw_byte(b'\n');
                    }
                    queue.line_buffer.push(b'\n');
                    queue.flush_line_buffer();
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }

                if self.termios.is_eof(c) {
                    let data = std::mem::take(&mut queue.line_buffer);
                    queue.read_queue.push_back(ReadPacket { data, has_eof: true });
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }

                if self.termios.is_eol(c) {
                    let parmrk_double = c == 0xff && self.termios.has_input_flags(PARMRK);
                    let pushed_len = if parmrk_double { 2 } else { 1 };
                    let max_bytes = if queue.read_queue.is_empty() {
                        CANON_MAX_BYTES
                    } else {
                        NON_CANON_MAX_BYTES
                    };
                    if queue.buffer_len() + pushed_len > max_bytes {
                        if queue.read_queue.is_empty() {
                            buffer = &buffer[1..];
                            return_value += 1;
                            continue;
                        }
                        break;
                    }
                    // Matches Linux `n_tty_receive_char_special`: `EOL_CHAR` / `EOL2_CHAR` (like
                    // `\n`) does not call `finish_erasing()`; an open `ECHOPRT` `\.../` sequence is
                    // closed by the next normal character (or `VLNEXT` / `VREPRINT`).
                    if self.termios.has_local_flags(ECHO) {
                        if queue.line_buffer.is_empty() {
                            self.echo_set_canon_col();
                        }
                        self.echo_char(c);
                    }
                    if parmrk_double {
                        queue.line_buffer.extend_from_slice(&[0xff, 0xff]);
                    } else {
                        queue.line_buffer.push(c);
                    }
                    queue.flush_line_buffer();
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }
            }

            // Step 8: Normal character (or \n in non-canonical mode).
            let parmrk_double = c == 0xff && self.termios.has_input_flags(PARMRK);
            let pushed_len = if parmrk_double { 2 } else { 1 };
            if queue.buffer_len() + pushed_len > NON_CANON_MAX_BYTES {
                if self.is_canon_enabled() && queue.read_queue.is_empty() {
                    buffer = &buffer[1..];
                    return_value += 1;
                    continue;
                }
                break;
            }

            if self.termios.has_local_flags(ECHO) {
                self.finish_erasing();
                if queue.line_buffer.is_empty() {
                    self.echo_set_canon_col();
                }
                self.echo_char(c);
            }

            if parmrk_double {
                queue.line_buffer.extend_from_slice(&[0xff, 0xff]);
            } else {
                queue.line_buffer.push(c);
            }
            buffer = &buffer[1..];
            return_value += 1;
        }

        self.commit_echoes();

        // In noncanonical mode (or EXTPROC), everything is immediately readable.
        if !self.is_canon_enabled() && !queue.line_buffer.is_empty() {
            queue.flush_line_buffer();
        }

        (return_value, signals)
    }
}

/// Alias used to mark bytes in the queues that have not yet been processed and pushed into the
/// read buffer. See `Queue`.
type RawByte = u8;

#[derive(Debug, Default)]
struct ReadPacket {
    data: Vec<u8>,
    has_eof: bool,
}

#[derive(Debug, Default)]
struct Queue {
    /// The queue of data ready to be read. Each element is a "datagram" (line or chunk).
    /// Empty byte vectors represent EOF markers (read returns 0).
    read_queue: VecDeque<ReadPacket>,

    /// The incomplete line/chunk being processed but not yet ready for the read_queue.
    /// In Canonical mode, this holds the current line being edited.
    /// In Non-Canonical mode, this holds data until it is pushed to the read_queue.
    line_buffer: Vec<u8>,

    /// Data that can't fit into readBuf. It is put here until it can be loaded into the read
    /// buffer. Contains data that hasn't been processed.
    wait_buffers: VecDeque<Vec<RawByte>>,

    /// The length of the data in `wait_buffers`.
    total_wait_buffer_length: usize,

    /// Whether this queue in the input queue. Needed to know how to transform received data.
    is_input: bool,
}

impl Queue {
    fn output_queue() -> Option<Self> {
        Some(Queue { is_input: false, ..Default::default() })
    }

    fn input_queue() -> Option<Self> {
        Some(Queue { is_input: true, ..Default::default() })
    }

    /// Returns whether the queue is ready to be written to.
    fn write_readiness(&self) -> FdEvents {
        if self.total_wait_buffer_length < WAIT_BUFFER_MAX_BYTES {
            FdEvents::POLLOUT
        } else {
            FdEvents::empty()
        }
    }

    /// Returns whether the queue is ready to be read from.
    fn read_readiness(&self) -> FdEvents {
        // If there's an empty "datagram" in read_queue, it means EOF, which is "readable" (returns 0).
        if !self.read_queue.is_empty() { FdEvents::POLLIN } else { FdEvents::empty() }
    }

    /// Returns the number of bytes ready to be read.
    fn readable_size(&self) -> usize {
        // We sum up everything in the read_queue.
        // NOTE: This might over-report if we only return one datagram at a time, but for poll/FIONREAD it's generally answering "how much is there".
        self.read_queue.iter().map(|p| p.data.len()).sum()
    }

    /// Returns the total buffer occupancy in `read_queue` (including 1 byte per `VEOF` marker,
    /// matching `__DISABLED_CHAR` in Linux `n_tty`'s `read_buf`) plus `line_buffer`.
    fn buffer_len(&self) -> usize {
        self.read_queue.iter().map(|p| p.data.len() + usize::from(p.has_eof)).sum::<usize>()
            + self.line_buffer.len()
    }

    /// Read from the queue into `data`. Returns the number of bytes copied and any pending signals
    /// generated by draining the wait buffer.
    fn read(
        &mut self,
        terminal: &mut LineDiscipline,
        data: &mut dyn OutputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        if self.read_queue.is_empty() {
            return error!(EAGAIN);
        }
        if data.available() == 0 {
            return Ok((0, PendingSignals::new()));
        }

        let mut total_written = 0;
        while let Some(mut packet) = self.read_queue.pop_front() {
            if packet.data.is_empty() {
                if total_written > 0 {
                    // We've already read some data. We need to complete the read with that data and
                    // leave the empty datagram in the queue to signal EOF on the next read.
                    self.read_queue.push_front(packet);
                }
                break;
            }

            match data.write(&packet.data) {
                Ok(written) => {
                    total_written += written;
                    if written < packet.data.len() {
                        // Put back the unread part, preserving the EOF marker on the tail.
                        let remaining = packet.data.split_off(written);
                        self.read_queue
                            .push_front(ReadPacket { data: remaining, has_eof: packet.has_eof });
                        // Destination full.
                        break;
                    }

                    // If we are in canonical input mode (and not EXTPROC), or this packet was
                    // terminated by VEOF, we stop after one packet (one line).
                    if (self.is_input && terminal.is_canon_enabled()) || packet.has_eof {
                        break;
                    }
                }
                Err(e) => {
                    // If write failed, push back the whole packet.
                    self.read_queue.push_front(packet);
                    if total_written > 0 {
                        // If we managed to write something before error, return success.
                        let signals = self.drain_waiting_buffer(terminal);
                        return Ok((total_written, signals));
                    }
                    return Err(e);
                }
            }
        }

        let signals = self.drain_waiting_buffer(terminal);
        Ok((total_written, signals))
    }

    /// Writes to the queue from `data`. Returns the number of bytes copied.
    fn write(
        &mut self,
        terminal: &mut LineDiscipline,
        data: &mut dyn InputBuffer,
    ) -> Result<(usize, PendingSignals), Errno> {
        let room = WAIT_BUFFER_MAX_BYTES - self.total_wait_buffer_length;
        let data_length = data.available();
        if room == 0 && data_length > 0 {
            return error!(EAGAIN);
        }
        let buffer = data.read_to_vec_exact(std::cmp::min(room, data_length))?;
        let read_from_userspace = buffer.len();
        let signals = self.push_to_waiting_buffer(terminal, buffer);
        Ok((read_from_userspace, signals))
    }

    /// Pushes the given buffer into the wait_buffers, and process the wait_buffers.
    fn push_to_waiting_buffer(
        &mut self,
        terminal: &mut LineDiscipline,
        buffer: Vec<RawByte>,
    ) -> PendingSignals {
        self.total_wait_buffer_length += buffer.len();
        self.wait_buffers.push_back(buffer);
        self.drain_waiting_buffer(terminal)
    }

    /// Processes the wait_buffers, filling the read buffer.
    fn drain_waiting_buffer(&mut self, terminal: &mut LineDiscipline) -> PendingSignals {
        let mut signals_to_return = PendingSignals::new();
        while let Some(wait_buffer) = self.wait_buffers.pop_front() {
            self.total_wait_buffer_length -= wait_buffer.len();
            let (count, signals) = terminal.transform(self.is_input, self, &wait_buffer);
            signals_to_return.append(signals);
            if count != wait_buffer.len() {
                let remaining = wait_buffer[count..].to_vec();
                self.total_wait_buffer_length += remaining.len();
                self.wait_buffers.push_front(remaining);
                break;
            }
        }
        signals_to_return
    }

    /// Flushed the line buffer to the read queue.
    fn flush_line_buffer(&mut self) {
        self.read_queue
            .push_back(ReadPacket { data: std::mem::take(&mut self.line_buffer), has_eof: false });
    }

    /// Flush the content of the queue.
    fn flush(&mut self) {
        self.flush_buffers();
        self.flush_unprocessed();
    }

    /// Flush the processed read queue and in-progress line buffer.
    fn flush_buffers(&mut self) {
        self.read_queue.clear();
        self.line_buffer.clear();
    }

    /// Flush only the part of the queue which has not yet been processed.
    fn flush_unprocessed(&mut self) {
        self.wait_buffers.clear();
        self.total_wait_buffer_length = 0;
    }

    /// Called when canonical mode or EXTPROC changes on the input queue.
    fn on_canon_mode_changed(&mut self, terminal: &mut LineDiscipline) -> PendingSignals {
        let mut combined = Vec::new();
        for packet in self.read_queue.drain(..) {
            combined.extend(packet.data);
            if packet.has_eof {
                combined.push(DISABLED_CHAR);
            }
        }
        combined.append(&mut self.line_buffer);
        if !combined.is_empty() {
            let has_eof = terminal.is_canon_enabled() && combined.last() == Some(&DISABLED_CHAR);
            if has_eof {
                combined.pop();
            }
            self.read_queue.push_back(ReadPacket { data: combined, has_eof });
        }
        self.drain_waiting_buffer(terminal)
    }
}

// Helper functions (copied from terminal.rs)
// Returns the ASCII representation of the given char. This will assert if the character is not
// ascii.
fn get_ascii(c: char) -> u8 {
    let mut dest: [u8; 1] = [0];
    c.encode_utf8(&mut dest);
    dest[0]
}

// Returns the control character associated with the given letter.
fn get_control_character(c: char) -> cc_t {
    get_ascii(c) - get_ascii('A') + 1
}

// Returns the default control characters of a terminal.
fn get_default_control_characters() -> [cc_t; 19usize] {
    [
        get_control_character('C'),  // VINTR = ^C
        get_control_character('\\'), // VQUIT = ^\
        get_ascii('\x7f'),           // VERASE = DEL
        get_control_character('U'),  // VKILL = ^U
        get_control_character('D'),  // VEOF = ^D
        0,                           // VTIME
        1,                           // VMIN
        0,                           // VSWTC
        get_control_character('Q'),  // VSTART = ^Q
        get_control_character('S'),  // VSTOP = ^S
        get_control_character('Z'),  // VSUSP = ^Z
        0,                           // VEOL
        get_control_character('R'),  // VREPRINT = ^R
        get_control_character('O'),  // VDISCARD = ^O
        get_control_character('W'),  // VWERASE = ^W
        get_control_character('V'),  // VLNEXT = ^V
        0,                           // VEOL2
        0,                           // Remaining data in the array,
        0,                           // Remaining data in the array,
    ]
}

const DEFAULT_SPEED: u32 = 38400;

// Returns the default replica terminal configuration.
pub fn get_default_termios() -> uapi::termios2 {
    uapi::termios2 {
        c_iflag: uapi::ICRNL | uapi::IXON,
        c_oflag: uapi::OPOST | uapi::ONLCR,
        c_cflag: uapi::B38400 | uapi::CS8 | uapi::CREAD,
        c_lflag: uapi::ISIG
            | uapi::ICANON
            | uapi::ECHO
            | uapi::ECHOE
            | uapi::ECHOK
            | uapi::ECHOCTL
            | uapi::ECHOKE
            | uapi::IEXTEN,
        c_line: 0,
        c_cc: get_default_control_characters(),
        c_ispeed: DEFAULT_SPEED,
        c_ospeed: DEFAULT_SPEED,
    }
}

/// Helper trait for termios to help parse the configuration.
trait TermIOS {
    fn has_input_flags(&self, flags: tcflag_t) -> bool;
    fn has_output_flags(&self, flags: tcflag_t) -> bool;
    fn has_local_flags(&self, flags: tcflag_t) -> bool;
    fn is_eof(&self, c: RawByte) -> bool;
    fn is_erase(&self, c: RawByte) -> bool;
    fn is_werase(&self, c: RawByte) -> bool;
    fn is_kill(&self, c: RawByte) -> bool;
    fn is_eol(&self, c: RawByte) -> bool;
    fn signal(&self, c: RawByte) -> Option<Signal>;
}

impl TermIOS for uapi::termios2 {
    fn has_input_flags(&self, flags: tcflag_t) -> bool {
        self.c_iflag & flags == flags
    }
    fn has_output_flags(&self, flags: tcflag_t) -> bool {
        self.c_oflag & flags == flags
    }
    fn has_local_flags(&self, flags: tcflag_t) -> bool {
        self.c_lflag & flags == flags
    }
    fn is_eof(&self, c: RawByte) -> bool {
        c == self.c_cc[VEOF as usize] && self.c_cc[VEOF as usize] != DISABLED_CHAR
    }
    fn is_erase(&self, c: RawByte) -> bool {
        c == self.c_cc[VERASE as usize] && self.c_cc[VERASE as usize] != DISABLED_CHAR
    }
    fn is_werase(&self, c: RawByte) -> bool {
        c == self.c_cc[VWERASE as usize]
            && self.c_cc[VWERASE as usize] != DISABLED_CHAR
            && self.has_local_flags(IEXTEN)
    }
    fn is_kill(&self, c: RawByte) -> bool {
        c == self.c_cc[VKILL as usize] && self.c_cc[VKILL as usize] != DISABLED_CHAR
    }
    fn is_eol(&self, c: RawByte) -> bool {
        if c == DISABLED_CHAR {
            return false;
        }
        if c == self.c_cc[VEOL as usize] {
            return true;
        }
        if c == self.c_cc[VEOL2 as usize] {
            return self.has_local_flags(IEXTEN);
        }
        false
    }
    fn signal(&self, c: RawByte) -> Option<Signal> {
        if c == DISABLED_CHAR {
            return None;
        }
        if c == self.c_cc[VINTR as usize] {
            return Some(SIGINT);
        }
        if c == self.c_cc[VQUIT as usize] {
            return Some(SIGQUIT);
        }
        if c == self.c_cc[VSUSP as usize] {
            return Some(SIGTSTP);
        }
        None
    }
}

fn is_cntrl(c: RawByte) -> bool {
    c <= 0x1f || c == 0x7f
}

fn is_linux_alnum_or_underscore(c: RawByte) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || matches!(c, 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0xff)
}

fn is_utf8_continuation(c: RawByte, termios: &uapi::termios2) -> bool {
    termios.has_input_flags(IUTF8) && (c & 0xc0) == 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[::fuchsia::test]
    fn test_ascii_conversion() {
        assert_eq!(get_ascii(' '), 32);
    }

    #[::fuchsia::test]
    fn test_control_character() {
        assert_eq!(get_control_character('C'), 3);
    }

    #[::fuchsia::test]
    fn test_signal_handling_with_disabled_chars() {
        let mut termios = get_default_termios();
        assert_eq!(termios.signal(3), Some(SIGINT));
        assert_eq!(termios.signal(28), Some(SIGQUIT));
        assert_eq!(termios.signal(26), Some(SIGTSTP));

        termios.c_cc[VINTR as usize] = DISABLED_CHAR;
        termios.c_cc[VQUIT as usize] = DISABLED_CHAR;
        termios.c_cc[VSUSP as usize] = DISABLED_CHAR;

        assert_eq!(termios.signal(0), None);
        assert_eq!(termios.signal(3), None); // Normally ^C (SIGINT)
        assert_eq!(termios.signal(28), None); // Normally ^\ (SIGQUIT)
        assert_eq!(termios.signal(26), None); // Normally ^Z (SIGTSTP)
    }

    struct TestBuffer {
        data: Vec<u8>,
    }

    impl InputBuffer for TestBuffer {
        fn available(&self) -> usize {
            self.data.len()
        }
        fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
            if size > self.data.len() {
                return error!(EAGAIN);
            }
            Ok(self.data.drain(0..size).collect())
        }
    }

    impl OutputBuffer for TestBuffer {
        fn available(&self) -> usize {
            usize::MAX
        }
        fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
            self.data.extend_from_slice(data);
            Ok(data.len())
        }
    }

    #[::fuchsia::test]
    fn test_flush() {
        fn make_ld() -> LineDiscipline {
            let mut ld = LineDiscipline::default();
            ld.main_open();
            ld.replica_open();

            let mut termios = get_default_termios();
            termios.c_lflag &= !ECHO;
            termios.c_oflag &= !OPOST;
            let _ = ld.set_termios(termios);

            // Write some data from main to the replica.
            // This goes to input_queue.
            let mut input = TestBuffer { data: b"ping\n".to_vec() };
            let (written, signals) = ld.main_write(&mut input).unwrap();
            assert_eq!(written, 5);
            assert!(signals.signals().is_empty());

            // Write some data from replica to main.
            // This goes to output_queue.
            let mut output = TestBuffer { data: b"pong\n".to_vec() };
            let written = ld.replica_write(&mut output).unwrap();
            assert_eq!(written, 5);
            ld
        }

        let mut read_buf = TestBuffer { data: vec![] };

        // A TCIFLUSH from the main side should flush only the main's input (output_queue)
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCIFLUSH).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
        assert!(read_buf.data.is_empty());
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIFLUSH from the replica side should flush only the replica's input (input_queue)
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCIFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.replica_read(&mut read_buf));
        assert!(read_buf.data.is_empty());

        // A TCOFLUSH from the main side should do nothing (instantaneous transmission)
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCOFLUSH from the replica side should do nothing
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIOFLUSH from main should flush only main's input (output_queue)
        let mut ld = make_ld();
        ld.flush(TerminalSide::Main, uapi::TCIOFLUSH).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
        read_buf.data.clear();
        assert!(ld.replica_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"ping\n");

        // A TCIOFLUSH from replica should flush only replica's input (input_queue)
        let mut ld = make_ld();
        ld.flush(TerminalSide::Replica, uapi::TCIOFLUSH).unwrap();
        read_buf.data.clear();
        assert!(ld.main_read(&mut read_buf).is_ok());
        assert_eq!(read_buf.data, b"pong\n");
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.replica_read(&mut read_buf));

        // A TCIFLUSH from replica while stopped via IXON should also clear pending_echoes.
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();
        let mut stop_and_text = TestBuffer { data: b"\x13stale\n".to_vec() };
        let _ = ld.main_write(&mut stop_and_text).unwrap();
        ld.flush(TerminalSide::Replica, uapi::TCIFLUSH).unwrap();
        let mut start_in = TestBuffer { data: b"\x11".to_vec() };
        let _ = ld.main_write(&mut start_in).unwrap();
        read_buf.data.clear();
        assert_eq!(error!(EAGAIN), ld.main_read(&mut read_buf));
    }

    #[::fuchsia::test]
    fn test_canonical_max_line_length_with_erase_and_eof() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Write 4096 'A's (only 4095 fit), then backspace (erases 4095th 'A'), then 'B', then '\n'.
        let mut payload = vec![b'A'; 4096];
        payload.extend_from_slice(b"\x7fB\n");
        let mut input = TestBuffer { data: payload };
        let (written, signals) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4099);
        assert!(signals.signals().is_empty());

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert!(signals.signals().is_empty());
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(&replica_out.data[4094..], b"B\n");

        let mut main_out = TestBuffer { data: vec![] };
        let _ = ld.main_read(&mut main_out).unwrap();
        assert_eq!(&main_out.data[..4095], &[b'A'; 4095][..]);
        assert_eq!(&main_out.data[4095..], b"\x08 \x08B\r\n");

        // Also test 4096 'C's terminated by VEOF (^D): replica should read 4095 'C's.
        let mut payload_eof = vec![b'C'; 4096];
        payload_eof.push(4);
        let mut input_eof = TestBuffer { data: payload_eof };
        let (written, _) = ld.main_write(&mut input_eof).unwrap();
        assert_eq!(written, 4097);

        replica_out.data.clear();
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert!(signals.signals().is_empty());
        assert_eq!(replica_out.data, vec![b'C'; 4095]);
        replica_out.data.clear();
        assert_eq!(ld.replica_read(&mut replica_out), error!(EAGAIN));
    }

    #[::fuchsia::test]
    fn test_pty_cflag_enforced() {
        let mut ld = LineDiscipline::default();
        let mut termios = get_default_termios();
        termios.c_cflag = uapi::CS5 | uapi::PARENB;
        let _ = ld.set_termios(termios);
        assert_eq!(ld.termios().c_cflag & CSIZE, CS8);
        assert_eq!(ld.termios().c_cflag & CREAD, CREAD);
        assert_eq!(ld.termios().c_cflag & PARENB, 0);
    }

    #[::fuchsia::test]
    fn test_close_events_and_eio() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        ld.replica_close();
        assert!(ld.is_replica_closed());
        assert_eq!(ld.main_query_events(), FdEvents::POLLOUT | FdEvents::POLLHUP);
        let mut read_buf = TestBuffer { data: vec![] };
        assert_eq!(ld.main_read(&mut read_buf), error!(EIO));

        ld.replica_open();
        let mut main_in = TestBuffer { data: b"unread\n".to_vec() };
        let _ = ld.main_write(&mut main_in).unwrap();
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 7);

        ld.main_close();
        assert!(ld.is_main_closed());
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 0);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 0);
        assert_eq!(
            ld.replica_query_events(),
            FdEvents::POLLIN | FdEvents::POLLOUT | FdEvents::POLLERR | FdEvents::POLLHUP
        );
        let (n, signals) = ld.replica_read(&mut read_buf).unwrap();
        assert_eq!(n, 0);
        assert!(signals.signals().is_empty());
        let mut write_buf = TestBuffer { data: b"x".to_vec() };
        assert_eq!(ld.replica_write(&mut write_buf), error!(EIO));
    }

    #[::fuchsia::test]
    fn test_parmrk_capacity_limits() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        let mut termios = get_default_termios();
        termios.c_iflag |= PARMRK;
        termios.c_lflag &= !ECHO;
        termios.c_cc[VEOL as usize] = b';';
        let _ = ld.set_termios(termios);

        // Canonical mode: 4094 'A's + 0xff (would require 2 bytes = 4096, leaving no room for EOL)
        // must drop the 0xff so the subsequent '\n' still terminates a 4095-byte line.
        let mut payload = vec![b'A'; 4094];
        payload.extend_from_slice(&[0xff, b'\n']);
        let mut input = TestBuffer { data: payload };
        let (written, _) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4096);

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(replica_out.data[4094], b'\n');

        // Canonical mode with VEOL = 0xff: 4095 'A's + 0xff (2 bytes > 4096) must not exceed
        // CANON_MAX_BYTES.
        termios.c_cc[VEOL as usize] = 0xff;
        let _ = ld.set_termios(termios);
        let mut payload_veol = vec![b'A'; 4095];
        payload_veol.extend_from_slice(&[0xff, b'\n']);
        let mut input_veol = TestBuffer { data: payload_veol };
        let _ = ld.main_write(&mut input_veol).unwrap();
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&replica_out.data[..4095], &[b'A'; 4095][..]);
        assert_eq!(replica_out.data[4095], b'\n');

        // Non-canonical mode: 4094 'A's + 0xff (2 bytes -> 4096 > NON_CANON_MAX_BYTES = 4095)
        // must stop before the 0xff so the first chunk is 4094 bytes and the second is [0xff, 0xff].
        termios.c_lflag &= !ICANON;
        let _ = ld.set_termios(termios);
        let mut payload_noncanon = vec![b'A'; 4094];
        payload_noncanon.push(0xff);
        let mut input_noncanon = TestBuffer { data: payload_noncanon };
        let (written, _) = ld.main_write(&mut input_noncanon).unwrap();
        assert_eq!(written, 4095);
        assert_eq!(ld.get_available_read_size(TerminalSide::Replica), 4094);
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4094);
        replica_out.data.clear();
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 2);
        assert_eq!(replica_out.data, vec![0xff, 0xff]);
    }

    #[::fuchsia::test]
    fn test_stopped_output_wait_buffer_not_drained_until_start() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Write 5000 'A's from replica: 4096 are transformed into output_queue.read_queue and 904
        // remain in output_queue.wait_buffers.
        let mut replica_in = TestBuffer { data: vec![b'A'; 5000] };
        let written = ld.replica_write(&mut replica_in).unwrap();
        assert_eq!(written, 5000);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 4096);

        // Send VSTOP (^S) from main to stop output.
        let mut stop_in = TestBuffer { data: vec![0x13] };
        let (written, signals) = ld.main_write(&mut stop_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());

        // Reading from main consumes the 4096 already-transformed bytes, but must NOT drain the
        // remaining 904 bytes from output_queue.wait_buffers while stopped.
        let mut main_out = TestBuffer { data: vec![] };
        let n = ld.main_read(&mut main_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(main_out.data.len(), 4096);
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 0);

        main_out.data.clear();
        assert_eq!(ld.main_read(&mut main_out), error!(EAGAIN));

        // Send VSTART (^Q) from main to resume output, which drains output_queue.wait_buffers.
        let mut start_in = TestBuffer { data: vec![0x11] };
        let (written, signals) = ld.main_write(&mut start_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());
        assert_eq!(ld.get_available_read_size(TerminalSide::Main), 904);

        let n = ld.main_read(&mut main_out).unwrap();
        assert_eq!(n, 904);
        assert_eq!(main_out.data, vec![b'A'; 904]);
    }

    #[::fuchsia::test]
    fn test_veof_and_unread_queue_buffer_capacity() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Each empty VEOF (^D) occupies 1 byte in `buffer_len()` (matching `__DISABLED_CHAR` in
        // Linux `n_tty`), so at most `NON_CANON_MAX_BYTES` (4095) empty VEOF packets are drained
        // into `read_queue` while the 4096th remains in `wait_buffers`.
        let mut eofs = TestBuffer { data: vec![0x04; 4096] };
        let (written, _) = ld.main_write(&mut eofs).unwrap();
        assert_eq!(written, 4096);
        assert_eq!(ld.input_queue().read_queue.len(), NON_CANON_MAX_BYTES);
        assert_eq!(ld.input_queue().total_wait_buffer_length, 1);

        // Reading 1 VEOF frees 1 slot and drains the 4096th VEOF from `wait_buffers`.
        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 0);
        assert_eq!(ld.input_queue().read_queue.len(), NON_CANON_MAX_BYTES);
        assert_eq!(ld.input_queue().total_wait_buffer_length, 0);
    }

    #[::fuchsia::test]
    fn test_iutf8_multibyte_at_canon_boundary() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        let mut termios = get_default_termios();
        termios.c_iflag |= IUTF8;
        termios.c_lflag &= !ECHO;
        let _ = ld.set_termios(termios);

        // 4094 'A's + "\xc3\xa9\n": Linux `n_tty` accepts the first byte 0xc3 (reaching 4095
        // non-EOL bytes), drops 0xa9, and accepts '\n' for a 4096-byte line.
        let mut payload = vec![b'A'; 4094];
        payload.extend_from_slice(b"\xc3\xa9\n");
        let mut input = TestBuffer { data: payload };
        let (written, _) = ld.main_write(&mut input).unwrap();
        assert_eq!(written, 4097);

        let mut replica_out = TestBuffer { data: vec![] };
        let (n, _) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(&replica_out.data[..4094], &[b'A'; 4094][..]);
        assert_eq!(&replica_out.data[4094..], b"\xc3\n");
    }

    #[::fuchsia::test]
    fn test_signal_flush_preserves_subsequent_wait_buffers() {
        let mut ld = LineDiscipline::default();
        ld.main_open();
        ld.replica_open();

        // Fill `read_queue` to 4095 bytes (so `buffer_len() == NON_CANON_MAX_BYTES` and subsequent
        // writes queue in `wait_buffers`).
        let mut fill = vec![b'A'; 4094];
        fill.push(b'\n');
        let mut fill_in = TestBuffer { data: fill };
        let (written, _) = ld.main_write(&mut fill_in).unwrap();
        assert_eq!(written, 4095);

        // Queue two separate `main_write` buffers while full: one with VINTR (^C) and one with
        // "after\n".
        let mut sig_in = TestBuffer { data: b"\x03".to_vec() };
        let (written, signals) = ld.main_write(&mut sig_in).unwrap();
        assert_eq!(written, 1);
        assert!(signals.signals().is_empty());

        let mut after_in = TestBuffer { data: b"after\n".to_vec() };
        let (written, signals) = ld.main_write(&mut after_in).unwrap();
        assert_eq!(written, 6);
        assert!(signals.signals().is_empty());

        // Reading the initial 4095-byte line drains `wait_buffers`: `^C` raises SIGINT and flushes
        // any prior input, then `"after\n"` is processed into `read_queue`.
        let mut replica_out = TestBuffer { data: vec![] };
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 4095);
        assert_eq!(signals.signals(), &[SIGINT]);

        replica_out.data.clear();
        let (n, signals) = ld.replica_read(&mut replica_out).unwrap();
        assert_eq!(n, 6);
        assert!(signals.signals().is_empty());
        assert_eq!(replica_out.data, b"after\n");
    }
}

pub mod testing;
