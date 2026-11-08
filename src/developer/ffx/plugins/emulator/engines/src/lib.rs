// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The ffx_emulator_engines crate contains the implementation
//! of each emulator "engine" such as aemu and qemu.

mod arg_templates;
mod qemu_based;
pub mod serialization;
mod show_output;

pub use arg_templates::{process_flag_template, process_flag_template_with_context};
use emulator_instance::{
    DeviceConfig, EmulatorConfiguration, EmulatorInstanceData, EmulatorInstanceInfo,
    EmulatorInstances, EngineOption, EngineState, EngineType, FlagData, GuestConfig, HostConfig,
    RuntimeConfig, SerialMode, read_from_disk,
};
use errors::ffx_bail;
use ffx_config::EnvironmentContext;
use ffx_emulator_common::config::EMU_SERIAL_ENABLED;
use ffx_emulator_config::EmulatorEngine;
use fho::{Result, bug, return_user_error};
use qemu_based::crosvm::CrosvmEngine;
use qemu_based::femu::FemuEngine;
use qemu_based::qemu::QemuEngine;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// The EngineBuilder is used to create and configure an EmulatorEngine, while ensuring the
/// configuration will result in a valid emulation instance.
///
/// Create an EngineBuilder using EngineBuilder::new(). This will populate the builder with the
/// defaults for all configuration options. Then use the setter methods to update configuration
/// options, and call "build()" when configuration is complete.
///
/// Setters are independent, optional, and idempotent; i.e. callers may call as many or as few of
/// the setters as needed, and repeat calls if necessary. However, setters consume the data that
/// are passed in, so the caller must set up a new structure for each call.
///
/// Once "build" is called, an engine will be instantiated of the indicated type, the configuration
/// will be loaded into that engine, and the engine's "configure" function will be invoked to
/// trigger validation and ensure the configuration is acceptable. If validation fails, the engine
/// will be destroyed. The EngineBuilder instance is consumed when invoking "build" regardless of
/// the outcome.
///
/// Example:
///
///    let builder = EngineBuilder::new()
///         .engine_type(EngineType::Femu)
///         .device(my_device_config)
///         .guest(my_guest_config)
///         .host(my_host_config)
///         .runtime(my_runtime_config);
///
///     let mut engine: Box<dyn EmulatorEngine> = builder.build()?;
///     (*engine).start().await
///
pub struct EngineBuilder {
    context: EnvironmentContext,
    emulator_configuration: EmulatorConfiguration,
    engine_type: EngineType,
    emu_instances: EmulatorInstances,
    serial_enabled: Option<bool>,
}

impl EngineBuilder {
    /// Create a new EngineBuilder, populated with default values for all configuration.
    pub fn new(context: &EnvironmentContext, emu_instances: EmulatorInstances) -> Self {
        Self {
            context: context.clone(),
            emulator_configuration: EmulatorConfiguration::default(),
            engine_type: EngineType::default(),
            emu_instances,
            serial_enabled: None,
        }
    }

    /// Create a new EngineBuilder using the emulator instance directories from `context`.
    pub fn from_env(context: &EnvironmentContext) -> Result<Self> {
        let dirs: Vec<PathBuf> =
            context.get(ffx_config::keys::EMU_INSTANCE_ROOT_DIR).map_err(|e| bug!("{e}"))?;
        Ok(Self::new(context, EmulatorInstances::from(dirs)))
    }

    /// Set the configuration to use when building a new engine.
    pub fn config(mut self, config: EmulatorConfiguration) -> EngineBuilder {
        self.emulator_configuration = config;
        self
    }

    /// Override the "emu.serial_number.enabled" config setting.
    pub fn serial_enabled(mut self, enabled: bool) -> EngineBuilder {
        self.serial_enabled = Some(enabled);
        self
    }

    /// Set the engine's virtual device configuration.
    pub fn device(mut self, device_config: DeviceConfig) -> EngineBuilder {
        self.emulator_configuration.device = device_config;
        self
    }

    /// Set the type of the engine to be built.
    pub fn engine_type(mut self, engine_type: EngineType) -> EngineBuilder {
        self.engine_type = engine_type;
        self
    }

    /// Set the engine's guest configuration.
    pub fn guest(mut self, guest_config: GuestConfig) -> EngineBuilder {
        self.emulator_configuration.guest = guest_config;
        self
    }

    /// Set the engine's host configuration.
    pub fn host(mut self, host_config: HostConfig) -> EngineBuilder {
        self.emulator_configuration.host = host_config;
        self
    }

    /// Set the engine's runtime configuration.
    pub fn runtime(mut self, runtime_config: RuntimeConfig) -> EngineBuilder {
        self.emulator_configuration.runtime = runtime_config;
        self
    }

    /// Create from an existing EmulatorInstanceData,
    /// Does not validate or perform any configuration steps. Call
    /// |build| for those steps to be performed.
    pub fn from_data(&self, data: EmulatorInstanceData) -> Box<dyn EmulatorEngine> {
        match data.get_engine_type() {
            EngineType::Femu => {
                Box::new(FemuEngine::new(&self.context, data, self.emu_instances.clone()))
            }
            EngineType::Qemu => {
                Box::new(QemuEngine::new(&self.context, data, self.emu_instances.clone()))
            }
            EngineType::Crosvm => {
                Box::new(CrosvmEngine::new(&self.context, data, self.emu_instances.clone()))
            }
        }
    }

    /// Finalize and validate the configuration, set up the engine's instance directory,
    /// and return the built engine.
    pub async fn build(mut self) -> Result<Box<dyn EmulatorEngine>> {
        // Set up the instance directory, now that we have enough information.
        let name = self.emulator_configuration.runtime.name.clone();
        self.emulator_configuration.runtime.engine_type = self.engine_type;

        // Make sure we don't overwrite an existing instance.
        let existing_dir =
            self.emu_instances.get_instance_dir(&name, false).map_err(anyhow::Error::from)?;
        if let Ok(EngineOption::DoesExist(instance_data)) = read_from_disk(&existing_dir) {
            if instance_data.is_running() {
                return_user_error!(
                    "An emulator named {} is already running. \
                    Use a different name, or run `ffx emu stop {}` \
                    to stop the running emulator.",
                    name,
                    name
                );
            }
        }

        let instance_dir =
            self.emu_instances.get_instance_dir(&name, true).map_err(anyhow::Error::from)?;
        self.emulator_configuration.runtime.instance_directory = instance_dir.clone();

        self.configure_short_sockets(&instance_dir);

        let serial_enabled = self
            .serial_enabled
            .unwrap_or_else(|| self.context.get(EMU_SERIAL_ENABLED).unwrap_or(true));
        update_serial_number(&mut self.emulator_configuration, serial_enabled);

        // Build and complete configuration on the engine, then pass it back to the caller.
        let instance_data = EmulatorInstanceData::new(
            self.emulator_configuration.clone(),
            self.engine_type,
            EngineState::Configured,
        );

        let mut engine: Box<dyn EmulatorEngine> = self.from_data(instance_data);
        engine.configure()?;

        engine.load_emulator_binary()?;

        engine.emu_config_mut().flags =
            process_flag_template_with_context(engine.emu_config(), &self.context).map_err(
                |e| bug!("Engine builder failed to process the flags template file: {e}"),
            )?;
        engine.save_to_disk().await?;

        Ok(engine)
    }

    fn configure_short_sockets(&mut self, instance_dir: &Path) {
        // UNIX domain socket paths are limited to 108 characters on Linux (sockaddr_un.sun_path).
        // In isolated test environments (e.g. on LUCI builders), the instance directory path
        // can be extremely deep, exceeding this limit and causing the emulator to fail to bind
        // its sockets.
        // To prevent this, we generate unique socket paths in the system temporary directory
        // (which is typically short and writeable) and store them in the emulator configuration
        // (engine.json). Clients read this configuration to connect directly to the short paths,
        // completely bypassing the limit without requiring fragile symlinks. We use a hash of
        // the instance directory and the user's UID to ensure the socket paths are unique and
        // do not conflict between multiple users or different emulator instances.
        let mut hasher = DefaultHasher::new();
        instance_dir.hash(&mut hasher);
        nix::unistd::getuid().hash(&mut hasher);
        let hash_val = hasher.finish();
        let set_short_socket = |socket: &mut PathBuf, prefix: &str| {
            if socket.as_os_str().is_empty() {
                *socket = std::env::temp_dir().join(format!("femu_{prefix}_{hash_val:x}"));
            }
        };

        let uart_enabled = qemu_based::is_uart_enabled(&self.emulator_configuration);
        let runtime = &mut self.emulator_configuration.runtime;
        match self.engine_type {
            EngineType::Qemu | EngineType::Femu => {
                if uart_enabled {
                    set_short_socket(&mut runtime.serial_socket, "s");
                }
                // Custom flag templates may place the QEMU control sockets anywhere (typically
                // in the instance directory), so only the built-in templates use the short paths.
                if runtime.template.is_none() {
                    for (socket, prefix) in [
                        (&mut runtime.monitor_socket, "m"),
                        (&mut runtime.qmp_socket, "q"),
                        (&mut runtime.console_socket, "c"),
                    ] {
                        set_short_socket(socket, prefix);
                    }
                }
            }
            EngineType::Crosvm => {
                // Custom flag templates pass their own flags; only built-in staging uses
                // `control_socket`.
                if runtime.template.is_none() {
                    set_short_socket(&mut runtime.control_socket, "ctrl");
                }
            }
        }
    }

    /// Returns the EmulatorEngine instance based on the name.
    /// If `name` is None:
    ///    - If no emulator instances are found, returns Ok(None).
    ///    - If exactly 1 instance is found, returns Ok(Some(engine)) and updates the `name` parameter.
    ///    - If multiple instances are found, returns an error.
    /// If `name` is Some:
    ///    - Returns Ok(Some(engine)) if the instance exists.
    ///    - Returns Ok(None) if the instance does not exist.
    pub fn get_engine_by_name(
        &self,
        name: &mut Option<String>,
    ) -> Result<Option<Box<dyn EmulatorEngine>>> {
        if name.is_none() {
            let all_instances = match self.emu_instances.get_all_instances() {
                Ok(list) => list,
                Err(e) => {
                    ffx_bail!("Error encountered looking up emulator instances: {e:?}");
                }
            };

            let running_instances: Vec<_> = all_instances
                .iter()
                .filter(|i| i.get_engine_state() == EngineState::Running)
                .collect();

            if running_instances.len() == 1 {
                *name = Some(running_instances[0].get_name().to_string());
            } else if running_instances.len() > 1 {
                return_user_error!(
                    "Multiple running emulators found. Indicate which emulator to access\n\
                by specifying the emulator name with your command.\n\
                See all the emulators available using `ffx emu list`."
                );
            } else if all_instances.len() == 1 {
                *name = Some(all_instances[0].get_name().to_string());
            } else if all_instances.len() == 0 {
                log::debug!("No emulators found.");
                return Ok(None);
            } else {
                return_user_error!(
                    "Multiple emulators found but none are running. Indicate which emulator to access\n\
                by specifying the emulator name with your command.\n\
                See all the emulators available using `ffx emu list`."
                );
            }
        }

        // If we got this far, name is set to either what the user asked for, or the only one running.
        if let Some(local_name) = name {
            let instance_dir = self
                .emu_instances
                .get_instance_dir(local_name, false)
                .map_err(|e| anyhow::Error::from(e))?;
            match read_from_disk(&instance_dir).map_err(|e| anyhow::Error::from(e))? {
                EngineOption::DoesExist(data) => Ok(Some(self.from_data(*data))),
                EngineOption::DoesNotExist(_) => Ok(None),
            }
        } else {
            ffx_bail!("No emulator instances found")
        }
    }
}

// Given the string representation of a flag template, apply the provided configuration to resolve
// the template into a FlagData object.
pub fn process_flags_from_str(text: &str, emu_config: &EmulatorConfiguration) -> Result<FlagData> {
    arg_templates::process_flags_from_str(text, emu_config)
        .map_err(|e| bug!("Error processing flags: {e}"))
}

pub fn process_flags_from_str_with_context(
    text: &str,
    emu_config: &EmulatorConfiguration,
    context: &EnvironmentContext,
) -> Result<FlagData> {
    arg_templates::process_flags_from_str_with_context(text, emu_config, context)
        .map_err(|e| bug!("Error processing flags: {e}"))
}

fn generate_serial() -> String {
    format!("EM-{:09X}", rand::random::<u64>() & 0xFFFFFFFFF)
}

/// Updates the serial number configuration based on the `emu.serial_number.enabled` setting.
///
/// Handles generation of new serial numbers, preservation of existing stable serials,
/// and transition of the serial number state if the config changes.
pub fn update_serial_number(config: &mut EmulatorConfiguration, serial_enabled: bool) {
    let runtime = &mut config.runtime;
    match &runtime.serial_number {
        SerialMode::Uninitialized => {
            if serial_enabled {
                runtime.serial_number = SerialMode::Enabled(generate_serial());
            } else {
                runtime.serial_number = SerialMode::Disabled;
            }
        }
        SerialMode::Enabled(_) => {
            if !serial_enabled {
                runtime.serial_number = SerialMode::Disabled;
            }
        }
        SerialMode::Disabled => {
            if serial_enabled {
                runtime.serial_number = SerialMode::Enabled(generate_serial());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_serial_number_generation() {
        use tempfile::tempdir;

        let builder = ffx_config::test_env();
        let builder = crate::qemu_based::tests::make_fake_sdk(builder).await;
        let env = builder.build().expect("test env");
        let temp_dir = tempdir().expect("Couldn't get a temporary directory for testing.");
        let emu_instances = EmulatorInstances::new(temp_dir.path().to_path_buf());

        // 1. Default: Serial number generation is enabled.
        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-emu-1".to_string();
        cfg.runtime.config_override = true;
        let builder = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg)
            .engine_type(EngineType::Qemu);

        let engine = builder.build().await.expect("engine built");
        assert!(engine.emu_config().runtime.serial_socket.as_os_str().is_empty());
        // The QEMU control sockets are kept short regardless of the instance directory's length.
        let runtime = &engine.emu_config().runtime;
        for (socket, prefix) in [
            (&runtime.monitor_socket, "femu_m_"),
            (&runtime.qmp_socket, "femu_q_"),
            (&runtime.console_socket, "femu_c_"),
        ] {
            let short_prefix = std::env::temp_dir().join(prefix);
            assert!(socket.to_string_lossy().starts_with(&*short_prefix.to_string_lossy()));
        }
        assert!(runtime.control_socket.as_os_str().is_empty());

        // Custom flag templates choose their own socket paths, so these stay unset.
        let mut cfg_template = EmulatorConfiguration::default();
        cfg_template.runtime.name = "test-emu-template".to_string();
        cfg_template.runtime.config_override = true;
        let template_path = temp_dir.path().join("custom.json.template");
        std::fs::write(
            &template_path,
            r#"{"args": [], "envs": {}, "features": [], "kernel_args": [], "options": []}"#,
        )
        .expect("write template");
        cfg_template.runtime.template = Some(template_path);
        let engine_template = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg_template)
            .engine_type(EngineType::Qemu)
            .build()
            .await
            .expect("custom template engine built");
        let runtime = &engine_template.emu_config().runtime;
        assert!(runtime.monitor_socket.as_os_str().is_empty());
        assert!(runtime.qmp_socket.as_os_str().is_empty());
        assert!(runtime.console_socket.as_os_str().is_empty());
        let serial = match &engine.emu_config().runtime.serial_number {
            SerialMode::Enabled(s) => s,
            _ => panic!(
                "expected serial to be generated, got {:?}",
                engine.emu_config().runtime.serial_number
            ),
        };
        assert_eq!(serial.len(), 12);
        assert!(serial.starts_with("EM-"));
        assert!(
            serial[3..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && (c.is_numeric() || c.is_uppercase()))
        );

        let mut cfg_uart = EmulatorConfiguration::default();
        cfg_uart.runtime.name = "test-emu-uart".to_string();
        cfg_uart.runtime.config_override = true;
        cfg_uart.runtime.addl_kernel_args = vec!["dev.fdomain.uart=true".to_string()];
        let engine_uart = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg_uart)
            .engine_type(EngineType::Qemu)
            .build()
            .await
            .expect("uart engine built");
        assert!(
            engine_uart
                .emu_config()
                .runtime
                .serial_socket
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("femu_s_"))
        );

        // 2. Disabled: Config is false.
        let builder_disabled_env = ffx_config::test_env();
        let builder_disabled_env =
            crate::qemu_based::tests::make_fake_sdk(builder_disabled_env).await;
        let env_disabled = builder_disabled_env
            .user_config(EMU_SERIAL_ENABLED, "false")
            .build()
            .expect("test env disabled");
        let mut cfg_disabled = EmulatorConfiguration::default();
        cfg_disabled.runtime.name = "test-emu-2".to_string();
        cfg_disabled.runtime.config_override = true;
        let builder_disabled = EngineBuilder::new(&env_disabled.context, emu_instances.clone())
            .config(cfg_disabled)
            .engine_type(EngineType::Qemu);

        let engine_disabled = builder_disabled.build().await.expect("engine built");
        assert_eq!(engine_disabled.emu_config().runtime.serial_number, SerialMode::Disabled);

        // 3. Pre-existing: Some("custom-serial") is not overwritten.
        let mut cfg_custom = EmulatorConfiguration::default();
        cfg_custom.runtime.name = "test-emu-3".to_string();
        cfg_custom.runtime.serial_number = SerialMode::Enabled("custom-serial".to_string());
        cfg_custom.runtime.config_override = true;
        let builder_custom = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg_custom)
            .engine_type(EngineType::Qemu);

        let engine_custom = builder_custom.build().await.expect("engine built");
        assert_eq!(
            engine_custom.emu_config().runtime.serial_number,
            SerialMode::Enabled("custom-serial".to_string())
        );
    }

    #[fuchsia::test]
    async fn test_crosvm_control_socket_paths() {
        use tempfile::tempdir;

        let builder = ffx_config::test_env();
        let builder = crate::qemu_based::tests::make_fake_sdk(builder).await;
        let env = builder.build().expect("test env");
        let temp_dir = tempdir().expect("cannot get tempdir");
        let emu_instances = EmulatorInstances::new(temp_dir.path().to_path_buf());

        let mut cfg = EmulatorConfiguration::default();
        cfg.runtime.name = "test-crosvm".to_string();
        cfg.runtime.config_override = true;
        let engine = EngineBuilder::new(&env.context, emu_instances.clone())
            .config(cfg)
            .engine_type(EngineType::Crosvm)
            .build()
            .await
            .expect("crosvm engine built");

        let runtime = &engine.emu_config().runtime;
        let short_prefix = std::env::temp_dir().join("femu_ctrl_");
        assert!(
            runtime.control_socket.to_string_lossy().starts_with(&*short_prefix.to_string_lossy())
        );
        assert!(runtime.serial_socket.as_os_str().is_empty());
        assert!(runtime.monitor_socket.as_os_str().is_empty());
        assert!(runtime.qmp_socket.as_os_str().is_empty());
        assert!(runtime.console_socket.as_os_str().is_empty());

        let template_path = temp_dir.path().join("custom.json.template");
        std::fs::write(
            &template_path,
            r#"{"args": [], "envs": {}, "features": [], "kernel_args": [], "options": []}"#,
        )
        .expect("write template");
        let mut cfg_template = EmulatorConfiguration::default();
        cfg_template.runtime.name = "test-crosvm-template".to_string();
        cfg_template.runtime.config_override = true;
        cfg_template.runtime.template = Some(template_path);
        let engine_template = EngineBuilder::new(&env.context, emu_instances)
            .config(cfg_template)
            .engine_type(EngineType::Crosvm)
            .build()
            .await
            .expect("custom template crosvm engine built");
        assert!(engine_template.emu_config().runtime.control_socket.as_os_str().is_empty());
    }
}
