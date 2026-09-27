// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use credentials::Credentials;

use gcs::auth;
use gcs::error::GcsError;
pub use pbms::AuthFlowChoice;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AuthError {
    #[error("Error getting new access token")]
    AccessToken(GcsError),
    #[error("Unsupported authentication scheme")]
    AuthFlow,
    #[error("Saving credentials failed")]
    Credentials(anyhow::Error),
    #[error("I/O Error")]
    IoError(#[from] std::io::Error),
    #[error("Error updating refresh token")]
    UpdateRefreshToken(anyhow::Error),
    #[error("Unexpected error")]
    Unexpected,
}

impl From<AuthError> for fho::Error {
    fn from(auth_error: AuthError) -> Self {
        match auth_error {
            AuthError::AuthFlow => fho::Error::User(AuthError::AuthFlow.into()),
            AuthError::AccessToken(GcsError::AuthRequired) => {
                fho::Error::User(AuthError::AuthFlow.into())
            }
            e => fho::Error::Unexpected(e.into()),
        }
    }
}

pub async fn mint_new_access_token<I>(
    auth_flow: &AuthFlowChoice,
    ui: &I,
) -> Result<String, AuthError>
where
    I: structured_ui::Interface,
{
    log::debug!("mint_new_access_token");
    match auth_flow {
        AuthFlowChoice::Gcloud => {
            // Shell out to gcloud to get an access token.
            // This approach natively supports headless environments
            // and does not require maintaining our own OAuth tokens.
            let output = std::process::Command::new("gcloud")
                .args(["auth", "print-access-token"])
                .output()
                .map_err(AuthError::IoError)?;
            if !output.status.success() {
                return Err(AuthError::AccessToken(GcsError::ExecForAccessFailed(
                    "gcloud".into(),
                    output.status,
                    format!(
                        "{}\nHint: You may need to run `gcloud auth login` to authenticate.",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                )));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        AuthFlowChoice::Exec(exec) => {
            let output = std::process::Command::new(exec).output().map_err(AuthError::IoError)?;
            if !output.status.success() {
                return Err(AuthError::AccessToken(GcsError::ExecForAccessFailed(
                    exec.into(),
                    output.status,
                    String::from_utf8_lossy(&output.stderr).to_string(),
                )));
            }
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        AuthFlowChoice::Default | AuthFlowChoice::Pkce | AuthFlowChoice::Device => {
            let credentials = Credentials::load_or_new().await;

            match auth::new_access_token(&credentials.gcs_credentials()).await {
                Ok(a) => Ok(a),
                Err(GcsError::NeedNewRefreshToken) => {
                    update_refresh_token(auth_flow, ui).await?;
                    // Make one additional attempt now that the refresh token
                    // is updated.
                    let credentials = credentials::Credentials::load_or_new().await;
                    auth::new_access_token(&credentials.gcs_credentials())
                        .await
                        .map_err(|e| AuthError::AccessToken(e))
                }
                Err(e) => Err(AuthError::AccessToken(e)),
            }
        }
        AuthFlowChoice::NoAuth => Err(AuthError::AccessToken(GcsError::AuthRequired)),
    }
}

async fn update_refresh_token<I>(auth_flow: &AuthFlowChoice, ui: &I) -> Result<(), AuthError>
where
    I: structured_ui::Interface,
{
    let refresh_token = match auth_flow {
        AuthFlowChoice::Default | AuthFlowChoice::Pkce => {
            auth::pkce::new_refresh_token(ui).await.map_err(|e| AuthError::UpdateRefreshToken(e))
        }
        AuthFlowChoice::Device => {
            auth::device::new_refresh_token(ui).await.map_err(|e| AuthError::UpdateRefreshToken(e))
        }
        _ => Err(AuthError::AuthFlow),
    };

    match refresh_token {
        Ok(refresh_token) => {
            let mut credentials = Credentials::load_or_new().await;
            credentials.oauth2.refresh_token = refresh_token.to_string();
            credentials.save().await.map_err(|e| AuthError::Credentials(e))?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn test_mint_new_access_token_no_auth() {
        let ui = structured_ui::MockUi::new();
        let result = mint_new_access_token(&AuthFlowChoice::NoAuth, &ui).await;
        assert!(matches!(result, Err(AuthError::AccessToken(GcsError::AuthRequired))));
    }

    #[fuchsia::test]
    async fn test_update_refresh_token_no_auth() {
        let ui = structured_ui::MockUi::new();
        let result = update_refresh_token(&AuthFlowChoice::NoAuth, &ui).await;
        assert!(matches!(result, Err(AuthError::AuthFlow)));
    }
    #[fuchsia::test]
    async fn test_auth_error_into_fho_error() {
        let err1 = AuthError::AccessToken(GcsError::AuthRequired);
        let fho_err1: fho::Error = err1.into();
        assert!(matches!(fho_err1, fho::Error::User(_)));
        if let fho::Error::User(inner) = fho_err1 {
            assert_eq!(inner.to_string(), "Unsupported authentication scheme");
        }

        let err2 = AuthError::AuthFlow;
        let fho_err2: fho::Error = err2.into();
        assert!(matches!(fho_err2, fho::Error::User(_)));
        if let fho::Error::User(inner) = fho_err2 {
            assert_eq!(inner.to_string(), "Unsupported authentication scheme");
        }
    }
}
