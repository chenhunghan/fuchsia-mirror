// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{AggregateEntry, BoardConfig, Device, ResourceEntry};
use anyhow::{Context, anyhow, bail};
#[cfg(not(target_os = "fuchsia"))]
use fidl_fuchsia_driver_framework_common as fdf_framework;
use fidl_fuchsia_driver_metadata_common as fdr;
#[cfg(not(target_os = "fuchsia"))]
use fidl_fuchsia_hardware_platform_bus_common as fpbus;
#[cfg(not(target_os = "fuchsia"))]
use fidl_fuchsia_hardware_power_common as fpower;
#[cfg(target_os = "fuchsia")]
use fidl_next_fuchsia_driver_framework as fdf_framework;
#[cfg(target_os = "fuchsia")]
use fidl_next_fuchsia_hardware_platform_bus as fpbus;
#[cfg(target_os = "fuchsia")]
use fidl_next_fuchsia_hardware_power as fpower;
use phf;
use std::collections::{HashMap, HashSet};

#[cfg(target_os = "fuchsia")]
pub use fuchsia::*;

pub const BIND_PROTOCOL_DEVICE: u32 = 85; // 0x55
pub const BIND_PLATFORM_DEV_VID_GENERIC: u32 = 0;
pub const BIND_PLATFORM_DEV_DID_DEVICETREE: u32 = 50; // 0x32

pub fn property_int(val: u32) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::IntValue(val)
}

pub fn property_string(val: &str) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::StringValue(val.to_string())
}

pub fn property_bool(val: bool) -> fdf_framework::NodePropertyValue {
    fdf_framework::NodePropertyValue::BoolValue(val)
}

fn map_interrupt_mode(mode: Option<&str>) -> fpbus::ZirconInterruptMode {
    match mode {
        Some("EdgeLow") => fpbus::ZirconInterruptMode::EdgeLow,
        Some("EdgeHigh") => fpbus::ZirconInterruptMode::EdgeHigh,
        Some("LevelLow") => fpbus::ZirconInterruptMode::LevelLow,
        Some("LevelHigh") => fpbus::ZirconInterruptMode::LevelHigh,
        Some("EdgeBoth") => fpbus::ZirconInterruptMode::EdgeBoth,
        _ => fpbus::ZirconInterruptMode::Default,
    }
}

pub type DriverSpecificMetadata =
    phf::Map<&'static str, &'static [(&'static str, fn() -> anyhow::Result<Vec<u8>>)]>;

pub type DriverSpecificPowerConfigs =
    phf::Map<&'static str, fn() -> anyhow::Result<Vec<fpower::PowerElementConfiguration>>>;

pub fn make_accept_bind_rule(
    key: &str,
    value: fdf_framework::NodePropertyValue,
) -> fdf_framework::BindRule2 {
    fdf_framework::BindRule2 {
        key: key.to_string(),
        condition: fdf_framework::Condition::Accept,
        values: vec![value],
    }
}

pub fn make_property2(
    key: &str,
    value: fdf_framework::NodePropertyValue,
) -> fdf_framework::NodeProperty2 {
    fdf_framework::NodeProperty2 { key: key.to_string(), value }
}

pub fn add_rule_and_property(
    bind_rules: &mut Vec<fdf_framework::BindRule2>,
    properties: &mut Vec<fdf_framework::NodeProperty2>,
    key: &str,
    value: fdf_framework::NodePropertyValue,
) {
    bind_rules.push(make_accept_bind_rule(key, value.clone()));
    properties.push(make_property2(key, value));
}

#[derive(Clone, Copy, Debug)]
pub enum ValueSource {
    /// Value of a given key within the constraint table.
    ConstraintKey(&'static str),

    /// The `name` field of the DML service offer.
    ResourceName,

    /// The target node name (the `to` field) of the DML service offer.
    ResourceNode,

    /// A literal integer value.
    Integer(u32),

    /// The `id` field of the provider (the `from` field) device in the DML.
    ProviderId,
}

#[derive(Clone, Copy, Debug)]
pub enum RuleValueType {
    Integer,
    String,
    Bool,
}

#[derive(Clone, Copy, Debug)]
pub enum Destination {
    /// Create a bind rule of the given key and value.
    BindRules,

    /// Create a node property for the given key and value.
    Properties,

    /// Create a node property and a bind rule for the given key and value.
    Both,
}

/// Defines how to create a node property or bind rule from a given DML service offer.
#[derive(Clone, Copy, Debug)]
pub struct PropertyRule {
    /// Name of key for the node property or bind rule.
    pub bind_key: &'static str,

    /// Candidates for the value of the node property or bind rule.
    ///
    /// Ordered by priority. If the first source exists it will be used, otherwise the next source
    /// is checked and so on.
    pub sources: &'static [ValueSource],

    /// The type of the node property's or bind rule's value.
    pub value_type: RuleValueType,

    /// Where to add `bind_key` and its value in the composite node spec.
    pub destination: Destination,
}

#[derive(Clone, Copy, Debug)]
pub enum TransportType {
    None,
    Zircon,
    Driver,
}

/// Defines how to convert a given DML service offer into a parent node of a composite node spec.
#[derive(Clone, Copy, Debug)]
pub struct ServiceBindConfig {
    /// Which transport type to use.
    pub transport: TransportType,

    /// Rules for creating node properties and bind rules for the parent node.
    pub rules: &'static [PropertyRule],

    /// Candidates for the deduplication key of the parent node.
    ///
    /// Ordered by priority. If the first source exists it will be used, otherwise the next source
    /// is checked and so on.
    pub parent_key_sources: &'static [ValueSource],

    /// If true, adds a `fuchsia.ID` bind rule from the `"id"` constraint when not already set by
    /// `rules`.
    pub bind_id: bool,
}

/// Service bind config for singleton services whose provider node does not set a `fuchsia.ID`
/// property (`bind_id: false`).
///
/// Note: [`generate_parent_spec_generic`] falls back to [`DEFAULT_ID_SERVICE_BIND_CONFIG`]
/// (`bind_id: true`) when a service is not in `parser_config.service_configs` or
/// [`STANDARD_SERVICE_CONFIGS`], so boards must explicitly map ID-less services to
/// [`DEFAULT_SERVICE_BIND_CONFIG`].
pub const DEFAULT_SERVICE_BIND_CONFIG: ServiceBindConfig = ServiceBindConfig {
    transport: TransportType::Zircon,
    rules: &[],
    parent_key_sources: &[ValueSource::ResourceName],
    bind_id: false,
};

/// Default fallback service bind config used when a service is not listed in
/// `parser_config.service_configs` or [`STANDARD_SERVICE_CONFIGS`] (`bind_id: true`).
pub const DEFAULT_ID_SERVICE_BIND_CONFIG: ServiceBindConfig =
    ServiceBindConfig { bind_id: true, ..DEFAULT_SERVICE_BIND_CONFIG };

pub struct DmlParserConfig {
    pub service_configs: phf::Map<&'static str, ServiceBindConfig>,
}

/// A list of instructions for how to convert common DML service offers into parent nodes
/// of composite node specs.
pub static STANDARD_SERVICE_CONFIGS: phf::Map<&'static str, ServiceBindConfig> = phf::phf_map! {
    "fuchsia.clock.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x494B4C43)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.pwm.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x004D5750)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.gpio.Init" => ServiceBindConfig {
        transport: TransportType::None,
        rules: &[PropertyRule {
            bind_key: "fuchsia.BIND_INIT_STEP",
            sources: &[ValueSource::Integer(0x4F495047)],
            value_type: RuleValueType::Integer,
            destination: Destination::Both,
        }],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.gpu.mali.Service" => ServiceBindConfig {
        transport: TransportType::Driver,
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.platform.device.Service" => ServiceBindConfig {
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PROTOCOL",
                sources: &[ValueSource::Integer(BIND_PROTOCOL_DEVICE)],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.platform.bus.Service" => ServiceBindConfig {
        transport: TransportType::Driver,
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_PID",
                sources: &[ValueSource::ConstraintKey("pid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PLATFORM_DEV_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
    "fuchsia.hardware.pci.Service" => ServiceBindConfig {
        rules: &[
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_VID",
                sources: &[ValueSource::ConstraintKey("vid")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_DID",
                sources: &[ValueSource::ConstraintKey("did")],
                value_type: RuleValueType::Integer,
                destination: Destination::Both,
            },
            PropertyRule {
                bind_key: "fuchsia.BIND_PCI_TOPO",
                sources: &[ValueSource::ConstraintKey("topo")],
                value_type: RuleValueType::Integer,
                destination: Destination::BindRules,
            },
        ],
        ..DEFAULT_SERVICE_BIND_CONFIG
    },
};

pub const DEFAULT_DML_PARSER_CONFIG: DmlParserConfig =
    DmlParserConfig { service_configs: phf::phf_map! {} };

impl Default for DmlParserConfig {
    fn default() -> Self {
        DEFAULT_DML_PARSER_CONFIG
    }
}

fn resolve_value(
    provider_id: Option<u32>,
    source: &ValueSource,
    res: &ResourceEntry,
    constraint: &fdr::Dictionary,
) -> Option<ResolvedValue> {
    match source {
        ValueSource::ConstraintKey(key) => {
            if let Some(i) = crate::get_int64(constraint, key) {
                Some(ResolvedValue::Integer(i as u32))
            } else if let Some(s) = crate::get_string(constraint, key) {
                Some(ResolvedValue::String(s))
            } else if let Some(b) = crate::get_bool(constraint, key) {
                Some(ResolvedValue::Bool(b))
            } else {
                None
            }
        }
        ValueSource::ResourceName => res.name.clone().map(ResolvedValue::String),
        ValueSource::ResourceNode => res.node.clone().map(ResolvedValue::String),
        ValueSource::Integer(i) => Some(ResolvedValue::Integer(*i)),
        ValueSource::ProviderId => provider_id.map(ResolvedValue::Integer),
    }
}

enum ResolvedValue {
    Integer(u32),
    String(String),
    Bool(bool),
}

fn apply_rule(
    provider_id: Option<u32>,
    rule: &PropertyRule,
    res: &ResourceEntry,
    constraint: &fdr::Dictionary,
    bind_rules: &mut Vec<fdf_framework::BindRule2>,
    properties: &mut Vec<fdf_framework::NodeProperty2>,
) -> anyhow::Result<()> {
    let resolved =
        rule.sources.iter().find_map(|source| resolve_value(provider_id, source, res, constraint));

    let Some(val) = resolved else {
        return Ok(());
    };

    let property_value = match (val, rule.value_type) {
        (ResolvedValue::Integer(i), RuleValueType::Integer) => property_int(i),
        (ResolvedValue::String(s), RuleValueType::String) => property_string(&s),
        (ResolvedValue::Bool(b), RuleValueType::Bool) => property_bool(b),
        (ResolvedValue::String(s), RuleValueType::Integer) => {
            let parsed = if s.starts_with("0x") || s.starts_with("0X") {
                u32::from_str_radix(&s[2..], 16)
            } else {
                s.parse::<u32>()
            };
            match parsed {
                Ok(i) => property_int(i),
                Err(e) => {
                    bail!("Failed to parse string {} as integer: {}", s, e);
                }
            }
        }
        (ResolvedValue::String(s), RuleValueType::Bool) => match s.parse::<bool>() {
            Ok(b) => property_bool(b),
            Err(e) => {
                bail!("Failed to parse string {} as bool: {}", s, e);
            }
        },
        _ => {
            bail!("Type mismatch in rule resolution");
        }
    };

    match rule.destination {
        Destination::BindRules => {
            bind_rules.push(make_accept_bind_rule(rule.bind_key, property_value));
        }
        Destination::Properties => {
            properties.push(make_property2(rule.bind_key, property_value));
        }
        Destination::Both => {
            add_rule_and_property(bind_rules, properties, rule.bind_key, property_value);
        }
    }

    Ok(())
}

pub fn generate_parent_spec_generic(
    agg: &AggregateEntry,
    res: &ResourceEntry,
    provider_id: Option<u32>,
    parser_config: &DmlParserConfig,
) -> anyhow::Result<Option<(fdf_framework::ParentSpec2, String)>> {
    let mut bind_rules = Vec::new();
    let mut properties = Vec::new();

    let service_name = agg.service.as_deref().unwrap_or("");
    let service_config = parser_config
        .service_configs
        .get(service_name)
        .or_else(|| STANDARD_SERVICE_CONFIGS.get(service_name))
        .unwrap_or(&DEFAULT_ID_SERVICE_BIND_CONFIG);

    match service_config.transport {
        TransportType::Zircon => {
            add_rule_and_property(
                &mut bind_rules,
                &mut properties,
                "fuchsia.Service",
                property_string(service_name),
            );
            properties.push(make_property2(
                service_name,
                property_string(&format!("{service_name}.ZirconTransport")),
            ));
        }
        TransportType::Driver => {
            add_rule_and_property(
                &mut bind_rules,
                &mut properties,
                "fuchsia.Service",
                property_string(service_name),
            );
            properties.push(make_property2(
                service_name,
                property_string(&format!("{service_name}.DriverTransport")),
            ));
        }
        TransportType::None => {}
    }

    let constraint =
        res.constraint.as_ref().ok_or_else(|| anyhow!("constraint is missing in ResourceEntry"))?;

    for rule in service_config.rules {
        apply_rule(provider_id, rule, res, constraint, &mut bind_rules, &mut properties)?;
    }

    if !properties.iter().any(|p| p.key == "fuchsia.NAME") {
        let name_opt = res.name.clone().or_else(|| crate::get_string(constraint, "name"));
        if let Some(name) = name_opt {
            properties.push(make_property2("fuchsia.NAME", property_string(&name)));
        }
    }

    if service_config.bind_id
        && !bind_rules.iter().any(|r| r.key == "fuchsia.ID")
        && let Some(id) = crate::get_uint32(constraint, "id")
    {
        bind_rules.push(make_accept_bind_rule("fuchsia.ID", property_int(id)));
    }

    let resolved_key = service_config.parent_key_sources.iter().find_map(|source| {
        match resolve_value(provider_id, source, res, constraint) {
            Some(ResolvedValue::String(k)) => Some(k),
            _ => None,
        }
    });

    let key = resolved_key.with_context(|| {
        format!(
            "Failed to resolve parent key for service {}. If this service should be ignored, add it to the config.",
            service_name
        )
    })?;

    let parent = fdf_framework::ParentSpec2 { bind_rules, properties };
    Ok(Some((parent, key)))
}

/// A device ready to publish: its name, its composite node spec, and its platform bus node.
pub type PreparedDevice = (String, Option<fdf_framework::CompositeNodeSpec>, Option<fpbus::Node>);

/// Returns the indices of `devices` in the order they should be published.
///
/// Devices are visited by descending `publish_priority`, with ties kept in config order. Each
/// device's providers (`providers_by_device[idx]`) are emitted before the device itself, so a
/// high-priority device pulls everything it binds against ahead of the rest of the board.
fn publish_order(devices: &[Device], providers_by_device: &[Vec<usize>]) -> Vec<usize> {
    fn visit(
        idx: usize,
        providers_by_device: &[Vec<usize>],
        visited: &mut [bool],
        order: &mut Vec<usize>,
    ) {
        // Marking before recursing makes dependency cycles terminate. Devices in a cycle are
        // emitted after their providers outside the cycle.
        if std::mem::replace(&mut visited[idx], true) {
            return;
        }
        for &provider in &providers_by_device[idx] {
            visit(provider, providers_by_device, visited, order);
        }
        order.push(idx);
    }

    let mut roots: Vec<usize> = (0..devices.len()).collect();
    roots.sort_by_key(|&idx| std::cmp::Reverse(devices[idx].publish_priority.unwrap_or(0)));

    let mut visited = vec![false; devices.len()];
    let mut order = Vec::with_capacity(devices.len());
    for idx in roots {
        visit(idx, providers_by_device, &mut visited, &mut order);
    }
    order
}

/// Computes a deterministic non-zero 32-bit FNV-1a instance ID from a device's name so that
/// adding, removing, or reordering devices in board DML shards does not shift other devices'
/// `BIND_PLATFORM_DEV_INSTANCE_ID` values.
pub fn compute_instance_id(name: &str) -> u32 {
    let mut hash: u32 = 2166136261;
    for byte in name.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(16777619);
    }
    hash.max(1)
}

pub fn prepare_dml_devices(
    config: &BoardConfig,
    parser_config: &DmlParserConfig,
    driver_metadata: Option<&DriverSpecificMetadata>,
    driver_power_configs: Option<&DriverSpecificPowerConfigs>,
    enabled_nodes: &[String],
) -> anyhow::Result<Vec<PreparedDevice>> {
    let devices = config
        .devices
        .as_ref()
        .ok_or_else(|| anyhow!("devices field is missing in BoardConfig"))?;

    let disabled_devices: HashSet<&str> = devices
        .iter()
        .filter(|d| crate::is_device_disabled(d, enabled_nodes))
        .filter_map(|d| d.name.as_deref())
        .collect();

    let index_by_name: HashMap<&str, usize> = devices
        .iter()
        .enumerate()
        .filter_map(|(idx, d)| d.name.as_deref().map(|name| (name, idx)))
        .collect();
    let index_by_irq_controller: HashMap<u32, usize> = devices
        .iter()
        .enumerate()
        .filter_map(|(idx, d)| d.interrupt_controller_id.map(|id| (id, idx)))
        .collect();

    let mut pdev_constraints_by_node = HashMap::<&str, &fdr::Dictionary>::new();
    let mut aggregates_by_node = HashMap::<&str, Vec<(&AggregateEntry, &ResourceEntry)>>::new();
    for agg in config.aggregates.as_deref().unwrap_or_default() {
        let is_pdev_service = matches!(agg.provider.as_deref(), Some("parent" | "pdev"))
            && agg.service.as_deref() == Some("fuchsia.hardware.platform.device.Service");
        // Skip aggregates from disabled providers upfront (unless it's a virtual pdev service).
        if !is_pdev_service && agg.provider.as_deref().is_some_and(|p| disabled_devices.contains(p))
        {
            continue;
        }
        for res in agg.resources.as_deref().unwrap_or_default() {
            let Some(node_name) = res.node.as_deref() else { continue };
            if is_pdev_service && let Some(constraint) = res.constraint.as_ref() {
                pdev_constraints_by_node.entry(node_name).or_insert(constraint);
            }
            aggregates_by_node.entry(node_name).or_default().push((agg, res));
        }
    }
    // Sort each node's aggregate entries by `(provider, service)` (stable sort preserves the
    // relative order of offers from the same provider and service) so that adding an offer in an
    // earlier shard does not reorder `CompositeNodeSpec.parents2` on unrelated devices.
    for list in aggregates_by_node.values_mut() {
        list.sort_by_key(|(agg, _)| (agg.provider.as_deref(), agg.service.as_deref()));
    }

    let mut prepared_devices: Vec<Option<PreparedDevice>> = devices.iter().map(|_| None).collect();
    let mut providers_by_device = vec![Vec::new(); devices.len()];

    for (idx, dev) in devices.iter().enumerate() {
        let dev_name = dev.name.as_deref().unwrap_or("");
        if disabled_devices.contains(dev_name) {
            continue;
        }
        if dev.disabled.unwrap_or(false) {
            log::info!("Publishing disabled DML device '{}' due to runtime override", dev_name);
        }
        let instance_id = compute_instance_id(dev_name);
        let mut node = fpbus::Node {
            name: dev.name.clone(),
            vid: Some(0),
            pid: Some(0),
            did: Some(0),
            instance_id: Some(instance_id),
            driver_host: dev.driver_host.clone(),
            ..Default::default()
        };
        node.interrupt_controller_id = dev.interrupt_controller_id;

        if let Some(compatible) = &dev.compatible {
            node.properties = Some(vec![fdf_framework::NodeProperty2 {
                key: "fuchsia.COMPATIBLE".to_string(),
                value: fdf_framework::NodePropertyValue::StringValue(compatible.clone()),
            }]);
            node.did = Some(BIND_PLATFORM_DEV_DID_DEVICETREE);
            node.vid = Some(BIND_PLATFORM_DEV_VID_GENERIC);
        }

        let mut mmio_list = Vec::new();
        let mut irq_list = Vec::new();
        let mut irq_controllers = Vec::new();
        let mut bti_list = Vec::new();
        let mut smc_list = Vec::new();
        let mut boot_metadata_list = Vec::new();

        if let Some(&pdev_dict) = pdev_constraints_by_node.get(dev_name) {
            for mmio in crate::mmio_list(pdev_dict) {
                mmio_list.push(fpbus::Mmio {
                    base: Some(mmio.base),
                    length: Some(mmio.length),
                    name: mmio.name,
                    ..Default::default()
                });
            }
            for irq in crate::irq_list(pdev_dict) {
                let irq_spec = match irq.controller {
                    Some(controller_id) => {
                        if !irq_controllers.contains(&controller_id) {
                            irq_controllers.push(controller_id);
                        }
                        fpbus::IrqSpec::UserspaceIrq(fpbus::UserspaceIrq {
                            irq: irq.number,
                            controller_id,
                        })
                    }
                    None => fpbus::IrqSpec::Irq(irq.number),
                };
                irq_list.push(fpbus::Irq {
                    irq: Some(irq_spec),
                    mode: Some(map_interrupt_mode(irq.mode.as_deref())),
                    name: irq.name.clone(),
                    wake_vector: irq.wake_vector,
                    ..Default::default()
                });
            }
            for bti in crate::bti_list(pdev_dict)? {
                bti_list.push(fpbus::Bti {
                    iommu_id: Some(bti.iommu_id),
                    bti_id: Some(bti.id),
                    name: bti.name,
                    bti_id_mask: bti.bti_id_mask,
                    ..Default::default()
                });
            }
            for smc in crate::smc_list(pdev_dict) {
                smc_list.push(fpbus::Smc {
                    service_call_num_base: Some(smc.service_call_num_base),
                    count: Some(smc.count),
                    exclusive: Some(smc.exclusive),
                    name: smc.name,
                    ..Default::default()
                });
            }
            for bm in crate::boot_metadata_list(pdev_dict) {
                boot_metadata_list.push(fpbus::BootMetadata {
                    zbi_type: Some(bm.zbi_type),
                    zbi_extra: bm.zbi_extra.or(Some(0)),
                    ..Default::default()
                });
            }
        }

        if !mmio_list.is_empty() {
            node.mmio = Some(mmio_list);
        }
        if !irq_list.is_empty() {
            node.irq = Some(irq_list);
        }
        if !bti_list.is_empty() {
            node.bti = Some(bti_list);
        }
        if !smc_list.is_empty() {
            node.smc = Some(smc_list);
        }
        if !boot_metadata_list.is_empty() {
            node.boot_metadata = Some(boot_metadata_list);
        }

        let mut metadata_list = Vec::new();

        // Handle static metadata from config
        if let Some(metadata) = &dev.metadata {
            for meta in metadata {
                let data =
                    meta.data.clone().ok_or_else(|| anyhow!("Static metadata missing data"))?;
                metadata_list.push(fpbus::Metadata {
                    id: meta.id.clone(),
                    data: Some(data),
                    ..Default::default()
                });
            }
        }

        if let Some(generators) = driver_metadata.and_then(|drv_meta| drv_meta.get(dev_name)) {
            for (metadata_id, gen_fn) in *generators {
                let data = gen_fn()
                    .with_context(|| format!("Failed to generate metadata for {}", dev_name))?;
                metadata_list.push(fpbus::Metadata {
                    id: Some(metadata_id.to_string()),
                    data: Some(data),
                    ..Default::default()
                });
            }
        }

        if !metadata_list.is_empty() {
            node.metadata = Some(metadata_list);
        }

        if let Some(gen_fn) = driver_power_configs.and_then(|configs| configs.get(dev_name)) {
            let power_config = gen_fn()
                .with_context(|| format!("Failed to generate power config for {}", dev_name))?;
            if !power_config.is_empty() {
                node.power_config = Some(power_config);
            }
        }

        let mut resource_parents = Vec::new();
        let mut generated_keys = HashSet::new();
        let mut providers = Vec::new();
        for &(agg, res) in aggregates_by_node.get(dev_name).into_iter().flatten() {
            let provider_idx = agg.provider.as_deref().and_then(|p| index_by_name.get(p).copied());
            providers.extend(provider_idx);
            if dev.compatible.is_some()
                && matches!(
                    agg.service.as_deref(),
                    Some("fuchsia.hardware.platform.device.Service")
                        | Some("fuchsia.hardware.interrupt.ControllerRegistryService")
                )
            {
                continue;
            }
            let provider_id = provider_idx.and_then(|idx| devices[idx].id);
            if let Some((parent, key)) =
                generate_parent_spec_generic(agg, res, provider_id, parser_config)?
            {
                if generated_keys.insert(key) {
                    if matches!(agg.provider.as_deref(), Some("parent") | Some("pdev")) {
                        resource_parents.insert(0, parent);
                    } else {
                        resource_parents.push(parent);
                    }
                }
            }
        }

        providers.extend(irq_controllers.iter().filter_map(|id| index_by_irq_controller.get(id)));
        providers_by_device[idx] = providers;

        for (parent, key) in generate_interrupt_controller_parents(&irq_controllers) {
            if generated_keys.insert(key) {
                resource_parents.push(parent);
            }
        }

        if resource_parents.is_empty() && dev.driver_host.is_some() {
            node.driver_host = dev.driver_host.clone();
        }

        let mut spec = fdf_framework::CompositeNodeSpec {
            name: dev.name.clone(),
            driver_host: dev.driver_host.clone(),
            ..Default::default()
        };

        let mut parents2 = Vec::new();

        // 1. Generate pdev parent if compatible is present
        if let Some(compatible) = &dev.compatible {
            let pdev_parent = fdf_framework::ParentSpec2 {
                bind_rules: vec![
                    make_accept_bind_rule(
                        "fuchsia.BIND_PROTOCOL",
                        property_int(BIND_PROTOCOL_DEVICE),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_VID",
                        property_int(BIND_PLATFORM_DEV_VID_GENERIC),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_DID",
                        property_int(BIND_PLATFORM_DEV_DID_DEVICETREE),
                    ),
                    make_accept_bind_rule(
                        "fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID",
                        property_int(instance_id),
                    ),
                    make_accept_bind_rule("fuchsia.COMPATIBLE", property_string(compatible)),
                ],
                properties: vec![
                    // TODO(https://fxbug.dev/555962083): Restore `fuchsia.NAME = "pdev"` once all
                    // composite drivers are migrated from `primary parent "devicetree"` to `"pdev"`
                    // (`driver-index` rejects `ParentSpec2.properties` when `fuchsia.NAME` does not
                    // equal the `.bind` primary parent symbol name).
                    make_property2("fuchsia.BIND_PROTOCOL", property_int(BIND_PROTOCOL_DEVICE)),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_VID",
                        property_int(BIND_PLATFORM_DEV_VID_GENERIC),
                    ),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_DID",
                        property_int(BIND_PLATFORM_DEV_DID_DEVICETREE),
                    ),
                    make_property2(
                        "fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID",
                        property_int(instance_id),
                    ),
                    make_property2("fuchsia.COMPATIBLE", property_string(compatible)),
                    make_property2(
                        "fuchsia.devicetree.FIRST_COMPATIBLE",
                        property_string(compatible),
                    ),
                    make_property2(
                        "fuchsia.Service",
                        property_string("fuchsia.hardware.platform.device.Service"),
                    ),
                ],
            };
            parents2.push(pdev_parent);
        }

        parents2.extend(resource_parents);

        let spec_opt = if !parents2.is_empty() {
            spec.parents2 = Some(parents2);
            Some(spec)
        } else {
            None
        };

        let node_opt = if dev.compatible.is_some() { Some(node) } else { None };

        prepared_devices[idx] = Some((dev_name.to_string(), spec_opt, node_opt));
    }

    let order = publish_order(devices, &providers_by_device);
    Ok(order.into_iter().filter_map(|idx| prepared_devices[idx].take()).collect())
}

/// Generates composite parent specifications for distinct userspace interrupt controllers
/// referenced by a device's interrupts, in first-reference order.
fn generate_interrupt_controller_parents(
    irq_controllers: &[u32],
) -> Vec<(fdf_framework::ParentSpec2, String)> {
    irq_controllers
        .iter()
        .enumerate()
        .map(|(idx, &controller_id)| {
            let key = format!("interrupt-controller-{idx}");
            let parent = fdf_framework::ParentSpec2 {
                bind_rules: vec![
                    make_accept_bind_rule("fuchsia.NAME", property_string("interrupt-controller")),
                    make_accept_bind_rule("fuchsia.ID", property_int(controller_id)),
                ],
                properties: vec![make_property2("fuchsia.NAME", property_string(&key))],
            };
            (parent, key)
        })
        .collect()
}

#[cfg(target_os = "fuchsia")]
mod fuchsia {
    use super::*;
    use crate::{Iommu, IommuType};
    use fdf_fidl::DriverChannel;
    use futures::future::try_join_all;
    use zx;

    pub async fn publish_dml_devices(
        pbus: &fidl_next::Client<fpbus::PlatformBus, DriverChannel>,
        composite_manager: &fidl_next::Client<fdf_framework::CompositeNodeManager, zx::Channel>,
        config: &BoardConfig,
        parser_config: &DmlParserConfig,
        driver_metadata: Option<&DriverSpecificMetadata>,
        driver_power_configs: Option<&DriverSpecificPowerConfigs>,
        enabled_nodes: &[String],
    ) -> anyhow::Result<()> {
        register_iommus(pbus, config.iommus.as_deref())
            .await
            .context("Failed to register IOMMUs")?;

        let prepared_devices = prepare_dml_devices(
            config,
            parser_config,
            driver_metadata,
            driver_power_configs,
            enabled_nodes,
        )?;

        let mut spec_futures = Vec::with_capacity(prepared_devices.len());
        let mut node_futures = Vec::with_capacity(prepared_devices.len());

        for (dev_name, spec_opt, node_opt) in prepared_devices {
            if let Some(spec) = spec_opt {
                let dev_name = dev_name.clone();
                spec_futures.push(async move {
                    log::debug!(
                        "DML-CONFIG: Adding composite spec: name={:?}, parents2={:?}",
                        spec.name,
                        spec.parents2
                    );
                    composite_manager
                        .add_spec_with(spec)
                        .await
                        .with_context(|| format!("AddSpec request failed for {dev_name}"))?
                        .map_err(|e| anyhow!("AddSpec failed for {dev_name}: {e:?}"))?;
                    Ok::<(), anyhow::Error>(())
                });
            }

            if let Some(node) = node_opt {
                node_futures.push(async move {
                    log::debug!("DML-CONFIG: Adding node: {}", dev_name);
                    pbus.node_add(node)
                        .await
                        .with_context(|| format!("NodeAdd request failed for {dev_name}"))?
                        .with_context(|| format!("NodeAdd failed for {dev_name}"))?;
                    Ok::<(), anyhow::Error>(())
                });
            }
        }

        try_join_all(spec_futures).await?;
        try_join_all(node_futures).await?;

        Ok(())
    }

    /// Registers `iommus` with the `pbus`.
    async fn register_iommus(
        pbus: &fidl_next::Client<fpbus::PlatformBus, DriverChannel>,
        iommus: Option<&[Iommu]>,
    ) -> anyhow::Result<()> {
        let Some(iommus) = iommus else {
            return Ok(());
        };

        let futures = iommus
            .iter()
            .map(|iommu| {
                let iommu_name = iommu.name.as_deref().unwrap_or("unnamed");
                let iommu_id =
                    iommu.id.with_context(|| format!("IOMMU {iommu_name:?} missing id"))?;
                let fpbus_iommu = match &iommu.iommu_type {
                    Some(IommuType::ArmSmmu(arm_smmu)) => fpbus::Iommu::ArmSmmu(fpbus::ArmSmmu {
                        base_address: arm_smmu.base_address,
                    }),
                    Some(IommuType::StubIommu(_)) | None => fpbus::Iommu::StubIommu(()),
                    _ => {
                        bail!("Unsupported IOMMU type for IOMMU {iommu_name:?}");
                    }
                };
                let future = async move {
                    log::info!("Registering IOMMU '{iommu_name}' (id: {iommu_id})");
                    pbus.register_iommu(iommu_id, &fpbus_iommu)
                        .await
                        .with_context(|| {
                            format!(
                                "Failed to send RegisterIommu FIDL request for IOMMU {iommu_name:?}"
                            )
                        })?
                        .with_context(|| format!("Failed to register IOMMU {iommu_name:?}"))?;
                    Ok::<(), anyhow::Error>(())
                };
                Ok(future)
            })
            .collect::<Result<Vec<_>, _>>()?;
        try_join_all(futures).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pci_service_parent_spec() {
        let res = ResourceEntry {
            name: Some("pci".to_string()),
            node: Some("mali-gpu".to_string()),
            constraint: Some(fdr::Dictionary {
                entries: Some(vec![
                    fdr::DictionaryEntry {
                        key: "vid".to_string(),
                        value: fdr::DictionaryValue::Int64(0x13b5),
                    },
                    fdr::DictionaryEntry {
                        key: "did".to_string(),
                        value: fdr::DictionaryValue::Int64(0x7212),
                    },
                    fdr::DictionaryEntry {
                        key: "topo".to_string(),
                        value: fdr::DictionaryValue::Int64(256),
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let agg = AggregateEntry {
            provider: Some("pcie-c500000".to_string()),
            service: Some("fuchsia.hardware.pci.Service".to_string()),
            resources: Some(vec![res.clone()]),
            ..Default::default()
        };

        let (parent, key) =
            generate_parent_spec_generic(&agg, &res, None, &DEFAULT_DML_PARSER_CONFIG)
                .expect("should succeed")
                .expect("should return parent spec");

        assert_eq!(key, "pci");
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_VID"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(0x13b5)]));
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_DID"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(0x7212)]));
        assert!(parent.bind_rules.iter().any(|r| r.key == "fuchsia.BIND_PCI_TOPO"
            && r.values == vec![fdf_framework::NodePropertyValue::IntValue(256)]));
        assert!(parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_VID"
            && p.value == fdf_framework::NodePropertyValue::IntValue(0x13b5)));
        assert!(parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_DID"
            && p.value == fdf_framework::NodePropertyValue::IntValue(0x7212)));
        assert!(!parent.properties.iter().any(|p| p.key == "fuchsia.BIND_PCI_TOPO"));
    }

    #[test]
    fn test_interrupt_controller_parents_dedup_and_order() {
        let pdev_dict = fdr::Dictionary {
            entries: Some(vec![
                fdr::DictionaryEntry {
                    key: "interrupts.0.number".to_string(),
                    value: fdr::DictionaryValue::Int64(574),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.1.number".to_string(),
                    value: fdr::DictionaryValue::Int64(17),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.1.controller".to_string(),
                    value: fdr::DictionaryValue::Int64(290),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.2.number".to_string(),
                    value: fdr::DictionaryValue::Int64(18),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.2.controller".to_string(),
                    value: fdr::DictionaryValue::Int64(290),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.3.number".to_string(),
                    value: fdr::DictionaryValue::Int64(4),
                },
                fdr::DictionaryEntry {
                    key: "interrupts.3.controller".to_string(),
                    value: fdr::DictionaryValue::Int64(289),
                },
            ]),
            ..Default::default()
        };

        let mut irq_controllers = Vec::new();
        for irq in crate::irq_list(&pdev_dict) {
            if let Some(controller_id) = irq.controller {
                if !irq_controllers.contains(&controller_id) {
                    irq_controllers.push(controller_id);
                }
            }
        }
        assert_eq!(irq_controllers, vec![290, 289]);

        let parents = generate_interrupt_controller_parents(&irq_controllers);
        assert_eq!(parents.len(), 2);

        let (parent0, key0) = &parents[0];
        assert_eq!(key0, "interrupt-controller-0");
        assert_eq!(
            parent0.bind_rules,
            vec![
                make_accept_bind_rule("fuchsia.NAME", property_string("interrupt-controller")),
                make_accept_bind_rule("fuchsia.ID", property_int(290)),
            ]
        );
        assert_eq!(
            parent0.properties,
            vec![make_property2("fuchsia.NAME", property_string("interrupt-controller-0"))]
        );

        let (parent1, key1) = &parents[1];
        assert_eq!(key1, "interrupt-controller-1");
        assert_eq!(
            parent1.bind_rules,
            vec![
                make_accept_bind_rule("fuchsia.NAME", property_string("interrupt-controller")),
                make_accept_bind_rule("fuchsia.ID", property_int(289)),
            ]
        );
        assert_eq!(
            parent1.properties,
            vec![make_property2("fuchsia.NAME", property_string("interrupt-controller-1"))]
        );
    }

    #[test]
    fn test_prepare_dml_devices() {
        let parser_config = DmlParserConfig {
            service_configs: phf::phf_map! {
                "fuchsia.hardware.gpio.Service" => ServiceBindConfig {
                    rules: &[PropertyRule {
                        bind_key: "fuchsia.gpio.CONTROLLER_ID",
                        sources: &[ValueSource::ProviderId],
                        value_type: RuleValueType::Integer,
                        destination: Destination::Both,
                    }],
                    ..DEFAULT_ID_SERVICE_BIND_CONFIG
                },
            },
        };

        let config = BoardConfig {
            devices: Some(vec![
                crate::Device {
                    name: Some("gpio-controller".to_string()),
                    compatible: Some("acme,gpio".to_string()),
                    id: Some(42),
                    interrupt_controller_id: Some(290),
                    ..Default::default()
                },
                crate::Device {
                    name: Some("disabled-dev".to_string()),
                    compatible: Some("acme,disabled".to_string()),
                    disabled: Some(true),
                    ..Default::default()
                },
                crate::Device {
                    name: Some("consumer-dev".to_string()),
                    compatible: Some("acme,consumer".to_string()),
                    ..Default::default()
                },
            ]),
            aggregates: Some(vec![
                AggregateEntry {
                    provider: Some("pdev".to_string()),
                    service: Some("fuchsia.hardware.platform.device.Service".to_string()),
                    resources: Some(vec![
                        ResourceEntry {
                            name: Some("pdev".to_string()),
                            node: Some("gpio-controller".to_string()),
                            constraint: Some(fdr::Dictionary {
                                entries: Some(vec![
                                    fdr::DictionaryEntry {
                                        key: "mmio.0.base".to_string(),
                                        value: fdr::DictionaryValue::Int64(0x1000),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "mmio.0.length".to_string(),
                                        value: fdr::DictionaryValue::Int64(0x100),
                                    },
                                ]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        ResourceEntry {
                            name: Some("pdev".to_string()),
                            node: Some("consumer-dev".to_string()),
                            constraint: Some(fdr::Dictionary {
                                entries: Some(vec![
                                    fdr::DictionaryEntry {
                                        key: "mmio.0.base".to_string(),
                                        value: fdr::DictionaryValue::Int64(0x2000),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "mmio.0.length".to_string(),
                                        value: fdr::DictionaryValue::Int64(0x200),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "interrupts.0.number".to_string(),
                                        value: fdr::DictionaryValue::Int64(17),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "interrupts.0.controller".to_string(),
                                        value: fdr::DictionaryValue::Int64(290),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "interrupts.0.mode".to_string(),
                                        value: fdr::DictionaryValue::Str("EdgeHigh".to_string()),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "btis.0.id".to_string(),
                                        value: fdr::DictionaryValue::Int64(0),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "btis.0.iommu_id".to_string(),
                                        value: fdr::DictionaryValue::Int64(1),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "btis.0.mask".to_string(),
                                        value: fdr::DictionaryValue::Int64(0x1f),
                                    },
                                    fdr::DictionaryEntry {
                                        key: "btis.0.name".to_string(),
                                        value: fdr::DictionaryValue::Str("dpu".to_string()),
                                    },
                                ]),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                },
                AggregateEntry {
                    provider: Some("disabled-dev".to_string()),
                    service: Some("fuchsia.hardware.gpio.Service".to_string()),
                    resources: Some(vec![ResourceEntry {
                        name: Some("gpio-from-disabled".to_string()),
                        node: Some("consumer-dev".to_string()),
                        constraint: Some(fdr::Dictionary {
                            entries: Some(vec![fdr::DictionaryEntry {
                                key: "id".to_string(),
                                value: fdr::DictionaryValue::Int64(99),
                            }]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                AggregateEntry {
                    provider: Some("gpio-controller".to_string()),
                    service: Some("fuchsia.hardware.gpio.Service".to_string()),
                    resources: Some(vec![ResourceEntry {
                        name: Some("gpio-reset".to_string()),
                        node: Some("consumer-dev".to_string()),
                        constraint: Some(fdr::Dictionary {
                            entries: Some(vec![fdr::DictionaryEntry {
                                key: "id".to_string(),
                                value: fdr::DictionaryValue::Int64(7),
                            }]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        };

        let prepared =
            prepare_dml_devices(&config, &parser_config, None, None, &[]).expect("should succeed");

        assert_eq!(prepared.len(), 2);

        // First device: gpio-controller
        let (name0, spec0_opt, node0_opt) = &prepared[0];
        assert_eq!(name0, "gpio-controller");
        let node0 = node0_opt.as_ref().expect("should have node");
        assert_eq!(node0.instance_id, Some(compute_instance_id("gpio-controller")));
        assert_eq!(node0.interrupt_controller_id, Some(290));
        let mmios0 = node0.mmio.as_ref().expect("should have mmio");
        assert_eq!(mmios0.len(), 1);
        assert_eq!(mmios0[0].base, Some(0x1000));
        assert_eq!(mmios0[0].length, Some(0x100));
        let spec0 = spec0_opt.as_ref().expect("should have spec");
        assert_eq!(spec0.parents2.as_ref().expect("should have parents").len(), 1);

        // Second device: consumer-dev (idx 1 was disabled)
        let (name1, spec1_opt, node1_opt) = &prepared[1];
        assert_eq!(name1, "consumer-dev");
        let node1 = node1_opt.as_ref().expect("should have node");
        assert_eq!(node1.instance_id, Some(compute_instance_id("consumer-dev")));
        let mmios1 = node1.mmio.as_ref().expect("should have mmio");
        assert_eq!(mmios1.len(), 1);
        assert_eq!(mmios1[0].base, Some(0x2000));
        assert_eq!(mmios1[0].length, Some(0x200));
        let irqs1 = node1.irq.as_ref().expect("should have irq");
        assert_eq!(irqs1.len(), 1);
        assert_eq!(
            irqs1[0].irq,
            Some(fpbus::IrqSpec::UserspaceIrq(fpbus::UserspaceIrq { irq: 17, controller_id: 290 }))
        );
        assert_eq!(irqs1[0].mode, Some(fpbus::ZirconInterruptMode::EdgeHigh));

        let btis1 = node1.bti.as_ref().expect("should have bti");
        assert_eq!(btis1.len(), 1);
        assert_eq!(btis1[0].iommu_id, Some(1));
        assert_eq!(btis1[0].bti_id, Some(0));
        assert_eq!(btis1[0].bti_id_mask, Some(0x1f));
        assert_eq!(btis1[0].name.as_deref(), Some("dpu"));

        let spec1 = spec1_opt.as_ref().expect("should have spec");
        let parents1 = spec1.parents2.as_ref().expect("should have parents");
        // Expected parents:
        // 0: primary pdev parent
        // 1: gpio-reset aggregate parent (with CONTROLLER_ID = 42 and ID = 7; disabled-dev offer skipped)
        // 2: interrupt-controller-0 parent (ID = 290)
        assert_eq!(parents1.len(), 3);
        assert!(parents1[0].bind_rules.iter().any(|r| {
            r.key == "fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID"
                && r.values
                    == vec![fdf_framework::NodePropertyValue::IntValue(compute_instance_id(
                        "consumer-dev",
                    ))]
        }));
        assert!(parents1[1].properties.iter().any(|p| {
            p.key == "fuchsia.NAME"
                && p.value
                    == fdf_framework::NodePropertyValue::StringValue("gpio-reset".to_string())
        }));
        assert!(parents1[1].bind_rules.iter().any(|r| {
            r.key == "fuchsia.gpio.CONTROLLER_ID"
                && r.values == vec![fdf_framework::NodePropertyValue::IntValue(42)]
        }));
        assert!(parents1[1].bind_rules.iter().any(|r| {
            r.key == "fuchsia.ID" && r.values == vec![fdf_framework::NodePropertyValue::IntValue(7)]
        }));
        assert!(parents1[2].properties.iter().any(|p| {
            p.key == "fuchsia.NAME"
                && p.value
                    == fdf_framework::NodePropertyValue::StringValue(
                        "interrupt-controller-0".to_string(),
                    )
        }));
        assert!(parents1[2].bind_rules.iter().any(|r| {
            r.key == "fuchsia.ID"
                && r.values == vec![fdf_framework::NodePropertyValue::IntValue(290)]
        }));
    }

    #[test]
    fn test_prepare_dml_devices_publish_order() {
        let device = |name: &str| crate::Device {
            name: Some(name.to_string()),
            compatible: Some(format!("acme,{name}")),
            ..Default::default()
        };
        let offer = |provider: &str, node: &str| AggregateEntry {
            provider: Some(provider.to_string()),
            service: Some("fuchsia.hardware.gpio.Service".to_string()),
            resources: Some(vec![ResourceEntry {
                name: Some(format!("{provider}-gpio")),
                node: Some(node.to_string()),
                constraint: Some(fdr::Dictionary::default()),
                ..Default::default()
            }]),
            ..Default::default()
        };
        let pdev_irq = |node: &str, controller: i64| AggregateEntry {
            provider: Some("pdev".to_string()),
            service: Some("fuchsia.hardware.platform.device.Service".to_string()),
            resources: Some(vec![ResourceEntry {
                name: Some("pdev".to_string()),
                node: Some(node.to_string()),
                constraint: Some(fdr::Dictionary {
                    entries: Some(vec![
                        fdr::DictionaryEntry {
                            key: "interrupts.0.number".to_string(),
                            value: fdr::DictionaryValue::Int64(5),
                        },
                        fdr::DictionaryEntry {
                            key: "interrupts.0.controller".to_string(),
                            value: fdr::DictionaryValue::Int64(controller),
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let config = BoardConfig {
            devices: Some(vec![
                device("low"),
                // `storage` is published first because it has the highest priority, but only
                // after its gpio provider and its interrupt controller.
                crate::Device { publish_priority: Some(10), ..device("storage") },
                device("gpio"),
                crate::Device { interrupt_controller_id: Some(1), ..device("irq-ctrl") },
                // `gpio` and `cycle` provide to each other; the cycle must not hang.
                device("cycle"),
                crate::Device { publish_priority: Some(5), ..device("usb") },
                crate::Device { disabled: Some(true), ..device("disabled") },
            ]),
            aggregates: Some(vec![
                offer("gpio", "storage"),
                offer("cycle", "gpio"),
                offer("gpio", "cycle"),
                offer("disabled", "usb"),
                pdev_irq("storage", 1),
            ]),
            ..Default::default()
        };

        let prepared = prepare_dml_devices(&config, &DEFAULT_DML_PARSER_CONFIG, None, None, &[])
            .expect("should succeed");
        let names: Vec<&str> = prepared.iter().map(|(name, _, _)| name.as_str()).collect();
        assert_eq!(names, ["cycle", "gpio", "irq-ctrl", "storage", "usb", "low"]);

        // Reordering must not change the instance IDs, which are hashed from device names.
        let instance_id = |name: &str| {
            prepared
                .iter()
                .find(|(n, _, _)| n == name)
                .and_then(|(_, _, node)| node.as_ref())
                .and_then(|node| node.instance_id)
        };
        assert_eq!(instance_id("low"), Some(compute_instance_id("low")));
        assert_eq!(instance_id("storage"), Some(compute_instance_id("storage")));
        assert_eq!(instance_id("usb"), Some(compute_instance_id("usb")));
    }
}
