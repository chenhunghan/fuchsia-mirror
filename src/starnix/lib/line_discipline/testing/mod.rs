// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::*;
use serde::Deserialize;
use starnix_uapi::errors::Errno;
use std::collections::HashMap;

#[derive(Deserialize, Debug)]
struct Scenario {
    name: String,
    initial_termios: TermiosConfig,
    events: Vec<Event>,
    final_termios: TermiosConfig,
}

#[derive(Deserialize, Debug)]
struct TermiosConfig {
    #[serde(default)]
    c_iflag: Vec<String>,
    #[serde(default)]
    c_oflag: Vec<String>,
    #[serde(default)]
    c_lflag: Vec<String>,
    #[allow(dead_code)]
    c_cflag: Option<u32>, // Keeping cflag simple for now or assume default
    c_cc: Option<HashMap<String, u8>>,
}

#[derive(Deserialize, Debug)]
#[serde(untagged)]
enum TraceData {
    Bytes(Vec<u8>),
    String(String),
}

impl TraceData {
    fn to_bytes(&self) -> Vec<u8> {
        match self {
            TraceData::Bytes(b) => b.clone(),
            TraceData::String(s) => s.as_bytes().to_vec(),
        }
    }
}

fn default_read_size() -> usize {
    4096
}

#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
enum Event {
    #[serde(rename = "write_to_master")]
    WriteToMaster {
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "write_to_master_blocked")]
    WriteToMasterBlocked { data: TraceData },
    #[serde(rename = "read_from_master")]
    ReadFromMaster { data: TraceData },
    #[serde(rename = "read_from_slave")]
    ReadFromSlave {
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "read_once_from_master")]
    ReadOnceFromMaster {
        #[serde(default = "default_read_size")]
        size: usize,
        data: TraceData,
    },
    #[serde(rename = "read_once_from_master_blocked")]
    ReadOnceFromMasterBlocked {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "read_once_from_master_eio")]
    ReadOnceFromMasterEio {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "read_once_from_slave")]
    ReadOnceFromSlave {
        #[serde(default = "default_read_size")]
        size: usize,
        data: TraceData,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "read_once_from_slave_blocked")]
    ReadOnceFromSlaveBlocked {
        #[serde(default = "default_read_size")]
        size: usize,
    },
    #[serde(rename = "write_to_slave")]
    WriteToSlave { data: TraceData },
    #[serde(rename = "write_to_slave_blocked")]
    WriteToSlaveBlocked { data: TraceData },
    #[serde(rename = "write_to_slave_eio")]
    WriteToSlaveEio { data: TraceData },
    #[serde(rename = "set_packet_mode")]
    SetPacketMode { enabled: bool },
    #[serde(rename = "set_termios")]
    SetTermios {
        termios: TermiosConfig,
        #[serde(default)]
        signals: Option<Vec<String>>,
    },
    #[serde(rename = "flush")]
    Flush { side: String, queue_selector: String },
    #[serde(rename = "wait_until_readable")]
    WaitUntilReadable { side: String },
    #[serde(rename = "check_readable_size")]
    CheckReadableSize { side: String, size: usize },
    #[serde(rename = "check_poll")]
    CheckPoll { side: String, events: Vec<String> },
    #[serde(rename = "close")]
    Close { side: String },
}

struct TestBuffer {
    data: Vec<u8>,
}

impl TestBuffer {
    fn new(data: Vec<u8>) -> Self {
        Self { data }
    }
}

impl InputBuffer for TestBuffer {
    fn available(&self) -> usize {
        self.data.len()
    }
    fn read_to_vec_exact(&mut self, size: usize) -> Result<Vec<u8>, Errno> {
        if size > self.data.len() {
            return error!(EAGAIN);
        }
        let result = self.data.drain(0..size).collect();
        Ok(result)
    }
}

struct TestOutputBuffer {
    data: Vec<u8>,
}

impl TestOutputBuffer {
    fn new() -> Self {
        Self { data: vec![] }
    }
}

impl OutputBuffer for TestOutputBuffer {
    fn available(&self) -> usize {
        usize::MAX
    }
    fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
        self.data.extend_from_slice(data);
        Ok(data.len())
    }
}

struct LimitedTestOutputBuffer {
    data: Vec<u8>,
    max_size: usize,
}

impl LimitedTestOutputBuffer {
    fn new(max_size: usize) -> Self {
        Self { data: vec![], max_size }
    }
}

impl OutputBuffer for LimitedTestOutputBuffer {
    fn available(&self) -> usize {
        self.max_size.saturating_sub(self.data.len())
    }
    fn write(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let to_write = std::cmp::min(self.available(), data.len());
        self.data.extend_from_slice(&data[..to_write]);
        Ok(to_write)
    }
}

fn parse_flags(flags: &[String], mapping: &[(u32, &str)]) -> u32 {
    let mut result = 0;
    for flag in flags {
        if let Some((val, _)) = mapping.iter().find(|(_, name)| name == flag) {
            result |= val;
        } else {
            panic!("Unknown flag {}", flag);
        }
    }
    result
}

fn get_iflag_mapping() -> Vec<(u32, &'static str)> {
    vec![
        (starnix_uapi::IGNBRK, "IGNBRK"),
        (starnix_uapi::BRKINT, "BRKINT"),
        (starnix_uapi::IGNPAR, "IGNPAR"),
        (starnix_uapi::PARMRK, "PARMRK"),
        (starnix_uapi::INPCK, "INPCK"),
        (starnix_uapi::ISTRIP, "ISTRIP"),
        (starnix_uapi::INLCR, "INLCR"),
        (starnix_uapi::IGNCR, "IGNCR"),
        (starnix_uapi::ICRNL, "ICRNL"),
        (starnix_uapi::IUCLC, "IUCLC"),
        (starnix_uapi::IXON, "IXON"),
        (starnix_uapi::IXANY, "IXANY"),
        (starnix_uapi::IXOFF, "IXOFF"),
        (starnix_uapi::IMAXBEL, "IMAXBEL"),
        (starnix_uapi::IUTF8, "IUTF8"),
    ]
}

fn get_oflag_mapping() -> Vec<(u32, &'static str)> {
    vec![
        (starnix_uapi::OPOST, "OPOST"),
        (starnix_uapi::OLCUC, "OLCUC"),
        (starnix_uapi::ONLCR, "ONLCR"),
        (starnix_uapi::OCRNL, "OCRNL"),
        (starnix_uapi::ONOCR, "ONOCR"),
        (starnix_uapi::ONLRET, "ONLRET"),
        (starnix_uapi::OFILL, "OFILL"),
        (starnix_uapi::OFDEL, "OFDEL"),
        (starnix_uapi::XTABS, "XTABS"),
    ]
}

fn get_lflag_mapping() -> Vec<(u32, &'static str)> {
    vec![
        (starnix_uapi::ISIG, "ISIG"),
        (starnix_uapi::ICANON, "ICANON"),
        (starnix_uapi::XCASE, "XCASE"),
        (starnix_uapi::ECHO, "ECHO"),
        (starnix_uapi::ECHOE, "ECHOE"),
        (starnix_uapi::ECHOK, "ECHOK"),
        (starnix_uapi::ECHONL, "ECHONL"),
        (starnix_uapi::ECHOCTL, "ECHOCTL"),
        (starnix_uapi::ECHOPRT, "ECHOPRT"),
        (starnix_uapi::ECHOKE, "ECHOKE"),
        (starnix_uapi::FLUSHO, "FLUSHO"),
        (starnix_uapi::NOFLSH, "NOFLSH"),
        (starnix_uapi::TOSTOP, "TOSTOP"),
        (starnix_uapi::PENDIN, "PENDIN"),
        (starnix_uapi::IEXTEN, "IEXTEN"),
        (starnix_uapi::EXTPROC, "EXTPROC"),
    ]
}

fn get_cc_mapping() -> HashMap<&'static str, usize> {
    let mut m = HashMap::new();
    m.insert("VMIN", starnix_uapi::VMIN as usize);
    m.insert("VTIME", starnix_uapi::VTIME as usize);
    m.insert("VINTR", starnix_uapi::VINTR as usize);
    m.insert("VQUIT", starnix_uapi::VQUIT as usize);
    m.insert("VERASE", starnix_uapi::VERASE as usize);
    m.insert("VKILL", starnix_uapi::VKILL as usize);
    m.insert("VEOF", starnix_uapi::VEOF as usize);
    m.insert("VSTART", starnix_uapi::VSTART as usize);
    m.insert("VSTOP", starnix_uapi::VSTOP as usize);
    m.insert("VSUSP", starnix_uapi::VSUSP as usize);
    m.insert("VEOL", starnix_uapi::VEOL as usize);
    m.insert("VREPRINT", starnix_uapi::VREPRINT as usize);
    m.insert("VDISCARD", starnix_uapi::VDISCARD as usize);
    m.insert("VWERASE", starnix_uapi::VWERASE as usize);
    m.insert("VLNEXT", starnix_uapi::VLNEXT as usize);
    m.insert("VEOL2", starnix_uapi::VEOL2 as usize);
    m
}

fn signal_to_name(sig: Signal) -> &'static str {
    if sig == SIGINT {
        "SIGINT"
    } else if sig == SIGQUIT {
        "SIGQUIT"
    } else if sig == SIGTSTP {
        "SIGTSTP"
    } else {
        panic!("Unexpected signal {:?}", sig);
    }
}

fn check_signals(
    pending_signals: PendingSignals,
    expected_signals: Option<Vec<String>>,
    event_name: &str,
    scenario_name: &str,
) {
    let actual: Vec<String> =
        pending_signals.signals().iter().map(|&s| signal_to_name(s).to_string()).collect();
    let expected = expected_signals.unwrap_or_default();
    assert_eq!(actual, expected, "{} signals mismatch in {}", event_name, scenario_name);
}

fn decompose_fd_events(events: FdEvents) -> Vec<String> {
    let mut out = Vec::new();
    if events.contains(FdEvents::POLLIN) {
        out.push("POLLIN".to_string());
    }
    if events.contains(FdEvents::POLLOUT) {
        out.push("POLLOUT".to_string());
    }
    if events.contains(FdEvents::POLLPRI) {
        out.push("POLLPRI".to_string());
    }
    if events.contains(FdEvents::POLLERR) {
        out.push("POLLERR".to_string());
    }
    if events.contains(FdEvents::POLLHUP) {
        out.push("POLLHUP".to_string());
    }
    out
}

pub fn test_replay_trace(name: &str, json_data: &str) {
    println!("Running trace: {}", name);
    let scenario: Scenario = serde_json::from_str(json_data).unwrap_or_else(|e| {
        panic!("Failed to parse trace {}: {}", name, e);
    });
    run_scenario(scenario);
}

fn apply_termios_config(
    mut termios: uapi::termios2,
    config: &TermiosConfig,
    iflags: &[(u32, &'static str)],
    oflags: &[(u32, &'static str)],
    lflags: &[(u32, &'static str)],
) -> uapi::termios2 {
    termios.c_iflag = parse_flags(&config.c_iflag, iflags);
    termios.c_oflag = parse_flags(&config.c_oflag, oflags);
    termios.c_lflag = parse_flags(&config.c_lflag, lflags);
    if let Some(cc) = &config.c_cc {
        let mapping = get_cc_mapping();
        for (name, &val) in cc {
            if let Some(&idx) = mapping.get(name.as_str()) {
                if idx < termios.c_cc.len() {
                    termios.c_cc[idx] = val;
                }
            } else {
                panic!("Unknown c_cc name {}", name);
            }
        }
    }
    termios
}

fn run_scenario(scenario: Scenario) {
    let iflags = get_iflag_mapping();
    let oflags = get_oflag_mapping();
    let lflags = get_lflag_mapping();

    let mut ld = LineDiscipline::default();
    ld.main_open();
    ld.replica_open();

    // Set initial termios
    let termios =
        apply_termios_config(*ld.termios(), &scenario.initial_termios, &iflags, &oflags, &lflags);
    let initial_signals = ld.set_termios(termios);
    assert!(initial_signals.signals().is_empty());

    for event in scenario.events {
        match event {
            Event::WriteToMaster { data, signals: expected_signals } => {
                let bytes = data.to_bytes();
                let mut buffer = TestBuffer::new(bytes.clone());
                let (written, pending_signals) =
                    ld.main_write(&mut buffer).expect("main_write failed");
                assert_eq!(
                    written,
                    bytes.len(),
                    "WriteToMaster partial write in {}",
                    scenario.name
                );
                check_signals(pending_signals, expected_signals, "WriteToMaster", &scenario.name);
            }
            Event::WriteToMasterBlocked { data } => {
                let mut buffer = TestBuffer::new(data.to_bytes());
                assert_eq!(
                    ld.main_write(&mut buffer),
                    error!(EAGAIN),
                    "Expected main_write to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::ReadFromMaster { data } => {
                let mut buffer = TestOutputBuffer::new();
                loop {
                    match ld.main_read(&mut buffer) {
                        Ok(_) => {}
                        Err(e) if e == (error!(EAGAIN) as Result<(), Errno>).unwrap_err() => {
                            break;
                        }
                        Err(e) => panic!("main_read failed: {:?}", e),
                    }
                }
                assert_eq!(
                    buffer.data,
                    data.to_bytes(),
                    "ReadFromMaster mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&data.to_bytes()),
                );
            }
            Event::ReadFromSlave { data, signals: expected_signals } => {
                let mut buffer = TestOutputBuffer::new();
                let mut pending_signals = PendingSignals::new();
                loop {
                    match ld.replica_read(&mut buffer) {
                        Ok((_, signals)) => pending_signals.append(signals),
                        Err(e) if e == (error!(EAGAIN) as Result<(), Errno>).unwrap_err() => {
                            break;
                        }
                        Err(e) => panic!("replica_read failed: {:?}", e),
                    }
                }
                check_signals(pending_signals, expected_signals, "ReadFromSlave", &scenario.name);
                assert_eq!(
                    buffer.data,
                    data.to_bytes(),
                    "ReadFromSlave mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&data.to_bytes()),
                );
            }
            Event::ReadOnceFromMaster { size, data } => {
                let mut buffer = LimitedTestOutputBuffer::new(size);
                let expected = data.to_bytes();
                let n = ld.main_read(&mut buffer).unwrap_or_else(|e| {
                    panic!("ReadOnceFromMaster failed in {}: {:?}", scenario.name, e)
                });
                assert_eq!(
                    n,
                    expected.len(),
                    "ReadOnceFromMaster length mismatch in {}",
                    scenario.name
                );
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadOnceFromMaster data mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadOnceFromMasterBlocked { size } => {
                let mut buffer = LimitedTestOutputBuffer::new(size);
                assert_eq!(
                    ld.main_read(&mut buffer),
                    error!(EAGAIN),
                    "Expected ReadOnceFromMaster to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::ReadOnceFromMasterEio { size } => {
                let mut buffer = LimitedTestOutputBuffer::new(size);
                assert_eq!(
                    ld.main_read(&mut buffer),
                    error!(EIO),
                    "Expected ReadOnceFromMaster to return EIO in {}",
                    scenario.name
                );
            }
            Event::ReadOnceFromSlave { size, data, signals: expected_signals } => {
                let mut buffer = LimitedTestOutputBuffer::new(size);
                let expected = data.to_bytes();
                let (n, pending_signals) = ld.replica_read(&mut buffer).unwrap_or_else(|e| {
                    panic!("ReadOnceFromSlave failed in {}: {:?}", scenario.name, e)
                });
                check_signals(
                    pending_signals,
                    expected_signals,
                    "ReadOnceFromSlave",
                    &scenario.name,
                );
                assert_eq!(
                    n,
                    expected.len(),
                    "ReadOnceFromSlave length mismatch in {}",
                    scenario.name
                );
                assert_eq!(
                    buffer.data,
                    expected,
                    "ReadOnceFromSlave data mismatch in {} (actual={:?}, expected={:?})",
                    scenario.name,
                    String::from_utf8_lossy(&buffer.data),
                    String::from_utf8_lossy(&expected),
                );
            }
            Event::ReadOnceFromSlaveBlocked { size } => {
                let mut buffer = LimitedTestOutputBuffer::new(size);
                assert_eq!(
                    ld.replica_read(&mut buffer),
                    error!(EAGAIN),
                    "Expected ReadOnceFromSlave to return EAGAIN in {}",
                    scenario.name
                );
            }
            Event::WriteToSlave { data } => {
                let bytes = data.to_bytes();
                let mut buffer = TestBuffer::new(bytes.clone());
                let written = ld.replica_write(&mut buffer).expect("replica_write failed");
                assert_eq!(written, bytes.len(), "WriteToSlave partial write in {}", scenario.name);
            }
            Event::WriteToSlaveBlocked { data } => {
                let mut buffer = TestBuffer::new(data.to_bytes());
                let result = ld.replica_write(&mut buffer);
                assert!(
                    result.is_err(),
                    "Expected replica_write to block/fail in {}, but it succeeded",
                    scenario.name
                );
                assert_eq!(result, error!(EAGAIN), "Expected EAGAIN in {}", scenario.name);
            }
            Event::WriteToSlaveEio { data } => {
                let mut buffer = TestBuffer::new(data.to_bytes());
                assert_eq!(
                    ld.replica_write(&mut buffer),
                    error!(EIO),
                    "Expected replica_write to return EIO in {}",
                    scenario.name
                );
            }
            Event::SetPacketMode { enabled } => {
                ld.set_packet_mode(enabled);
            }
            Event::SetTermios { termios: ref termios_config, signals: expected_signals } => {
                let termios =
                    apply_termios_config(*ld.termios(), termios_config, &iflags, &oflags, &lflags);
                let pending_signals = ld.set_termios(termios);
                check_signals(pending_signals, expected_signals, "SetTermios", &scenario.name);
            }
            Event::Flush { side, queue_selector } => {
                let side = match side.as_str() {
                    "main" => TerminalSide::Main,
                    "replica" => TerminalSide::Replica,
                    _ => panic!("Unknown side {}", side),
                };
                let queue_selector_val = match queue_selector.as_str() {
                    "TCIFLUSH" => starnix_uapi::TCIFLUSH,
                    "TCOFLUSH" => starnix_uapi::TCOFLUSH,
                    "TCIOFLUSH" => starnix_uapi::TCIOFLUSH,
                    _ => panic!("Unknown queue_selector {}", queue_selector),
                };
                ld.flush(side, queue_selector_val).expect("flush failed");
            }
            Event::WaitUntilReadable { side } => {
                let events = match side.as_str() {
                    "main" => ld.main_query_events(),
                    "replica" => ld.replica_query_events(),
                    _ => panic!("Unknown side {}", side),
                };
                assert!(
                    events.intersects(FdEvents::POLLIN | FdEvents::POLLHUP),
                    "Expected {} to be readable in {}, got {:?}",
                    side,
                    scenario.name,
                    events
                );
            }
            Event::CheckReadableSize { side, size } => {
                let terminal_side = match side.as_str() {
                    "main" => TerminalSide::Main,
                    "replica" => TerminalSide::Replica,
                    _ => panic!("Unknown side {}", side),
                };
                assert_eq!(
                    ld.get_available_read_size(terminal_side),
                    size,
                    "CheckReadableSize ({}) mismatch in {}",
                    side,
                    scenario.name
                );
            }
            Event::CheckPoll { side, events } => {
                let actual_events = match side.as_str() {
                    "main" => ld.main_query_events(),
                    "replica" => ld.replica_query_events(),
                    _ => panic!("Unknown side {}", side),
                };
                assert_eq!(
                    decompose_fd_events(actual_events),
                    events,
                    "CheckPoll ({}) mismatch in {}",
                    side,
                    scenario.name
                );
            }
            Event::Close { side } => match side.as_str() {
                "main" => ld.main_close(),
                "replica" => ld.replica_close(),
                _ => panic!("Unknown side {}", side),
            },
        }
    }

    assert_eq!(
        ld.termios().c_iflag,
        parse_flags(&scenario.final_termios.c_iflag, &iflags),
        "final_termios.c_iflag mismatch in {}",
        scenario.name
    );
    assert_eq!(
        ld.termios().c_oflag,
        parse_flags(&scenario.final_termios.c_oflag, &oflags),
        "final_termios.c_oflag mismatch in {}",
        scenario.name
    );
    assert_eq!(
        ld.termios().c_lflag,
        parse_flags(&scenario.final_termios.c_lflag, &lflags),
        "final_termios.c_lflag mismatch in {}",
        scenario.name
    );
}
