// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::client::GceClient;
use crate::error::{GceError, IoContext as _, Result};
use crate::{GceInstanceData, GceTunnel};
use credentials::Credentials;
use discovery::gce_watcher::Instance;
use ffx_config::EnvironmentContext;
use gcs::auth::new_access_token;
use std::path::{Path, PathBuf};

/// Execution context for GCE commands, encapsulating project, zone, and an authenticated GCE API
/// client.
#[derive(Debug)]
pub struct GceContext {
    pub env_context: EnvironmentContext,
    pub project: String,
    pub zone: String,
    pub client: GceClient,
}

impl GceContext {
    /// Initializes a `GceContext` by resolving project and zone from CLI flags or ffx config,
    /// and obtaining an OAuth2 access token.
    pub async fn new(
        env_context: EnvironmentContext,
        project_flag: Option<String>,
        zone_flag: Option<String>,
    ) -> Result<Self> {
        let project = resolve_setting(&env_context, project_flag, "GCP project", "project")?;
        let zone = resolve_setting(&env_context, zone_flag, "GCE zone", "zone")?;

        let creds = load_credentials_or_adc().await;
        Self::new_with_credentials(env_context, project, zone, creds).await
    }

    /// Initializes a `GceContext` for a resolved project and zone using explicit `Credentials`.
    pub async fn new_with_credentials(
        env_context: EnvironmentContext,
        project: String,
        zone: String,
        creds: Credentials,
    ) -> Result<Self> {
        if creds.oauth2.refresh_token.is_empty() {
            return Err(GceError::MissingCredentials);
        }
        let access_token =
            new_access_token(&creds.gcs_credentials()).await.map_err(GceError::AccessToken)?;
        let client = GceClient::new(access_token);

        Ok(Self { env_context, project, zone, client })
    }

    /// Resolves the GCS bucket from `--bucket` or `gce.bucket`, falling back to
    /// `<project>-<default_suffix>`.
    pub fn resolve_bucket(&self, bucket_flag: Option<&str>, default_suffix: &str) -> String {
        resolve_config_string(&self.env_context, bucket_flag, "gce.bucket")
            .unwrap_or_else(|| format!("{}-{default_suffix}", self.project))
    }

    /// Returns `true` if a GCS bucket was explicitly specified via `--bucket` or `gce.bucket`.
    pub fn has_explicit_bucket(&self, bucket_flag: Option<&str>) -> bool {
        resolve_config_string(&self.env_context, bucket_flag, "gce.bucket").is_some()
    }

    /// Derives the GCE SSH serial port gateway endpoint for this context's zone.
    /// e.g. "us-central1-a" -> "us-central1-ssh-serialport.googleapis.com:9600"
    pub fn serial_endpoint(&self) -> String {
        get_serial_endpoint(&self.zone)
    }

    /// Reads instance state data for an instance in this context's project and zone.
    pub fn read_instance_data(&self, instance_name: &str) -> Result<Option<GceInstanceData>> {
        let instance = Instance::new(&self.project, &self.zone, instance_name)?;
        instance
            .read(&self.env_context)
            .io_context(|| format!("Failed to read GCE instance state for {}", instance.name))
    }

    /// Starts a background SSH tunnel to an instance in this context's project and zone.
    pub async fn start_tunnel(&self, instance_name: &str) -> Result<GceInstanceData> {
        GceTunnel::start_tunnel(&self.env_context, &self.project, &self.zone, instance_name).await
    }

    /// Stops the background SSH tunnel for an instance in this context's project and zone.
    pub fn stop_tunnel(&self, instance_name: &str) -> Result<()> {
        GceTunnel::stop_tunnel(&self.env_context, &self.project, &self.zone, instance_name)
    }

    /// Streams serial port output from an instance over the GCE SSH serial port gateway.
    pub async fn stream_ssh_serial<W: std::io::Write>(
        &self,
        instance_name: &str,
        port: u32,
        follow: bool,
        writer: &mut W,
    ) -> Result<()> {
        GceTunnel::stream_ssh_serial(
            &self.env_context,
            &self.project,
            &self.zone,
            instance_name,
            port,
            follow,
            writer,
        )
        .await
    }
}

/// Loads Fuchsia [`Credentials`], falling back to Google Cloud Application Default Credentials
/// (`application_default_credentials.json`) if no OAuth2 refresh token is configured in
/// `~/.fuchsia/debug/google_credentials.json`.
async fn load_credentials_or_adc() -> Credentials {
    let mut creds = Credentials::load_or_new().await;
    if !creds.oauth2.refresh_token.is_empty() {
        return creds;
    }
    for path in adc_candidate_paths() {
        if let Some(oauth2) = load_adc_oauth2(&path) {
            log::debug!("Loaded OAuth2 credentials from {}", path.display());
            creds.oauth2 = oauth2;
            break;
        }
    }
    creds
}

/// Returns candidate paths for Google Cloud Application Default Credentials in priority order.
fn adc_candidate_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(path) = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
    {
        paths.push(path);
    }
    if let Some(dir) =
        std::env::var_os("CLOUDSDK_CONFIG").filter(|s| !s.is_empty()).map(PathBuf::from)
    {
        paths.push(dir.join("application_default_credentials.json"));
    }
    if let Some(home) = home::home_dir() {
        paths.push(home.join(".config/gcloud/application_default_credentials.json"));
    }
    paths
}

/// Parses an OAuth2 user credentials JSON file (such as `application_default_credentials.json`)
/// if it exists and contains a non-empty `refresh_token`.
fn load_adc_oauth2(path: &Path) -> Option<credentials::OAuth2Credentials> {
    let content = std::fs::read_to_string(path).ok()?;
    let oauth2: credentials::OAuth2Credentials = serde_json::from_str(&content).ok()?;
    (!oauth2.refresh_token.is_empty()).then_some(oauth2)
}

/// Derives the GCE SSH serial port gateway endpoint for a given zone.
/// e.g. "us-central1-a" -> "us-central1-ssh-serialport.googleapis.com:9600"
pub fn get_serial_endpoint(zone: &str) -> String {
    let parts: Vec<&str> = zone.split('-').collect();
    let region =
        if parts.len() > 1 { parts[..parts.len() - 1].join("-") } else { zone.to_string() };
    format!("{}-ssh-serialport.googleapis.com:9600", region.to_lowercase())
}

/// Returns the first non-empty value of `flag` or the `key` ffx config setting, trimmed.
///
/// Trimming keeps a stray space in a flag or config value from later failing validation of
/// project, zone, and instance names.
pub fn resolve_config_string(
    context: &EnvironmentContext,
    flag: Option<&str>,
    key: &str,
) -> Option<String> {
    flag.map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).or_else(|| {
        context.get(key).ok().map(|s: String| s.trim().to_owned()).filter(|s| !s.is_empty())
    })
}

fn resolve_setting(
    context: &EnvironmentContext,
    flag: Option<String>,
    name: &'static str,
    param: &'static str,
) -> Result<String> {
    let config_key = format!("gce.{param}");
    resolve_config_string(context, flag.as_deref(), &config_key)
        .ok_or(GceError::MissingSetting { name, param })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_gce_context_unauthenticated() {
        let env = ffx_config::test_init().expect("test env");
        let res = GceContext::new_with_credentials(
            env.context.clone(),
            "test-project".to_string(),
            "test-zone".to_string(),
            Credentials::new(),
        )
        .await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("No Google Cloud credentials found"));
        assert!(err.contains("ffx auth generate"));
        assert!(err.contains("gcloud auth application-default login"));
    }

    #[fuchsia::test]
    fn test_load_adc_oauth2() {
        let temp = tempfile::tempdir().expect("temp dir");
        let adc_path = temp.path().join("application_default_credentials.json");

        // Missing file returns None.
        assert!(load_adc_oauth2(&adc_path).is_none());

        // Valid ADC authorized_user JSON with refresh_token returns Some.
        std::fs::write(
            &adc_path,
            r#"{
                "account": "",
                "client_id": "test-client-id.apps.googleusercontent.com",
                "client_secret": "test-client-secret",
                "quota_project_id": "my-project",
                "refresh_token": "test-refresh-token",
                "type": "authorized_user",
                "universe_domain": "googleapis.com"
            }"#,
        )
        .expect("write adc file");
        let loaded = load_adc_oauth2(&adc_path).expect("should parse valid ADC");
        assert_eq!(loaded.client_id, "test-client-id.apps.googleusercontent.com");
        assert_eq!(loaded.client_secret, "test-client-secret");
        assert_eq!(loaded.refresh_token, "test-refresh-token");

        // Empty refresh_token returns None.
        std::fs::write(
            &adc_path,
            r#"{
                "client_id": "id",
                "client_secret": "secret",
                "refresh_token": ""
            }"#,
        )
        .expect("write empty adc file");
        assert!(load_adc_oauth2(&adc_path).is_none());
    }

    #[fuchsia::test]
    fn test_has_explicit_bucket() {
        let env = ffx_config::test_init().expect("test env");
        let ctx = GceContext {
            env_context: env.context.clone(),
            project: "my-gcp-project".to_string(),
            zone: "us-central1-a".to_string(),
            client: GceClient::new("token".to_string()),
        };
        assert!(!ctx.has_explicit_bucket(None));
        assert!(!ctx.has_explicit_bucket(Some("   ")));
        assert!(ctx.has_explicit_bucket(Some("custom-bucket")));

        let configured_env = ffx_config::test_env()
            .user_config("gce.bucket", "configured-bucket")
            .build()
            .expect("configured test env");
        let configured_ctx = GceContext {
            env_context: configured_env.context.clone(),
            project: "my-gcp-project".to_string(),
            zone: "us-central1-a".to_string(),
            client: GceClient::new("token".to_string()),
        };
        assert!(configured_ctx.has_explicit_bucket(None));
    }

    #[fuchsia::test]
    async fn test_gce_context_missing_zone() {
        let env = ffx_config::test_init().expect("test env");
        let res =
            GceContext::new(env.context.clone(), Some("test-project".to_string()), None).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("No GCE zone specified"));
        assert!(err.contains("ffx config set gce.zone"));
    }

    #[fuchsia::test]
    async fn test_gce_context_missing_project() {
        let env = ffx_config::test_init().expect("test env");
        let res = GceContext::new(env.context.clone(), None, Some("test-zone".to_string())).await;
        assert!(res.is_err());
        let err = res.unwrap_err().to_string();
        assert!(err.contains("No GCP project specified"));
        assert!(err.contains("ffx config set gce.project"));
    }

    #[fuchsia::test]
    fn test_resolve_config_string_trims() {
        let env = ffx_config::test_env()
            .user_config("gce.project", "  configured-project  ")
            .build()
            .expect("test env");
        assert_eq!(
            resolve_config_string(&env.context, Some("  flag-project  "), "gce.project").as_deref(),
            Some("flag-project")
        );
        // A whitespace-only flag falls through to the (trimmed) config value.
        assert_eq!(
            resolve_config_string(&env.context, Some("   "), "gce.project").as_deref(),
            Some("configured-project")
        );
        assert_eq!(resolve_config_string(&env.context, None, "gce.zone"), None);
    }

    #[fuchsia::test]
    async fn test_gce_context_struct() {
        let env = ffx_config::test_init().expect("test env");
        let ctx = GceContext {
            env_context: env.context.clone(),
            project: "test-proj".to_string(),
            zone: "test-zone".to_string(),
            client: GceClient::new("token123".to_string()),
        };
        assert_eq!(ctx.project, "test-proj");
        assert_eq!(ctx.zone, "test-zone");
        assert_eq!(ctx.env_context.get::<String, _>("gce.project").ok(), Some("".to_string()));
    }

    #[fuchsia::test]
    fn test_serial_endpoint() {
        assert_eq!(
            get_serial_endpoint("us-central1-a"),
            "us-central1-ssh-serialport.googleapis.com:9600"
        );
        assert_eq!(
            get_serial_endpoint("europe-west1-b"),
            "europe-west1-ssh-serialport.googleapis.com:9600"
        );
    }

    #[fuchsia::test]
    fn test_resolve_bucket() {
        let env = ffx_config::test_init().expect("test env");
        let ctx = GceContext {
            env_context: env.context.clone(),
            project: "my-gcp-project".to_string(),
            zone: "us-central1-a".to_string(),
            client: GceClient::new("token".to_string()),
        };

        // Flag takes precedence
        assert_eq!(ctx.resolve_bucket(Some("custom-bucket"), "fuchsia-images"), "custom-bucket");
        assert_eq!(
            ctx.resolve_bucket(Some("  custom-bucket  "), "fuchsia-images"),
            "custom-bucket"
        );

        // Whitespace-only or empty flag falls back to default when config is unset
        assert_eq!(
            ctx.resolve_bucket(Some("   "), "fuchsia-images"),
            "my-gcp-project-fuchsia-images"
        );
        assert_eq!(ctx.resolve_bucket(None, "fuchsia-images"), "my-gcp-project-fuchsia-images");

        // Config fallback takes precedence over default
        let configured_env = ffx_config::test_env()
            .user_config("gce.bucket", "configured-bucket")
            .build()
            .expect("configured test env");
        let configured_ctx = GceContext {
            env_context: configured_env.context.clone(),
            project: "my-gcp-project".to_string(),
            zone: "us-central1-a".to_string(),
            client: GceClient::new("token".to_string()),
        };
        assert_eq!(configured_ctx.resolve_bucket(None, "fuchsia-images"), "configured-bucket");
        assert_eq!(
            configured_ctx.resolve_bucket(Some("   "), "fuchsia-images"),
            "configured-bucket"
        );
        assert_eq!(
            configured_ctx.resolve_bucket(Some("flag-override"), "fuchsia-images"),
            "flag-override"
        );

        // Whitespace-only config falls back to default
        let whitespace_env = ffx_config::test_env()
            .user_config("gce.bucket", "   ")
            .build()
            .expect("whitespace test env");
        let whitespace_ctx = GceContext {
            env_context: whitespace_env.context.clone(),
            project: "my-gcp-project".to_string(),
            zone: "us-central1-a".to_string(),
            client: GceClient::new("token".to_string()),
        };
        assert_eq!(
            whitespace_ctx.resolve_bucket(None, "fuchsia-images"),
            "my-gcp-project-fuchsia-images"
        );
    }
}
