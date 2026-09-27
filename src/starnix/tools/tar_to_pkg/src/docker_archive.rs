// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Result, bail};
use itertools::Itertools;
use serde::Deserialize;
use std::fs::{File, canonicalize};
use std::io::Read;
use std::path::Path;
use tar::Archive;
use tempfile::TempDir;

/// A Docker archive loaded from a tarball created by "docker save".
///
/// Its format is described here:
/// https://github.com/docker/docker-ce/blob/master/components/engine/image/spec/v1.2.md#combined-image-json--filesystem-changeset-format
pub struct DockerArchive {
    /// Where the tarball has been extracted.
    temp_dir: TempDir,

    /// The image's config data.
    config: Config,

    /// The names of the tar files within `temp_dir` containing each layer.
    layers: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DockerArchiveArchitecture {
    Amd64,
    Arm64,
}

impl std::str::FromStr for DockerArchiveArchitecture {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "amd64" => Ok(DockerArchiveArchitecture::Amd64),
            "arm64" => Ok(DockerArchiveArchitecture::Arm64),
            other => bail!("Unsupported image architecture \"{other}\""),
        }
    }
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct ManifestEntry {
    config: String,
    layers: Vec<String>,
}

#[derive(Deserialize, Debug)]
struct Config {
    architecture: String,

    /// The `config` section is optional (and may be explicitly `null`): an image is not required
    /// to describe how it is run.
    config: Option<ConfigInner>,
}

/// The subset of the image configuration that we consume.
///
/// Every field is optional because image configs routinely omit them, and because "absent" and
/// "present but null" are both common spellings of "not set". `Option<Vec<_>>` accepts both, while
/// a defaulted `Vec<_>` would reject an explicit null.
#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct ConfigInner {
    entrypoint: Option<Vec<String>>,
    cmd: Option<Vec<String>>,
    env: Option<Vec<String>>,
}

/// Opens a `file` contained in `dir`. The file must be either a regular file or a symlink whose
/// target is also contained in the directory.
fn safe_open_in_dir(dir: &Path, file: &str) -> Result<File> {
    let canonicalized_dir = canonicalize(dir)?;
    let canonicalized_file = canonicalize(dir.join(file))?;
    if canonicalized_file.starts_with(&canonicalized_dir) {
        if canonicalized_file.is_file() {
            Ok(File::open(canonicalized_file)?)
        } else {
            bail!(
                "Blocked attempt to open {} which is not a regular file",
                canonicalized_file.display()
            );
        }
    } else {
        bail!(
            "Blocked attempt to open {} outside {}",
            canonicalized_file.display(),
            canonicalized_dir.display()
        );
    }
}

fn read_json_file<T: serde::de::DeserializeOwned>(mut file: File) -> Result<T> {
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(serde_json::from_slice::<T>(&data)?)
}

impl DockerArchive {
    /// Opens the given Docker archive.
    pub fn open(input_archive: impl Read) -> Result<DockerArchive> {
        // Extract the tarball into a temporary directory.
        let temp_dir = TempDir::new()?;
        Archive::new(input_archive).unpack(temp_dir.path())?;

        // Parse the manifest.
        let manifest_file = safe_open_in_dir(temp_dir.path(), "manifest.json")?;
        let manifest: Vec<ManifestEntry> = read_json_file(manifest_file)?;
        let Ok(Some(manifest)) = manifest.into_iter().at_most_one() else {
            bail!("Manifest should contain exactly one entry");
        };

        // Parse the config.
        let config_file = safe_open_in_dir(temp_dir.path(), &manifest.config)?;
        let config = read_json_file::<Config>(config_file)?;

        Ok(DockerArchive { temp_dir, config, layers: manifest.layers })
    }

    /// Returns an iterator that yields all the layers.
    pub fn layers(&self) -> Result<impl Iterator<Item = Archive<File>>> {
        let result: Result<Vec<_>> = self
            .layers
            .iter()
            .map(|layer| Ok(Archive::new(safe_open_in_dir(self.temp_dir.path(), layer)?)))
            .collect();
        Ok(result?.into_iter())
    }

    /// Returns the container's target architecture.
    pub fn architecture(&self) -> Result<DockerArchiveArchitecture> {
        self.config.architecture.parse()
    }

    /// Returns the defined environment variables (in "KEY=VALUE" format).
    pub fn environ(&self) -> Vec<&str> {
        self.config
            .config
            .as_ref()
            .and_then(|c| c.env.as_ref())
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect()
    }

    /// Returns the image-provided default command.
    ///
    /// Per the image spec the entrypoint and the command are concatenated: when both are present
    /// `Cmd` supplies the default arguments to `Entrypoint`, and when only one is present it is
    /// the whole command.
    pub fn default_command(&self) -> Result<Vec<&str>> {
        let entrypoint =
            self.config.config.as_ref().and_then(|c| c.entrypoint.as_ref()).into_iter().flatten();
        let cmd = self.config.config.as_ref().and_then(|c| c.cmd.as_ref()).into_iter().flatten();
        let command: Vec<&str> = entrypoint.chain(cmd).map(String::as_str).collect();

        if command.is_empty() {
            bail!("Image specifies neither Entrypoint nor Cmd, so it has no default command");
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// Appends a regular file to a tar archive under construction.
    fn append_file<W: std::io::Write>(builder: &mut tar::Builder<W>, path: &str, contents: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(contents.len() as u64);
        builder.append_data(&mut header, path, contents).unwrap();
    }

    /// Builds a minimal "docker save" tarball whose image config is `config_json`.
    ///
    /// The image has a single, empty layer, which is enough for the config-parsing tests.
    fn docker_archive(config_json: &str) -> DockerArchive {
        let empty_layer = tar::Builder::new(Vec::new()).into_inner().unwrap();

        let mut builder = tar::Builder::new(Vec::new());
        append_file(
            &mut builder,
            "manifest.json",
            br#"[{"Config":"config.json","Layers":["layer0.tar"]}]"#,
        );
        append_file(&mut builder, "config.json", config_json.as_bytes());
        append_file(&mut builder, "layer0.tar", &empty_layer);

        DockerArchive::open(Cursor::new(builder.into_inner().unwrap())).unwrap()
    }

    #[test]
    fn entrypoint_only_image_is_supported() {
        // Images built with ENTRYPOINT and no CMD are common; they used to fail to parse at all.
        let archive =
            docker_archive(r#"{"architecture":"amd64","config":{"Entrypoint":["/app"]}}"#);

        assert_eq!(archive.default_command().unwrap(), vec!["/app"]);
        assert_eq!(archive.environ(), Vec::<&str>::new());
    }

    #[test]
    fn entrypoint_and_cmd_are_concatenated() {
        let archive = docker_archive(
            r#"{"architecture":"amd64",
                "config":{"Entrypoint":["/bin/tini","--"],"Cmd":["nginx","-g","daemon off;"]}}"#,
        );

        assert_eq!(
            archive.default_command().unwrap(),
            vec!["/bin/tini", "--", "nginx", "-g", "daemon off;"]
        );
    }

    #[test]
    fn cmd_only_image_is_supported() {
        let archive = docker_archive(r#"{"architecture":"amd64","config":{"Cmd":["/bin/sh"]}}"#);

        assert_eq!(archive.default_command().unwrap(), vec!["/bin/sh"]);
    }

    #[test]
    fn null_fields_are_treated_as_absent() {
        // Image configs routinely spell "not set" as an explicit null.
        let archive = docker_archive(
            r#"{"architecture":"amd64","config":{"Entrypoint":["/app"],"Cmd":null,"Env":null}}"#,
        );

        assert_eq!(archive.default_command().unwrap(), vec!["/app"]);
        assert_eq!(archive.environ(), Vec::<&str>::new());
    }

    #[test]
    fn missing_config_section_is_tolerated() {
        for json in [r#"{"architecture":"amd64"}"#, r#"{"architecture":"amd64","config":null}"#] {
            let archive = docker_archive(json);

            assert_eq!(archive.environ(), Vec::<&str>::new());
            assert!(archive.default_command().is_err());
        }
    }

    #[test]
    fn image_without_a_command_is_rejected() {
        let archive = docker_archive(r#"{"architecture":"amd64","config":{"Env":["PATH=/bin"]}}"#);

        let error = archive.default_command().expect_err("image has no default command");
        assert!(error.to_string().contains("Entrypoint"), "unexpected error: {error}");
    }

    #[test]
    fn environment_is_returned() {
        let archive = docker_archive(
            r#"{"architecture":"amd64","config":{"Cmd":["/bin/sh"],"Env":["PATH=/bin","TZ=UTC"]}}"#,
        );

        assert_eq!(archive.environ(), vec!["PATH=/bin", "TZ=UTC"]);
    }

    #[test]
    fn known_architectures_are_recognized() {
        let amd64 = docker_archive(r#"{"architecture":"amd64","config":{}}"#);
        assert_eq!(amd64.architecture().unwrap(), DockerArchiveArchitecture::Amd64);

        let arm64 = docker_archive(r#"{"architecture":"arm64","config":{}}"#);
        assert_eq!(arm64.architecture().unwrap(), DockerArchiveArchitecture::Arm64);
    }

    #[test]
    fn unknown_architecture_does_not_prevent_parsing() {
        // An architecture we do not support should be reported where it is used, rather than
        // failing the whole archive with a deserialization error.
        let archive = docker_archive(r#"{"architecture":"riscv64","config":{"Cmd":["/bin/sh"]}}"#);

        assert_eq!(archive.default_command().unwrap(), vec!["/bin/sh"]);

        let error = archive.architecture().expect_err("riscv64 is not supported");
        assert!(error.to_string().contains("riscv64"), "unexpected error: {error}");
    }
}
