// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A small FXT (Fuchsia trace format) writer for test fixtures.
//!
//! Writes only the records zpo reads: thread and process names, context
//! switches and wakeups, duration events, flows and counters. See
//! //docs/reference/tracing/trace-format.md.

/// Builds an .fxt file in memory.
#[derive(Default)]
pub struct Writer {
    words: Vec<u64>,
}

/// An argument of an event, kernel object or scheduling record.
pub enum Arg<'a> {
    I32(&'a str, i32),
    U64(&'a str, u64),
    Koid(&'a str, u64),
}

fn inline_ref(s: &str) -> u64 {
    0x8000 | s.len() as u64
}

fn str_words(s: &str) -> Vec<u64> {
    let mut b = s.as_bytes().to_vec();
    while b.len() % 8 != 0 {
        b.push(0);
    }
    b.chunks(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
}

fn arg_words(a: &Arg<'_>) -> Vec<u64> {
    let (name, ty, inline_val, extra): (&str, u64, u64, Option<u64>) = match *a {
        Arg::I32(n, v) => (n, 1, v as u32 as u64, None),
        Arg::U64(n, v) => (n, 4, 0, Some(v)),
        Arg::Koid(n, v) => (n, 8, 0, Some(v)),
    };
    let name_w = str_words(name);
    let size = 1 + name_w.len() + extra.is_some() as usize;
    let mut out = vec![ty | (size as u64) << 4 | inline_ref(name) << 16 | inline_val << 32];
    out.extend(name_w);
    out.extend(extra);
    out
}

fn args_words(args: &[Arg<'_>]) -> Vec<u64> {
    args.iter().flat_map(arg_words).collect()
}

/// Duration event kinds.
const INSTANT: u64 = 0;
const COUNTER: u64 = 1;
const COMPLETE: u64 = 4;
const FLOW_BEGIN: u64 = 8;
const FLOW_END: u64 = 10;

/// The thread state a context switch leaves a blocked thread in.
pub const BLOCKED: u64 = 3;
/// The weight of the idle thread.
pub const IDLE: i32 = i32::MIN;

impl Writer {
    /// A trace with the magic record, one provider and 1 GHz ticks.
    pub fn new() -> Self {
        let mut w = Writer { words: vec![0x0016_5478_4604_0010] };
        let name = "zpo";
        let nw = str_words(name);
        w.push_record(0, &[(1 << 16) | (1 << 20) | ((name.len() as u64) << 52)], &nw);
        w.push_record(1, &[], &[1_000_000_000]);
        w
    }

    /// Appends a record: `header_bits` are OR'd into the header word after the
    /// type and size fields are set.
    fn push_record(&mut self, ty: u64, header_bits: &[u64], body: &[u64]) {
        let size = 1 + body.len() as u64;
        let bits = header_bits.iter().fold(0, |a, b| a | b);
        self.words.push(ty | size << 4 | bits);
        self.words.extend_from_slice(body);
    }

    pub fn process(&mut self, koid: u64, name: &str) {
        let mut body = vec![koid];
        body.extend(str_words(name));
        self.push_record(7, &[1 << 16, inline_ref(name) << 24], &body);
    }

    pub fn thread(&mut self, koid: u64, pid: u64, name: &str) {
        let mut body = vec![koid];
        body.extend(str_words(name));
        body.extend(args_words(&[Arg::Koid("process", pid)]));
        self.push_record(7, &[2 << 16, inline_ref(name) << 24, 1 << 40], &body);
    }

    /// A context switch on `cpu` at `ts` from `out` (left in `out_state`) to
    /// `inc`; weights are the fair weight or the packed deadline parameters.
    pub fn switch(
        &mut self,
        ts: u64,
        cpu: u64,
        out: u64,
        out_weight: i32,
        out_state: u64,
        inc: u64,
        in_weight: i32,
    ) {
        let mut body = vec![ts, out, inc];
        body.extend(args_words(&[
            Arg::I32("incoming_weight", in_weight),
            Arg::I32("outgoing_weight", out_weight),
        ]));
        self.push_record(8, &[2 << 16, cpu << 20, out_state << 36, 1 << 60], &body);
    }

    /// A wakeup of `tid` on `cpu`; `waker` is None for a timer or interrupt.
    pub fn wakeup(&mut self, ts: u64, cpu: u64, tid: u64, weight: i32, waker: Option<u64>) {
        let mut args = vec![Arg::I32("weight", weight)];
        if let Some(w) = waker {
            args.push(Arg::Koid("waker", w));
        }
        let mut body = vec![ts, tid];
        body.extend(args_words(&args));
        self.push_record(8, &[(args.len() as u64) << 16, cpu << 20, 2 << 60], &body);
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &mut self,
        kind: u64,
        ts: u64,
        pid: u64,
        tid: u64,
        cat: &str,
        name: &str,
        args: &[Arg<'_>],
        extra: &[u64],
    ) {
        let mut body = vec![ts, pid, tid];
        body.extend(str_words(cat));
        body.extend(str_words(name));
        body.extend(args_words(args));
        body.extend_from_slice(extra);
        self.push_record(
            4,
            &[kind << 16, (args.len() as u64) << 20, inline_ref(cat) << 32, inline_ref(name) << 48],
            &body,
        );
    }

    /// A slice [ts, end) on a thread.
    pub fn complete(
        &mut self,
        ts: u64,
        end: u64,
        pid: u64,
        tid: u64,
        cat: &str,
        name: &str,
        args: &[Arg<'_>],
    ) {
        self.event(COMPLETE, ts, pid, tid, cat, name, args, &[end]);
    }

    pub fn instant(&mut self, ts: u64, pid: u64, tid: u64, cat: &str, name: &str) {
        self.event(INSTANT, ts, pid, tid, cat, name, &[], &[]);
    }

    pub fn flow_begin(&mut self, ts: u64, pid: u64, tid: u64, cat: &str, name: &str, id: u64) {
        self.event(FLOW_BEGIN, ts, pid, tid, cat, name, &[], &[id]);
    }

    pub fn flow_end(&mut self, ts: u64, pid: u64, tid: u64, cat: &str, name: &str, id: u64) {
        self.event(FLOW_END, ts, pid, tid, cat, name, &[], &[id]);
    }

    /// A counter sample: the track is `<name>:<arg>:<id>`.
    #[allow(clippy::too_many_arguments)]
    pub fn counter(
        &mut self,
        ts: u64,
        pid: u64,
        tid: u64,
        cat: &str,
        name: &str,
        arg: &str,
        id: u64,
        value: u64,
    ) {
        self.event(COUNTER, ts, pid, tid, cat, name, &[Arg::U64(arg, value)], &[id]);
    }

    pub fn bytes(&self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}
