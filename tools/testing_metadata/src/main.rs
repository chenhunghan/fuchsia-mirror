// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod owners;

use anyhow::{Context, Result};
use argh::FromArgs;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use valico::json_schema;
use walkdir::WalkDir;

const INPUT_SCHEMA: &str = include_str!("../testing_input.schema.json");

#[derive(FromArgs)]
/// Testing metadata collector tool.
struct Args {
    /// path to the fuchsia directory.
    #[argh(option)]
    fuchsia_dir: PathBuf,

    /// path to the output JSON file.
    #[argh(option)]
    output: PathBuf,

    /// optional path to the output directory owners JSON file.
    #[argh(option)]
    directory_owners_file_output: Option<PathBuf>,

    /// path to the output depfile.
    #[argh(option)]
    depfile: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct Coverage {
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    subcategory: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inherit_tags: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct TestingMetadataSource {
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage: Option<Coverage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
struct TestingMetadataDirectoryOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    coverage: Option<Coverage>,
    parent_directory: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
struct MetadataOutput {
    metadata: BTreeMap<String, TestingMetadataDirectoryOutput>,
    version: u32,
}

fn main() -> Result<()> {
    let args: Args = argh::from_env();
    collect_metadata(args)
}

/// Serializes `value` as pretty-printed JSON and writes it to `path`, creating parent directories
/// if needed.
fn write_pretty_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let json_content = serde_json::to_string_pretty(value).context("failed to serialize JSON")?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, json_content).with_context(|| format!("failed to write output: {:?}", path))
}

/// Walks `--fuchsia-dir` to collect `TESTING.json5`, `OWNERS`, and `METADATA.textproto` files,
/// writes `testing_metadata.json` (and optionally `directory_owners.json`), and emits the Ninja
/// depfile.
fn collect_metadata(args: Args) -> Result<()> {
    let abs_fuchsia_dir = if args.fuchsia_dir.is_absolute() {
        args.fuchsia_dir.clone()
    } else {
        std::env::current_dir()?.join(&args.fuchsia_dir)
    };

    let abs_fuchsia_dir = fs::canonicalize(&abs_fuchsia_dir)
        .with_context(|| format!("failed to canonicalize fuchsia-dir: {:?}", abs_fuchsia_dir))?;

    let mut metadata_map: HashMap<PathBuf, TestingMetadataSource> = HashMap::new();
    let mut ownership_collector =
        args.directory_owners_file_output.as_ref().map(|_| owners::OwnershipCollector::default());
    let mut dep_files = Vec::new();

    let ignore_files = HashSet::from(["out", "prebuilt", "third_party"]);

    // Load the schema for validation.
    let mut scope = json_schema::Scope::new();
    let schema_json: serde_json::Value = serde_json::from_str(INPUT_SCHEMA)?;
    let schema = scope
        .compile_and_return(schema_json, false)
        .map_err(|e| anyhow::anyhow!("invalid input schema: {:?}", e))?;

    let mut validation_errors = Vec::new();
    let mut all_dirs = Vec::new();

    for entry in WalkDir::new(&abs_fuchsia_dir).into_iter().filter_entry(|e| {
        if e.file_type().is_dir() {
            let name = e.file_name().to_string_lossy();
            if e.depth() == 1 && (name.starts_with('.') || ignore_files.contains(name.as_ref())) {
                return false;
            }
        }
        true
    }) {
        let entry = entry.context("failed to read directory entry")?;
        if entry.file_type().is_dir() {
            let rel_dir = entry.path().strip_prefix(&abs_fuchsia_dir)?;
            if rel_dir == Path::new("") {
                all_dirs.push(PathBuf::from(""));
            } else {
                all_dirs.push(rel_dir.to_path_buf());
            }
            continue;
        }

        let path = entry.path();
        if let Some(collector) = &mut ownership_collector {
            let rel_path = path
                .strip_prefix(&abs_fuchsia_dir)
                .context("failed to get relative path for ownership collector")?;
            collector.visit_file(path, rel_path);
        }

        if entry.file_name() != "TESTING.json5" {
            continue;
        }
        let content =
            fs::read_to_string(path).with_context(|| format!("failed to read {:?}", path))?;
        let json_value: serde_json::Value = match serde_json5::from_str(&content) {
            Ok(v) => v,
            Err(e) => {
                validation_errors.push(anyhow::anyhow!("failed to parse {:?}: {e:?}", path));
                continue;
            }
        };

        // Validate the input file against the schema.
        let validate_result = schema.validate(&json_value);
        if !validate_result.is_valid() {
            validation_errors.push(anyhow::anyhow!(
                "file {:?} does not match schema: {:?}",
                path,
                validate_result.errors
            ));
            continue;
        }

        let source: TestingMetadataSource = serde_json::from_value(json_value)
            .with_context(|| format!("failed to decode {:?}", path))?;

        let rel_dir = entry
            .path()
            .parent()
            .unwrap()
            .strip_prefix(&abs_fuchsia_dir)
            .context("failed to get relative path")?;

        let key = if rel_dir == Path::new("") { PathBuf::from("") } else { rel_dir.to_path_buf() };
        metadata_map.insert(key, source);
        let rel_path = path
            .strip_prefix(&abs_fuchsia_dir)
            .context("failed to get relative path for depfile")?;
        if !rel_path.is_absolute() {
            // CQ has a check that depfiles are not absolute. We can add the relative paths from
            // local builds here, but in CQ we have to skip adding depfiles.
            //
            // Fortunately, in CQ we do not modify the TESTING.json5 files directly, and the
            // dependency on build.ninja should be sufficient to ensure that this file is
            // regenerated following a new gn gen.
            dep_files.push(args.fuchsia_dir.join(rel_path));
        }
    }

    if !validation_errors.is_empty() {
        // Validation failed, join all errors in a string and return.
        return Err(anyhow::anyhow!(
            "validation errors:\n{}",
            validation_errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n"),
        ));
    }

    let mut final_metadata = BTreeMap::new();
    for dir in &all_dirs {
        let mut merged_source = TestingMetadataDirectoryOutput::default();

        let mut components = Vec::new();
        let mut p = dir.as_path();
        while p != Path::new("") && p != Path::new(".") {
            components.push(p);
            p = match p.parent() {
                Some(parent) => parent,
                None => break,
            };
        }
        components.push(Path::new(""));
        components.reverse();

        for component in components {
            if let Some(source) = metadata_map.get(component) {
                merged_source.parent_directory = component.to_string_lossy().into_owned();
                merge_metadata(&mut merged_source, source);
            }
        }

        final_metadata.insert(dir.to_string_lossy().into_owned(), merged_source);
    }

    let output_json = MetadataOutput { metadata: final_metadata, version: 1 };
    write_pretty_json(&args.output, &output_json)?;

    if let (Some(ownership_output_path), Some(mut collector)) =
        (&args.directory_owners_file_output, ownership_collector)
    {
        let (ownership_output, ownership_deps) = collector.finish(&all_dirs, &abs_fuchsia_dir);
        write_pretty_json(ownership_output_path, &ownership_output)?;
        for rel_dep in ownership_deps {
            if !rel_dep.is_absolute() {
                dep_files.push(args.fuchsia_dir.join(rel_dep));
            }
        }
    }

    let mut depfile_content = match &args.directory_owners_file_output {
        Some(ownership_output_path) => format!(
            "{} {}:",
            args.output.to_string_lossy(),
            ownership_output_path.to_string_lossy()
        ),
        None => format!("{}:", args.output.to_string_lossy()),
    };
    // Rebuild the testing list if gn gen is run too.
    dep_files.push(PathBuf::from("build.ninja"));
    dep_files.sort();
    dep_files.dedup();
    for dep in dep_files {
        depfile_content.push_str(&format!(" {}", dep.to_string_lossy()));
    }
    if let Some(parent) = args.depfile.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.depfile, depfile_content)
        .with_context(|| format!("failed to write depfile: {:?}", args.depfile))?;

    Ok(())
}

/// Merges coverage category, subcategory, and tag metadata from an ancestor or current directory's
/// `TESTING.json5` (`source`) into `target`.
fn merge_metadata(target: &mut TestingMetadataDirectoryOutput, source: &TestingMetadataSource) {
    if let Some(source_cov) = &source.coverage {
        if target.coverage.is_none() {
            target.coverage = Some(Coverage::default());
        }
        let target_cov = target.coverage.as_mut().unwrap();

        if let Some(cat) = &source_cov.category {
            target_cov.category = Some(cat.clone());
        }
        if let Some(subcat) = &source_cov.subcategory {
            target_cov.subcategory = Some(subcat.clone());
        }
        if let Some(inherit) = source_cov.inherit_tags {
            if !inherit {
                target_cov.tags = None;
            }
        }
        if let Some(tags) = &source_cov.tags {
            if target_cov.tags.is_none() {
                target_cov.tags = Some(Vec::new());
            }
            target_cov.tags.as_mut().unwrap().extend(tags.iter().cloned());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    const OUTPUT_SCHEMA: &str = include_str!("../testing_metadata.schema.json");

    /// Protects against: regressions in directory inheritance, tag override/accumulation, schema
    /// conformance, and depfile emission for `TESTING.json5`.
    #[test]
    fn test_integration() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        // Create TESTING.json5 at root
        fs::write(
            root.join("TESTING.json5"),
            r#"{
            "coverage": {
                "category": "root_cat",
                "tags": ["tag1"]
            }
        }"#,
        )?;

        // Create a subdirectory with its own TESTING.json5
        let sub = root.join("sub");
        fs::create_dir(&sub)?;
        fs::write(
            sub.join("TESTING.json5"),
            r#"{
            "coverage": {
                "subcategory": "sub_cat",
                "tags": ["tag2"]
            }
        }"#,
        )?;

        // Create another subdirectory without its own TESTING.toml
        let testdir = sub.join("testdir");
        fs::create_dir(&testdir)?;

        let nested = testdir.join("nested");
        fs::create_dir(&nested)?;
        fs::write(
            nested.join("TESTING.json5"),
            r#"{
            "coverage": {
                "category": "nested_cat",
                "subcategory": "nested_subcat",
                "inherit_tags": false,
                "tags": ["nested_tag"]
            }
        }"#,
        )?;

        let output = root.join("out.json");
        let depfile = root.join("out.d");

        let args = Args {
            fuchsia_dir: root.to_path_buf(),
            output: output.clone(),
            directory_owners_file_output: None,
            depfile: depfile.clone(),
        };

        collect_metadata(args)?;

        let json_content = fs::read_to_string(output)?;
        let result_json: serde_json::Value = serde_json::from_str(&json_content)?;

        let mut scope = json_schema::Scope::new();
        let schema_json: serde_json::Value = serde_json::from_str(OUTPUT_SCHEMA)?;
        let schema = scope
            .compile_and_return(schema_json, false)
            .map_err(|e| anyhow::anyhow!("invalid output schema: {:?}", e))?;
        let validate_result = schema.validate(&result_json);
        if !validate_result.is_valid() {
            panic!("output does not match schema: {:?}", validate_result.errors);
        }

        let result: MetadataOutput = serde_json::from_value(result_json)?;

        let expected_output = r#"
        {
            "version": 1,
            "metadata": {
                "": {
                    "coverage": {
                        "category": "root_cat",
                        "tags": ["tag1"]
                    },
                    "parent_directory": ""
                },
                "sub": {
                    "coverage": {
                        "category": "root_cat",
                        "subcategory": "sub_cat",
                        "tags": ["tag1", "tag2"]
                    },
                    "parent_directory": "sub"
                },
                "sub/testdir": {
                    "coverage": {
                        "category": "root_cat",
                        "subcategory": "sub_cat",
                        "tags": ["tag1", "tag2"]
                    },
                    "parent_directory": "sub"
                },
                "sub/testdir/nested": {
                    "coverage": {
                        "category": "nested_cat",
                        "subcategory": "nested_subcat",
                        "tags": ["nested_tag"]
                    },
                    "parent_directory": "sub/testdir/nested"
                }
            }
        }
        "#;

        let expected: MetadataOutput = serde_json::from_str(expected_output)?;

        assert_eq!(result, expected);

        // Check depfile
        let dep_content = fs::read_to_string(depfile)?;
        assert!(dep_content.contains("TESTING.json5"));

        Ok(())
    }

    /// Protects against: CLI wiring regressions where `--directory-owners-file-output` fails to
    /// write `directory_owners.json` or omits it from the multi-output Ninja depfile rule LHS.
    #[test]
    fn test_ownership_cli_output_and_depfile() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        fs::write(root.join("TESTING.json5"), r#"{ "coverage": { "category": "root_cat" } }"#)?;
        fs::create_dir_all(root.join("src"))?;
        fs::write(root.join("src/OWNERS"), "owner@google.com\n")?;

        let output = root.join("testing_metadata.json");
        let ownership_output = root.join("directory_owners.json");
        let depfile = root.join("testing_metadata.json.d");

        collect_metadata(Args {
            fuchsia_dir: root.to_path_buf(),
            output: output.clone(),
            directory_owners_file_output: Some(ownership_output.clone()),
            depfile: depfile.clone(),
        })?;

        let parsed: owners::DirectoryOwnersOutput =
            serde_json::from_str(&fs::read_to_string(&ownership_output)?)?;
        assert_eq!(parsed.directories["src"].owners.len(), 1);
        assert_eq!(parsed.directories["src"].owners[0].email, "owner@google.com");

        let dep_content = fs::read_to_string(depfile)?;
        assert!(dep_content.starts_with(&format!(
            "{} {}:",
            output.to_string_lossy(),
            ownership_output.to_string_lossy()
        )));
        assert!(dep_content.contains("src/OWNERS"));
        Ok(())
    }

    /// Protects against: silent acceptance of malformed JSON5 or schema-invalid `TESTING.json5`
    /// files, and early termination that hides subsequent validation errors.
    #[test]
    fn test_validation_errors() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path();

        // Create a valid TESTING.json5
        let valid_dir = root.join("valid");
        fs::create_dir(&valid_dir)?;
        fs::write(
            valid_dir.join("TESTING.json5"),
            r#"{
            "coverage": {
                "category": "valid_cat"
            }
        }"#,
        )?;

        // Create an invalid TESTING.json5 (parse error)
        let invalid_parse_dir = root.join("invalid_parse");
        fs::create_dir(&invalid_parse_dir)?;
        fs::write(
            invalid_parse_dir.join("TESTING.json5"),
            r#"{
            "coverage": {
                "category": "invalid_parse"
            }
            "invalid":
        }"#,
        )?;

        // Create an invalid TESTING.json5 (schema error)
        let invalid_schema_dir = root.join("invalid_schema");
        fs::create_dir(&invalid_schema_dir)?;
        fs::write(
            invalid_schema_dir.join("TESTING.json5"),
            r#"{
            "coverage": {
                "category": 123
            }
        }"#,
        )?;

        let output = root.join("out.json");
        let depfile = root.join("out.d");

        let args = Args {
            fuchsia_dir: root.to_path_buf(),
            output: output.clone(),
            directory_owners_file_output: None,
            depfile: depfile.clone(),
        };

        let result = collect_metadata(args);
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(err_msg.contains("validation errors:"));
        assert!(err_msg.contains("failed to parse"));
        assert!(err_msg.contains("does not match schema"));

        Ok(())
    }
}
