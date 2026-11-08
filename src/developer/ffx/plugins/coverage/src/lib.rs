// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ffx_coverage_args::CoverageCommand;
use ffx_writer::{ToolIO, VerifiedMachineWriter};
use fho::{FfxMain, FfxTool};
use glob::glob;
use rayon::prelude::*;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::Mutex;
use symbol_index::{SymbolIndex, global_symbol_index_path};
use tempfile::tempdir;

// The line found right above build ID in `llvm-profdata show --binary-ids` output.
const BINARY_ID_LINE: &str = "Binary IDs:";

/// Errors that can occur while running coverage extraction and reporting operations.
#[derive(Debug, thiserror::Error)]
pub enum CoverageError {
    #[error("failed to run command {bin:?}: {source}")]
    CommandIo {
        bin: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("command terminated unexpectedly by signal")]
    CommandTerminatedBySignal,
    #[error("failed to create temp directory for merging: {0}")]
    TempDir(std::io::Error),
    #[error("failed to create file {path:?}: {source}")]
    FileCreate {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no profiles could be merged")]
    NoProfilesMerged,
    #[error("failed to merge raw profiles: status code {code}, stderr: {stderr}")]
    ProfileMergeFailed { code: i32, stderr: String },
    #[error("failed to show binary IDs: status code {code}, stderr: {stderr}")]
    ShowBinaryIdsFailed { code: i32, stderr: String },
    #[error("unexpected llvm-profdata show output: {0}")]
    UnexpectedProfdataOutput(String),
    #[error("binary ID must have more than 2 characters, got '{0}'")]
    BinaryIdTooShort(String),
    #[error("no matching debug files found for binary ID {0}")]
    DebugFileNotFound(String),
    #[error("failed to find binary file for ID {id} in {path:?}: {source}")]
    BinaryResolution {
        id: String,
        path: PathBuf,
        #[source]
        source: Box<CoverageError>,
    },
    #[error("failed to show coverage: status code {code}, stderr: {stderr}")]
    ShowCoverageFailed { code: i32, stderr: String },
    #[error("failed to export HTML coverage: status code {code}, stderr: {stderr}")]
    HtmlExportFailed { code: i32, stderr: String },
    #[error("failed to export LCOV: status code {code}, stderr: {stderr}")]
    LcovExportFailed { code: i32, stderr: String },
    #[error("failed to export JSON: status code {code}, stderr: {stderr}")]
    JsonExportFailed { code: i32, stderr: String },
    #[error("failed to glob profraw files: {0}")]
    Glob(#[from] glob::PatternError),
    #[error("invalid path encoding for pattern: {0:?}")]
    InvalidPath(PathBuf),
}

impl From<CoverageError> for fho::Error {
    fn from(e: CoverageError) -> Self {
        fho::user_error!("{e}")
    }
}

/// A convenient struct grouping common parameters to export/show functions.
struct ExportParams<'a> {
    llvm_cov_bin: PathBuf,
    merged_profile: PathBuf,
    bin_files_args: Vec<&'a str>,
    src_files: Vec<PathBuf>,
    extra_args: Vec<&'a str>,
}

/// Contains paths to generated report files or directories based on the flags passed
/// to the tool or default machine output mode.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema, PartialEq)]
pub struct CoverageOutput {
    /// Path to the JSON coverage export file if generated or defaulted in machine mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_path: Option<PathBuf>,
    /// Path to the HTML coverage directory if `--export-html` was specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html_path: Option<PathBuf>,
    /// Path to the LCOV coverage export file if `--export-lcov` was specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lcov_path: Option<PathBuf>,
}

#[derive(FfxTool)]
pub struct CoverageTool {
    #[command]
    cmd: CoverageCommand,
    context: ffx_config::EnvironmentContext,
}

fho::embedded_plugin!(CoverageTool);

impl FfxMain for CoverageTool {
    type Writer = VerifiedMachineWriter<CoverageOutput>;

    type Error = ::fho::Error;

    async fn main(self, mut writer: Self::Writer) -> fho::Result<()> {
        coverage(self.cmd, &self.context, &mut writer).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum VerboseMode {
    NotVerbose,
    Verbose,
}

impl From<bool> for VerboseMode {
    fn from(v: bool) -> Self {
        if v { Self::Verbose } else { Self::NotVerbose }
    }
}

pub async fn coverage(
    cmd: CoverageCommand,
    context: &ffx_config::EnvironmentContext,
    writer: &mut VerifiedMachineWriter<CoverageOutput>,
) -> fho::Result<()> {
    let verbose_mode = VerboseMode::from(cmd.verbose);
    let clang_bin_dir = cmd.clang_dir.join("bin");
    let llvm_profdata_bin = clang_bin_dir.join("llvm-profdata");
    llvm_profdata_bin
        .exists()
        .then_some(())
        .ok_or_else(|| fho::user_error!("{:?} does not exist", llvm_profdata_bin))?;
    let llvm_cov_bin = clang_bin_dir.join("llvm-cov");
    llvm_cov_bin
        .exists()
        .then_some(())
        .ok_or_else(|| fho::user_error!("{:?} does not exist", llvm_cov_bin))?;

    let profdata_runner = ProfdataRunner::new(llvm_profdata_bin, verbose_mode);

    let profraws = glob_profraws(&cmd.test_output_dir)?;
    if profraws.is_empty() {
        if writer.is_machine() {
            writer.machine(&CoverageOutput::default())?;
        }
        return Ok(());
    }

    let symbol_index_path = match cmd.symbol_index_json {
        Some(p) => p.to_string_lossy().to_string(),
        None => global_symbol_index_path(context)
            .map_err(|e| fho::user_error!("failed to get global symbol index path: {}", e))?,
    };
    let symbol_index = SymbolIndex::load_aggregate(&symbol_index_path).map_err(|e| {
        fho::user_error!("failed to load symbol index from {}: {}", symbol_index_path, e)
    })?;
    let bin_files = profdata_runner.find_binaries(&symbol_index, &profraws)?;

    let merged_profile = cmd.test_output_dir.join("merged.profdata");
    profdata_runner.merge_profraws(&profraws, &merged_profile, &bin_files)?;

    // Flatten the binaries for llvm-cov and ensure they are unique.
    let mut unique_binaries = HashSet::new();
    for bins in &bin_files {
        for bin in bins {
            unique_binaries.insert(bin.clone());
        }
    }
    let unique_binaries_vec: Vec<PathBuf> = unique_binaries.into_iter().collect();

    let extra_args = to_extra_export_args(
        &cmd.path_remappings,
        cmd.compilation_dir.as_ref(),
        cmd.include_filename_regex.as_deref(),
        cmd.ignore_filename_regex.as_deref(),
    );

    let params = ExportParams {
        llvm_cov_bin,
        merged_profile,
        bin_files_args: to_llvm_cov_args(&unique_binaries_vec),
        src_files: cmd.src_files,
        extra_args,
    };

    let is_export =
        cmd.export_html.is_some() || cmd.export_lcov.is_some() || cmd.export_json.is_some();

    if !writer.is_machine() && !is_export {
        show_coverage(&params, verbose_mode)?;
        return Ok(());
    }

    if let Some(ref html_export_dir) = cmd.export_html {
        export_html(&params, html_export_dir, verbose_mode)?;
    }

    if let Some(ref lcov_export_path) = cmd.export_lcov {
        export_lcov(&params, lcov_export_path, verbose_mode)?;
    }

    let default_json_path = cmd.test_output_dir.join("coverage.json");
    let json_export_path = cmd
        .export_json
        .or_else(|| (writer.is_machine() && !is_export).then_some(default_json_path));

    if let Some(ref json_path) = json_export_path {
        export_json(&params, json_path, verbose_mode)?;
    }

    if writer.is_machine() {
        let output = CoverageOutput {
            json_path: json_export_path,
            html_path: cmd.export_html,
            lcov_path: cmd.export_lcov,
        };
        writer.machine(&output)?;
    }

    Ok(())
}

struct ProfdataRunner {
    llvm_profdata_bin: PathBuf,
    verbose: VerboseMode,
    #[cfg(test)]
    mock_binary_ids: Mutex<HashMap<PathBuf, Vec<String>>>,
}

impl ProfdataRunner {
    fn new(llvm_profdata_bin: PathBuf, verbose: VerboseMode) -> Self {
        #[cfg(test)]
        {
            Self { llvm_profdata_bin, verbose, mock_binary_ids: Mutex::new(HashMap::new()) }
        }
        #[cfg(not(test))]
        {
            Self { llvm_profdata_bin, verbose }
        }
    }

    #[cfg(test)]
    fn add_mock_binary_ids(&self, binary_ids: HashMap<PathBuf, Vec<String>>) {
        self.mock_binary_ids.lock().unwrap().extend(binary_ids);
    }

    /// Merges input `profraws` using llvm-profdata and writes output to `output_path`.
    fn merge_profraws(
        &self,
        profraws: &[PathBuf],
        output_path: &Path,
        bin_files: &[Vec<PathBuf>],
    ) -> Result<(), CoverageError> {
        let temp_dir = tempdir().map_err(CoverageError::TempDir)?;

        let individual_profdatas: Vec<PathBuf> = profraws
            .par_iter()
            .zip(bin_files.par_iter())
            .enumerate()
            .filter_map(|(i, (profraw, bins))| {
                let temp_profdata = temp_dir.path().join(format!("{}.profdata", i));
                let mut cmd = Command::new(&self.llvm_profdata_bin);
                cmd.args(["merge", "--sparse", "--output"]).arg(&temp_profdata);
                for bin in bins {
                    cmd.arg("--binary-file").arg(bin);
                }
                cmd.arg(profraw);

                match run_with_verbose(&mut cmd, self.verbose) {
                    Ok(merge_cmd) if merge_cmd.status.success() => Some(temp_profdata),
                    _ => {
                        if self.verbose == VerboseMode::Verbose {
                            eprintln!(
                                "Warning: failed to merge {:?} with binaries {:?}",
                                profraw, bins
                            );
                        }
                        None
                    }
                }
            })
            .collect();

        if individual_profdatas.is_empty() {
            return Err(CoverageError::NoProfilesMerged);
        }

        let mut final_cmd = Command::new(&self.llvm_profdata_bin);
        final_cmd
            .args(["merge", "--sparse", "--output"])
            .arg(output_path)
            .args(individual_profdatas);

        let final_merge_cmd = run_with_verbose(&mut final_cmd, self.verbose)?;

        match final_merge_cmd.status.code() {
            Some(0) => Ok(()),
            Some(code) => Err(CoverageError::ProfileMergeFailed {
                code,
                stderr: String::from_utf8_lossy(&final_merge_cmd.stderr).into_owned(),
            }),
            None => Err(CoverageError::CommandTerminatedBySignal),
        }
    }

    /// Calls `llvm-profdata show --binary-ids` to fetch binary IDs from input raw profile.
    fn show_binary_ids(&self, profraw: &Path) -> Result<Vec<String>, CoverageError> {
        #[cfg(test)]
        {
            let binary_ids_map = self.mock_binary_ids.lock().unwrap();
            if let Some(ids) = binary_ids_map.get(profraw) {
                return Ok(ids.clone());
            }
            // If mock binary IDs are not configured for this profraw in tests,
            // fall back to executing `llvm-profdata` (which may be a test dummy script).
        }
        let cmd = run_with_verbose(
            Command::new(&self.llvm_profdata_bin).args(["show", "--binary-ids"]).arg(profraw),
            self.verbose,
        )?;
        match cmd.status.code() {
            Some(0) => {}
            Some(code) => {
                return Err(CoverageError::ShowBinaryIdsFailed {
                    code,
                    stderr: String::from_utf8_lossy(&cmd.stderr).into_owned(),
                });
            }
            None => return Err(CoverageError::CommandTerminatedBySignal),
        }
        let stdout = String::from_utf8_lossy(&cmd.stdout);
        let tokens: Vec<&str> = stdout.split(BINARY_ID_LINE).collect();
        match tokens[..] {
            [_, binary_ids_str] => Ok(binary_ids_str
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()),
            _ => Err(CoverageError::UnexpectedProfdataOutput(stdout.into_owned())),
        }
    }

    /// Find binary files from .build-id directories to pass. These are needed for `llvm-cov show`.
    fn find_binaries(
        &self,
        symbol_index: &SymbolIndex,
        profraws: &[PathBuf],
    ) -> Result<Vec<Vec<PathBuf>>, CoverageError> {
        profraws
            .par_iter()
            .map(|profraw| {
                let binary_ids = self.show_binary_ids(profraw)?;
                binary_ids
                    .iter()
                    .map(|binary_id| {
                        find_debug_file(symbol_index, binary_id).map_err(|source| {
                            CoverageError::BinaryResolution {
                                id: binary_id.clone(),
                                path: profraw.clone(),
                                source: Box::new(source),
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, CoverageError>>()
            })
            .collect()
    }
}

/// Finds debug file in local .build-id directories from symbol index.
fn find_debug_file(symbol_index: &SymbolIndex, binary_id: &str) -> Result<PathBuf, CoverageError> {
    if binary_id.len() > 2 {
        // For simplicity always return the first match. Note this is not always safe.
        symbol_index
            .build_id_dirs
            .iter()
            .find_map(|dir| {
                let p = PathBuf::from(&dir.path)
                    .join(&binary_id[..2])
                    .join(format!("{}.debug", &binary_id[2..]));
                p.exists().then_some(p)
            })
            .ok_or_else(|| CoverageError::DebugFileNotFound(binary_id.to_string()))
    } else {
        Err(CoverageError::BinaryIdTooShort(binary_id.to_string()))
    }
}

fn to_llvm_cov_args(bin_files: &[PathBuf]) -> Vec<&str> {
    bin_files.iter().fold(Vec::new(), |mut acc, val| {
        if acc.len() > 0 {
            acc.push("-object");
        }
        acc.push(val.to_str().expect("failed to convert path to string"));
        acc
    })
}

fn to_extra_export_args<'a>(
    path_remappings: &'a [String],
    compilation_dir: Option<&'a PathBuf>,
    include_filename_regex: Option<&'a str>,
    ignore_filename_regex: Option<&'a str>,
) -> Vec<&'a str> {
    match path_remappings {
        &[] => Vec::new(),
        _ => ["-path-equivalence"]
            .into_iter()
            .chain(path_remappings.iter().map(|s| s.as_str()))
            .collect(),
    }
    .into_iter()
    .chain(match compilation_dir {
        Some(dir) => vec!["-compilation-dir", dir.to_str().unwrap()],
        None => Vec::new(),
    })
    .chain(match include_filename_regex {
        Some(include_regex) => vec!["--include-filename-regex", include_regex],
        None => Vec::new(),
    })
    .chain(match ignore_filename_regex {
        Some(ignore_regex) => vec!["--ignore-filename-regex", ignore_regex],
        None => Vec::new(),
    })
    .collect()
}

/// Calls `llvm-cov show` to display coverage from `merged_profile` for `bin_files`.
/// `src_files` can be used to filter coverage for selected source files.
fn show_coverage(
    params: &ExportParams<'_>,
    verbose_mode: VerboseMode,
) -> Result<(), CoverageError> {
    let show_cmd = run_with_verbose(
        Command::new(&params.llvm_cov_bin)
            .args(["show", "-instr-profile"])
            .arg(&params.merged_profile)
            .args(&params.extra_args)
            .args(&params.bin_files_args)
            .args(&params.src_files)
            .stdout(Stdio::inherit()),
        verbose_mode,
    )?;
    match show_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(CoverageError::ShowCoverageFailed {
            code,
            stderr: String::from_utf8_lossy(&show_cmd.stderr).into_owned(),
        }),
        None => Err(CoverageError::CommandTerminatedBySignal),
    }
}

/// Calls `llvm-cov show -format html` to write HTML pages for collected test coverage.
fn export_html(
    params: &ExportParams<'_>,
    html_export_path: &Path,
    verbose_mode: VerboseMode,
) -> Result<(), CoverageError> {
    let show_cmd = run_with_verbose(
        Command::new(&params.llvm_cov_bin)
            .args(["show", "-format", "html", "-output-dir"])
            .arg(html_export_path)
            .arg("-instr-profile")
            .arg(&params.merged_profile)
            .args(&params.extra_args)
            .args(&params.bin_files_args)
            .args(&params.src_files),
        verbose_mode,
    )?;
    match show_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(CoverageError::HtmlExportFailed {
            code,
            stderr: String::from_utf8_lossy(&show_cmd.stderr).into_owned(),
        }),
        None => Err(CoverageError::CommandTerminatedBySignal),
    }
}

/// Calls `llvm-cov export -format lcov` to write a LCOV file for collected test coverage.
fn export_lcov(
    params: &ExportParams<'_>,
    lcov_export_path: &Path,
    verbose_mode: VerboseMode,
) -> Result<(), CoverageError> {
    let output_lcov = File::create(lcov_export_path).map_err(|source| {
        CoverageError::FileCreate { path: lcov_export_path.to_path_buf(), source }
    })?;
    let show_cmd = run_with_verbose(
        Command::new(&params.llvm_cov_bin)
            .args(["export", "-format", "lcov", "-skip-expansions", "-skip-functions"])
            .arg("-instr-profile")
            .arg(&params.merged_profile)
            .args(&params.extra_args)
            .args(&params.bin_files_args)
            .args(&params.src_files)
            .stdout(output_lcov),
        verbose_mode,
    )?;

    match show_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(CoverageError::LcovExportFailed {
            code,
            stderr: String::from_utf8_lossy(&show_cmd.stderr).into_owned(),
        }),
        None => Err(CoverageError::CommandTerminatedBySignal),
    }
}

/// Calls `llvm-cov export -format text` to write a JSON file for collected test coverage.
fn export_json(
    params: &ExportParams<'_>,
    json_export_path: &Path,
    verbose_mode: VerboseMode,
) -> Result<(), CoverageError> {
    let output_json = File::create(json_export_path).map_err(|source| {
        CoverageError::FileCreate { path: json_export_path.to_path_buf(), source }
    })?;
    let show_cmd = run_with_verbose(
        Command::new(&params.llvm_cov_bin)
            .args(["export", "-format", "text"])
            .arg("-instr-profile")
            .arg(&params.merged_profile)
            .args(&params.extra_args)
            .args(&params.bin_files_args)
            .args(&params.src_files)
            .stdout(output_json),
        verbose_mode,
    )?;

    match show_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(CoverageError::JsonExportFailed {
            code,
            stderr: String::from_utf8_lossy(&show_cmd.stderr).into_owned(),
        }),
        None => Err(CoverageError::CommandTerminatedBySignal),
    }
}

/// Finds all raw coverage profiles in `test_out_dir`.
fn glob_profraws(test_out_dir: &Path) -> Result<Vec<PathBuf>, CoverageError> {
    let pattern = test_out_dir.join("**").join("*.profraw");
    let pattern_str =
        pattern.to_str().ok_or_else(|| CoverageError::InvalidPath(pattern.clone()))?;
    Ok(glob(pattern_str)?.filter_map(std::result::Result::ok).collect::<Vec<PathBuf>>())
}

/// Maximum attempts to retry executing a command when encountering ETXTBSY.
const ETXTBSY_MAX_RETRIES: usize = 50;
/// Delay between retries when encountering ETXTBSY.
const ETXTBSY_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Retries an operation if it fails with `ErrorKind::ExecutableFileBusy` (`ETXTBSY`).
///
/// In multi-threaded test runners, when one thread writes a mock script file and another thread
/// fork/execs concurrently, child processes may momentarily inherit write file descriptors,
/// causing immediate `execve` calls on the written script to return `ETXTBSY`. Retrying with
/// a brief delay allows the forked child to complete `execve` and close inherited descriptors.
fn retry_on_etxtbsy<T>(mut op: impl FnMut() -> std::io::Result<T>) -> std::io::Result<T> {
    let mut attempts = 0;
    loop {
        match op() {
            Ok(val) => return Ok(val),
            Err(e)
                if (e.kind() == std::io::ErrorKind::ExecutableFileBusy
                    || e.raw_os_error() == Some(26))
                    && attempts < ETXTBSY_MAX_RETRIES =>
            {
                attempts += 1;
                std::thread::sleep(ETXTBSY_RETRY_DELAY);
            }
            Err(e) => return Err(e),
        }
    }
}

/// Run a command, respecting the --verbose setting to output command line and outputs if set.
fn run_with_verbose(
    cmd: &mut Command,
    verbose_mode: VerboseMode,
) -> Result<std::process::Output, CoverageError> {
    if verbose_mode == VerboseMode::Verbose {
        eprintln!("Command: {:?}", cmd);
    }
    let cmd_output = retry_on_etxtbsy(|| cmd.output()).map_err(|source| {
        CoverageError::CommandIo { bin: PathBuf::from(cmd.get_program()), source }
    })?;
    if verbose_mode == VerboseMode::Verbose {
        eprintln!("Command stdout:\n{}", String::from_utf8_lossy(&cmd_output.stdout));
        eprintln!("Command stderr:\n{}", String::from_utf8_lossy(&cmd_output.stderr));
    }
    Ok(cmd_output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use std::fs::{Permissions, create_dir, set_permissions};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use symbol_index::BuildIdDir;
    use tempfile::TempDir;

    // Guard against Linux ETXTBSY caused by concurrent fork/exec across test threads
    // inheriting open write file descriptors (https://github.com/rust-lang/rust/issues/114554).
    static SCRIPT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn test_glob_profraws() {
        let test_dir = TempDir::new().unwrap();
        create_dir(test_dir.path().join("nested")).unwrap();

        File::create(test_dir.path().join("foo.profraw")).unwrap();
        File::create(test_dir.path().join("nested").join("bar.profraw")).unwrap();
        File::create(test_dir.path().join("foo.not_profraw")).unwrap();
        File::create(test_dir.path().join("nested").join("baz.not_profraw")).unwrap();

        assert_eq!(
            glob_profraws(&test_dir.path().to_path_buf()).unwrap(),
            vec![
                PathBuf::from(test_dir.path().join("foo.profraw")),
                PathBuf::from(test_dir.path().join("nested").join("bar.profraw")),
            ],
        );
    }

    #[fuchsia::test]
    async fn test_coverage() {
        let _lock = SCRIPT_LOCK.lock().unwrap();
        let env = ffx_config::test_init().unwrap();

        let test_dir = TempDir::new().unwrap();
        let test_dir_path = test_dir.path().to_path_buf();
        let test_bin_dir = test_dir_path.join("bin");
        create_dir(&test_bin_dir).unwrap();

        // Create an empty symbol index for testing.
        let test_symbol_index_json = test_dir.path().join("symbol_index.json");
        File::create(&test_symbol_index_json).unwrap().write_all(b"{}").unwrap();

        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::new_test(None, &test_buffers);

        // Missing both llvm-profdata and llvm-cov.
        assert!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await
            .is_err()
        );

        // Create empty test binaries for the coverage function to call.
        File::create(test_bin_dir.join("llvm-profdata"))
            .unwrap()
            .write_all(
                b"#!/bin/sh\n\
                echo \"Binary IDs:\"",
            )
            .unwrap();
        set_permissions(test_bin_dir.join("llvm-profdata"), Permissions::from_mode(0o770)).unwrap();

        // Still missing llvm-cov.
        assert!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await
            .is_err()
        );

        File::create(test_bin_dir.join("llvm-cov"))
            .unwrap()
            .write_all(
                b"#!/bin/sh\n\
                for arg in \"$@\"; do\n\
                  if [ \"$arg\" = \"text\" ]; then\n\
                    echo '{\"data\":[],\"type\":\"llvm.coverage.json.export\",\"version\":\"2.0.1\"}'\n\
                    exit 0\n\
                  fi\n\
                done",
            )
            .unwrap();
        set_permissions(test_bin_dir.join("llvm-cov"), Permissions::from_mode(0o770)).unwrap();

        File::create(test_dir_path.join("foo.profraw")).unwrap();

        // Print coverage to stdout.
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await,
            Ok(())
        );

        // Export HTML.
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: Some(PathBuf::from(&test_dir_path)),
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await,
            Ok(())
        );

        // Export LCOV.
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: Some(PathBuf::from(&test_dir_path).join("test.lcov")),
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await,
            Ok(())
        );

        // Export JSON.
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: Some(PathBuf::from(&test_dir_path).join("test.json")),
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await,
            Ok(())
        );

        // Export with non-empty path_remappings and compilation_dir.
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: vec![
                        "from_path,to_path".to_string(),
                        "from_path2,to_path2".to_string(),
                    ],
                    compilation_dir: Some(PathBuf::from("path/to/comp/dir")),
                    include_filename_regex: Some("include_pat".to_string()),
                    ignore_filename_regex: Some("ignore_pat".to_string()),
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut writer,
            )
            .await,
            Ok(())
        );

        // Test machine mode (no export flags) emits status pointing to default coverage.json file.
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut machine_writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut machine_writer,
            )
            .await,
            Ok(())
        );
        let machine_output = test_buffers.into_stdout_str();
        assert!(machine_output.contains("coverage.json"));
        let default_coverage_json = test_dir_path.join("coverage.json");
        assert!(default_coverage_json.exists());
        let default_coverage_content = std::fs::read_to_string(&default_coverage_json).unwrap();
        assert!(default_coverage_content.contains("llvm.coverage.json.export"));

        // Test machine mode with secondary export (e.g. HTML) emits status pointing to html path.
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut machine_writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: Some(PathBuf::from(&test_dir_path)),
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut machine_writer,
            )
            .await,
            Ok(())
        );
        let machine_output = test_buffers.into_stdout_str();
        assert!(machine_output.contains("html_path"));

        // Test machine mode with JSON export emits status pointing to json path and writes file.
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut machine_writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        let machine_export_json = test_dir_path.join("machine_test.json");
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: Some(machine_export_json.clone()),
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut machine_writer,
            )
            .await,
            Ok(())
        );
        let machine_output = test_buffers.into_stdout_str();
        assert!(machine_output.contains("machine_test.json"));
        assert!(machine_export_json.exists());
        let machine_export_content = std::fs::read_to_string(&machine_export_json).unwrap();
        assert!(machine_export_content.contains("llvm.coverage.json.export"));

        // Test machine mode with empty profraws outputs empty object without failing.
        let empty_test_dir = TempDir::new().unwrap();
        let empty_test_dir_path = empty_test_dir.path().to_path_buf();
        let test_buffers = ffx_writer::TestBuffers::default();
        let mut machine_writer =
            VerifiedMachineWriter::new_test(Some(ffx_writer::Format::Json), &test_buffers);
        assert_matches!(
            coverage(
                CoverageCommand {
                    test_output_dir: PathBuf::from(&empty_test_dir_path),
                    clang_dir: PathBuf::from(&test_dir_path),
                    symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                    export_html: None,
                    export_lcov: None,
                    export_json: None,
                    path_remappings: Vec::new(),
                    compilation_dir: None,
                    include_filename_regex: None,
                    ignore_filename_regex: None,
                    src_files: Vec::new(),
                    verbose: false,
                },
                &env.context,
                &mut machine_writer,
            )
            .await,
            Ok(())
        );
        let machine_output = test_buffers.into_stdout_str();
        assert_eq!(machine_output.trim(), "{}");
    }

    #[test]
    fn test_find_binaries_single_match() {
        let test_dir = TempDir::new().unwrap();
        create_dir(test_dir.path().join("fo")).unwrap();
        let debug_file = test_dir.path().join("fo").join("obar.debug");
        File::create(&debug_file).unwrap();

        let profraw = PathBuf::from("test.profraw");
        let profdata_cmd = ProfdataRunner::new(PathBuf::new(), VerboseMode::NotVerbose);
        let mut mocks = HashMap::new();
        mocks.insert(profraw.clone(), vec!["foobar".to_string()]);
        profdata_cmd.add_mock_binary_ids(mocks);

        assert_eq!(
            profdata_cmd
                .find_binaries(
                    &SymbolIndex {
                        build_id_dirs: vec![BuildIdDir {
                            path: test_dir.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        }],
                        includes: Vec::new(),
                        ids_txts: Vec::new(),
                        gcs_flat: Vec::new(),
                        debuginfod: Vec::new(),
                    },
                    &[profraw],
                )
                .unwrap(),
            vec![vec![debug_file]],
        )
    }

    #[test]
    fn test_find_binaries_multiple_matches() {
        let test_dir1 = TempDir::new().unwrap();
        create_dir(test_dir1.path().join("fo")).unwrap();
        let debug_file1 = test_dir1.path().join("fo").join("obar.debug");
        File::create(&debug_file1).unwrap();

        let test_dir2 = TempDir::new().unwrap();
        create_dir(test_dir2.path().join("ba")).unwrap();
        let debug_file2 = test_dir2.path().join("ba").join("rbaz.debug");
        File::create(&debug_file2).unwrap();

        let profraw1 = PathBuf::from("test1.profraw");
        let profraw2 = PathBuf::from("test2.profraw");

        let profdata_cmd = ProfdataRunner::new(PathBuf::new(), VerboseMode::NotVerbose);
        let mut mocks = HashMap::new();
        mocks.insert(profraw1.clone(), vec!["foobar".to_string()]);
        mocks.insert(profraw2.clone(), vec!["barbaz".to_string()]);
        profdata_cmd.add_mock_binary_ids(mocks);

        let results = profdata_cmd
            .find_binaries(
                &SymbolIndex {
                    build_id_dirs: vec![
                        BuildIdDir {
                            path: test_dir1.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        },
                        BuildIdDir {
                            path: test_dir2.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        },
                    ],
                    includes: Vec::new(),
                    ids_txts: Vec::new(),
                    gcs_flat: Vec::new(),
                    debuginfod: Vec::new(),
                },
                &[profraw1, profraw2],
            )
            .unwrap();

        // Use Set for comparison since par_iter order is non-deterministic
        let results_flat: HashSet<_> = results.into_iter().flatten().collect();
        let expected_flat: HashSet<_> = vec![debug_file1, debug_file2].into_iter().collect();
        assert_eq!(results_flat, expected_flat);
    }

    #[test]
    fn test_find_binaries_no_matches() {
        let test_dir = TempDir::new().unwrap();
        let profraw = PathBuf::from("test.profraw");
        let profdata_cmd = ProfdataRunner::new(PathBuf::new(), VerboseMode::NotVerbose);
        let mut mocks = HashMap::new();
        mocks.insert(profraw.clone(), vec!["foobar".to_string()]);
        profdata_cmd.add_mock_binary_ids(mocks);

        assert!(
            profdata_cmd
                .find_binaries(
                    &SymbolIndex {
                        build_id_dirs: vec![BuildIdDir {
                            path: test_dir.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        }],
                        includes: Vec::new(),
                        ids_txts: Vec::new(),
                        gcs_flat: Vec::new(),
                        debuginfod: Vec::new(),
                    },
                    &[profraw],
                )
                .is_err()
        )
    }

    #[test]
    fn test_find_binaries_show_id_err() {
        let profdata_cmd = ProfdataRunner::new(PathBuf::new(), VerboseMode::NotVerbose);
        // no mock IDs, so it will throw an error

        assert!(
            profdata_cmd
                .find_binaries(
                    &SymbolIndex {
                        build_id_dirs: Vec::new(),
                        includes: Vec::new(),
                        ids_txts: Vec::new(),
                        gcs_flat: Vec::new(),
                        debuginfod: Vec::new(),
                    },
                    &[PathBuf::new()], // profraws, actual values don't matter
                )
                .is_err()
        )
    }

    #[test]
    fn test_find_binaries_id_too_short() {
        let profraw = PathBuf::from("test.profraw");
        let profdata_cmd = ProfdataRunner::new(PathBuf::new(), VerboseMode::NotVerbose);
        let mut mocks = HashMap::new();
        mocks.insert(profraw.clone(), vec!["a".to_string()]);
        profdata_cmd.add_mock_binary_ids(mocks);

        assert!(
            profdata_cmd
                .find_binaries(
                    &SymbolIndex {
                        build_id_dirs: Vec::new(),
                        includes: Vec::new(),
                        ids_txts: Vec::new(),
                        gcs_flat: Vec::new(),
                        debuginfod: Vec::new(),
                    },
                    &[profraw],
                )
                .is_err()
        )
    }

    #[test]
    fn test_to_extra_export_args() {
        assert_eq!(to_extra_export_args(&[], None, None, None), Vec::<&str>::new());
        assert_eq!(
            to_extra_export_args(
                &["from,to".to_string(), "path1,path2".to_string()],
                None,
                None,
                None
            ),
            vec!["-path-equivalence", "from,to", "path1,path2"]
        );
        assert_eq!(
            to_extra_export_args(&[], Some(&PathBuf::from("path/to/comp/dir")), None, None),
            vec!["-compilation-dir", "path/to/comp/dir"]
        );
        assert_eq!(
            to_extra_export_args(
                &["p1,p2".to_string()],
                Some(&PathBuf::from("comp_dir")),
                Some("include_pattern"),
                Some("ignore_pattern"),
            ),
            vec![
                "-path-equivalence",
                "p1,p2",
                "-compilation-dir",
                "comp_dir",
                "--include-filename-regex",
                "include_pattern",
                "--ignore-filename-regex",
                "ignore_pattern",
            ]
        );
    }

    #[fuchsia::test]
    async fn test_export_json() {
        let _lock = SCRIPT_LOCK.lock().unwrap();
        let test_dir = TempDir::new().unwrap();
        let test_dir_path = test_dir.path().to_path_buf();
        let test_bin_dir = test_dir_path.join("bin");
        create_dir(&test_bin_dir).unwrap();

        let llvm_cov_bin = test_bin_dir.join("llvm-cov");
        File::create(&llvm_cov_bin)
            .unwrap()
            .write_all(
                b"#!/bin/sh\n\
                for arg in \"$@\"; do\n\
                  if [ \"$arg\" = \"text\" ]; then\n\
                    echo '{\"data\":[{\"files\":[{\"filename\":\"a.cc\",\"segments\":[],\"branches\":[],\"summary\":{\"lines\":{\"count\":1,\"covered\":1,\"percent\":100.0}}}],\"totals\":{\"lines\":{\"count\":1,\"covered\":1,\"percent\":100.0}}}],\"type\":\"llvm.coverage.json.export\",\"version\":\"2.0.1\"}'\n\
                    exit 0\n\
                  fi\n\
                done",
            )
            .unwrap();
        set_permissions(&llvm_cov_bin, Permissions::from_mode(0o770)).unwrap();

        let json_export_path = test_dir_path.join("test.json");
        let params = ExportParams {
            llvm_cov_bin,
            merged_profile: PathBuf::from("merged.profdata"),
            bin_files_args: vec![],
            src_files: vec![],
            extra_args: vec![],
        };

        export_json(&params, &json_export_path, VerboseMode::NotVerbose).unwrap();

        let json_content = std::fs::read_to_string(json_export_path).unwrap();
        assert_eq!(
            json_content,
            "{\"data\":[{\"files\":[{\"filename\":\"a.cc\",\"segments\":[],\"branches\":[],\"summary\":{\"lines\":{\"count\":1,\"covered\":1,\"percent\":100.0}}}],\"totals\":{\"lines\":{\"count\":1,\"covered\":1,\"percent\":100.0}}}],\"type\":\"llvm.coverage.json.export\",\"version\":\"2.0.1\"}\n"
        );
    }

    #[test]
    fn test_coverage_output_schema() {
        let empty_val = serde_json::json!({});
        VerifiedMachineWriter::<CoverageOutput>::verify_schema(&empty_val)
            .expect("empty schema matches");

        let with_paths_val = serde_json::json!({
            "json_path": "coverage.json",
            "html_path": "html_dir",
            "lcov_path": "cov.lcov",
        });
        VerifiedMachineWriter::<CoverageOutput>::verify_schema(&with_paths_val)
            .expect("paths schema matches");

        let test_buffers = ffx_writer::TestBuffers::default();
        let mut writer = VerifiedMachineWriter::<CoverageOutput>::new_test(
            Some(ffx_writer::Format::JsonPretty),
            &test_buffers,
        );
        writer.try_print_schema().expect("printed schema");
        let schema_str = test_buffers.into_stdout_str();
        assert!(!schema_str.is_empty());
    }

    #[test]
    fn test_show_binary_ids_failed() {
        let _lock = SCRIPT_LOCK.lock().unwrap();
        let test_dir = TempDir::new().unwrap();
        let test_bin = test_dir.path().join("llvm-profdata");
        std::fs::write(&test_bin, b"#!/bin/sh\necho \"error: corrupted file\" >&2\nexit 1\n")
            .unwrap();
        set_permissions(&test_bin, Permissions::from_mode(0o770)).unwrap();

        let runner = ProfdataRunner::new(test_bin, VerboseMode::NotVerbose);
        let profraw = test_dir.path().join("test.profraw");
        let result = runner.show_binary_ids(&profraw);
        assert_matches!(
            result,
            Err(CoverageError::ShowBinaryIdsFailed { code: 1, stderr }) if stderr.contains("corrupted file")
        );
    }

    #[test]
    fn test_show_coverage_failed_stderr() {
        let _lock = SCRIPT_LOCK.lock().unwrap();
        let test_dir = TempDir::new().unwrap();
        let test_bin = test_dir.path().join("llvm-cov");
        std::fs::write(&test_bin, b"#!/bin/sh\necho \"custom cov error\" >&2\nexit 2\n").unwrap();
        set_permissions(&test_bin, Permissions::from_mode(0o770)).unwrap();

        let params = ExportParams {
            llvm_cov_bin: test_bin,
            merged_profile: PathBuf::from("merged.profdata"),
            bin_files_args: vec![],
            src_files: vec![],
            extra_args: vec![],
        };
        let result = show_coverage(&params, VerboseMode::NotVerbose);
        assert_matches!(
            result,
            Err(CoverageError::ShowCoverageFailed { code: 2, stderr }) if stderr.contains("custom cov error")
        );
    }
}
