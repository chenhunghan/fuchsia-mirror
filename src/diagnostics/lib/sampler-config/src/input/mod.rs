// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The format of the Sampler config files that are checked into the tree.
//!
//! These configs identify Cobalt metrics by ID or by name. `generate_sampler_configs` validates
//! them against the Cobalt registry and resolves them into the [`crate::runtime`] and
//! [`crate::assembly`] types, which identify metrics only by ID.

use crate::common::{EventCode, MetricId, MetricType, ProjectId};
use fidl_fuchsia_diagnostics::Selector;
use serde::Deserialize;

/// Configuration for a single project to map Inspect data to its Cobalt metrics.
#[derive(Deserialize, Debug, PartialEq)]
pub struct ProjectConfig {
    /// Project ID that metrics are being sampled and forwarded on behalf of.
    pub project_id: ProjectId,

    /// Groupings of metrics that share a poll rate for this project.
    pub data_sets: Vec<DataSetConfig>,
}

/// Grouping unit for metrics within a single project that share a polling frequency.
#[derive(Deserialize, Debug, PartialEq)]
pub struct DataSetConfig {
    /// The frequency with which metrics are sampled, in seconds.
    #[serde(deserialize_with = "crate::utils::greater_than_zero")]
    pub poll_rate_sec: i64,

    /// The collection of mappings from Inspect to Cobalt.
    pub metrics: Vec<MetricConfig>,
}

/// Configuration for a single metric to map from an Inspect property to a Cobalt metric.
///
/// Exactly one of `metric_id` and `metric_name` must be specified.
#[derive(Deserialize, Debug, PartialEq)]
pub struct MetricConfig {
    /// Selector identifying the metric to sample via the diagnostics platform.
    #[serde(rename = "selector", deserialize_with = "crate::utils::one_or_many_selectors")]
    pub selectors: Vec<Selector>,

    /// ID of the Cobalt metric to map the selector to.
    #[serde(default)]
    pub metric_id: Option<MetricId>,

    /// Name of the Cobalt metric to map the selector to.
    #[serde(default)]
    pub metric_name: Option<String>,

    /// Data type to transform the metric to.
    pub metric_type: MetricType,

    /// Event codes defining the dimensions of the Cobalt metric. Note: Order matters, and must
    /// match the order of the defined dimensions in the Cobalt metric file.
    /// Missing field means the same as empty list.
    #[serde(default)]
    pub event_codes: Vec<EventCode>,

    /// Optional boolean specifying whether to upload the specified metric only once, the first time
    /// it becomes available to the sampler. Defaults to false.
    #[serde(default)]
    pub upload_once: bool,
}

/// Template for a FIRE project, which is expanded into a [`crate::runtime::ProjectConfig`] for
/// each of the FIRE components.
#[derive(Deserialize, Debug, PartialEq)]
pub struct ProjectTemplate {
    /// Project ID that metrics are being sampled and forwarded on behalf of.
    pub project_id: ProjectId,

    /// The frequency with which metrics are sampled, in seconds.
    #[serde(deserialize_with = "crate::utils::greater_than_zero")]
    pub poll_rate_sec: i64,

    /// The collection of mappings from Inspect to Cobalt.
    pub metrics: Vec<MetricTemplate>,
}

/// Template for a single FIRE metric.
///
/// Exactly one of `metric_id` and `metric_name` must be specified.
#[derive(Deserialize, Debug, PartialEq)]
pub struct MetricTemplate {
    /// Selector templates identifying the metric to sample via the diagnostics platform.
    #[serde(rename = "selector", deserialize_with = "crate::utils::one_or_many_strings")]
    pub selectors: Vec<String>,

    /// ID of the Cobalt metric to map the selector to.
    #[serde(default)]
    pub metric_id: Option<MetricId>,

    /// Name of the Cobalt metric to map the selector to.
    #[serde(default)]
    pub metric_name: Option<String>,

    /// Data type to transform the metric to.
    pub metric_type: MetricType,

    /// Event codes defining the dimensions of the Cobalt metric, after the FIRE component ID
    /// dimension. Missing field means the same as empty list.
    #[serde(default)]
    pub event_codes: Vec<EventCode>,

    /// Optional boolean specifying whether to upload the specified metric only once, the first time
    /// it becomes available to the sampler. Defaults to false.
    #[serde(default)]
    pub upload_once: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn parse_project_config_with_metric_name() {
        let json = r#"{
            project_id: 10,
            data_sets: [{
                poll_rate_sec: 60,
                metrics: [{
                    selector: "core/foo:root:bar",
                    metric_name: "test_occurrence",
                    metric_type: "Occurrence",
                    event_codes: [1],
                }],
            }],
        }"#;
        let config: ProjectConfig = serde_json5::from_str(json).expect("parse json");
        assert_eq!(
            config,
            ProjectConfig {
                project_id: ProjectId(10),
                data_sets: vec![DataSetConfig {
                    poll_rate_sec: 60,
                    metrics: vec![MetricConfig {
                        selectors: vec![selectors::parse_verbose("core/foo:root:bar").unwrap()],
                        metric_id: None,
                        metric_name: Some("test_occurrence".into()),
                        metric_type: MetricType::Occurrence,
                        event_codes: vec![EventCode(1)],
                        upload_once: false,
                    }],
                }],
            }
        );
    }

    #[fuchsia::test]
    fn parse_project_template_with_metric_name() {
        let json = r#"{
            project_id: 10,
            poll_rate_sec: 60,
            metrics: [{
                selector: "{MONIKER}:root:val",
                metric_name: "test_fire_histogram",
                metric_type: "IntHistogram",
                event_codes: [2],
            }],
        }"#;
        let template: ProjectTemplate = serde_json5::from_str(json).expect("parse json");
        assert_eq!(
            template,
            ProjectTemplate {
                project_id: ProjectId(10),
                poll_rate_sec: 60,
                metrics: vec![MetricTemplate {
                    selectors: vec!["{MONIKER}:root:val".into()],
                    metric_id: None,
                    metric_name: Some("test_fire_histogram".into()),
                    metric_type: MetricType::IntHistogram,
                    event_codes: vec![EventCode(2)],
                    upload_once: false,
                }],
            }
        );
    }
}
