// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use fidl::endpoints::create_endpoints;
use fidl_fuchsia_component_runner as frunner;
use fidl_fuchsia_unknown as funknown;
use starnix_uapi::errno;
use starnix_uapi::errors::Errno;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Purpose for which a container namespace directory is being opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NamespaceNodePurpose {
    /// Backing directory for a `remotefs` or `remote_bundle` filesystem mount.
    DirectoryMount { read_only: bool },
    /// Encryption key directory (`keylocation=`) for a `remotevol` volume mount.
    VolumeKeys,
}

/// Returns whether `path` is a canonical absolute path suitable for addressing a container
/// namespace entry or subdirectory.
///
/// Canonical namespace paths start with `/`, contain at least one path segment, do not end with
/// `/`, and contain no empty (`//`), `.`, or `..` segments.
fn is_canonical_ns_path(path: &str) -> bool {
    path.starts_with('/')
        && path.len() > 1
        && !path.ends_with('/')
        && path[1..]
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
}

/// Returns whether `path` refers to a namespace capability that must never be mounted or opened as
/// a directory tree in the container VFS.
fn is_disallowed_ns_dir_path(path: &str) -> bool {
    matches!(path, "/svc" | "/tmp_lifecycle")
        || path.starts_with("/svc/")
        || path.starts_with("/tmp_lifecycle/")
}

/// Validates that `path` is a canonical container namespace path allowed for `purpose`.
pub fn check_ns_dir_path(path: &str, purpose: NamespaceNodePurpose) -> Result<(), Errno> {
    if !is_canonical_ns_path(path) || is_disallowed_ns_dir_path(path) {
        return Err(errno!(ENOENT));
    }

    if path == "/pkg" || path.starts_with("/pkg/") {
        match purpose {
            NamespaceNodePurpose::DirectoryMount { read_only } => {
                if !read_only || !path.strip_prefix("/pkg/data/").is_some_and(|sub| !sub.is_empty())
                {
                    return Err(errno!(EACCES));
                }
            }
            NamespaceNodePurpose::VolumeKeys => {
                return Err(errno!(EACCES));
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
pub struct ContainerNamespace {
    /// Internal collection of path to namespace entry.
    namespace_entries: HashMap<PathBuf, funknown::CloneableSynchronousProxy>,
}

impl ContainerNamespace {
    pub fn new() -> Self {
        Self { namespace_entries: Default::default() }
    }

    /// Returns a bool indicating whether this ContainerNamespace contains a
    /// channel entry corresponding to the given path.
    pub fn has_channel_entry(&self, channel_path: impl AsRef<Path>) -> bool {
        return self.namespace_entries.contains_key(channel_path.as_ref());
    }

    /// Removes and returns a channel at the given namespace path.
    pub fn remove_namespace_channel(
        &mut self,
        channel_path: impl AsRef<Path>,
    ) -> Result<zx::Channel, anyhow::Error> {
        let path = channel_path.as_ref();
        if !path.is_absolute() {
            anyhow::bail!(
                "Invalid parameter provided to remove_namespace_channel: {}",
                path.display()
            );
        }

        match self.namespace_entries.remove(path) {
            Some(cloneable_proxy) => Ok(cloneable_proxy.into_channel()),
            None => anyhow::bail!("Could not find an entry for {}", path.display()),
        }
    }

    /// Attempts to find a channel at the given namespace path.
    pub fn get_namespace_channel(
        &self,
        channel_path: impl AsRef<Path>,
    ) -> Result<zx::Channel, anyhow::Error> {
        let path = channel_path.as_ref();
        if !path.is_absolute() {
            anyhow::bail!(
                "Invalid parameter provided to get_namespace_channel: {}",
                path.display()
            );
        }

        match self.namespace_entries.get(path) {
            Some(cloneable_proxy) => {
                let (cloned_client, cloned_server) =
                    create_endpoints::<funknown::CloneableMarker>();
                let clone_result = cloneable_proxy.clone(cloned_server);
                if clone_result.is_err() {
                    anyhow::bail!("Unable to clone the proxy channel for {}!", path.display())
                }
                Ok(cloned_client.into_channel())
            }
            None => anyhow::bail!("Could not find an entry for {}", path.display()),
        }
    }

    /// Resolves the closest matching namespace root entry for canonical `search_path` without cloning
    /// its channel, returning `(matched_root, remaining_subdir)`.
    pub fn find_closest_entry<'a>(
        &self,
        search_path: &'a str,
    ) -> Result<(&'a str, &'a str), anyhow::Error> {
        if !is_canonical_ns_path(search_path) {
            anyhow::bail!("Non-canonical path provided to find_closest_entry: {}", search_path);
        }
        if is_disallowed_ns_dir_path(search_path) {
            anyhow::bail!(
                "Disallowed namespace path provided to find_closest_entry: {}",
                search_path
            );
        }

        let mut prefix = search_path;
        loop {
            if self.has_channel_entry(prefix) {
                let remaining_subdir = search_path.get(prefix.len() + 1..).unwrap_or("");
                return Ok((prefix, remaining_subdir));
            }
            let Some((parent, _)) = prefix.rsplit_once('/') else {
                break;
            };
            if parent.is_empty() {
                break;
            }
            prefix = parent;
        }

        anyhow::bail!("Unable to find a namespace corresponding to {}", search_path);
    }

    /// Attempts to clone this ContainerNamespace, returning an error if the
    /// proxy cloning process failed.
    pub fn try_clone(&self) -> Result<ContainerNamespace, anyhow::Error> {
        let mut cloned_entries = HashMap::new();
        for (path, _) in &self.namespace_entries {
            match self.get_namespace_channel(path) {
                Ok(cloned_channel) => {
                    cloned_entries.insert(
                        path.clone(),
                        funknown::CloneableSynchronousProxy::new(cloned_channel),
                    );
                }
                Err(err) => {
                    anyhow::bail!(
                        "The ContainerNamespace clone operation for {} has failed: {}",
                        path.display(),
                        err,
                    )
                }
            }
        }
        Ok(ContainerNamespace { namespace_entries: cloned_entries })
    }
}

impl From<Vec<frunner::ComponentNamespaceEntry>> for ContainerNamespace {
    fn from(namespace: Vec<frunner::ComponentNamespaceEntry>) -> Self {
        let mut namespace_entries = HashMap::new();
        for mut entry in namespace {
            if let (Some(entry_name), Some(entry_dir)) =
                (entry.path.clone(), entry.directory.take())
            {
                let entry_channel = entry_dir.into_channel();
                namespace_entries.insert(
                    PathBuf::from(entry_name),
                    funknown::CloneableSynchronousProxy::new(entry_channel),
                );
            }
        }
        ContainerNamespace { namespace_entries }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl::endpoints::{ClientEnd, Proxy};
    use fidl_fuchsia_io as fio;
    use fuchsia_fs::directory;

    #[::fuchsia::test]
    fn correctly_reports_entries() {
        // Initialize with only the /pkg channel.
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        let data_handle = ClientEnd::new(pkg_channel);
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/pkg".to_string()),
            directory: Some(data_handle),
            ..Default::default()
        });

        // Assert that /pkg is reported, and /data is not.
        let cn_under_test = ContainerNamespace::from(ns);
        assert!(cn_under_test.has_channel_entry("/pkg"));
        assert_eq!(cn_under_test.has_channel_entry("/data"), false);
    }

    #[::fuchsia::test]
    fn correctly_provides_and_retains_channel_entries() {
        // Initialize with only the /pkg channel.
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        let data_handle = ClientEnd::new(pkg_channel);
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/pkg".to_string()),
            directory: Some(data_handle),
            ..Default::default()
        });

        // Assert that we can get a channel for /pkg, that the channel is valid,
        // and that the ContainerNamespace still retains its own /pkg reference.
        let cn_under_test = ContainerNamespace::from(ns);
        let returned_channel = cn_under_test
            .get_namespace_channel("/pkg")
            .expect("get_namespace_channel should return a valid /pkg channel.");
        assert!(returned_channel.write(b"hello", &mut vec![]).is_ok());
        assert!(cn_under_test.has_channel_entry("/pkg"));
    }

    #[::fuchsia::test]
    fn returns_err_on_invalid_request() {
        let _stub_exec = fuchsia_async::TestExecutor::new();

        // Initialize with no channels, and validate request fails.
        let cn_under_test = ContainerNamespace::new();
        assert!(cn_under_test.get_namespace_channel("/pkg").is_err());
    }

    #[::fuchsia::test]
    fn correctly_returns_closest_channel_partial_match() {
        // Initialize with only the /pkg channel.
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        let data_handle = ClientEnd::new(pkg_channel);
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/pkg".to_string()),
            directory: Some(data_handle),
            ..Default::default()
        });

        // Assert that a request for a namespace extension path
        // (e.g. /pkg/foo/bar) resolves to `/pkg` and yields a valid channel for it.
        let cn_under_test = ContainerNamespace::from(ns);
        let (ns_root, subdir) = cn_under_test
            .find_closest_entry("/pkg/foo/bar")
            .expect("find_closest_entry should resolve /pkg.");
        assert_eq!(ns_root, "/pkg");
        let returned_channel = cn_under_test
            .get_namespace_channel(ns_root)
            .expect("get_namespace_channel should return a valid /pkg channel.");

        // Assert that the channel is valid, and the ContainerNamespace retains
        // a reference to the root channel as well.
        assert!(returned_channel.write(b"hello", &mut vec![]).is_ok());
        assert!(cn_under_test.has_channel_entry("/pkg"));

        // Assert that the remaining subdir returned is correct.
        assert_eq!(subdir, "foo/bar");
    }

    #[::fuchsia::test]
    fn correctly_returns_closest_channel_exact_match() {
        // Initialize with only the /pkg channel.
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        let data_handle = ClientEnd::new(pkg_channel);
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/pkg".to_string()),
            directory: Some(data_handle),
            ..Default::default()
        });

        // Assert that a request for an exact namespace (e.g. /pkg)
        // resolves to `/pkg` and yields a valid channel for it.
        let cn_under_test = ContainerNamespace::from(ns);
        let (ns_root, subdir) = cn_under_test
            .find_closest_entry("/pkg")
            .expect("find_closest_entry should resolve /pkg.");
        assert_eq!(ns_root, "/pkg");
        let returned_channel = cn_under_test
            .get_namespace_channel(ns_root)
            .expect("get_namespace_channel should return a valid /pkg channel.");

        // Assert that the channel is valid, and the ContainerNamespace retains
        // a reference to the root channel as well.
        assert!(returned_channel.write(b"hello", &mut vec![]).is_ok());
        assert!(cn_under_test.has_channel_entry("/pkg"));

        // Assert that the remaining subdir returned is correct.
        assert_eq!(subdir, "");
    }

    #[::fuchsia::test]
    fn correctly_clones() {
        // Initialize with only the /pkg channel.
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        let data_handle = ClientEnd::new(pkg_channel);
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/pkg".to_string()),
            directory: Some(data_handle),
            ..Default::default()
        });

        let cn_under_test = ContainerNamespace::from(ns);
        let clone_under_test = cn_under_test.try_clone().expect("Clone should succeed.");

        // Assert both original and clone contain /pkg channel references,
        // and that those channels are both valid.
        let original_channel = cn_under_test
            .get_namespace_channel("/pkg")
            .expect("get_namespace_channel should return a valid /pkg channel.");
        let cloned_channel = clone_under_test
            .get_namespace_channel("/pkg")
            .expect("get_namespace_channel should return a valid /pkg channel.");
        assert!(original_channel.write(b"hello", &mut vec![]).is_ok());
        assert!(cloned_channel.write(b"hello", &mut vec![]).is_ok());

        // Assert both original and clone retain their own references.
        assert!(cn_under_test.has_channel_entry("/pkg"));
        assert!(clone_under_test.has_channel_entry("/pkg"));
    }

    #[::fuchsia::test]
    fn removes_namespace_channel() {
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        let pkg_channel: zx::Channel =
            directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                .expect("failed to open /pkg")
                .into_channel()
                .expect("into_channel")
                .into();
        ns.push(frunner::ComponentNamespaceEntry {
            path: Some("/tmp_lifecycle".to_string()),
            directory: Some(ClientEnd::new(pkg_channel)),
            ..Default::default()
        });

        let mut cn_under_test = ContainerNamespace::from(ns);
        assert!(cn_under_test.remove_namespace_channel("tmp_lifecycle").is_err());
        assert!(cn_under_test.has_channel_entry("/tmp_lifecycle"));
        let removed_channel = cn_under_test
            .remove_namespace_channel("/tmp_lifecycle")
            .expect("remove_namespace_channel should succeed");
        assert!(removed_channel.write(b"hello", &mut vec![]).is_ok());
        assert!(!cn_under_test.has_channel_entry("/tmp_lifecycle"));
        assert!(cn_under_test.remove_namespace_channel("/tmp_lifecycle").is_err());
    }

    #[::fuchsia::test]
    fn rejects_non_canonical_and_disallowed_paths() {
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let mut ns = Vec::<frunner::ComponentNamespaceEntry>::new();
        for path in ["/pkg", "/svc", "/tmp_lifecycle"] {
            let pkg_channel: zx::Channel =
                directory::open_in_namespace("/pkg", fio::PERM_READABLE | fio::PERM_EXECUTABLE)
                    .expect("failed to open /pkg")
                    .into_channel()
                    .expect("into_channel")
                    .into();
            ns.push(frunner::ComponentNamespaceEntry {
                path: Some(path.to_string()),
                directory: Some(ClientEnd::new(pkg_channel)),
                ..Default::default()
            });
        }
        let cn_under_test = ContainerNamespace::from(ns);

        for invalid in [
            "",
            "/",
            ".",
            "..",
            "pkg",
            "/pkg/",
            "//pkg",
            "/pkg//data",
            "/pkg/.",
            "/pkg/..",
            "/pkg/data/../meta",
            "/svc",
            "/svc/fuchsia.logger.LogSink",
            "/tmp_lifecycle",
            "/tmp_lifecycle/bootreason",
        ] {
            assert!(
                cn_under_test.find_closest_entry(invalid).is_err(),
                "expected find_closest_entry({invalid:?}) to fail"
            );
            for purpose in [
                NamespaceNodePurpose::DirectoryMount { read_only: true },
                NamespaceNodePurpose::DirectoryMount { read_only: false },
                NamespaceNodePurpose::VolumeKeys,
            ] {
                assert_eq!(
                    check_ns_dir_path(invalid, purpose),
                    Err(errno!(ENOENT)),
                    "expected check_ns_dir_path({invalid:?}, {purpose:?}) to fail with ENOENT"
                );
            }
        }
    }

    #[::fuchsia::test]
    fn find_closest_entry_resolves_without_cloning() {
        let _stub_exec = fuchsia_async::TestExecutor::new();
        let (data_client, data_server) = zx::Channel::create();
        let (fbe_keys_client, fbe_keys_server) = zx::Channel::create();
        let (foo_bar_client, foo_bar_server) = zx::Channel::create();
        let cn_under_test = ContainerNamespace::from(vec![
            frunner::ComponentNamespaceEntry {
                path: Some("/data".to_string()),
                directory: Some(ClientEnd::new(data_client)),
                ..Default::default()
            },
            frunner::ComponentNamespaceEntry {
                path: Some("/data/fbe_keys".to_string()),
                directory: Some(ClientEnd::new(fbe_keys_client)),
                ..Default::default()
            },
            frunner::ComponentNamespaceEntry {
                path: Some("/foo/bar".to_string()),
                directory: Some(ClientEnd::new(foo_bar_client)),
                ..Default::default()
            },
        ]);

        assert_eq!(cn_under_test.find_closest_entry("/data").unwrap(), ("/data", ""));
        assert_eq!(cn_under_test.find_closest_entry("/data/sub").unwrap(), ("/data", "sub"));
        assert_eq!(
            cn_under_test.find_closest_entry("/data/fbe_keys").unwrap(),
            ("/data/fbe_keys", "")
        );
        assert_eq!(
            cn_under_test.find_closest_entry("/data/fbe_keys/sub").unwrap(),
            ("/data/fbe_keys", "sub")
        );
        assert_eq!(
            cn_under_test.find_closest_entry("/data/fbe_keys_extra").unwrap(),
            ("/data", "fbe_keys_extra")
        );
        assert_eq!(cn_under_test.find_closest_entry("/foo/bar").unwrap(), ("/foo/bar", ""));
        assert_eq!(
            cn_under_test.find_closest_entry("/foo/bar/sub/leaf").unwrap(),
            ("/foo/bar", "sub/leaf")
        );
        assert!(cn_under_test.find_closest_entry("/foo").is_err());

        // Resolving namespace entries must not send any `Clone` messages on the channels.
        let mut buf = zx::MessageBuf::new();
        assert_eq!(data_server.read(&mut buf), Err(zx::Status::SHOULD_WAIT));
        assert_eq!(fbe_keys_server.read(&mut buf), Err(zx::Status::SHOULD_WAIT));
        assert_eq!(foo_bar_server.read(&mut buf), Err(zx::Status::SHOULD_WAIT));
    }

    #[::fuchsia::test]
    fn checks_ns_dir_path_by_purpose() {
        for denied_pkg_path in ["/pkg", "/pkg/data", "/pkg/meta", "/pkg/bin/starnix_kernel"] {
            for purpose in [
                NamespaceNodePurpose::DirectoryMount { read_only: true },
                NamespaceNodePurpose::DirectoryMount { read_only: false },
                NamespaceNodePurpose::VolumeKeys,
            ] {
                assert_eq!(
                    check_ns_dir_path(denied_pkg_path, purpose),
                    Err(errno!(EACCES)),
                    "expected check_ns_dir_path({denied_pkg_path:?}, {purpose:?}) to fail with EACCES"
                );
            }
        }

        for allowed_pkg_data_path in ["/pkg/data/system", "/pkg/data/system/subdir"] {
            assert_eq!(
                check_ns_dir_path(
                    allowed_pkg_data_path,
                    NamespaceNodePurpose::DirectoryMount { read_only: true },
                ),
                Ok(())
            );
            assert_eq!(
                check_ns_dir_path(
                    allowed_pkg_data_path,
                    NamespaceNodePurpose::DirectoryMount { read_only: false },
                ),
                Err(errno!(EACCES))
            );
            assert_eq!(
                check_ns_dir_path(allowed_pkg_data_path, NamespaceNodePurpose::VolumeKeys),
                Err(errno!(EACCES))
            );
        }

        for data_path in ["/data", "/data/subdir"] {
            for purpose in [
                NamespaceNodePurpose::DirectoryMount { read_only: true },
                NamespaceNodePurpose::DirectoryMount { read_only: false },
                NamespaceNodePurpose::VolumeKeys,
            ] {
                assert_eq!(check_ns_dir_path(data_path, purpose), Ok(()));
            }
        }
    }
}
