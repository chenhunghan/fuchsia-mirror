// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::{
    EmulatorInstanceData, EmulatorInstanceInfo, EmulatorInstances, EngineOption, NetworkingMode,
    Result, SerialMode,
};
use std::path::Path;

/// Address for communicating with an emulator.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EmulatorAddr {
    /// Direct connection to VM guest using virtio-vsock.
    Vsock { cid: u32 },
    /// SSH connection forwarded to host loopback port (127.0.0.1:<port>).
    LoopbackPort(u16),
}

/// Target representation emitted by emulator watcher.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EmulatorTargetInfo {
    pub nodename: String,
    pub addresses: Vec<EmulatorAddr>,
    pub serial_number: Option<String>,
}

fn handle_instance(instance: &EmulatorInstanceData) -> Option<EmulatorTargetInfo> {
    if instance.is_running() {
        log::debug!(
            "Making target from {} using ssh port {:?}",
            instance.get_name(),
            instance.get_ssh_port()
        );
        make_target(instance)
    } else {
        None
    }
}

fn make_target(instance: &EmulatorInstanceData) -> Option<EmulatorTargetInfo> {
    let nodename: String = instance.get_name().into();
    let mut addresses = Vec::with_capacity(2);
    let vsock_device = instance.emulator_configuration.device.vsock.clone().filter(|x| x.enabled);

    if let Some(v) = &vsock_device {
        addresses.push(EmulatorAddr::Vsock { cid: v.cid });
    }

    if nodename.is_empty() {
        log::debug!("Skipping making target for emulator with empty nodename");
        return None;
    }

    // TUN/TAP emulators are discoverable via mDNS.
    // TODO(435460863): Add unit tests that check this path.
    if instance.get_networking_mode() == &NetworkingMode::Tap && vsock_device.is_none() {
        log::debug!("Skipping making target for {}, since it is tun/tap networking", nodename);
        return None;
    }
    let ssh_port = instance.get_ssh_port();
    if ssh_port.is_none() && vsock_device.is_none() {
        // No ssh port assigned and no vsock device, so don't create a target.
        log::debug!(
            "Skipping making target for {}, since ssh port and vsock device are both none",
            nodename
        );
        return None;
    }
    if let Some(port) = ssh_port {
        addresses.push(EmulatorAddr::LoopbackPort(port));
    }

    let serial_number = match &instance.emulator_configuration.runtime.serial_number {
        SerialMode::Enabled(serial) => Some(serial.clone()),
        _ => None,
    };

    Some(EmulatorTargetInfo { nodename, addresses, serial_number })
}

pub fn get_all_targets(instances: &EmulatorInstances) -> Result<Vec<EmulatorTargetInfo>> {
    let items = instances.get_all_instances()?;
    Ok(items.iter().flat_map(handle_instance).collect())
}

pub fn get_target(instances: &EmulatorInstances, name: &str) -> Result<Option<EmulatorTargetInfo>> {
    let instance_dir = instances.get_instance_dir(name, false)?;
    match crate::read_from_disk(&instance_dir) {
        Ok(EngineOption::DoesExist(emu_instance)) => Ok(handle_instance(&emu_instance)),
        _ => Ok(None),
    }
}

pub fn instance_name_from_path(instance_dir: &Path, path: &Path) -> Option<String> {
    if let Some(ext) = path.extension() {
        if ext == "log" || ext == "serial" {
            return None;
        }
    }
    let relative = path.strip_prefix(instance_dir).ok()?;
    let mut name: String = "".into();
    if let Some(instance_name) = relative.parent() {
        name = instance_name.to_string_lossy().to_string();
        if name.is_empty() {
            name = relative.to_string_lossy().to_string();
        }
    } else if !relative.to_string_lossy().is_empty() {
        name = relative.to_string_lossy().to_string();
    }
    if !name.is_empty() { Some(name) } else { None }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::create_dir_all;
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn test_instance_name_from_path() -> Result<()> {
        let temp = tempdir().expect("cannot get tempdir");
        let instance_dir = temp.path().to_path_buf();
        if !instance_dir.exists() {
            create_dir_all(&instance_dir)?;
        }
        let test_data = vec![
            (instance_dir.join("emu-instance"), Some(String::from("emu-instance"))),
            (instance_dir.join("emu-instance/engine.json"), Some(String::from("emu-instance"))),
            (instance_dir.join("emu-instance/emulator.log"), None),
            (instance_dir.join("emu-instance/emulator.serial"), None),
            (PathBuf::from("/someplace/unknown/emu-instance"), None),
            (PathBuf::from("./emu-instance"), None),
            (PathBuf::from("emu-instance"), None),
            (PathBuf::from(""), None),
        ];
        for (p, expected) in test_data {
            let actual = instance_name_from_path(&instance_dir, &p);
            assert_eq!(actual, expected, "Calling instance_name_from_path({p:?})");
        }
        Ok(())
    }

    #[test]
    fn test_get_all_targets() -> Result<()> {
        use std::fs::File;
        use std::io::Write;
        let temp_dir = tempdir().expect("Couldn't get a temporary directory for testing.");

        let instance_root = PathBuf::from(temp_dir.path());
        let emulator_instances = EmulatorInstances::new(instance_root.clone());

        let path1 = instance_root.join("path1");
        create_dir_all(path1.as_path())?;
        let file1_path = path1.join(crate::instances::SERIALIZE_FILE_NAME);
        let mut file1 = File::create(&file1_path)?;
        let mut instance_data = crate::EmulatorInstanceData::new_with_state(
            "emu-data-instance",
            crate::EngineState::Running,
        );
        instance_data.set_pid(std::process::id());
        let config = instance_data.get_emulator_configuration_mut();
        config.host.networking = crate::NetworkingMode::User;
        config.runtime.serial_number = SerialMode::Enabled("EM-123456789".to_string());
        config
            .host
            .port_map
            .insert(String::from("ssh"), crate::PortMapping { guest: 22, host: Some(3322) });
        let emu_config = serde_json::to_string(&instance_data)?;
        file1.write_all(emu_config.as_bytes())?;

        let path2 = instance_root.join("path2");
        create_dir_all(path2.as_path())?;
        let file2_path = path2.join(crate::instances::SERIALIZE_FILE_NAME);
        let mut file2 = File::create(&file2_path)?;
        let stopped_instance = crate::EmulatorInstanceData::new_with_state(
            "stopped-emu-instance",
            crate::EngineState::Staged,
        );
        let stopped_config = serde_json::to_string(&stopped_instance)?;
        file2.write_all(stopped_config.as_bytes())?;

        let targets = get_all_targets(&emulator_instances)?;
        assert_eq!(targets.len(), 1);
        assert_eq!(targets.first().unwrap().nodename, "emu-data-instance");
        assert_eq!(targets.first().unwrap().serial_number, Some("EM-123456789".to_string()));

        Ok(())
    }

    #[test]
    fn test_make_target_tap_without_vsock_returns_none() {
        let mut instance_data = crate::EmulatorInstanceData::new_with_state(
            "emu-tap-no-vsock",
            crate::EngineState::Running,
        );
        instance_data.set_pid(std::process::id());
        let config = instance_data.get_emulator_configuration_mut();
        config.host.networking = crate::NetworkingMode::Tap;

        assert_eq!(make_target(&instance_data), None);
    }

    #[test]
    fn test_make_target_tap_with_vsock_produces_vsock_target() {
        let mut instance_data = crate::EmulatorInstanceData::new_with_state(
            "emu-tap-vsock",
            crate::EngineState::Running,
        );
        instance_data.set_pid(std::process::id());
        let config = instance_data.get_emulator_configuration_mut();
        config.host.networking = crate::NetworkingMode::Tap;
        config.device.vsock = Some(crate::VsockDevice { enabled: true, cid: 42 });

        let target = make_target(&instance_data).expect("Should create target for TAP+VSOCK");
        assert_eq!(target.nodename, "emu-tap-vsock");
        assert_eq!(target.addresses, vec![EmulatorAddr::Vsock { cid: 42 }]);
    }

    #[test]
    fn test_make_target_user_with_vsock_and_ssh() {
        let mut instance_data = crate::EmulatorInstanceData::new_with_state(
            "emu-user-vsock-ssh",
            crate::EngineState::Running,
        );
        instance_data.set_pid(std::process::id());
        let config = instance_data.get_emulator_configuration_mut();
        config.host.networking = crate::NetworkingMode::User;
        config
            .host
            .port_map
            .insert(String::from("ssh"), crate::PortMapping { guest: 22, host: Some(8022) });
        config.device.vsock = Some(crate::VsockDevice { enabled: true, cid: 99 });

        let target = make_target(&instance_data).expect("Should create target for User+VSOCK+SSH");
        assert_eq!(target.nodename, "emu-user-vsock-ssh");
        assert!(target.addresses.contains(&EmulatorAddr::Vsock { cid: 99 }));
        assert!(target.addresses.contains(&EmulatorAddr::LoopbackPort(8022)));
    }
}
