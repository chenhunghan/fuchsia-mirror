// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::{self, DeviceFilter, SERVICE_DIR};
use crate::indented_serializer;
use crate::input_report_types::{InputReport, InstanceReport};
use anyhow::{Context, Result};
use argh::FromArgs;
use fidl_fuchsia_io as fidl_legacy_io;
use fidl_next_fuchsia_input_report as fidl_input_report;
use fuchsia_fs::directory;
use futures::StreamExt;
use futures::channel::mpsc::{self, UnboundedSender};
use std::io::{self, Write};
use std::num::NonZero;
use std::str::FromStr;

const MAX_UNACKNOWLEDGED_REPORTS: u16 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Text,
    Json,
}

impl FromStr for OutputFormat {
    type Err = String;
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input.to_lowercase().as_str() {
            "text" => Ok(OutputFormat::Text),
            "json" => Ok(OutputFormat::Json),
            _ => Err(format!("Invalid output format: {input}")),
        }
    }
}

/// Read input reports.
#[derive(FromArgs, Debug, PartialEq, Eq)]
#[argh(subcommand, name = "read")]
pub struct ReadArgs {
    /// output format: "text" or "json" (default: "text")
    #[argh(option, default = "OutputFormat::Text")]
    output: OutputFormat,

    /// instance name to filter by (e.g. "touch"); if none of the `instance`,
    /// `vendor`, or `product` filters are provided, reads from all devices
    #[argh(option)]
    instance: Option<String>,

    /// vendor ID to filter by, in hexadecimal format (e.g. "0xabcd")
    #[argh(option, from_str_fn(parse_hex))]
    vendor: Option<u32>,

    /// product ID to filter by, in hexadecimal format (e.g. "0x2c3d")
    #[argh(option, from_str_fn(parse_hex))]
    product: Option<u32>,

    /// number of total reports to read from all devices (must not be zero; if
    /// `None`, prints reports forever)
    #[argh(option)]
    num_reads: Option<NonZero<u32>>,
}

fn parse_hex(value: &str) -> Result<u32, String> {
    let value = value.trim_start_matches("0x");
    u32::from_str_radix(value, 16).map_err(|err| format!("Failed to parse hex string: {err}"))
}

impl ReadArgs {
    /// Runs the `read` subcommand and writes the formatted input reports to stdout.
    ///
    /// Returns an error if opening the service directory, connecting to input devices, or writing
    /// output fails.
    pub async fn run(self) -> Result<()> {
        let service_directory = directory::open_in_namespace(
            SERVICE_DIR,
            fidl_legacy_io::Flags::PROTOCOL_DIRECTORY | fidl_legacy_io::PERM_READABLE,
        )
        .context(format!("Failed to open {SERVICE_DIR}"))?;

        let mut stdout = io::stdout();
        self.run_with_service_directory(&service_directory, &mut stdout).await
    }

    async fn run_with_service_directory<W: Write>(
        &self,
        service_directory: &fidl_legacy_io::DirectoryProxy,
        mut writer: W,
    ) -> Result<()> {
        let instances = common::get_filtered_instances(
            service_directory,
            DeviceFilter {
                instance: self.instance.as_deref(),
                vendor_id: self.vendor,
                product_id: self.product,
            },
        )
        .await?;

        if instances.is_empty() {
            writeln!(writer, "No matching devices found.")?;
            return Ok(());
        }

        let (sender, mut receiver) = mpsc::unbounded::<InstanceReport>();
        let mut readers = Vec::with_capacity(instances.len());
        for instance in &instances {
            let reader = setup_reader(service_directory, instance, sender.clone()).await?;
            readers.push(reader);
        }
        // Drop the initial sender so the channel can close when all readers close.
        drop(sender);

        let mut reports_read: u32 = 0;

        while self.num_reads.is_none_or(|max_reads| reports_read < max_reads.get()) {
            let Some(instance_report) = receiver.next().await else {
                break;
            };
            match self.output {
                OutputFormat::Text => {
                    let text = indented_serializer::serialize_indented(&instance_report)?;
                    writeln!(writer, "{text}")?;
                }
                OutputFormat::Json => {
                    let json = serde_json::to_string(&instance_report)?;
                    writeln!(writer, "{json}")?;
                }
            }
            reports_read += 1;
        }

        // Drop the receiver, call `client.close()`, and await `join_handle` for all readers to
        // ensure clean shutdown.
        drop(receiver);
        for reader in readers {
            reader.client.close();
            // Intentionally ignore the handler join result during shutdown.
            let _ = reader.join_handle.await;
        }

        Ok(())
    }
}

struct ActiveReader {
    client: fidl_next::Client<fidl_input_report::InputReportsReaderV2>,
    join_handle: fidl_next::HandlerJoinHandle<zx::Channel, ReaderHandler>,
}

struct ReaderHandler {
    client: fidl_next::Client<fidl_input_report::InputReportsReaderV2>,
    sender: UnboundedSender<InstanceReport>,
    instance: String,
}

impl fidl_input_report::InputReportsReaderV2ClientHandler for ReaderHandler {
    async fn on_input_reports(
        &mut self,
        request: fidl_next::Request<fidl_input_report::input_reports_reader_v2::OnInputReports>,
    ) {
        let payload = request.payload();
        for report in payload.reports {
            let instance_report = InstanceReport {
                instance: self.instance.clone(),
                report: InputReport::from(report),
            };
            if self.sender.unbounded_send(instance_report).is_err() {
                self.client.close();
                return;
            }
        }
        // Intentionally ignore failure during `acknowledge_reports`, as the server may have closed.
        let _ = self.client.acknowledge_reports(payload.last_report_stamp).await;
    }
}

async fn setup_reader(
    service_directory: &fidl_legacy_io::DirectoryProxy,
    instance: &str,
    sender: UnboundedSender<InstanceReport>,
) -> Result<ActiveReader> {
    let device_client = common::connect_to_input_device(service_directory, instance)?;
    let (reader_client_end, reader_server_end) =
        fidl_next::fuchsia::create_channel::<fidl_input_report::InputReportsReaderV2>();
    device_client
        .get_input_reports_reader_v2(reader_server_end, MAX_UNACKNOWLEDGED_REPORTS)
        .await
        .context("Failed to get InputReportsReaderV2")?;
    let instance = instance.to_string();
    let (client, join_handle) = reader_client_end.spawn_handler_full_with(|client| ReaderHandler {
        client,
        sender,
        instance,
    });
    Ok(ActiveReader { client, join_handle })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeInputDevice;
    use fuchsia_async::Task;
    use googletest::prelude::*;
    use serde_json::Value;
    use vfs::directory;
    use vfs::execution_scope::ExecutionScope;

    fn axis(min: i64, max: i64) -> fidl_input_report::Axis {
        fidl_input_report::Axis {
            range: fidl_input_report::Range { min, max },
            unit: fidl_input_report::Unit { type_: fidl_input_report::UnitType::None, exponent: 0 },
        }
    }

    fn mouse_descriptor(vendor_id: u32, product_id: u32) -> fidl_input_report::DeviceDescriptor {
        fidl_input_report::DeviceDescriptor {
            device_information: Some(fidl_input_report::DeviceInformation {
                vendor_id: Some(vendor_id),
                product_id: Some(product_id),
                manufacturer_name: Some("TestManufacturer".to_string()),
                product_name: Some("TestMouse".to_string()),
                serial_number: Some("TestSerial".to_string()),
                ..Default::default()
            }),
            mouse: Some(fidl_input_report::MouseDescriptor {
                input: Some(fidl_input_report::MouseInputDescriptor {
                    movement_x: Some(axis(-100, 100)),
                    movement_y: Some(axis(-200, 200)),
                    buttons: Some(vec![1, 2, 3]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_json_success() {
        let device = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let service_directory = vfs::pseudo_directory! {
            "mouse_instance" => vfs::pseudo_directory! {
                "input_device" => device.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: Some("mouse_instance".to_string()),
            vendor: None,
            product: None,
            num_reads: Some(const { NonZero::new(2).unwrap() }),
        };

        let mut output_buffer = Vec::new();
        let read_task = Task::spawn(async move {
            args.run_with_service_directory(&directory_proxy, &mut output_buffer)
                .await
                .map(|()| output_buffer)
        });

        device.wait_for_readers(1).await;

        let reports = vec![
            fidl_input_report::InputReport {
                event_time: Some(100),
                report_id: Some(1),
                mouse: Some(fidl_input_report::MouseInputReport {
                    movement_x: Some(5),
                    movement_y: Some(-5),
                    ..Default::default()
                }),
                ..Default::default()
            },
            fidl_input_report::InputReport {
                event_time: Some(200),
                report_id: Some(1),
                mouse: Some(fidl_input_report::MouseInputReport {
                    movement_x: Some(2),
                    movement_y: Some(-2),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ];
        device.send_input_reports(&reports).await;

        let output_buffer = read_task.await.expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        let json_reports: Vec<Value> =
            output.trim().split('\n').map(|line| serde_json::from_str(line).unwrap()).collect();

        let expected = vec![
            serde_json::json!({
                "instance": "mouse_instance",
                "report": {
                    "event_time": 100,
                    "report_id": 1,
                    "mouse": {
                        "movement_x": 5,
                        "movement_y": -5
                    }
                }
            }),
            serde_json::json!({
                "instance": "mouse_instance",
                "report": {
                    "event_time": 200,
                    "report_id": 1,
                    "mouse": {
                        "movement_x": 2,
                        "movement_y": -2
                    }
                }
            }),
        ];
        expect_eq!(json_reports, expected);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_text_success() {
        let device = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let service_directory = vfs::pseudo_directory! {
            "mouse_instance" => vfs::pseudo_directory! {
                "input_device" => device.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let args = ReadArgs {
            output: OutputFormat::Text,
            instance: Some("mouse_instance".to_string()),
            vendor: None,
            product: None,
            num_reads: Some(const { NonZero::new(1).unwrap() }),
        };

        let mut output_buffer = Vec::new();
        let read_task = Task::spawn(async move {
            args.run_with_service_directory(&directory_proxy, &mut output_buffer)
                .await
                .map(|()| output_buffer)
        });

        device.wait_for_readers(1).await;

        let reports = vec![fidl_input_report::InputReport {
            event_time: Some(150),
            report_id: Some(1),
            mouse: Some(fidl_input_report::MouseInputReport {
                movement_x: Some(10),
                movement_y: Some(-20),
                ..Default::default()
            }),
            ..Default::default()
        }];
        device.send_input_reports(&reports).await;

        let output_buffer = read_task.await.expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();

        #[rustfmt::skip]
        let expected = vec![
            "InstanceReport",
            "  instance: mouse_instance",
            "  report: InputReport",
            "    event_time: 150",
            "    report_id: 1",
            "    mouse: MouseInputReport",
            "      movement_x: 10",
            "      movement_y: -20",
        ].join("\n") + "\n";
        expect_eq!(output, expected);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_filter_vendor_product_match() {
        let device_1 = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let device_2 = FakeInputDevice::new(mouse_descriptor(0x3333, 0x4444));
        let service_directory = vfs::pseudo_directory! {
            "mouse_1" => vfs::pseudo_directory! {
                "input_device" => device_1.serve(),
            },
            "mouse_2" => vfs::pseudo_directory! {
                "input_device" => device_2.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: None,
            vendor: Some(0x1111),
            product: Some(0x2222),
            num_reads: Some(const { NonZero::new(1).unwrap() }),
        };

        let mut output_buffer = Vec::new();
        let read_task = Task::spawn(async move {
            args.run_with_service_directory(&directory_proxy, &mut output_buffer)
                .await
                .map(|()| output_buffer)
        });

        device_1.wait_for_readers(1).await;

        let reports = vec![fidl_input_report::InputReport {
            event_time: Some(100),
            report_id: Some(1),
            mouse: Some(fidl_input_report::MouseInputReport {
                movement_x: Some(5),
                movement_y: Some(-5),
                ..Default::default()
            }),
            ..Default::default()
        }];
        device_1.send_input_reports(&reports).await;

        let output_buffer = read_task.await.expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        let json_reports: Vec<Value> =
            output.trim().split('\n').map(|line| serde_json::from_str(line).unwrap()).collect();

        let expected = vec![serde_json::json!({
            "instance": "mouse_1",
            "report": {
                "event_time": 100,
                "report_id": 1,
                "mouse": {
                    "movement_x": 5,
                    "movement_y": -5
                }
            }
        })];
        expect_eq!(json_reports, expected);
        expect_eq!(device_2.num_readers(), 0);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_filter_vendor_product_no_match() {
        let device = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let service_directory = vfs::pseudo_directory! {
            "mouse_1" => vfs::pseudo_directory! {
                "input_device" => device.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let mut output_buffer = Vec::new();
        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: None,
            vendor: Some(0x7777),
            product: Some(0x8888),
            num_reads: Some(const { NonZero::new(2).unwrap() }),
        };

        args.run_with_service_directory(&directory_proxy, &mut output_buffer)
            .await
            .expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        expect_eq!(output, "No matching devices found.\n");
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_filter_instance_match() {
        let device_1 = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let device_2 = FakeInputDevice::new(mouse_descriptor(0x3333, 0x4444));
        let service_directory = vfs::pseudo_directory! {
            "mouse_1" => vfs::pseudo_directory! {
                "input_device" => device_1.serve(),
            },
            "mouse_2" => vfs::pseudo_directory! {
                "input_device" => device_2.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: Some("mouse_2".to_string()),
            vendor: None,
            product: None,
            num_reads: Some(const { NonZero::new(1).unwrap() }),
        };

        let mut output_buffer = Vec::new();
        let read_task = Task::spawn(async move {
            args.run_with_service_directory(&directory_proxy, &mut output_buffer)
                .await
                .map(|()| output_buffer)
        });

        device_2.wait_for_readers(1).await;

        let reports = vec![fidl_input_report::InputReport {
            event_time: Some(300),
            report_id: Some(1),
            mouse: Some(fidl_input_report::MouseInputReport {
                movement_x: Some(7),
                movement_y: Some(-7),
                ..Default::default()
            }),
            ..Default::default()
        }];
        device_2.send_input_reports(&reports).await;

        let output_buffer = read_task.await.expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        let json_reports: Vec<Value> =
            output.trim().split('\n').map(|line| serde_json::from_str(line).unwrap()).collect();

        let expected = vec![serde_json::json!({
            "instance": "mouse_2",
            "report": {
                "event_time": 300,
                "report_id": 1,
                "mouse": {
                    "movement_x": 7,
                    "movement_y": -7
                }
            }
        })];
        expect_eq!(json_reports, expected);
        expect_eq!(device_1.num_readers(), 0);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_device_filter_instance_no_match() {
        let device = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let service_directory = vfs::pseudo_directory! {
            "mouse_1" => vfs::pseudo_directory! {
                "input_device" => device.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let mut output_buffer = Vec::new();
        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: Some("unknown_mouse".to_string()),
            vendor: None,
            product: None,
            num_reads: Some(const { NonZero::new(2).unwrap() }),
        };

        args.run_with_service_directory(&directory_proxy, &mut output_buffer)
            .await
            .expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        expect_eq!(output, "No matching devices found.\n");
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_read_multiple_devices() {
        let device_1 = FakeInputDevice::new(mouse_descriptor(0x1111, 0x2222));
        let device_2 = FakeInputDevice::new(mouse_descriptor(0x3333, 0x4444));
        let service_directory = vfs::pseudo_directory! {
            "mouse_1" => vfs::pseudo_directory! {
                "input_device" => device_1.serve(),
            },
            "mouse_2" => vfs::pseudo_directory! {
                "input_device" => device_2.serve(),
            },
        };
        let directory_proxy = directory::serve_read_only(service_directory, ExecutionScope::new());

        let args = ReadArgs {
            output: OutputFormat::Json,
            instance: None,
            vendor: None,
            product: None,
            num_reads: Some(const { NonZero::new(2).unwrap() }),
        };

        let mut output_buffer = Vec::new();
        let read_task = Task::spawn(async move {
            args.run_with_service_directory(&directory_proxy, &mut output_buffer)
                .await
                .map(|()| output_buffer)
        });

        device_1.wait_for_readers(1).await;
        device_2.wait_for_readers(1).await;

        let mouse_1_reports = vec![fidl_input_report::InputReport {
            event_time: Some(100),
            report_id: Some(1),
            mouse: Some(fidl_input_report::MouseInputReport {
                movement_x: Some(5),
                movement_y: Some(-5),
                ..Default::default()
            }),
            ..Default::default()
        }];
        let mouse_2_reports = vec![fidl_input_report::InputReport {
            event_time: Some(200),
            report_id: Some(1),
            mouse: Some(fidl_input_report::MouseInputReport {
                movement_x: Some(2),
                movement_y: Some(-2),
                ..Default::default()
            }),
            ..Default::default()
        }];

        device_1.send_input_reports(&mouse_1_reports).await;
        device_2.send_input_reports(&mouse_2_reports).await;

        let output_buffer = read_task.await.expect("run_with_service_directory failed");
        let output = String::from_utf8(output_buffer).unwrap();
        let mut json_reports: Vec<Value> =
            output.trim().split('\n').map(|line| serde_json::from_str(line).unwrap()).collect();
        json_reports.sort_by_key(|report| report["instance"].as_str().unwrap().to_string());

        let expected = vec![
            serde_json::json!({
                "instance": "mouse_1",
                "report": {
                    "event_time": 100,
                    "report_id": 1,
                    "mouse": {
                        "movement_x": 5,
                        "movement_y": -5
                    }
                }
            }),
            serde_json::json!({
                "instance": "mouse_2",
                "report": {
                    "event_time": 200,
                    "report_id": 1,
                    "mouse": {
                        "movement_x": 2,
                        "movement_y": -2
                    }
                }
            }),
        ];
        expect_eq!(json_reports, expected);
    }
}
