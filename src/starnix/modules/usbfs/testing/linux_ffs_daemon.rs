// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Dynamic Linux `configfs` + `functionfs` + `dummy_hcd` gadget controller serving
//! [`DeviceCommand`] requests for `usbfs` trace recording and replay.

use linux_uapi::{
    usb_functionfs_event, usb_functionfs_event_type_FUNCTIONFS_DISABLE,
    usb_functionfs_event_type_FUNCTIONFS_ENABLE, usb_functionfs_event_type_FUNCTIONFS_SETUP,
};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::symlink;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use usbfs_trace_schema::{DeviceCommand, DeviceResponse, DeviceSpec, EndpointSpec};
use zerocopy::FromBytes as _;

const FFS_MOUNT_DIR: &str = "/dev/ffs";
const GADGET_DIR: &str = "/sys/kernel/config/usb_gadget/g1";
const ENUMERATION_TIMEOUT: Duration = Duration::from_secs(10);

extern "C" fn sigusr1_noop_handler(_sig: libc::c_int) {}

fn ensure_sigusr1_handler() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: Registers a process-wide no-op SIGUSR1 handler without SA_RESTART so blocking
        // FunctionFS endpoint read/write syscalls can be interrupted cleanly during teardown.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = sigusr1_noop_handler as *const () as usize;
            libc::sigemptyset(&mut sa.sa_mask);
            sa.sa_flags = 0;
            libc::sigaction(libc::SIGUSR1, &sa, std::ptr::null_mut());
        }
    });
}

#[derive(Clone, Debug)]
enum ControlScriptAction {
    Reply(Vec<u8>),
    Stall,
    Hold(String),
}

impl ControlScriptAction {
    fn from_value(v: &Value) -> Option<Self> {
        if v.get("stall").and_then(Value::as_bool) == Some(true) {
            return Some(Self::Stall);
        }
        if let Some(hold_id) = v.get("hold").and_then(Value::as_str) {
            return Some(Self::Hold(hold_id.to_string()));
        }
        if let Some(arr) = v.get("reply").and_then(Value::as_array) {
            let bytes: Vec<u8> = arr.iter().filter_map(|x| x.as_u64().map(|b| b as u8)).collect();
            return Some(Self::Reply(bytes));
        }
        None
    }
}

#[derive(Clone, Debug)]
struct ScriptedControlRule {
    bm_request_type: Option<u8>,
    b_request: u8,
    w_value: Option<u16>,
    w_index: Option<u16>,
    w_length: Option<u16>,
    action: ControlScriptAction,
}

impl ScriptedControlRule {
    fn matches(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        w_length: u16,
    ) -> bool {
        if self.b_request != b_request {
            return false;
        }
        match self.bm_request_type {
            Some(expected_bm) if expected_bm != bm_request_type => return false,
            None if (bm_request_type & 0x60) == 0 => return false,
            _ => {}
        }
        if self.w_value.is_some_and(|v| v != w_value) {
            return false;
        }
        if self.w_index.is_some_and(|v| v != w_index) {
            return false;
        }
        if self.w_length.is_some_and(|v| v != w_length) {
            return false;
        }
        true
    }
}

struct OutEndpointQueue {
    packets: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
    worker_tid: AtomicI32,
    in_read_syscall: AtomicBool,
}

impl OutEndpointQueue {
    fn new() -> Self {
        Self {
            packets: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
            worker_tid: AtomicI32::new(0),
            in_read_syscall: AtomicBool::new(false),
        }
    }
}

struct InEndpointQueue {
    packets: Mutex<VecDeque<Vec<u8>>>,
    cv: Condvar,
}

impl InEndpointQueue {
    fn new() -> Self {
        Self { packets: Mutex::new(VecDeque::new()), cv: Condvar::new() }
    }
}

struct HeldControlState {
    released_actions: HashMap<String, ControlScriptAction>,
}

struct SharedGadgetRuntime {
    stop: AtomicBool,
    enabled: AtomicBool,
    auto_echo: AtomicBool,
    in_flight_writes: AtomicUsize,
    scripted_controls: Mutex<Vec<ScriptedControlRule>>,
    held_controls: (Mutex<HeldControlState>, Condvar),
    last_ctrl_out: Mutex<Vec<u8>>,
    in_queues: HashMap<u8, Arc<InEndpointQueue>>,
    out_queues: HashMap<u8, Arc<OutEndpointQueue>>,
}

struct ActiveGadget {
    spec: DeviceSpec,
    udc_name: String,
    connected: bool,
    ep_fds: Vec<RawFd>,
    workers: Vec<(Option<libc::pthread_t>, JoinHandle<()>)>,
    runtime: Arc<SharedGadgetRuntime>,
}

static GADGET_STATE: OnceLock<Mutex<Option<ActiveGadget>>> = OnceLock::new();

fn gadget_state() -> &'static Mutex<Option<ActiveGadget>> {
    GADGET_STATE.get_or_init(|| Mutex::new(None))
}

/// Returns the default `0x1234:0x5678` [`DeviceSpec`] matching `usb_test.rs`.
pub fn default_usb_test_device_spec() -> DeviceSpec {
    DeviceSpec::default()
}

/// Builds FunctionFS v2 descriptors (`FS | HS | SS | ALL_CTRL_RECIP | CONFIG0_SETUP`) and returns
/// `(descriptor_bytes, ordered_unique_endpoints)` where `ordered_unique_endpoints[i]` corresponds
/// to `/dev/ffs/ep{i + 1}`.
pub fn build_ffs_v2_descriptors_and_eps(device: &DeviceSpec) -> (Vec<u8>, Vec<EndpointSpec>) {
    device.build_ffs_descriptors_v2_and_eps()
}

fn find_udc_name() -> io::Result<String> {
    let mut entries = fs::read_dir("/sys/class/udc")?;
    let entry = entries
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no UDC in /sys/class/udc"))??;
    Ok(entry.file_name().to_string_lossy().into_owned())
}

pub(crate) fn kill_external_ffs_holders() {
    let self_pid = std::process::id();
    let Ok(proc_entries) = fs::read_dir("/proc") else {
        return;
    };
    let mut killed_pids = Vec::new();
    for entry in proc_entries.flatten() {
        let fname = entry.file_name();
        let Ok(pid) = fname.to_string_lossy().parse::<u32>() else {
            continue;
        };
        if pid == self_pid || pid <= 1 {
            continue;
        }
        let fd_dir = entry.path().join("fd");
        let holds_ffs = fs::read_dir(fd_dir).ok().into_iter().flatten().flatten().any(|fd_entry| {
            fs::read_link(fd_entry.path())
                .map(|target| target.to_string_lossy().starts_with("/dev/ffs"))
                .unwrap_or(false)
        });
        let is_ffs_daemon = fs::read(entry.path().join("environ"))
            .map(|e| e.windows(22).any(|w| w == b"USBFS_FFS_DAEMON_ONLY="))
            .unwrap_or(false);
        if holds_ffs || is_ffs_daemon {
            // SAFETY: Sends SIGKILL to a sibling test daemon holding /dev/ffs/* fds.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGKILL);
                libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG);
            }
            killed_pids.push(pid);
        }
    }
    for pid in killed_pids {
        let proc_path = PathBuf::from(format!("/proc/{pid}"));
        let status_path = proc_path.join("status");
        let fd_dir = proc_path.join("fd");
        let deadline = Instant::now() + Duration::from_secs(2);
        while proc_path.exists() && Instant::now() < deadline {
            // SAFETY: Reaps terminated child process if `pid` is a child of this process.
            let wait_rc =
                unsafe { libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG) };
            if wait_rc > 0 {
                break;
            }
            let is_zombie = fs::read_to_string(&status_path)
                .map(|s| s.lines().any(|line| line.starts_with("State:\tZ")))
                .unwrap_or(true);
            let fds_closed =
                fs::read_dir(&fd_dir).map(|mut entries| entries.next().is_none()).unwrap_or(true);
            if is_zombie && fds_closed {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}

fn find_enumerated_usb_device(vendor_id: u16, product_id: u16) -> Option<(PathBuf, u32, u32)> {
    let entries = fs::read_dir("/sys/bus/usb/devices").ok()?;
    let expected_vid = format!("{:04x}", vendor_id);
    let expected_pid = format!("{:04x}", product_id);
    for entry in entries.flatten() {
        let path = entry.path();
        let vid = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
        let pid = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
        if vid.trim().eq_ignore_ascii_case(&expected_vid)
            && pid.trim().eq_ignore_ascii_case(&expected_pid)
        {
            let busnum = fs::read_to_string(path.join("busnum"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())?;
            let devnum = fs::read_to_string(path.join("devnum"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())?;
            return Some((path, busnum, devnum));
        }
    }
    None
}

fn wait_for_enumeration(vendor_id: u16, product_id: u16) -> Result<(), String> {
    let deadline = Instant::now() + ENUMERATION_TIMEOUT;
    while Instant::now() < deadline {
        if let Some((sysfs_dir, busnum, devnum)) = find_enumerated_usb_device(vendor_id, product_id)
        {
            let dev_node = PathBuf::from(format!("/dev/bus/usb/{:03}/{:03}", busnum, devnum));
            let cfg_val =
                fs::read_to_string(sysfs_dir.join("bConfigurationValue")).unwrap_or_default();
            if dev_node.exists() && !cfg_val.trim().is_empty() {
                return Ok(());
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "timed out waiting for USB device {:04x}:{:04x} to enumerate",
        vendor_id, product_id
    ))
}

fn wait_for_disconnection(
    vendor_id: u16,
    product_id: u16,
    real_sysfs_dir: Option<&Path>,
    dev_node: Option<&Path>,
) -> Result<(), String> {
    let deadline = Instant::now() + ENUMERATION_TIMEOUT;
    while Instant::now() < deadline {
        let bus_link_gone = find_enumerated_usb_device(vendor_id, product_id).is_none();
        let sysfs_gone = real_sysfs_dir.is_none_or(|dir| !dir.exists());
        let dev_node_gone = dev_node.is_none_or(|node| !node.exists());
        if bus_link_gone && sysfs_gone && dev_node_gone {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Err(format!(
        "timed out waiting for USB device {:04x}:{:04x} to disconnect",
        vendor_id, product_id
    ))
}

fn is_tid_sleeping(tid: i32) -> bool {
    if tid <= 0 {
        return true;
    }
    let stat_path = format!("/proc/self/task/{tid}/stat");
    let Ok(stat_str) = fs::read_to_string(&stat_path) else {
        return true;
    };
    let Some(rparen) = stat_str.rfind(')') else {
        return false;
    };
    let state_ch = stat_str[rparen + 1..].trim_start().chars().next().unwrap_or(' ');
    state_ch == 'S'
}

fn teardown_gadget(active: &mut Option<ActiveGadget>) {
    let old_vid_pid = active.as_ref().map(|g| (g.spec.vendor_id, g.spec.product_id));
    let had_active = active.is_some();

    let g1 = Path::new(GADGET_DIR);
    if g1.join("UDC").exists() {
        let _ = fs::write(g1.join("UDC"), "\n");
    }

    if let Some(mut gadget) = active.take() {
        gadget.runtime.stop.store(true, Ordering::SeqCst);
        gadget.runtime.held_controls.1.notify_all();
        for in_q in gadget.runtime.in_queues.values() {
            in_q.cv.notify_all();
        }
        for out_q in gadget.runtime.out_queues.values() {
            out_q.cv.notify_all();
        }
        for (tid_opt, handle) in gadget.workers.drain(..) {
            while !handle.is_finished() {
                if let Some(tid) = tid_opt {
                    // SAFETY: Sends SIGUSR1 to this unjoined worker thread to interrupt blocking FunctionFS I/O.
                    unsafe {
                        libc::pthread_kill(tid, libc::SIGUSR1);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let _ = handle.join();
        }
        for fd in gadget.ep_fds.drain(..) {
            // SAFETY: Closes owned FunctionFS endpoint fds after all worker threads have exited.
            unsafe {
                libc::close(fd);
            }
        }
    }

    if !had_active {
        kill_external_ffs_holders();
    }

    if let Ok(c_ffs) = CString::new(FFS_MOUNT_DIR) {
        // SAFETY: Detaches the /dev/ffs mount after all endpoint fds are closed.
        unsafe {
            libc::umount2(c_ffs.as_ptr(), libc::MNT_DETACH);
        }
    }

    if g1.exists() {
        if let Ok(configs) = fs::read_dir(g1.join("configs")) {
            for cfg_entry in configs.flatten() {
                let cfg_path = cfg_entry.path();
                if let Ok(cfg_children) = fs::read_dir(&cfg_path) {
                    for child in cfg_children.flatten() {
                        let child_path = child.path();
                        if child_path.is_symlink()
                            || child.file_name().to_string_lossy().starts_with('f')
                        {
                            let _ = fs::remove_file(&child_path);
                        }
                    }
                }
                let _ = fs::remove_dir(cfg_path.join("strings/0x409"));
                let _ = fs::remove_dir(&cfg_path);
            }
        }
        if let Ok(funcs) = fs::read_dir(g1.join("functions")) {
            for func_entry in funcs.flatten() {
                let _ = fs::remove_dir(func_entry.path());
            }
        }
        let _ = fs::remove_dir(g1.join("strings/0x409"));
        let _ = fs::remove_dir(g1);
    }

    if let Some((vid, pid)) = old_vid_pid {
        let _ = wait_for_disconnection(vid, pid, None, None);
    }
}

fn stall_ep0(ep0_fd: RawFd, is_in: bool) {
    let mut placeholder = [0u8; 1];
    if is_in {
        // SAFETY: Performs opposite-direction read on ep0_fd to trigger a FunctionFS protocol STALL.
        unsafe {
            libc::read(ep0_fd, placeholder.as_mut_ptr() as *mut libc::c_void, 1);
        }
    } else {
        // SAFETY: Performs opposite-direction write on ep0_fd to trigger a FunctionFS protocol STALL.
        unsafe {
            libc::write(ep0_fd, placeholder.as_ptr() as *const libc::c_void, 1);
        }
    }
}

fn spawn_ep0_worker(
    ep0_fd: RawFd,
    runtime: Arc<SharedGadgetRuntime>,
    tid_tx: std::sync::mpsc::SyncSender<libc::pthread_t>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // SAFETY: Queries current thread's pthread_t handle.
        let _ = tid_tx.send(unsafe { libc::pthread_self() });
        let event_size = std::mem::size_of::<usb_functionfs_event>();
        let mut event_buf = vec![0u8; event_size * 4];

        while !runtime.stop.load(Ordering::SeqCst) {
            let mut pfd = libc::pollfd { fd: ep0_fd, events: libc::POLLIN, revents: 0 };
            // SAFETY: Polls valid ep0_fd for FunctionFS setup/enable events.
            let prc = unsafe { libc::poll(&mut pfd, 1, 50) };
            if runtime.stop.load(Ordering::SeqCst) {
                break;
            }
            if prc <= 0 {
                continue;
            }

            // SAFETY: Reads usb_functionfs_event structs from ep0_fd into event_buf.
            let n = unsafe {
                libc::read(ep0_fd, event_buf.as_mut_ptr() as *mut libc::c_void, event_buf.len())
            };
            if n < event_size as isize {
                let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EBADF || runtime.stop.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }

            let count = (n as usize) / event_size;
            for i in 0..count {
                let slice = &event_buf[i * event_size..(i + 1) * event_size];
                let Ok(ev) = usb_functionfs_event::read_from_bytes(slice) else {
                    continue;
                };
                match ev.type_ as u32 {
                    t if t == usb_functionfs_event_type_FUNCTIONFS_ENABLE => {
                        runtime.enabled.store(true, Ordering::SeqCst);
                    }
                    t if t == usb_functionfs_event_type_FUNCTIONFS_DISABLE => {
                        runtime.enabled.store(false, Ordering::SeqCst);
                    }
                    t if t == usb_functionfs_event_type_FUNCTIONFS_SETUP => {
                        // SAFETY: Event type is FUNCTIONFS_SETUP, so union field `setup` is valid.
                        let setup = unsafe { ev.u.setup };
                        let is_in = (setup.bRequestType & 0x80) != 0;
                        let is_vendor = (setup.bRequestType & 0x60) == 0x40;
                        let w_val = u16::from_le(setup.wValue);
                        let w_idx = u16::from_le(setup.wIndex);
                        let w_len_u16 = u16::from_le(setup.wLength);
                        let w_len = usize::from(w_len_u16);
                        let b_req = setup.bRequest;

                        let scripted = runtime
                            .scripted_controls
                            .lock()
                            .unwrap()
                            .iter()
                            .rev()
                            .find(|r| r.matches(setup.bRequestType, b_req, w_val, w_idx, w_len_u16))
                            .map(|r| r.action.clone());
                        let resolved_action = match scripted {
                            Some(ControlScriptAction::Hold(hold_id)) => {
                                let (lock, cv) = &runtime.held_controls;
                                let mut held = lock.lock().unwrap();
                                let mut final_act = None;
                                while !runtime.stop.load(Ordering::SeqCst) {
                                    if let Some(released) = held.released_actions.remove(&hold_id) {
                                        final_act = Some(released);
                                        break;
                                    }
                                    let (next_held, _) =
                                        cv.wait_timeout(held, Duration::from_millis(50)).unwrap();
                                    held = next_held;
                                }
                                drop(held);
                                final_act
                            }
                            other => other,
                        };

                        match resolved_action {
                            Some(ControlScriptAction::Stall) => {
                                stall_ep0(ep0_fd, is_in);
                            }
                            Some(ControlScriptAction::Reply(mut reply_bytes)) => {
                                if is_in {
                                    if reply_bytes.len() > w_len {
                                        reply_bytes.truncate(w_len);
                                    }
                                    // SAFETY: Writes control IN data stage response to ep0_fd.
                                    unsafe {
                                        libc::write(
                                            ep0_fd,
                                            reply_bytes.as_ptr() as *const libc::c_void,
                                            reply_bytes.len(),
                                        );
                                    }
                                } else {
                                    let mut out_buf = vec![0u8; w_len];
                                    // SAFETY: Reads control OUT data stage payload from ep0_fd.
                                    let r = unsafe {
                                        libc::read(
                                            ep0_fd,
                                            out_buf.as_mut_ptr() as *mut libc::c_void,
                                            w_len,
                                        )
                                    };
                                    if r >= 0 && is_vendor && b_req == 0x01 {
                                        out_buf.truncate(r as usize);
                                        *runtime.last_ctrl_out.lock().unwrap() = out_buf;
                                    }
                                }
                            }
                            Some(ControlScriptAction::Hold(_)) => {}
                            None => {
                                if is_vendor && b_req == 0xff {
                                    stall_ep0(ep0_fd, is_in);
                                    continue;
                                }
                                if is_in {
                                    let mut resp = if is_vendor && b_req == 0x01 {
                                        let stored = runtime.last_ctrl_out.lock().unwrap().clone();
                                        if stored.is_empty() {
                                            vec![0x11, 0x22, 0x33, 0x44]
                                        } else {
                                            stored
                                        }
                                    } else {
                                        vec![0x42; w_len]
                                    };
                                    resp.resize(w_len, 0);
                                    // SAFETY: Writes default control IN response to ep0_fd.
                                    unsafe {
                                        libc::write(
                                            ep0_fd,
                                            resp.as_ptr() as *const libc::c_void,
                                            resp.len(),
                                        );
                                    }
                                } else {
                                    let mut out_buf = vec![0u8; w_len];
                                    // SAFETY: Acknowledges control OUT request on ep0_fd.
                                    let r = unsafe {
                                        libc::read(
                                            ep0_fd,
                                            out_buf.as_mut_ptr() as *mut libc::c_void,
                                            w_len,
                                        )
                                    };
                                    if r >= 0 && is_vendor && b_req == 0x01 {
                                        out_buf.truncate(r as usize);
                                        *runtime.last_ctrl_out.lock().unwrap() = out_buf;
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    })
}

fn spawn_in_ep_worker(
    ep_fd: RawFd,
    in_q: Arc<InEndpointQueue>,
    runtime: Arc<SharedGadgetRuntime>,
    tid_tx: std::sync::mpsc::SyncSender<libc::pthread_t>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // SAFETY: Queries current thread's pthread_t handle.
        let _ = tid_tx.send(unsafe { libc::pthread_self() });
        while !runtime.stop.load(Ordering::SeqCst) {
            let payload = {
                let mut guard = in_q.packets.lock().unwrap();
                while guard.is_empty() && !runtime.stop.load(Ordering::SeqCst) {
                    let (next_guard, _) =
                        in_q.cv.wait_timeout(guard, Duration::from_millis(50)).unwrap();
                    guard = next_guard;
                }
                if runtime.stop.load(Ordering::SeqCst) {
                    break;
                }
                guard.pop_front()
            };
            let Some(data) = payload else {
                continue;
            };
            while !runtime.stop.load(Ordering::SeqCst) {
                if !runtime.enabled.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                runtime.in_flight_writes.fetch_add(1, Ordering::SeqCst);
                // SAFETY: Writes queued IN packet (including 0-byte ZLP) to FunctionFS IN endpoint fd.
                let n =
                    unsafe { libc::write(ep_fd, data.as_ptr() as *const libc::c_void, data.len()) };
                runtime.in_flight_writes.fetch_sub(1, Ordering::SeqCst);
                if n >= 0 {
                    break;
                }
                let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EBADF || runtime.stop.load(Ordering::SeqCst) {
                    return;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    })
}

fn spawn_out_ep_worker(
    ep_fd: RawFd,
    max_packet_size: u16,
    out_q: Arc<OutEndpointQueue>,
    echo_in_q: Option<Arc<InEndpointQueue>>,
    runtime: Arc<SharedGadgetRuntime>,
    tid_tx: std::sync::mpsc::SyncSender<libc::pthread_t>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        // SAFETY: Calling `pthread_self` and `SYS_gettid` on the current thread has no
        // preconditions and is always sound.
        let (pthread, worker_tid) =
            unsafe { (libc::pthread_self(), libc::syscall(libc::SYS_gettid) as i32) };
        let _ = tid_tx.send(pthread);
        out_q.worker_tid.store(worker_tid, Ordering::SeqCst);
        // Read one USB packet (`wMaxPacketSize`) per FunctionFS `read()` call so each max-packet
        // transfer and any trailing 0-byte ZLP (`USBDEVFS_URB_ZERO_PACKET`) completes a distinct
        // `usb_request` on `dummy_hcd`.
        let mut buf = vec![0u8; (max_packet_size as usize).max(64)];
        while !runtime.stop.load(Ordering::SeqCst) {
            if !runtime.enabled.load(Ordering::SeqCst) {
                out_q.in_read_syscall.store(false, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            out_q.in_read_syscall.store(true, Ordering::SeqCst);
            // SAFETY: Reads one OUT packet from FunctionFS OUT endpoint fd.
            let n = unsafe { libc::read(ep_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            out_q.in_read_syscall.store(false, Ordering::SeqCst);
            if runtime.stop.load(Ordering::SeqCst) {
                break;
            }
            if n >= 0 {
                // Preserve n == 0 as an explicit 0-byte ZLP packet (`Vec::new()`).
                let pkt = if n == 0 { Vec::new() } else { buf[..n as usize].to_vec() };
                if runtime.auto_echo.load(Ordering::SeqCst) {
                    if !pkt.is_empty()
                        && let Some(ref in_q) = echo_in_q
                    {
                        in_q.packets.lock().unwrap().push_back(pkt);
                        in_q.cv.notify_one();
                    }
                } else {
                    out_q.packets.lock().unwrap().push_back(pkt);
                    out_q.cv.notify_one();
                }
            } else {
                let err = io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EBADF || runtime.stop.load(Ordering::SeqCst) {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    })
}

/// Configures `/sys/kernel/config/usb_gadget/g1` and `/dev/ffs` for `device`.
pub fn configure_gadget(device: DeviceSpec, auto_echo: bool) -> Result<(), String> {
    device.validate_for_ffs()?;
    ensure_sigusr1_handler();
    let mut guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(ref gadget) = *guard {
        if gadget.spec == device
            && gadget.connected
            && gadget.runtime.enabled.load(Ordering::SeqCst)
            && gadget.runtime.in_flight_writes.load(Ordering::SeqCst) == 0
            && let Some((sysfs_dir, busnum, devnum)) =
                find_enumerated_usb_device(device.vendor_id, device.product_id)
        {
            let dev_node = PathBuf::from(format!("/dev/bus/usb/{busnum:03}/{devnum:03}"));
            let cfg_val =
                fs::read_to_string(sysfs_dir.join("bConfigurationValue")).unwrap_or_default();
            if dev_node.exists() && cfg_val.trim() == "1" {
                gadget.runtime.auto_echo.store(auto_echo, Ordering::SeqCst);
                gadget.runtime.scripted_controls.lock().unwrap().clear();
                gadget.runtime.held_controls.0.lock().unwrap().released_actions.clear();
                gadget.runtime.last_ctrl_out.lock().unwrap().clear();
                for q in gadget.runtime.in_queues.values() {
                    q.packets.lock().unwrap().clear();
                }
                for q in gadget.runtime.out_queues.values() {
                    q.packets.lock().unwrap().clear();
                }
                return Ok(());
            }
        }
    }
    teardown_gadget(&mut guard);

    let config_root = Path::new("/sys/kernel/config");
    let gadget_root = config_root.join("usb_gadget");
    if !gadget_root.exists() {
        let _ = fs::create_dir_all(config_root);
        let src = CString::new("none").unwrap();
        let target = CString::new("/sys/kernel/config").unwrap();
        let fstype = CString::new("configfs").unwrap();
        // SAFETY: Mounts configfs at /sys/kernel/config if not yet mounted.
        unsafe {
            libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null());
        }
    }

    let g1 = Path::new(GADGET_DIR);
    fs::create_dir_all(g1).map_err(|e| format!("create {GADGET_DIR}: {e}"))?;
    fs::write(g1.join("idVendor"), format!("0x{:04x}\n", device.vendor_id))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("idProduct"), format!("0x{:04x}\n", device.product_id))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bcdUSB"), format!("0x{:04x}\n", device.bcd_usb))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bcdDevice"), format!("0x{:04x}\n", device.bcd_device))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bDeviceClass"), format!("0x{:02x}\n", device.device_class))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bDeviceSubClass"), format!("0x{:02x}\n", device.device_subclass))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bDeviceProtocol"), format!("0x{:02x}\n", device.device_protocol))
        .map_err(|e| e.to_string())?;
    fs::write(g1.join("bMaxPacketSize0"), format!("{}\n", device.max_packet_size0))
        .map_err(|e| e.to_string())?;

    let max_speed_str = match device.speed.to_ascii_lowercase().as_str() {
        "low" => "low-speed\n",
        "full" => "full-speed\n",
        "super" => "super-speed\n",
        "super-plus" | "super_plus" => "super-speed-plus\n",
        _ => "high-speed\n",
    };
    let _ = fs::write(g1.join("max_speed"), max_speed_str);

    let str_dir = g1.join("strings/0x409");
    fs::create_dir_all(&str_dir).map_err(|e| e.to_string())?;
    fs::write(str_dir.join("manufacturer"), "Test Mfg\n").map_err(|e| e.to_string())?;
    fs::write(str_dir.join("product"), "Test Product\n").map_err(|e| e.to_string())?;
    fs::write(str_dir.join("serialnumber"), "123456\n").map_err(|e| e.to_string())?;

    let func_dir = g1.join("functions/ffs.usb");
    fs::create_dir_all(&func_dir).map_err(|e| e.to_string())?;

    for (idx, cfg) in device.configs.iter().enumerate() {
        let cfg_dir = g1.join(format!("configs/c.{}", cfg.config_value));
        fs::create_dir_all(&cfg_dir).map_err(|e| e.to_string())?;
        if cfg.bm_attributes != 0 {
            let _ =
                fs::write(cfg_dir.join("bmAttributes"), format!("0x{:02x}\n", cfg.bm_attributes));
        }
        fs::write(cfg_dir.join("MaxPower"), format!("{}\n", cfg.max_power_ma))
            .map_err(|e| e.to_string())?;
        if idx == 0 {
            symlink(&func_dir, cfg_dir.join("f1")).map_err(|e| e.to_string())?;
        }
    }

    fs::create_dir_all(FFS_MOUNT_DIR).map_err(|e| e.to_string())?;
    let src = CString::new("usb").unwrap();
    let target = CString::new(FFS_MOUNT_DIR).unwrap();
    let fstype = CString::new("functionfs").unwrap();
    // SAFETY: Mounts functionfs instance "usb" at /dev/ffs.
    let rc =
        unsafe { libc::mount(src.as_ptr(), target.as_ptr(), fstype.as_ptr(), 0, std::ptr::null()) };
    if rc != 0 {
        teardown_gadget(&mut guard);
        return Err(format!("mount functionfs failed: {}", io::Error::last_os_error()));
    }

    let ep0_c = CString::new("/dev/ffs/ep0").unwrap();
    // SAFETY: Opens /dev/ffs/ep0 to write FunctionFS v2 descriptors and strings.
    let ep0_fd = unsafe { libc::open(ep0_c.as_ptr(), libc::O_RDWR) };
    if ep0_fd < 0 {
        teardown_gadget(&mut guard);
        return Err(format!("open /dev/ffs/ep0 failed: {}", io::Error::last_os_error()));
    }

    let (descs, unique_eps) = device.build_ffs_descriptors_v2_and_eps();
    // SAFETY: Writes FunctionFS v2 descriptor header and payload to ep0_fd.
    let nw = unsafe { libc::write(ep0_fd, descs.as_ptr() as *const libc::c_void, descs.len()) };
    if nw != descs.len() as isize {
        let err = io::Error::last_os_error();
        // SAFETY: Closes ep0_fd on write failure.
        unsafe { libc::close(ep0_fd) };
        teardown_gadget(&mut guard);
        return Err(format!("write ffs descriptors failed: {err}"));
    }

    let strings = DeviceSpec::build_ffs_strings_0x0409("Test Interface");
    // SAFETY: Writes FunctionFS strings table to ep0_fd.
    let nw = unsafe { libc::write(ep0_fd, strings.as_ptr() as *const libc::c_void, strings.len()) };
    if nw != strings.len() as isize {
        let err = io::Error::last_os_error();
        // SAFETY: Closes ep0_fd on write failure.
        unsafe { libc::close(ep0_fd) };
        teardown_gadget(&mut guard);
        return Err(format!("write ffs strings failed: {err}"));
    }

    let mut ep_fds = vec![ep0_fd];
    let mut in_queues = HashMap::new();
    let mut out_queues = HashMap::new();
    let mut opened_eps: Vec<(u8, RawFd, bool, u16)> = Vec::new();

    for (idx, ep_spec) in unique_eps.iter().enumerate() {
        let ep_path = CString::new(format!("/dev/ffs/ep{}", idx + 1)).unwrap();
        // SAFETY: Opens FunctionFS endpoint file /dev/ffs/epN after writing descriptors and strings.
        let fd = unsafe { libc::open(ep_path.as_ptr(), libc::O_RDWR) };
        if fd < 0 {
            let err = io::Error::last_os_error();
            for &open_fd in &ep_fds {
                // SAFETY: Closes previously opened endpoint fds on error.
                unsafe { libc::close(open_fd) };
            }
            teardown_gadget(&mut guard);
            return Err(format!("open /dev/ffs/ep{} failed: {err}", idx + 1));
        }
        ep_fds.push(fd);
        let is_in = (ep_spec.address & 0x80) != 0;
        if is_in {
            in_queues.insert(ep_spec.address, Arc::new(InEndpointQueue::new()));
        } else {
            out_queues.insert(ep_spec.address, Arc::new(OutEndpointQueue::new()));
        }
        opened_eps.push((ep_spec.address, fd, is_in, ep_spec.max_packet_size));
    }

    let first_in_q = in_queues.get(&0x81).cloned().or_else(|| in_queues.values().next().cloned());

    let runtime = Arc::new(SharedGadgetRuntime {
        stop: AtomicBool::new(false),
        enabled: AtomicBool::new(false),
        auto_echo: AtomicBool::new(auto_echo),
        in_flight_writes: AtomicUsize::new(0),
        scripted_controls: Mutex::new(Vec::new()),
        held_controls: (
            Mutex::new(HeldControlState { released_actions: HashMap::new() }),
            Condvar::new(),
        ),
        last_ctrl_out: Mutex::new(Vec::new()),
        in_queues,
        out_queues,
    });

    let mut workers = Vec::new();

    let (tid_tx, tid_rx) = std::sync::mpsc::sync_channel(1);
    let ep0_handle = spawn_ep0_worker(ep0_fd, Arc::clone(&runtime), tid_tx);
    workers.push((tid_rx.recv().ok(), ep0_handle));

    for (addr, fd, is_in, mps) in opened_eps {
        let (tid_tx, tid_rx) = std::sync::mpsc::sync_channel(1);
        let handle = if is_in {
            let in_q = Arc::clone(runtime.in_queues.get(&addr).unwrap());
            spawn_in_ep_worker(fd, in_q, Arc::clone(&runtime), tid_tx)
        } else {
            let out_q = Arc::clone(runtime.out_queues.get(&addr).unwrap());
            spawn_out_ep_worker(fd, mps, out_q, first_in_q.clone(), Arc::clone(&runtime), tid_tx)
        };
        workers.push((tid_rx.recv().ok(), handle));
    }

    let vendor_id = device.vendor_id;
    let product_id = device.product_id;

    *guard = Some(ActiveGadget {
        spec: device,
        udc_name: String::new(),
        connected: false,
        ep_fds,
        workers,
        runtime,
    });

    let udc_name = match find_udc_name() {
        Ok(name) => name,
        Err(e) => {
            teardown_gadget(&mut guard);
            return Err(e.to_string());
        }
    };
    if let Err(e) = fs::write(g1.join("UDC"), format!("{udc_name}\n")) {
        teardown_gadget(&mut guard);
        return Err(format!("bind UDC {udc_name}: {e}"));
    }
    if let Some(ref mut gadget) = *guard {
        gadget.udc_name = udc_name;
        gadget.connected = true;
    }

    if let Err(e) = wait_for_enumeration(vendor_id, product_id) {
        teardown_gadget(&mut guard);
        return Err(e);
    }

    Ok(())
}

/// Resets the `dummy_hcd.0` platform device on Linux so the next enumerated gadget receives
/// `devnum = 2` (`/dev/bus/usb/001/002`), then configures the default `0x1234:0x5678` gadget
/// with `auto_echo = true` for `usb_test.rs` compatibility.
pub fn restore_default_usb_test_gadget() -> Result<(), String> {
    {
        let mut guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
        teardown_gadget(&mut guard);
    }
    let hcd_driver = Path::new("/sys/bus/platform/drivers/dummy_hcd");
    if hcd_driver.exists() {
        let _ = fs::write(hcd_driver.join("unbind"), "dummy_hcd.0\n");
        let _ = fs::write(hcd_driver.join("bind"), "dummy_hcd.0\n");
    }
    configure_gadget(default_usb_test_device_spec(), true)
}

/// Dispatches a single [`DeviceCommand`] against the active Linux gadget.
pub fn handle_command(cmd: DeviceCommand) -> DeviceResponse {
    match cmd {
        DeviceCommand::ConfigureDevice { device } => match configure_gadget(device, false) {
            Ok(()) => DeviceResponse { ok: true, error: None, data: None },
            Err(e) => DeviceResponse { ok: false, error: Some(e), data: None },
        },
        DeviceCommand::ScriptControl {
            bm_request_type,
            b_request,
            w_value,
            w_index,
            w_length,
            action,
        } => {
            let guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref gadget) = *guard else {
                return DeviceResponse {
                    ok: false,
                    error: Some("no active gadget".to_string()),
                    data: None,
                };
            };
            if let Some(auto_echo) = action.get("auto_echo").and_then(Value::as_bool) {
                gadget.runtime.auto_echo.store(auto_echo, Ordering::SeqCst);
                if auto_echo {
                    gadget.runtime.scripted_controls.lock().unwrap().clear();
                    gadget.runtime.held_controls.0.lock().unwrap().released_actions.clear();
                    gadget.runtime.last_ctrl_out.lock().unwrap().clear();
                    for q in gadget.runtime.in_queues.values() {
                        q.packets.lock().unwrap().clear();
                    }
                    for q in gadget.runtime.out_queues.values() {
                        q.packets.lock().unwrap().clear();
                    }
                }
                return DeviceResponse { ok: true, error: None, data: None };
            }
            let Some(parsed) = ControlScriptAction::from_value(&action) else {
                return DeviceResponse {
                    ok: false,
                    error: Some(format!("invalid ScriptControl action: {action}")),
                    data: None,
                };
            };
            gadget.runtime.scripted_controls.lock().unwrap().push(ScriptedControlRule {
                bm_request_type,
                b_request,
                w_value,
                w_index,
                w_length,
                action: parsed,
            });
            DeviceResponse { ok: true, error: None, data: None }
        }
        DeviceCommand::ReleaseControl { hold, action } => {
            let guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref gadget) = *guard else {
                return DeviceResponse {
                    ok: false,
                    error: Some("no active gadget".to_string()),
                    data: None,
                };
            };
            let Some(parsed) = ControlScriptAction::from_value(&action) else {
                return DeviceResponse {
                    ok: false,
                    error: Some(format!("invalid ReleaseControl action: {action}")),
                    data: None,
                };
            };
            let (lock, cv) = &gadget.runtime.held_controls;
            lock.lock().unwrap().released_actions.insert(hold, parsed);
            cv.notify_all();
            DeviceResponse { ok: true, error: None, data: None }
        }
        DeviceCommand::PushIn { ep, data } => {
            let guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref gadget) = *guard else {
                return DeviceResponse {
                    ok: false,
                    error: Some("no active gadget".to_string()),
                    data: None,
                };
            };
            let Some(in_q) = gadget.runtime.in_queues.get(&ep) else {
                return DeviceResponse {
                    ok: false,
                    error: Some(format!("unknown IN endpoint {ep:#x}")),
                    data: None,
                };
            };
            in_q.packets.lock().unwrap().push_back(data);
            in_q.cv.notify_one();
            DeviceResponse { ok: true, error: None, data: None }
        }
        DeviceCommand::PopOut { ep, timeout_ms } => {
            let out_q = {
                let guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
                let Some(ref gadget) = *guard else {
                    return DeviceResponse {
                        ok: false,
                        error: Some("no active gadget".to_string()),
                        data: None,
                    };
                };
                let Some(q) = gadget.runtime.out_queues.get(&ep) else {
                    return DeviceResponse {
                        ok: false,
                        error: Some(format!("unknown OUT endpoint {ep:#x}")),
                        data: None,
                    };
                };
                Arc::clone(q)
            };
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            let mut pkts = out_q.packets.lock().unwrap();
            loop {
                if let Some(pkt) = pkts.pop_front() {
                    return DeviceResponse { ok: true, error: None, data: Some(pkt) };
                }
                let now = Instant::now();
                if now >= deadline {
                    return DeviceResponse {
                        ok: false,
                        error: Some("ETIMEDOUT".to_string()),
                        data: None,
                    };
                }
                let (next_pkts, _) = out_q.cv.wait_timeout(pkts, deadline - now).unwrap();
                pkts = next_pkts;
            }
        }
        DeviceCommand::AssertEmpty { ep } => {
            let out_q = {
                let guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
                let Some(ref gadget) = *guard else {
                    return DeviceResponse {
                        ok: false,
                        error: Some("no active gadget".to_string()),
                        data: None,
                    };
                };
                let Some(q) = gadget.runtime.out_queues.get(&ep) else {
                    return DeviceResponse {
                        ok: false,
                        error: Some(format!("unknown OUT endpoint {ep:#x}")),
                        data: None,
                    };
                };
                Arc::clone(q)
            };
            // On Linux, a completed host OUT transfer wakes both the host-side `usbfs` waiter and
            // the FunctionFS endpoint worker thread, so `AssertEmpty` can arrive before the worker
            // thread finishes copying the payload to user space and pushing it onto `out_q.packets`.
            // Wait until either a packet is queued or the worker is parked in interruptible sleep
            // waiting for the next `read()`.
            let settle_deadline = Instant::now() + Duration::from_millis(500);
            let mut pkts = out_q.packets.lock().unwrap();
            while pkts.is_empty() && Instant::now() < settle_deadline {
                let tid = out_q.worker_tid.load(Ordering::SeqCst);
                if out_q.in_read_syscall.load(Ordering::SeqCst) && is_tid_sleeping(tid) {
                    break;
                }
                let (next_pkts, _) = out_q.cv.wait_timeout(pkts, Duration::from_millis(2)).unwrap();
                pkts = next_pkts;
            }
            let len = pkts.len();
            drop(pkts);
            if len == 0 {
                DeviceResponse { ok: true, error: None, data: None }
            } else {
                DeviceResponse { ok: false, error: Some("EAGAIN".to_string()), data: None }
            }
        }
        DeviceCommand::Disconnect { .. } => {
            let mut guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref mut gadget) = *guard else {
                return DeviceResponse {
                    ok: false,
                    error: Some("no active gadget".to_string()),
                    data: None,
                };
            };
            if gadget.connected {
                // Trigger host-side sysfs removal before unbinding the UDC so
                // usbcore flushes in-flight URBs with -ESHUTDOWN before the
                // simulated HCD timer sees the deactivated port and returns
                // -EPROTO. Capture the canonical sysfs kobject directory and
                // character device path beforehand because `/sys/bus/usb/devices`
                // unlinks the bus symlink before driver unbind and `usbfs`
                // removal notification complete.
                let mut real_sysfs_dir = None;
                let mut dev_node = None;
                if let Some((sysfs_dir, busnum, devnum)) =
                    find_enumerated_usb_device(gadget.spec.vendor_id, gadget.spec.product_id)
                {
                    real_sysfs_dir = fs::canonicalize(&sysfs_dir).ok();
                    dev_node = Some(PathBuf::from(format!("/dev/bus/usb/{busnum:03}/{devnum:03}")));
                    let _ = fs::write(sysfs_dir.join("remove"), "1\n");
                }
                gadget.runtime.enabled.store(false, Ordering::SeqCst);
                let g1 = Path::new(GADGET_DIR);
                if let Err(e) = fs::write(g1.join("UDC"), "\n") {
                    return DeviceResponse {
                        ok: false,
                        error: Some(format!("unbind UDC failed: {e}")),
                        data: None,
                    };
                }
                if let Err(e) = wait_for_disconnection(
                    gadget.spec.vendor_id,
                    gadget.spec.product_id,
                    real_sysfs_dir.as_deref(),
                    dev_node.as_deref(),
                ) {
                    return DeviceResponse { ok: false, error: Some(e), data: None };
                }
                gadget.connected = false;
            }
            DeviceResponse { ok: true, error: None, data: None }
        }
        DeviceCommand::Reconnect => {
            let mut guard = gadget_state().lock().unwrap_or_else(|e| e.into_inner());
            let Some(ref mut gadget) = *guard else {
                return DeviceResponse {
                    ok: false,
                    error: Some("no active gadget".to_string()),
                    data: None,
                };
            };
            if !gadget.connected {
                let g1 = Path::new(GADGET_DIR);
                if let Err(e) = fs::write(g1.join("UDC"), format!("{}\n", gadget.udc_name)) {
                    return DeviceResponse {
                        ok: false,
                        error: Some(format!("rebind UDC failed: {e}")),
                        data: None,
                    };
                }
                if let Err(e) = wait_for_enumeration(gadget.spec.vendor_id, gadget.spec.product_id)
                {
                    return DeviceResponse { ok: false, error: Some(e), data: None };
                }
                gadget.connected = true;
            }
            DeviceResponse { ok: true, error: None, data: None }
        }
    }
}

fn handle_client(stream: TcpStream) {
    let Ok(writer_stream) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut writer = io::BufWriter::new(writer_stream);
    let mut line = String::new();
    while reader.read_line(&mut line).unwrap_or(0) > 0 {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let resp = match serde_json::from_str::<DeviceCommand>(trimmed) {
                Ok(cmd) => handle_command(cmd),
                Err(e) => DeviceResponse {
                    ok: false,
                    error: Some(format!("invalid DeviceCommand JSON: {e}")),
                    data: None,
                },
            };
            if let Ok(json) = serde_json::to_string(&resp) {
                let _ = writeln!(writer, "{json}");
                let _ = writer.flush();
            }
        }
        line.clear();
    }
}

/// Starts the Linux FunctionFS gadget control server on `listen_addr`.
pub fn serve(listen_addr: &str) -> io::Result<()> {
    ensure_sigusr1_handler();
    let listener = TcpListener::bind(listen_addr)?;
    for incoming in listener.incoming() {
        if let Ok(stream) = incoming {
            std::thread::spawn(move || {
                handle_client(stream);
            });
        }
    }
    Ok(())
}
