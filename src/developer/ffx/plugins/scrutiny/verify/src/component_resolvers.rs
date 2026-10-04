// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Result, anyhow};
use ffx_scrutiny_verify_args::component_resolvers::Command;
use scrutiny_frontend::verify::component_resolvers::{
    ComponentResolverRequest, ComponentResolverResponse,
};
use scrutiny_frontend::{Scrutiny, ScrutinyArtifacts};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

type Moniker = String;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct AllowListEntry {
    #[serde(flatten)]
    query: ComponentResolverRequest,
    components: Vec<Moniker>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
struct AllowList(Vec<AllowListEntry>);

impl AllowList {
    pub fn iter(&self) -> impl Iterator<Item = (ComponentResolverRequest, &[Moniker])> {
        self.0.iter().map(|entry| (entry.query.clone(), entry.components.as_slice()))
    }
}

/// A trait to query scrutiny's verify/component_resolvers API.
trait QueryComponentResolvers {
    /// Walk the v2 component tree, finding all components with a component resolver for `scheme`
    /// in its environment that has the given `moniker` and capability name `resolver`.
    fn query(
        &self,
        scheme: String,
        moniker: Moniker,
        resolver: String,
    ) -> Result<ComponentResolverResponse>;
}

/// An impl of [`QueryComponentResolvers`] that launches and queries scrutiny
/// for artifacts inside a product bundle.
struct ScrutinyQueryComponentResolvers {
    artifacts: ScrutinyArtifacts,
}

impl QueryComponentResolvers for ScrutinyQueryComponentResolvers {
    fn query(
        &self,
        scheme: String,
        moniker: Moniker,
        resolver: String,
    ) -> Result<ComponentResolverResponse> {
        self.artifacts.get_monikers_for_resolver(scheme, moniker, resolver)
    }
}

/// For each section of the provided `allowlist`, queries scrutiny for all components configured
/// with a component resolver for `scheme` with the given `moniker` and capability name `resolver`.
/// If any components match but are not in the allowlist, returns an allowlist that would allow all
/// found violations. On success, returns the set of files accessed to run the analysis, for
/// depfile generation.
fn verify_component_resolvers(
    scrutiny: impl QueryComponentResolvers,
    allowlist: AllowList,
) -> Result<Result<HashSet<PathBuf>, AllowList>> {
    let mut violations = vec![];
    let mut deps = HashSet::new();

    for (query, allowed_monikers) in allowlist.iter() {
        let allowed_monikers: HashSet<&Moniker> = allowed_monikers.iter().collect();

        let response = scrutiny
            .query(query.scheme.clone(), query.moniker.clone(), query.resolver.clone())
            .with_context(|| {
                format!("Failed to query verify.capability_component_resolvers with {:?}", query)
            })?;
        deps.extend(response.deps);

        let mut unexpected = vec![];

        for moniker in response.monikers {
            if !allowed_monikers.contains(&moniker) {
                unexpected.push(moniker);
            }
        }

        if !unexpected.is_empty() {
            violations.push(AllowListEntry { query, components: unexpected });
        }
    }

    if violations.is_empty() { Ok(Ok(deps)) } else { Ok(Err(AllowList(violations))) }
}

pub async fn verify(cmd: &Command, recovery: bool) -> Result<HashSet<PathBuf>> {
    let allowlist_path = &cmd.allowlist;

    let artifacts = if recovery {
        Scrutiny::from_product_bundle_recovery(&cmd.product_bundle)
    } else {
        Scrutiny::from_product_bundle(&cmd.product_bundle)
    }?
    .collect()?;
    let scrutiny = ScrutinyQueryComponentResolvers { artifacts };

    let allowlist: AllowList = serde_json5::from_str(
        &fs::read_to_string(&allowlist_path).context("Failed to read allowlist")?,
    )
    .context("Failed to deserialize allowlist")?;

    verify_component_resolvers(scrutiny, allowlist)?.map_err(|violations| {
        anyhow!(
            "
Static Component Resolver Capability Analysis Error:
The component resolver verifier found some components configured to be resolved using
a privileged component resolver.

If it is intended for these components to be resolved using the given resolver, add an entry
to the allowlist located at: {:?}

Verification Errors:
{}",
            allowlist_path,
            serde_json::to_string_pretty(&violations).unwrap()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use std::collections::HashMap;

    #[derive(Debug)]
    struct MockQueryComponentResolvers {
        responses: HashMap<(String, Moniker, String), String>,
    }

    impl MockQueryComponentResolvers {
        fn new() -> Self {
            Self { responses: HashMap::new() }
        }

        fn with_response(
            self,
            query: (String, Moniker, String),
            response: Vec<Moniker>,
            response_deps: Vec<String>,
        ) -> Self {
            let raw_response = serde_json::to_string(&ComponentResolverResponse {
                monikers: response,
                deps: response_deps.into_iter().map(PathBuf::from).collect(),
            })
            .unwrap();
            self.with_raw_response(query, raw_response)
        }

        fn with_raw_response(mut self, query: (String, Moniker, String), response: String) -> Self {
            self.responses.insert(query, response);
            self
        }
    }

    impl QueryComponentResolvers for MockQueryComponentResolvers {
        fn query(
            &self,
            scheme: String,
            moniker: Moniker,
            resolver: String,
        ) -> Result<ComponentResolverResponse> {
            let key = (scheme, moniker, resolver);

            let response = self
                .responses
                .get(&key)
                .unwrap_or_else(|| panic!("mock to be configured for key {:?}", key));

            Ok(serde_json5::from_str(&response).context(format!(
                "Failed to deserialize verify component resolvers results: {:?}",
                response
            ))?)
        }
    }

    fn parse_allowlist(raw: &str) -> AllowList {
        let mut allowlist: AllowList = serde_json5::from_str(raw).unwrap();

        for entry in allowlist.0.iter_mut() {
            entry.components.sort_unstable();
        }
        allowlist.0.sort_unstable();

        allowlist
    }

    #[test]
    fn fails_on_invalid_response() {
        let allowlist = parse_allowlist(
            r#"[
            {
                scheme: "test-scheme",
                moniker: "/foo/bar",
                resolver: "my_resolver",
                components: [
                ],
            },
        ]"#,
        );

        let scrutiny = MockQueryComponentResolvers::new().with_raw_response(
            ("test-scheme".to_owned(), "/foo/bar".to_owned(), "my_resolver".to_owned()),
            "invalid".to_owned(),
        );

        assert_matches!(verify_component_resolvers(scrutiny, allowlist), Err(_));
    }

    #[test]
    fn reports_unexpected_entry() {
        let allowlist = parse_allowlist(
            r#"[
            {
                scheme: "test-scheme",
                moniker: "/foo/bar",
                resolver: "my_resolver",
                components: [
                    "/baz/allowed",
                ],
            },
        ]"#,
        );

        let violations = parse_allowlist(
            r#"[
            {
                scheme: "test-scheme",
                moniker: "/foo/bar",
                resolver: "my_resolver",
                components: [
                    "/baz/stopme",
                ],
            },
        ]"#,
        );

        let scrutiny = MockQueryComponentResolvers::new().with_response(
            ("test-scheme".to_owned(), "/foo/bar".to_owned(), "my_resolver".to_owned()),
            vec!["/baz/allowed".to_owned(), "/baz/stopme".to_owned()],
            vec!["path/to/dep.zbi".to_owned()],
        );

        assert_eq!(verify_component_resolvers(scrutiny, allowlist).unwrap(), Err(violations));
    }

    #[test]
    fn ignores_unused_allow() {
        let allowlist = parse_allowlist(
            r#"[
            {
                scheme: "test-scheme",
                moniker: "/foo/bar",
                resolver: "my_resolver",
                components: [
                    "/baz/allowed",
                    "/baz/also-allowed",
                ],
            },
        ]"#,
        );

        let scrutiny = MockQueryComponentResolvers::new().with_response(
            ("test-scheme".to_owned(), "/foo/bar".to_owned(), "my_resolver".to_owned()),
            vec!["/baz/allowed".to_owned(), "/baz/also-allowed".to_owned()],
            vec!["path/to/dep.zbi".to_owned()],
        );

        let expected_deps = vec!["path/to/dep.zbi".to_string().into()].into_iter().collect();
        assert_eq!(verify_component_resolvers(scrutiny, allowlist).unwrap(), Ok(expected_deps));
    }

    #[test]
    fn checks_all_entries() {
        let allowlist = parse_allowlist(
            r#"[
            {
                scheme: "a",
                moniker: "/foo/resolver-a",
                resolver: "resolver-a",
                components: [
                    "/foo/allowed-a",
                ],
            },
            {
                scheme: "b",
                moniker: "/foo/resolver-b",
                resolver: "resolver-b",
                components: [
                    "/foo/allowed-b",
                ],
            },
            {
                scheme: "c",
                moniker: "/foo/resolver-c",
                resolver: "resolver-c",
                components: [
                    "/foo/allowed-c",
                ],
            },
        ]"#,
        );

        let violations = parse_allowlist(
            r#"[
            {
                scheme: "a",
                moniker: "/foo/resolver-a",
                resolver: "resolver-a",
                components: [
                    "/foo/violation-a",
                ],
            },
            {
                scheme: "c",
                moniker: "/foo/resolver-c",
                resolver: "resolver-c",
                components: [
                    "/foo/violation-c",
                ],
            },
        ]"#,
        );

        let scrutiny = MockQueryComponentResolvers::new()
            .with_response(
                ("a".to_owned(), "/foo/resolver-a".to_owned(), "resolver-a".to_owned()),
                vec!["/foo/allowed-a".to_owned(), "/foo/violation-a".to_owned()],
                vec!["dep1".to_owned()],
            )
            .with_response(
                ("b".to_owned(), "/foo/resolver-b".to_owned(), "resolver-b".to_owned()),
                vec!["/foo/allowed-b".to_owned()],
                vec!["dep2".to_owned()],
            )
            .with_response(
                ("c".to_owned(), "/foo/resolver-c".to_owned(), "resolver-c".to_owned()),
                vec!["/foo/allowed-c".to_owned(), "/foo/violation-c".to_owned()],
                vec!["dep3".to_owned()],
            );

        assert_eq!(verify_component_resolvers(scrutiny, allowlist).unwrap(), Err(violations));
    }
}
