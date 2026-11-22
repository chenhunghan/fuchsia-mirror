// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Linux/Starnix `libc` syscall executor for `usbfs` trace recording and verification.

pub mod linux_ffs_daemon;

use linux_uapi::{
    USBDEVFS_BULK, USBDEVFS_CLAIMINTERFACE, USBDEVFS_CLEAR_HALT, USBDEVFS_CONNECT,
    USBDEVFS_CONNECTINFO, USBDEVFS_CONTROL, USBDEVFS_DISCARDURB, USBDEVFS_DISCONNECT,
    USBDEVFS_DISCONNECT_CLAIM, USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER,
    USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER, USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_SPEED,
    USBDEVFS_GETDRIVER, USBDEVFS_IOCTL, USBDEVFS_REAPURB, USBDEVFS_REAPURBNDELAY,
    USBDEVFS_RELEASEINTERFACE, USBDEVFS_RESET, USBDEVFS_SETCONFIGURATION, USBDEVFS_SETINTERFACE,
    USBDEVFS_SUBMITURB, USBDEVFS_URB_BULK_CONTINUATION, USBDEVFS_URB_ISO_ASAP,
    USBDEVFS_URB_NO_FSBR, USBDEVFS_URB_NO_INTERRUPT, USBDEVFS_URB_SHORT_NOT_OK,
    USBDEVFS_URB_TYPE_BULK, USBDEVFS_URB_TYPE_CONTROL, USBDEVFS_URB_TYPE_INTERRUPT,
    USBDEVFS_URB_TYPE_ISO, USBDEVFS_URB_ZERO_PACKET, uaddr, usb_ctrlrequest, usbdevfs_bulktransfer,
    usbdevfs_connectinfo, usbdevfs_ctrltransfer, usbdevfs_disconnect_claim, usbdevfs_getdriver,
    usbdevfs_ioctl, usbdevfs_setinterface,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, mpsc};
use std::time::{Duration, Instant};
use std::{fs, thread};
pub use usbfs_trace_schema::{
    DeviceCommand, DeviceResponse, DeviceSpec, DivergenceManifest, RecordedStep, RecordedTrace,
    Scenario, Step, StepOutcome, errno_to_name, parse_usbdevfs_ioctl_params, urb_status_to_string,
};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Execution mode for [`linux_exec`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    /// Execute a [`Scenario`] against real Linux and record a [`RecordedTrace`].
    Record,
    /// Replay a [`RecordedTrace`] and assert outcomes match.
    Replay { apply_divergences: bool },
}

// Architecture-independent 32-bit compat USBDEVFS ioctl numbers (`include/uapi/linux/usbdevice_fs.h`).
pub const USBDEVFS_CONTROL32: u32 = 0xC010_5500;
pub const USBDEVFS_BULK32: u32 = 0xC010_5502;
pub const USBDEVFS_SUBMITURB32: u32 = 0x802C_550A;
pub const USBDEVFS_REAPURB32: u32 = 0x4004_550C;
pub const USBDEVFS_REAPURBNDELAY32: u32 = 0x4004_550D;
pub const USBDEVFS_DISCSIGNAL32: u32 = 0x8008_550E;
pub const USBDEVFS_IOCTL32: u32 = 0xC00C_5512;

#[repr(C)]
#[allow(non_snake_case)]
#[derive(Clone, Copy, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct UsbdevfsCtrltransfer32 {
    pub bRequestType: u8,
    pub bRequest: u8,
    pub wValue: u16,
    pub wIndex: u16,
    pub wLength: u16,
    pub timeout: u32,
    pub data: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct UsbdevfsBulktransfer32 {
    pub ep: u32,
    pub len: u32,
    pub timeout: u32,
    pub data: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct UsbdevfsIoctl32 {
    pub ifno: i32,
    pub ioctl_code: i32,
    pub data: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct UsbdevfsUrbHeader {
    pub type_: u8,
    pub endpoint: u8,
    pub _pad0: [u8; 2],
    pub status: i32,
    pub flags: u32,
    pub _pad1: [u8; 4],
    pub buffer: u64,
    pub buffer_length: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets_or_stream_id: u32,
    pub error_count: i32,
    pub signr: u32,
    pub usercontext: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct UsbdevfsUrb32Header {
    pub type_: u8,
    pub endpoint: u8,
    pub _pad0: [u8; 2],
    pub status: i32,
    pub flags: u32,
    pub buffer: u32,
    pub buffer_length: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets_or_stream_id: u32,
    pub error_count: i32,
    pub signr: u32,
    pub usercontext: u32,
}

static EXEC_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquires the process-wide execution lock serializing all USB gadget/syscall operations.
pub fn lock_execution() -> MutexGuard<'static, ()> {
    EXEC_LOCK.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
}

static NEXT_LOW_MMAP_HINT: AtomicUsize = AtomicUsize::new(0x2000_0000);

fn map_low32_pages(size: usize, prot: libc::c_int) -> *mut u8 {
    let page_size = 4096usize;
    let aligned_len = (size.max(page_size) + page_size - 1) & !(page_size - 1);
    for _ in 0..64 {
        let hint = NEXT_LOW_MMAP_HINT.fetch_add(aligned_len + page_size, Ordering::Relaxed);
        if hint >= 0x7000_0000 {
            NEXT_LOW_MMAP_HINT.store(0x2000_0000, Ordering::Relaxed);
        }
        // SAFETY: Maps anonymous private pages with a low-32-bit address hint.
        let ptr = unsafe {
            libc::mmap(
                hint as *mut libc::c_void,
                aligned_len,
                prot,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };
        if ptr != libc::MAP_FAILED && (ptr as usize) + aligned_len <= u32::MAX as usize {
            return ptr as *mut u8;
        }
        if ptr != libc::MAP_FAILED {
            // SAFETY: Unmaps high-address allocation that exceeded 32-bit range.
            unsafe {
                libc::munmap(ptr, aligned_len);
            }
        }
    }
    panic!("failed to allocate low-32-bit mmap region of size {aligned_len}");
}

/// Returns the address of a `PROT_NONE` guard page for deterministic `EFAULT` ("unmapped") tests.
pub fn unmapped_page_addr() -> usize {
    static GUARD_PAGE: OnceLock<usize> = OnceLock::new();
    *GUARD_PAGE.get_or_init(|| map_low32_pages(4096, libc::PROT_NONE) as usize)
}

const CANARY_PAD: usize = 64;
const CANARY_BYTE: u8 = 0xa5;
const IN_INIT_BYTE: u8 = 0x5a;

struct CanaryBuffer {
    base_ptr: *mut u8,
    alloc_len: usize,
    user_len: usize,
}

// SAFETY: `CanaryBuffer` owns an anonymous `mmap` region accessed exclusively through `&self` or
// `&mut self` while holding the `SharedExecState` mutex.
unsafe impl Send for CanaryBuffer {}

impl CanaryBuffer {
    fn new(user_len: usize, is_in: bool) -> Self {
        let page_size = 4096usize;
        let raw_total = CANARY_PAD + user_len + CANARY_PAD;
        let alloc_len = (raw_total.max(page_size) + page_size - 1) & !(page_size - 1);
        let base_ptr = map_low32_pages(alloc_len, libc::PROT_READ | libc::PROT_WRITE);
        // SAFETY: Initializes newly mapped pages with 0xA5 canaries and 0x5A IN fill bytes.
        unsafe {
            std::ptr::write_bytes(base_ptr, CANARY_BYTE, alloc_len);
            if user_len > 0 {
                let fill = if is_in { IN_INIT_BYTE } else { 0 };
                std::ptr::write_bytes(base_ptr.add(CANARY_PAD), fill, user_len);
            }
        }
        Self { base_ptr, alloc_len, user_len }
    }

    fn user_ptr(&self) -> usize {
        // SAFETY: `base_ptr + CANARY_PAD` is within the `alloc_len` mapped region.
        unsafe { self.base_ptr.add(CANARY_PAD) as usize }
    }

    fn write_at(&mut self, offset: usize, bytes: &[u8]) {
        if offset + bytes.len() <= self.user_len {
            // SAFETY: Bounds checked against `user_len` inside `alloc_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    bytes.as_ptr(),
                    self.base_ptr.add(CANARY_PAD + offset),
                    bytes.len(),
                );
            }
        }
    }

    fn read_range(&self, start: usize, end: usize) -> Vec<u8> {
        let clamped_end = end.min(self.user_len);
        let clamped_start = start.min(clamped_end);
        let len = clamped_end - clamped_start;
        let mut out = vec![0u8; len];
        if len > 0 {
            // SAFETY: Bounds checked against `user_len` inside `alloc_len`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.base_ptr.add(CANARY_PAD + clamped_start),
                    out.as_mut_ptr(),
                    len,
                );
            }
        }
        out
    }

    fn canary_intact(&self) -> bool {
        // SAFETY: `base_ptr` is valid for `alloc_len` bytes.
        let slice = unsafe { std::slice::from_raw_parts(self.base_ptr, self.alloc_len) };
        let leading_ok = slice[..CANARY_PAD].iter().all(|&b| b == CANARY_BYTE);
        let trailing_start = CANARY_PAD + self.user_len;
        let trailing_ok =
            slice[trailing_start..trailing_start + CANARY_PAD].iter().all(|&b| b == CANARY_BYTE);
        leading_ok && trailing_ok
    }

    fn unwritten_tail_intact(&self, written_end: usize) -> bool {
        if written_end >= self.user_len {
            return true;
        }
        // SAFETY: `base_ptr + CANARY_PAD` is valid for `user_len` bytes.
        let slice =
            unsafe { std::slice::from_raw_parts(self.base_ptr.add(CANARY_PAD), self.user_len) };
        slice[written_end..self.user_len].iter().all(|&b| b == IN_INIT_BYTE)
    }
}

impl Drop for CanaryBuffer {
    fn drop(&mut self) {
        if !self.base_ptr.is_null() {
            // SAFETY: Unmaps the region allocated in `CanaryBuffer::new`.
            unsafe {
                libc::munmap(self.base_ptr as *mut libc::c_void, self.alloc_len);
            }
        }
    }
}

struct UrbSlot {
    fd: RawFd,
    urb_page_ptr: *mut u8,
    owns_page: bool,
    arch32: bool,
    is_in: bool,
    is_control: bool,
    in_flight: bool,
    canary_buf: CanaryBuffer,
}

// SAFETY: `UrbSlot` owns an anonymous `mmap` URB header page and `CanaryBuffer` accessed only under
// the `SharedExecState` mutex.
unsafe impl Send for UrbSlot {}

impl Drop for UrbSlot {
    fn drop(&mut self) {
        if self.owns_page && !self.urb_page_ptr.is_null() {
            // SAFETY: Unmaps the 4096-byte URB header page allocated for this slot.
            unsafe {
                libc::munmap(self.urb_page_ptr as *mut libc::c_void, 4096);
            }
        }
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

fn outcome_ok(result: Value) -> StepOutcome {
    StepOutcome { status: "ok".to_string(), errno: None, result }
}

fn outcome_err(err: i32) -> StepOutcome {
    StepOutcome {
        status: "error".to_string(),
        errno: Some(errno_to_name(err)),
        result: Value::Null,
    }
}

fn resolve_ptr_mode(mode: Option<&str>, valid_addr: usize) -> usize {
    match mode.unwrap_or("valid") {
        "null" => 0,
        "unmapped" => unmapped_page_addr(),
        _ => valid_addr,
    }
}

pub fn send_device_command(
    control_addr: &str,
    cmd: &DeviceCommand,
) -> Result<DeviceResponse, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut stream = loop {
        match TcpStream::connect(control_addr) {
            Ok(s) => break s,
            Err(e) => {
                if Instant::now() >= deadline {
                    return Err(format!("connect to gadget server {control_addr} failed: {e}"));
                }
                thread::sleep(Duration::from_millis(20));
            }
        }
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let mut payload = serde_json::to_string(cmd).map_err(|e| e.to_string())?;
    payload.push('\n');
    stream.write_all(payload.as_bytes()).map_err(|e| e.to_string())?;
    stream.flush().map_err(|e| e.to_string())?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str::<DeviceResponse>(line.trim()).map_err(|e| e.to_string())
}

fn current_exe_stamp() -> String {
    static STAMP: OnceLock<String> = OnceLock::new();
    STAMP
        .get_or_init(|| {
            let Ok(bytes) = fs::read("/proc/self/exe") else {
                return String::new();
            };
            let mut hash: u64 = 0xcbf29ce484222325;
            for &b in bytes.iter().step_by(64) {
                hash ^= b as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
            format!("{}:{hash:016x}", bytes.len())
        })
        .clone()
}

/// Ensures that on Linux (`/sys/class/udc` exists), a background `linux_ffs_daemon` is listening
/// on `control_addr`. Uses the `/proc/self/exe` inode replacement trick before spawning the
/// daemon via `Command` (`execve`) to avoid `ETXTBSY` in `starnix_test_runner` / Machina VMs and
/// avoid multi-threaded `fork`-without-`exec` hazards, restarting any stale daemon left behind by
/// an earlier test binary in the persistent guest VM.
pub fn ensure_control_daemon_running(control_addr: &str) {
    if let Ok(daemon_addr) = std::env::var("USBFS_FFS_DAEMON_ONLY") {
        // SAFETY: Detaches the freshly exec'd daemon child process into its own session.
        unsafe {
            libc::setsid();
        }
        let _ = linux_ffs_daemon::serve(&daemon_addr);
        std::process::exit(0);
    }
    if !Path::new("/sys/class/udc").exists() {
        return;
    }
    let stamp_path = Path::new("/tmp/usbfs_ffs_daemon.stamp");
    let expected_stamp = current_exe_stamp();
    if TcpStream::connect(control_addr).is_ok() {
        let on_disk = fs::read_to_string(stamp_path).unwrap_or_default();
        if !expected_stamp.is_empty() && on_disk.trim() == expected_stamp {
            return;
        }
        linux_ffs_daemon::kill_external_ffs_holders();
        let stop_deadline = Instant::now() + Duration::from_secs(2);
        while TcpStream::connect(control_addr).is_ok() && Instant::now() < stop_deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }
    let exe_path = fs::read_link("/proc/self/exe").expect("read /proc/self/exe");
    let daemon_bin = exe_path.with_extension("daemon");
    let spawn_path =
        if fs::copy(&exe_path, &daemon_bin).is_ok() { daemon_bin.clone() } else { exe_path };
    std::process::Command::new(&spawn_path)
        .args(["--test-threads=1", "test_replay_trace"])
        .env("USBFS_FFS_DAEMON_ONLY", control_addr)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn linux_ffs_daemon process");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if TcpStream::connect(control_addr).is_ok() {
            let _ = fs::remove_file(&daemon_bin);
            let _ = fs::write(stamp_path, format!("{expected_stamp}\n"));
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = fs::remove_file(&daemon_bin);
    panic!("linux_ffs_daemon did not start listening on {control_addr}");
}

struct SharedExecState {
    device: DeviceSpec,
    control_addr: String,
    sysfs_dir: PathBuf,
    busnum: u32,
    devnum: u32,
    dev_path: PathBuf,
    fds: HashMap<String, RawFd>,
    urbs: HashMap<String, UrbSlot>,
    addr_to_urb: HashMap<usize, String>,
    unpopped_out_packets: HashMap<u8, usize>,
}

impl SharedExecState {
    fn new(device: DeviceSpec, control_addr: &str) -> Self {
        let mut st = Self {
            device,
            control_addr: control_addr.to_string(),
            sysfs_dir: PathBuf::new(),
            busnum: 1,
            devnum: 2,
            dev_path: PathBuf::from("/dev/bus/usb/001/002"),
            fds: HashMap::new(),
            urbs: HashMap::new(),
            addr_to_urb: HashMap::new(),
            unpopped_out_packets: HashMap::new(),
        };
        st.discover_device();
        st
    }

    fn discover_device(&mut self) {
        let expected_vid = format!("{:04x}", self.device.vendor_id);
        let expected_pid = format!("{:04x}", self.device.product_id);
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") {
                for entry in entries.flatten() {
                    let path = entry.path();
                    let vid = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
                    let pid = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
                    if vid.trim().eq_ignore_ascii_case(&expected_vid)
                        && pid.trim().eq_ignore_ascii_case(&expected_pid)
                    {
                        let busnum = fs::read_to_string(path.join("busnum"))
                            .ok()
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(1);
                        let devnum = fs::read_to_string(path.join("devnum"))
                            .ok()
                            .and_then(|s| s.trim().parse::<u32>().ok())
                            .unwrap_or(2);
                        let dev_path =
                            PathBuf::from(format!("/dev/bus/usb/{:03}/{:03}", busnum, devnum));
                        if dev_path.exists() {
                            self.sysfs_dir = path;
                            self.busnum = busnum;
                            self.devnum = devnum;
                            self.dev_path = dev_path;
                            return;
                        }
                    }
                }
            }
            thread::sleep(Duration::from_millis(15));
        }
        panic!(
            "USB device {:04x}:{:04x} did not appear in /sys/bus/usb/devices",
            self.device.vendor_id, self.device.product_id
        );
    }

    fn get_fd(&self, params: &Value) -> RawFd {
        let name = params.get("fd").and_then(Value::as_str).unwrap_or("fd1");
        self.fds.get(name).copied().unwrap_or(-1)
    }
}

impl Drop for SharedExecState {
    fn drop(&mut self) {
        for (_, fd) in self.fds.drain() {
            // SAFETY: Closes open usbfs file descriptors owned by this scenario execution.
            unsafe {
                libc::close(fd);
            }
        }
    }
}

fn normalize_uevent(raw: &str, busnum: u32, devnum: u32) -> String {
    let minor = (busnum.saturating_sub(1) * 128) + devnum.saturating_sub(1);
    let minor_exact = format!("MINOR={minor}");
    let devname_exact = format!("DEVNAME=bus/usb/{:03}/{:03}", busnum, devnum);
    let devname_norm = format!("DEVNAME=bus/usb/{:03}/$DEVNUM", busnum);
    let devnum_exact = format!("DEVNUM={:03}", devnum);
    let devpath_exact = format!("DEVPATH=/devices/{}-{}", busnum, devnum);
    let devpath_norm = format!("DEVPATH=/devices/{}-$DEVNUM", busnum);

    let mut out_lines = Vec::new();
    for line in raw.lines() {
        if line.starts_with("DRIVER=") {
            continue;
        }
        if line == minor_exact {
            out_lines.push("MINOR=$MINOR".to_string());
        } else if line == devname_exact {
            out_lines.push(devname_norm.clone());
        } else if line == devnum_exact {
            out_lines.push("DEVNUM=$DEVNUM".to_string());
        } else if line == devpath_exact {
            out_lines.push(devpath_norm.clone());
        } else {
            out_lines.push(line.to_string());
        }
    }
    let mut joined = out_lines.join("\n");
    joined.push('\n');
    joined
}

fn parse_poll_events(events: Option<&Vec<Value>>) -> libc::c_short {
    let Some(arr) = events else {
        return libc::POLLIN | libc::POLLOUT | libc::POLLWRNORM | libc::POLLERR | libc::POLLHUP;
    };
    let mut mask: libc::c_short = 0;
    for v in arr {
        match v.as_str().unwrap_or("") {
            "POLLIN" | "EPOLLIN" => mask |= libc::POLLIN,
            "POLLOUT" | "EPOLLOUT" => mask |= libc::POLLOUT,
            "POLLWRNORM" => mask |= libc::POLLWRNORM,
            "POLLRDNORM" => mask |= libc::POLLRDNORM,
            "POLLERR" | "EPOLLERR" => mask |= libc::POLLERR,
            "POLLHUP" | "EPOLLHUP" => mask |= libc::POLLHUP,
            "POLLPRI" => mask |= libc::POLLPRI,
            _ => {}
        }
    }
    mask
}

fn format_poll_revents(revents: i32) -> Vec<String> {
    let mut out = Vec::new();
    if (revents & (libc::POLLIN as i32)) != 0 {
        out.push("POLLIN".to_string());
    }
    if (revents & (libc::POLLOUT as i32)) != 0 {
        out.push("POLLOUT".to_string());
    }
    if (revents & (libc::POLLWRNORM as i32)) != 0 {
        out.push("POLLWRNORM".to_string());
    }
    if (revents & (libc::POLLERR as i32)) != 0 {
        out.push("POLLERR".to_string());
    }
    if (revents & (libc::POLLHUP as i32)) != 0 {
        out.push("POLLHUP".to_string());
    }
    if (revents & (libc::POLLNVAL as i32)) != 0 {
        out.push("POLLNVAL".to_string());
    }
    out
}

fn parse_urb_flags(params: &Value) -> u32 {
    let mut flags = params.get("raw_extra_flags").and_then(Value::as_u64).unwrap_or(0) as u32;
    if let Some(arr) = params.get("flags").and_then(Value::as_array) {
        for item in arr {
            match item.as_str().unwrap_or("") {
                "SHORT_NOT_OK" | "USBDEVFS_URB_SHORT_NOT_OK" => flags |= USBDEVFS_URB_SHORT_NOT_OK,
                "ISO_ASAP" | "USBDEVFS_URB_ISO_ASAP" => flags |= USBDEVFS_URB_ISO_ASAP,
                "BULK_CONTINUATION" | "USBDEVFS_URB_BULK_CONTINUATION" => {
                    flags |= USBDEVFS_URB_BULK_CONTINUATION
                }
                "NO_FSBR" | "USBDEVFS_URB_NO_FSBR" => flags |= USBDEVFS_URB_NO_FSBR,
                "ZERO_PACKET" | "USBDEVFS_URB_ZERO_PACKET" => flags |= USBDEVFS_URB_ZERO_PACKET,
                "NO_INTERRUPT" | "USBDEVFS_URB_NO_INTERRUPT" => flags |= USBDEVFS_URB_NO_INTERRUPT,
                _ => {}
            }
        }
    }
    flags
}

fn wait_for_tid_sleeping(tid: i32, done_flag: &Arc<std::sync::atomic::AtomicBool>) {
    let stat_path = format!("/proc/self/task/{tid}/stat");
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if done_flag.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(stat_str) = fs::read_to_string(&stat_path) {
            if let Some(rparen) = stat_str.rfind(')') {
                let state_ch = stat_str[rparen + 1..].trim_start().chars().next().unwrap_or(' ');
                if state_ch == 'S' || state_ch == 'D' {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn execute_step(
    step: &Step,
    state: &Arc<Mutex<SharedExecState>>,
    tasks: &mut HashMap<String, mpsc::Receiver<StepOutcome>>,
) -> StepOutcome {
    let p = &step.params;
    let arg_mode = p.get("arg_ptr").and_then(Value::as_str);
    let data_mode = p.get("data_ptr").and_then(Value::as_str);
    let arch32 = p.get("arch32").and_then(Value::as_bool).unwrap_or(false);

    match step.op.as_str() {
        // 1. FD & VFS Operations
        "open" => {
            let mut st = state.lock().unwrap();
            let fd_name = p.get("fd").and_then(Value::as_str).unwrap_or("fd1").to_string();
            let mode = p.get("mode").and_then(Value::as_str).unwrap_or("rdwr");
            let flags = match mode {
                "rdonly" => libc::O_RDONLY,
                "wronly" => libc::O_WRONLY,
                _ => libc::O_RDWR,
            } | libc::O_CLOEXEC;
            let c_path = CString::new(st.dev_path.to_string_lossy().as_bytes()).unwrap();
            // SAFETY: Opens the usbfs device node path with requested flags.
            let fd = unsafe { libc::open(c_path.as_ptr(), flags) };
            if fd < 0 {
                return outcome_err(last_errno());
            }
            if let Some(old_fd) = st.fds.insert(fd_name.clone(), fd) {
                // SAFETY: Closes replaced fd under the same logical name.
                unsafe {
                    libc::close(old_fd);
                }
            }
            outcome_ok(json!({ "fd": fd_name }))
        }
        "close" => {
            let mut st = state.lock().unwrap();
            let fd_name = p.get("fd").and_then(Value::as_str).unwrap_or("fd1");
            if let Some(fd) = st.fds.remove(fd_name) {
                for u in st.urbs.values_mut() {
                    if u.fd == fd {
                        u.in_flight = false;
                    }
                }
                // SAFETY: Closes owned usbfs fd.
                let rc = unsafe { libc::close(fd) };
                if rc < 0 {
                    return outcome_err(last_errno());
                }
                outcome_ok(json!({ "closed": true }))
            } else {
                outcome_err(libc::EBADF)
            }
        }
        "stat_dev" => {
            let st = state.lock().unwrap();
            // SAFETY: Zero-initializes `libc::stat` POD struct.
            let mut stat_buf: libc::stat = unsafe { std::mem::zeroed() };
            let rc = if let Some(fd_name) = p.get("fd").and_then(Value::as_str) {
                let fd = st.fds.get(fd_name).copied().unwrap_or(-1);
                // SAFETY: Calls fstat on `fd` with valid `stat_buf`.
                unsafe { libc::fstat(fd, &mut stat_buf) }
            } else {
                let c_path = CString::new(st.dev_path.to_string_lossy().as_bytes()).unwrap();
                // SAFETY: Calls stat on `c_path` with valid `stat_buf`.
                unsafe { libc::stat(c_path.as_ptr(), &mut stat_buf) }
            };
            if rc < 0 {
                return outcome_err(last_errno());
            }
            let is_char_dev = (stat_buf.st_mode & libc::S_IFMT) == libc::S_IFCHR;
            let major = libc::major(stat_buf.st_rdev);
            let minor = libc::minor(stat_buf.st_rdev) as i64;
            let expected_minor = ((st.busnum as i64 - 1) * 128) + (st.devnum as i64 - 1);
            outcome_ok(json!({
                "is_char_dev": is_char_dev,
                "major": major,
                "minor_offset_from_devnum": minor - expected_minor,
            }))
        }
        "read" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let size = p.get("size").and_then(Value::as_u64).unwrap_or(256) as usize;
            let mut buf = vec![0u8; size];
            // SAFETY: Reads up to `size` bytes from `raw_fd` into `buf`.
            let n = unsafe { libc::read(raw_fd, buf.as_mut_ptr() as *mut libc::c_void, size) };
            if n < 0 {
                return outcome_err(last_errno());
            }
            buf.truncate(n as usize);
            outcome_ok(json!({ "bytes_read": n, "data": buf }))
        }
        "pread" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let size = p.get("size").and_then(Value::as_u64).unwrap_or(64) as usize;
            let offset = p.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let mut buf = vec![0u8; size];
            // SAFETY: Reads up to `size` bytes at `offset` from `raw_fd` into `buf`.
            let n =
                unsafe { libc::pread(raw_fd, buf.as_mut_ptr() as *mut libc::c_void, size, offset) };
            if n < 0 {
                return outcome_err(last_errno());
            }
            buf.truncate(n as usize);
            outcome_ok(json!({ "bytes_read": n, "data": buf }))
        }
        "lseek" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let offset = p.get("offset").and_then(Value::as_i64).unwrap_or(0);
            let whence = match p.get("whence") {
                Some(Value::String(s)) => match s.as_str() {
                    "SEEK_SET" => libc::SEEK_SET,
                    "SEEK_CUR" => libc::SEEK_CUR,
                    "SEEK_END" => libc::SEEK_END,
                    _ => 999,
                },
                Some(Value::Number(n)) => n.as_i64().unwrap_or(0) as i32,
                _ => libc::SEEK_SET,
            };
            // SAFETY: Calls lseek on `raw_fd`.
            let pos = unsafe { libc::lseek(raw_fd, offset, whence) };
            if pos < 0 {
                return outcome_err(last_errno());
            }
            outcome_ok(json!({ "offset": pos }))
        }
        "write" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let data: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_else(|| vec![1, 2, 3, 4]);
            // SAFETY: Calls write on `raw_fd` with `data` slice.
            let n =
                unsafe { libc::write(raw_fd, data.as_ptr() as *const libc::c_void, data.len()) };
            if n < 0 {
                return outcome_err(last_errno());
            }
            outcome_ok(json!({ "bytes_written": n }))
        }
        "poll" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let events = parse_poll_events(p.get("events").and_then(Value::as_array));
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_i64).unwrap_or(0) as i32;
            let mut pfd = libc::pollfd { fd: raw_fd, events, revents: 0 };
            // SAFETY: Calls poll on single `pollfd` struct.
            let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
            if rc < 0 {
                return outcome_err(last_errno());
            }
            outcome_ok(json!({
                "ready": rc,
                "revents": format_poll_revents(pfd.revents as i32),
            }))
        }
        "epoll_check" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let events = parse_poll_events(p.get("events").and_then(Value::as_array)) as i32 as u32;
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_i64).unwrap_or(0) as i32;
            // SAFETY: Creates an epoll instance, registers `raw_fd`, waits, and closes `epfd`.
            let epfd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
            if epfd < 0 {
                return outcome_err(last_errno());
            }
            let mut ev = libc::epoll_event { events, u64: 0x1234 };
            // SAFETY: Registers `raw_fd` on `epfd`.
            let ctl_rc = unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, raw_fd, &mut ev) };
            if ctl_rc < 0 {
                let err = last_errno();
                // SAFETY: Closes `epfd` on error.
                unsafe { libc::close(epfd) };
                return outcome_err(err);
            }
            let mut out_ev = libc::epoll_event { events: 0, u64: 0 };
            // SAFETY: Waits on `epfd` for up to `timeout_ms`.
            let n = unsafe { libc::epoll_wait(epfd, &mut out_ev, 1, timeout_ms) };
            let err = last_errno();
            // SAFETY: Closes `epfd`.
            unsafe { libc::close(epfd) };
            if n < 0 {
                return outcome_err(err);
            }
            let out_events = if n > 0 { out_ev.events as i32 } else { 0 };
            outcome_ok(json!({
                "ready": n,
                "revents": format_poll_revents(out_events),
            }))
        }
        "read_sysfs" => {
            let (sysfs_dir, busnum, devnum) = {
                let st = state.lock().unwrap();
                (st.sysfs_dir.clone(), st.busnum, st.devnum)
            };
            let attr = p.get("attr").and_then(Value::as_str).unwrap_or("idVendor");
            let attr_path = sysfs_dir.join(attr);
            if attr == "descriptors" {
                match fs::read(&attr_path) {
                    Ok(bytes) => outcome_ok(json!({ "attr": attr, "bytes": bytes })),
                    Err(e) => outcome_err(e.raw_os_error().unwrap_or(libc::ENOENT)),
                }
            } else {
                match fs::read_to_string(&attr_path) {
                    Ok(val) => {
                        let normalized = if attr == "devnum" && val.trim() == devnum.to_string() {
                            "$DEVNUM\n".to_string()
                        } else if attr == "uevent" {
                            normalize_uevent(&val, busnum, devnum)
                        } else {
                            val
                        };
                        outcome_ok(json!({ "attr": attr, "value": normalized }))
                    }
                    Err(e) => outcome_err(e.raw_os_error().unwrap_or(libc::ENOENT)),
                }
            }
        }

        // 2. Synchronous USBDEVFS_* Ioctls
        "get_speed" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            // SAFETY: Calls USBDEVFS_GET_SPEED ioctl.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_GET_SPEED as libc::Ioctl, 0) };
            if rc < 0 { outcome_err(last_errno()) } else { outcome_ok(json!({ "speed": rc })) }
        }
        "get_capabilities" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let mut caps: u32 = 0xffff_ffff;
            let ptr = resolve_ptr_mode(arg_mode, &mut caps as *mut u32 as usize);
            // SAFETY: Calls USBDEVFS_GET_CAPABILITIES ioctl with `ptr` (valid, null, or unmapped).
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_GET_CAPABILITIES as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "capabilities": caps }))
            }
        }
        "connectinfo" => {
            let (raw_fd, devnum) = {
                let st = state.lock().unwrap();
                (st.get_fd(p), st.devnum)
            };
            let mut ci = usbdevfs_connectinfo::default();
            let ptr = resolve_ptr_mode(arg_mode, &mut ci as *mut usbdevfs_connectinfo as usize);
            // SAFETY: Calls USBDEVFS_CONNECTINFO ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_CONNECTINFO as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({
                    "devnum_matches": ci.devnum == devnum,
                    "slow": ci.slow,
                }))
            }
        }
        "get_driver" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut gd = usbdevfs_getdriver { interface, ..Default::default() };
            let ptr = resolve_ptr_mode(arg_mode, &mut gd as *mut usbdevfs_getdriver as usize);
            // SAFETY: Calls USBDEVFS_GETDRIVER ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_GETDRIVER as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                let drv_bytes: Vec<u8> =
                    gd.driver.iter().map(|&c| c as u8).take_while(|&b| b != 0).collect();
                let drv_str = String::from_utf8_lossy(&drv_bytes).into_owned();
                outcome_ok(json!({ "interface": gd.interface, "driver": drv_str }))
            }
        }
        "claim_interface" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut ifno = interface;
            let ptr = resolve_ptr_mode(arg_mode, &mut ifno as *mut u32 as usize);
            // SAFETY: Calls USBDEVFS_CLAIMINTERFACE ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_CLAIMINTERFACE as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "claimed": interface }))
            }
        }
        "release_interface" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut ifno = interface;
            let ptr = resolve_ptr_mode(arg_mode, &mut ifno as *mut u32 as usize);
            // SAFETY: Calls USBDEVFS_RELEASEINTERFACE ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_RELEASEINTERFACE as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "released": interface }))
            }
        }
        "disconnect_claim" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut flags = p.get("raw_flags").and_then(Value::as_u64).unwrap_or(0) as u32;
            if let Some(arr) = p.get("flags").and_then(Value::as_array) {
                for f in arr {
                    match f.as_str().unwrap_or("") {
                        "IF_DRIVER" => flags |= USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER,
                        "EXCEPT_DRIVER" => flags |= USBDEVFS_DISCONNECT_CLAIM_EXCEPT_DRIVER,
                        _ => {}
                    }
                }
            }
            let driver = p.get("driver").and_then(Value::as_str).unwrap_or("usbfs");
            let mut dc = usbdevfs_disconnect_claim { interface, flags, ..Default::default() };
            for (dst, &src) in dc.driver.iter_mut().zip(driver.as_bytes().iter()) {
                *dst = src as _;
            }
            let ptr =
                resolve_ptr_mode(arg_mode, &mut dc as *mut usbdevfs_disconnect_claim as usize);
            // SAFETY: Calls USBDEVFS_DISCONNECT_CLAIM ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_DISCONNECT_CLAIM as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "claimed": interface }))
            }
        }
        "set_interface" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let interface = p.get("interface").and_then(Value::as_u64).unwrap_or(0) as u32;
            let altsetting = p.get("altsetting").and_then(Value::as_u64).unwrap_or(0) as u32;
            let mut si = usbdevfs_setinterface { interface, altsetting };
            let ptr = resolve_ptr_mode(arg_mode, &mut si as *mut usbdevfs_setinterface as usize);
            // SAFETY: Calls USBDEVFS_SETINTERFACE ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_SETINTERFACE as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "interface": interface, "altsetting": altsetting }))
            }
        }
        "set_configuration" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let config = p.get("config").and_then(Value::as_i64).unwrap_or(1) as i32;
            let mut cfg = config;
            let ptr = resolve_ptr_mode(arg_mode, &mut cfg as *mut i32 as usize);
            // SAFETY: Calls USBDEVFS_SETCONFIGURATION ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_SETCONFIGURATION as libc::Ioctl, ptr) };
            if rc < 0 { outcome_err(last_errno()) } else { outcome_ok(json!({ "config": config })) }
        }
        "clear_halt" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let endpoint =
                p.get("endpoint").or_else(|| p.get("ep")).and_then(Value::as_u64).unwrap_or(0x81)
                    as u32;
            let mut ep = endpoint;
            let ptr = resolve_ptr_mode(arg_mode, &mut ep as *mut u32 as usize);
            // SAFETY: Calls USBDEVFS_CLEAR_HALT ioctl with `ptr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_CLEAR_HALT as libc::Ioctl, ptr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "endpoint": endpoint }))
            }
        }
        "reset" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            // SAFETY: Calls USBDEVFS_RESET ioctl.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_RESET as libc::Ioctl, 0) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                state.lock().unwrap().unpopped_out_packets.clear();
                outcome_ok(json!({ "reset": true }))
            }
        }
        "raw_ioctl" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let request = p.get("request").and_then(Value::as_u64).unwrap_or(0xdead_beef) as u32;
            let mut placeholder: u64 = 0;
            let ptr = resolve_ptr_mode(arg_mode, &mut placeholder as *mut u64 as usize);
            // SAFETY: Calls arbitrary ioctl number `request` on `raw_fd` for unknown-ioctl testing.
            let rc = unsafe { libc::ioctl(raw_fd, request as libc::Ioctl, ptr) };
            if rc < 0 { outcome_err(last_errno()) } else { outcome_ok(json!({ "retval": rc })) }
        }
        "usbdevfs_ioctl" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let (ifno, ioctl_code) =
                parse_usbdevfs_ioctl_params(p, USBDEVFS_DISCONNECT as i32, USBDEVFS_CONNECT as i32);
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let ioctl_size = (((ioctl_code as u32) >> 16) & 0x3fff) as usize;
            let has_data = p.get("data").is_some() || data_mode.is_some();
            let mut data_buf = CanaryBuffer::new(ioctl_size.max(data_vec.len()).max(4), false);
            if !data_vec.is_empty() {
                data_buf.write_at(0, &data_vec);
            }
            let data_addr =
                if has_data { resolve_ptr_mode(data_mode, data_buf.user_ptr()) } else { 0 };
            let rc = if arch32 {
                let ctl32 = UsbdevfsIoctl32 { ifno, ioctl_code, data: data_addr as u32 };
                let mut ctl32_buf =
                    CanaryBuffer::new(std::mem::size_of::<UsbdevfsIoctl32>(), false);
                ctl32_buf.write_at(0, ctl32.as_bytes());
                let ptr = resolve_ptr_mode(arg_mode, ctl32_buf.user_ptr());
                // SAFETY: `USBDEVFS_IOCTL32` takes a user pointer to `UsbdevfsIoctl32`; `ptr` is
                // either `ctl32_buf` (mapped <4GiB and live across the call), 0, or an intentional
                // fault address (`PROT_NONE` / OOB), and `ctl32.data` is either `data_buf` (live
                // across the call), 0, or an intentional fault address.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_IOCTL32 as libc::Ioctl, ptr) }
            } else {
                let mut ctl =
                    usbdevfs_ioctl { ifno, ioctl_code, data: uaddr { addr: data_addr as u64 } };
                let ptr = resolve_ptr_mode(arg_mode, &mut ctl as *mut usbdevfs_ioctl as usize);
                // SAFETY: `USBDEVFS_IOCTL` takes a user pointer to `usbdevfs_ioctl`; `ptr` is
                // either stack-allocated `ctl` (live across the call), 0, or an intentional fault
                // address (`PROT_NONE` / OOB), and `ctl.data` is either `data_buf` (live across
                // the call), 0, or an intentional fault address.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_IOCTL as libc::Ioctl, ptr) }
            };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "ifno": ifno, "retval": rc }))
            }
        }
        "control" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let bm_request_type =
                p.get("bm_request_type").and_then(Value::as_u64).unwrap_or(0) as u8;
            let b_request = p.get("b_request").and_then(Value::as_u64).unwrap_or(0) as u8;
            let w_value = p.get("w_value").and_then(Value::as_u64).unwrap_or(0) as u16;
            let w_index = p.get("w_index").and_then(Value::as_u64).unwrap_or(0) as u16;
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let w_length = p
                .get("w_length")
                .and_then(Value::as_u64)
                .map(|v| v as u16)
                .unwrap_or(data_vec.len() as u16);
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000) as u32;

            let is_in = (bm_request_type & 0x80) != 0;
            let buf_len = (w_length as usize).max(data_vec.len());
            let mut canary_buf = CanaryBuffer::new(buf_len, is_in);
            if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(0, &data_vec);
            }
            let data_addr = resolve_ptr_mode(data_mode, canary_buf.user_ptr());

            let rc = if arch32 {
                let mut ctrl32 = UsbdevfsCtrltransfer32 {
                    bRequestType: bm_request_type,
                    bRequest: b_request,
                    wValue: w_value,
                    wIndex: w_index,
                    wLength: w_length,
                    timeout: timeout_ms,
                    data: data_addr as u32,
                };
                let arg_addr =
                    resolve_ptr_mode(arg_mode, &mut ctrl32 as *mut UsbdevfsCtrltransfer32 as usize);
                // SAFETY: Calls 32-bit compat USBDEVFS_CONTROL32 ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_CONTROL32 as libc::Ioctl, arg_addr) }
            } else {
                let mut ctrl64 = usbdevfs_ctrltransfer {
                    bRequestType: bm_request_type,
                    bRequest: b_request,
                    wValue: w_value,
                    wIndex: w_index,
                    wLength: w_length,
                    timeout: timeout_ms,
                    data: uaddr { addr: data_addr as u64 },
                    ..Default::default()
                };
                let arg_addr =
                    resolve_ptr_mode(arg_mode, &mut ctrl64 as *mut usbdevfs_ctrltransfer as usize);
                // SAFETY: Calls 64-bit USBDEVFS_CONTROL ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_CONTROL as libc::Ioctl, arg_addr) }
            };

            if rc < 0 {
                outcome_err(last_errno())
            } else {
                let actual = rc as usize;
                if is_in {
                    outcome_ok(json!({
                        "bytes_transferred": actual,
                        "data": canary_buf.read_range(0, actual),
                        "canary_intact": canary_buf.canary_intact(),
                        "unwritten_tail_intact": canary_buf.unwritten_tail_intact(actual),
                    }))
                } else {
                    outcome_ok(json!({
                        "bytes_transferred": actual,
                        "canary_intact": canary_buf.canary_intact(),
                    }))
                }
            }
        }
        "bulk" => {
            let raw_fd = state.lock().unwrap().get_fd(p);
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(2) as u32;
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let len = p
                .get("len")
                .and_then(Value::as_u64)
                .map(|v| v as u32)
                .unwrap_or(data_vec.len() as u32);
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000) as u32;

            let is_in = (ep & 0x80) != 0;
            let alloc_len = if (len as usize) <= 20 * 1024 * 1024 {
                (len as usize).max(data_vec.len())
            } else {
                4096
            };
            let mut canary_buf = CanaryBuffer::new(alloc_len, is_in);
            if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(0, &data_vec);
            }
            let data_addr = resolve_ptr_mode(data_mode, canary_buf.user_ptr());

            let rc = if arch32 {
                let mut bulk32 =
                    UsbdevfsBulktransfer32 { ep, len, timeout: timeout_ms, data: data_addr as u32 };
                let arg_addr =
                    resolve_ptr_mode(arg_mode, &mut bulk32 as *mut UsbdevfsBulktransfer32 as usize);
                // SAFETY: Calls 32-bit compat USBDEVFS_BULK32 ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_BULK32 as libc::Ioctl, arg_addr) }
            } else {
                let mut bulk64 = usbdevfs_bulktransfer {
                    ep,
                    len,
                    timeout: timeout_ms,
                    data: uaddr { addr: data_addr as u64 },
                    ..Default::default()
                };
                let arg_addr =
                    resolve_ptr_mode(arg_mode, &mut bulk64 as *mut usbdevfs_bulktransfer as usize);
                // SAFETY: Calls 64-bit USBDEVFS_BULK ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_BULK as libc::Ioctl, arg_addr) }
            };

            if rc < 0 {
                outcome_err(last_errno())
            } else {
                let actual = rc as usize;
                if is_in {
                    outcome_ok(json!({
                        "bytes_transferred": actual,
                        "data": canary_buf.read_range(0, actual),
                        "canary_intact": canary_buf.canary_intact(),
                        "unwritten_tail_intact": canary_buf.unwritten_tail_intact(actual),
                    }))
                } else {
                    *state.lock().unwrap().unpopped_out_packets.entry(ep as u8).or_insert(0) += 1;
                    outcome_ok(json!({
                        "bytes_transferred": actual,
                        "canary_intact": canary_buf.canary_intact(),
                    }))
                }
            }
        }

        // 3. Asynchronous URB Operations
        "submit_urb" => {
            let mut st = state.lock().unwrap();
            let raw_fd = st.get_fd(p);
            let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1").to_string();
            let urb_type_val = match p.get("urb_type") {
                Some(Value::String(s)) => match s.as_str() {
                    "iso" => USBDEVFS_URB_TYPE_ISO as u8,
                    "interrupt" => USBDEVFS_URB_TYPE_INTERRUPT as u8,
                    "control" => USBDEVFS_URB_TYPE_CONTROL as u8,
                    "bulk" => USBDEVFS_URB_TYPE_BULK as u8,
                    _ => 99,
                },
                Some(Value::Number(n)) => n.as_u64().unwrap_or(3) as u8,
                _ => USBDEVFS_URB_TYPE_BULK as u8,
            };
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x81) as u8;
            let flags = parse_urb_flags(p);
            let signr = p.get("signr").and_then(Value::as_u64).unwrap_or(0) as u32;
            let usercontext = p.get("usercontext").and_then(Value::as_u64).unwrap_or(0);
            let data_vec: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();

            let is_control = urb_type_val == (USBDEVFS_URB_TYPE_CONTROL as u8);
            let setup_opt = p.get("setup");
            let is_in = if is_control {
                if let Some(setup_obj) = setup_opt {
                    let bm =
                        setup_obj.get("bm_request_type").and_then(Value::as_u64).unwrap_or(0) as u8;
                    let w_len = setup_obj.get("w_length").and_then(Value::as_u64).unwrap_or(0);
                    (bm & 0x80) != 0 && w_len > 0
                } else if data_vec.len() >= 8 {
                    let bm = data_vec[0];
                    let w_len = u16::from_le_bytes([data_vec[6], data_vec[7]]);
                    (bm & 0x80) != 0 && w_len > 0
                } else {
                    (ep & 0x80) != 0
                }
            } else {
                (ep & 0x80) != 0
            };

            let default_buf_len = if is_control && setup_opt.is_some() {
                let w_len = setup_opt
                    .unwrap()
                    .get("w_length")
                    .and_then(Value::as_u64)
                    .unwrap_or(data_vec.len() as u64) as i32;
                8 + w_len
            } else {
                data_vec.len() as i32
            };
            let buffer_len = p
                .get("buffer_len")
                .and_then(Value::as_i64)
                .map(|v| v as i32)
                .unwrap_or(default_buf_len);

            let alloc_user_len = if buffer_len > 0 && (buffer_len as usize) <= 20 * 1024 * 1024 {
                (buffer_len as usize).max(data_vec.len() + if is_control { 8 } else { 0 })
            } else {
                64
            };
            let mut canary_buf = CanaryBuffer::new(alloc_user_len, is_in);
            if is_control {
                if let Some(setup_obj) = setup_opt {
                    let ctrl_req = usb_ctrlrequest {
                        bRequestType: setup_obj
                            .get("bm_request_type")
                            .and_then(Value::as_u64)
                            .unwrap_or(0) as u8,
                        bRequest: setup_obj.get("b_request").and_then(Value::as_u64).unwrap_or(0)
                            as u8,
                        wValue: (setup_obj.get("w_value").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                        wIndex: (setup_obj.get("w_index").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                        wLength: (setup_obj.get("w_length").and_then(Value::as_u64).unwrap_or(0)
                            as u16)
                            .to_le(),
                    };
                    canary_buf.write_at(0, ctrl_req.as_bytes());
                    if !is_in && !data_vec.is_empty() {
                        canary_buf.write_at(8, &data_vec);
                    }
                } else if !data_vec.is_empty() {
                    canary_buf.write_at(0, &data_vec);
                }
            } else if !is_in && !data_vec.is_empty() {
                canary_buf.write_at(0, &data_vec);
            }

            let buf_addr = resolve_ptr_mode(data_mode, canary_buf.user_ptr());
            let (urb_page_ptr, owns_page) =
                if let Some(same_as) = p.get("same_addr_as").and_then(Value::as_str) {
                    if let Some(existing) = st.urbs.get(same_as) {
                        (existing.urb_page_ptr, false)
                    } else {
                        (map_low32_pages(4096, libc::PROT_READ | libc::PROT_WRITE), true)
                    }
                } else {
                    (map_low32_pages(4096, libc::PROT_READ | libc::PROT_WRITE), true)
                };

            let rc = if arch32 {
                let hdr32 = UsbdevfsUrb32Header {
                    type_: urb_type_val,
                    endpoint: ep,
                    _pad0: [0; 2],
                    status: 0,
                    flags,
                    buffer: buf_addr as u32,
                    buffer_length: buffer_len,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets_or_stream_id: 0,
                    error_count: 0,
                    signr,
                    usercontext: usercontext as u32,
                };
                // SAFETY: Writes 32-bit URB header into mapped 4096-byte page.
                unsafe {
                    std::ptr::write(urb_page_ptr as *mut UsbdevfsUrb32Header, hdr32);
                }
                let arg_addr = resolve_ptr_mode(arg_mode, urb_page_ptr as usize);
                // SAFETY: Calls 32-bit compat USBDEVFS_SUBMITURB32 ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_SUBMITURB32 as libc::Ioctl, arg_addr) }
            } else {
                let hdr64 = UsbdevfsUrbHeader {
                    type_: urb_type_val,
                    endpoint: ep,
                    _pad0: [0; 2],
                    status: 0,
                    flags,
                    _pad1: [0; 4],
                    buffer: buf_addr as u64,
                    buffer_length: buffer_len,
                    actual_length: 0,
                    start_frame: 0,
                    number_of_packets_or_stream_id: 0,
                    error_count: 0,
                    signr,
                    usercontext,
                };
                // SAFETY: Writes 64-bit URB header into mapped 4096-byte page.
                unsafe {
                    std::ptr::write(urb_page_ptr as *mut UsbdevfsUrbHeader, hdr64);
                }
                let arg_addr = resolve_ptr_mode(arg_mode, urb_page_ptr as usize);
                // SAFETY: Calls 64-bit USBDEVFS_SUBMITURB ioctl.
                unsafe { libc::ioctl(raw_fd, USBDEVFS_SUBMITURB as libc::Ioctl, arg_addr) }
            };

            if rc == 0 || !st.addr_to_urb.contains_key(&(urb_page_ptr as usize)) {
                st.addr_to_urb.insert(urb_page_ptr as usize, urb_name.clone());
            }
            st.urbs.insert(
                urb_name.clone(),
                UrbSlot {
                    fd: raw_fd,
                    urb_page_ptr,
                    owns_page,
                    arch32,
                    is_in,
                    is_control,
                    in_flight: rc == 0,
                    canary_buf,
                },
            );

            if rc < 0 { outcome_err(last_errno()) } else { outcome_ok(json!({ "urb": urb_name })) }
        }
        "discard_urb" => {
            let (raw_fd, urb_addr, urb_name) = {
                let st = state.lock().unwrap();
                let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1").to_string();
                let addr = st.urbs.get(&urb_name).map(|u| u.urb_page_ptr as usize).unwrap_or(0);
                (st.get_fd(p), addr, urb_name)
            };
            let arg_addr = resolve_ptr_mode(arg_mode, urb_addr);
            // SAFETY: Calls USBDEVFS_DISCARDURB ioctl with `arg_addr`.
            let rc = unsafe { libc::ioctl(raw_fd, USBDEVFS_DISCARDURB as libc::Ioctl, arg_addr) };
            if rc < 0 {
                outcome_err(last_errno())
            } else {
                outcome_ok(json!({ "discarded": urb_name }))
            }
        }
        "reap_urb_ndelay" | "reap_urb" => {
            let blocking = step.op == "reap_urb";
            let raw_fd = state.lock().unwrap().get_fd(p);
            let reap_buf = CanaryBuffer::new(16, false);
            let scratch_ptr = reap_buf.user_ptr();
            let arg_addr = resolve_ptr_mode(arg_mode, scratch_ptr);
            let req = match (blocking, arch32) {
                (false, false) => USBDEVFS_REAPURBNDELAY,
                (false, true) => USBDEVFS_REAPURBNDELAY32,
                (true, false) => USBDEVFS_REAPURB,
                (true, true) => USBDEVFS_REAPURB32,
            };
            // SAFETY: Calls USBDEVFS_REAPURB / REAPURBNDELAY (64-bit or 32-bit) ioctl with per-call `reap_buf`.
            let rc = unsafe { libc::ioctl(raw_fd, req as libc::Ioctl, arg_addr) };
            if rc < 0 {
                return outcome_err(last_errno());
            }
            let reaped_ptr = if arch32 {
                // SAFETY: Reads 32-bit reaped URB pointer from `reap_buf`.
                (unsafe { std::ptr::read(scratch_ptr as *const u32) }) as usize
            } else {
                // SAFETY: Reads 64-bit reaped URB pointer from `reap_buf`.
                (unsafe { std::ptr::read(scratch_ptr as *const u64) }) as usize
            };
            let mut st = state.lock().unwrap();
            let urb_name = st.addr_to_urb.get(&reaped_ptr).cloned().unwrap_or_default();
            let (status_i32, actual_length, usercontext) = if let Some(slot) =
                st.urbs.get_mut(&urb_name)
            {
                slot.in_flight = false;
                if slot.arch32 {
                    // SAFETY: Reads 32-bit URB header from `slot.urb_page_ptr`.
                    let h =
                        unsafe { std::ptr::read(slot.urb_page_ptr as *const UsbdevfsUrb32Header) };
                    (h.status, h.actual_length, h.usercontext as u64)
                } else {
                    // SAFETY: Reads 64-bit URB header from `slot.urb_page_ptr`.
                    let h =
                        unsafe { std::ptr::read(slot.urb_page_ptr as *const UsbdevfsUrbHeader) };
                    (h.status, h.actual_length, h.usercontext)
                }
            } else {
                (0, 0, 0)
            };
            outcome_ok(json!({
                "urb": urb_name,
                "status": urb_status_to_string(status_i32),
                "actual_length": actual_length,
                "usercontext": usercontext,
            }))
        }
        "inspect_urb" => {
            let st = state.lock().unwrap();
            let urb_name = p.get("urb").and_then(Value::as_str).unwrap_or("u1");
            let Some(slot) = st.urbs.get(urb_name) else {
                return outcome_err(libc::ENOENT);
            };
            let (status_i32, actual_length_i32, error_count, usercontext) = if slot.arch32 {
                // SAFETY: Reads 32-bit URB header from `slot.urb_page_ptr`.
                let h = unsafe { std::ptr::read(slot.urb_page_ptr as *const UsbdevfsUrb32Header) };
                (h.status, h.actual_length, h.error_count, h.usercontext as u64)
            } else {
                // SAFETY: Reads 64-bit URB header from `slot.urb_page_ptr`.
                let h = unsafe { std::ptr::read(slot.urb_page_ptr as *const UsbdevfsUrbHeader) };
                (h.status, h.actual_length, h.error_count, h.usercontext)
            };
            let actual_len = actual_length_i32.max(0) as usize;
            let canary_intact = slot.canary_buf.canary_intact();
            let (data, trailing_intact) = if slot.is_control {
                if slot.is_in {
                    (
                        slot.canary_buf.read_range(8, 8 + actual_len),
                        slot.canary_buf.unwritten_tail_intact(8 + actual_len),
                    )
                } else {
                    (Vec::new(), true)
                }
            } else if slot.is_in {
                (
                    slot.canary_buf.read_range(0, actual_len),
                    slot.canary_buf.unwritten_tail_intact(actual_len),
                )
            } else {
                (Vec::new(), true)
            };
            outcome_ok(json!({
                "urb": urb_name,
                "status": urb_status_to_string(status_i32),
                "actual_length": actual_length_i32,
                "error_count": error_count,
                "usercontext": usercontext,
                "data": data,
                "canary_intact": canary_intact,
                "trailing_canary_intact": trailing_intact,
                "unwritten_tail_intact": trailing_intact,
            }))
        }

        // 4. Concurrency Primitives
        "start_op" => {
            let task_name = p.get("task").and_then(Value::as_str).unwrap_or("t1").to_string();
            let inner_val = p.get("inner").cloned().unwrap_or(Value::Null);
            let inner_step: Step =
                serde_json::from_value(inner_val).expect("valid inner Step in start_op");
            let state_clone = Arc::clone(state);
            let done_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let done_flag_worker = Arc::clone(&done_flag);
            let (tid_tx, tid_rx) = mpsc::sync_channel::<i32>(1);
            let (out_tx, out_rx) = mpsc::sync_channel::<StepOutcome>(1);

            thread::spawn(move || {
                // SAFETY: Queries Linux kernel TID of current worker thread via SYS_gettid.
                let tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
                let _ = tid_tx.send(tid);
                let mut nested_tasks = HashMap::new();
                let res = execute_step(&inner_step, &state_clone, &mut nested_tasks);
                done_flag_worker.store(true, Ordering::SeqCst);
                let _ = out_tx.send(res);
            });

            if let Ok(tid) = tid_rx.recv() {
                wait_for_tid_sleeping(tid, &done_flag);
            }
            tasks.insert(task_name.clone(), out_rx);
            outcome_ok(json!({ "task": task_name, "started": true }))
        }
        "await_task" => {
            let task_name = p.get("task").and_then(Value::as_str).unwrap_or("t1");
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(5000);
            let Some(rx) = tasks.remove(task_name) else {
                return outcome_err(libc::ESRCH);
            };
            match rx.recv_timeout(Duration::from_millis(timeout_ms)) {
                Ok(inner_outcome) => inner_outcome,
                Err(_) => outcome_err(libc::ETIMEDOUT),
            }
        }

        // 5. Device / Gadget Actions
        "device_script_control" => {
            let addr = state.lock().unwrap().control_addr.clone();
            let bm_request_type = p.get("bm_request_type").and_then(Value::as_u64).map(|v| v as u8);
            let b_request = p.get("b_request").and_then(Value::as_u64).unwrap_or(0) as u8;
            let w_value = p.get("w_value").and_then(Value::as_u64).map(|v| v as u16);
            let w_index = p.get("w_index").and_then(Value::as_u64).map(|v| v as u16);
            let w_length = p.get("w_length").and_then(Value::as_u64).map(|v| v as u16);
            let action = p.get("action").cloned().unwrap_or(Value::Null);
            let resp = send_device_command(
                &addr,
                &DeviceCommand::ScriptControl {
                    bm_request_type,
                    b_request,
                    w_value,
                    w_index,
                    w_length,
                    action,
                },
            )
            .expect("ScriptControl RPC failed");
            assert!(resp.ok, "ScriptControl failed: {:?}", resp.error);
            outcome_ok(json!({ "ok": true }))
        }
        "device_release_control" => {
            let addr = state.lock().unwrap().control_addr.clone();
            let hold = p.get("hold").and_then(Value::as_str).unwrap_or("h1").to_string();
            let action = p.get("action").cloned().unwrap_or(Value::Null);
            let resp = send_device_command(&addr, &DeviceCommand::ReleaseControl { hold, action })
                .expect("ReleaseControl RPC failed");
            assert!(resp.ok, "ReleaseControl failed: {:?}", resp.error);
            outcome_ok(json!({ "ok": true }))
        }
        "device_ep_push_in" => {
            let addr = state.lock().unwrap().control_addr.clone();
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x81) as u8;
            let data: Vec<u8> = p
                .get("data")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(|v| v.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default();
            let len = data.len();
            let resp = send_device_command(&addr, &DeviceCommand::PushIn { ep, data })
                .expect("PushIn RPC failed");
            assert!(resp.ok, "PushIn failed: {:?}", resp.error);
            outcome_ok(json!({ "ep": ep, "bytes_pushed": len }))
        }
        "device_ep_pop_out" => {
            let addr = state.lock().unwrap().control_addr.clone();
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x02) as u8;
            let timeout_ms = p.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000);
            let resp = send_device_command(&addr, &DeviceCommand::PopOut { ep, timeout_ms })
                .expect("PopOut RPC failed");
            if resp.ok {
                if let Some(cnt) = state.lock().unwrap().unpopped_out_packets.get_mut(&ep) {
                    *cnt = cnt.saturating_sub(1);
                }
                let data = resp.data.unwrap_or_default();
                outcome_ok(json!({
                    "ep": ep,
                    "bytes_popped": data.len(),
                    "data": data,
                }))
            } else if resp.error.as_deref() == Some("ETIMEDOUT") {
                outcome_err(libc::ETIMEDOUT)
            } else {
                panic!("PopOut failed: {:?}", resp.error);
            }
        }
        "device_ep_assert_empty" => {
            let (addr, expect_non_empty) = {
                let st = state.lock().unwrap();
                (
                    st.control_addr.clone(),
                    st.unpopped_out_packets
                        .get(&(p.get("ep").and_then(Value::as_u64).unwrap_or(0x02) as u8))
                        .copied()
                        .unwrap_or(0)
                        > 0,
                )
            };
            let ep = p.get("ep").and_then(Value::as_u64).unwrap_or(0x02) as u8;
            let deadline = Instant::now() + Duration::from_millis(2000);
            let mut resp = send_device_command(&addr, &DeviceCommand::AssertEmpty { ep })
                .expect("AssertEmpty RPC failed");
            while expect_non_empty && resp.ok && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
                resp = send_device_command(&addr, &DeviceCommand::AssertEmpty { ep })
                    .expect("AssertEmpty RPC failed");
            }
            if expect_non_empty && resp.ok {
                panic!(
                    "AssertEmpty timed out waiting for unpopped packet on ep 0x{ep:02x} to arrive at daemon"
                );
            }
            if resp.ok {
                outcome_ok(json!({ "ep": ep, "empty": true }))
            } else if resp.error.as_deref() == Some("EAGAIN") {
                outcome_err(libc::EAGAIN)
            } else {
                panic!("AssertEmpty failed: {:?}", resp.error);
            }
        }
        "device_disconnect" => {
            let (addr, wait_for_held_transfer, open_fds) = {
                let mut st = state.lock().unwrap();
                let has_inflight_urb =
                    st.urbs.values().any(|u| u.in_flight && (u.is_in || u.is_control));
                let fds: Vec<(RawFd, bool)> = st
                    .fds
                    .values()
                    .map(|&fd| (fd, st.urbs.values().any(|u| u.in_flight && u.fd == fd)))
                    .collect();
                for u in st.urbs.values_mut() {
                    u.in_flight = false;
                }
                st.unpopped_out_packets.clear();
                (st.control_addr.clone(), !tasks.is_empty() || has_inflight_urb, fds)
            };
            let resp =
                send_device_command(&addr, &DeviceCommand::Disconnect { wait_for_held_transfer })
                    .expect("Disconnect RPC failed");
            assert!(resp.ok, "Disconnect failed: {:?}", resp.error);
            // Wait until host-side USB device removal has finished notifying all open `usbfs` file
            // descriptors (`POLLERR | POLLHUP`, plus `POLLOUT` on any fd that had an un-reaped URB
            // in flight at disconnect) before running subsequent `poll` steps.
            let poll_deadline = Instant::now() + Duration::from_secs(2);
            for (fd, expect_pollout) in open_fds {
                let events = if expect_pollout { libc::POLLOUT } else { 0 };
                let expected =
                    libc::POLLERR | libc::POLLHUP | if expect_pollout { libc::POLLOUT } else { 0 };
                loop {
                    let remaining_ms = poll_deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(libc::c_int::MAX as u128)
                        as libc::c_int;
                    let mut pfd = libc::pollfd { fd, events, revents: 0 };
                    // SAFETY: Polls open `usbfs` fd to observe disconnect revents.
                    let rc = unsafe { libc::poll(&mut pfd, 1, remaining_ms) };
                    if rc < 0
                        || (pfd.revents & libc::POLLNVAL) != 0
                        || (pfd.revents & expected) == expected
                        || remaining_ms == 0
                    {
                        break;
                    }
                    thread::yield_now();
                }
            }
            outcome_ok(json!({ "disconnected": true }))
        }
        "device_reconnect" => {
            let addr = state.lock().unwrap().control_addr.clone();
            let resp = send_device_command(&addr, &DeviceCommand::Reconnect)
                .expect("Reconnect RPC failed");
            assert!(resp.ok, "Reconnect failed: {:?}", resp.error);
            let mut st = state.lock().unwrap();
            st.unpopped_out_packets.clear();
            st.discover_device();
            outcome_ok(json!({ "reconnected": true }))
        }
        other => panic!("unsupported Step op: {other}"),
    }
}

/// Executes a declarative [`Scenario`] against Linux and records its [`RecordedTrace`].
pub fn record_scenario(scenario: &Scenario, control_addr: &str) -> RecordedTrace {
    let _guard = lock_execution();

    let resp = send_device_command(
        control_addr,
        &DeviceCommand::ConfigureDevice { device: scenario.device.clone() },
    )
    .expect("ConfigureDevice RPC failed");
    assert!(resp.ok, "ConfigureDevice failed: {:?}", resp.error);

    let state = Arc::new(Mutex::new(SharedExecState::new(scenario.device.clone(), control_addr)));
    let mut tasks = HashMap::new();
    let mut recorded_steps = Vec::with_capacity(scenario.steps.len());

    for step in &scenario.steps {
        let outcome = execute_step(step, &state, &mut tasks);
        recorded_steps.push(RecordedStep { step: step.clone(), outcome });
    }

    RecordedTrace {
        name: scenario.name.clone(),
        device: scenario.device.clone(),
        steps: recorded_steps,
    }
}

fn verify_outcome_with_divergences(
    scenario_name: &str,
    step_idx: usize,
    step_op: &str,
    expected: &StepOutcome,
    actual: &StepOutcome,
    divergences: &DivergenceManifest,
    apply_divergences: bool,
) -> Result<(), String> {
    divergences
        .matches_or_allows(scenario_name, step_op, expected, actual, apply_divergences)
        .map_err(|e| format!("scenario '{scenario_name}' step #{step_idx} ({step_op}): {e}"))
}

struct RestoreDefaultDeviceGuard<'a> {
    control_addr: &'a str,
}

impl Drop for RestoreDefaultDeviceGuard<'_> {
    fn drop(&mut self) {
        let _ = send_device_command(
            self.control_addr,
            &DeviceCommand::ConfigureDevice { device: DeviceSpec::default() },
        );
        let _ = send_device_command(
            self.control_addr,
            &DeviceCommand::ScriptControl {
                bm_request_type: Some(0x40),
                b_request: 0xfe,
                w_value: None,
                w_index: None,
                w_length: None,
                action: json!({ "auto_echo": true }),
            },
        );
    }
}

/// Replays a [`RecordedTrace`] over `libc` syscalls and verifies every step outcome.
pub fn replay_trace(
    trace: &RecordedTrace,
    divergences: &DivergenceManifest,
    apply_divergences: bool,
    control_addr: &str,
) {
    let _guard = lock_execution();
    ensure_control_daemon_running(control_addr);
    let _restore = RestoreDefaultDeviceGuard { control_addr };

    let resp = send_device_command(
        control_addr,
        &DeviceCommand::ConfigureDevice { device: trace.device.clone() },
    )
    .expect("ConfigureDevice RPC failed");
    assert!(resp.ok, "ConfigureDevice failed: {:?}", resp.error);

    let state = Arc::new(Mutex::new(SharedExecState::new(trace.device.clone(), control_addr)));
    let mut tasks = HashMap::new();

    for (idx, recorded) in trace.steps.iter().enumerate() {
        let actual = execute_step(&recorded.step, &state, &mut tasks);
        if let Err(msg) = verify_outcome_with_divergences(
            &trace.name,
            idx,
            &recorded.step.op,
            &recorded.outcome,
            &actual,
            divergences,
            apply_divergences,
        ) {
            panic!("{msg}\n  expected: {:?}\n  actual:   {:?}", recorded.outcome, actual);
        }
    }
}
