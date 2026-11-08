// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod assembly;

use anyhow::{Context, Result, anyhow};
use assembly::Assembly;
use assembly_android_config::OtaMetadataSource;
use assembly_artifact_cache::{ArtifactCache, ArtifactError};
use assembly_tool::PlatformToolProvider;
use assembly_util::{fast_copy, shorten_path};
use camino::Utf8PathBuf;
use delivery_blob::DeliveryBlobType;
use errors::FfxError;
use ffx_config::EnvironmentContext;
use ffx_product_bundle_create_args::CreateCommand;
use ffx_writer::{SimpleWriter, ToolIO};
use fho::{FfxMain, FfxTool};
use gcs;
use pbms;
use product_bundle::{ProductBundleBuilder, Slot};
use structured_ui;
use tempfile::tempdir;

#[derive(FfxTool)]
pub struct ProductBundleCreateTool {
    #[command]
    pub cmd: CreateCommand,
    ctx: EnvironmentContext,
}

fho::embedded_plugin!(ProductBundleCreateTool);

/// Create a fuchsia product bundle.
impl FfxMain for ProductBundleCreateTool {
    type Writer = SimpleWriter;

    type Error = ::fho::Error;

    async fn main(self, writer: Self::Writer) -> fho::Result<()> {
        let build_dir = self
            .ctx
            .build_dir()
            .map(|p| {
                Utf8PathBuf::from_path_buf(p.to_path_buf()).map_err(|build_dir| {
                    fho::bug!("Failed to parse build_dir as utf8: {}", build_dir.display())
                })
            })
            .transpose()?;
        product_bundle_create(&self.ctx, self.cmd, build_dir, writer)
            .await
            .map_err(flatten_error_sources)
    }
}

/// Create a fuchsia product bundle and return an anyhow result.
/// This allows us to work with anyhow, and map to a fho result above.
async fn product_bundle_create(
    context: &EnvironmentContext,
    cmd: CreateCommand,
    build_dir: Option<Utf8PathBuf>,
    writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let sanitized_cmd = cmd.try_into()?;
    Box::pin(sanitized_product_bundle_create(context, sanitized_cmd, build_dir, writer)).await
}

/// Convert the anyhow error into a pretty/stacked fho error.
fn flatten_error_sources(e: ArtifactError) -> fho::Error {
    let suggestion =
        e.suggestion.map(|s| format!("\n\nSuggestion: {}", s)).unwrap_or_else(|| "".to_string());
    FfxError::Error(
        anyhow::anyhow!(
            "Failed: {}{}{}",
            e.error,
            e.error
                .chain()
                .skip(1)
                .enumerate()
                .map(|(i, e)| format!("\n  {: >3}.  {}", i + 1, e))
                .collect::<Vec<String>>()
                .concat(),
            suggestion,
        ),
        -1,
    )
    .into()
}

/// All the inputs necessary to run `ffx product-bundle create` after checking
/// that the arguments were properly given.
#[derive(Debug)]
struct SanitizedCreateCommand {
    /// The platform artifacts to use.
    /// If None, then we use the default local artifacts.
    pub platform: Option<String>,

    /// The product config to use.
    pub product_config: String,

    /// The product config to use for the recovery image.
    pub recovery_product_config: Option<String>,

    /// The board config to use.
    pub board_config: String,

    /// The board config to use for the recovery image.
    pub recovery_board_config: Option<String>,

    /// The name to add to the output product bundle.
    pub output_name: Option<String>,

    /// The authentication flow to use to access googleapis.
    pub auth: pbms::AuthFlowChoice,

    /// The tuf keys to use.
    pub tuf_keys: Option<Utf8PathBuf>,

    /// path to a file specifying developer-level overrides for assembly.
    pub developer_overrides: Option<Utf8PathBuf>,

    /// What result we want from running `ffx product create`.
    pub result: CreateResult,

    /// Whether to only build the ZBI and skip filesystems.
    pub zbi_only: bool,

    /// The board input bundle sets to use.
    pub bib_sets: Vec<String>,

    /// The product input bundles to use.
    pub pibs: Vec<String>,

    /// Android artifacts to generate alongside the product bundle, if requested.
    pub android: Option<AndroidOutput>,
}

/// Inputs for generating Android-compatible artifacts.
#[derive(Debug, PartialEq)]
struct AndroidOutput {
    /// Directory to write Android-compatible partition images and ota.zip to.
    pub out_dir: Utf8PathBuf,

    /// Where the Android build metadata of the ota.zip comes from.
    pub metadata: AndroidOtaMetadata,

    /// Optional X.509 certificate to embed as META-INF/com/android/otacert.
    pub ota_cert: Option<Utf8PathBuf>,
}

/// Source of the Android device and build metadata recorded in the ota.zip.
#[derive(Debug, PartialEq)]
enum AndroidOtaMetadata {
    /// Copy the metadata from an existing Android ota.zip.
    BaseOta(Utf8PathBuf),
    /// Derive the metadata from Android build.prop files.
    BuildProps(Vec<Utf8PathBuf>),
}

/// What result we want from running `ffx product create`.
#[derive(Debug)]
enum CreateResult {
    /// Stage the inputs, but do not run assembly.
    Stage,

    /// Run assembly and generate the outputs to this path.
    Out(Utf8PathBuf),

    /// Run assembly and output only what's required for the ZBI to this path.
    ZbiOnly(Utf8PathBuf),

    /// Run assembly and generates the outputs to the default location.
    Default,
}

/// Parse the input command, and ensure that the user formatted it correctly,
/// then return a new command with all the requirements for running
/// `ffx product create`.
impl TryFrom<CreateCommand> for SanitizedCreateCommand {
    type Error = anyhow::Error;

    fn try_from(cmd: CreateCommand) -> Result<Self> {
        let platform = cmd.platform;

        // Determine what result we want from this command.
        let result = match (cmd.stage, cmd.out, cmd.zbi_only) {
            (true, Some(_), _) => {
                anyhow::bail!("--stage and --out cannot be used together.")
            }
            (true, _, true) => {
                anyhow::bail!("--stage and --zbi-only cannot be used together.")
            }
            (_, None, true) => {
                anyhow::bail!("--out is required when --zbi-only is specified.");
            }
            (true, None, false) => CreateResult::Stage,
            (false, Some(out), true) => CreateResult::ZbiOnly(out),
            (false, Some(out), false) => CreateResult::Out(out),
            (false, None, false) => CreateResult::Default,
        };

        // Choose between a product_config.board_config combo and --product --board flags.
        let (product_config, board_config) =
            if let Some(combo) = cmd.product_config_board_config_combo {
                let (p, b) = combo
                    .split_once(".")
                    .context("product_config.board_config combo must have a period")?;
                (p.to_string(), b.to_string())
            } else {
                let p = cmd.product_config.context("--product-config is missing")?;
                let b = cmd.board_config.context("--board-config is missing")?;
                (p, b)
            };

        let CreateCommand {
            output_name,
            tuf_keys,
            developer_overrides,
            auth,
            recovery_product_config,
            recovery_board_config,
            zbi_only,
            bib_set,
            pib,
            android,
            android_base_ota,
            android_build_prop,
            android_ota_cert,
            ..
        } = cmd;

        if recovery_board_config.is_some() && recovery_product_config.is_none() {
            anyhow::bail!(
                "--recovery-product-config is required if --recovery-board-config is specified."
            );
        }

        let android = match android {
            None => {
                for (flag, is_set) in [
                    ("--android-base-ota", android_base_ota.is_some()),
                    ("--android-build-prop", !android_build_prop.is_empty()),
                    ("--android-ota-cert", android_ota_cert.is_some()),
                ] {
                    if is_set {
                        anyhow::bail!("--android is required when {flag} is specified.");
                    }
                }
                None
            }
            Some(out_dir) => {
                if matches!(result, CreateResult::Stage) {
                    anyhow::bail!("--android and --stage cannot be used together.");
                }
                if zbi_only {
                    anyhow::bail!("--android and --zbi-only cannot be used together.");
                }
                if tuf_keys.is_none() {
                    anyhow::bail!("--tuf-keys is required when --android is specified.");
                }
                let metadata = match (android_base_ota, android_build_prop.is_empty()) {
                    (Some(base_ota), true) => AndroidOtaMetadata::BaseOta(base_ota),
                    (None, false) => AndroidOtaMetadata::BuildProps(android_build_prop),
                    (Some(_), false) => anyhow::bail!(
                        "--android-base-ota and --android-build-prop cannot be used together."
                    ),
                    (None, true) => anyhow::bail!(
                        "--android-base-ota or --android-build-prop is required when --android is specified."
                    ),
                };
                Some(AndroidOutput { out_dir, metadata, ota_cert: android_ota_cert })
            }
        };

        Ok(Self {
            platform,
            product_config,
            recovery_product_config,
            board_config,
            recovery_board_config,
            output_name,
            auth,
            tuf_keys,
            developer_overrides,
            result,
            zbi_only,
            bib_sets: bib_set,
            pibs: pib,
            android,
        })
    }
}

fn default_path_for_product_bundle_name(
    context: &EnvironmentContext,
    name: impl AsRef<str>,
) -> fho::Result<Utf8PathBuf> {
    let home = context.get_home_dir().map_err(|e| fho::bug!("Getting the home dir: {e}"))?;
    let home = Utf8PathBuf::from_path_buf(home)
        .map_err(|e| fho::bug!("error converting path to utf8: {:?}", e))?;
    Ok(home.join(".fuchsia").join("product_bundles").join(name.as_ref()))
}

/// Construct a product bundle using sanitized inputs.
async fn sanitized_product_bundle_create(
    context: &EnvironmentContext,
    cmd: SanitizedCreateCommand,
    build_dir: Option<Utf8PathBuf>,
    mut writer: SimpleWriter,
) -> Result<(), ArtifactError> {
    let tmp = tempdir().unwrap();
    let tmp_path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    let gcs_client = gcs::client::Client::initial().map_err(|e| {
        ArtifactError::new(anyhow::anyhow!("Failed to initialize GCS client: {}", e))
    })?;
    if !matches!(cmd.auth, pbms::AuthFlowChoice::NoAuth) {
        let token = pbms::handle_new_access_token(&cmd.auth, &structured_ui::MockUi::new())
            .await
            .context("Failed to handle or retrieve new access token")?;
        gcs_client.set_access_token(token).await;
    }

    let cache = ArtifactCache::new(build_dir, gcs_client)?;
    let assembly = Assembly::new(
        &cache,
        cmd.platform.clone(),
        cmd.product_config,
        cmd.board_config.clone(),
        cmd.bib_sets.clone(),
        cmd.pibs.clone(),
    )
    .await?;
    writer
        .line(format!("Staged the artifacts\n{}", assembly.version_string()))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    let name = cmd.output_name.unwrap_or_else(|| {
        let product_name = assembly.product_config_release_info.info.name.clone();
        let board_name = assembly.board_config_release_info.info.name.clone();
        format!("{}.{}", product_name, board_name)
    });

    // Return early if we are only staging the inputs.
    let out = match cmd.result {
        CreateResult::Stage => return Ok(()),
        CreateResult::Out(out) => Ok(out),
        CreateResult::ZbiOnly(out) => Ok(out),
        CreateResult::Default => {
            default_path_for_product_bundle_name(context, &name).map_err(anyhow::Error::from)
        }
    }?;

    writer
        .line(format!("Assembling into {} ...", out))
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;
    let tools = PlatformToolProvider::new(assembly.platform_path.clone());
    let should_configure_example =
        context.get::<bool, _>("assembly_example_enabled").unwrap_or_default();
    let system = Box::pin(assembly.create_system(
        context,
        should_configure_example,
        cmd.zbi_only,
        cmd.developer_overrides.clone(),
        &tmp_path.join("system"),
    ))
    .await?;

    if cmd.zbi_only {
        let zbi_path = system
            .images
            .iter()
            .find_map(|i| match i {
                assembled_system::Image::ZBI { path, .. } => Some(path.clone()),
                _ => None,
            })
            .ok_or_else(|| anyhow!("No ZBI found in assembled system"))?;

        if !out.is_dir() {
            return Err(ArtifactError::new(anyhow::anyhow!("Zbi output path is not a directory")));
        }

        let dest_path = out.join("fuchsia.zbi");

        fast_copy(&zbi_path, &dest_path)
            .with_context(|| format!("copying zbi from {} to {}", zbi_path, dest_path))?;
        println!("{}", dest_path);
        return Ok(());
    }
    let mut builder =
        ProductBundleBuilder::new(name.clone()).system(system, Slot::A).update_package(1);

    if let Some(tuf_keys) = cmd.tuf_keys {
        builder = builder.repository(DeliveryBlobType::Type1, tuf_keys);
    }

    // Build the recovery image if requested.
    if let Some(recovery_product_config) = cmd.recovery_product_config {
        let recovery_assembly = Assembly::new(
            &cache,
            cmd.platform.clone(),
            recovery_product_config,
            cmd.recovery_board_config.unwrap_or_else(|| cmd.board_config.clone()),
            cmd.bib_sets.clone(),
            vec![],
        )
        .await?;
        let recovery_system = Box::pin(recovery_assembly.create_system(
            context,
            should_configure_example,
            /* zbi_only=*/ false,
            cmd.developer_overrides.clone(),
            &tmp_path.join("recovery_system"),
        ))
        .await?;
        builder = builder.system(recovery_system, Slot::R);
    }

    let product_bundle = builder
        .build(Box::new(tools), &out)
        .await
        .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;

    if let Some(android) = &cmd.android {
        writer
            .line(format!("Generating Android artifacts into {} ...", android.out_dir))
            .map_err(|e| ArtifactError::new(anyhow::anyhow!("{}", e)))?;
        let metadata_source = match &android.metadata {
            AndroidOtaMetadata::BaseOta(path) => OtaMetadataSource::BaseOta(path),
            AndroidOtaMetadata::BuildProps(paths) => OtaMetadataSource::BuildProps(paths),
        };
        assembly_android_config::create_android_artifacts(
            &product_bundle,
            &android.out_dir,
            metadata_source,
            android.ota_cert.as_deref(),
        )
        .with_context(|| format!("Generating Android artifacts into {}", android.out_dir))?;
    }

    cache.purge()?;

    println!(
        "\nNext, try flashing this product bundle:\n\tffx target flash -b {}",
        shorten_path(&out)
    );
    println!(
        "\nOr archive the product bundle to share it with someone else:\
         \n\t(cd {} && zip -r ~/{}.zip *)",
        shorten_path(&out),
        name.clone(),
    );

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use pbms::AuthFlowChoice;

    fn base_create_command() -> CreateCommand {
        CreateCommand {
            product_config_board_config_combo: None,
            platform: None,
            product_config: Some("product".to_string()),
            recovery_product_config: None,
            board_config: Some("board".to_string()),
            recovery_board_config: None,
            output_name: None,
            tuf_keys: None,
            developer_overrides: None,
            stage: false,
            out: None,
            auth: AuthFlowChoice::Default,
            zbi_only: false,
            bib_set: vec![],
            pib: vec![],
            android: None,
            android_base_ota: None,
            android_build_prop: vec![],
            android_ota_cert: None,
        }
    }

    fn android_create_command() -> CreateCommand {
        let mut cmd = base_create_command();
        cmd.tuf_keys = Some("keys".into());
        cmd.android = Some("android_out".into());
        cmd.android_base_ota = Some("base-ota.zip".into());
        cmd
    }

    #[test]
    fn test_recovery_board_config_requires_recovery_product_config() {
        let mut cmd = base_create_command();
        cmd.recovery_board_config = Some("recovery_board".to_string());

        let result = SanitizedCreateCommand::try_from(cmd);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "--recovery-product-config is required if --recovery-board-config is specified."
        );
    }

    #[test]
    fn test_recovery_board_config_success() {
        let mut cmd = base_create_command();
        cmd.recovery_product_config = Some("recovery_product".to_string());
        cmd.recovery_board_config = Some("recovery_board".to_string());

        let result = SanitizedCreateCommand::try_from(cmd);
        assert!(result.is_ok());
        let sanitized = result.unwrap();
        assert_eq!(sanitized.recovery_board_config, Some("recovery_board".to_string()));
    }

    #[test]
    fn test_default_path_for_product_bundle_name_isolated() {
        let env = ffx_config::test_env().build().expect("test env");
        let path =
            default_path_for_product_bundle_name(&env.context, "my_pb").expect("default pb path");
        let expected_root =
            std::path::absolute(env.isolate_root.path()).expect("absolute isolate_root");
        assert_eq!(
            path,
            Utf8PathBuf::from_path_buf(
                expected_root.join(".fuchsia").join("product_bundles").join("my_pb")
            )
            .unwrap()
        );

        let env_with_home =
            ffx_config::test_env().env_var("HOME", "/custom/home").build().expect("test env");
        let custom_path = default_path_for_product_bundle_name(&env_with_home.context, "my_pb")
            .expect("custom pb path");
        assert_eq!(custom_path, Utf8PathBuf::from("/custom/home/.fuchsia/product_bundles/my_pb"));
    }

    #[test]
    fn test_android_requires_tuf_keys() {
        let mut cmd = android_create_command();
        cmd.tuf_keys = None;

        let err = SanitizedCreateCommand::try_from(cmd).unwrap_err();
        assert_eq!(err.to_string(), "--tuf-keys is required when --android is specified.");
    }

    #[test]
    fn test_android_sub_flags_require_android() {
        let mut base_ota = base_create_command();
        base_ota.android_base_ota = Some("base-ota.zip".into());
        assert_eq!(
            SanitizedCreateCommand::try_from(base_ota).unwrap_err().to_string(),
            "--android is required when --android-base-ota is specified."
        );

        let mut build_prop = base_create_command();
        build_prop.android_build_prop = vec!["build.prop".into()];
        assert_eq!(
            SanitizedCreateCommand::try_from(build_prop).unwrap_err().to_string(),
            "--android is required when --android-build-prop is specified."
        );

        let mut cert = base_create_command();
        cert.android_ota_cert = Some("ota.x509.pem".into());
        assert_eq!(
            SanitizedCreateCommand::try_from(cert).unwrap_err().to_string(),
            "--android is required when --android-ota-cert is specified."
        );
    }

    #[test]
    fn test_android_conflicts_with_stage_and_zbi_only() {
        let mut stage_cmd = android_create_command();
        stage_cmd.stage = true;
        assert_eq!(
            SanitizedCreateCommand::try_from(stage_cmd).unwrap_err().to_string(),
            "--android and --stage cannot be used together."
        );

        let mut zbi_cmd = android_create_command();
        zbi_cmd.zbi_only = true;
        zbi_cmd.out = Some("pb_out".into());
        assert_eq!(
            SanitizedCreateCommand::try_from(zbi_cmd).unwrap_err().to_string(),
            "--android and --zbi-only cannot be used together."
        );
    }

    #[test]
    fn test_android_requires_exactly_one_metadata_source() {
        let mut neither = android_create_command();
        neither.android_base_ota = None;
        assert_eq!(
            SanitizedCreateCommand::try_from(neither).unwrap_err().to_string(),
            "--android-base-ota or --android-build-prop is required when --android is specified."
        );

        let mut both = android_create_command();
        both.android_build_prop = vec!["build.prop".into()];
        assert_eq!(
            SanitizedCreateCommand::try_from(both).unwrap_err().to_string(),
            "--android-base-ota and --android-build-prop cannot be used together."
        );
    }

    #[test]
    fn test_android_with_base_ota_succeeds() {
        let mut cmd = android_create_command();
        cmd.android_ota_cert = Some("ota.x509.pem".into());

        let sanitized = SanitizedCreateCommand::try_from(cmd).unwrap();
        assert_eq!(
            sanitized.android,
            Some(AndroidOutput {
                out_dir: "android_out".into(),
                metadata: AndroidOtaMetadata::BaseOta("base-ota.zip".into()),
                ota_cert: Some("ota.x509.pem".into()),
            })
        );
    }

    #[test]
    fn test_android_with_build_props_succeeds() {
        let mut cmd = android_create_command();
        cmd.android_base_ota = None;
        cmd.android_build_prop = vec!["system/build.prop".into(), "vendor/build.prop".into()];

        let sanitized = SanitizedCreateCommand::try_from(cmd).unwrap();
        assert_eq!(
            sanitized.android,
            Some(AndroidOutput {
                out_dir: "android_out".into(),
                metadata: AndroidOtaMetadata::BuildProps(vec![
                    "system/build.prop".into(),
                    "vendor/build.prop".into(),
                ]),
                ota_cert: None,
            })
        );
    }
}
