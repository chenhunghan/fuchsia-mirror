// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
//
// Validates the parsed TRF AST against explicit component manifest routing rules.

use crate::parser::{ConfigValue, ParsedAstResult};
use anyhow::{Context, Result as AnyhowResult};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const INJECTABLE_UNIVERSE_MONIKER: &str = "IU";
const COMPONENT_UNDER_TEST_MONIKER: &str = "CUT";
const TEST_DRIVER_MONIKER: &str = "TD";

/// Expected configuration types for Component Under Test (CUT) manifest fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConfigType {
    Boolean,
    Integer,
    String,
    Vector,
}

/// Representation of a component manifest (`.cml` or `.cm` JSON) for the Component Under Test (CUT).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ComponentManifest {
    #[serde(default)]
    pub config_schema: HashMap<String, ConfigType>,
    #[serde(default)]
    pub config_capabilities: BTreeMap<String, String>,
    #[serde(default)]
    pub used_protocols: Vec<String>,
    #[serde(default)]
    pub exposed_protocols: Vec<String>,
    #[serde(default)]
    pub has_includes: bool,
}

fn parse_cml_config_type(type_name: &str) -> Option<ConfigType> {
    match type_name {
        "bool" => Some(ConfigType::Boolean),
        "uint8" | "uint16" | "uint32" | "uint64" | "int8" | "int16" | "int32" | "int64" => {
            Some(ConfigType::Integer)
        }
        "string" => Some(ConfigType::String),
        "vector" => Some(ConfigType::Vector),
        _ => None,
    }
}

fn collect_protocol_values(value: &serde_json::Value, output: &mut Vec<String>) {
    match value {
        serde_json::Value::String(protocol) => {
            if !output.contains(protocol) {
                output.push(protocol.clone());
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                if let Some(protocol) = item.as_str() {
                    let protocol_string = protocol.to_string();
                    if !output.contains(&protocol_string) {
                        output.push(protocol_string);
                    }
                }
            }
        }
        _ => {}
    }
}

impl ComponentManifest {
    /// Parses either a Fuchsia `.cml` JSON5 manifest or a serialized `ComponentManifest` JSON document.
    pub fn parse(content: &str) -> AnyhowResult<Self> {
        let root_value: serde_json::Value =
            serde_json5::from_str(content).context("Failed to parse component manifest")?;
        let root_object = root_value
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Component manifest root must be an object"))?;

        if root_object.contains_key("config_schema")
            || root_object.contains_key("config_capabilities")
            || root_object.contains_key("used_protocols")
            || root_object.contains_key("exposed_protocols")
        {
            return serde_json::from_value(root_value)
                .context("Failed to deserialize structured ComponentManifest JSON");
        }

        let mut manifest = ComponentManifest::default();
        manifest.has_includes = root_object
            .get("include")
            .and_then(|value| value.as_array())
            .is_some_and(|includes| !includes.is_empty());

        if let Some(use_entries) = root_object.get("use").and_then(|value| value.as_array()) {
            for entry in use_entries {
                let Some(entry_object) = entry.as_object() else {
                    continue;
                };
                if let Some(protocol_value) = entry_object.get("protocol") {
                    collect_protocol_values(protocol_value, &mut manifest.used_protocols);
                }
                if let Some(capability_name) =
                    entry_object.get("config").and_then(|value| value.as_str())
                    && let Some(config_key) =
                        entry_object.get("key").and_then(|value| value.as_str())
                {
                    manifest
                        .config_capabilities
                        .insert(config_key.to_string(), capability_name.to_string());
                    if let Some(config_type) = entry_object
                        .get("type")
                        .and_then(|value| value.as_str())
                        .and_then(parse_cml_config_type)
                    {
                        manifest.config_schema.insert(config_key.to_string(), config_type);
                    }
                }
            }
        }

        if let Some(config_block) = root_object.get("config").and_then(|value| value.as_object()) {
            for (config_key, declaration) in config_block {
                if let Some(config_type) = declaration
                    .get("type")
                    .and_then(|value| value.as_str())
                    .and_then(parse_cml_config_type)
                {
                    manifest.config_schema.entry(config_key.clone()).or_insert(config_type);
                }
            }
        }

        if let Some(expose_entries) = root_object.get("expose").and_then(|value| value.as_array()) {
            for entry in expose_entries {
                let Some(entry_object) = entry.as_object() else {
                    continue;
                };
                let Some(protocol_value) = entry_object.get("protocol") else {
                    continue;
                };
                if protocol_value.is_string()
                    && let Some(alias) = entry_object.get("as").and_then(|value| value.as_str())
                {
                    let alias_string = alias.to_string();
                    if !manifest.exposed_protocols.contains(&alias_string) {
                        manifest.exposed_protocols.push(alias_string);
                    }
                } else {
                    collect_protocol_values(protocol_value, &mut manifest.exposed_protocols);
                }
            }
        }

        Ok(manifest)
    }
}

/// Capability route automatically inferred between components in the TRF topology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityRoute {
    pub protocol: String,
    pub source: String,
    pub target: String,
}

/// Collection of automatically inferred capability routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct AutoRoutes {
    pub routes: Vec<CapabilityRoute>,
}

/// Linting error or warning encountered during manifest validation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LintError {
    UnknownConfigKey {
        key: String,
        suggestion: Option<String>,
        function_name: String,
    },
    ConfigTypeMismatch {
        key: String,
        expected: ConfigType,
        actual: ConfigType,
        function_name: String,
    },
    UnmatchedMockProtocol {
        protocol: String,
        struct_name: String,
    },
}

impl std::fmt::Display for LintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LintError::UnknownConfigKey { key, suggestion, function_name } => {
                if let Some(sug) = suggestion {
                    write!(
                        f,
                        "Unknown config key '{key}' in test function '{function_name}'. Did you mean '{sug}'?"
                    )
                } else {
                    write!(f, "Unknown config key '{key}' in test function '{function_name}'.")
                }
            }
            LintError::ConfigTypeMismatch { key, expected, actual, function_name } => {
                write!(
                    f,
                    "Config type mismatch for key '{key}' in test function '{function_name}': expected {expected:?}, got {actual:?}."
                )
            }
            LintError::UnmatchedMockProtocol { protocol, struct_name } => {
                write!(
                    f,
                    "Mock struct '{struct_name}' targets protocol '{protocol}', but this protocol is not in the CUT's used protocols."
                )
            }
        }
    }
}

impl std::error::Error for LintError {}

/// Calculates the Levenshtein edit distance between two string slices.
fn calculate_levenshtein_distance(string_a: &str, string_b: &str) -> usize {
    let chars_a: Vec<char> = string_a.chars().collect();
    let chars_b: Vec<char> = string_b.chars().collect();
    let length_a = chars_a.len();
    let length_b = chars_b.len();

    let mut distance_matrix = vec![vec![0usize; length_b + 1]; length_a + 1];

    for index_a in 0..=length_a {
        distance_matrix[index_a][0] = index_a;
    }
    for index_b in 0..=length_b {
        distance_matrix[0][index_b] = index_b;
    }

    for index_a in 1..=length_a {
        for index_b in 1..=length_b {
            let substitution_cost =
                if chars_a[index_a - 1] == chars_b[index_b - 1] { 0 } else { 1 };
            distance_matrix[index_a][index_b] = (distance_matrix[index_a - 1][index_b] + 1)
                .min(distance_matrix[index_a][index_b - 1] + 1)
                .min(distance_matrix[index_a - 1][index_b - 1] + substitution_cost);
        }
    }

    distance_matrix[length_a][length_b]
}

/// Finds the closest matching key in valid keys for a given unknown key to provide a helpful suggestion.
fn find_closest_config_key_suggestion<'a>(
    unknown_key: &str,
    valid_keys: impl Iterator<Item = &'a String>,
) -> Option<String> {
    let mut closest_match: Option<(&'a String, usize)> = None;

    for valid_key in valid_keys {
        let distance = calculate_levenshtein_distance(unknown_key, valid_key);
        let is_prefix_or_suffix =
            valid_key.starts_with(unknown_key) || unknown_key.starts_with(valid_key);
        let maximum_allowed_distance = (unknown_key.len() / 2).max(3);

        if distance <= maximum_allowed_distance || is_prefix_or_suffix {
            if let Some((_, minimum_distance)) = closest_match {
                if distance < minimum_distance {
                    closest_match = Some((valid_key, distance));
                }
            } else {
                closest_match = Some((valid_key, distance));
            }
        }
    }

    closest_match.map(|(key, _)| key.clone())
}

/// Returns the corresponding `ConfigType` for a `ConfigValue`.
fn derive_config_type(value: &ConfigValue) -> ConfigType {
    match value {
        ConfigValue::Boolean(_) => ConfigType::Boolean,
        ConfigValue::Integer(_) => ConfigType::Integer,
        ConfigValue::String(_) => ConfigType::String,
        ConfigValue::Vector(_) => ConfigType::Vector,
    }
}

/// Compares a parsed test function against a Component Manifest to validate config overrides.
fn validate_config_overrides(
    ast: &ParsedAstResult,
    manifest: &ComponentManifest,
    has_namespace_fallback: bool,
    lint_errors: &mut Vec<LintError>,
) {
    let allow_shard_defined_keys = has_namespace_fallback && manifest.has_includes;
    for test_function in &ast.test_functions {
        for config_override in &test_function.config_overrides {
            match manifest.config_schema.get(&config_override.key) {
                None => {
                    if allow_shard_defined_keys {
                        continue;
                    }
                    let suggestion = find_closest_config_key_suggestion(
                        &config_override.key,
                        manifest.config_schema.keys(),
                    );
                    lint_errors.push(LintError::UnknownConfigKey {
                        key: config_override.key.clone(),
                        suggestion,
                        function_name: test_function.function_name.clone(),
                    });
                }
                Some(expected_config_type) => {
                    let actual_config_type = derive_config_type(&config_override.value);
                    if &actual_config_type != expected_config_type {
                        lint_errors.push(LintError::ConfigTypeMismatch {
                            key: config_override.key.clone(),
                            expected: expected_config_type.clone(),
                            actual: actual_config_type,
                            function_name: test_function.function_name.clone(),
                        });
                    }
                }
            }
        }
    }
}

/// Matches the declared TRF mocks against the Component Manifest's used protocols.
fn validate_mocked_protocols(
    ast: &ParsedAstResult,
    manifest: &ComponentManifest,
    lint_errors: &mut Vec<LintError>,
) {
    for mock_declaration in &ast.mock_declarations {
        if !manifest
            .used_protocols
            .iter()
            .any(|used_protocol| used_protocol == &mock_declaration.protocol)
        {
            lint_errors.push(LintError::UnmatchedMockProtocol {
                protocol: mock_declaration.protocol.clone(),
                struct_name: mock_declaration.struct_name.clone(),
            });
        }
    }
}

/// Returns the fallback protocols when the manifest doesn't specify any exposed protocols.
fn get_fallback_protocols(ast: &ParsedAstResult) -> Vec<String> {
    let mut fallback_protocols = Vec::new();
    for conn in &ast.protocol_connections {
        // Simple heuristic to extract FIDL name from a generated Rust marker type.
        // e.g., `fidl_fuchsia_examples_reverser::ReverserMarker` ->
        //     `fuchsia.examples.reverser.Reverser`
        let parts: Vec<&str> = conn.split("::").collect();
        if parts.len() >= 2 {
            let pkg = parts.first().unwrap();
            let marker = parts.last().unwrap();
            if let Some(pkg_suffix) = pkg.strip_prefix("fidl_") {
                let mut fidl_name = pkg_suffix.replace("_", ".");
                fidl_name.push('.');
                if let Some(protocol) = marker.strip_suffix("Marker") {
                    fidl_name.push_str(protocol);
                    if !fallback_protocols.contains(&fidl_name) {
                        fallback_protocols.push(fidl_name);
                    }
                    continue;
                }
            }
        }
        if !fallback_protocols.contains(conn) {
            fallback_protocols.push(conn.clone());
        }
    }
    fallback_protocols
}

/// Evaluates verified configurations and infers the necessary dependency graph capacity paths.
fn infer_capability_routes(
    ast: &ParsedAstResult,
    manifest: Option<&ComponentManifest>,
) -> AutoRoutes {
    let mut inferred_routes = Vec::new();

    // Route mock protocols: Injectable Universe (IU) -> Component Under Test (CUT)
    for mock_declaration in &ast.mock_declarations {
        let route = CapabilityRoute {
            protocol: mock_declaration.protocol.clone(),
            source: INJECTABLE_UNIVERSE_MONIKER.to_string(),
            target: COMPONENT_UNDER_TEST_MONIKER.to_string(),
        };
        if !inferred_routes.contains(&route) {
            inferred_routes.push(route);
        }
    }

    // Route CUT exposed protocols: Component Under Test (CUT) -> Test Driver (TD)
    let exposed_protocols_to_route = if let Some(m) = manifest {
        if !m.exposed_protocols.is_empty() {
            m.exposed_protocols.clone()
        } else {
            get_fallback_protocols(ast)
        }
    } else {
        get_fallback_protocols(ast)
    };

    for exposed_protocol in &exposed_protocols_to_route {
        let route = CapabilityRoute {
            protocol: exposed_protocol.clone(),
            source: COMPONENT_UNDER_TEST_MONIKER.to_string(),
            target: TEST_DRIVER_MONIKER.to_string(),
        };
        if !inferred_routes.contains(&route) {
            inferred_routes.push(route);
        }
    }

    AutoRoutes { routes: inferred_routes }
}

/// Validates parsed AST test annotations against a Component Under Test manifest.
///
/// Returns auto-inferred capability routes on success, or a list of lint errors/warnings on
/// failure.
pub fn check_consistency(
    ast: &ParsedAstResult,
    manifest: Option<&ComponentManifest>,
    has_namespace_fallback: bool,
) -> Result<AutoRoutes, Vec<LintError>> {
    let mut lint_errors = Vec::new();

    if let Some(m) = manifest {
        validate_config_overrides(ast, m, has_namespace_fallback, &mut lint_errors);
        validate_mocked_protocols(ast, m, &mut lint_errors);
    }

    if !lint_errors.is_empty() {
        return Err(lint_errors);
    }

    Ok(infer_capability_routes(ast, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::{ConfigOverride, MockDeclaration, TestFunction};

    #[test]
    fn test_lint_config_valid_keys_and_types_passes() {
        let mut config_schema = HashMap::new();
        config_schema.insert("enable_feature".to_string(), ConfigType::Boolean);
        config_schema.insert("max_threads".to_string(), ConfigType::Integer);
        config_schema.insert("log_prefix".to_string(), ConfigType::String);
        config_schema.insert("allowed_ports".to_string(), ConfigType::Vector);

        let manifest = ComponentManifest {
            config_schema,
            used_protocols: vec!["fuchsia.logger.LogSink".to_string()],
            exposed_protocols: vec!["fuchsia.example.Echo".to_string()],
            ..Default::default()
        };

        let ast = ParsedAstResult {
            mock_declarations: vec![MockDeclaration {
                struct_name: "MockLogger".to_string(),
                protocol: "fuchsia.logger.LogSink".to_string(),
                has_control_impl: false,
                control_methods: vec![],
            }],
            test_functions: vec![TestFunction {
                function_name: "test_valid_config".to_string(),
                config_overrides: vec![
                    ConfigOverride {
                        key: "enable_feature".to_string(),
                        value: ConfigValue::Boolean(true),
                    },
                    ConfigOverride {
                        key: "max_threads".to_string(),
                        value: ConfigValue::Integer(4),
                    },
                    ConfigOverride {
                        key: "log_prefix".to_string(),
                        value: ConfigValue::String("TEST_PREFIX".to_string()),
                    },
                    ConfigOverride {
                        key: "allowed_ports".to_string(),
                        value: ConfigValue::Vector(vec![ConfigValue::Integer(8080)]),
                    },
                ],
                selected_mocks: vec![],
                protocol_connections: vec![],
            }],
            protocol_connections: vec![],
        };

        let result = check_consistency(&ast, Some(&manifest), false);

        assert!(result.is_ok(), "Expected linting to pass with valid config keys and types");
    }

    #[test]
    fn test_lint_config_unknown_key_fails_with_suggestion() {
        let mut config_schema = HashMap::new();
        config_schema.insert("fix_capitalization".to_string(), ConfigType::Boolean);

        let manifest = ComponentManifest { config_schema, ..Default::default() };

        let ast = ParsedAstResult {
            mock_declarations: vec![],
            test_functions: vec![TestFunction {
                function_name: "test_unknown_key".to_string(),
                config_overrides: vec![ConfigOverride {
                    key: "fix_capital".to_string(),
                    value: ConfigValue::Boolean(true),
                }],
                selected_mocks: vec![],
                protocol_connections: vec![],
            }],
            protocol_connections: vec![],
        };

        let result = check_consistency(&ast, Some(&manifest), false);

        assert!(result.is_err(), "Expected linting to fail due to unknown config key");
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            LintError::UnknownConfigKey { key, suggestion, function_name } => {
                assert_eq!(key, "fix_capital");
                assert_eq!(suggestion.as_deref(), Some("fix_capitalization"));
                assert_eq!(function_name, "test_unknown_key");
            }
            _ => panic!("Expected LintError::UnknownConfigKey variant"),
        }
    }

    #[test]
    fn test_lint_config_type_mismatch_fails() {
        let mut config_schema = HashMap::new();
        config_schema.insert("max_threads".to_string(), ConfigType::Integer);

        let manifest = ComponentManifest { config_schema, ..Default::default() };

        let ast = ParsedAstResult {
            mock_declarations: vec![],
            test_functions: vec![TestFunction {
                function_name: "test_type_mismatch".to_string(),
                config_overrides: vec![ConfigOverride {
                    key: "max_threads".to_string(),
                    value: ConfigValue::String("four".to_string()),
                }],
                selected_mocks: vec![],
                protocol_connections: vec![],
            }],
            protocol_connections: vec![],
        };

        let result = check_consistency(&ast, Some(&manifest), false);

        assert!(result.is_err(), "Expected linting to fail due to config type mismatch");
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            LintError::ConfigTypeMismatch { key, expected, actual, function_name } => {
                assert_eq!(key, "max_threads");
                assert_eq!(expected, &ConfigType::Integer);
                assert_eq!(actual, &ConfigType::String);
                assert_eq!(function_name, "test_type_mismatch");
            }
            _ => panic!("Expected LintError::ConfigTypeMismatch variant"),
        }
    }

    #[test]
    fn test_lint_unmatched_mock_protocol_warning() {
        let manifest = ComponentManifest {
            used_protocols: vec!["fuchsia.logger.LogSink".to_string()],
            ..Default::default()
        };

        let ast = ParsedAstResult {
            mock_declarations: vec![MockDeclaration {
                struct_name: "MockUnusedService".to_string(),
                protocol: "fuchsia.unused.UnusedProtocol".to_string(),
                has_control_impl: false,
                control_methods: vec![],
            }],
            test_functions: vec![],
            protocol_connections: vec![],
        };

        let result = check_consistency(&ast, Some(&manifest), false);

        assert!(
            result.is_err(),
            "Expected linting to fail/warn when mocked protocol is not used by CUT"
        );
        let errors = result.unwrap_err();
        assert_eq!(errors.len(), 1);
        match &errors[0] {
            LintError::UnmatchedMockProtocol { protocol, struct_name } => {
                assert_eq!(protocol, "fuchsia.unused.UnusedProtocol");
                assert_eq!(struct_name, "MockUnusedService");
            }
            _ => panic!("Expected LintError::UnmatchedMockProtocol variant"),
        }
    }

    #[test]
    fn test_auto_inferred_capability_routes() {
        let manifest = ComponentManifest {
            used_protocols: vec!["fuchsia.emoji.EmojiReverser".to_string()],
            exposed_protocols: vec!["fuchsia.text.Text".to_string()],
            ..Default::default()
        };

        let ast = ParsedAstResult {
            mock_declarations: vec![MockDeclaration {
                struct_name: "MockEmojiService".to_string(),
                protocol: "fuchsia.emoji.EmojiReverser".to_string(),
                has_control_impl: false,
                control_methods: vec![],
            }],
            test_functions: vec![],
            protocol_connections: vec![],
        };

        let result = check_consistency(&ast, Some(&manifest), false);

        assert!(result.is_ok(), "Expected linting to succeed and auto-infer routes");
        let auto_routes = result.unwrap();
        assert_eq!(auto_routes.routes.len(), 2);
        assert!(auto_routes.routes.contains(&CapabilityRoute {
            protocol: "fuchsia.emoji.EmojiReverser".to_string(),
            source: INJECTABLE_UNIVERSE_MONIKER.to_string(),
            target: COMPONENT_UNDER_TEST_MONIKER.to_string(),
        }));
        assert!(auto_routes.routes.contains(&CapabilityRoute {
            protocol: "fuchsia.text.Text".to_string(),
            source: COMPONENT_UNDER_TEST_MONIKER.to_string(),
            target: TEST_DRIVER_MONIKER.to_string(),
        }));
    }

    #[test]
    fn test_get_fallback_protocols_parsing() {
        let ast = ParsedAstResult {
            mock_declarations: vec![],
            test_functions: vec![],
            protocol_connections: vec![
                "fidl_fuchsia_driver_framework::TopologyMarker".to_string(),
                "fuchsia.unknown.Protocol".to_string(),
            ],
        };

        let fallbacks = get_fallback_protocols(&ast);

        assert_eq!(fallbacks.len(), 2);
        assert_eq!(fallbacks[0], "fuchsia.driver.framework.Topology");
        assert_eq!(fallbacks[1], "fuchsia.unknown.Protocol");
    }

    #[test]
    fn test_calculate_levenshtein_distance() {
        assert_eq!(calculate_levenshtein_distance("kitten", "sitting"), 3);
        assert_eq!(calculate_levenshtein_distance("flaw", "lawn"), 2);
    }

    #[test]
    fn test_find_closest_config_key_suggestion() {
        let valid = vec!["enable_foo".to_string(), "max_val".to_string()];

        // Minor typo
        assert_eq!(
            find_closest_config_key_suggestion("enable_fo", valid.iter()).as_deref(),
            Some("enable_foo")
        );

        // Entirely different string
        assert_eq!(find_closest_config_key_suggestion("totally_unrelated", valid.iter()), None);
    }

    #[test]
    fn test_parse_cml_manifest_extracts_config_capabilities_and_protocols() {
        let cml = r#"
            // Copyright 2026 The Fuchsia Authors. All rights reserved.
            {
                include: [ "syslog/client.shard.cml" ],
                use: [
                    {
                        protocol: [ "fuchsia.diagnostics.ArchiveAccessor", "fuchsia.net.NameLookup" ],
                    },
                    {
                        config: "fuchsia.diagnostics.sampler.BindServices",
                        key: "bind_services",
                        type: "bool",
                        availability: "transitional",
                        default: false,
                    },
                    {
                        config: "fuchsia.diagnostics.sampler.PollRateSec",
                        key: "poll_rate_sec",
                        type: "int64",
                    },
                ],
                expose: [
                    {
                        protocol: "fuchsia.diagnostics.sampler.InternalController",
                        as: "fuchsia.diagnostics.sampler.Controller",
                        from: "self",
                    },
                ],
            }
        "#;

        let manifest = ComponentManifest::parse(cml).expect("CML should parse");
        assert!(manifest.has_includes);
        assert_eq!(
            manifest.config_capabilities.get("bind_services").map(String::as_str),
            Some("fuchsia.diagnostics.sampler.BindServices")
        );
        assert_eq!(
            manifest.config_capabilities.get("poll_rate_sec").map(String::as_str),
            Some("fuchsia.diagnostics.sampler.PollRateSec")
        );
        assert_eq!(manifest.config_schema.get("bind_services"), Some(&ConfigType::Boolean));
        assert_eq!(manifest.config_schema.get("poll_rate_sec"), Some(&ConfigType::Integer));
        assert_eq!(
            manifest.used_protocols,
            vec![
                "fuchsia.diagnostics.ArchiveAccessor".to_string(),
                "fuchsia.net.NameLookup".to_string(),
            ]
        );
        assert_eq!(
            manifest.exposed_protocols,
            vec!["fuchsia.diagnostics.sampler.Controller".to_string()]
        );
    }
}
