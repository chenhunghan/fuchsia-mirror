// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Streaming Android `ota.zip` construction from a Fuchsia `ProductBundle`.

use crate::AndroidConfigError;
use camino::{Utf8Path, Utf8PathBuf};
use product_bundle::ProductBundle;
use prost::Message;
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufReader, Cursor, Read as _, Write as _};
use zip::read::{ArchiveOffset, Config, ZipArchive};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// ZIP entry paths inside an Android A/B OTA package.
const ENTRY_METADATA_LEGACY: &str = "META-INF/com/android/metadata";
const ENTRY_METADATA_PROTO: &str = "META-INF/com/android/metadata.pb";
const ENTRY_OTACERT: &str = "META-INF/com/android/otacert";
const ENTRY_PAYLOAD_BIN: &str = "payload.bin";
const ENTRY_PAYLOAD_PROPERTIES: &str = "payload_properties.txt";

/// Property-files map keys in `OtaMetadata.property_files` and legacy `metadata`.
const KEY_OTA_PROPERTY_FILES: &str = "ota-property-files";
const KEY_OTA_STREAMING_PROPERTY_FILES: &str = "ota-streaming-property-files";

/// Placeholder `payload_properties.txt` contents used for Fuchsia streaming OTA
/// packages. Fuchsia's `payload.bin` is an OTA manifest rather than an
/// `update_engine` payload, so no real payload properties apply.
const DUMMY_PAYLOAD_PROPERTIES: &[u8] = b"dummy_key=dummy_value";

/// Width of space placeholder reserved after `<entry_name>:` during Pass 1 so
/// `<offset>:<size>` always fits in Pass 2 without changing `metadata(.pb)` size.
const PROPERTY_ENTRY_PLACEHOLDER_WIDTH: usize = 25;

/// Length in bytes of a ZIP End-of-Central-Directory record without a comment.
const ZIP_EOCD_LEN: usize = 22;

/// Little-endian ZIP End-of-Central-Directory magic (`PK\x05\x06`).
const ZIP_EOCD_MAGIC: &[u8; 4] = b"PK\x05\x06";

/// Separator used for multi-valued fields in the legacy `metadata` file.
const LEGACY_METADATA_SEPARATOR: &str = "|";

/// Android build properties used to derive OTA metadata, in the same way as
/// `GetPackageMetadata` in AOSP `releasetools/ota_utils.py`.
const PROP_BUILD_DATE_UTC: &str = "ro.build.date.utc";
const PROP_BUILD_FINGERPRINT: &str = "ro.build.fingerprint";
const PROP_BUILD_INCREMENTAL: &str = "ro.build.version.incremental";
const PROP_BUILD_SDK: &str = "ro.build.version.sdk";
const PROP_BUILD_SECURITY_PATCH: &str = "ro.build.version.security_patch";
const PROP_PRODUCT_DEVICE: &str = "ro.product.device";

/// Partitions whose `ro.product.<partition>.device` and
/// `ro.<partition>.build.fingerprint` are consulted, in order, when the generic
/// property is absent (matching AOSP `BuildInfo`'s `ro.product.*` resolution).
const PROP_PARTITION_ORDER: &[&str] = &["product", "odm", "vendor", "system_ext", "system"];

/// Protobuf representation of `build.tools.releasetools.PartitionState` from
/// AOSP `build/make/tools/releasetools/ota_metadata.proto`.
#[derive(Clone, PartialEq, Message)]
struct PartitionState {
    #[prost(string, tag = "1")]
    partition_name: String,
    #[prost(string, repeated, tag = "2")]
    device: Vec<String>,
    #[prost(string, repeated, tag = "3")]
    build: Vec<String>,
    #[prost(string, tag = "4")]
    version: String,
}

/// Protobuf representation of `build.tools.releasetools.DeviceState` from
/// AOSP `build/make/tools/releasetools/ota_metadata.proto`.
#[derive(Clone, PartialEq, Message)]
struct DeviceState {
    #[prost(string, repeated, tag = "1")]
    device: Vec<String>,
    #[prost(string, repeated, tag = "2")]
    build: Vec<String>,
    #[prost(string, tag = "3")]
    build_incremental: String,
    #[prost(int64, tag = "4")]
    timestamp: i64,
    #[prost(string, tag = "5")]
    sdk_level: String,
    #[prost(string, tag = "6")]
    security_patch_level: String,
    #[prost(message, repeated, tag = "7")]
    partition_state: Vec<PartitionState>,
}

/// `build.tools.releasetools.OtaMetadata.OtaType.AB`.
const OTA_TYPE_AB: i32 = 1;

/// Protobuf representation of `build.tools.releasetools.OtaMetadata` from
/// AOSP `build/make/tools/releasetools/ota_metadata.proto`. Field 7
/// (`retrofit_dynamic_partitions`) is obsolete and not used.
#[derive(Clone, PartialEq, Message)]
struct OtaMetadata {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(bool, tag = "2")]
    wipe: bool,
    #[prost(bool, tag = "3")]
    downgrade: bool,
    // A BTreeMap keeps the encoding of `metadata.pb` deterministic.
    #[prost(btree_map = "string, string", tag = "4")]
    property_files: BTreeMap<String, String>,
    #[prost(message, optional, tag = "5")]
    precondition: Option<DeviceState>,
    #[prost(message, optional, tag = "6")]
    postcondition: Option<DeviceState>,
    #[prost(int64, tag = "8")]
    required_cache: i64,
    #[prost(bool, tag = "9")]
    spl_downgrade: bool,
}

/// Where to get the Android device and build information recorded in the OTA
/// metadata (`pre-device`, `post-build`, `post-timestamp`, ...).
#[derive(Clone, Copy, Debug)]
pub enum OtaMetadataSource<'a> {
    /// Copy the device pre/postconditions from an existing Android OTA package
    /// for the same Android build.
    BaseOta(&'a Utf8Path),
    /// Derive the metadata from the Android build's `build.prop` files (for
    /// example `SYSTEM/build.prop` and `VENDOR/build.prop` from target files).
    /// Later files take precedence over earlier ones.
    BuildProps(&'a [Utf8PathBuf]),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ZipEntrySpan {
    offset: u64,
    size: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ArchiveEntrySpans {
    metadata_legacy: ZipEntrySpan,
    metadata_proto: ZipEntrySpan,
    payload_bin: ZipEntrySpan,
    payload_properties: ZipEntrySpan,
}

/// Create an unsigned streaming Android `ota.zip` at `out_ota_zip` from `product_bundle`.
///
/// The device pre/postcondition metadata comes from `metadata_source`. If
/// `otacert` is provided, it is stored as `META-INF/com/android/otacert`.
///
/// The archive ends with an empty ZIP comment, so an AOSP `SignApk`-style
/// whole-file signature can later be appended in the ZIP comment without
/// changing any of the entry offsets recorded in the OTA metadata.
pub fn create_ota_zip(
    product_bundle: &ProductBundle,
    out_ota_zip: impl AsRef<Utf8Path>,
    metadata_source: OtaMetadataSource<'_>,
    otacert: Option<&[u8]>,
) -> Result<(), AndroidConfigError> {
    let out_ota_zip = out_ota_zip.as_ref();
    let ProductBundle::V2(pb) = product_bundle;

    let ota_manifest_path = pb
        .repositories
        .iter()
        .find_map(|repo| repo.ota_manifest_path.as_ref())
        .ok_or(AndroidConfigError::MissingOtaManifest)?;

    let payload_bytes = std::fs::read(ota_manifest_path).map_err(|source| {
        AndroidConfigError::ReadFile { path: ota_manifest_path.clone(), source }
    })?;

    let mut metadata = match metadata_source {
        OtaMetadataSource::BaseOta(base_path) => load_base_ota_metadata(base_path)?,
        OtaMetadataSource::BuildProps(paths) => build_props_ota_metadata(paths)?,
    };
    metadata.r#type = OTA_TYPE_AB;
    metadata.required_cache = 0;

    // Pass 1: reserve fixed-width space for all property-file entries so the
    // serialized sizes of `metadata` and `metadata.pb` remain identical in Pass 2.
    let initial_ota_props = placeholder_property_files_string(&[
        "payload_metadata.bin",
        "payload.bin",
        "payload_properties.txt",
        "metadata",
        "metadata.pb",
    ]);
    let initial_streaming_props = placeholder_property_files_string(&[
        "payload.bin",
        "payload_properties.txt",
        "metadata",
        "metadata.pb",
    ]);
    metadata.property_files.clear();
    metadata.property_files.insert(KEY_OTA_PROPERTY_FILES.to_string(), initial_ota_props.clone());
    metadata
        .property_files
        .insert(KEY_OTA_STREAMING_PROPERTY_FILES.to_string(), initial_streaming_props.clone());

    let pass1_legacy_bytes = format_legacy_metadata(&metadata).into_bytes();
    let pass1_proto_bytes = metadata.encode_to_vec();
    let pass1_zip =
        build_unsigned_ota_zip(&pass1_legacy_bytes, &pass1_proto_bytes, &payload_bytes, otacert)?;
    let pass1_spans = read_archive_entry_spans(&pass1_zip)?;

    // Pass 2: compute exact `<name>:<offset>:<size>` strings and pad to the
    // exact byte length used in Pass 1.
    let final_ota_props = pad_property_files_string(
        &format_ota_property_files(&pass1_spans),
        initial_ota_props.len(),
    )?;
    let final_streaming_props = pad_property_files_string(
        &format_streaming_property_files(&pass1_spans),
        initial_streaming_props.len(),
    )?;
    metadata.property_files.insert(KEY_OTA_PROPERTY_FILES.to_string(), final_ota_props);
    metadata
        .property_files
        .insert(KEY_OTA_STREAMING_PROPERTY_FILES.to_string(), final_streaming_props);

    let pass2_legacy_bytes = format_legacy_metadata(&metadata).into_bytes();
    let pass2_proto_bytes = metadata.encode_to_vec();
    if pass2_legacy_bytes.len() != pass1_legacy_bytes.len()
        || pass2_proto_bytes.len() != pass1_proto_bytes.len()
    {
        return Err(AndroidConfigError::InvalidPropertyFilesLayout(format!(
            "metadata sizes changed between pass 1 ({}/{}) and pass 2 ({}/{})",
            pass1_legacy_bytes.len(),
            pass1_proto_bytes.len(),
            pass2_legacy_bytes.len(),
            pass2_proto_bytes.len()
        )));
    }

    let pass2_zip =
        build_unsigned_ota_zip(&pass2_legacy_bytes, &pass2_proto_bytes, &payload_bytes, otacert)?;
    let pass2_spans = read_archive_entry_spans(&pass2_zip)?;
    if pass2_spans != pass1_spans {
        return Err(AndroidConfigError::InvalidPropertyFilesLayout(
            "ZIP entry offsets or sizes shifted between pass 1 and pass 2".to_string(),
        ));
    }

    verify_signable_zip(&pass2_zip)?;
    if let Some(parent) = out_ota_zip.parent()
        && !parent.as_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| AndroidConfigError::CreateDir {
            path: parent.to_path_buf(),
            source,
        })?;
    }
    std::fs::write(out_ota_zip, pass2_zip).map_err(|source| AndroidConfigError::WriteFile {
        path: out_ota_zip.to_path_buf(),
        source,
    })?;
    Ok(())
}

/// Build OTA metadata for a full A/B update from Android `build.prop` files,
/// following `GetPackageMetadata` / `UpdateDeviceState` in AOSP
/// `releasetools/ota_utils.py`.
fn build_props_ota_metadata(paths: &[Utf8PathBuf]) -> Result<OtaMetadata, AndroidConfigError> {
    let mut props = HashMap::new();
    for path in paths {
        let text = std::fs::read_to_string(path)
            .map_err(|source| AndroidConfigError::ReadFile { path: path.clone(), source })?;
        props.extend(parse_build_prop(&text));
    }

    let required = |key: &str| {
        props
            .get(key)
            .filter(|value| !value.is_empty())
            .cloned()
            .ok_or_else(|| AndroidConfigError::MissingBuildProp { key: key.to_string() })
    };
    let resolve = |key: &str, partition_key: fn(&str) -> String| {
        std::iter::once(key.to_string())
            .chain(PROP_PARTITION_ORDER.iter().map(|partition| partition_key(partition)))
            .find_map(|key| props.get(&key).filter(|value| !value.is_empty()).cloned())
            .ok_or_else(|| AndroidConfigError::MissingBuildProp { key: key.to_string() })
    };

    let device = resolve(PROP_PRODUCT_DEVICE, |p| format!("ro.product.{p}.device"))?;
    let fingerprint = resolve(PROP_BUILD_FINGERPRINT, |p| format!("ro.{p}.build.fingerprint"))?;
    let timestamp_str = required(PROP_BUILD_DATE_UTC)?;
    let timestamp =
        timestamp_str.parse::<i64>().map_err(|_| AndroidConfigError::InvalidBuildProp {
            key: PROP_BUILD_DATE_UTC.to_string(),
            value: timestamp_str.clone(),
        })?;

    Ok(OtaMetadata {
        r#type: OTA_TYPE_AB,
        precondition: Some(DeviceState { device: vec![device.clone()], ..Default::default() }),
        postcondition: Some(DeviceState {
            device: vec![device],
            build: vec![fingerprint],
            build_incremental: required(PROP_BUILD_INCREMENTAL)?,
            timestamp,
            sdk_level: required(PROP_BUILD_SDK)?,
            security_patch_level: required(PROP_BUILD_SECURITY_PATCH)?,
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// Parse a `build.prop` file into key/value pairs, ignoring comments, blank
/// lines, and `import` directives.
fn parse_build_prop(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines().filter_map(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let (key, value) = line.split_once('=')?;
        Some((key.trim().to_string(), value.trim().to_string()))
    })
}

fn load_base_ota_metadata(base_ota_zip: &Utf8Path) -> Result<OtaMetadata, AndroidConfigError> {
    let file = File::open(base_ota_zip).map_err(|source| AndroidConfigError::ReadFile {
        path: base_ota_zip.to_path_buf(),
        source,
    })?;
    // OTA packages never contain prepended data, so skip the forward scan
    // for the archive offset and go straight to the End-of-Central-Directory.
    let mut archive = ZipArchive::with_config(
        Config { archive_offset: ArchiveOffset::Known(0) },
        BufReader::new(file),
    )?;

    if let Ok(mut proto_entry) = archive.by_name(ENTRY_METADATA_PROTO) {
        let mut buf = Vec::new();
        proto_entry.read_to_end(&mut buf).map_err(|source| AndroidConfigError::ReadZipEntry {
            entry: ENTRY_METADATA_PROTO.to_string(),
            source,
        })?;
        return Ok(OtaMetadata::decode(buf.as_slice())?);
    }

    if let Ok(mut legacy_entry) = archive.by_name(ENTRY_METADATA_LEGACY) {
        let mut text = String::new();
        legacy_entry.read_to_string(&mut text).map_err(|source| {
            AndroidConfigError::ReadZipEntry { entry: ENTRY_METADATA_LEGACY.to_string(), source }
        })?;
        return Ok(parse_legacy_metadata(&text));
    }

    Err(AndroidConfigError::MissingBaseOtaMetadata { path: base_ota_zip.to_path_buf() })
}

fn parse_legacy_metadata(text: &str) -> OtaMetadata {
    let mut pre = DeviceState::default();
    let mut post = DeviceState::default();
    let mut wipe = false;
    let mut downgrade = false;
    let mut spl_downgrade = false;

    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "ota-wipe" => wipe = value == "yes" || value == "true",
            "ota-downgrade" => downgrade = value == "yes" || value == "true",
            "spl-downgrade" | "ota-spl-downgrade" => {
                spl_downgrade = value == "yes" || value == "true"
            }
            "pre-device" if !value.is_empty() => {
                pre.device = value.split('|').map(str::to_string).collect();
            }
            "pre-build" if !value.is_empty() => {
                pre.build = value.split('|').map(str::to_string).collect();
            }
            "pre-build-incremental" => pre.build_incremental = value.to_string(),
            "post-device" if !value.is_empty() => {
                post.device = value.split('|').map(str::to_string).collect();
            }
            "post-build" if !value.is_empty() => {
                post.build = value.split('|').map(str::to_string).collect();
            }
            "post-build-incremental" => post.build_incremental = value.to_string(),
            "post-timestamp" => post.timestamp = value.parse::<i64>().unwrap_or(0),
            "post-sdk-level" => post.sdk_level = value.to_string(),
            "post-security-patch-level" => post.security_patch_level = value.to_string(),
            _ => {}
        }
    }
    if post.device.is_empty() && !pre.device.is_empty() {
        post.device = pre.device.clone();
    }

    OtaMetadata {
        r#type: OTA_TYPE_AB,
        wipe,
        downgrade,
        precondition: Some(pre),
        postcondition: Some(post),
        spl_downgrade,
        ..Default::default()
    }
}

/// Format the legacy `META-INF/com/android/metadata` key=value text, matching
/// `BuildLegacyOtaMetadata` and `WriteMetadata` in AOSP
/// `releasetools/ota_utils.py`.
fn format_legacy_metadata(metadata: &OtaMetadata) -> String {
    let default_state = DeviceState::default();
    let post = metadata.postcondition.as_ref().unwrap_or(&default_state);
    let pre = metadata.precondition.as_ref().unwrap_or(&default_state);

    let mut pairs: BTreeMap<String, String> = BTreeMap::new();
    pairs.insert("ota-type".to_string(), "AB".to_string());
    if metadata.wipe {
        pairs.insert("ota-wipe".to_string(), "yes".to_string());
    }
    if metadata.downgrade {
        pairs.insert("ota-downgrade".to_string(), "yes".to_string());
    }
    pairs.insert("ota-required-cache".to_string(), metadata.required_cache.to_string());

    pairs.insert("post-build".to_string(), post.build.join(LEGACY_METADATA_SEPARATOR));
    pairs.insert("post-build-incremental".to_string(), post.build_incremental.clone());
    pairs.insert("post-sdk-level".to_string(), post.sdk_level.clone());
    pairs.insert("post-security-patch-level".to_string(), post.security_patch_level.clone());
    pairs.insert("post-timestamp".to_string(), post.timestamp.to_string());

    pairs.insert("pre-device".to_string(), pre.device.join(LEGACY_METADATA_SEPARATOR));
    if !pre.build.is_empty() {
        pairs.insert("pre-build".to_string(), pre.build.join(LEGACY_METADATA_SEPARATOR));
        pairs.insert("pre-build-incremental".to_string(), pre.build_incremental.clone());
    }

    if metadata.spl_downgrade {
        pairs.insert("spl-downgrade".to_string(), "yes".to_string());
    }
    for (k, v) in &metadata.property_files {
        pairs.insert(k.clone(), v.clone());
    }

    let mut out = String::new();
    for (k, v) in pairs {
        out.push_str(&k);
        out.push('=');
        out.push_str(&v);
        out.push('\n');
    }
    out
}

fn placeholder_property_files_string(names: &[&str]) -> String {
    let placeholder = " ".repeat(PROPERTY_ENTRY_PLACEHOLDER_WIDTH);
    names.iter().map(|name| format!("{name}:{placeholder}")).collect::<Vec<_>>().join(",")
}

fn format_ota_property_files(spans: &ArchiveEntrySpans) -> String {
    format!(
        "payload_metadata.bin:{}:0,payload.bin:{}:{},payload_properties.txt:{}:{},metadata:{}:{},metadata.pb:{}:{}",
        spans.payload_bin.offset,
        spans.payload_bin.offset,
        spans.payload_bin.size,
        spans.payload_properties.offset,
        spans.payload_properties.size,
        spans.metadata_legacy.offset,
        spans.metadata_legacy.size,
        spans.metadata_proto.offset,
        spans.metadata_proto.size,
    )
}

fn format_streaming_property_files(spans: &ArchiveEntrySpans) -> String {
    format!(
        "payload.bin:{}:{},payload_properties.txt:{}:{},metadata:{}:{},metadata.pb:{}:{}",
        spans.payload_bin.offset,
        spans.payload_bin.size,
        spans.payload_properties.offset,
        spans.payload_properties.size,
        spans.metadata_legacy.offset,
        spans.metadata_legacy.size,
        spans.metadata_proto.offset,
        spans.metadata_proto.size,
    )
}

fn pad_property_files_string(
    finalized: &str,
    reserved_len: usize,
) -> Result<String, AndroidConfigError> {
    let pad_len = reserved_len.checked_sub(finalized.len()).ok_or_else(|| {
        AndroidConfigError::InvalidPropertyFilesLayout(format!(
            "finalized property-files string ({}) exceeds reserved length ({reserved_len})",
            finalized.len()
        ))
    })?;
    let mut padded = String::with_capacity(reserved_len);
    padded.push_str(finalized);
    padded.push_str(&" ".repeat(pad_len));
    Ok(padded)
}

fn build_unsigned_ota_zip(
    legacy_metadata_bytes: &[u8],
    proto_metadata_bytes: &[u8],
    payload_bytes: &[u8],
    otacert_bytes: Option<&[u8]>,
) -> Result<Vec<u8>, AndroidConfigError> {
    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = ZipWriter::new(&mut cursor);
        let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

        write_zip_entry(&mut writer, ENTRY_METADATA_LEGACY, legacy_metadata_bytes, stored)?;
        write_zip_entry(&mut writer, ENTRY_METADATA_PROTO, proto_metadata_bytes, stored)?;
        write_zip_entry(&mut writer, ENTRY_PAYLOAD_BIN, payload_bytes, stored)?;
        write_zip_entry(&mut writer, ENTRY_PAYLOAD_PROPERTIES, DUMMY_PAYLOAD_PROPERTIES, stored)?;
        if let Some(otacert_bytes) = otacert_bytes {
            write_zip_entry(&mut writer, ENTRY_OTACERT, otacert_bytes, stored)?;
        }
        writer.finish()?;
    }
    Ok(cursor.into_inner())
}

/// Check that `zip_bytes` ends with a comment-less EOCD record, as required for
/// appending a whole-file signature.
///
/// Like ZIP readers and AOSP's whole-file OTA verifier, which scan backwards
/// from the end of the file, only the EOCD record itself must not contain
/// another EOCD magic; the archive contents may.
fn verify_signable_zip(zip_bytes: &[u8]) -> Result<(), AndroidConfigError> {
    let eocd_offset = zip_bytes.len().checked_sub(ZIP_EOCD_LEN).ok_or_else(|| {
        AndroidConfigError::InvalidPropertyFilesLayout(
            "archive smaller than an EOCD record".to_string(),
        )
    })?;
    let eocd = &zip_bytes[eocd_offset..];
    if &eocd[..4] != ZIP_EOCD_MAGIC || eocd[20..22] != [0, 0] {
        return Err(AndroidConfigError::InvalidPropertyFilesLayout(
            "archive does not end with a comment-less EOCD record".to_string(),
        ));
    }
    if eocd[ZIP_EOCD_MAGIC.len()..].windows(ZIP_EOCD_MAGIC.len()).any(|w| w == ZIP_EOCD_MAGIC) {
        return Err(AndroidConfigError::InvalidPropertyFilesLayout(
            "EOCD record contains another EOCD magic sequence".to_string(),
        ));
    }
    Ok(())
}

fn write_zip_entry<W: std::io::Write + std::io::Seek>(
    writer: &mut ZipWriter<W>,
    name: &str,
    contents: &[u8],
    options: SimpleFileOptions,
) -> Result<(), AndroidConfigError> {
    writer.start_file(name, options)?;
    writer
        .write_all(contents)
        .map_err(|source| AndroidConfigError::WriteZipEntry { entry: name.to_string(), source })?;
    Ok(())
}

fn read_archive_entry_spans(zip_bytes: &[u8]) -> Result<ArchiveEntrySpans, AndroidConfigError> {
    let mut archive = ZipArchive::new(Cursor::new(zip_bytes))?;
    Ok(ArchiveEntrySpans {
        metadata_legacy: entry_span(&mut archive, ENTRY_METADATA_LEGACY)?,
        metadata_proto: entry_span(&mut archive, ENTRY_METADATA_PROTO)?,
        payload_bin: entry_span(&mut archive, ENTRY_PAYLOAD_BIN)?,
        payload_properties: entry_span(&mut archive, ENTRY_PAYLOAD_PROPERTIES)?,
    })
}

fn entry_span<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<ZipEntrySpan, AndroidConfigError> {
    let entry = archive.by_name(name)?;
    let offset = entry.data_start().ok_or_else(|| {
        AndroidConfigError::InvalidPropertyFilesLayout(format!(
            "missing data_start offset for ZIP entry {name}"
        ))
    })?;
    Ok(ZipEntrySpan { offset, size: entry.size() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assembly_partitions_config::PartitionsConfig;
    use product_bundle::{ProductBundleV2, Repository};
    use tempfile::TempDir;

    const TEST_FINGERPRINT: &str =
        "vendor/test_product/test_board:16/MAIN/14227767:userdebug/dev-keys";

    /// Write `system` and `vendor` build.prop files in the layout of Android
    /// target files, with the device name only in the vendor partition.
    fn write_build_props(dir: &Utf8Path) -> Vec<Utf8PathBuf> {
        let system = dir.join("SYSTEM/build.prop");
        let vendor = dir.join("VENDOR/build.prop");
        std::fs::create_dir_all(system.parent().unwrap()).unwrap();
        std::fs::create_dir_all(vendor.parent().unwrap()).unwrap();
        std::fs::write(
            &system,
            format!(
                "# begin build properties\n\
                 import /system/etc/prop.default\n\
                 ro.build.fingerprint={TEST_FINGERPRINT}\n\
                 ro.build.version.incremental=14227767\n\
                 ro.build.version.sdk=36\n\
                 ro.build.version.security_patch=2025-11-05\n\
                 ro.build.date.utc=1759863069\n\
                 ro.product.system.device=generic\n"
            ),
        )
        .unwrap();
        std::fs::write(&vendor, "ro.product.vendor.device=test_board\n").unwrap();
        vec![system, vendor]
    }

    fn make_product_bundle_with_ota_manifest(
        dir: &Utf8Path,
        ota_manifest_contents: Option<&[u8]>,
    ) -> ProductBundle {
        let ota_manifest_path = ota_manifest_contents.map(|bytes| {
            let p = dir.join("repository/ota_manifest");
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, bytes).unwrap();
            p
        });

        ProductBundle::V2(ProductBundleV2 {
            product_name: "test_product.test_board".to_string(),
            product_version: "99.20261102.1.1".to_string(),
            partitions: PartitionsConfig {
                hardware_revision: "test_board".to_string(),
                ..Default::default()
            },
            sdk_version: "sdk".to_string(),
            system_a: None,
            platform_tools_a: vec![],
            system_b: None,
            platform_tools_b: vec![],
            system_r: None,
            platform_tools_r: vec![],
            release_info: None,
            repositories: vec![Repository {
                name: "fuchsia.com".into(),
                metadata_path: dir.join("repository"),
                blobs_path: dir.join("blobs"),
                delivery_blob_type: 1,
                root_private_key_path: None,
                targets_private_key_path: None,
                snapshot_private_key_path: None,
                timestamp_private_key_path: None,
                ota_manifest_signature_path: None,
                ota_manifest_path,
            }],
            update_package_hash: None,
            virtual_devices_path: None,
        })
    }

    fn verify_property_file_slices(zip_bytes: &[u8], prop_line: &str) -> BTreeMap<String, Vec<u8>> {
        let mut slices = BTreeMap::new();
        for token in prop_line.split(',') {
            let trimmed = token.trim();
            let parts: Vec<&str> = trimmed.split(':').collect();
            assert_eq!(parts.len(), 3, "malformed token: {trimmed}");
            let name = parts[0].to_string();
            let offset: usize = parts[1].parse().unwrap();
            let size: usize = parts[2].parse().unwrap();
            slices.insert(name, zip_bytes[offset..offset + size].to_vec());
        }
        slices
    }

    #[test]
    fn create_streaming_ota_zip_from_build_props_with_otacert() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let manifest_bytes = br#"{"version":1,"blobs":[{"merkle":"abc123"}]}"#;
        let pb = make_product_bundle_with_ota_manifest(dir, Some(manifest_bytes));

        let build_props = write_build_props(dir);

        let ota_zip_path = dir.join("out/ota.zip");
        let otacert = b"-----BEGIN CERTIFICATE-----\ntest\n-----END CERTIFICATE-----\n";
        create_ota_zip(
            &pb,
            &ota_zip_path,
            OtaMetadataSource::BuildProps(&build_props),
            Some(otacert),
        )
        .unwrap();

        let ota_bytes = std::fs::read(&ota_zip_path).unwrap();
        verify_signable_zip(&ota_bytes).unwrap();

        let mut archive = ZipArchive::new(Cursor::new(&ota_bytes)).unwrap();
        for i in 0..archive.len() {
            let entry = archive.by_index(i).unwrap();
            assert_eq!(
                entry.compression(),
                CompressionMethod::Stored,
                "entry {} must be ZIP_STORED",
                entry.name().unwrap()
            );
        }

        let mut otacert_buf = Vec::new();
        archive.by_name(ENTRY_OTACERT).unwrap().read_to_end(&mut otacert_buf).unwrap();
        assert_eq!(otacert_buf, otacert);

        let mut proto_buf = Vec::new();
        archive.by_name(ENTRY_METADATA_PROTO).unwrap().read_to_end(&mut proto_buf).unwrap();
        let decoded = OtaMetadata::decode(proto_buf.as_slice()).unwrap();
        assert_eq!(decoded.r#type, OTA_TYPE_AB);
        assert_eq!(decoded.required_cache, 0);
        assert_eq!(decoded.precondition.as_ref().unwrap().device, vec!["test_board".to_string()]);
        pretty_assertions::assert_eq!(
            decoded.postcondition,
            Some(DeviceState {
                device: vec!["test_board".to_string()],
                build: vec![TEST_FINGERPRINT.to_string()],
                build_incremental: "14227767".to_string(),
                timestamp: 1_759_863_069,
                sdk_level: "36".to_string(),
                security_patch_level: "2025-11-05".to_string(),
                ..Default::default()
            })
        );
        // The protobuf encoding must be deterministic.
        assert_eq!(proto_buf, decoded.encode_to_vec());

        let mut legacy_str = String::new();
        archive.by_name(ENTRY_METADATA_LEGACY).unwrap().read_to_string(&mut legacy_str).unwrap();
        let legacy_keys: Vec<&str> =
            legacy_str.lines().map(|line| line.split_once('=').unwrap().0).collect();
        pretty_assertions::assert_eq!(
            legacy_keys,
            vec![
                "ota-property-files",
                "ota-required-cache",
                "ota-streaming-property-files",
                "ota-type",
                "post-build",
                "post-build-incremental",
                "post-sdk-level",
                "post-security-patch-level",
                "post-timestamp",
                "pre-device",
            ]
        );

        let streaming_slices = verify_property_file_slices(
            &ota_bytes,
            &decoded.property_files[KEY_OTA_STREAMING_PROPERTY_FILES],
        );
        assert_eq!(streaming_slices["payload.bin"], manifest_bytes);
        assert_eq!(streaming_slices["payload_properties.txt"], DUMMY_PAYLOAD_PROPERTIES);
        assert_eq!(streaming_slices["metadata.pb"], proto_buf);

        let ota_slices = verify_property_file_slices(
            &ota_bytes,
            &decoded.property_files[KEY_OTA_PROPERTY_FILES],
        );
        assert!(ota_slices["payload_metadata.bin"].is_empty());
        assert_eq!(ota_slices["payload.bin"], manifest_bytes);
    }

    #[test]
    fn create_ota_zip_patches_base_ota_metadata() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();

        // Build a synthetic base Android ota.zip with rich Android build properties.
        let base_meta = OtaMetadata {
            r#type: OTA_TYPE_AB,
            precondition: Some(DeviceState {
                device: vec!["test_board".to_string()],
                ..Default::default()
            }),
            postcondition: Some(DeviceState {
                device: vec!["test_board".to_string()],
                build: vec![TEST_FINGERPRINT.to_string()],
                build_incremental: "14227767".to_string(),
                timestamp: 1_759_863_069,
                sdk_level: "36".to_string(),
                security_patch_level: "2025-11-05".to_string(),
                ..Default::default()
            }),
            required_cache: 4096,
            spl_downgrade: true,
            ..Default::default()
        };
        let base_ota_path: Utf8PathBuf = dir.join("base-ota.zip");
        {
            let f = File::create(&base_ota_path).unwrap();
            let mut zw = ZipWriter::new(f);
            let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zw.start_file(ENTRY_METADATA_PROTO, opts).unwrap();
            zw.write_all(&base_meta.encode_to_vec()).unwrap();
            zw.finish().unwrap();
        }

        let manifest_bytes = b"fuchsia-ota-manifest-v1-payload";
        let pb = make_product_bundle_with_ota_manifest(dir, Some(manifest_bytes));
        let out_ota_path = dir.join("patched-ota.zip");
        create_ota_zip(&pb, &out_ota_path, OtaMetadataSource::BaseOta(&base_ota_path), None)
            .unwrap();

        let ota_bytes = std::fs::read(&out_ota_path).unwrap();
        verify_signable_zip(&ota_bytes).unwrap();

        let mut archive = ZipArchive::new(Cursor::new(&ota_bytes)).unwrap();
        assert!(archive.by_name(ENTRY_OTACERT).is_err(), "otacert must be omitted when not given");
        let mut legacy_str = String::new();
        archive.by_name(ENTRY_METADATA_LEGACY).unwrap().read_to_string(&mut legacy_str).unwrap();
        assert!(legacy_str.contains(
            "post-build=vendor/test_product/test_board:16/MAIN/14227767:userdebug/dev-keys\n"
        ));
        assert!(legacy_str.contains("post-build-incremental=14227767\n"));
        assert!(legacy_str.contains("post-sdk-level=36\n"));
        assert!(legacy_str.contains("post-security-patch-level=2025-11-05\n"));
        assert!(legacy_str.contains("post-timestamp=1759863069\n"));
        assert!(legacy_str.contains("pre-device=test_board\n"));
        assert!(legacy_str.contains("ota-required-cache=0\n"));
        assert!(legacy_str.contains("spl-downgrade=yes\n"));
        assert!(!legacy_str.contains("ota-spl-downgrade"));

        let mut proto_buf = Vec::new();
        archive.by_name(ENTRY_METADATA_PROTO).unwrap().read_to_end(&mut proto_buf).unwrap();
        let decoded = OtaMetadata::decode(proto_buf.as_slice()).unwrap();
        let streaming_slices = verify_property_file_slices(
            &ota_bytes,
            &decoded.property_files[KEY_OTA_STREAMING_PROPERTY_FILES],
        );
        assert_eq!(streaming_slices["metadata"], legacy_str.as_bytes());
        assert_eq!(streaming_slices["payload.bin"], manifest_bytes);
    }

    #[test]
    fn create_ota_zip_errors_when_ota_manifest_missing() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let pb = make_product_bundle_with_ota_manifest(dir, None);
        let build_props = write_build_props(dir);
        let err = create_ota_zip(
            &pb,
            dir.join("ota.zip"),
            OtaMetadataSource::BuildProps(&build_props),
            None,
        )
        .unwrap_err();
        assert!(matches!(err, AndroidConfigError::MissingOtaManifest));
    }

    #[test]
    fn build_props_metadata_requires_timestamp() {
        let tmp = TempDir::new().unwrap();
        let dir = Utf8Path::from_path(tmp.path()).unwrap();
        let props = dir.join("build.prop");
        let without_timestamp = format!(
            "ro.product.device=test_board\n\
             ro.build.fingerprint={TEST_FINGERPRINT}\n\
             ro.build.version.incremental=1\n\
             ro.build.version.sdk=36\n\
             ro.build.version.security_patch=2025-11-05\n"
        );
        std::fs::write(&props, &without_timestamp).unwrap();
        let err = build_props_ota_metadata(&[props.clone()]).unwrap_err();
        assert!(
            matches!(&err, AndroidConfigError::MissingBuildProp { key } if key == PROP_BUILD_DATE_UTC),
            "{err:?}"
        );

        std::fs::write(&props, format!("{without_timestamp}ro.build.date.utc=soon\n")).unwrap();
        let err = build_props_ota_metadata(&[props]).unwrap_err();
        assert!(
            matches!(&err, AndroidConfigError::InvalidBuildProp { key, .. } if key == PROP_BUILD_DATE_UTC),
            "{err:?}"
        );
    }

    #[test]
    fn ota_metadata_uses_aosp_field_numbers() {
        // `required_cache` is field 8 and `spl_downgrade` is field 9 in AOSP's
        // ota_metadata.proto; field 7 is the obsolete `retrofit_dynamic_partitions`.
        let metadata = OtaMetadata { required_cache: 1, spl_downgrade: true, ..Default::default() };
        assert_eq!(metadata.encode_to_vec(), [0x40, 0x01, 0x48, 0x01]);
    }

    #[test]
    fn parse_legacy_metadata_accepts_both_spl_downgrade_keys() {
        assert!(parse_legacy_metadata("spl-downgrade=yes\n").spl_downgrade);
        assert!(parse_legacy_metadata("ota-spl-downgrade=yes\n").spl_downgrade);
        assert!(!parse_legacy_metadata("pre-device=test_board\n").spl_downgrade);
    }

    #[test]
    fn verify_signable_zip_allows_eocd_magic_in_contents() {
        // An entry whose contents contain the EOCD magic, like a payload digest
        // or signature might, must not be rejected.
        let payload = [b"before".as_slice(), ZIP_EOCD_MAGIC, b"after"].concat();
        let zip = build_unsigned_ota_zip(b"legacy", b"proto", &payload, None).unwrap();
        verify_signable_zip(&zip).unwrap();

        // A ZIP comment is rejected, since the signature is appended there.
        let mut with_comment = zip.clone();
        let len = with_comment.len();
        with_comment[len - 2] = 1;
        assert!(verify_signable_zip(&with_comment).is_err());
    }
}
