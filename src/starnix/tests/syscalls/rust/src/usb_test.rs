// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Cross-kernel (Fuchsia Starnix and Linux Machina) USB (`usbfs`) syscall tests.
//!
//! On Fuchsia Starnix, the test realm runs `fake_usb_device` serving
//! `fuchsia.hardware.usb.UsbService` to the kernel's `usbfs` module.
//! On Linux (Machina VM), `ensure_usb_device()` configures a matching USB gadget
//! via `configfs` + `functionfs` + `dummy_hcd` (`dummy_udc.0`) and forks a
//! background FunctionFS responder daemon so `/dev/bus/usb/001/002` behaves
//! identically across both kernels.

use linux_uapi::{
    FUNCTIONFS_DESCRIPTORS_MAGIC_V2, FUNCTIONFS_STRINGS_MAGIC, USBDEVFS_BULK,
    USBDEVFS_CAP_BULK_CONTINUATION, USBDEVFS_CAP_NO_PACKET_SIZE_LIM,
    USBDEVFS_CAP_REAP_AFTER_DISCONNECT, USBDEVFS_CAP_ZERO_PACKET, USBDEVFS_CLAIMINTERFACE,
    USBDEVFS_CLEAR_HALT, USBDEVFS_CONNECTINFO, USBDEVFS_CONTROL, USBDEVFS_DISCARDURB,
    USBDEVFS_DISCONNECT_CLAIM, USBDEVFS_GET_CAPABILITIES, USBDEVFS_GET_SPEED, USBDEVFS_GETDRIVER,
    USBDEVFS_REAPURB, USBDEVFS_REAPURBNDELAY, USBDEVFS_RELEASEINTERFACE, USBDEVFS_RESET,
    USBDEVFS_SETCONFIGURATION, USBDEVFS_SETINTERFACE, USBDEVFS_SUBMITURB, USBDEVFS_URB_TYPE_BULK,
    USBDEVFS_URB_TYPE_CONTROL, functionfs_flags_FUNCTIONFS_ALL_CTRL_RECIP,
    functionfs_flags_FUNCTIONFS_HAS_FS_DESC, functionfs_flags_FUNCTIONFS_HAS_HS_DESC,
    usb_device_descriptor, usb_device_speed_USB_SPEED_HIGH, usb_functionfs_event,
    usb_functionfs_event_type_FUNCTIONFS_SETUP, usbdevfs_bulktransfer, usbdevfs_connectinfo,
    usbdevfs_ctrltransfer, usbdevfs_disconnect_claim, usbdevfs_getdriver, usbdevfs_setinterface,
    usbdevfs_urb,
};
use serial_test::serial;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{FileExt, FileTypeExt, MetadataExt, symlink};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};
use zerocopy::FromBytes as _;

const USB_DEV_PATH: &str = "/dev/bus/usb/001/002";
const USB_DEVICE_MAJOR: u32 = 189;
const DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

fn build_ffs_v2_descriptors() -> Vec<u8> {
    // 1 interface descriptor (9 bytes) + 2 endpoint descriptors (7 bytes each) = 23 bytes per speed.
    let fs_descs: [u8; 23] = [
        // Interface 0: vendor-specific (0xff), 2 endpoints
        9, 4, 0, 0, 2, 0xff, 0, 0, 1,
        //
        // Endpoint 1 IN (0x81), Bulk, max packet 64
        7, 5, 0x81, 2, 64, 0, 0, //
        // Endpoint 2 OUT (0x02), Bulk, max packet 64
        7, 5, 0x02, 2, 64, 0, 0,
    ];
    let hs_descs: [u8; 23] = [
        // Interface 0: vendor-specific (0xff), 2 endpoints
        9, 4, 0, 0, 2, 0xff, 0, 0, 1,
        //
        // Endpoint 1 IN (0x81), Bulk, max packet 512 (0x0200)
        7, 5, 0x81, 2, 0x00, 0x02, 0,
        //
        // Endpoint 2 OUT (0x02), Bulk, max packet 512 (0x0200)
        7, 5, 0x02, 2, 0x00, 0x02, 0,
    ];

    let total_len = (12 + 8 + fs_descs.len() + hs_descs.len()) as u32;
    let flags = functionfs_flags_FUNCTIONFS_HAS_FS_DESC
        | functionfs_flags_FUNCTIONFS_HAS_HS_DESC
        | functionfs_flags_FUNCTIONFS_ALL_CTRL_RECIP;
    let fs_count: u32 = 3;
    let hs_count: u32 = 3;

    let mut buf = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&FUNCTIONFS_DESCRIPTORS_MAGIC_V2.to_le_bytes());
    buf.extend_from_slice(&total_len.to_le_bytes());
    buf.extend_from_slice(&flags.to_le_bytes());
    buf.extend_from_slice(&fs_count.to_le_bytes());
    buf.extend_from_slice(&hs_count.to_le_bytes());
    buf.extend_from_slice(&fs_descs);
    buf.extend_from_slice(&hs_descs);
    buf
}

fn build_ffs_strings() -> Vec<u8> {
    let s = b"Test Interface\0";
    let total_len = (16 + 2 + s.len()) as u32;
    let str_count: u32 = 1;
    let lang_count: u32 = 1;
    let lang: u16 = 0x0409;

    let mut buf = Vec::with_capacity(total_len as usize);
    buf.extend_from_slice(&FUNCTIONFS_STRINGS_MAGIC.to_le_bytes());
    buf.extend_from_slice(&total_len.to_le_bytes());
    buf.extend_from_slice(&str_count.to_le_bytes());
    buf.extend_from_slice(&lang_count.to_le_bytes());
    buf.extend_from_slice(&lang.to_le_bytes());
    buf.extend_from_slice(s);
    buf
}

fn run_linux_ffs_daemon(ep0_fd: RawFd, ep1_fd: RawFd, ep2_fd: RawFd) -> ! {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // ep1 (Bulk IN, 0x81) writer thread
    std::thread::spawn(move || {
        while let Ok(payload) = rx.recv() {
            loop {
                // SAFETY: Writing payload to FunctionFS ep1 descriptor.
                let n = unsafe {
                    libc::write(ep1_fd, payload.as_ptr() as *const libc::c_void, payload.len())
                };
                if n >= 0 {
                    break;
                }
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EBADF {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });

    // ep2 (Bulk OUT, 0x02) reader thread
    std::thread::spawn(move || {
        // Buffer aligned to max packet size (512) per Linux FunctionFS requirements.
        let mut buf = vec![0u8; 4096];
        loop {
            // SAFETY: Reading payload from FunctionFS ep2 descriptor into allocated buffer.
            let n = unsafe { libc::read(ep2_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n > 0 {
                let _ = tx.send(buf[..n as usize].to_vec());
            } else if n == 0 {
                // In FunctionFS, reading 0 bytes indicates receipt of a USB Zero-Length Packet (ZLP),
                // not EOF. Continue listening for subsequent transfers.
                continue;
            } else {
                let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
                if err == libc::EBADF {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });

    // ep0 control event loop
    let last_ctrl_out = Arc::new(Mutex::new(Vec::<u8>::new()));
    let event_size = std::mem::size_of::<usb_functionfs_event>();
    let mut event_buf = vec![0u8; event_size * 4];
    loop {
        // SAFETY: Reading FunctionFS setup events from ep0 descriptor into allocated buffer.
        let n = unsafe {
            libc::read(ep0_fd, event_buf.as_mut_ptr() as *mut libc::c_void, event_buf.len())
        };
        if n == 0 {
            // SAFETY: Terminating child daemon process immediately without running exit handlers.
            unsafe { libc::_exit(0) };
        }
        if n < event_size as isize {
            let err = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            if err == libc::EBADF {
                // SAFETY: Terminating child daemon process immediately without running exit handlers.
                unsafe { libc::_exit(0) };
            }
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }
        let count = (n as usize) / event_size;
        for i in 0..count {
            let slice = &event_buf[i * event_size..(i + 1) * event_size];
            let Ok(ev) = usb_functionfs_event::read_from_bytes(slice) else {
                continue;
            };
            if ev.type_ as u32 == usb_functionfs_event_type_FUNCTIONFS_SETUP {
                // SAFETY: Under the FunctionFS protocol, when ev.type_ == FUNCTIONFS_SETUP,
                // the union `u` is guaranteed to contain a valid `usb_ctrlrequest` setup packet.
                let setup = unsafe { ev.u.setup };
                let is_in = (setup.bRequestType & 0x80) != 0;
                let is_vendor = (setup.bRequestType & 0x60) == 0x40;
                let w_len = u16::from_le(setup.wLength) as usize;

                if is_vendor && setup.bRequest == 0xff {
                    // Simulate endpoint STALL (EPIPE) by performing opposite-direction I/O on ep0.
                    let mut placeholder = [0u8; 1];
                    if is_in {
                        // SAFETY: Opposite-direction read on ep0 triggers STALL in FunctionFS.
                        unsafe {
                            libc::read(ep0_fd, placeholder.as_mut_ptr() as *mut libc::c_void, 1);
                        }
                    } else {
                        // SAFETY: Opposite-direction write on ep0 triggers STALL in FunctionFS.
                        unsafe {
                            libc::write(ep0_fd, placeholder.as_ptr() as *const libc::c_void, 1);
                        }
                    }
                    continue;
                }

                if is_in {
                    let mut resp = if is_vendor && setup.bRequest == 0x01 {
                        let stored = last_ctrl_out.lock().unwrap().clone();
                        if stored.is_empty() { vec![0x11, 0x22, 0x33, 0x44] } else { stored }
                    } else {
                        vec![0x42; w_len]
                    };
                    resp.truncate(w_len);
                    // SAFETY: Writing control IN response data to ep0 descriptor.
                    unsafe {
                        libc::write(ep0_fd, resp.as_ptr() as *const libc::c_void, resp.len());
                    }
                } else {
                    let mut out_buf = vec![0u8; w_len];
                    // SAFETY: Reading control OUT data from ep0 descriptor into allocated buffer.
                    let r = unsafe {
                        libc::read(ep0_fd, out_buf.as_mut_ptr() as *mut libc::c_void, w_len)
                    };
                    if r >= 0 && is_vendor && setup.bRequest == 0x01 {
                        out_buf.truncate(r as usize);
                        *last_ctrl_out.lock().unwrap() = out_buf;
                    }
                }
            }
        }
    }
}

fn setup_linux_dummy_hcd_gadget() {
    let config_root = Path::new("/sys/kernel/config");
    let gadget_root = config_root.join("usb_gadget");
    if !gadget_root.exists() {
        let _ = fs::create_dir_all(config_root);
        // SAFETY: Mounting configfs for USB gadget configuration.
        let rc = unsafe {
            libc::mount(
                c"none".as_ptr(),
                c"/sys/kernel/config".as_ptr(),
                c"configfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        assert!(
            rc == 0 || last_errno() == libc::EBUSY,
            "mount configfs failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let g1 = gadget_root.join("g1");
    let udc_bound = !fs::read_to_string(g1.join("UDC")).unwrap_or_default().trim().is_empty();
    if !udc_bound {
        fs::create_dir_all(&g1).expect("failed to create /sys/kernel/config/usb_gadget/g1");
        fs::write(g1.join("idVendor"), "0x1234\n").unwrap();
        fs::write(g1.join("idProduct"), "0x5678\n").unwrap();
        fs::write(g1.join("bcdUSB"), "0x0200\n").unwrap();
        fs::write(g1.join("bcdDevice"), "0x0543\n").unwrap();
        fs::write(g1.join("bDeviceClass"), "0x00\n").unwrap();
        fs::write(g1.join("bDeviceSubClass"), "0x00\n").unwrap();
        fs::write(g1.join("bDeviceProtocol"), "0x00\n").unwrap();
        fs::write(g1.join("bMaxPacketSize0"), "64\n").unwrap();

        let str_dir = g1.join("strings/0x409");
        fs::create_dir_all(&str_dir).unwrap();
        fs::write(str_dir.join("manufacturer"), "Test Mfg\n").unwrap();
        fs::write(str_dir.join("product"), "Test Product\n").unwrap();
        fs::write(str_dir.join("serialnumber"), "123456\n").unwrap();

        let cfg_dir = g1.join("configs/c.1");
        let cfg_str_dir = cfg_dir.join("strings/0x409");
        fs::create_dir_all(&cfg_str_dir).unwrap();
        fs::write(cfg_str_dir.join("configuration"), "Config 1\n").unwrap();
        fs::write(cfg_dir.join("MaxPower"), "100\n").unwrap();

        let func_dir = g1.join("functions/ffs.usb");
        fs::create_dir_all(&func_dir).unwrap();
        let _ = symlink(&func_dir, cfg_dir.join("f1"));

        fs::create_dir_all("/dev/ffs").unwrap();
        // SAFETY: Mounting functionfs for FunctionFS endpoint handling.
        let rc = unsafe {
            libc::mount(
                c"usb".as_ptr(),
                c"/dev/ffs".as_ptr(),
                c"functionfs".as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        assert!(
            rc == 0 || last_errno() == libc::EBUSY,
            "mount functionfs failed: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: Opening FunctionFS ep0.
        let ep0_fd = unsafe { libc::open(c"/dev/ffs/ep0".as_ptr(), libc::O_RDWR) };
        assert!(ep0_fd >= 0, "open /dev/ffs/ep0 failed: {}", std::io::Error::last_os_error());

        let descs = build_ffs_v2_descriptors();
        // SAFETY: Writing FunctionFS v2 descriptors to ep0.
        let nw = unsafe { libc::write(ep0_fd, descs.as_ptr() as *const libc::c_void, descs.len()) };
        assert_eq!(
            nw,
            descs.len() as isize,
            "write ffs descriptors failed: {}",
            std::io::Error::last_os_error()
        );

        let strings = build_ffs_strings();
        // SAFETY: Writing FunctionFS strings to ep0.
        let nw =
            unsafe { libc::write(ep0_fd, strings.as_ptr() as *const libc::c_void, strings.len()) };
        assert_eq!(
            nw,
            strings.len() as isize,
            "write ffs strings failed: {}",
            std::io::Error::last_os_error()
        );

        // SAFETY: Opening FunctionFS ep1.
        let ep1_fd = unsafe { libc::open(c"/dev/ffs/ep1".as_ptr(), libc::O_RDWR) };
        assert!(ep1_fd >= 0, "open /dev/ffs/ep1 failed: {}", std::io::Error::last_os_error());

        // SAFETY: Opening FunctionFS ep2.
        let ep2_fd = unsafe { libc::open(c"/dev/ffs/ep2".as_ptr(), libc::O_RDWR) };
        assert!(ep2_fd >= 0, "open /dev/ffs/ep2 failed: {}", std::io::Error::last_os_error());

        // Replace the on-disk test binary path with a fresh inode copy in the same directory
        // before forking so that the long-lived daemon child holds an unlinked inode and never
        // causes ETXTBSY if starnix_test_runner's PutFile overwrites the binary path in a
        // subsequent test run.
        if let Ok(exe_path) = fs::read_link("/proc/self/exe") {
            let tmp_copy = exe_path.with_extension("copy");
            if fs::copy(&exe_path, &tmp_copy).is_ok() {
                let _ = fs::rename(&tmp_copy, &exe_path);
            }
        }

        // Fork a background daemon process so the FunctionFS endpoints remain open across
        // individual libtest test-case invocations in the Machina guest VM.
        // SAFETY: Forking the background FunctionFS daemon. In this test context,
        // starnix_test_runner invokes each test case in a dedicated process, and glibc
        // fork handlers ensure clean allocator and threading state.
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            // SAFETY: Detaching the child daemon from the controlling terminal and redirecting standard I/O to /dev/null.
            unsafe {
                libc::setsid();
                let null_fd = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
                if null_fd >= 0 {
                    libc::dup2(null_fd, 0);
                    libc::dup2(null_fd, 1);
                    libc::dup2(null_fd, 2);
                    if null_fd > 2 {
                        libc::close(null_fd);
                    }
                }
            }
            run_linux_ffs_daemon(ep0_fd, ep1_fd, ep2_fd);
        }

        // SAFETY: Closing parent-side endpoint fds after child daemon inherits them.
        unsafe {
            libc::close(ep0_fd);
            libc::close(ep1_fd);
            libc::close(ep2_fd);
        }

        let udc_name = fs::read_dir("/sys/class/udc")
            .expect("failed to read /sys/class/udc")
            .next()
            .expect("no UDC found in /sys/class/udc")
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();
        fs::write(g1.join("UDC"), format!("{udc_name}\n")).expect("failed to bind UDC");
    }
}

fn try_find_sysfs_usb_device() -> Option<PathBuf> {
    let entries = fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        let vid = fs::read_to_string(path.join("idVendor")).unwrap_or_default();
        let pid = fs::read_to_string(path.join("idProduct")).unwrap_or_default();
        if vid.trim() == "1234" && pid.trim() == "5678" {
            return Some(path);
        }
    }
    None
}

fn ensure_usb_device() -> &'static Path {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let dev_path = Path::new(USB_DEV_PATH);
        if !dev_path.exists() && Path::new("/sys/class/udc").exists() {
            setup_linux_dummy_hcd_gadget();
        }
        let deadline = Instant::now() + DEVICE_TIMEOUT;
        loop {
            let configured = dev_path.exists()
                && try_find_sysfs_usb_device().is_some_and(|sysfs_dir| {
                    fs::read_to_string(sysfs_dir.join("bConfigurationValue"))
                        .map(|s| s.trim() == "1")
                        .unwrap_or(false)
                });
            if configured {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{USB_DEV_PATH} was not configured within {DEVICE_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    Path::new(USB_DEV_PATH)
}

fn open_usb_rdwr() -> OwnedFd {
    let path = ensure_usb_device();
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    // SAFETY: Opening the character device node with O_RDWR.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDWR) };
    assert!(fd >= 0, "failed to open {}: {}", path.display(), std::io::Error::last_os_error());
    // SAFETY: `fd` is a valid open file descriptor and ownership is transferred to OwnedFd.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn find_sysfs_usb_device() -> PathBuf {
    let _ = ensure_usb_device();
    let deadline = Instant::now() + DEVICE_TIMEOUT;
    loop {
        if let Some(path) = try_find_sysfs_usb_device() {
            return path;
        }
        assert!(
            Instant::now() < deadline,
            "USB device 1234:5678 not found under /sys/bus/usb/devices"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

/// Helper performing a raw ioctl and returning `Ok(ret)` on success or `Err(errno)` on failure.
///
/// # Safety
///
/// Callers must ensure that `request` and `arg` satisfy the pointer and memory safety
/// requirements of the specific ioctl.
unsafe fn raw_ioctl(fd: RawFd, request: u32, arg: *mut libc::c_void) -> Result<i32, i32> {
    // SAFETY: The caller guarantees that `arg` conforms to the contract of `request`.
    let rc = unsafe { libc::ioctl(fd, request as libc::Ioctl, arg) };
    if rc >= 0 { Ok(rc) } else { Err(last_errno()) }
}

/// Returns the USB device speed via `USBDEVFS_GET_SPEED`.
fn usb_get_speed(fd: &impl AsRawFd) -> Result<i32, i32> {
    // SAFETY: USBDEVFS_GET_SPEED takes an integer/null argument and returns the speed code or -1.
    // No pointers are dereferenced by the kernel.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_GET_SPEED, std::ptr::null_mut()) }
}

/// Retrieves device capabilities via `USBDEVFS_GET_CAPABILITIES`.
fn usb_get_capabilities(fd: &impl AsRawFd, caps: &mut u32) -> Result<(), i32> {
    // SAFETY: `caps` is a valid, aligned, mutable reference to a `u32` for the kernel to write into.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_GET_CAPABILITIES, caps as *mut u32 as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Retrieves connection information via `USBDEVFS_CONNECTINFO`.
fn usb_get_connectinfo(fd: &impl AsRawFd, ci: &mut usbdevfs_connectinfo) -> Result<(), i32> {
    // SAFETY: `ci` is a valid, aligned, mutable reference to `usbdevfs_connectinfo`.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_CONNECTINFO, ci as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Queries the driver bound to an interface via `USBDEVFS_GETDRIVER`.
fn usb_get_driver(fd: &impl AsRawFd, gd: &mut usbdevfs_getdriver) -> Result<(), i32> {
    // SAFETY: `gd` is a valid, aligned, mutable reference to `usbdevfs_getdriver`.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_GETDRIVER, gd as *mut _ as *mut libc::c_void).map(|_| ())
    }
}

/// Claims the specified interface via `USBDEVFS_CLAIMINTERFACE`.
fn usb_claim_interface(fd: &impl AsRawFd, interface: u32) -> Result<(), i32> {
    let mut ifno = interface;
    // SAFETY: `ifno` is a valid stack-allocated `u32` read synchronously by the kernel.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &mut ifno as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Releases the specified interface via `USBDEVFS_RELEASEINTERFACE`.
fn usb_release_interface(fd: &impl AsRawFd, interface: u32) -> Result<(), i32> {
    let mut ifno = interface;
    // SAFETY: `ifno` is a valid stack-allocated `u32` read synchronously by the kernel.
    unsafe {
        raw_ioctl(
            fd.as_raw_fd(),
            USBDEVFS_RELEASEINTERFACE,
            &mut ifno as *mut _ as *mut libc::c_void,
        )
        .map(|_| ())
    }
}

/// Disconnects kernel driver and claims interface via `USBDEVFS_DISCONNECT_CLAIM`.
fn usb_disconnect_claim(fd: &impl AsRawFd, dc: &mut usbdevfs_disconnect_claim) -> Result<(), i32> {
    // SAFETY: `dc` is a valid, aligned, mutable reference to `usbdevfs_disconnect_claim`.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_DISCONNECT_CLAIM, dc as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Sets the alternate setting on an interface via `USBDEVFS_SETINTERFACE`.
fn usb_set_interface(fd: &impl AsRawFd, set_iface: &usbdevfs_setinterface) -> Result<(), i32> {
    // SAFETY: `set_iface` is a valid reference to `usbdevfs_setinterface` read synchronously by the kernel.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_SETINTERFACE, set_iface as *const _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Sets the active device configuration via `USBDEVFS_SETCONFIGURATION`.
fn usb_set_configuration(fd: &impl AsRawFd, config: i32) -> Result<(), i32> {
    let mut cfg = config;
    // SAFETY: `cfg` is a valid stack-allocated `i32` read synchronously by the kernel.
    unsafe {
        raw_ioctl(
            fd.as_raw_fd(),
            USBDEVFS_SETCONFIGURATION,
            &mut cfg as *mut _ as *mut libc::c_void,
        )
        .map(|_| ())
    }
}

/// Clears the halt condition on an endpoint via `USBDEVFS_CLEAR_HALT`.
fn usb_clear_halt(fd: &impl AsRawFd, ep: u32) -> Result<(), i32> {
    let mut ep_num = ep;
    // SAFETY: `ep_num` is a valid stack-allocated `u32` read synchronously by the kernel.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_CLEAR_HALT, &mut ep_num as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Resets the USB device via `USBDEVFS_RESET`.
fn usb_reset(fd: &impl AsRawFd) -> Result<(), i32> {
    // SAFETY: USBDEVFS_RESET takes no pointer arguments.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_RESET, std::ptr::null_mut()).map(|_| ()) }
}

/// Executes a synchronous control transfer via `USBDEVFS_CONTROL`.
fn usb_control_sync(fd: &impl AsRawFd, ctrl: &mut usbdevfs_ctrltransfer) -> Result<i32, i32> {
    // SAFETY: `ctrl` is a valid mutable reference to `usbdevfs_ctrltransfer`.
    // The kernel reads/writes `ctrl` synchronously during the ioctl syscall and does not retain the pointer.
    // The memory pointed to by `ctrl.data` is accessed synchronously within the call duration.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_CONTROL, ctrl as *mut _ as *mut libc::c_void) }
}

/// Executes a synchronous bulk transfer via `USBDEVFS_BULK`.
fn usb_bulk_sync(fd: &impl AsRawFd, bulk: &mut usbdevfs_bulktransfer) -> Result<i32, i32> {
    // SAFETY: `bulk` is a valid mutable reference to `usbdevfs_bulktransfer`.
    // The kernel reads/writes `bulk` synchronously during the ioctl syscall and does not retain the pointer.
    // The memory pointed to by `bulk.data` is accessed synchronously within the call duration.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_BULK, bulk as *mut _ as *mut libc::c_void) }
}

/// Submits an asynchronous URB via `USBDEVFS_SUBMITURB`.
///
/// # Safety
///
/// Callers must ensure that `urb` and its underlying buffer remain valid and pinned in memory
/// until the URB is reaped via `usb_reap_urb` or `usb_reap_urb_ndelay`.
unsafe fn usb_submit_urb(fd: &impl AsRawFd, urb: *mut usbdevfs_urb) -> Result<(), i32> {
    // SAFETY: The caller guarantees that `urb` and its data buffer remain valid until reaped.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_SUBMITURB, urb as *mut libc::c_void).map(|_| ()) }
}

/// Reaps a completed asynchronous URB (blocking) via `USBDEVFS_REAPURB`.
///
/// # Safety
///
/// `reaped_ptr` must be a valid, aligned pointer to receive the reaped `*mut usbdevfs_urb`.
unsafe fn usb_reap_urb(fd: &impl AsRawFd, reaped_ptr: &mut *mut usbdevfs_urb) -> Result<(), i32> {
    // SAFETY: `reaped_ptr` is a valid mutable reference where the kernel writes the reaped URB pointer.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_REAPURB, reaped_ptr as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Reaps a completed asynchronous URB (non-blocking) via `USBDEVFS_REAPURBNDELAY`.
///
/// # Safety
///
/// `reaped_ptr` must be a valid, aligned pointer to receive the reaped `*mut usbdevfs_urb`.
unsafe fn usb_reap_urb_ndelay(
    fd: &impl AsRawFd,
    reaped_ptr: &mut *mut usbdevfs_urb,
) -> Result<(), i32> {
    // SAFETY: `reaped_ptr` is a valid mutable reference where the kernel writes the reaped URB pointer.
    unsafe {
        raw_ioctl(fd.as_raw_fd(), USBDEVFS_REAPURBNDELAY, reaped_ptr as *mut _ as *mut libc::c_void)
            .map(|_| ())
    }
}

/// Discards/cancels a previously submitted asynchronous URB via `USBDEVFS_DISCARDURB`.
///
/// # Safety
///
/// `urb` must point to an active in-flight URB previously submitted via `usb_submit_urb`.
unsafe fn usb_discard_urb(fd: &impl AsRawFd, urb: *mut usbdevfs_urb) -> Result<(), i32> {
    // SAFETY: `urb` points to an in-flight URB being discarded.
    unsafe { raw_ioctl(fd.as_raw_fd(), USBDEVFS_DISCARDURB, urb as *mut libc::c_void).map(|_| ()) }
}

#[test]
#[serial]
fn test_usb_devtmpfs_and_sysfs_discovery() {
    let dev_path = ensure_usb_device();
    let meta = fs::metadata(dev_path).expect("failed to stat /dev/bus/usb/001/002");
    assert!(meta.file_type().is_char_device(), "expected character device");
    let rdev = meta.rdev();
    let major = libc::major(rdev);
    let minor = libc::minor(rdev);
    assert_eq!(major, USB_DEVICE_MAJOR);
    assert_eq!(minor, 1);

    let sysfs_dir = find_sysfs_usb_device();
    assert_eq!(fs::read_to_string(sysfs_dir.join("idVendor")).unwrap(), "1234\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("idProduct")).unwrap(), "5678\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bcdDevice")).unwrap(), "0543\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bDeviceClass")).unwrap(), "00\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bDeviceSubClass")).unwrap(), "00\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bDeviceProtocol")).unwrap(), "00\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bNumConfigurations")).unwrap(), "1\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("bConfigurationValue")).unwrap(), "1\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("busnum")).unwrap(), "1\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("devnum")).unwrap(), "2\n");
    assert_eq!(fs::read_to_string(sysfs_dir.join("speed")).unwrap(), "480\n");

    let uevent = fs::read_to_string(sysfs_dir.join("uevent")).unwrap();
    assert!(uevent.lines().any(|l| l == "MAJOR=189"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "MINOR=1"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "DEVNAME=bus/usb/001/002"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "DEVTYPE=usb_device"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "PRODUCT=1234/5678/543"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "TYPE=0/0/0"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "BUSNUM=001"), "uevent: {uevent}");
    assert!(uevent.lines().any(|l| l == "DEVNUM=002"), "uevent: {uevent}");
}

#[test]
#[serial]
fn test_usb_read_raw_descriptors() {
    let dev_path = ensure_usb_device();
    let mut file = File::open(dev_path).expect("failed to open usbfs device read-only");
    let mut raw_desc = Vec::new();
    file.read_to_end(&mut raw_desc).expect("failed to read raw descriptors");

    // 18-byte device descriptor + 9-byte config + 9-byte interface + 2 * 7-byte endpoints = 50 bytes.
    assert_eq!(raw_desc.len(), 50);

    let sysfs_dir = find_sysfs_usb_device();
    let sysfs_desc =
        fs::read(sysfs_dir.join("descriptors")).expect("failed to read sysfs descriptors");
    assert_eq!(raw_desc, sysfs_desc);

    let dev_desc = usb_device_descriptor::read_from_bytes(&raw_desc[..18]).unwrap();
    assert_eq!(dev_desc.bLength, 18);
    assert_eq!(dev_desc.bDescriptorType, 1);
    assert_eq!(u16::from_le(dev_desc.bcdUSB), 0x0200);
    assert_eq!(dev_desc.bMaxPacketSize0, 64);
    assert_eq!(u16::from_le(dev_desc.idVendor), 0x1234);
    assert_eq!(u16::from_le(dev_desc.idProduct), 0x5678);
    assert_eq!(u16::from_le(dev_desc.bcdDevice), 0x0543);
    assert_eq!(dev_desc.bNumConfigurations, 1);
    assert_eq!(raw_desc[38], 0x81);
    assert_eq!(raw_desc[45], 0x02);

    // pread at offset 18 reads the 9-byte configuration descriptor
    let mut cfg_hdr = [0u8; 9];
    let n = file.read_at(&mut cfg_hdr, 18).expect("pread config descriptor failed");
    assert_eq!(n, 9);
    assert_eq!(cfg_hdr[0], 9);
    assert_eq!(cfg_hdr[1], 2); // USB_DT_CONFIG
    assert_eq!(u16::from_le_bytes([cfg_hdr[2], cfg_hdr[3]]), 32);
    assert_eq!(cfg_hdr[4], 1); // bNumInterfaces = 1
    assert_eq!(cfg_hdr[5], 1); // bConfigurationValue = 1
}

#[test]
#[serial]
fn test_usb_rdonly_ioctl_rejected_with_eperm() {
    let dev_path = ensure_usb_device();
    let file = OpenOptions::new().read(true).open(dev_path).expect("open O_RDONLY failed");

    assert_eq!(usb_get_speed(&file), Err(libc::EPERM));
    assert_eq!(usb_claim_interface(&file, 0), Err(libc::EPERM));
}

#[test]
#[serial]
fn test_usb_device_info_ioctls() {
    let fd = open_usb_rdwr();
    let raw_fd = fd.as_raw_fd();

    // USBDEVFS_GET_SPEED
    let speed = usb_get_speed(&fd).expect("get_speed failed");
    assert_eq!(speed, usb_device_speed_USB_SPEED_HIGH as i32);

    // USBDEVFS_GET_CAPABILITIES
    let mut caps: u32 = 0xffff_ffff;
    assert_eq!(usb_get_capabilities(&fd, &mut caps), Ok(()));
    assert_ne!(caps, 0xffff_ffff);
    let required_caps = USBDEVFS_CAP_ZERO_PACKET
        | USBDEVFS_CAP_BULK_CONTINUATION
        | USBDEVFS_CAP_NO_PACKET_SIZE_LIM
        | USBDEVFS_CAP_REAP_AFTER_DISCONNECT;
    assert_eq!(caps & required_caps, required_caps, "unexpected capabilities: {caps:#x}");

    // Negative test: null pointer passed to GET_CAPABILITIES returns EFAULT
    // SAFETY: Intentionally passing null pointer to verify kernel returns EFAULT.
    let rc = unsafe { raw_ioctl(raw_fd, USBDEVFS_GET_CAPABILITIES, std::ptr::null_mut()) };
    assert_eq!(rc, Err(libc::EFAULT));

    // USBDEVFS_CONNECTINFO
    let mut ci = usbdevfs_connectinfo::default();
    assert_eq!(usb_get_connectinfo(&fd, &mut ci), Ok(()));
    assert_eq!(ci.devnum, 2);
    assert_eq!(ci.slow, 0);

    // Negative test: null pointer passed to CONNECTINFO returns EFAULT
    // SAFETY: Intentionally passing null pointer to verify kernel returns EFAULT.
    let rc = unsafe { raw_ioctl(raw_fd, USBDEVFS_CONNECTINFO, std::ptr::null_mut()) };
    assert_eq!(rc, Err(libc::EFAULT));

    // USBDEVFS_GETDRIVER on unclaimed vendor-specific interface 0 returns ENODATA
    let mut gd = usbdevfs_getdriver { interface: 0, ..Default::default() };
    assert_eq!(usb_get_driver(&fd, &mut gd), Err(libc::ENODATA));

    // Negative test: null pointer passed to GETDRIVER returns EFAULT
    // SAFETY: Intentionally passing null pointer to verify kernel returns EFAULT.
    let rc = unsafe { raw_ioctl(raw_fd, USBDEVFS_GETDRIVER, std::ptr::null_mut()) };
    assert_eq!(rc, Err(libc::EFAULT));
}

#[test]
#[serial]
fn test_usb_interface_claim_release_and_disconnect_claim() {
    let fd1 = open_usb_rdwr();
    let fd2 = open_usb_rdwr();

    // Non-existent interface within bitmask range (< 64) returns ENOENT on claim/release
    assert_eq!(usb_claim_interface(&fd1, 5), Err(libc::ENOENT));
    assert_eq!(usb_release_interface(&fd1, 5), Err(libc::ENOENT));

    // Out-of-range interface (>= 64) returns EINVAL
    assert_eq!(usb_claim_interface(&fd1, 99), Err(libc::EINVAL));

    // Releasing valid but unclaimed interface 0 returns EINVAL
    assert_eq!(usb_release_interface(&fd1, 0), Err(libc::EINVAL));

    // Claim interface 0 on fd1
    assert_eq!(usb_claim_interface(&fd1, 0), Ok(()));

    // Re-claiming on same fd1 is idempotent (0)
    assert_eq!(usb_claim_interface(&fd1, 0), Ok(()));

    // Claiming on fd2 while claimed by fd1 fails with EBUSY
    assert_eq!(usb_claim_interface(&fd2, 0), Err(libc::EBUSY));

    // Releasing on fd2 fails with EINVAL
    assert_eq!(usb_release_interface(&fd2, 0), Err(libc::EINVAL));

    // Release on fd1 succeeds
    assert_eq!(usb_release_interface(&fd1, 0), Ok(()));

    // USBDEVFS_DISCONNECT_CLAIM with invalid interface returns EINVAL
    let mut dc = usbdevfs_disconnect_claim { interface: 99, ..Default::default() };
    assert_eq!(usb_disconnect_claim(&fd2, &mut dc), Err(libc::EINVAL));

    // USBDEVFS_DISCONNECT_CLAIM on interface 0 succeeds on fd2
    dc.interface = 0;
    assert_eq!(usb_disconnect_claim(&fd2, &mut dc), Ok(()));

    // fd1 cannot claim while fd2 holds it
    assert_eq!(usb_claim_interface(&fd1, 0), Err(libc::EBUSY));

    // Closing fd2 automatically releases its claimed interface
    drop(fd2);

    assert_eq!(usb_claim_interface(&fd1, 0), Ok(()));
}

#[test]
#[serial]
fn test_usb_setinterface_setconfiguration_clearhalt_reset() {
    let fd1 = open_usb_rdwr();
    let fd2 = open_usb_rdwr();

    // Claim interface 0 on fd1
    assert_eq!(usb_claim_interface(&fd1, 0), Ok(()));

    // USBDEVFS_SETINTERFACE on (0, 0) succeeds
    let set_iface = usbdevfs_setinterface { interface: 0, altsetting: 0 };
    assert_eq!(usb_set_interface(&fd1, &set_iface), Ok(()));

    // USBDEVFS_SETINTERFACE with invalid altsetting fails with EINVAL
    let bad_alt = usbdevfs_setinterface { interface: 0, altsetting: 5 };
    assert_eq!(usb_set_interface(&fd1, &bad_alt), Err(libc::EINVAL));

    // USBDEVFS_SETINTERFACE on fd2 while fd1 owns interface 0 fails with EBUSY
    assert_eq!(usb_set_interface(&fd2, &set_iface), Err(libc::EBUSY));

    // USBDEVFS_SETCONFIGURATION while interface 0 is claimed fails with EBUSY
    assert_eq!(usb_set_configuration(&fd1, 1), Err(libc::EBUSY));

    // USBDEVFS_CLEAR_HALT on valid endpoints 0x81 and 0x02 succeeds
    assert_eq!(usb_clear_halt(&fd1, 0x81), Ok(()));
    assert_eq!(usb_clear_halt(&fd1, 0x02), Ok(()));

    // USBDEVFS_CLEAR_HALT on non-existent endpoint 0x89 fails with ENOENT
    assert_eq!(usb_clear_halt(&fd1, 0x89), Err(libc::ENOENT));

    // Release interface 0, then USBDEVFS_SETCONFIGURATION succeeds
    assert_eq!(usb_release_interface(&fd1, 0), Ok(()));
    assert_eq!(usb_set_configuration(&fd1, 1), Ok(()));

    // USBDEVFS_RESET succeeds
    assert_eq!(usb_reset(&fd1), Ok(()));
}

#[test]
#[serial]
fn test_usb_synchronous_control_transfers() {
    let fd = open_usb_rdwr();

    // 1. Vendor Control OUT (0x40, bRequest=0x01) storing 4 bytes
    let mut out_payload = [0xca_u8, 0xfe, 0xba, 0xbe];
    let mut ctrl_out = usbdevfs_ctrltransfer {
        bRequestType: 0x40,
        bRequest: 0x01,
        wValue: 0x1234,
        wIndex: 0,
        wLength: out_payload.len() as u16,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: out_payload.as_mut_ptr() as u64 },
        ..Default::default()
    };
    let n = usb_control_sync(&fd, &mut ctrl_out).expect("USBDEVFS_CONTROL OUT failed");
    assert_eq!(n, 4);

    // 2. Vendor Control IN (0xc0, bRequest=0x01) reading back the 4 bytes
    let mut in_buf = [0u8; 4];
    let mut ctrl_in = usbdevfs_ctrltransfer {
        bRequestType: 0xc0,
        bRequest: 0x01,
        wValue: 0x1234,
        wIndex: 0,
        wLength: in_buf.len() as u16,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: in_buf.as_mut_ptr() as u64 },
        ..Default::default()
    };
    let n = usb_control_sync(&fd, &mut ctrl_in).expect("USBDEVFS_CONTROL IN failed");
    assert_eq!(n, 4);
    assert_eq!(in_buf, [0xca, 0xfe, 0xba, 0xbe]);

    // 3. Zero-length Control OUT (wLength=0, data=null)
    let mut ctrl_zlp = usbdevfs_ctrltransfer {
        bRequestType: 0x40,
        bRequest: 0x02,
        wValue: 0,
        wIndex: 0,
        wLength: 0,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: 0 },
        ..Default::default()
    };
    let n = usb_control_sync(&fd, &mut ctrl_zlp).expect("USBDEVFS_CONTROL ZLP failed");
    assert_eq!(n, 0);

    // 4. Control transfer STALL (bRequest=0xff) -> EPIPE
    let mut stall_req = usbdevfs_ctrltransfer {
        bRequestType: 0xc0,
        bRequest: 0xff,
        wValue: 0,
        wIndex: 0,
        wLength: 4,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: in_buf.as_mut_ptr() as u64 },
        ..Default::default()
    };
    assert_eq!(usb_control_sync(&fd, &mut stall_req), Err(libc::EPIPE));

    // 5. Bounds and EFAULT checks
    let mut oversize_buf = vec![0u8; 4097];
    let mut oversize = usbdevfs_ctrltransfer {
        bRequestType: 0xc0,
        bRequest: 0x01,
        wValue: 0,
        wIndex: 0,
        wLength: oversize_buf.len() as u16,
        timeout: 1000,
        data: linux_uapi::uaddr { addr: oversize_buf.as_mut_ptr() as u64 },
        ..Default::default()
    };
    assert_eq!(usb_control_sync(&fd, &mut oversize), Err(libc::EINVAL));

    let mut null_data = usbdevfs_ctrltransfer {
        bRequestType: 0x40,
        bRequest: 0x01,
        wValue: 0,
        wIndex: 0,
        wLength: 4,
        timeout: 1000,
        data: linux_uapi::uaddr { addr: 0 },
        ..Default::default()
    };
    assert_eq!(usb_control_sync(&fd, &mut null_data), Err(libc::EFAULT));
}

#[test]
#[serial]
fn test_usb_synchronous_bulk_transfers() {
    let fd1 = open_usb_rdwr();
    let fd2 = open_usb_rdwr();

    assert_eq!(usb_claim_interface(&fd1, 0), Ok(()));

    // 1. Bulk OUT on 0x02
    let mut tx_data = [0xde_u8, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04];
    let mut bulk_out = usbdevfs_bulktransfer {
        ep: 0x02,
        len: tx_data.len() as u32,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: tx_data.as_mut_ptr() as u64 },
        ..Default::default()
    };
    let n = usb_bulk_sync(&fd1, &mut bulk_out).expect("USBDEVFS_BULK OUT failed");
    assert_eq!(n, 8);

    // 2. Bulk IN on 0x81 (requesting 64 bytes, receiving 8-byte short packet)
    let mut rx_data = [0u8; 64];
    let mut bulk_in = usbdevfs_bulktransfer {
        ep: 0x81,
        len: rx_data.len() as u32,
        timeout: 2000,
        data: linux_uapi::uaddr { addr: rx_data.as_mut_ptr() as u64 },
        ..Default::default()
    };
    let n = usb_bulk_sync(&fd1, &mut bulk_in).expect("USBDEVFS_BULK IN failed");
    assert_eq!(n, 8);
    assert_eq!(&rx_data[..8], &tx_data);

    // 3. Contention: fd2 attempting USBDEVFS_BULK while fd1 claims interface 0 fails with EBUSY
    assert_eq!(usb_bulk_sync(&fd2, &mut bulk_out), Err(libc::EBUSY));

    // 4. Non-existent endpoint 0x89 -> ENOENT, invalid endpoint 0x1ff -> EINVAL, null data -> EFAULT
    let mut bad_ep = usbdevfs_bulktransfer {
        ep: 0x89,
        len: 4,
        timeout: 1000,
        data: linux_uapi::uaddr { addr: rx_data.as_mut_ptr() as u64 },
        ..Default::default()
    };
    assert_eq!(usb_bulk_sync(&fd1, &mut bad_ep), Err(libc::ENOENT));

    bad_ep.ep = 0x1ff;
    assert_eq!(usb_bulk_sync(&fd1, &mut bad_ep), Err(libc::EINVAL));

    let mut null_bulk = usbdevfs_bulktransfer {
        ep: 0x02,
        len: 4,
        timeout: 1000,
        data: linux_uapi::uaddr { addr: 0 },
        ..Default::default()
    };
    assert_eq!(usb_bulk_sync(&fd1, &mut null_bulk), Err(libc::EFAULT));
}

#[test]
#[serial]
fn test_usb_async_urb_submit_poll_reap_and_discard() {
    let fd = open_usb_rdwr();
    let raw_fd = fd.as_raw_fd();

    assert_eq!(usb_claim_interface(&fd, 0), Ok(()));

    // 1. REAPURBNDELAY with no completed URBs fails with EAGAIN
    let mut reaped_ptr: *mut usbdevfs_urb = std::ptr::null_mut();
    // SAFETY: reaped_ptr is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb_ndelay(&fd, &mut reaped_ptr) }, Err(libc::EAGAIN));

    // 2. Submit an async Bulk IN URB on 0x81 when no OUT data has been sent yet -> stays pending
    let mut in_buf = [0u8; 64];
    let mut urb_in = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_BULK as u8,
        endpoint: 0x81,
        buffer: linux_uapi::uaddr { addr: in_buf.as_mut_ptr() as u64 },
        buffer_length: in_buf.len() as i32,
        usercontext: linux_uapi::uaddr { addr: 0x1111 },
        ..Default::default()
    };
    // SAFETY: `in_buf` and `urb_in` are stack-allocated in this test frame and will remain valid
    // until reaped below.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut urb_in) }, Ok(()));

    // While pending, poll(POLLOUT) returns 0 and REAPURBNDELAY returns EAGAIN
    let mut pfd = libc::pollfd { fd: raw_fd, events: libc::POLLOUT, revents: 0 };
    // SAFETY: `pfd` is a valid stack-allocated pollfd struct.
    assert_eq!(unsafe { libc::poll(&mut pfd, 1, 0) }, 0);
    // SAFETY: `reaped_ptr` is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb_ndelay(&fd, &mut reaped_ptr) }, Err(libc::EAGAIN));

    // Cancel the in-flight Bulk IN URB via USBDEVFS_DISCARDURB
    // SAFETY: `urb_in` was previously submitted and is currently in-flight.
    assert_eq!(unsafe { usb_discard_urb(&fd, &mut urb_in) }, Ok(()));

    // Discarding an already-discarded or unknown URB fails with EINVAL
    // SAFETY: `urb_in` is already discarded.
    assert_eq!(unsafe { usb_discard_urb(&fd, &mut urb_in) }, Err(libc::EINVAL));

    // Reap the cancelled URB -> status == -ENOENT (-2)
    // SAFETY: `reaped_ptr` is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb_ndelay(&fd, &mut reaped_ptr) }, Ok(()));
    assert_eq!(reaped_ptr, &mut urb_in as *mut usbdevfs_urb);
    assert_eq!(urb_in.status, -libc::ENOENT);
    assert_eq!(urb_in.actual_length, 0);
    assert_eq!(urb_in.usercontext.addr, 0x1111);

    // 3. Submit an async Bulk OUT URB on 0x02 and reap it
    let mut out_buf = [0x10_u8, 0x20, 0x30, 0x40];
    let mut urb_out = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_BULK as u8,
        endpoint: 0x02,
        buffer: linux_uapi::uaddr { addr: out_buf.as_mut_ptr() as u64 },
        buffer_length: out_buf.len() as i32,
        usercontext: linux_uapi::uaddr { addr: 0x2222 },
        ..Default::default()
    };
    // SAFETY: `out_buf` and `urb_out` will remain valid until reaped below.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut urb_out) }, Ok(()));

    let mut pfd = libc::pollfd { fd: raw_fd, events: libc::POLLOUT, revents: 0 };
    // SAFETY: `pfd` is a valid stack-allocated pollfd struct.
    assert_eq!(unsafe { libc::poll(&mut pfd, 1, 2000) }, 1);
    assert_ne!(pfd.revents & libc::POLLOUT, 0);

    reaped_ptr = std::ptr::null_mut();
    // SAFETY: `reaped_ptr` is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb(&fd, &mut reaped_ptr) }, Ok(()));
    assert_eq!(reaped_ptr, &mut urb_out as *mut usbdevfs_urb);
    assert_eq!(urb_out.status, 0);
    assert_eq!(urb_out.actual_length, 4);
    assert_eq!(urb_out.usercontext.addr, 0x2222);

    // 4. Submit an async Bulk IN URB on 0x81 and wait via epoll(EPOLLOUT) for the echoed packet
    in_buf.fill(0);
    urb_in.status = 0;
    urb_in.actual_length = 0;
    urb_in.usercontext = linux_uapi::uaddr { addr: 0x3333 };

    // SAFETY: epoll_create1 syscall.
    let epfd = unsafe { libc::epoll_create1(0) };
    assert!(epfd >= 0);
    let mut ev = libc::epoll_event { events: libc::EPOLLOUT as u32, u64: 0x9999 };
    // SAFETY: `epfd` and `raw_fd` are valid file descriptors, `ev` is valid epoll_event.
    assert_eq!(unsafe { libc::epoll_ctl(epfd, libc::EPOLL_CTL_ADD, raw_fd, &mut ev) }, 0);

    // SAFETY: `in_buf` and `urb_in` remain valid until reaped below.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut urb_in) }, Ok(()));

    let mut out_ev = libc::epoll_event { events: 0, u64: 0 };
    // SAFETY: `epfd` is valid, `out_ev` is a valid epoll_event pointer.
    let n_ev = unsafe { libc::epoll_wait(epfd, &mut out_ev, 1, 2000) };
    // SAFETY: `epfd` is a valid file descriptor.
    unsafe {
        libc::close(epfd);
    }
    assert_eq!(n_ev, 1);
    assert_ne!(out_ev.events & (libc::EPOLLOUT as u32), 0);
    // Copy out of `epoll_event`, which is `#[repr(packed)]` on x86_64.
    let out_data = out_ev.u64;
    assert_eq!(out_data, 0x9999);

    reaped_ptr = std::ptr::null_mut();
    // SAFETY: `reaped_ptr` is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb(&fd, &mut reaped_ptr) }, Ok(()));
    assert_eq!(reaped_ptr, &mut urb_in as *mut usbdevfs_urb);
    assert_eq!(urb_in.status, 0);
    assert_eq!(urb_in.actual_length, 4);
    assert_eq!(urb_in.usercontext.addr, 0x3333);
    assert_eq!(&in_buf[..4], &[0x10, 0x20, 0x30, 0x40]);

    // 5. Submit an async Control IN URB (8-byte setup + 4-byte data buffer)
    let mut ctrl_buf = [
        0xc0_u8, 0x02, 0x00, 0x00, 0x00, 0x00, 0x04,
        0x00, // 8-byte setup (bRequest = 0x02, wLength = 4)
        0x00, 0x00, 0x00, 0x00, // 4-byte IN data buffer
    ];
    let mut urb_ctrl = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_CONTROL as u8,
        endpoint: 0,
        buffer: linux_uapi::uaddr { addr: ctrl_buf.as_mut_ptr() as u64 },
        buffer_length: ctrl_buf.len() as i32,
        usercontext: linux_uapi::uaddr { addr: 0x4444 },
        ..Default::default()
    };
    // SAFETY: `ctrl_buf` and `urb_ctrl` remain valid until reaped below.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut urb_ctrl) }, Ok(()));

    reaped_ptr = std::ptr::null_mut();
    // SAFETY: `reaped_ptr` is a valid pointer to receive the reaped URB pointer.
    assert_eq!(unsafe { usb_reap_urb(&fd, &mut reaped_ptr) }, Ok(()));
    assert_eq!(reaped_ptr, &mut urb_ctrl as *mut usbdevfs_urb);
    assert_eq!(urb_ctrl.status, 0);
    assert_eq!(urb_ctrl.actual_length, 4);
    assert_eq!(urb_ctrl.usercontext.addr, 0x4444);
    assert_eq!(&ctrl_buf[..8], &[0xc0, 0x02, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00]);
    assert_eq!(&ctrl_buf[8..12], &[0x42; 4]);

    // 6. SUBMITURB invalid parameters:
    // - buffer_length > 0 with null buffer -> EINVAL
    let mut null_buf_urb = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_BULK as u8,
        endpoint: 0x02,
        buffer: linux_uapi::uaddr { addr: 0 },
        buffer_length: 4,
        ..Default::default()
    };
    // SAFETY: Testing rejection of null buffer pointer.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut null_buf_urb) }, Err(libc::EINVAL));

    // - unmapped non-null buffer on OUT URB -> EFAULT
    // SAFETY: Creating an unmapped memory region with PROT_NONE.
    let unmapped = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            4096,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(unmapped, libc::MAP_FAILED);
    let mut unmapped_out_urb = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_BULK as u8,
        endpoint: 0x02,
        buffer: linux_uapi::uaddr { addr: unmapped as u64 },
        buffer_length: 4,
        ..Default::default()
    };
    // SAFETY: Testing rejection of unmapped memory buffer.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut unmapped_out_urb) }, Err(libc::EFAULT));
    // SAFETY: Cleaning up the temporary mapped memory.
    unsafe {
        libc::munmap(unmapped, 4096);
    }

    // - out-of-bounds buffer (past end of user address space) on IN and OUT URBs -> EFAULT
    for ep in [0x81u8, 0x02u8] {
        let mut oob_urb = usbdevfs_urb {
            type_: USBDEVFS_URB_TYPE_BULK as u8,
            endpoint: ep,
            buffer: linux_uapi::uaddr { addr: u64::MAX - 3 },
            buffer_length: 4,
            ..Default::default()
        };
        // SAFETY: Testing rejection of out-of-bounds buffer address.
        assert_eq!(unsafe { usb_submit_urb(&fd, &mut oob_urb) }, Err(libc::EFAULT));
    }

    // - non-existent endpoint 0x89 -> ENOENT
    let mut bad_ep_urb = usbdevfs_urb {
        type_: USBDEVFS_URB_TYPE_BULK as u8,
        endpoint: 0x89,
        buffer: linux_uapi::uaddr { addr: out_buf.as_mut_ptr() as u64 },
        buffer_length: 4,
        ..Default::default()
    };
    // SAFETY: Testing rejection of non-existent endpoint.
    assert_eq!(unsafe { usb_submit_urb(&fd, &mut bad_ep_urb) }, Err(libc::ENOENT));
}
