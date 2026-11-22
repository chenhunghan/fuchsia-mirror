// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use discovery::{FastbootConnectionState, TargetHandle, TargetState};
use ffx_diagnostics::{Check, CheckFut, Notifier};
use ffx_diagnostics_analytics::{PointOfFailure, ResultExt};
use ffx_fastboot_connection_factory::{
    ConnectionFactory, FastbootConnectionFactory, FastbootConnectionKind,
};
use termio::Colors;

pub async fn check_fastboot_device<N>(
    context: &ffx_config::EnvironmentContext,
    notifier: &mut N,
    device: TargetHandle,
) -> fho::Result<()>
where
    N: Notifier + std::marker::Unpin,
{
    let factory = ConnectionFactory::new(context);
    check_fastboot_device_impl(&factory, notifier, device).await
}

pub(crate) async fn check_fastboot_device_impl<N, F>(
    factory: &F,
    notifier: &mut N,
    device: TargetHandle,
) -> fho::Result<()>
where
    N: Notifier + std::marker::Unpin,
    F: FastbootConnectionFactory,
{
    let (info, notifier) = FastbootDeviceStatus::new(factory)
        .check_with_notifier(device, notifier)
        .await
        .map_err(|e| fho::Error::User(e.into()))?;
    let colors = Colors::current();
    let safe_info = safe_string::TermSafe::from_str_escaped(&info);
    notifier.on_success(format!("Got device info: {}{safe_info}{}", colors.green, colors.reset))?;
    Ok(())
}

pub struct FastbootDeviceStatus<'a, F, N> {
    factory: &'a F,
    _w: std::marker::PhantomData<N>,
}

impl<'a, F, N> FastbootDeviceStatus<'a, F, N> {
    pub fn new(factory: &'a F) -> Self {
        Self { factory, _w: Default::default() }
    }
}

#[derive(thiserror::Error, Debug)]
#[error(
    "\nUnable to connect to Fastboot device. Consult {doc_link}\nUnderlying error: {underlying:#}"
)]
pub struct FastbootConnectionError {
    pub doc_link: String,
    #[source]
    pub underlying: anyhow::Error,
}

impl<F, N> Check for FastbootDeviceStatus<'_, F, N>
where
    N: Notifier + Sized,
    F: FastbootConnectionFactory,
{
    type Input = TargetHandle;
    type Output = String;
    type Notifier = N;

    fn write_preamble(
        &self,
        input: &Self::Input,
        notifier: &mut Self::Notifier,
    ) -> anyhow::Result<()> {
        notifier.info(format!("Attempting to connect to Fastboot device: {input:?}... "))
    }

    fn check<'a>(
        &'a mut self,
        input: Self::Input,
        _notifier: &'a mut Self::Notifier,
    ) -> CheckFut<'a, Self::Output> {
        Box::pin(async move {
            // Example handle: [TargetHandle { node_name: None, state: Fastboot(FastbootTargetState
            // { serial_number: "", connection_state: Tcp([TargetIpAddr(127.0.0.1:38957)]) }),
            // manual: false, origin: FastbootTcp }]
            let fastboot_state = match input.state {
                TargetState::Fastboot(ref s) => s,
                _ => {
                    ffx_diagnostics_analytics::mark_point_of_failure(
                        PointOfFailure::NonFastbootTargetHandle { handle: input.clone() },
                    )
                    .await;
                    return Err(anyhow::anyhow!("received non-fastboot target handle: {input:?}"));
                }
            };
            let (connection_kind, doc_section) = match &fastboot_state.connection_state {
                FastbootConnectionState::Usb => {
                    let discovery::TargetState::Fastboot(discovery::FastbootTargetState {
                        ref serial_number,
                        ..
                    }) = input.state
                    else {
                        panic!(
                            "input in incorrect state. Expecting Fastboot state instead found: {input:?}"
                        );
                    };
                    (
                        FastbootConnectionKind::Usb(serial_number.to_owned()),
                        "usb-fastboot-discovery",
                    )
                }
                // This assumes the first address in the array will a.) exist, and b.) be the _most
                // correct_ address from which we're selecting.
                FastbootConnectionState::Tcp(v) => (
                    FastbootConnectionKind::Tcp(
                        input.node_name.clone().unwrap_or_else(|| "".to_owned()),
                        v[0].into(),
                    ),
                    "fastboot-file-discovery",
                ),
                FastbootConnectionState::Udp(v) => (
                    FastbootConnectionKind::Udp(
                        input.node_name.clone().unwrap_or_else(|| "".to_owned()),
                        v[0].into(),
                    ),
                    "fastboot-file-discovery",
                ),
            };
            let doc_link = ffx_diagnostics_formatting::doc_link(&format!(
                "development/tools/ffx/workflows/network-connectivity/device-discovery#{doc_section}"
            ));
            // This error is covered within the fastboot library itself, so has no analytics.
            let mut interface =
                self.factory.build_interface(connection_kind).await.map_err(|e| {
                    FastbootConnectionError { doc_link: doc_link.clone(), underlying: e.into() }
                })?;
            interface
                .get_var("serialno")
                .await
                .or_analytics(PointOfFailure::FastbootQueryingSerialNo { handle: input.clone() })
                .await
                .map_err(|e| FastbootConnectionError { doc_link, underlying: e.into() }.into())
        })
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use addr::TargetIpAddr;
    use ffx_fastboot_connection_factory::test::setup_connection_factory;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[fuchsia::test]
    async fn test_fastboot_check_tcp() {
        let (state, factory) = setup_connection_factory();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234);
        let handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Tcp(vec![TargetIpAddr::from(addr)]),
            }),
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let fake_serial = "fake-serial-number".to_string();
        state.lock().unwrap().set_var("serialno".to_string(), fake_serial.clone());
        let mut check = FastbootDeviceStatus::new(&factory);
        let res = check.check(handle, &mut notifier).await.unwrap();
        assert_eq!(res, fake_serial);
    }

    #[fuchsia::test]
    async fn test_fastboot_check_udp() {
        let (state, factory) = setup_connection_factory();
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234);
        let handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Udp(vec![TargetIpAddr::from(addr)]),
            }),
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let fake_serial = "fake-serial-number".to_string();
        state.lock().unwrap().set_var("serialno".to_string(), fake_serial.clone());
        let mut check = FastbootDeviceStatus::new(&factory);
        let res = check.check(handle, &mut notifier).await.unwrap();
        assert_eq!(res, fake_serial);
    }

    #[fuchsia::test]
    async fn test_fastboot_check_usb() {
        let (state, factory) = setup_connection_factory();
        let handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let fake_serial = "fake-serial-number".to_string();
        state.lock().unwrap().set_var("serialno".to_string(), fake_serial.clone());
        let mut check = FastbootDeviceStatus::new(&factory);
        let res = check.check(handle, &mut notifier).await.unwrap();
        assert_eq!(res, fake_serial);
    }

    #[fuchsia::test]
    async fn test_fastboot_check_failure() {
        let (_state, factory) = setup_connection_factory();
        let handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Unknown,
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let mut check = FastbootDeviceStatus::new(&factory);
        let res = check.check(handle, &mut notifier).await;
        assert!(res.is_err());
    }

    #[fuchsia::test]
    async fn test_fastboot_check_connection_error_doc_links() {
        // By default, `setup_connection_factory()` leaves the fake fastboot variable map empty, so
        // `get_var("serialno")` fails with `FastbootError::GetVariableError`.
        let (_state, factory) = setup_connection_factory();
        let usb_handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let mut check = FastbootDeviceStatus::new(&factory);
        let err = check.check(usb_handle, &mut notifier).await.unwrap_err().to_string();
        assert!(
            err.contains("#usb-fastboot-discovery"),
            "Expected #usb-fastboot-discovery link in error: {err}"
        );

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 1234);
        let tcp_handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Tcp(vec![TargetIpAddr::from(addr)]),
            }),
            manual: false,
        };
        let err = check.check(tcp_handle, &mut notifier).await.unwrap_err().to_string();
        assert!(
            err.contains("#fastboot-file-discovery"),
            "Expected #fastboot-file-discovery link in error: {err}"
        );

        let udp_handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Udp(vec![TargetIpAddr::from(addr)]),
            }),
            manual: false,
        };
        let err = check.check(udp_handle, &mut notifier).await.unwrap_err().to_string();
        assert!(
            err.contains("#fastboot-file-discovery"),
            "Expected #fastboot-file-discovery link in error: {err}"
        );
    }

    #[fuchsia::test]
    async fn test_check_fastboot_device_escapes_control_characters() {
        let (state, factory) = setup_connection_factory();
        let handle = TargetHandle {
            node_name: Some("test-node".to_string()),
            state: TargetState::Fastboot(discovery::FastbootTargetState {
                serial_number: "test-serial".to_string(),
                connection_state: FastbootConnectionState::Usb,
            }),
            manual: false,
        };
        let mut notifier = ffx_diagnostics::StringNotifier::new();
        let malicious_serial = "serial\x1b[31m_evil\n\0".to_string();
        state.lock().unwrap().set_var("serialno".to_string(), malicious_serial);
        check_fastboot_device_impl(&factory, &mut notifier, handle).await.unwrap();

        let output: String = notifier.into();
        assert!(!output.contains('\x1b'));
        assert!(!output.contains('\0'));
        assert!(output.contains("serial\\u{1b}[31m_evil\\n\\u{0}"));
    }
}
