// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::common::{self, DeviceFilter, SERVICE_DIR};
use crate::descriptor_types::DeviceDescriptor;
use crate::indented_serializer;
use anyhow::{Context, Result};
use argh::FromArgs;
use fidl_fuchsia_io as fidl_legacy_io;
use fuchsia_fs::directory;
use serde::Serialize;
use std::str::FromStr;

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

#[derive(Debug, Serialize)]
struct GetDescriptorResult {
    instance: String,
    descriptor: DeviceDescriptor,
}

/// Get the descriptor of an input device.
#[derive(FromArgs, Debug, PartialEq, Eq)]
#[argh(subcommand, name = "get-descriptor")]
pub struct GetDescriptorArgs {
    /// output format: "text" or "json" (default: "text")
    #[argh(option, default = "OutputFormat::Text")]
    output: OutputFormat,

    /// instance name to filter by (e.g. "touch"); if none of the `instance`,
    /// `vendor`, or `product` filters are provided, returns descriptors of all
    /// devices
    #[argh(option)]
    instance: Option<String>,

    /// vendor ID to filter by, in hexadecimal format (e.g. "0xabcd")
    #[argh(option, from_str_fn(parse_hex))]
    vendor: Option<u32>,

    /// product ID to filter by, in hexadecimal format (e.g. "0x2c3d")
    #[argh(option, from_str_fn(parse_hex))]
    product: Option<u32>,
}

fn parse_hex(value: &str) -> Result<u32, String> {
    let value = value.trim_start_matches("0x");
    u32::from_str_radix(value, 16).map_err(|err| format!("Failed to parse hex string: {err}"))
}

impl GetDescriptorArgs {
    /// Runs the `get-descriptor` subcommand and writes the formatted descriptors to stdout.
    ///
    /// Returns an error if opening the service directory, querying descriptors, or serializing
    /// output fails.
    pub async fn run(self) -> Result<()> {
        let service_dir = directory::open_in_namespace(
            SERVICE_DIR,
            fidl_legacy_io::Flags::PROTOCOL_DIRECTORY | fidl_legacy_io::PERM_READABLE,
        )
        .context(format!("Failed to open {SERVICE_DIR}"))?;

        let output = self.get_descriptors_to_string(&service_dir).await?;
        print!("{output}");
        Ok(())
    }

    async fn get_descriptors_to_string(
        &self,
        service_dir: &fidl_legacy_io::DirectoryProxy,
    ) -> Result<String> {
        let instances = common::get_filtered_instances(
            service_dir,
            DeviceFilter {
                instance: self.instance.as_deref(),
                vendor_id: self.vendor,
                product_id: self.product,
            },
        )
        .await?;

        let mut results = Vec::with_capacity(instances.len());

        for instance in instances {
            let device = common::connect_to_input_device(service_dir, &instance)?;
            let response = device.get_descriptor().await.map_err(|err| {
                anyhow::anyhow!("Failed to get descriptor for {instance}: {err:?}")
            })?;

            let descriptor = DeviceDescriptor::from(response.descriptor);
            results.push(GetDescriptorResult { instance, descriptor });
        }

        match self.output {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(&results)?;
                Ok(format!("{json}\n"))
            }
            OutputFormat::Text => {
                let mut output = String::new();
                for result in results {
                    output.push_str(&format!("Device Instance: {}\n", result.instance));
                    output.push_str(&indented_serializer::serialize_indented(&result.descriptor)?);
                    output.push('\n');
                }
                Ok(output)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::FakeInputDevice;
    use fidl_next_fuchsia_input as fidl_input;
    use fidl_next_fuchsia_input_report as fidl_input_report;
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

    fn mouse_descriptor() -> fidl_input_report::DeviceDescriptor {
        fidl_input_report::DeviceDescriptor {
            device_information: Some(fidl_input_report::DeviceInformation {
                vendor_id: Some(0x1111),
                product_id: Some(0x2222),
                manufacturer_name: Some("Manufacturer1".to_string()),
                product_name: Some("Product1".to_string()),
                serial_number: Some("Serial1".to_string()),
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

    fn keyboard_descriptor() -> fidl_input_report::DeviceDescriptor {
        fidl_input_report::DeviceDescriptor {
            device_information: Some(fidl_input_report::DeviceInformation {
                vendor_id: Some(0x3333),
                product_id: Some(0x4444),
                manufacturer_name: Some("Manufacturer2".to_string()),
                product_name: Some("Product2".to_string()),
                serial_number: Some("Serial2".to_string()),
                ..Default::default()
            }),
            keyboard: Some(fidl_input_report::KeyboardDescriptor {
                input: Some(fidl_input_report::KeyboardInputDescriptor {
                    keys3: Some(vec![fidl_input::Key::A, fidl_input::Key::B]),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn touch_descriptor() -> fidl_input_report::DeviceDescriptor {
        fidl_input_report::DeviceDescriptor {
            device_information: Some(fidl_input_report::DeviceInformation {
                vendor_id: Some(0x5555),
                product_id: Some(0x6666),
                manufacturer_name: Some("Manufacturer3".to_string()),
                product_name: Some("Product3".to_string()),
                serial_number: Some("Serial3".to_string()),
                ..Default::default()
            }),
            touch: Some(fidl_input_report::TouchDescriptor {
                input: Some(fidl_input_report::TouchInputDescriptor {
                    contacts: Some(vec![fidl_input_report::ContactInputDescriptor {
                        position_x: Some(axis(0, 1000)),
                        position_y: Some(axis(0, 2000)),
                        ..Default::default()
                    }]),
                    max_contacts: Some(10),
                    touch_type: Some(fidl_input_report::TouchType::Touchscreen),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn setup_fake_service_directory() -> fidl_legacy_io::DirectoryProxy {
        let service_dir = vfs::pseudo_directory! {
            "mouse_instance" => vfs::pseudo_directory! {
                "input_device" => FakeInputDevice::new(mouse_descriptor()).serve(),
            },
            "keyboard_instance" => vfs::pseudo_directory! {
                "input_device" => FakeInputDevice::new(keyboard_descriptor()).serve(),
            },
            "touch_instance" => vfs::pseudo_directory! {
                "input_device" => FakeInputDevice::new(touch_descriptor()).serve(),
            },
        };
        directory::serve_read_only(service_dir, ExecutionScope::new())
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_to_string_text_mouse() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Text,
            instance: Some("mouse_instance".to_string()),
            vendor: None,
            product: None,
        };
        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        #[rustfmt::skip]
        let expected = vec![
            "Device Instance: mouse_instance",
            "DeviceDescriptor",
            "  device_information: DeviceInformation",
            "    vendor_id: 0x1111",
            "    product_id: 0x2222",
            "    manufacturer_name: Manufacturer1",
            "    product_name: Product1",
            "    serial_number: Serial1",
            "  mouse: MouseDescriptor",
            "    input: MouseInputDescriptor",
            "      movement_x: Axis",
            "        range: Range",
            "          min: -100",
            "          max: 100",
            "        unit: Unit",
            "          type_: None",
            "          exponent: 0",
            "      movement_y: Axis",
            "        range: Range",
            "          min: -200",
            "          max: 200",
            "        unit: Unit",
            "          type_: None",
            "          exponent: 0",
            "      buttons: List",
            "        #0: 1",
            "        #1: 2",
            "        #2: 3",
        ].join("\n") + "\n";
        expect_eq!(output, expected);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_to_string_text_keyboard() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Text,
            instance: Some("keyboard_instance".to_string()),
            vendor: None,
            product: None,
        };
        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        #[rustfmt::skip]
        let expected = vec![
            "Device Instance: keyboard_instance",
            "DeviceDescriptor",
            "  device_information: DeviceInformation",
            "    vendor_id: 0x3333",
            "    product_id: 0x4444",
            "    manufacturer_name: Manufacturer2",
            "    product_name: Product2",
            "    serial_number: Serial2",
            "  keyboard: KeyboardDescriptor",
            "    input: KeyboardInputDescriptor",
            "      keys3: List",
            "        #0: A",
            "        #1: B",
        ].join("\n") + "\n";
        expect_eq!(output, expected);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_to_string_text_touch() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Text,
            instance: Some("touch_instance".to_string()),
            vendor: None,
            product: None,
        };
        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        #[rustfmt::skip]
        let expected = vec![
            "Device Instance: touch_instance",
            "DeviceDescriptor",
            "  device_information: DeviceInformation",
            "    vendor_id: 0x5555",
            "    product_id: 0x6666",
            "    manufacturer_name: Manufacturer3",
            "    product_name: Product3",
            "    serial_number: Serial3",
            "  touch: TouchDescriptor",
            "    input: TouchInputDescriptor",
            "      contacts: List",
            "        #0: ContactInputDescriptor",
            "          position_x: Axis",
            "            range: Range",
            "              min: 0",
            "              max: 1000",
            "            unit: Unit",
            "              type_: None",
            "              exponent: 0",
            "          position_y: Axis",
            "            range: Range",
            "              min: 0",
            "              max: 2000",
            "            unit: Unit",
            "              type_: None",
            "              exponent: 0",
            "      max_contacts: 10",
            "      touch_type: Touchscreen",
        ].join("\n") + "\n";
        expect_eq!(output, expected);
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_to_string_json() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Json,
            instance: None,
            vendor: None,
            product: None,
        };

        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let descriptors = parsed.as_array().unwrap();
        assert_that!(descriptors, len(eq(3)));
        expect_eq!(descriptors[0]["instance"].as_str().unwrap(), "keyboard_instance");
        expect_eq!(
            descriptors[0]["descriptor"]["device_information"]["vendor_id"].as_str().unwrap(),
            "0x3333"
        );
        expect_true!(descriptors[0]["descriptor"]["keyboard"].is_object());

        expect_eq!(descriptors[1]["instance"].as_str().unwrap(), "mouse_instance");
        expect_eq!(
            descriptors[1]["descriptor"]["device_information"]["vendor_id"].as_str().unwrap(),
            "0x1111"
        );
        expect_true!(descriptors[1]["descriptor"]["mouse"].is_object());

        expect_eq!(descriptors[2]["instance"].as_str().unwrap(), "touch_instance");
        expect_eq!(
            descriptors[2]["descriptor"]["device_information"]["vendor_id"].as_str().unwrap(),
            "0x5555"
        );
        expect_true!(descriptors[2]["descriptor"]["touch"].is_object());
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_vendor_device_filter() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Json,
            instance: None,
            vendor: Some(0x3333),
            product: Some(0x4444),
        };

        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        let parsed: Value = serde_json::from_str(&output).unwrap();
        let descriptors = parsed.as_array().unwrap();
        assert_that!(descriptors, len(eq(1)));
        expect_eq!(descriptors[0]["instance"].as_str().unwrap(), "keyboard_instance");
        expect_eq!(
            descriptors[0]["descriptor"]["device_information"]["vendor_id"].as_str().unwrap(),
            "0x3333"
        );
        expect_true!(descriptors[0]["descriptor"]["keyboard"].is_object());
    }

    #[gtest]
    #[fuchsia::test]
    async fn test_get_descriptors_vendor_device_filter_no_match() {
        let dir_proxy = setup_fake_service_directory();
        let args = GetDescriptorArgs {
            output: OutputFormat::Text,
            instance: None,
            vendor: Some(0x7777),
            product: Some(0x8888),
        };

        let output = args.get_descriptors_to_string(&dir_proxy).await.unwrap();
        expect_eq!(output, "");
    }
}
