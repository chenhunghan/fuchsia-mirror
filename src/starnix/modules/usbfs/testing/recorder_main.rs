// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Guest CLI binary invoked inside QEMU (`debian_guest`) to record `usbfs` scenarios into
//! golden JSON traces.

use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use std::{env, fs, thread};
use usbfs_linux_trace_exec::linux_ffs_daemon;
use usbfs_trace_schema::{DEFAULT_CONTROL_ADDR, Scenario};

fn wait_for_daemon(addr: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("Timed out waiting for linux_ffs_daemon on {addr}");
}

fn main() {
    if let Ok(addr) = env::var("USBFS_FFS_DAEMON_ONLY") {
        linux_ffs_daemon::serve(&addr).expect("linux_ffs_daemon::serve failed");
        return;
    }
    let args: Vec<String> = env::args().skip(1).collect();
    let mut scenarios_dir = PathBuf::from("/tmp/in/scenarios");
    let mut list_path = PathBuf::from("/tmp/in/scenarios_list.json");
    let mut out_dir = PathBuf::from("/tmp/out");
    let mut control_addr = DEFAULT_CONTROL_ADDR.to_string();
    let mut daemon_only = false;
    let mut filter: Vec<String> = Vec::new();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--scenarios-dir" => {
                i += 1;
                assert!(i < args.len(), "Missing value for --scenarios-dir");
                scenarios_dir = PathBuf::from(&args[i]);
            }
            "--list" => {
                i += 1;
                assert!(i < args.len(), "Missing value for --list");
                list_path = PathBuf::from(&args[i]);
            }
            "--out-dir" => {
                i += 1;
                assert!(i < args.len(), "Missing value for --out-dir");
                out_dir = PathBuf::from(&args[i]);
            }
            "--control-addr" => {
                i += 1;
                assert!(i < args.len(), "Missing value for --control-addr");
                control_addr = args[i].clone();
            }
            "--daemon-only" => {
                daemon_only = true;
            }
            "--scenarios" => {
                i += 1;
                while i < args.len() && !args[i].starts_with("--") {
                    filter.push(args[i].clone());
                    i += 1;
                }
                continue;
            }
            other => {
                panic!("Unknown argument: {other}");
            }
        }
        i += 1;
    }

    if daemon_only {
        linux_ffs_daemon::serve(&control_addr).expect("linux_ffs_daemon::serve failed");
        return;
    }

    let daemon_addr = control_addr.clone();
    thread::spawn(move || {
        if let Err(err) = linux_ffs_daemon::serve(&daemon_addr) {
            panic!("linux_ffs_daemon failed: {err}");
        }
    });
    wait_for_daemon(&control_addr, Duration::from_secs(5));

    let list_content = fs::read_to_string(&list_path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", list_path.display()));
    let all_names: Vec<String> =
        serde_json::from_str(&list_content).expect("Invalid scenarios_list.json");

    let selected: Vec<String> = if filter.is_empty() { all_names } else { filter };

    fs::create_dir_all(&out_dir).expect("Failed to create out_dir");

    for name in &selected {
        let scenario_path: PathBuf = Path::new(&scenarios_dir).join(format!("{name}.json"));
        let raw = fs::read_to_string(&scenario_path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {e}", scenario_path.display()));
        let scenario: Scenario = serde_json::from_str(&raw).unwrap_or_else(|e| {
            panic!("Invalid Scenario JSON in {}: {e}", scenario_path.display())
        });
        assert_eq!(
            &scenario.name, name,
            "Scenario name '{}' does not match filename '{name}.json'",
            scenario.name
        );

        eprintln!("[usbfs_trace_recorder] Recording scenario '{name}'...");
        let trace = usbfs_linux_trace_exec::record_scenario(&scenario, &control_addr);
        let trace_json =
            serde_json::to_string_pretty(&trace).expect("Failed to serialize RecordedTrace");
        let out_path = out_dir.join(format!("{name}.json"));
        fs::write(&out_path, format!("{trace_json}\n"))
            .unwrap_or_else(|e| panic!("Failed to write {}: {e}", out_path.display()));
        eprintln!(
            "[usbfs_trace_recorder] Recorded scenario '{name}' ({} steps)",
            trace.steps.len()
        );
    }
}
