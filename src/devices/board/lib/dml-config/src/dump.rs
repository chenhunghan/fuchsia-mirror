// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::{
    DmlParserConfig, DriverSpecificMetadata, DriverSpecificPowerConfigs, PreparedDevice,
    prepare_dml_devices,
};
use crate::{BoardConfig, Iommu, IommuType};
use anyhow::{Context, bail};
#[cfg(not(target_os = "fuchsia"))]
use fidl_fuchsia_driver_framework_common as fdf_framework;
use fidl_fuchsia_driver_metadata_common as fdr;
#[cfg(not(target_os = "fuchsia"))]
use fidl_fuchsia_hardware_platform_bus_common as fpbus;
#[cfg(target_os = "fuchsia")]
use fidl_next_fuchsia_driver_framework as fdf_framework;
#[cfg(target_os = "fuchsia")]
use fidl_next_fuchsia_hardware_platform_bus as fpbus;
use std::fmt::Write as _;

/// Optional hook for board-specific `dml_parsed` binaries to decode custom FIDL metadata payloads
/// that are not persisted `fuchsia.driver.metadata.Dictionary` tables.
///
/// If this returns `Some(lines)`, each line should already be indented with `indent` and end with
/// a newline.
pub type CustomMetadataFormatter = fn(id: &str, data: &[u8], indent: &str) -> Option<String>;

fn stringify_irq_mode(mode: fpbus::ZirconInterruptMode) -> &'static str {
    match mode {
        fpbus::ZirconInterruptMode::Default => "DEFAULT",
        fpbus::ZirconInterruptMode::EdgeLow => "EDGE_LOW",
        fpbus::ZirconInterruptMode::EdgeHigh => "EDGE_HIGH",
        fpbus::ZirconInterruptMode::LevelLow => "LEVEL_LOW",
        fpbus::ZirconInterruptMode::LevelHigh => "LEVEL_HIGH",
        fpbus::ZirconInterruptMode::EdgeBoth => "EDGE_BOTH",
    }
}

fn stringify_protocol(val: u32) -> String {
    let name = match val {
        1 => "BLOCK",
        20 => "GPIO",
        24 => "I2C",
        31 => "PCI",
        33 => "USB",
        47 => "BT_HCI",
        54 => "SDHCI",
        55 => "SDMMC",
        72 => "POWER",
        85 => "PDEV",
        90 => "CLOCK",
        121 => "SPI",
        129 => "CPU_CTRL",
        147 => "ADC",
        152 => "REGISTERS",
        153 => "DAI",
        171 => "AUDIO_COMPOSITE",
        _ => return val.to_string(),
    };
    format!("{name} ({val})")
}

fn stringify_vid(val: u32) -> String {
    let name = match val {
        0 => "GENERIC",
        1 => "QEMU",
        3 => "GOOGLE",
        5 => "AMLOGIC",
        6 => "BROADCOM",
        9 => "NXP",
        19 => "QUALCOMM",
        _ => return val.to_string(),
    };
    format!("{name} ({val})")
}

fn stringify_pid(vid: u32, val: u32) -> String {
    if vid == 3 {
        let name = match val {
            1 => Some("GAUSS"),
            2 => Some("MACHINA"),
            3 => Some("ASTRO"),
            5 => Some("SHERLOCK"),
            6 => Some("CLEO"),
            7 => Some("EAGLE"),
            8 => Some("VISALIA"),
            9 => Some("C18"),
            10 => Some("NELSON"),
            11 => Some("VS680_EVK"),
            12 => Some("LUIS"),
            13 => Some("GOLDFISH"),
            14 => Some("MOTMOT"),
            15 => Some("AV400"),
            16 => Some("PINECREST"),
            17 => Some("CLOVER"),
            18 => Some("VIOLET"),
            19 => Some("KOLA"),
            20 => Some("IRIS"),
            _ => None,
        };
        if let Some(name) = name {
            return format!("{name} ({val})");
        }
    }
    val.to_string()
}

fn stringify_did(vid: u32, pid: u32, val: u32) -> String {
    if vid == 0 && pid == 0 && val == 50 {
        return format!("DEVICETREE ({val})");
    }
    val.to_string()
}

fn get_vid_from_props(props: &[fdf_framework::NodeProperty2]) -> u32 {
    for prop in props {
        if prop.key == "fuchsia.BIND_PLATFORM_DEV_VID"
            && let fdf_framework::NodePropertyValue::IntValue(v) = prop.value
        {
            return v;
        }
    }
    0
}

fn get_pid_from_props(props: &[fdf_framework::NodeProperty2]) -> u32 {
    for prop in props {
        if prop.key == "fuchsia.BIND_PLATFORM_DEV_PID"
            && let fdf_framework::NodePropertyValue::IntValue(v) = prop.value
        {
            return v;
        }
    }
    0
}

fn get_vid_from_rules(rules: &[fdf_framework::BindRule2]) -> u32 {
    for rule in rules {
        if rule.key == "fuchsia.BIND_PLATFORM_DEV_VID" {
            for val in &rule.values {
                if let fdf_framework::NodePropertyValue::IntValue(v) = val {
                    return *v;
                }
            }
        }
    }
    0
}

fn get_pid_from_rules(rules: &[fdf_framework::BindRule2]) -> u32 {
    for rule in rules {
        if rule.key == "fuchsia.BIND_PLATFORM_DEV_PID" {
            for val in &rule.values {
                if let fdf_framework::NodePropertyValue::IntValue(v) = val {
                    return *v;
                }
            }
        }
    }
    0
}

fn stringify_property_value(
    value: &fdf_framework::NodePropertyValue,
    key: &str,
    vid: u32,
    pid: u32,
) -> String {
    if let fdf_framework::NodePropertyValue::IntValue(val) = value {
        match key {
            "fuchsia.BIND_PROTOCOL" => return stringify_protocol(*val),
            "fuchsia.BIND_PLATFORM_DEV_VID" => return stringify_vid(*val),
            "fuchsia.BIND_PLATFORM_DEV_PID" => return stringify_pid(vid, *val),
            "fuchsia.BIND_PLATFORM_DEV_DID" => return stringify_did(vid, pid, *val),
            _ => {}
        }
    }

    match value {
        fdf_framework::NodePropertyValue::BoolValue(b) => {
            if *b { "true" } else { "false" }.to_string()
        }
        fdf_framework::NodePropertyValue::IntValue(i) => i.to_string(),
        fdf_framework::NodePropertyValue::EnumValue(e) => e.clone(),
        fdf_framework::NodePropertyValue::StringValue(s) => format!("\"{s}\""),
        _ => "<unknown>".to_string(),
    }
}

fn stringify_properties(props: &[fdf_framework::NodeProperty2], out: &mut String, indent: &str) {
    let vid = get_vid_from_props(props);
    let pid = get_pid_from_props(props);

    for prop in props {
        let val = stringify_property_value(&prop.value, &prop.key, vid, pid);
        let _ = writeln!(out, "{indent}property {{ {} = {val} }}", prop.key);
    }
}

fn stringify_parent_spec(spec: &fdf_framework::ParentSpec2, out: &mut String, indent: &str) {
    if spec.bind_rules.is_empty() && spec.properties.is_empty() {
        return;
    }

    let mut vid = get_vid_from_rules(&spec.bind_rules);
    if vid == 0 {
        vid = get_vid_from_props(&spec.properties);
    }
    let mut pid = get_pid_from_rules(&spec.bind_rules);
    if pid == 0 {
        pid = get_pid_from_props(&spec.properties);
    }

    let _ = writeln!(out, "{indent}bind_rules {{");
    for rule in &spec.bind_rules {
        let cond = match rule.condition {
            fdf_framework::Condition::Accept => "Accept",
            fdf_framework::Condition::Reject => "Reject",
            _ => "Unknown",
        };
        let values: Vec<String> =
            rule.values.iter().map(|v| stringify_property_value(v, &rule.key, vid, pid)).collect();
        let _ = writeln!(out, "{indent}  {cond}({}, [{}])", rule.key, values.join(", "));
    }
    let _ = writeln!(out, "{indent}}}");

    if !spec.properties.is_empty() {
        let _ = writeln!(out, "{indent}properties {{");
        let prop_indent = format!("{indent}  ");
        stringify_properties(&spec.properties, out, &prop_indent);
        let _ = writeln!(out, "{indent}}}");
    }
}

fn format_dictionary_value(val: &fdr::DictionaryValue) -> String {
    match val {
        fdr::DictionaryValue::Int64(i) => i.to_string(),
        fdr::DictionaryValue::Int64Vec(v) => {
            let items: Vec<String> = v.iter().map(|i| i.to_string()).collect();
            format!("[{}]", items.join(", "))
        }
        fdr::DictionaryValue::Str(s) => format!("\"{s}\""),
        fdr::DictionaryValue::StrVec(v) => {
            let items: Vec<String> = v.iter().map(|s| format!("\"{s}\"")).collect();
            format!("[{}]", items.join(", "))
        }
        fdr::DictionaryValue::Boolean(b) => b.to_string(),
        _ => "<unknown>".to_string(),
    }
}

fn strip_array_indices(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for (idx, seg) in key.split('.').enumerate() {
        if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
            out.push_str("[]");
        } else {
            if idx > 0 {
                out.push('.');
            }
            out.push_str(seg);
        }
    }
    out
}

fn format_dictionary_metadata(data: &[u8], indent: &str) -> Option<String> {
    let dict = fidl::unpersist::<fdr::Dictionary>(data).ok()?;
    let entries = dict.entries?;
    let mut out = String::new();
    for entry in &entries {
        if let Some(prefix) = entry.key.strip_suffix("._count") {
            if entry.value == fdr::DictionaryValue::Int64(0) {
                let key = strip_array_indices(prefix);
                let _ = writeln!(out, "{indent}{key} = []");
            }
            continue;
        }
        let key = strip_array_indices(&entry.key);
        let val = format_dictionary_value(&entry.value);
        let _ = writeln!(out, "{indent}{key} = {val}");
    }
    Some(out)
}

fn stringify_pbus_node(
    node: &fpbus::Node,
    custom_metadata: Option<CustomMetadataFormatter>,
    out: &mut String,
) {
    let name = node.name.as_deref().unwrap_or("unnamed");
    let _ = writeln!(out, "  \"{name}\" {{");
    let vid = node.vid.unwrap_or(0);
    let pid = node.pid.unwrap_or(0);
    if let Some(v) = node.vid {
        let _ = writeln!(out, "    vid = {}", stringify_vid(v));
    }
    if let Some(p) = node.pid.filter(|&p| p != 0) {
        let _ = writeln!(out, "    pid = {}", stringify_pid(vid, p));
    }
    if let Some(d) = node.did {
        let _ = writeln!(out, "    did = {}", stringify_did(vid, pid, d));
    }
    if let Some(instance_id) = node.instance_id {
        let _ = writeln!(out, "    instance_id = {instance_id}");
    }
    if let Some(ic_id) = node.interrupt_controller_id {
        let _ = writeln!(out, "    interrupt_controller_id = {ic_id}");
    }

    if let Some(mmios) = node.mmio.as_deref().filter(|m| !m.is_empty()) {
        let _ = writeln!(out, "    mmios {{");
        for mmio in mmios {
            let _ = writeln!(out, "      mmio {{");
            if let Some(base) = mmio.base {
                let _ = writeln!(out, "        base = 0x{base:x}");
            }
            if let Some(length) = mmio.length {
                let _ = writeln!(out, "        length = 0x{length:x}");
            }
            if let Some(name) = &mmio.name {
                let _ = writeln!(out, "        name = \"{name}\"");
            }
            let _ = writeln!(out, "      }}");
        }
        let _ = writeln!(out, "    }}");
    }

    if let Some(irqs) = node.irq.as_deref() {
        for irq in irqs {
            let _ = writeln!(out, "    irq {{");
            if let Some(spec) = &irq.irq {
                match spec {
                    fpbus::IrqSpec::Irq(num) => {
                        let _ = writeln!(out, "      number = {num}");
                    }
                    fpbus::IrqSpec::UserspaceIrq(uirq) => {
                        let _ = writeln!(out, "      number = {}", uirq.irq);
                        let _ = writeln!(out, "      controller = {}", uirq.controller_id);
                    }
                    _ => {}
                }
            }
            if let Some(mode) = irq.mode {
                let _ = writeln!(out, "      mode = {}", stringify_irq_mode(mode));
            }
            if let Some(wake) = irq.wake_vector {
                let _ = writeln!(out, "      wake_vector = {wake}");
            }
            if let Some(name) = &irq.name {
                let _ = writeln!(out, "      name = \"{name}\"");
            }
            if let Some(props) = irq.properties.as_deref().filter(|p| !p.is_empty()) {
                let _ = writeln!(out, "      properties {{");
                stringify_properties(props, out, "        ");
                let _ = writeln!(out, "      }}");
            }
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(btis) = node.bti.as_deref() {
        for bti in btis {
            let _ = writeln!(out, "    bti {{");
            if let Some(iommu_id) = bti.iommu_id {
                let _ = writeln!(out, "      iommu_id = {iommu_id}");
            }
            if let Some(bti_id) = bti.bti_id {
                let _ = writeln!(out, "      bti_id = {bti_id}");
            }
            if let Some(bti_id_mask) = bti.bti_id_mask {
                let _ = writeln!(out, "      bti_id_mask = 0x{bti_id_mask:x}");
            }
            if let Some(name) = &bti.name {
                let _ = writeln!(out, "      name = \"{name}\"");
            }
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(metadata) = node.metadata.as_deref() {
        for md in metadata {
            let _ = writeln!(out, "    metadata {{");
            let id_str = md.id.as_deref().unwrap_or("");
            if let Some(id) = &md.id {
                let _ = writeln!(out, "      id = \"{id}\"");
            }
            if let Some(data) = &md.data {
                let formatted = custom_metadata
                    .and_then(|fmt| fmt(id_str, data, "        "))
                    .or_else(|| format_dictionary_metadata(data, "        "));
                if let Some(lines) = formatted {
                    if lines.is_empty() {
                        let _ = writeln!(out, "      data {{}}");
                    } else {
                        let _ = writeln!(out, "      data {{");
                        out.push_str(&lines);
                        let _ = writeln!(out, "      }}");
                    }
                } else {
                    let _ = writeln!(out, "      data = [ <binary> size: {} bytes ]", data.len());
                }
            } else {
                let _ = writeln!(out, "      data = [ <binary> ]");
            }
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(power_configs) = node.power_config.as_deref() {
        for _ in power_configs {
            let _ = writeln!(out, "    power_config {{");
            let _ = writeln!(out, "      <binary>");
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(smcs) = node.smc.as_deref() {
        for smc in smcs {
            let _ = writeln!(out, "    smc {{");
            if let Some(base) = smc.service_call_num_base {
                let _ = writeln!(out, "      base = {base}");
            }
            if let Some(count) = smc.count {
                let _ = writeln!(out, "      count = {count}");
            }
            if let Some(exclusive) = smc.exclusive {
                let _ = writeln!(out, "      exclusive = {exclusive}");
            }
            if let Some(name) = &smc.name {
                let _ = writeln!(out, "      name = \"{name}\"");
            }
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(boot_metadata) = node.boot_metadata.as_deref() {
        for bm in boot_metadata {
            let _ = writeln!(out, "    boot_metadata {{");
            if let Some(zbi_type) = bm.zbi_type {
                let _ = writeln!(out, "      zbi_type = {zbi_type}");
            }
            if let Some(zbi_extra) = bm.zbi_extra {
                let _ = writeln!(out, "      zbi_extra = {zbi_extra}");
            }
            let _ = writeln!(out, "    }}");
        }
    }

    if let Some(props) = node.properties.as_deref().filter(|p| !p.is_empty()) {
        stringify_properties(props, out, "    ");
    }

    if let Some(driver_host) = &node.driver_host {
        let _ = writeln!(out, "    driver_host = \"{driver_host}\"");
    }

    let _ = writeln!(out, "  }}");
}

fn stringify_composite_node_spec(spec: &fdf_framework::CompositeNodeSpec, out: &mut String) {
    let name = spec.name.as_deref().unwrap_or("unnamed");
    let _ = writeln!(out, "  \"{name}\" {{");
    if let Some(parents) = spec.parents2.as_deref() {
        for parent in parents {
            let _ = writeln!(out, "    parent {{");
            stringify_parent_spec(parent, out, "      ");
            let _ = writeln!(out, "    }}");
        }
    }
    if let Some(driver_host) = &spec.driver_host {
        let _ = writeln!(out, "    driver_host = \"{driver_host}\"");
    }
    let _ = writeln!(out, "  }}");
}

/// Formats prepared DML devices and IOMMUs into the `.dml.parsed` / `.dt.parsed` golden text
/// format, preserving the publish order of `devices`.
pub fn format_dml_parsed(
    devices: &[PreparedDevice],
    iommus: Option<&[Iommu]>,
    custom_metadata: Option<CustomMetadataFormatter>,
) -> String {
    let mut out = String::new();

    let _ = writeln!(&mut out, "platform_bus_nodes {{");
    for (_, _, node_opt) in devices {
        if let Some(node) = node_opt {
            stringify_pbus_node(node, custom_metadata, &mut out);
            let _ = writeln!(&mut out);
        }
    }
    let _ = writeln!(&mut out, "}}\n");

    let _ = writeln!(&mut out, "board_child_nodes {{");
    let _ = writeln!(&mut out, "}}\n");

    let _ = writeln!(&mut out, "composite_node_specs {{");
    for (_, spec_opt, _) in devices {
        if let Some(spec) = spec_opt {
            stringify_composite_node_spec(spec, &mut out);
            let _ = writeln!(&mut out);
        }
    }
    let _ = writeln!(&mut out, "}}\n");

    let _ = writeln!(&mut out, "iommus {{");
    if let Some(iommus) = iommus {
        let mut sorted: Vec<&Iommu> = iommus.iter().collect();
        sorted.sort_by_key(|i| i.id.unwrap_or(0));
        for iommu in sorted {
            let Some(id) = iommu.id else { continue };
            let iommu_str = match &iommu.iommu_type {
                Some(IommuType::ArmSmmu(arm_smmu)) => {
                    format!("arm_smmu: {{ base_address: 0x{:x} }}", arm_smmu.base_address)
                }
                Some(IommuType::StubIommu(_)) | None => "stub_iommu: {}".to_string(),
                _ => "<unknown union variant>".to_string(),
            };
            let _ = writeln!(&mut out, "  iommu {{");
            let _ = writeln!(&mut out, "    id = {id}");
            let _ = writeln!(&mut out, "    {iommu_str}");
            let _ = writeln!(&mut out, "  }}");
        }
    }
    let _ = writeln!(&mut out, "}}");

    out
}

/// Runs `prepare_dml_devices` on `config` and formats the result into the `.dml.parsed` text
/// representation.
pub fn dump_dml_config(
    config: &BoardConfig,
    parser_config: &DmlParserConfig,
    driver_metadata: Option<&DriverSpecificMetadata>,
    driver_power_configs: Option<&DriverSpecificPowerConfigs>,
    enabled_nodes: &[String],
    custom_metadata: Option<CustomMetadataFormatter>,
) -> anyhow::Result<String> {
    let prepared = prepare_dml_devices(
        config,
        parser_config,
        driver_metadata,
        driver_power_configs,
        enabled_nodes,
    )?;
    Ok(format_dml_parsed(&prepared, config.iommus.as_deref(), custom_metadata))
}

/// CLI entry point for host `dml-dump-parsed` tools.
///
/// Usage: `<tool> <board-config.fidl> [output.dml.parsed]`
pub fn run_dump_parsed_cli(
    parser_config: &DmlParserConfig,
    driver_metadata: Option<&DriverSpecificMetadata>,
    driver_power_configs: Option<&DriverSpecificPowerConfigs>,
    init_fn: Option<fn(&BoardConfig)>,
    custom_metadata: Option<CustomMetadataFormatter>,
) -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args.len() > 3 {
        bail!("Usage: {} <board-config.fidl> [output.dml.parsed]", args[0]);
    }
    let input_path = &args[1];
    let bytes = std::fs::read(input_path)
        .with_context(|| format!("Failed to read board config from {input_path}"))?;
    let config: BoardConfig = fidl::unpersist(&bytes)
        .with_context(|| format!("Failed to unpersist BoardConfig from {input_path}"))?;

    if let Some(init) = init_fn {
        init(&config);
    }

    let rendered = dump_dml_config(
        &config,
        parser_config,
        driver_metadata,
        driver_power_configs,
        &[],
        custom_metadata,
    )?;
    if let Some(output_path) = args.get(2) {
        std::fs::write(output_path, rendered)
            .with_context(|| format!("Failed to write parsed DML dump to {output_path}"))?;
    } else {
        print!("{rendered}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::DEFAULT_DML_PARSER_CONFIG;
    use crate::{AggregateEntry, ArmSmmu, Device, ResourceEntry, StaticMetadata, StubIommu};
    use fidl_fuchsia_driver_metadata_common as fdr;

    #[test]
    fn test_dump_dml_config_formats_nodes_specs_and_iommus() {
        let dict_bytes = fidl::persist(&fdr::Dictionary {
            entries: Some(vec![
                fdr::DictionaryEntry {
                    key: "bus_id".to_string(),
                    value: fdr::DictionaryValue::Int64(0),
                },
                fdr::DictionaryEntry {
                    key: "channels.0.name".to_string(),
                    value: fdr::DictionaryValue::Str("console".to_string()),
                },
                fdr::DictionaryEntry {
                    key: "enabled".to_string(),
                    value: fdr::DictionaryValue::Boolean(true),
                },
            ]),
            ..Default::default()
        })
        .unwrap();

        let config = BoardConfig {
            iommus: Some(vec![
                Iommu {
                    name: Some("smmu".to_string()),
                    id: Some(2),
                    iommu_type: Some(IommuType::ArmSmmu(ArmSmmu { base_address: 0xc5a0000 })),
                    ..Default::default()
                },
                Iommu {
                    name: Some("stub".to_string()),
                    id: Some(1),
                    iommu_type: Some(IommuType::StubIommu(StubIommu {})),
                    ..Default::default()
                },
            ]),
            devices: Some(vec![Device {
                name: Some("uart-5480000".to_string()),
                compatible: Some("google,Steppe-uart".to_string()),
                driver_host: Some("boot".to_string()),
                metadata: Some(vec![StaticMetadata {
                    id: Some("fuchsia.driver.metadata.Dictionary".to_string()),
                    data: Some(dict_bytes),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            aggregates: Some(vec![AggregateEntry {
                provider: Some("pdev".to_string()),
                service: Some("fuchsia.hardware.platform.device.Service".to_string()),
                resources: Some(vec![ResourceEntry {
                    name: Some("pdev".to_string()),
                    node: Some("uart-5480000".to_string()),
                    constraint: Some(fdr::Dictionary {
                        entries: Some(vec![
                            fdr::DictionaryEntry {
                                key: "mmio.0.base".to_string(),
                                value: fdr::DictionaryValue::Int64(0x5480000),
                            },
                            fdr::DictionaryEntry {
                                key: "mmio.0.length".to_string(),
                                value: fdr::DictionaryValue::Int64(0x100),
                            },
                            fdr::DictionaryEntry {
                                key: "interrupts.0.number".to_string(),
                                value: fdr::DictionaryValue::Int64(683),
                            },
                            fdr::DictionaryEntry {
                                key: "interrupts.0.mode".to_string(),
                                value: fdr::DictionaryValue::Str("LevelHigh".to_string()),
                            },
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                }]),
                ..Default::default()
            }]),
            ..Default::default()
        };

        let dumped =
            dump_dml_config(&config, &DEFAULT_DML_PARSER_CONFIG, None, None, &[], None).unwrap();
        let expected_id = crate::parser::compute_instance_id("uart-5480000");
        let expected = format!(
            "\
platform_bus_nodes {{
  \"uart-5480000\" {{
    vid = GENERIC (0)
    did = DEVICETREE (50)
    instance_id = {expected_id}
    mmios {{
      mmio {{
        base = 0x5480000
        length = 0x100
      }}
    }}
    irq {{
      number = 683
      mode = LEVEL_HIGH
    }}
    metadata {{
      id = \"fuchsia.driver.metadata.Dictionary\"
      data {{
        bus_id = 0
        channels[].name = \"console\"
        enabled = true
      }}
    }}
    property {{ fuchsia.COMPATIBLE = \"google,Steppe-uart\" }}
    driver_host = \"boot\"
  }}

}}

board_child_nodes {{
}}

composite_node_specs {{
  \"uart-5480000\" {{
    parent {{
      bind_rules {{
        Accept(fuchsia.BIND_PROTOCOL, [PDEV (85)])
        Accept(fuchsia.BIND_PLATFORM_DEV_VID, [GENERIC (0)])
        Accept(fuchsia.BIND_PLATFORM_DEV_DID, [DEVICETREE (50)])
        Accept(fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID, [{expected_id}])
        Accept(fuchsia.COMPATIBLE, [\"google,Steppe-uart\"])
      }}
      properties {{
        property {{ fuchsia.BIND_PROTOCOL = PDEV (85) }}
        property {{ fuchsia.BIND_PLATFORM_DEV_VID = GENERIC (0) }}
        property {{ fuchsia.BIND_PLATFORM_DEV_DID = DEVICETREE (50) }}
        property {{ fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID = {expected_id} }}
        property {{ fuchsia.COMPATIBLE = \"google,Steppe-uart\" }}
        property {{ fuchsia.devicetree.FIRST_COMPATIBLE = \"google,Steppe-uart\" }}
        property {{ fuchsia.Service = \"fuchsia.hardware.platform.device.Service\" }}
      }}
    }}
    driver_host = \"boot\"
  }}

}}

iommus {{
  iommu {{
    id = 1
    stub_iommu: {{}}
  }}
  iommu {{
    id = 2
    arm_smmu: {{ base_address: 0xc5a0000 }}
  }}
}}
"
        );
        assert_eq!(dumped, expected);
    }
}
