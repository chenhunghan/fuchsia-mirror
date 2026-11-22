// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, format_err};
use flex_fuchsia_driver_framework as fdf;
use std::io::Write;
use zx_status::Status;

pub fn node_property_value_to_string(value: &fdf::NodePropertyValue) -> String {
    match value {
        fdf::NodePropertyValue::IntValue(int_val) => {
            format!("{:#08x}", int_val)
        }
        fdf::NodePropertyValue::StringValue(str_val) => {
            format!("\"{}\"", str_val)
        }
        fdf::NodePropertyValue::BoolValue(bool_val) => bool_val.to_string(),
        fdf::NodePropertyValue::EnumValue(enum_val) => {
            format!("Enum({})", enum_val)
        }
        _ => "Unknown value".to_string(),
    }
}

pub fn write_node_properties(
    properties: &Vec<fdf::NodeProperty2>,
    writer: &mut dyn Write,
) -> Result<()> {
    let props_len = properties.len();
    writeln!(writer, "  {0} {1}", props_len, "Properties")?;

    for (index, property) in properties.iter().enumerate() {
        let key = &property.key;
        let value = node_property_value_to_string(&property.value);
        writeln!(
            writer,
            "  [{0:>2}/{1:>2}] : Key {2:30} Value {3}",
            index + 1,
            props_len,
            key,
            value,
        )?;
    }

    Ok(())
}

pub fn colorized(string: &str, color: ansi_term::Colour, with_style: bool) -> String {
    if with_style { color.paint(string).to_string() } else { string.to_string() }
}

/// Returns the error to report when a `StartSuspendSession` request for `target`
/// fails with `status`.
pub fn suspend_request_error(target: &str, status: Status) -> anyhow::Error {
    let reason = match status {
        Status::INVALID_ARGS => {
            "the target is invalid or matches multiple nodes (specify --disambiguator)"
        }
        Status::NOT_FOUND => "no matching node or power dependency was found",
        Status::BAD_STATE => {
            "the node or a descendant in its subtree has no bound driver, is stopping, or is \
             restarting (see `driver node show`)"
        }
        Status::NOT_SUPPORTED => {
            "suspend is not enabled in driver manager or is unsupported for this node"
        }
        Status::ACCESS_DENIED => "the power broker did not authorize the change",
        Status::ALREADY_EXISTS => "a suspend session is already active for this driver",
        Status::BAD_HANDLE => "an invalid token was provided",
        Status::UNAVAILABLE => "boot has not completed yet",
        _ => return format_err!("Failed to suspend {target}: {status}"),
    };
    format_err!("Failed to suspend {target}: {reason} ({status})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_node_property_value_to_string() {
        assert_eq!(
            "0x000001".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::IntValue(0x0001))
        );

        assert_eq!(
            "0x000bbb".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::IntValue(0x0BBB))
        );

        assert_eq!(
            "0xffffffff".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::IntValue(0xFFFFFFFF))
        );

        assert_eq!(
            "\"Hello\"".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::StringValue(
                "Hello".to_string()
            ))
        );

        assert_eq!(
            "true".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::BoolValue(true))
        );

        assert_eq!(
            "false".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::BoolValue(false))
        );

        assert_eq!(
            "Enum(Hello.World)".to_string(),
            node_property_value_to_string(&fdf::NodePropertyValue::EnumValue(
                "Hello.World".to_string()
            ))
        );
    }

    #[fuchsia::test]
    fn test_suspend_request_error() {
        assert_eq!(
            suspend_request_error("dev.sys.test", Status::INVALID_ARGS).to_string(),
            "Failed to suspend dev.sys.test: the target is invalid or matches multiple nodes \
             (specify --disambiguator) (INVALID_ARGS)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::NOT_FOUND).to_string(),
            "Failed to suspend dev.sys.test: no matching node or power dependency was found \
             (NOT_FOUND)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::NOT_SUPPORTED).to_string(),
            "Failed to suspend dev.sys.test: suspend is not enabled in driver manager or is \
             unsupported for this node (NOT_SUPPORTED)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::BAD_STATE).to_string(),
            "Failed to suspend dev.sys.test: the node or a descendant in its subtree has no bound \
             driver, is stopping, or is restarting (see `driver node show`) (BAD_STATE)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::ACCESS_DENIED).to_string(),
            "Failed to suspend dev.sys.test: the power broker did not authorize the change \
             (ACCESS_DENIED)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::ALREADY_EXISTS).to_string(),
            "Failed to suspend dev.sys.test: a suspend session is already active for this driver \
             (ALREADY_EXISTS)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::BAD_HANDLE).to_string(),
            "Failed to suspend dev.sys.test: an invalid token was provided (BAD_HANDLE)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::UNAVAILABLE).to_string(),
            "Failed to suspend dev.sys.test: boot has not completed yet (UNAVAILABLE)"
        );

        assert_eq!(
            suspend_request_error("dev.sys.test", Status::INTERNAL).to_string(),
            "Failed to suspend dev.sys.test: INTERNAL"
        );
    }
}
