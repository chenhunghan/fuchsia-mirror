// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod driver_specific_data;

use dml_config::dump::run_dump_parsed_cli;
use dml_config::parser::DEFAULT_DML_PARSER_CONFIG;
use driver_specific_data::VIM3_DRIVER_METADATA;
use std::fmt::Write as _;

fn format_vim3_metadata(id: &str, data: &[u8], indent: &str) -> Option<String> {
    match id {
        "0" => {
            let bytes: [u8; 8] = data.try_into().ok()?;
            let sram_base = u64::from_le_bytes(bytes);
            Some(format!("{indent}sram_base = 0x{sram_base:x}\n"))
        }
        "fuchsia.hardware.usb.dwc2.Metadata" => {
            let md = fidl::unpersist::<fidl_fuchsia_hardware_usb_dwc2::Metadata>(data).ok()?;
            let mut out = String::new();
            let _ = writeln!(out, "{indent}dma_burst_len = {:?}", md.dma_burst_len);
            let _ = writeln!(out, "{indent}usb_turnaround_time = {}", md.usb_turnaround_time);
            let _ = writeln!(out, "{indent}rx_fifo_size = {}", md.rx_fifo_size);
            let _ = writeln!(out, "{indent}nptx_fifo_size = {}", md.nptx_fifo_size);
            let _ = writeln!(out, "{indent}tx_fifo_sizes = {:?}", md.tx_fifo_sizes);
            Some(out)
        }
        "fuchsia.hardware.amlogic.metadata.CpuMetadata" => {
            let md = fidl::unpersist::<fidl_fuchsia_hardware_amlogic_metadata::CpuMetadata>(data)
                .ok()?;
            let mut out = String::new();
            if let Some(pds) = &md.performance_domains {
                for (i, pd) in pds.iter().enumerate() {
                    let _ = writeln!(out, "{indent}performance_domains.{i}.id = {}", pd.id);
                    let _ = writeln!(
                        out,
                        "{indent}performance_domains.{i}.core_count = {}",
                        pd.core_count
                    );
                    let _ = writeln!(
                        out,
                        "{indent}performance_domains.{i}.relative_performance = {}",
                        pd.relative_performance
                    );
                    let _ = writeln!(out, "{indent}performance_domains.{i}.name = \"{}\"", pd.name);
                }
            }
            if let Some(opps) = &md.operating_points {
                for (i, opp) in opps.iter().enumerate() {
                    let _ = writeln!(out, "{indent}operating_points.{i}.freq_hz = {}", opp.freq_hz);
                    let _ = writeln!(out, "{indent}operating_points.{i}.volt_uv = {}", opp.volt_uv);
                    let _ = writeln!(out, "{indent}operating_points.{i}.pd_id = {}", opp.pd_id);
                }
            }
            Some(out)
        }
        "fuchsia.hardware.serial.SerialPortInfo" => {
            let md = fidl::unpersist::<fidl_fuchsia_hardware_serial::SerialPortInfo>(data).ok()?;
            let mut out = String::new();
            let _ = writeln!(out, "{indent}serial_class = {:?}", md.serial_class);
            let _ = writeln!(out, "{indent}serial_vid = {}", md.serial_vid);
            let _ = writeln!(out, "{indent}serial_pid = {}", md.serial_pid);
            Some(out)
        }
        "fuchsia.wlan.broadcom.WifiConfig" => {
            use fidl_fuchsia_wlan_broadcom::{IovarEntry, WifiConfig};
            let md = fidl::unpersist::<WifiConfig>(data).ok()?;
            let mut out = String::new();
            let _ = writeln!(out, "{indent}oob_irq_mode = {}", md.oob_irq_mode);
            let _ = writeln!(out, "{indent}clm_needed = {}", md.clm_needed);
            for (i, entry) in md.iovar_table.iter().enumerate() {
                match entry {
                    IovarEntry::Command(c) => {
                        let _ = writeln!(
                            out,
                            "{indent}iovar_table.{i} = Command(cmd={}, val={})",
                            c.cmd, c.val
                        );
                    }
                    IovarEntry::String(s) => {
                        let _ = writeln!(
                            out,
                            "{indent}iovar_table.{i} = String(name=\"{}\", val={})",
                            s.name, s.val
                        );
                    }
                    _ => {
                        let _ = writeln!(out, "{indent}iovar_table.{i} = {entry:?}");
                    }
                }
            }
            let cc_items: Vec<String> =
                md.cc_table.iter().map(|c| format!("\"{}:{}\"", c.cc_abbr, c.cc_rev)).collect();
            let _ = writeln!(out, "{indent}cc_table = [{}]", cc_items.join(", "));
            Some(out)
        }
        "fuchsia.hardware.clockimpl.InitMetadata" => {
            use fidl_fuchsia_hardware_clockimpl::{InitCall, InitMetadata};
            let md = fidl::unpersist::<InitMetadata>(data).ok()?;
            let mut out = String::new();
            for (i, step) in md.steps.iter().enumerate() {
                let id_str = step.id.map_or_else(|| "<none>".to_string(), |id| format!("0x{id:x}"));
                let call_str = match &step.call {
                    Some(InitCall::Enable(_)) => "Enable".to_string(),
                    Some(InitCall::Disable(_)) => "Disable".to_string(),
                    Some(InitCall::RateHz(rate)) => format!("RateHz({rate})"),
                    Some(InitCall::InputIdx(idx)) => format!("InputIdx({idx})"),
                    Some(InitCall::Delay(delay)) => format!("Delay({delay})"),
                    Some(other) => format!("{other:?}"),
                    None => "<none>".to_string(),
                };
                let _ = writeln!(out, "{indent}steps.{i} = {id_str}: {call_str}");
            }
            Some(out)
        }
        "fuchsia.hardware.sdmmc.SdmmcMetadata" => {
            let md = fidl::unpersist::<fidl_fuchsia_hardware_sdmmc::SdmmcMetadata>(data).ok()?;
            let mut out = String::new();
            if let Some(max_freq) = md.max_frequency {
                let _ = writeln!(out, "{indent}max_frequency = {max_freq}");
            }
            if let Some(speed_caps) = md.speed_capabilities {
                let _ = writeln!(out, "{indent}speed_capabilities = {speed_caps:?}");
            }
            if let Some(enable_cache) = md.enable_cache {
                let _ = writeln!(out, "{indent}enable_cache = {enable_cache}");
            }
            if let Some(removable) = md.removable {
                let _ = writeln!(out, "{indent}removable = {removable}");
            }
            if let Some(max_packing) = md.max_command_packing {
                let _ = writeln!(out, "{indent}max_command_packing = {max_packing}");
            }
            if let Some(use_fidl) = md.use_fidl {
                let _ = writeln!(out, "{indent}use_fidl = {use_fidl}");
            }
            if let Some(instance_id) = &md.instance_identifier {
                let _ = writeln!(out, "{indent}instance_identifier = \"{instance_id}\"");
            }
            if let Some(vccq_off) = md.vccq_off_with_controller_off {
                let _ = writeln!(out, "{indent}vccq_off_with_controller_off = {vccq_off}");
            }
            Some(out)
        }
        _ => None,
    }
}

fn main() -> anyhow::Result<()> {
    run_dump_parsed_cli(
        &DEFAULT_DML_PARSER_CONFIG,
        Some(&VIM3_DRIVER_METADATA),
        None,
        None,
        Some(format_vim3_metadata),
    )
}
