// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::error::AgentLockError;
use ffx_config::EnvironmentContext;
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};

/// File extension used for per-target agent lockfiles.
pub const LOCK_FILE_SUFFIX: &str = ".lock";

fn validate_serial(serial: &str) -> Result<&str, AgentLockError> {
    if serial.is_empty() || serial.contains('/') || serial.contains('\0') {
        return Err(AgentLockError::InvalidTarget(serial.to_string()));
    }
    Ok(serial)
}

fn validate_session_id(session_id: &str) -> Result<&str, AgentLockError> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Err(AgentLockError::MissingSessionId);
    }
    Ok(trimmed)
}

pub(crate) fn default_lock_dir_with_bases(
    euid: u32,
    run_user_base: &Path,
    tmp_base: &Path,
) -> PathBuf {
    let run_user_dir = run_user_base.join(euid.to_string());
    if run_user_dir.is_dir() {
        run_user_dir.join("ffx/agent-locks")
    } else {
        tmp_base.join(format!("ffx-agent-locks-{euid}"))
    }
}

fn default_lock_dir() -> PathBuf {
    let euid = nix::unistd::geteuid().as_raw();
    default_lock_dir_with_bases(euid, Path::new("/run/user"), Path::new("/tmp"))
}

/// Resolves the directory storing per-target agent ownership lockfiles.
///
/// Uses `agent.lock_dir` if explicitly configured, and otherwise defaults to
/// `/run/user/<euid>/ffx/agent-locks` (or `/tmp/ffx-agent-locks-<euid>` if `/run/user/<euid>`
/// does not exist).
pub fn lock_dir(env: &EnvironmentContext) -> PathBuf {
    env.get::<String, _>(ffx_config::keys::AGENT_LOCK_DIR)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(default_lock_dir)
}

fn read_owner(path: &Path) -> Result<Option<String>, AgentLockError> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let owner = contents.trim();
            if owner.is_empty() { Ok(None) } else { Ok(Some(owner.to_string())) }
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
        Err(e) => Err(AgentLockError::Io(e)),
    }
}

/// Verifies that `serial` is currently claimed by `session_id`.
///
/// # Errors
///
/// Returns [`AgentLockError::NotClaimed`] if the target is unclaimed,
/// [`AgentLockError::ClaimedByOther`] if claimed by another session,
/// [`AgentLockError::InvalidTarget`] if `serial` is invalid,
/// [`AgentLockError::MissingSessionId`] if `session_id` is empty, or
/// [`AgentLockError::Io`] if lockfile I/O fails.
pub fn verify_target_ownership(
    env: &EnvironmentContext,
    serial: &str,
    session_id: &str,
) -> Result<(), AgentLockError> {
    let session_id = validate_session_id(session_id)?;
    let serial = validate_serial(serial)?;
    let path = lock_dir(env).join(format!("{serial}{LOCK_FILE_SUFFIX}"));

    match read_owner(&path)? {
        Some(owner) if owner == session_id => Ok(()),
        Some(owner) => Err(AgentLockError::ClaimedByOther { serial: serial.to_string(), owner }),
        None => Err(AgentLockError::NotClaimed(serial.to_string())),
    }
}

/// Claims target `serial` for `session_id` by atomically creating `<serial>.lock`.
///
/// # Errors
///
/// Returns [`AgentLockError::ClaimedByOther`] if the target is claimed by another session,
/// [`AgentLockError::InvalidTarget`] if `serial` is invalid,
/// [`AgentLockError::MissingSessionId`] if `session_id` is empty, or
/// [`AgentLockError::Io`] if lockfile I/O fails.
pub fn claim_target(
    env: &EnvironmentContext,
    serial: &str,
    session_id: &str,
) -> Result<(), AgentLockError> {
    let session_id = validate_session_id(session_id)?;
    let serial = validate_serial(serial)?;
    let dir = lock_dir(env);
    fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
    let path = dir.join(format!("{serial}{LOCK_FILE_SUFFIX}"));

    let mut tmp = tempfile::NamedTempFile::new_in(&dir)?;
    tmp.write_all(session_id.as_bytes())?;
    match tmp.persist_noclobber(&path) {
        Ok(_) => Ok(()),
        Err(e) if e.error.kind() == ErrorKind::AlreadyExists => match read_owner(&path)? {
            Some(owner) if owner == session_id => Ok(()),
            Some(owner) => {
                Err(AgentLockError::ClaimedByOther { serial: serial.to_string(), owner })
            }
            // Note: check-then-act between read_owner and persist is an accepted race for v1.
            None => e.file.persist(&path).map(|_| ()).map_err(|e| AgentLockError::Io(e.error)),
        },
        Err(e) => Err(AgentLockError::Io(e.error)),
    }
}

/// Releases the claim on `serial` by removing `<serial>.lock`.
///
/// If `caller_session_id` is `Some(id)`, verifies that the lock is currently owned by `id`
/// before removing it. If `None`, removes the lockfile unconditionally.
///
/// # Errors
///
/// Returns [`AgentLockError::NotClaimed`] if the target is not claimed,
/// [`AgentLockError::ClaimedByOther`] if `caller_session_id` is `Some` and does not match the
/// owning session, [`AgentLockError::InvalidTarget`] if `serial` is invalid,
/// [`AgentLockError::MissingSessionId`] if `caller_session_id` is `Some("")`, or
/// [`AgentLockError::Io`] if lockfile I/O fails.
pub fn release_target(
    env: &EnvironmentContext,
    serial: &str,
    caller_session_id: Option<&str>,
) -> Result<(), AgentLockError> {
    let serial = validate_serial(serial)?;
    let path = lock_dir(env).join(format!("{serial}{LOCK_FILE_SUFFIX}"));

    if let Some(caller) = caller_session_id {
        let caller = validate_session_id(caller)?;
        match read_owner(&path)? {
            Some(owner) if owner == caller => {}
            Some(owner) => {
                return Err(AgentLockError::ClaimedByOther { serial: serial.to_string(), owner });
            }
            None => return Err(AgentLockError::NotClaimed(serial.to_string())),
        }
    }

    // Note: check-then-act between read_owner and remove_file is an accepted race for v1.
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            Err(AgentLockError::NotClaimed(serial.to_string()))
        }
        Err(e) => Err(AgentLockError::Io(e)),
    }
}
