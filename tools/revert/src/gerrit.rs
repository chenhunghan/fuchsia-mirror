// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::process::{Command, Stdio};
use url::Url;

/// Only Gerrit hosts under this domain are trusted.
const TRUSTED_HOST_SUFFIX: &str = ".googlesource.com";

/// Maximum number of characters of a Gerrit error body included in error messages.
const MAX_ERROR_BODY_CHARS: usize = 1024;

/// Seconds allowed for establishing a connection to Gerrit.
const CURL_CONNECT_TIMEOUT_SECS: &str = "10";

/// Seconds allowed for a complete Gerrit request.
const CURL_MAX_TIME_SECS: &str = "60";

/// Gerrit prefixes JSON responses with this string to prevent XSSI.
const XSSI_PREFIX: &str = ")]}'";

/// Represents a parsed Gerrit change URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GerritUrl {
    pub host: String,
    pub project: Option<String>,
    pub change_id_or_number: String,
    pub patchset: Option<u32>,
}

/// Normalizes internal corp Gerrit hostnames (e.g. *.git.corp.google.com) to public *.googlesource.com.
pub fn normalize_gerrit_host(host: &str) -> String {
    if let Some(prefix) = host.strip_suffix(".git.corp.google.com") {
        format!("{prefix}{TRUSTED_HOST_SUFFIX}")
    } else {
        host.to_string()
    }
}

/// Returns true if `host` is a trusted Gerrit host (i.e. `<name>.googlesource.com`).
pub fn is_trusted_gerrit_host(host: &str) -> bool {
    match host.strip_suffix(TRUSTED_HOST_SUFFIX) {
        Some(name) => {
            !name.is_empty()
                && !name.starts_with('.')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
        }
        None => false,
    }
}

impl GerritUrl {
    /// Parses a full Gerrit URL into its components.
    pub fn parse(raw_url: &str) -> Result<Self> {
        let parsed = Url::parse(raw_url).with_context(|| format!("Invalid URL: {raw_url}"))?;
        let host_raw =
            parsed.host_str().ok_or_else(|| anyhow!("Missing host in URL: {raw_url}"))?;
        let host = normalize_gerrit_host(host_raw);
        if !is_trusted_gerrit_host(&host) {
            return Err(anyhow!(
                "Untrusted Gerrit host '{host}' in URL: {raw_url}. Only *{TRUSTED_HOST_SUFFIX} hosts are supported."
            ));
        }

        let path = parsed.path().trim_matches('/');
        // Typical format: c/<project>/+/<change_number>[/<patchset>]
        // Or changes/<change_id>
        let segments: Vec<&str> = path.split('/').collect();

        if let Some(plus_idx) = segments.iter().position(|&s| s == "+") {
            let project = if plus_idx > 0 {
                let proj_parts = if segments[0] == "c" {
                    &segments[1..plus_idx]
                } else {
                    &segments[0..plus_idx]
                };
                if proj_parts.is_empty() { None } else { Some(proj_parts.join("/")) }
            } else {
                None
            };

            let after_plus = &segments[plus_idx + 1..];
            if after_plus.is_empty() || after_plus[0].is_empty() {
                return Err(anyhow!("Missing change ID/number after '+' in URL: {raw_url}"));
            }

            let change_id_or_number = after_plus[0].to_string();
            let patchset =
                if after_plus.len() > 1 { after_plus[1].parse::<u32>().ok() } else { None };

            Ok(Self { host, project, change_id_or_number, patchset })
        } else if segments.len() >= 2 && segments[0] == "changes" {
            let change_id_or_number = segments[1].to_string();
            let patchset = if segments.len() > 2 { segments[2].parse::<u32>().ok() } else { None };
            Ok(Self { host, project: None, change_id_or_number, patchset })
        } else {
            Err(anyhow!(
                "Unrecognized Gerrit URL format: {raw_url}. Expected format like https://<host>/c/<project>/+/<change_number>"
            ))
        }
    }
}

/// Metadata describing a Gerrit change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeDetail {
    pub id: String,
    pub project: String,
    pub branch: String,
    pub change_id: String,
    #[serde(rename = "_number")]
    pub number: u64,
    pub subject: String,
    pub status: String,
    #[serde(default)]
    pub current_revision: Option<String>,
    #[serde(default)]
    pub revisions: HashMap<String, RevisionDetail>,
    #[serde(default)]
    pub submitted: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub submission_id: Option<String>,
    /// Number of the change this change reverts, if it is a revert.
    #[serde(default)]
    pub revert_of: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevisionDetail {
    #[serde(rename = "_number")]
    pub number: u32,
    #[serde(default)]
    pub commit: Option<CommitDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitDetail {
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub subject: Option<String>,
    pub message: String,
    #[serde(default)]
    pub parents: Vec<ParentDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParentDetail {
    pub commit: String,
    #[serde(default)]
    pub subject: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevertInput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Explicitly set to `false` so Gerrit creates the revert CL in the Active (Ready for Review)
    /// state even if the caller's Gerrit account has `work_in_progress_by_default` enabled;
    /// Rubber Stamper ignores WIP changes.
    #[serde(default)]
    pub work_in_progress: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewInput {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub reviewers: Vec<ReviewerInput>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub labels: HashMap<String, i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewerInput {
    pub reviewer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

/// Client for the subset of the Gerrit REST API used by `fx revert`.
///
/// `change_id` arguments accept any change identifier understood by Gerrit (e.g. a change
/// number or a `Change-Id`), and `host` is a trusted Gerrit host such as
/// `fuchsia-review.googlesource.com`. Implemented by [`CurlGerritFetcher`] for live Gerrit
/// hosts and by mock fetchers in tests.
pub trait GerritFetcher {
    /// Fetches the details of change `change_id` on `host`, including all of its revisions and
    /// their commit messages.
    fn fetch_change_detail(&self, host: &str, change_id: &str) -> Result<ChangeDetail>;
    /// Reverts the single merged change `change_id` on `host` via Gerrit's `/revert` endpoint,
    /// using `message` (if set) as the revert commit message and `topic` (if set) as its topic.
    /// Returns the created revert change.
    fn revert_single(
        &self,
        host: &str,
        change_id: &str,
        message: Option<&str>,
        topic: Option<&str>,
    ) -> Result<ChangeDetail>;
    /// Posts a review on the current revision of `change_id` on `host` that adds `reviewers`
    /// and applies the label votes in `labels` (e.g. `Fuchsia-Auto-Submit: +1`).
    fn post_review(
        &self,
        host: &str,
        change_id: &str,
        reviewers: &[&str],
        labels: &HashMap<String, i32>,
    ) -> Result<()>;
}

/// Outcome of a curl invocation that reached the server.
#[derive(Debug)]
struct CurlResponse {
    status: u16,
    body: String,
}

/// Error from invoking a curl binary.
#[derive(Debug)]
enum CurlError {
    /// The binary could not be executed, or failed before receiving an HTTP response
    /// (e.g. DNS or authentication helper failures).
    Transport(anyhow::Error),
}

impl CurlResponse {
    /// Converts the response into the JSON body, or an error containing the HTTP status
    /// and (truncated) Gerrit error message.
    fn into_json_body(self, bin: &str, url: &str) -> Result<String> {
        if (200..300).contains(&self.status) {
            Ok(strip_xssi_prefix(&self.body))
        } else {
            Err(anyhow!(
                "{bin} request to {url} failed with HTTP {}: {}",
                self.status,
                truncate_for_error(self.body.trim())
            ))
        }
    }
}

/// Strips Gerrit's XSSI protection prefix from a JSON response body.
fn strip_xssi_prefix(raw_body: &str) -> String {
    let trimmed = raw_body.trim();
    trimmed.strip_prefix(XSSI_PREFIX).unwrap_or(trimmed).trim_start().to_string()
}

/// Truncates `s` to at most `MAX_ERROR_BODY_CHARS` characters.
fn truncate_for_error(s: &str) -> String {
    if s.chars().count() <= MAX_ERROR_BODY_CHARS {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(MAX_ERROR_BODY_CHARS).collect();
        format!("{truncated}... [truncated]")
    }
}

/// Splits curl stdout produced with `-w "\n%{http_code}"` into the body and HTTP status.
fn split_body_and_status(stdout: &str) -> Option<(String, u16)> {
    let (body, code) = stdout.rsplit_once('\n')?;
    let status = code.trim().parse::<u16>().ok()?;
    if status == 0 {
        // curl reports 000 when no HTTP response was received.
        return None;
    }
    Some((body.to_string(), status))
}

/// Rewrites a Gerrit REST URL to its authenticated `/a/` form.
fn to_authenticated_url(url: &str) -> String {
    if url.contains("/changes/") && !url.contains("/a/changes/") {
        url.replacen("/changes/", "/a/changes/", 1)
    } else {
        url.to_string()
    }
}

/// Gerrit client implemented on top of `gob-curl` (when available) or `curl`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CurlGerritFetcher;

impl CurlGerritFetcher {
    fn curl_json(url: &str) -> Result<String> {
        let auth_url = to_authenticated_url(url);

        if let Ok(gob_path) = which_gob_curl() {
            match Self::run_curl_cmd(&gob_path, &auth_url, None, false) {
                // gob-curl reached Gerrit: report its answer directly (including HTTP
                // errors) instead of masking it with an unauthenticated retry.
                Ok(resp) => return resp.into_json_body(&gob_path, &auth_url),
                Err(CurlError::Transport(e)) => {
                    eprintln!("Warning: gob-curl failed ({e:#}); falling back to curl.");
                }
            }
        } else {
            warn_gob_curl_missing();
        }

        // Fall back to standard curl
        let use_cookies = dirs_cookie_path().is_some();
        match Self::run_curl_cmd("curl", &auth_url, None, use_cookies) {
            Ok(resp) => resp.into_json_body("curl", &auth_url),
            Err(CurlError::Transport(e)) => Err(e),
        }
    }

    fn curl_post_json(url: &str, json_body: &str) -> Result<String> {
        let auth_url = to_authenticated_url(url);

        if let Ok(gob_path) = which_gob_curl() {
            match Self::run_curl_cmd(&gob_path, &auth_url, Some(json_body), false) {
                Ok(resp) => return resp.into_json_body(&gob_path, &auth_url),
                Err(CurlError::Transport(e)) => {
                    eprintln!("Warning: gob-curl POST failed ({e:#}); falling back to curl.");
                }
            }
        } else {
            warn_gob_curl_missing();
        }

        let use_cookies = dirs_cookie_path().is_some();
        match Self::run_curl_cmd("curl", &auth_url, Some(json_body), use_cookies) {
            Ok(resp) => resp.into_json_body("curl", &auth_url),
            Err(CurlError::Transport(e)) => Err(e),
        }
    }

    /// Runs `bin` (curl or gob-curl) against `url`. If `payload` is set, issues a JSON POST.
    fn run_curl_cmd(
        bin: &str,
        url: &str,
        payload: Option<&str>,
        use_cookies: bool,
    ) -> std::result::Result<CurlResponse, CurlError> {
        let mut cmd = Command::new(bin);
        // `-s` (`--silent`): suppress the progress meter, so stdout contains only the response
        // body followed by the HTTP status code written by `-w` below.
        cmd.arg("-s");
        // `-S` (`--show-error`): still print an error message to stderr when the request fails,
        // even though `-s` silences all other diagnostic output.
        cmd.arg("-S");
        // `--globoff`: disable curl's URL globbing, so `[]` and `{}` characters in Gerrit URLs are
        // sent literally instead of being expanded into multiple requests.
        cmd.arg("--globoff");
        // `--netrc-optional`: authenticate with credentials for the host from `~/.netrc` if that
        // file exists, without failing when it does not.
        cmd.arg("--netrc-optional");
        // `--connect-timeout <secs>`: maximum time allowed for establishing the connection.
        cmd.arg("--connect-timeout").arg(CURL_CONNECT_TIMEOUT_SECS);
        // `--max-time <secs>`: maximum time allowed for the whole request, so an unresponsive
        // server cannot hang the tool indefinitely.
        cmd.arg("--max-time").arg(CURL_MAX_TIME_SECS);
        // `-w "\n%{http_code}"` (`--write-out`): after the response body, print a newline and the
        // HTTP status code (`000` if no response was received). Without `--fail`, curl exits
        // successfully on HTTP errors, so `split_body_and_status` uses this trailing status code
        // to detect them while keeping Gerrit's error body for diagnostics.
        cmd.arg("-w").arg("\n%{http_code}");
        if use_cookies
            && let Some(cookie_path) = dirs_cookie_path()
            && std::path::Path::new(&cookie_path).exists()
        {
            // `-b <file>` (`--cookie`): send matching cookies from the given Netscape-format
            // cookie file (`~/.gitcookies`), which holds Gerrit authentication cookies.
            cmd.arg("-b").arg(cookie_path);
        }
        if payload.is_some() {
            // Pass the request body over stdin (`--data-binary @-`) rather than as a command-line
            // argument. `gob-curl` strips any argv entry containing the target hostname, which
            // would silently drop a `-d <json>` argument whenever the quoted commit message
            // contains a `Reviewed-on: https://<host>/...` footer.
            //
            // `-X POST` (`--request`): issue a POST request instead of the default GET.
            cmd.arg("-X").arg("POST");
            // `-H <header>` (`--header`): declare the request body as JSON for Gerrit.
            cmd.arg("-H").arg("Content-Type: application/json");
            // `--data-binary @-`: send the request body read from stdin exactly as-is (unlike
            // `-d @-`, which strips newlines and carriage returns from the input).
            cmd.arg("--data-binary").arg("@-");
            cmd.stdin(Stdio::piped());
        }
        // URLs are always constructed as `https://...`, so they cannot be parsed as options.
        cmd.arg(url);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(|e| {
            CurlError::Transport(anyhow!("Failed to execute {bin} for URL {url}: {e}"))
        })?;
        if let Some(payload) = payload
            && let Some(mut stdin) = child.stdin.take()
        {
            // Ignore BrokenPipe here; if curl exited early, wait_with_output below will report
            // its exit status and stderr.
            let _ = stdin.write_all(payload.as_bytes());
        }

        let output = child.wait_with_output().map_err(|e| {
            CurlError::Transport(anyhow!("Failed to wait on {bin} for URL {url}: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        match split_body_and_status(&stdout) {
            Some((body, status)) => Ok(CurlResponse { status, body }),
            None => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(CurlError::Transport(anyhow!(
                    "{bin} error ({}) for URL {url}: {}",
                    output.status,
                    truncate_for_error(stderr.trim())
                )))
            }
        }
    }
}

/// Warns (once per process) that `gob-curl` was not found, so requests fall back to plain `curl`.
fn warn_gob_curl_missing() {
    static WARN_ONCE: std::sync::Once = std::sync::Once::new();
    WARN_ONCE.call_once(|| {
        eprintln!(
            "Warning: gob-curl not found; falling back to plain curl (requests to internal hosts or authenticated endpoints may fail without ~/.gitcookies)."
        );
    });
}

fn which_gob_curl() -> Result<String> {
    if std::path::Path::new("/usr/bin/gob-curl").exists() {
        return Ok("/usr/bin/gob-curl".to_string());
    }
    if let Ok(output) = Command::new("which").arg("gob-curl").output()
        && output.status.success()
    {
        let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !path.is_empty() {
            return Ok(path);
        }
    }
    Err(anyhow!("gob-curl not found"))
}

fn dirs_cookie_path() -> Option<String> {
    std::env::var("HOME").ok().map(|h| format!("{h}/.gitcookies"))
}

impl GerritFetcher for CurlGerritFetcher {
    fn fetch_change_detail(&self, host: &str, change_id: &str) -> Result<ChangeDetail> {
        let url =
            format!("https://{host}/changes/{change_id}/detail?o=ALL_REVISIONS&o=ALL_COMMITS");
        let body = Self::curl_json(&url)?;
        serde_json::from_str::<ChangeDetail>(&body).with_context(|| {
            format!(
                "Failed to parse change detail from Gerrit response ({url}):\n{}",
                truncate_for_error(&body)
            )
        })
    }

    fn revert_single(
        &self,
        host: &str,
        change_id: &str,
        message: Option<&str>,
        topic: Option<&str>,
    ) -> Result<ChangeDetail> {
        let url = format!("https://{host}/changes/{change_id}/revert");
        let input = RevertInput {
            message: message.map(String::from),
            topic: topic.map(String::from),
            work_in_progress: false,
        };
        let payload = serde_json::to_string(&input)?;
        let body = Self::curl_post_json(&url, &payload)?;
        serde_json::from_str::<ChangeDetail>(&body).with_context(|| {
            format!(
                "Failed to parse revert response from Gerrit ({url}):\n{}",
                truncate_for_error(&body)
            )
        })
    }

    fn post_review(
        &self,
        host: &str,
        change_id: &str,
        reviewers: &[&str],
        labels: &HashMap<String, i32>,
    ) -> Result<()> {
        let url = format!("https://{host}/changes/{change_id}/revisions/current/review");
        let review_input = ReviewInput {
            reviewers: reviewers
                .iter()
                .map(|r| ReviewerInput { reviewer: r.to_string(), state: None })
                .collect(),
            labels: labels.clone(),
            message: None,
        };
        let payload = serde_json::to_string(&review_input)?;
        let _ = Self::curl_post_json(&url, &payload)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_gerrit_url_parse() {
        let u1 = "https://fuchsia-review.googlesource.com/c/fuchsia/+/123456";
        let parsed = GerritUrl::parse(u1).unwrap();
        assert_eq!(
            parsed,
            GerritUrl {
                host: "fuchsia-review.googlesource.com".into(),
                project: Some("fuchsia".into()),
                change_id_or_number: "123456".into(),
                patchset: None,
            }
        );

        let u2 = "https://turquoise-internal-review.googlesource.com/c/vendor/google/+/78910/3";
        let parsed = GerritUrl::parse(u2).unwrap();
        assert_eq!(
            parsed,
            GerritUrl {
                host: "turquoise-internal-review.googlesource.com".into(),
                project: Some("vendor/google".into()),
                change_id_or_number: "78910".into(),
                patchset: Some(3),
            }
        );

        let u3 = "https://fuchsia-review.git.corp.google.com/c/fuchsia/+/1731910";
        let parsed = GerritUrl::parse(u3).unwrap();
        assert_eq!(
            parsed,
            GerritUrl {
                host: "fuchsia-review.googlesource.com".into(),
                project: Some("fuchsia".into()),
                change_id_or_number: "1731910".into(),
                patchset: None,
            }
        );
    }

    #[fuchsia::test]
    fn test_gerrit_url_parse_changes_form() {
        let parsed =
            GerritUrl::parse("https://fuchsia-review.googlesource.com/changes/I1234/2").unwrap();
        assert_eq!(parsed.project, None);
        assert_eq!(parsed.change_id_or_number, "I1234");
        assert_eq!(parsed.patchset, Some(2));
    }

    #[fuchsia::test]
    fn test_gerrit_url_parse_rejects_untrusted_hosts() {
        assert!(GerritUrl::parse("https://evil.example.com/c/fuchsia/+/123").is_err());
        assert!(GerritUrl::parse("https://googlesource.com/c/fuchsia/+/123").is_err());
        assert!(
            GerritUrl::parse("https://fuchsia-review.googlesource.com.evil.com/c/f/+/1").is_err()
        );
        assert!(GerritUrl::parse("https://fuchsia-review.googlesource.com/c/fuchsia/+/").is_err());
        assert!(GerritUrl::parse("https://fuchsia-review.googlesource.com/q/foo").is_err());
    }

    #[fuchsia::test]
    fn test_is_trusted_gerrit_host() {
        assert!(is_trusted_gerrit_host("fuchsia-review.googlesource.com"));
        assert!(is_trusted_gerrit_host("turquoise-internal-review.googlesource.com"));
        assert!(!is_trusted_gerrit_host(".googlesource.com"));
        assert!(!is_trusted_gerrit_host("googlesource.com"));
        assert!(!is_trusted_gerrit_host("example.com"));
        assert!(!is_trusted_gerrit_host("..googlesource.com"));
    }

    #[fuchsia::test]
    fn test_to_authenticated_url_replaces_first_only() {
        assert_eq!(
            to_authenticated_url("https://h.googlesource.com/changes/1/changes/x"),
            "https://h.googlesource.com/a/changes/1/changes/x"
        );
        assert_eq!(
            to_authenticated_url("https://h.googlesource.com/a/changes/1"),
            "https://h.googlesource.com/a/changes/1"
        );
    }

    #[fuchsia::test]
    fn test_split_body_and_status() {
        assert_eq!(split_body_and_status("{}\n200"), Some(("{}".to_string(), 200)));
        assert_eq!(
            split_body_and_status("Not found\nmore\n404"),
            Some(("Not found\nmore".to_string(), 404))
        );
        assert_eq!(split_body_and_status("\n000"), None);
        assert_eq!(split_body_and_status("garbage"), None);
    }

    #[fuchsia::test]
    fn test_curl_response_into_json_body() {
        let ok = CurlResponse { status: 200, body: ")]}'\n{\"a\":1}".into() };
        assert_eq!(ok.into_json_body("curl", "u").unwrap(), "{\"a\":1}");

        let err = CurlResponse { status: 403, body: "Forbidden: not allowed".into() };
        let msg = format!("{:#}", err.into_json_body("curl", "u").unwrap_err());
        assert!(msg.contains("HTTP 403"), "{msg}");
        assert!(msg.contains("Forbidden: not allowed"), "{msg}");

        let huge = CurlResponse { status: 500, body: "x".repeat(10_000) };
        let msg = format!("{:#}", huge.into_json_body("curl", "u").unwrap_err());
        assert!(msg.len() < 2_000, "error message was not truncated: {} chars", msg.len());
        assert!(msg.contains("[truncated]"));
    }

    #[fuchsia::test]
    fn test_run_curl_cmd_pipes_payload_over_stdin() {
        use crate::git::test_util::{wait_until_executable, write_executable};

        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("fake-gob-curl.sh");
        // Mimic gob-curl: reject if any non-final argument contains the target hostname, and
        // echo stdin followed by the HTTP status code.
        write_executable(
            &script,
            "#!/bin/sh\n\
             while [ \"$#\" -gt 1 ]; do\n\
               case \"$1\" in\n\
                 *fuchsia-review.googlesource.com*)\n\
                   echo \"arg contained target host: $1\" >&2\n\
                   exit 2\n\
                   ;;\n\
               esac\n\
               shift\n\
             done\n\
             cat\n\
             printf '\\n200'\n",
        );
        wait_until_executable(&script);

        let payload = r#"{"message":"Reviewed-on: https://fuchsia-review.googlesource.com/c/fuchsia/+/1885744"}"#;
        let resp = CurlGerritFetcher::run_curl_cmd(
            script.to_str().unwrap(),
            "https://fuchsia-review.googlesource.com/a/changes/1885744/revert",
            Some(payload),
            false,
        )
        .unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, payload);
    }

    #[fuchsia::test]
    fn test_revert_input_serializes_work_in_progress_false() {
        let input = RevertInput {
            message: Some("Revert \"Foo\"".into()),
            topic: Some("revert-1".into()),
            work_in_progress: false,
        };
        let json = serde_json::to_string(&input).unwrap();
        assert!(json.contains("\"work_in_progress\":false"), "{json}");
    }
}
