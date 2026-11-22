// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::device::{Device, DeviceTag, Parent};
use crate::environment::{DevicePublisher, Environment, SinglePublisher};

use super::Matcher;
use anyhow::{Context as _, Error};
use async_trait::async_trait;
use fshost_assembly_config::{BlockDeviceConfig, BlockDeviceParent};
use std::collections::HashMap;
use std::collections::hash_map::Entry;

pub async fn get_config_matchers(
    publisher: &DevicePublisher,
) -> Result<Vec<Box<dyn Matcher>>, Error> {
    let devices_str =
        match fuchsia_fs::file::read_in_namespace_to_string("/boot/config/fshost").await {
            Ok(devices_str) => devices_str,
            Err(error) => {
                log::warn!(error:?; "Could not read fshost config, skipping configured matchers");
                return Ok(Vec::new());
            }
        };
    let devices: Vec<BlockDeviceConfig> =
        serde_json::from_str(&devices_str).context("deserializing config")?;
    build_config_matchers(publisher, devices)
}

/// Builds `ConfigMatcher` instances from `devices`, grouping entries that share the same
/// `(parent, label)` source so a single matched block device can be published to multiple paths
/// under `/block`.
pub(super) fn build_config_matchers(
    publisher: &DevicePublisher,
    devices: Vec<BlockDeviceConfig>,
) -> Result<Vec<Box<dyn Matcher>>, Error> {
    let mut matchers: Vec<Box<ConfigMatcher>> = Vec::new();
    let mut index_by_source: HashMap<(Parent, String), usize> = HashMap::new();
    for device in devices {
        let parent = match device.from.parent {
            BlockDeviceParent::Gpt => Parent::SystemPartitionTable,
            BlockDeviceParent::Dev => Parent::Dev,
        };
        let staged_publisher = Box::new(publisher.stage(&device.device)?);
        match index_by_source.entry((parent, device.from.label.clone())) {
            Entry::Occupied(entry) => {
                matchers[*entry.get()].add_publisher(device.device, staged_publisher);
            }
            Entry::Vacant(entry) => {
                entry.insert(matchers.len());
                matchers.push(ConfigMatcher::new(
                    device.device,
                    device.from.label,
                    parent,
                    staged_publisher,
                ));
            }
        }
    }

    Ok(matchers.into_iter().map(|m| m as Box<dyn Matcher>).collect())
}

pub struct ConfigMatcher {
    label: String,
    parent: Parent,
    publishers: Vec<(String, Box<dyn SinglePublisher>)>,
}

impl ConfigMatcher {
    pub fn new(
        name: String,
        label: String,
        parent: Parent,
        publisher: Box<dyn SinglePublisher>,
    ) -> Box<Self> {
        Box::new(ConfigMatcher { label, parent, publishers: vec![(name, publisher)] })
    }

    fn add_publisher(&mut self, name: String, publisher: Box<dyn SinglePublisher>) {
        self.publishers.push((name, publisher));
    }
}

#[async_trait]
impl Matcher for ConfigMatcher {
    fn matcher_name(&self) -> &str {
        "Config"
    }

    async fn match_device(&self, device: &mut dyn Device) -> bool {
        !self.publishers.is_empty()
            && device.partition_label().await.is_ok_and(|label| label == &self.label)
            && device.parent() == self.parent
    }

    async fn process_device(
        &mut self,
        device: &mut dyn Device,
        _env: &mut dyn Environment,
    ) -> Result<Option<DeviceTag>, Error> {
        for (name, publisher) in std::mem::take(&mut self.publishers) {
            log::info!("publishing device to /block/{name}");
            publisher.publish(device)?;
        }
        Ok(None)
    }
}
