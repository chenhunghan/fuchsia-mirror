// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result};
use fidl::endpoints::Proxy;
use fidl_fuchsia_io as fidl_legacy_io;
use fidl_next_fuchsia_input_report as fidl_input_report;
use fuchsia_fs::directory;
use serde::{Serialize, Serializer};

/// Path to the `fuchsia.input.report.Service` directory in the component namespace.
pub const SERVICE_DIR: &str = "/svc/fuchsia.input.report.Service";

/// Device identification and metadata for an input device instance.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceInfo {
    pub instance: String,
    #[serde(serialize_with = "to_hex")]
    pub vendor_id: u32,
    #[serde(serialize_with = "to_hex")]
    pub product_id: u32,
    pub serial_number: Option<String>,
    pub manufacturer_name: Option<String>,
    pub product_name: Option<String>,
}

/// Filter criteria for selecting input device instances.
#[derive(Debug, Default)]
pub struct DeviceFilter<'a> {
    pub instance: Option<&'a str>,
    pub vendor_id: Option<u32>,
    pub product_id: Option<u32>,
}

fn to_hex<S>(value: &u32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&format!("0x{value:04x}"))
}

/// Lists all input device service instance names inside `service_dir`.
///
/// Returns an error if reading `service_dir` fails.
pub async fn list_input_device_instances(
    service_dir: &fidl_legacy_io::DirectoryProxy,
) -> Result<Vec<String>> {
    let entries = directory::readdir(service_dir).await?;
    Ok(map_vec_by(entries, |entry| entry.name))
}

/// Connects to the [`fidl_input_report::InputDevice`] service node for the given `instance`
/// directory inside `service_dir`.
///
/// Returns an error with the device path context if connecting to the service node fails.
pub fn connect_to_input_device(
    service_dir: &fidl_legacy_io::DirectoryProxy,
    instance: &str,
) -> Result<fidl_next::Client<fidl_input_report::InputDevice>> {
    let device_path = format!("{instance}/input_device");
    let (client_end, server_end) =
        fidl_next::fuchsia::create_channel::<fidl_input_report::InputDevice>();
    fdio::service_connect_at(
        service_dir.as_channel().as_ref(),
        &device_path,
        server_end.into_untyped(),
    )
    .context(format!("Failed to connect to {device_path}"))?;
    Ok(client_end.spawn())
}

/// Queries the [`DeviceInfo`] for `instance` inside `service_dir`.
///
/// Returns an error if connecting to the device or fetching its descriptor fails.
///
/// # Panics
///
/// Panics if the device descriptor is missing `device_information`, `vendor_id`, or
/// `product_id`, which are required by the [`fidl_input_report::InputDevice`] FIDL protocol.
pub async fn get_device_info(
    service_dir: &fidl_legacy_io::DirectoryProxy,
    instance: String,
) -> Result<DeviceInfo> {
    let device = connect_to_input_device(service_dir, &instance)?;

    let response = device
        .get_descriptor()
        .await
        .map_err(|err| anyhow::anyhow!("Failed to get descriptor for {instance}: {err:?}"))?;

    let device_info = response
        .descriptor
        .device_information
        .expect("device_information is required by FIDL protocol");

    Ok(DeviceInfo {
        instance,
        vendor_id: device_info.vendor_id.expect("vendor_id is required by FIDL protocol"),
        product_id: device_info.product_id.expect("product_id is required by FIDL protocol"),
        serial_number: device_info.serial_number,
        manufacturer_name: device_info.manufacturer_name,
        product_name: device_info.product_name,
    })
}

/// Lists input device service instances in `service_dir` matching `filter`.
///
/// Returns an error if reading `service_dir` fails.
pub async fn get_filtered_instances(
    service_dir: &fidl_legacy_io::DirectoryProxy,
    filter: DeviceFilter<'_>,
) -> Result<Vec<String>> {
    let mut instances: Vec<String> = list_input_device_instances(service_dir).await?;

    if let Some(instance_filter) = filter.instance {
        instances.retain(|instance| instance == instance_filter);
    }

    if filter.vendor_id.is_some() || filter.product_id.is_some() {
        let mut filtered_instances = Vec::new();
        for instance in instances {
            // Skip instances whose descriptors cannot be queried when filtering by vendor or
            // product ID.
            let Ok(device_info) = get_device_info(service_dir, instance.clone()).await else {
                continue;
            };
            let matches_vendor =
                filter.vendor_id.is_none_or(|vendor| device_info.vendor_id == vendor);
            let matches_product =
                filter.product_id.is_none_or(|product| device_info.product_id == product);

            if matches_vendor && matches_product {
                filtered_instances.push(instance);
            }
        }
        instances = filtered_instances;
    }

    Ok(instances)
}

/// Maps an iterator of items to a `Vec<U>` using a mapping function `f`.
pub fn map_vec_by<T, U>(items: impl IntoIterator<Item = T>, f: impl FnMut(T) -> U) -> Vec<U> {
    items.into_iter().map(f).collect()
}

/// Converts an iterator of items into a `Vec<U>` using `Into`.
pub fn convert_vec<T: Into<U>, U>(items: impl IntoIterator<Item = T>) -> Vec<U> {
    map_vec_by(items, Into::into)
}

/// Converts an iterator of items into a `Vec<String>` using `Debug` formatting.
pub fn debug_vec<T: std::fmt::Debug>(items: impl IntoIterator<Item = T>) -> Vec<String> {
    map_vec_by(items, |x| format!("{x:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_vec_by() {
        let input = vec![1, 2, 3];
        let result = map_vec_by(input, |x| x * 2);
        assert_eq!(result, vec![2, 4, 6]);
    }

    #[test]
    fn test_convert_vec() {
        #[derive(Debug, PartialEq, Eq)]
        struct ComplexNumber {
            real: i32,
            imag: i32,
        }

        impl From<i32> for ComplexNumber {
            fn from(val: i32) -> Self {
                Self { real: val, imag: 0 }
            }
        }

        let input = vec![1, 2, 3];
        let result: Vec<ComplexNumber> = convert_vec(input);
        assert_eq!(
            result,
            vec![
                ComplexNumber { real: 1, imag: 0 },
                ComplexNumber { real: 2, imag: 0 },
                ComplexNumber { real: 3, imag: 0 },
            ]
        );
    }

    #[test]
    fn test_debug_vec() {
        #[derive(Debug)]
        enum Sample {
            Foo,
            Bar,
        }
        let input = vec![Sample::Foo, Sample::Bar];
        assert_eq!(debug_vec(input), vec!["Foo".to_string(), "Bar".to_string()]);
    }
}
