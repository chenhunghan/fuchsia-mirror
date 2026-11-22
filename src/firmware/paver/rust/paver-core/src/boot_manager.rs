// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{PaverConfig, ResponderExt as _};
use anyhow::{Context as _, Error};
use async_trait::async_trait;
use fidl_next::{Request, Responder};
use fidl_next_fuchsia_paver::{
    self as fpaver, BootManagerLocalServerHandler,
    BootManagerQueryConfigurationStatusAndBootAttemptsResponse, Configuration, ConfigurationStatus,
    boot_manager,
};

/// Trait representing the `fuchsia.paver.BootManager` FIDL protocol.
#[async_trait(?Send)]
pub trait BootManager {
    /// Queries the configuration the system is currently running (`A`, `B`, or `Recovery`).
    async fn query_current_configuration(&mut self) -> Result<Configuration, zx::Status>;

    /// Queries the configuration that will be selected by default on a normal cold boot.
    ///
    /// Returns `Err(zx::Status::NOT_SUPPORTED)` if no A/B configuration is currently active
    /// (for example, if both slots are unbootable and the device would fall back to recovery).
    async fn query_active_configuration(&mut self) -> Result<Configuration, zx::Status>;

    /// Queries the configuration that was most recently marked active via
    /// [`set_configuration_active`](Self::set_configuration_active), regardless of whether that
    /// slot is currently bootable.
    async fn query_configuration_last_set_active(&mut self) -> Result<Configuration, zx::Status>;

    /// Queries the boot status (`Healthy`, `Pending`, or `Unbootable`) of `configuration`.
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `configuration` is `Configuration::Recovery`.
    async fn query_configuration_status(
        &mut self,
        configuration: Configuration,
    ) -> Result<ConfigurationStatus, zx::Status>;

    /// Queries the boot status, elapsed pending boot attempts, and unbootable reason for
    /// `configuration`.
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `configuration` is `Configuration::Recovery`.
    async fn query_configuration_status_and_boot_attempts(
        &mut self,
        configuration: Configuration,
    ) -> Result<BootManagerQueryConfigurationStatusAndBootAttemptsResponse, zx::Status>;

    /// Marks `configuration` as the active boot slot, resets its retry counter and unbootable/
    /// successful flags (`Pending`), and clears the active flag on the opposite slot.
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `configuration` is `Configuration::Recovery`.
    async fn set_configuration_active(
        &mut self,
        configuration: Configuration,
    ) -> Result<(), zx::Status>;

    /// Marks `configuration` as unbootable.
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `configuration` is `Configuration::Recovery`.
    async fn set_configuration_unbootable(
        &mut self,
        configuration: Configuration,
    ) -> Result<(), zx::Status>;

    /// Marks `configuration` as healthy (`Successful`) and clears any healthy mark on the
    /// opposite slot.
    ///
    /// Returns `Err(zx::Status::INVALID_ARGS)` if `configuration` is `Configuration::Recovery` or
    /// is currently unbootable.
    async fn set_configuration_healthy(
        &mut self,
        configuration: Configuration,
    ) -> Result<(), zx::Status>;

    /// Requests that the bootloader boot into recovery on the next reboot only, without modifying
    /// A/B slot state.
    async fn set_one_shot_recovery(&mut self) -> Result<(), zx::Status>;

    /// Flushes any buffered A/B/R metadata changes to persistent storage.
    async fn flush(&mut self) -> Result<(), zx::Status> {
        Ok(())
    }
}

impl BootManagerLocalServerHandler for Box<dyn BootManager> {
    async fn query_current_configuration(
        &mut self,
        responder: Responder<boot_manager::QueryCurrentConfiguration>,
    ) {
        let res = (**self).query_current_configuration().await;
        responder.send_result(res).await;
    }

    async fn query_active_configuration(
        &mut self,
        responder: Responder<boot_manager::QueryActiveConfiguration>,
    ) {
        let res = (**self).query_active_configuration().await;
        responder.send_result(res).await;
    }

    async fn query_configuration_last_set_active(
        &mut self,
        responder: Responder<boot_manager::QueryConfigurationLastSetActive>,
    ) {
        let res = (**self).query_configuration_last_set_active().await;
        responder.send_result(res).await;
    }

    async fn query_configuration_status(
        &mut self,
        request: Request<boot_manager::QueryConfigurationStatus>,
        responder: Responder<boot_manager::QueryConfigurationStatus>,
    ) {
        let res = (**self).query_configuration_status(request.payload().configuration).await;
        responder.send_result(res).await;
    }

    async fn query_configuration_status_and_boot_attempts(
        &mut self,
        request: Request<boot_manager::QueryConfigurationStatusAndBootAttempts>,
        responder: Responder<boot_manager::QueryConfigurationStatusAndBootAttempts>,
    ) {
        let res = (**self)
            .query_configuration_status_and_boot_attempts(request.payload().configuration)
            .await;
        responder.send_result(res).await;
    }

    async fn set_configuration_active(
        &mut self,
        request: Request<boot_manager::SetConfigurationActive>,
        responder: Responder<boot_manager::SetConfigurationActive>,
    ) {
        let res = (**self).set_configuration_active(request.payload().configuration).await;
        responder.send_response(res).await;
    }

    async fn set_configuration_unbootable(
        &mut self,
        request: Request<boot_manager::SetConfigurationUnbootable>,
        responder: Responder<boot_manager::SetConfigurationUnbootable>,
    ) {
        let res = (**self).set_configuration_unbootable(request.payload().configuration).await;
        responder.send_response(res).await;
    }

    async fn set_configuration_healthy(
        &mut self,
        request: Request<boot_manager::SetConfigurationHealthy>,
        responder: Responder<boot_manager::SetConfigurationHealthy>,
    ) {
        let res = (**self).set_configuration_healthy(request.payload().configuration).await;
        responder.send_response(res).await;
    }

    async fn set_one_shot_recovery(
        &mut self,
        responder: Responder<boot_manager::SetOneShotRecovery>,
    ) {
        let res = (**self).set_one_shot_recovery().await;
        responder.send_result(res).await;
    }

    async fn flush(&mut self, responder: Responder<boot_manager::Flush>) {
        let res = (**self).flush().await;
        responder.send_response(res).await;
    }
}

/// Serves the `fuchsia.paver.BootManager` FIDL protocol over `server_end` using `manager`.
pub async fn serve_boot_manager<B: BootManagerLocalServerHandler>(
    manager: B,
    server_end: fidl_next::ServerEnd<fpaver::BootManager>,
) -> Result<(), Error> {
    fidl_next::ServerDispatcher::new(server_end)
        .run_local(manager)
        .await
        .map(|_| ())
        .context("serving BootManager")
}

fn parse_slot_str(slot: &str) -> Result<Configuration, zx::Status> {
    let trimmed = slot.strip_prefix(['_', '-']).unwrap_or(slot);
    if trimmed.eq_ignore_ascii_case("a") {
        Ok(Configuration::A)
    } else if trimmed.eq_ignore_ascii_case("b") {
        Ok(Configuration::B)
    } else if trimmed.eq_ignore_ascii_case("r") {
        Ok(Configuration::Recovery)
    } else {
        log::error!(slot; "Invalid boot slot string");
        Err(zx::Status::NOT_SUPPORTED)
    }
}

/// Parses the currently booted configuration (`A`, `B`, or `Recovery`) from [`PaverConfig`]
/// (`zvb_current_slot` or `android_boot_slot_suffix`).
///
/// Returns `Err(zx::Status::NOT_SUPPORTED)` if neither field is set or if the configured slot
/// string is invalid.
///
/// Note: `fuchsia.zvb.boot-partition-uuid` is intentionally not included in [`PaverConfig`];
/// `paver.cml` constrains that field to `max_size: 20` (too short for a 36-character UUID string),
/// and all supported bootloaders pass either `zvb.current_slot` or `androidboot.slot_suffix`.
pub fn parse_current_configuration(config: &PaverConfig) -> Result<Configuration, zx::Status> {
    if !config.zvb_current_slot.is_empty() {
        return parse_slot_str(&config.zvb_current_slot);
    }
    if !config.android_boot_slot_suffix.is_empty() {
        return parse_slot_str(&config.android_boot_slot_suffix);
    }
    log::warn!("System does not have zvb_current_slot or android_boot_slot_suffix configured");
    Err(zx::Status::NOT_SUPPORTED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_next_fuchsia_paver::UnbootableReason;
    use fuchsia_async as fasync;
    use std::cell::Cell;
    use std::rc::Rc;

    #[fuchsia::test]
    fn test_parse_current_configuration() {
        let mut cfg =
            PaverConfig { android_boot_slot_suffix: "_a".to_string(), ..Default::default() };
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::A));

        cfg.android_boot_slot_suffix = "b".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::B));

        cfg.android_boot_slot_suffix = "_B".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::B));

        cfg.android_boot_slot_suffix = "_r".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::Recovery));

        cfg.android_boot_slot_suffix = "-a".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::A));

        cfg.android_boot_slot_suffix = "__a".to_string();
        assert_eq!(parse_current_configuration(&cfg), Err(zx::Status::NOT_SUPPORTED));

        cfg.android_boot_slot_suffix = "_x".to_string();
        assert_eq!(parse_current_configuration(&cfg), Err(zx::Status::NOT_SUPPORTED));

        cfg.android_boot_slot_suffix = "_".to_string();
        assert_eq!(parse_current_configuration(&cfg), Err(zx::Status::NOT_SUPPORTED));

        // `zvb_current_slot` takes precedence over `android_boot_slot_suffix` (even when invalid).
        cfg.android_boot_slot_suffix = "_a".to_string();
        cfg.zvb_current_slot = "invalid".to_string();
        assert_eq!(parse_current_configuration(&cfg), Err(zx::Status::NOT_SUPPORTED));

        cfg.zvb_current_slot = "-b".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::B));

        cfg.zvb_current_slot = "_A".to_string();
        assert_eq!(parse_current_configuration(&cfg), Ok(Configuration::A));

        // Both empty returns NOT_SUPPORTED.
        cfg.zvb_current_slot.clear();
        cfg.android_boot_slot_suffix.clear();
        assert_eq!(parse_current_configuration(&cfg), Err(zx::Status::NOT_SUPPORTED));
    }

    struct TestTraitBootManager {
        current: Configuration,
        active: Configuration,
        last_active: Configuration,
        status_a: ConfigurationStatus,
        one_shot_recovery: Rc<Cell<bool>>,
        flushes: Rc<Cell<u32>>,
    }

    #[async_trait(?Send)]
    impl BootManager for TestTraitBootManager {
        async fn query_current_configuration(&mut self) -> Result<Configuration, zx::Status> {
            Ok(self.current)
        }

        async fn query_active_configuration(&mut self) -> Result<Configuration, zx::Status> {
            Ok(self.active)
        }

        async fn query_configuration_last_set_active(
            &mut self,
        ) -> Result<Configuration, zx::Status> {
            Ok(self.last_active)
        }

        async fn query_configuration_status(
            &mut self,
            configuration: Configuration,
        ) -> Result<ConfigurationStatus, zx::Status> {
            match configuration {
                Configuration::A => Ok(self.status_a),
                Configuration::B => Ok(ConfigurationStatus::Unbootable),
                Configuration::Recovery => Err(zx::Status::INVALID_ARGS),
            }
        }

        async fn query_configuration_status_and_boot_attempts(
            &mut self,
            configuration: Configuration,
        ) -> Result<BootManagerQueryConfigurationStatusAndBootAttemptsResponse, zx::Status>
        {
            match configuration {
                Configuration::A => {
                    Ok(BootManagerQueryConfigurationStatusAndBootAttemptsResponse {
                        status: Some(self.status_a),
                        boot_attempts: Some(2),
                        unbootable_reason: None,
                    })
                }
                Configuration::B => {
                    Ok(BootManagerQueryConfigurationStatusAndBootAttemptsResponse {
                        status: Some(ConfigurationStatus::Unbootable),
                        boot_attempts: None,
                        unbootable_reason: Some(UnbootableReason::VerificationFailure),
                    })
                }
                Configuration::Recovery => Err(zx::Status::INVALID_ARGS),
            }
        }

        async fn set_configuration_active(
            &mut self,
            configuration: Configuration,
        ) -> Result<(), zx::Status> {
            if configuration == Configuration::Recovery {
                return Err(zx::Status::INVALID_ARGS);
            }
            self.active = configuration;
            self.last_active = configuration;
            Ok(())
        }

        async fn set_configuration_unbootable(
            &mut self,
            configuration: Configuration,
        ) -> Result<(), zx::Status> {
            if configuration == Configuration::Recovery {
                return Err(zx::Status::INVALID_ARGS);
            }
            if configuration == Configuration::A {
                self.status_a = ConfigurationStatus::Unbootable;
            }
            Ok(())
        }

        async fn set_configuration_healthy(
            &mut self,
            configuration: Configuration,
        ) -> Result<(), zx::Status> {
            if configuration == Configuration::Recovery {
                return Err(zx::Status::INVALID_ARGS);
            }
            if configuration == Configuration::A {
                self.status_a = ConfigurationStatus::Healthy;
            }
            Ok(())
        }

        async fn set_one_shot_recovery(&mut self) -> Result<(), zx::Status> {
            self.one_shot_recovery.set(true);
            Ok(())
        }

        async fn flush(&mut self) -> Result<(), zx::Status> {
            self.flushes.set(self.flushes.get() + 1);
            Ok(())
        }
    }

    /// Verifies the `BootManagerLocalServerHandler` adapter for `Box<dyn BootManager>` across all
    /// 10 `fuchsia.paver.BootManager` FIDL methods, including error propagation.
    #[fuchsia::test]
    async fn test_box_dyn_boot_manager_server_handler() {
        let scope = fasync::Scope::new_with_name("test_box_dyn_bm");
        let (client_end, server_end) = fidl_next::fuchsia::create_channel::<fpaver::BootManager>();
        let client = client_end.spawn();

        let one_shot_recovery = Rc::new(Cell::new(false));
        let flushes = Rc::new(Cell::new(0));
        let boxed: Box<dyn BootManager> = Box::new(TestTraitBootManager {
            current: Configuration::A,
            active: Configuration::A,
            last_active: Configuration::B,
            status_a: ConfigurationStatus::Pending,
            one_shot_recovery: one_shot_recovery.clone(),
            flushes: flushes.clone(),
        });
        scope.spawn_local(async move {
            serve_boot_manager(boxed, server_end).await.expect("serve Box<dyn BootManager>");
        });

        assert_eq!(
            client.query_current_configuration().await.expect("fidl").map(|r| r.configuration),
            Ok(Configuration::A)
        );
        assert_eq!(
            client.query_active_configuration().await.expect("fidl").map(|r| r.configuration),
            Ok(Configuration::A)
        );
        assert_eq!(
            client
                .query_configuration_last_set_active()
                .await
                .expect("fidl")
                .map(|r| r.configuration),
            Ok(Configuration::B)
        );
        assert_eq!(
            client
                .query_configuration_status(Configuration::A)
                .await
                .expect("fidl")
                .map(|r| r.status),
            Ok(ConfigurationStatus::Pending)
        );
        assert_eq!(
            client
                .query_configuration_status(Configuration::Recovery)
                .await
                .expect("fidl")
                .map(|r| r.status),
            Err(zx::Status::INVALID_ARGS)
        );
        let attempts_b = client
            .query_configuration_status_and_boot_attempts(Configuration::B)
            .await
            .expect("fidl")
            .expect("ok");
        assert_eq!(attempts_b.status, Some(ConfigurationStatus::Unbootable));
        assert_eq!(attempts_b.boot_attempts, None);
        assert_eq!(attempts_b.unbootable_reason, Some(UnbootableReason::VerificationFailure));
        assert_eq!(
            client
                .query_configuration_status_and_boot_attempts(Configuration::Recovery)
                .await
                .expect("fidl")
                .err(),
            Some(zx::Status::INVALID_ARGS)
        );

        assert_eq!(
            client.set_configuration_active(Configuration::B).await.expect("fidl").status,
            Ok(())
        );
        assert_eq!(
            client.set_configuration_active(Configuration::Recovery).await.expect("fidl").status,
            Err(zx::Status::INVALID_ARGS)
        );
        assert_eq!(
            client.query_active_configuration().await.expect("fidl").map(|r| r.configuration),
            Ok(Configuration::B)
        );
        assert_eq!(
            client.set_configuration_healthy(Configuration::A).await.expect("fidl").status,
            Ok(())
        );
        assert_eq!(
            client
                .query_configuration_status(Configuration::A)
                .await
                .expect("fidl")
                .map(|r| r.status),
            Ok(ConfigurationStatus::Healthy)
        );
        assert_eq!(
            client.set_configuration_unbootable(Configuration::A).await.expect("fidl").status,
            Ok(())
        );
        assert_eq!(
            client
                .query_configuration_status(Configuration::A)
                .await
                .expect("fidl")
                .map(|r| r.status),
            Ok(ConfigurationStatus::Unbootable)
        );
        assert!(!one_shot_recovery.get());
        assert_eq!(client.set_one_shot_recovery().await.expect("fidl"), Ok(()));
        assert!(one_shot_recovery.get());
        assert_eq!(flushes.get(), 0);
        assert_eq!(client.flush().await.expect("fidl").status, Ok(()));
        assert_eq!(flushes.get(), 1);

        drop(client);
        scope.cancel().await;
    }
}
