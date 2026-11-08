// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared test harness for `ffx uart` host-side E2E tests.
//!
//! Provides [`TestEmulator`] lifecycle management, `ffx uart` subcommand helpers,
//! metadata and socket fixture helpers backed by [`uart_driver_api`], and a mock
//! UNIX-socket UART target backed by [`uart_fpl`].

use ffx_config::environment::ExecutableKind;
use ffx_config::{ConfigMap, EnvironmentContext};
use ffx_executor::CommandOutput;
use ffx_isolate::{Isolate, IsolateError};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uart_driver_api::{ConnectionMetadata, ConnectionStatus, UartProtocol};
use uart_fpl::{
    CONTROL_CHANNEL_ID, FrameParser, FrameType, ProtocolId, TargetHandshake, encode_frame,
};

/// Base name of the emulator instance shared by most tests. `ffx emu start`
/// appends `-emu` to produce the nodename reported by the target.
const SHARED_EMU_BASE_NAME: &str = "shared-uart-emu";

/// Kernel argument that enables the target-side `fdomain-uart-driver`.
const UART_KERNEL_ARG: &str = "dev.fdomain.uart=true";

/// Metadata JSON for a driver whose PID no longer exists.
pub const STALE_METADATA_JSON: &str = r#"{"pid": 999999, "target": "stale_target"}"#;

fn per_process_tmp_dir(prefix: &str) -> PathBuf {
    let username = std::env::var("USER").unwrap_or_else(|_| "ffx_uart_test".to_string());
    PathBuf::from(format!("/tmp/{prefix}_{username}_{}", std::process::id()))
}

static SHARED_UART_DATA_DIR: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| per_process_tmp_dir("shared_ffx_uart_data"));

static SHARED_EMU_INSTANCE_DIR: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| per_process_tmp_dir("shared_ffx_emu_instances"));

static SHARED_SSH_KEY_DIR: std::sync::LazyLock<PathBuf> =
    std::sync::LazyLock::new(|| per_process_tmp_dir("shared_ffx_uart_ssh"));

static INITIALIZED: AtomicBool = AtomicBool::new(false);

static SHARED_ISOLATE: Mutex<Option<Arc<Isolate>>> = Mutex::new(None);

// ==================== Process & permission helpers ====================

pub fn process_exists(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn wait_for_process_exit(pid: i32, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if !process_exists(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    !process_exists(pid)
}

fn kill_process(pid: i32) {
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status();
    wait_for_process_exit(pid, Duration::from_secs(1));
}

fn kill_drivers_recorded_in(uart_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(uart_dir) else { return };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().and_then(|s| s.to_str())
            != Some(uart_driver_api::METADATA_FILE_EXTENSION)
        {
            continue;
        }
        if let Some(pid) = read_pid_from_metadata_file(&path) {
            if pid as u32 != std::process::id() {
                kill_process(pid);
            }
        }
    }
}

fn read_pid_from_metadata_file(path: &Path) -> Option<i32> {
    let content = std::fs::read_to_string(path).ok()?;
    let val: serde_json::Value = serde_json::from_str(&content).ok()?;
    val.get("pid")?.as_i64().map(|pid| pid as i32)
}

/// A long-lived child process whose `argv[0]` matches `ffx uart`'s driver-process
/// heuristics, used to stand in for a live `ffx-uart-driver` when seeding metadata.
pub struct MockDriverProcess(std::process::Child);

impl MockDriverProcess {
    pub fn spawn() -> Self {
        use std::os::unix::process::CommandExt;
        let child = std::process::Command::new("sleep")
            .arg("60")
            .arg0("mock_driver")
            .spawn()
            .expect("spawn mock_driver");
        Self(child)
    }

    pub fn pid(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for MockDriverProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Restores write permission on `path` when dropped.
pub struct PermissionGuard {
    path: PathBuf,
}

impl Drop for PermissionGuard {
    fn drop(&mut self) {
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(&self.path, perms);
        }
    }
}

/// Serializes emulator boots across concurrently running test binaries.
struct BootLockGuard {
    lock_dir: PathBuf,
}

impl BootLockGuard {
    fn acquire() -> Self {
        let lock_dir = PathBuf::from("/tmp/ffx_uart_boot.lock");
        loop {
            Self::remove_if_owner_dead(&lock_dir);
            match std::fs::create_dir(&lock_dir) {
                Ok(()) => {
                    let _ = std::fs::write(lock_dir.join("pid"), std::process::id().to_string());
                    return Self { lock_dir };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => panic!("Failed to create lock dir: {e:?}"),
            }
        }
    }

    fn remove_if_owner_dead(lock_dir: &Path) {
        if !lock_dir.exists() {
            return;
        }
        let pid_file = lock_dir.join("pid");
        for _ in 0..5 {
            if pid_file.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let owner_alive = std::fs::read_to_string(&pid_file)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
            .is_some_and(process_exists);
        if !owner_alive {
            let _ = std::fs::remove_dir_all(lock_dir);
        }
    }
}

impl Drop for BootLockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.lock_dir);
    }
}

// ==================== TestEmulator ====================

pub struct TestEmulatorInner {
    pub emu_name: String,
    pub isolate: Arc<Isolate>,
    pub shared_data_dir: PathBuf,
    is_shared: bool,
    stopped: AtomicBool,
}

#[derive(Clone)]
pub struct TestEmulator {
    inner: Arc<TestEmulatorInner>,
}

impl std::ops::Deref for TestEmulator {
    type Target = TestEmulatorInner;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Drop for TestEmulatorInner {
    fn drop(&mut self) {
        if !self.is_shared && !self.stopped.load(Ordering::SeqCst) {
            let _ = self.isolate.ffx_sync(&["emu", "stop", &self.emu_name]);
        }
    }
}

impl TestEmulator {
    /// Returns the process-wide shared emulator with all host-side UART state wiped.
    pub async fn shared() -> Self {
        let emu = Self::boot(SHARED_EMU_BASE_NAME, true).await;
        emu.reset().await;
        emu
    }

    /// Boots a dedicated emulator instance that is stopped when the handle is dropped.
    pub async fn private(base_name: &str) -> Self {
        Self::boot(base_name, false).await
    }

    /// Kills every recorded driver process and recreates the shared data directory.
    pub async fn reset(&self) {
        kill_drivers_recorded_in(&self.uart_dir());
        let _ = std::fs::remove_dir_all(&self.shared_data_dir);
        let _ = std::fs::create_dir_all(&self.shared_data_dir);
    }

    async fn boot(base_name: &str, is_shared: bool) -> Self {
        let _boot_lock = BootLockGuard::acquire();

        if !INITIALIZED.swap(true, Ordering::SeqCst) {
            let _ = std::process::Command::new("pkill").args(["-f", "qemu-system-x86"]).status();
            let _ = std::fs::remove_dir_all(&*SHARED_SSH_KEY_DIR);
        }
        let _ = std::fs::create_dir_all(&*SHARED_SSH_KEY_DIR);
        Self::kill_stale_drivers();

        let isolate = if is_shared {
            let cached = SHARED_ISOLATE.lock().unwrap().clone();
            match cached {
                Some(iso) => iso,
                None => {
                    let iso = Arc::new(Self::new_isolate(base_name).await);
                    *SHARED_ISOLATE.lock().unwrap() = Some(iso.clone());
                    iso
                }
            }
        } else {
            Arc::new(Self::new_isolate(base_name).await)
        };

        let emu_name = format!("{base_name}-emu");
        if !Self::emulator_is_running(&isolate, &emu_name).await {
            let _ = std::fs::create_dir_all(&*SHARED_UART_DATA_DIR);
            let _ = std::fs::create_dir_all(&*SHARED_EMU_INSTANCE_DIR);
            let log = isolate.log_dir().join("emulator.log");
            start_emulator_instance(&isolate, &emu_name, &log).await;
        }

        Self {
            inner: Arc::new(TestEmulatorInner {
                emu_name,
                isolate,
                shared_data_dir: SHARED_UART_DATA_DIR.clone(),
                is_shared,
                stopped: AtomicBool::new(false),
            }),
        }
    }

    fn kill_stale_drivers() {
        let _ = std::process::Command::new("pkill").args(["-f", "ffx-uart-driver"]).status();
        for _ in 0..100 {
            let any_alive = std::process::Command::new("pgrep")
                .args(["-f", "ffx-uart-driver"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !any_alive {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    async fn new_isolate(name: &str) -> Isolate {
        let ssh_priv_key = SHARED_SSH_KEY_DIR.join("ssh_private_key");
        let ssh_pub_key = SHARED_SSH_KEY_DIR.join("ssh_public_key");

        let env_context = EnvironmentContext::detect(
            ExecutableKind::Test,
            ConfigMap::new(),
            &std::env::current_dir().expect("current directory"),
            None,
            false,
        )
        .expect("new detected context");

        let isolate = Isolate::new_in_test(name, ssh_priv_key.clone(), &env_context)
            .await
            .expect("create isolate");

        // Workaround for analytics panic in isolated environment for Googlers.
        let metrics_dir = isolate.dir().join("metrics_home/.fuchsia/metrics");
        let _ = std::fs::write(metrics_dir.join("analytics-status-internal"), "0");

        let settings = [
            ("ssh.priv", ssh_priv_key.to_string_lossy().into_owned()),
            ("ssh.pub", ssh_pub_key.to_string_lossy().into_owned()),
            ("log.level", "debug".to_string()),
            ("shared_data", SHARED_UART_DATA_DIR.to_string_lossy().into_owned()),
            ("emu.instance_dir", SHARED_EMU_INSTANCE_DIR.to_string_lossy().into_owned()),
        ];
        for (key, value) in settings {
            let output = isolate.ffx(&["config", "set", key, &value]).await.expect("config set");
            assert_success(&output, &format!("ffx config set {key}"));
        }
        isolate
    }

    async fn emulator_is_running(isolate: &Isolate, emu_name: &str) -> bool {
        let Ok(output) = isolate.ffx(&["--machine", "json", "emu", "list"]).await else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        serde_json::from_str::<serde_json::Value>(&output.stdout)
            .ok()
            .and_then(|val| val.as_array().cloned())
            .is_some_and(|arr| {
                arr.iter().any(|item| {
                    item.get("name").and_then(|n| n.as_str()) == Some(emu_name)
                        && item.get("state").and_then(|s| s.as_str()) == Some("running")
                })
            })
    }

    pub async fn stop(&self) -> CommandOutput {
        self.stopped.store(true, Ordering::SeqCst);
        let output = self.isolate.ffx(&["emu", "stop", &self.emu_name]).await.unwrap();
        assert_success(&output, "ffx emu stop");
        output
    }

    pub async fn restart(&self) {
        self.stop().await;
        self.stopped.store(false, Ordering::SeqCst);
        let reboot_log = self.isolate.log_dir().join("emulator_reboot.log");
        start_emulator_instance(&self.isolate, &self.emu_name, &reboot_log).await;
    }

    pub async fn wait_for_target_echo(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            let echo = self.isolate.ffx(&["-t", &self.emu_name, "target", "echo"]).await.unwrap();
            if echo.status.success() {
                return true;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        false
    }

    // ---------- Raw `ffx uart` subcommands ----------

    pub async fn connect(&self, args: &[&str]) -> Result<CommandOutput, IsolateError> {
        let output = self.run_uart_subcommand(&["connect"], args).await?;
        if !output.status.success() {
            self.dump_emulator_log();
        }
        Ok(output)
    }

    pub async fn reconnect(&self, args: &[&str]) -> Result<CommandOutput, IsolateError> {
        self.run_uart_subcommand(&["connect", "--reconnect"], args).await
    }

    pub async fn disconnect(&self, args: &[&str]) -> Result<CommandOutput, IsolateError> {
        self.run_uart_subcommand(&["disconnect"], args).await
    }

    pub async fn probe(&self, args: &[&str]) -> Result<CommandOutput, IsolateError> {
        self.run_uart_subcommand(&["probe"], args).await
    }

    async fn run_uart_subcommand(
        &self,
        subcommand: &[&str],
        args: &[&str],
    ) -> Result<CommandOutput, IsolateError> {
        let target_arg = args.first().map(|t| with_uart_prefix(t));
        let mut ffx_args: Vec<&str> = Vec::new();
        if let Some(t) = target_arg.as_deref() {
            ffx_args.extend(["-t", t]);
        }
        ffx_args.push("uart");
        ffx_args.extend_from_slice(subcommand);
        ffx_args.extend_from_slice(args.get(1..).unwrap_or(&[]));
        self.isolate.ffx(&ffx_args).await
    }

    // ---------- Checked `ffx uart` helpers (`_ok` / `_err` / `*_target`) ----------

    pub async fn connect_ok(&self, args: &[&str]) -> CommandOutput {
        let res = self.connect(args).await.unwrap();
        assert_success(&res, &format!("connect {args:?}"));
        res
    }

    pub async fn connect_err(&self, args: &[&str]) -> CommandOutput {
        let res = self.connect(args).await.unwrap();
        assert_failure(&res, &format!("connect {args:?}"));
        res
    }

    pub async fn reconnect_ok(&self, args: &[&str]) -> CommandOutput {
        let res = self.reconnect(args).await.unwrap();
        assert_success(&res, &format!("reconnect {args:?}"));
        res
    }

    pub async fn reconnect_err(&self, args: &[&str]) -> CommandOutput {
        let res = self.reconnect(args).await.unwrap();
        assert_failure(&res, &format!("reconnect {args:?}"));
        res
    }

    pub async fn disconnect_ok(&self, args: &[&str]) -> CommandOutput {
        let res = self.disconnect(args).await.unwrap();
        assert_success(&res, &format!("disconnect {args:?}"));
        res
    }

    pub async fn disconnect_err(&self, args: &[&str]) -> CommandOutput {
        let res = self.disconnect(args).await.unwrap();
        assert_failure(&res, &format!("disconnect {args:?}"));
        res
    }

    pub async fn probe_ok(&self, args: &[&str]) -> CommandOutput {
        let res = self.probe(args).await.unwrap();
        assert_success(&res, &format!("probe {args:?}"));
        assert!(res.stdout.contains("ALIVE"), "probe stdout: {}", res.stdout);
        res
    }

    pub async fn probe_err(&self, args: &[&str]) -> CommandOutput {
        let res = self.probe(args).await.unwrap();
        assert_failure(&res, &format!("probe {args:?}"));
        res
    }

    /// Connects to `self.emu_name` and asserts that socket, metadata, and `list` entry exist.
    pub async fn connect_target(&self) {
        self.connect_ok(&[&self.emu_name]).await;
        self.assert_connected(&self.emu_name).await;
    }

    /// Reconnects to `self.emu_name` and asserts that socket, metadata, and `list` entry exist.
    pub async fn reconnect_target(&self) {
        self.reconnect_ok(&[&self.emu_name]).await;
        self.assert_connected(&self.emu_name).await;
    }

    /// Disconnects `self.emu_name` and asserts that socket, metadata, and `list` entry are removed.
    pub async fn disconnect_target(&self) {
        self.disconnect_ok(&[&self.emu_name]).await;
        self.assert_disconnected(&self.emu_name).await;
    }

    /// Runs `ffx -t uart:<emu_name> target echo` and asserts that it succeeds.
    ///
    /// The `uart:` prefix forces the request over the UART connection, so this verifies that
    /// FDomain traffic flows end to end rather than only that the driver reports `Connected`.
    pub async fn assert_target_echo(&self) {
        let target = with_uart_prefix(&self.emu_name);
        let output = self.isolate.ffx(&["-t", &target, "target", "echo"]).await.unwrap();
        assert_success(&output, &format!("ffx -t {target} target echo"));
    }

    /// Returns the parsed output of `ffx --machine json uart list`.
    pub async fn list_connections(&self) -> serde_json::Value {
        let output = self
            .isolate
            .ffx(&["--machine", "json", "uart", "list"])
            .await
            .expect("ffx --machine json uart list");
        assert_success(&output, "ffx --machine json uart list");
        serde_json::from_str(&output.stdout).expect("valid json from ffx uart list")
    }

    pub fn find_connection<'a>(
        &self,
        list: &'a serde_json::Value,
        target: &str,
    ) -> Option<&'a serde_json::Value> {
        let raw = strip_uart_prefix(target);
        let resolved = self.resolved_target_path(raw).to_string_lossy().into_owned();
        let expected_id = self.target_id(raw);
        list.as_array()?.iter().find(|entry| {
            let entry_target = entry.get("target").and_then(|t| t.as_str());
            let entry_nodename = entry.get("nodename").and_then(|n| n.as_str());
            let entry_id = entry.get("id").and_then(|i| i.as_str());
            entry_target == Some(raw)
                || entry_target == Some(resolved.as_str())
                || entry_nodename == Some(raw)
                || entry_id == Some(expected_id.as_str())
        })
    }

    pub fn list_contains_target(&self, list: &serde_json::Value, target: &str) -> bool {
        self.find_connection(list, target).is_some()
    }

    pub async fn assert_connected(&self, target: &str) {
        let metadata_path = self.metadata_path(target);
        assert!(metadata_path.exists(), "metadata file {metadata_path:?} should exist");
        let socket_path = self.socket_path(target);
        assert!(socket_path.exists(), "socket file {socket_path:?} should exist");
        let list = self.list_connections().await;
        assert!(
            self.list_contains_target(&list, target),
            "target {target} should be in the list of connections, list: {list:?}"
        );
    }

    pub async fn assert_disconnected(&self, target: &str) {
        let metadata_path = self.metadata_path(target);
        assert!(!metadata_path.exists(), "metadata file {metadata_path:?} should NOT exist");
        let socket_path = self.socket_path(target);
        assert!(!socket_path.exists(), "socket file {socket_path:?} should NOT exist");
        let list = self.list_connections().await;
        assert!(
            !self.list_contains_target(&list, target),
            "target {target} should NOT be in the list of connections, list: {list:?}"
        );
    }

    fn dump_emulator_log(&self) {
        eprintln!("=== [FAIL] EMULATOR LOG FOR {} ===", self.emu_name);
        match std::fs::read_to_string(self.isolate.log_dir().join("emulator.log")) {
            Ok(content) => eprintln!("{content}"),
            Err(_) => eprintln!("Could not read emulator.log"),
        }
    }

    // ---------- Host-side metadata paths (via `uart_driver_api`) ----------

    pub fn uart_dir(&self) -> PathBuf {
        self.shared_data_dir.join(uart_driver_api::UART_SHARED_SUBDIR)
    }

    pub fn socket_path(&self, target: &str) -> PathBuf {
        self.uart_dir().join(format!(
            "ffx_uart_{}.{}",
            self.target_id(target),
            uart_driver_api::UNIX_SOCKET_EXTENSION
        ))
    }

    pub fn metadata_path(&self, target: &str) -> PathBuf {
        uart_driver_api::get_metadata_path(&self.socket_path(target))
    }

    pub fn write_metadata(&self, target: &str, contents: &str) -> PathBuf {
        let path = self.metadata_path(target);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents).unwrap();
        path
    }

    pub fn write_placeholder_socket(&self, target: &str) -> PathBuf {
        let path = self.socket_path(target);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "").unwrap();
        path
    }

    pub fn write_metadata_for_mock_driver(
        &self,
        target: &str,
        driver: &MockDriverProcess,
        status: ConnectionStatus,
    ) {
        let meta = ConnectionMetadata {
            pid: driver.pid(),
            target: target.to_string(),
            status,
            id: Some(self.target_id(target)),
            baud: None,
            protocol: UartProtocol::ResendSP,
            log_level: None,
            nodename: None,
            serial: None,
        };
        self.write_metadata(target, &serde_json::to_string(&meta).unwrap());
    }

    pub fn driver_pid(&self, target: &str) -> i32 {
        read_pid_from_metadata_file(&self.metadata_path(target))
            .unwrap_or_else(|| panic!("no pid in metadata for {target}"))
    }

    /// Returns the baud rate recorded in `target`'s connection metadata, if any.
    pub fn driver_baud(&self, target: &str) -> Option<u64> {
        let content = std::fs::read_to_string(self.metadata_path(target)).ok()?;
        let val: serde_json::Value = serde_json::from_str(&content).ok()?;
        val.get("baud")?.as_u64()
    }

    pub fn make_uart_dir_readonly(&self) -> PermissionGuard {
        let dir = self.uart_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        let _ = std::fs::set_permissions(&dir, perms);
        PermissionGuard { path: dir }
    }

    pub fn target_id(&self, target: &str) -> String {
        let target = target.trim_end_matches(['/', '\\']);
        let target = if target.is_empty() { "/" } else { target };
        uart_driver_api::get_target_id_from_target_path(&self.resolved_target_path(target))
    }

    pub fn resolved_target_path(&self, target: &str) -> PathBuf {
        if target.contains('/') || target.ends_with(".sock") {
            return PathBuf::from(target);
        }
        let instance_dir = SHARED_EMU_INSTANCE_DIR.join(target);
        let serial_socket = std::fs::read_to_string(instance_dir.join("engine.json"))
            .ok()
            .and_then(|content| serde_json::from_str::<serde_json::Value>(&content).ok())
            .and_then(|val| {
                val.pointer("/emulator_configuration/runtime/serial_socket")
                    .and_then(|v| v.as_str())
                    .map(|s| s.strip_prefix("unix:").unwrap_or(s).to_string())
            })
            .filter(|s| !s.is_empty());
        match serial_socket {
            Some(path) => PathBuf::from(path),
            None => instance_dir.join("serial_transport"),
        }
    }
}

fn strip_uart_prefix(target: &str) -> &str {
    target.strip_prefix("uart:").unwrap_or(target)
}

fn with_uart_prefix(target: &str) -> String {
    format!("uart:{}", strip_uart_prefix(target))
}

async fn start_emulator_instance(isolate: &Isolate, emu_name: &str, log: &Path) {
    let product_bundle_path = std::env::var("PRODUCT_BUNDLE_PATH")
        .expect("PRODUCT_BUNDLE_PATH env var must be set by the test runner");
    assert!(
        Path::new(&product_bundle_path).exists(),
        "Product bundle path {product_bundle_path:?} does not exist. \
         This test requires a build configuration that supports emulation (e.g., core.x64). \
         If you are targeting physical hardware, please skip this E2E test."
    );
    let log = log.display().to_string();
    let output = isolate
        .ffx(&[
            "emu",
            "start",
            "--headless",
            "--net",
            "user",
            "--name",
            emu_name,
            "--log",
            &log,
            "--kernel-args",
            UART_KERNEL_ARG,
            &product_bundle_path,
        ])
        .await
        .expect("start emulator");
    assert_success(&output, "ffx emu start");
}

// ==================== Mock UART Target (backed by `uart_fpl`) ====================

/// A UNIX-socket UART target that answers the first `NegotiateReq` with a
/// `NegotiateResp` selecting `ResendSP`, then drains input until the host disconnects.
pub struct MockUartTarget {
    _listener: UnixListener,
    _handle: std::thread::JoinHandle<()>,
}

impl MockUartTarget {
    pub fn spawn(socket_path: &Path) -> Self {
        Self::spawn_with(socket_path, |frame| frame)
    }

    /// Passes the encoded `NegotiateResp` frame through `mutate_response` before sending it.
    pub fn spawn_with<F>(socket_path: &Path, mutate_response: F) -> Self
    where
        F: FnOnce(Vec<u8>) -> Vec<u8> + Send + 'static,
    {
        let listener = UnixListener::bind(socket_path).unwrap();
        let accept_listener = listener.try_clone().unwrap();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let Ok((mut stream, _)) = accept_listener.accept() else { return };

            let mut parser = FrameParser::new();
            let mut buf = [0u8; 64];
            let req = loop {
                let Ok(n) = stream.read(&mut buf) else { return };
                if n == 0 {
                    return;
                }
                parser.feed(&buf[..n]);
                if let Some(frame) = parser.next_frame() {
                    break frame;
                }
            };

            let target_hs = TargetHandshake::new(vec![ProtocolId::ResendSP]);
            let Ok((_, resp_payload)) = target_hs.handle_request(&req.payload) else { return };
            let response = encode_frame(
                req.session_id,
                CONTROL_CHANNEL_ID,
                1,
                FrameType::NegotiateResp,
                &resp_payload,
            )
            .unwrap();
            let _ = stream.write_all(&mutate_response(response));

            let mut discard = [0u8; 1024];
            while let Ok(n) = stream.read(&mut discard) {
                if n == 0 {
                    break;
                }
            }
        });
        Self { _listener: listener, _handle: handle }
    }
}

// ==================== Assertions ====================

pub fn assert_success(output: &CommandOutput, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: status={:?}, stdout={}, stderr={}",
        output.status,
        output.stdout,
        output.stderr
    );
}

pub fn assert_failure(output: &CommandOutput, what: &str) {
    assert!(
        !output.status.success(),
        "{what} unexpectedly succeeded: stdout={}, stderr={}",
        output.stdout,
        output.stderr
    );
}

pub fn connection_count(list: &serde_json::Value) -> usize {
    list.as_array().map_or(0, |a| a.len())
}
