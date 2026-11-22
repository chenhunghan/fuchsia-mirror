// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdomain_fuchsia_kernel as fstats;
use fdomain_fuchsia_power_metrics::{self as fmetrics, CpuLoad, Metric};
use ffx_cpu_load_args as args_mod;
use ffx_writer::{ToolIO, VerifiedMachineWriter};
use fho::{Deferred, FfxMain, FfxTool, deferred};
use serde::Serialize;
use target_holders::{RemoteControlProxyHolder, moniker};

/// Machine-readable output for `ffx profile cpu-load`.
#[derive(Serialize, schemars::JsonSchema, Debug, PartialEq)]
pub struct CpuLoadOutput {
    /// Load percentages per CPU.
    pub cpu_loads: Vec<CpuLoadEntry>,
    /// Total load across all CPUs.
    pub total: f32,
}

impl From<&[f32]> for CpuLoadOutput {
    fn from(cpu_loads: &[f32]) -> Self {
        let entries = cpu_loads
            .iter()
            .enumerate()
            .map(|(cpu, &load_pct)| CpuLoadEntry { cpu, load_pct })
            .collect();
        let total = cpu_loads.iter().sum::<f32>();
        Self { cpu_loads: entries, total }
    }
}

/// Load percentage for an individual CPU core.
#[derive(Serialize, schemars::JsonSchema, Debug, PartialEq)]
pub struct CpuLoadEntry {
    /// CPU index.
    pub cpu: usize,
    /// Percentage load on the CPU (0.0 to 100.0).
    pub load_pct: f32,
}

#[derive(thiserror::Error, Debug)]
pub enum MeasureError {
    #[error("Duration must be > 0")]
    ZeroDuration,
    #[error("FIDL error: {0}")]
    Fidl(#[from] fidl::Error),
}

#[derive(FfxTool)]
pub struct CpuLoadTool {
    #[command]
    cmd: args_mod::CpuLoadCommand,
    rcs_proxy: RemoteControlProxyHolder,
    // Deferred so that `--duration`, which only needs fuchsia.kernel.Stats, works on
    // products that do not include metrics-logger.
    #[with(deferred(moniker("/core/metrics-logger")))]
    cpu_logger: Deferred<fmetrics::RecorderProxy>,
}

fho::embedded_plugin!(CpuLoadTool);

impl FfxMain for CpuLoadTool {
    type Writer = VerifiedMachineWriter<Option<CpuLoadOutput>>;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        let CpuLoadTool { cmd, rcs_proxy, cpu_logger, .. } = self;
        match (cmd.subcommand, cmd.duration) {
            (Some(subcommand), None) => {
                let cpu_logger = cpu_logger.await?;
                match subcommand {
                    args_mod::SubCommand::Start(start_cmd) => start(cpu_logger, start_cmd).await?,
                    args_mod::SubCommand::Stop(_) => stop(cpu_logger).await?,
                }
                writer.machine(&None)?;
            }
            (None, Some(duration)) => {
                let stats_proxy = rcs::kernel_stats(&rcs_proxy, std::time::Duration::from_secs(5))
                    .await
                    .map_err(|e| fho::user_error!("Could not open fuchsia.kernel.Stats: {e}"))?;

                let cpu_loads =
                    measure(stats_proxy, duration).await.map_err(|e| fho::user_error!("{e}"))?;
                let output = CpuLoadOutput::from(cpu_loads.as_slice());
                if writer.is_machine() {
                    writer.machine(&Some(output))?;
                } else {
                    print_loads(&output, &mut writer)?;
                }
            }
            _ => fho::return_user_error!(
                "Please specify a duration for immediate load display, or alternatively, utilize \
            the start/stop subcommand to instruct the metrics-logger component to record the \
            CPU usage data on the target."
            ),
        }
        Ok(())
    }
}

pub async fn measure(
    stats_proxy: fstats::StatsProxy,
    duration: std::time::Duration,
) -> Result<Vec<f32>, MeasureError> {
    if duration.is_zero() {
        return Err(MeasureError::ZeroDuration);
    }

    let cpu_loads = stats_proxy.get_cpu_load(duration.as_nanos() as i64).await?;
    Ok(cpu_loads)
}

/// Prints CPU load values in the following format:
///     CPU 0: 0.66%
///     CPU 1: 1.56%
///     CPU 2: 0.83%
///     CPU 3: 0.71%
///     Total: 3.76%
fn print_loads<W: std::io::Write>(
    cpu_loads: &CpuLoadOutput,
    writer: &mut W,
) -> std::io::Result<()> {
    for entry in &cpu_loads.cpu_loads {
        writeln!(writer, "CPU {}: {:.2}%", entry.cpu, entry.load_pct)?;
    }
    writeln!(writer, "Total: {:.2}%", cpu_loads.total)?;

    Ok(())
}

pub async fn start(
    cpu_logger: fmetrics::RecorderProxy,
    cmd: args_mod::StartCommand,
) -> fho::Result<()> {
    let interval_ms = cmd.interval.as_millis() as u32;

    // Dispatch to Recorder.StartLogging or Recorder.StartLoggingForever,
    // depending on whether a logging duration is specified.
    let result = if let Some(duration) = cmd.duration {
        let duration_ms = duration.as_millis() as u32;
        cpu_logger
            .start_logging(
                "ffx_cpu",
                &[Metric::CpuLoad(CpuLoad { interval_ms })],
                duration_ms,
                cmd.output_to_syslog,
                false,
            )
            .await
            .map_err(|e| fho::user_error!("Failed to call Recorder/StartLogging: {e}"))?
    } else {
        cpu_logger
            .start_logging_forever(
                "ffx_cpu",
                &[Metric::CpuLoad(CpuLoad { interval_ms })],
                cmd.output_to_syslog,
                false,
            )
            .await
            .map_err(|e| fho::user_error!("Failed to call Recorder/StartLoggingForever: {e}"))?
    };

    match result {
        Err(fmetrics::RecorderError::InvalidSamplingInterval) => fho::return_user_error!(
            "Recorder.StartLogging received an invalid sampling interval. \n\
            Please check if `interval` meets the following requirements: \n\
            1) Must be smaller than `duration` if `duration` is specified; \n\
            2) Must not be smaller than 500ms if `output_to_syslog` is enabled."
        ),
        Err(fmetrics::RecorderError::AlreadyLogging) => fho::return_user_error!(
            "Ffx cpu-load logging is already active. Use \"stop\" subcommand to stop the active \
            logging manually."
        ),
        Err(fmetrics::RecorderError::TooManyActiveClients) => fho::return_user_error!(
            "Recorder is running too many clients. Retry after any other client is stopped."
        ),
        Err(fmetrics::RecorderError::Internal) => {
            fho::return_user_error!("Recorder encountered an internal error.")
        }
        _ => Ok(()),
    }
}

pub async fn stop(cpu_logger: fmetrics::RecorderProxy) -> fho::Result<()> {
    let stopped = cpu_logger
        .stop_logging("ffx_cpu")
        .await
        .map_err(|e| fho::user_error!("Failed to call Recorder/StopLogging: {e}"))?;
    if !stopped {
        fho::return_user_error!(
            "Stop logging returned false; Check if logging is already inactive."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fdomain_client::fidl::ServerEnd;
    use fdomain_fuchsia_power_metrics::{self as fmetrics};
    use ffx_writer::TestBuffers;
    use futures::channel::mpsc;
    use futures::{StreamExt, TryStreamExt};
    use std::time::Duration;
    use target_holders::{fake_async_proxy, fake_proxy};

    // Create a metrics-logger that expects a specific request type (Start, StartForever, or
    // Stop), and returns a specific error
    macro_rules! make_proxy {
        ($client:expr, $request_type:tt, $error_type:tt) => {
            fake_proxy($client, move |req| match req {
                fmetrics::RecorderRequest::$request_type { responder, .. } => {
                    responder.send(Err(fmetrics::RecorderError::$error_type)).unwrap();
                }
                _ => {
                    panic!("Expected RecorderRequest::{}; got {:?}", stringify!($request_type), req)
                }
            })
        };
    }

    const ONE_SEC: Duration = Duration::from_secs(1);

    /// Tests that invalid arguments are rejected.
    #[fuchsia::test]
    async fn test_invalid_args() {
        let client = fdomain_local::local_client_empty();
        let (proxy, _) = client.create_proxy_and_stream::<fstats::StatsMarker>();
        assert_matches!(
            measure(proxy, Duration::from_secs(0)).await,
            Err(MeasureError::ZeroDuration)
        );
    }

    /// Tests that the input parameter for duration is correctly converted between seconds and
    /// nanoseconds. The test uses a duration parameter of one second.
    #[fuchsia::test]
    async fn test_cpu_load_duration() {
        let (duration_request_sender, mut duration_request_receiver) = mpsc::channel(1);

        let client = fdomain_local::local_client_empty();
        let proxy = fake_async_proxy(client, move |req| {
            let mut duration_request_sender = duration_request_sender.clone();
            async move {
                match req {
                    fstats::StatsRequest::GetCpuLoad { duration, responder } => {
                        duration_request_sender.try_send(duration).unwrap();
                        let _ = responder.send(&[]); // returned values don't matter for this test
                    }
                    request => panic!("Unexpected request: {:?}", request),
                }
            }
        });

        let _ = measure(proxy, Duration::from_secs(1)).await.unwrap();

        match duration_request_receiver.next().await {
            Some(duration_request) => {
                assert_eq!(duration_request as u128, Duration::from_secs(1).as_nanos())
            }
            e => panic!("Failed to get duration_request: {:?}", e),
        }
    }

    /// Confirms that `--duration` measures CPU load without connecting to metrics-logger, which
    /// some products (e.g. userdebug builds) do not include.
    #[fuchsia::test]
    async fn test_duration_does_not_need_metrics_logger() {
        let client = fdomain_local::local_client_empty();
        let scope = fuchsia_async::Scope::new();
        let scope_handle = scope.to_handle();

        let mut capability_handlers = std::collections::HashMap::new();
        capability_handlers.insert(
            "svc/fuchsia.kernel.Stats".to_string(),
            Box::new(move |server_channel| {
                let mut stream =
                    ServerEnd::<fstats::StatsMarker>::new(server_channel).into_stream();
                scope_handle.spawn_local(async move {
                    while let Ok(Some(req)) = stream.try_next().await {
                        match req {
                            fstats::StatsRequest::GetCpuLoad { responder, .. } => {
                                responder.send(&[25.0, 50.0]).unwrap();
                            }
                            request => panic!("Unexpected request: {:?}", request),
                        }
                    }
                });
            }) as Box<dyn Fn(fdomain_client::Channel) + 'static>,
        );
        let rcs = testing_lib::setup_fake_rcs(
            client,
            testing_lib::FakeRcsConfig { capability_handlers, ..Default::default() },
        );

        let tool = CpuLoadTool {
            cmd: args_mod::CpuLoadCommand { duration: Some(ONE_SEC), subcommand: None },
            rcs_proxy: rcs.into(),
            cpu_logger: Deferred::from_output(Err(fho::user_error!(
                "metrics-logger should not be connected"
            ))),
        };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::new_test(None, &buffers);
        tool.main(writer).await.unwrap();

        assert_eq!(buffers.into_stdout_str(), "CPU 0: 25.00%\nCPU 1: 50.00%\nTotal: 75.00%\n");
    }

    /// Confirms that the start/stop subcommands report a missing metrics-logger.
    #[fuchsia::test]
    async fn test_subcommand_reports_missing_metrics_logger() {
        let client = fdomain_local::local_client_empty();
        let rcs = testing_lib::setup_fake_rcs(client, testing_lib::FakeRcsConfig::default());
        let tool = CpuLoadTool {
            cmd: args_mod::CpuLoadCommand {
                duration: None,
                subcommand: Some(args_mod::SubCommand::Stop(args_mod::StopCommand {})),
            },
            rcs_proxy: rcs.into(),
            cpu_logger: Deferred::from_output(Err(fho::user_error!("no metrics-logger"))),
        };
        let buffers = TestBuffers::default();
        let writer = VerifiedMachineWriter::new_test(None, &buffers);

        let err = tool.main(writer).await.unwrap_err();
        assert!(err.to_string().contains("no metrics-logger"), "unexpected error: {err}");
    }

    #[fuchsia::test]
    async fn test_cpu_load_output() {
        let mut writer = Vec::new();
        let data = vec![0.66f32, 1.56, 0.83, 0.71];
        let output = CpuLoadOutput::from(data.as_slice());
        let _ = print_loads(&output, &mut writer).unwrap();

        let output_str = String::from_utf8(writer).expect("valid utf8 output");
        assert_eq!(
            output_str,
            "\
CPU 0: 0.66%
CPU 1: 1.56%
CPU 2: 0.83%
CPU 3: 0.71%
Total: 3.76%
",
        );
    }

    #[fuchsia::test]
    fn test_cpu_load_output_json_serialization() {
        let none_output: Option<CpuLoadOutput> = None;
        assert_eq!(serde_json::to_value(&none_output).unwrap(), serde_json::json!(null));

        let data = vec![0.66f32, 1.56, 0.83, 0.71];
        let measure_output = CpuLoadOutput::from(data.as_slice());
        let expected_measure = serde_json::json!({
            "cpu_loads": [
                { "cpu": 0, "load_pct": 0.66f32 },
                { "cpu": 1, "load_pct": 1.56f32 },
                { "cpu": 2, "load_pct": 0.83f32 },
                { "cpu": 3, "load_pct": 0.71f32 }
            ],
            "total": 3.76f32
        });
        assert_eq!(serde_json::to_value(&Some(&measure_output)).unwrap(), expected_measure);
    }

    /// Confirms that the start logging request is dispatched to FIDL requests as expected.
    #[fuchsia::test]
    async fn test_request_dispatch_start_logging() {
        // Start logging: interval=1s, duration=4s
        let args = args_mod::StartCommand {
            interval: ONE_SEC,
            duration: Some(4 * ONE_SEC),
            output_to_syslog: false,
        };
        let (mut sender, mut receiver) = mpsc::channel(1);
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, move |req| match req {
            fmetrics::RecorderRequest::StartLogging {
                client_id,
                metrics,
                duration_ms,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
            } => {
                assert_eq!(String::from("ffx_cpu"), client_id);
                assert_eq!(metrics.len(), 1);
                assert_eq!(metrics[0], Metric::CpuLoad(CpuLoad { interval_ms: 1000 }),);
                assert_eq!(output_samples_to_syslog, false);
                assert_eq!(output_stats_to_syslog, false);
                assert_eq!(duration_ms, 4000);
                responder.send(Ok(())).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StartLogging; got {:?}", req),
        });
        start(proxy, args).await.unwrap();
        assert_matches!(receiver.next().await, Some(()));
    }

    /// Confirms that the start logging forever request is dispatched to FIDL requests as expected.
    #[fuchsia::test]
    async fn test_request_dispatch_start_logging_forever() {
        // Start logging: interval=1s, duration=forever
        let args =
            args_mod::StartCommand { interval: ONE_SEC, duration: None, output_to_syslog: false };
        let (mut sender, mut receiver) = mpsc::channel(1);
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, move |req| match req {
            fmetrics::RecorderRequest::StartLoggingForever {
                client_id,
                metrics,
                output_samples_to_syslog,
                output_stats_to_syslog,
                responder,
                ..
            } => {
                assert_eq!(String::from("ffx_cpu"), client_id);
                assert_eq!(metrics.len(), 1);
                assert_eq!(metrics[0], Metric::CpuLoad(CpuLoad { interval_ms: 1000 }),);
                assert_eq!(output_samples_to_syslog, false);
                assert_eq!(output_stats_to_syslog, false);
                responder.send(Ok(())).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StartLoggingForever; got {:?}", req),
        });
        start(proxy, args).await.unwrap();
        assert_matches!(receiver.next().await, Some(()));
    }

    /// Confirms that the stop logging request is dispatched to FIDL requests as expected.
    #[fuchsia::test]
    async fn test_request_dispatch_stop_logging() {
        // Stop logging
        let (mut sender, mut receiver) = mpsc::channel(1);
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, move |req| match req {
            fmetrics::RecorderRequest::StopLogging { client_id, responder } => {
                assert_eq!(String::from("ffx_cpu"), client_id);
                responder.send(true).unwrap();
                sender.try_send(()).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StopLogging; got {:?}", req),
        });
        stop(proxy).await.unwrap();
        assert_matches!(receiver.next().await, Some(()));
    }

    #[fuchsia::test]
    async fn test_stop_logging_error() {
        let client = fdomain_local::local_client_empty();
        let proxy = fake_proxy(client, move |req| match req {
            fmetrics::RecorderRequest::StopLogging { responder, .. } => {
                responder.send(false).unwrap();
            }
            _ => panic!("Expected RecorderRequest::StopLogging; got {:?}", req),
        });
        let error = stop(proxy).await.unwrap_err();
        assert!(error.to_string().contains("Stop logging returned false"));
    }

    #[fuchsia::test]
    async fn test_start_logging_interval_error() {
        let args = args_mod::StartCommand {
            interval: ONE_SEC,
            duration: Some(2 * ONE_SEC),
            output_to_syslog: false,
        };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLogging, InvalidSamplingInterval);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid sampling interval"));
    }

    #[fuchsia::test]
    async fn test_start_logging_forever_interval_error() {
        let args =
            args_mod::StartCommand { interval: ONE_SEC, duration: None, output_to_syslog: false };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLoggingForever, InvalidSamplingInterval);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("invalid sampling interval"));
    }

    #[fuchsia::test]
    async fn test_start_logging_already_active_error() {
        let args = args_mod::StartCommand {
            interval: ONE_SEC,
            duration: Some(2 * ONE_SEC),
            output_to_syslog: false,
        };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLogging, AlreadyLogging);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[fuchsia::test]
    async fn test_start_logging_forever_already_active_error() {
        let args =
            args_mod::StartCommand { interval: ONE_SEC, duration: None, output_to_syslog: false };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLoggingForever, AlreadyLogging);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("already active"));
    }

    #[fuchsia::test]
    async fn test_start_logging_too_many_clients_error() {
        let args = args_mod::StartCommand {
            interval: ONE_SEC,
            duration: Some(2 * ONE_SEC),
            output_to_syslog: false,
        };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLogging, TooManyActiveClients);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("too many clients"));
    }

    #[fuchsia::test]
    async fn test_start_logging_forever_too_many_clients_error() {
        let args =
            args_mod::StartCommand { interval: ONE_SEC, duration: None, output_to_syslog: false };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLoggingForever, TooManyActiveClients);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("too many clients"));
    }

    #[fuchsia::test]
    async fn test_start_logging_internal_error() {
        let args = args_mod::StartCommand {
            interval: ONE_SEC,
            duration: Some(2 * ONE_SEC),
            output_to_syslog: false,
        };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLogging, Internal);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("an internal error"));
    }

    #[fuchsia::test]
    async fn test_start_logging_forever_internal_error() {
        let args =
            args_mod::StartCommand { interval: ONE_SEC, duration: None, output_to_syslog: false };
        let client = fdomain_local::local_client_empty();
        let proxy = make_proxy!(client, StartLoggingForever, Internal);
        let error = start(proxy, args).await.unwrap_err();
        assert!(error.to_string().contains("an internal error"));
    }
}
