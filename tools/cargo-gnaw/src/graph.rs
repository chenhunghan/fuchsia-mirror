// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::target::GnTarget;
use crate::types::*;
use anyhow::{Context as _, Result, anyhow};
use cargo_metadata::{DependencyKind, Metadata, Node, Package, PackageId};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::convert::TryFrom;

const KERNEL_PLATFORM_CFG: &str = "cfg(kernel)";
const NON_KERNEL_PLATFORM_CFG: &str = "cfg(not(kernel))";

/// Represents a resolved normal dependency edge `(platform, package, extern_crate_name)`.
#[derive(Clone, Debug)]
struct DepEdge<'a> {
    platform: Option<Platform>,
    pkg: &'a Package,
    extern_name: String,
}

impl PartialEq for DepEdge<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.platform == other.platform
            && self.pkg.id == other.pkg.id
            && self.extern_name == other.extern_name
    }
}

impl Eq for DepEdge<'_> {}

impl PartialOrd for DepEdge<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DepEdge<'_> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.platform
            .cmp(&other.platform)
            .then_with(|| self.pkg.id.cmp(&other.pkg.id))
            .then_with(|| self.extern_name.cmp(&other.extern_name))
    }
}

/// Holds Cargo's resolved feature sets and normal dependency edges for a single environment.
struct EnvResolve<'a> {
    reachable: HashSet<PackageId>,
    features: HashMap<PackageId, BTreeSet<Feature>>,
    deps: HashMap<PackageId, BTreeSet<DepEdge<'a>>>,
}

impl<'a> EnvResolve<'a> {
    fn from_kernel_metadata(main_metadata: &'a Metadata, kernel_metadata: &Metadata) -> Self {
        let mut reachable = HashSet::new();
        let mut features = HashMap::new();
        let mut deps = HashMap::<PackageId, BTreeSet<DepEdge<'a>>>::new();

        let Some(resolve) = kernel_metadata.resolve.as_ref() else {
            return Self { reachable, features, deps };
        };
        let Some(root_id) = resolve.root.as_ref() else {
            return Self { reachable, features, deps };
        };
        let node_map: HashMap<&PackageId, &Node> =
            resolve.nodes.iter().map(|n| (&n.id, n)).collect();
        let pkg_by_id: HashMap<&PackageId, &'a Package> =
            main_metadata.packages.iter().map(|p| (&p.id, p)).collect();

        // Walk normal dependency edges starting at root, stopping before recursing inside
        // proc-macros (which always compile on `$host_toolchain`, i.e., `!is_kernel`).
        let mut queue = vec![root_id];
        while let Some(curr_id) = queue.pop() {
            let Some(node) = node_map.get(curr_id) else {
                continue;
            };
            for nd in &node.deps {
                for dk in &nd.dep_kinds {
                    if dk.kind == DependencyKind::Normal
                        && let Some(&dep_pkg) = pkg_by_id.get(&nd.pkg)
                    {
                        if curr_id != root_id {
                            let platform = dk.target.as_ref().map(|t| format!("{}", t));
                            let _ = deps.entry(curr_id.clone()).or_default().insert(DepEdge {
                                platform,
                                pkg: dep_pkg,
                                extern_name: nd.name.clone(),
                            });
                        }
                        if !dep_pkg.is_proc_macro() && reachable.insert(nd.pkg.clone()) {
                            queue.push(&nd.pkg);
                        }
                    }
                }
            }
        }

        for pkg_id in &reachable {
            if let Some(node) = node_map.get(pkg_id) {
                let _ = features.insert(pkg_id.clone(), node.features.iter().cloned().collect());
            }
        }

        Self { reachable, features, deps }
    }

    fn from_userspace_metadata(main_metadata: &'a Metadata, userspace_metadata: &Metadata) -> Self {
        let mut reachable = HashSet::new();
        let mut features = HashMap::new();
        let mut deps = HashMap::<PackageId, BTreeSet<DepEdge<'a>>>::new();

        let Some(resolve) = userspace_metadata.resolve.as_ref() else {
            return Self { reachable, features, deps };
        };
        let root_id = resolve.root.as_ref();
        let pkg_by_id: HashMap<&PackageId, &'a Package> =
            main_metadata.packages.iter().map(|p| (&p.id, p)).collect();

        for node in &resolve.nodes {
            if root_id == Some(&node.id) {
                continue;
            }
            let _ = reachable.insert(node.id.clone());
            let _ = features.insert(node.id.clone(), node.features.iter().cloned().collect());
            for nd in &node.deps {
                for dk in &nd.dep_kinds {
                    if dk.kind == DependencyKind::Normal
                        && let Some(&dep_pkg) = pkg_by_id.get(&nd.pkg)
                    {
                        let platform = dk.target.as_ref().map(|t| format!("{}", t));
                        let _ = deps.entry(node.id.clone()).or_default().insert(DepEdge {
                            platform,
                            pkg: dep_pkg,
                            extern_name: nd.name.clone(),
                        });
                    }
                }
            }
        }

        Self { reachable, features, deps }
    }
}

fn combine_platform_cfg(env_cfg: &str, dep_platform: Option<&str>) -> Platform {
    match dep_platform {
        None => env_cfg.to_string(),
        Some(plat) => {
            let env_inner =
                env_cfg.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')).unwrap_or(env_cfg);
            let dep_inner =
                plat.strip_prefix("cfg(").and_then(|s| s.strip_suffix(')')).unwrap_or(plat);
            format!("cfg(all({}, {}))", env_inner, dep_inner)
        }
    }
}

pub struct GnBuildGraph<'a> {
    metadata: &'a Metadata,
    // hashset because the same target can get added multiple times
    targets: HashSet<GnTarget<'a>>,
    visited: HashSet<PackageId>,
    split_envs: Option<(EnvResolve<'a>, EnvResolve<'a>)>,
}

impl<'a> GnBuildGraph<'a> {
    pub fn new(metadata: &'a Metadata, split_metadata: Option<(&Metadata, &Metadata)>) -> Self {
        let split_envs = split_metadata.map(|(k_meta, u_meta)| {
            (
                EnvResolve::from_kernel_metadata(metadata, k_meta),
                EnvResolve::from_userspace_metadata(metadata, u_meta),
            )
        });
        GnBuildGraph { metadata, targets: HashSet::new(), visited: HashSet::new(), split_envs }
    }

    pub fn targets(&'a self) -> impl Iterator<Item = &'a GnTarget<'a>> {
        self.targets.iter()
    }

    pub fn find_library_target(&self, package: &str, version: &str) -> Option<&GnTarget<'_>> {
        self.targets().find(|t| match t.target_type {
            GnRustType::Library
            | GnRustType::Rlib
            | GnRustType::Staticlib
            | GnRustType::Dylib
            | GnRustType::Cdylib
            | GnRustType::ProcMacro => t.pkg_name == package && t.version() == version,
            _ => false,
        })
    }

    pub fn find_binary_target(
        &self,
        package: &str,
        version: &str,
        target: &str,
    ) -> Option<&GnTarget<'_>> {
        self.targets().find(|t| match t.target_type {
            GnRustType::Binary => {
                t.pkg_name == package && t.version() == version && t.target_name == target
            }
            _ => false,
        })
    }

    fn resolve_target_features_and_deps(
        &self,
        pkg_id: &PackageId,
        default_features: &[Feature],
        default_deps: HashMap<Option<Platform>, Vec<(&'a Package, String)>>,
    ) -> (
        Vec<Feature>,
        BTreeMap<Platform, Vec<Feature>>,
        HashMap<Option<Platform>, Vec<(&'a Package, String)>>,
    ) {
        let Some((ref kernel_env, ref userspace_env)) = self.split_envs else {
            return (default_features.to_vec(), BTreeMap::new(), default_deps);
        };

        let empty_feats = BTreeSet::new();
        let empty_edges = BTreeSet::new();

        // Purely non-kernel / host package: unaffected by `cfg(kernel)`.
        if !kernel_env.reachable.contains(pkg_id) {
            return (default_features.to_vec(), BTreeMap::new(), default_deps);
        }

        let k_feats = kernel_env.features.get(pkg_id).unwrap_or(&empty_feats);
        let k_edges = kernel_env.deps.get(pkg_id).unwrap_or(&empty_edges);

        // Purely kernel package: emit Cargo's kernel-resolved features & deps.
        if !userspace_env.reachable.contains(pkg_id) {
            let mut deps = HashMap::<Option<Platform>, Vec<(&'a Package, String)>>::new();
            for edge in k_edges {
                deps.entry(edge.platform.clone())
                    .or_default()
                    .push((edge.pkg, edge.extern_name.clone()));
            }
            return (k_feats.iter().cloned().collect(), BTreeMap::new(), deps);
        }

        // Package shared between `is_kernel` and `!is_kernel`: partition Cargo's two resolutions.
        let u_feats = userspace_env.features.get(pkg_id).unwrap_or(&empty_feats);
        let u_edges = userspace_env.deps.get(pkg_id).unwrap_or(&empty_edges);

        let common_features: Vec<Feature> = k_feats.intersection(u_feats).cloned().collect();
        let kernel_only_features: Vec<Feature> = k_feats.difference(u_feats).cloned().collect();
        let userspace_only_features: Vec<Feature> = u_feats.difference(k_feats).cloned().collect();

        let mut platform_features = BTreeMap::new();
        if !kernel_only_features.is_empty() {
            let _ = platform_features.insert(KERNEL_PLATFORM_CFG.to_string(), kernel_only_features);
        }
        if !userspace_only_features.is_empty() {
            let _ = platform_features
                .insert(NON_KERNEL_PLATFORM_CFG.to_string(), userspace_only_features);
        }

        let mut deps = HashMap::<Option<Platform>, Vec<(&'a Package, String)>>::new();
        for edge in k_edges.intersection(u_edges) {
            deps.entry(edge.platform.clone())
                .or_default()
                .push((edge.pkg, edge.extern_name.clone()));
        }
        for edge in k_edges.difference(u_edges) {
            let plat = Some(combine_platform_cfg(KERNEL_PLATFORM_CFG, edge.platform.as_deref()));
            deps.entry(plat).or_default().push((edge.pkg, edge.extern_name.clone()));
        }
        for edge in u_edges.difference(k_edges) {
            let plat =
                Some(combine_platform_cfg(NON_KERNEL_PLATFORM_CFG, edge.platform.as_deref()));
            deps.entry(plat).or_default().push((edge.pkg, edge.extern_name.clone()));
        }

        (common_features, platform_features, deps)
    }

    /// Add a cargo package to the target list. If the dependencies
    /// are not already in the target graph, add them as well
    pub fn add_cargo_package(&mut self, cargo_pkg_id: PackageId) -> Result<()> {
        if !self.visited.insert(cargo_pkg_id.clone()) {
            return Ok(());
        }

        let package = &self.metadata[&cargo_pkg_id];
        for node in self
            .metadata
            .resolve
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .filter(|node| node.id == cargo_pkg_id)
        {
            let mut dependencies = HashMap::<Option<Platform>, Vec<(&'a Package, String)>>::new();
            let mut dev_dependencies =
                HashMap::<Option<Platform>, Vec<(&'a Package, String)>>::new();

            // collect the dependency edges for this node
            for node_dep in node.deps.iter() {
                let id = &node_dep.pkg;
                if node_dep.dep_kinds.is_empty() {
                    return Err(anyhow!(
                        "Must use rustc 1.41+ to use cargo-gnaw. Needs dependency kind information."
                    ));
                }
                for kinds in &node_dep.dep_kinds {
                    let kind = &kinds.kind;
                    let target = &kinds.target;
                    match kind {
                        DependencyKind::Normal => {
                            self.add_cargo_package(id.clone())?;
                            let platform = target.as_ref().map(|x| format!("{}", x));
                            let platform_deps = dependencies.entry(platform).or_default();

                            // Somehow we get duplicates of the same dependency
                            // from cargo on some targets. Filter those out
                            // here. This is technically quadratic, but that's
                            // unlikely to matter for the number of deps on a
                            // single crate.
                            if !platform_deps.iter().any(|dep| dep.1 == node_dep.name.clone()) {
                                platform_deps.push((&self.metadata[id], node_dep.name.clone()));
                            }
                        }
                        DependencyKind::Build => {}
                        DependencyKind::Development => {
                            self.add_cargo_package(id.clone())?;
                            let platform = target.as_ref().map(|x| format!("{}", x));
                            let platform_deps = dev_dependencies.entry(platform).or_default();

                            if !platform_deps.iter().any(|dep| dep.1 == node_dep.name.clone()) {
                                platform_deps.push((&self.metadata[id], node_dep.name.clone()));
                            }
                        }
                        err => {
                            return Err(anyhow!(
                                "Don't know how to handle this kind of dependency edge: {:?}",
                                err
                            ));
                        }
                    }
                }
            }

            // Also collect dev-dependencies declared on package.dependencies that may not be in node.deps
            // (Cargo resolve node graph omits dev-dependencies for non-workspace members)
            for dep in &package.dependencies {
                if dep.kind == DependencyKind::Development {
                    let dep_crate_name =
                        dep.rename.clone().unwrap_or_else(|| dep.name.replace("-", "_"));
                    let platform = dep.target.as_ref().map(|x| format!("{}", x));
                    let platform_deps = dev_dependencies.entry(platform).or_default();

                    if !platform_deps.iter().any(|d| d.1 == dep_crate_name)
                        && let Some(target_pkg) = self
                            .metadata
                            .packages
                            .iter()
                            .find(|p| p.name == dep.name && dep.req.matches(&p.version))
                    {
                        self.add_cargo_package(target_pkg.id.clone())?;
                        platform_deps.push((target_pkg, dep_crate_name));
                    }
                }
            }

            let has_build_script = package.targets.iter().any(|target| {
                for kind in &target.kind {
                    let gn_rust_type = GnRustType::try_from(kind)
                        .with_context(|| {
                            format!("Failed to resolve GN target type for: {:?}", target)
                        })
                        .unwrap();

                    if gn_rust_type == GnRustType::BuildScript {
                        return true;
                    }
                }

                false
            });

            let (resolved_features, resolved_platform_features, resolved_dependencies) = self
                .resolve_target_features_and_deps(&node.id, node.features.as_slice(), dependencies);

            for rust_target in package.targets.iter() {
                for kind in &rust_target.kind {
                    let target_type = GnRustType::try_from(kind)?;
                    match target_type {
                        GnRustType::Library | GnRustType::Rlib | GnRustType::ProcMacro => {
                            let gn_target = GnTarget::with_platform_features(
                                &node.id,
                                &rust_target.name,
                                &package.name,
                                package.edition,
                                &rust_target.src_path,
                                &package.version,
                                target_type,
                                resolved_features.clone(),
                                resolved_platform_features.clone(),
                                has_build_script,
                                resolved_dependencies.clone(),
                                dev_dependencies.clone(),
                            );
                            let _ = self.targets.insert(gn_target);
                        }
                        GnRustType::Binary => {
                            // If a crate contains both a library and binary targets,
                            // cargo implicitly adds a dependency from each binary to the library.
                            //
                            // This logic imitates that behaviour.
                            let mut deps = resolved_dependencies.clone();
                            let maybe_library = package.targets.iter().find(|v| {
                                v.kind.iter().any(|kind| {
                                    matches!(GnRustType::try_from(kind), Ok(GnRustType::Library))
                                })
                            });
                            if let Some(lib) = maybe_library {
                                deps.entry(None).or_default().push((
                                    &self.metadata[&node.id],
                                    lib.name.clone().replace('-', "_"),
                                ));
                            }

                            let gn_target = GnTarget::with_platform_features(
                                &node.id,
                                &rust_target.name,
                                &package.name,
                                package.edition,
                                &rust_target.src_path,
                                &package.version,
                                target_type,
                                resolved_features.clone(),
                                resolved_platform_features.clone(),
                                has_build_script,
                                deps,
                                dev_dependencies.clone(),
                            );
                            let _ = self.targets.insert(gn_target);
                        }

                        // FIXME(https://fxbug.dev/42173393): support staticlib, dylib, and
                        // cdylib crate types.
                        GnRustType::Staticlib | GnRustType::Dylib | GnRustType::Cdylib => (),

                        // BuildScripts are handled as part of the targets
                        GnRustType::BuildScript => (),

                        // TODO support building tests. Should integrate with whatever GN
                        // metadata collection system used by the main build.
                        GnRustType::Example | GnRustType::Bench | GnRustType::Test => (),
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_combine_platform_cfg() {
        assert_eq!(combine_platform_cfg("cfg(kernel)", None), "cfg(kernel)");
        assert_eq!(combine_platform_cfg("cfg(not(kernel))", None), "cfg(not(kernel))");
        assert_eq!(
            combine_platform_cfg("cfg(not(kernel))", Some("cfg(target_os = \"fuchsia\")")),
            "cfg(all(not(kernel), target_os = \"fuchsia\"))"
        );
        assert_eq!(
            combine_platform_cfg("cfg(kernel)", Some("cfg(target_arch = \"aarch64\")")),
            "cfg(all(kernel, target_arch = \"aarch64\"))"
        );
    }
}
