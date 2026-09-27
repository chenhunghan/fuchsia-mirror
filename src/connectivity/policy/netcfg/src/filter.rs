// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashSet;

use fidl_fuchsia_net_filter as fnet_filter;
use fidl_fuchsia_net_filter_ext::{
    self as fnet_filter_ext, Action, Change, CommitError, Domain, InstalledIpRoutine,
    InstalledNatRoutine, IpHook, Matchers, Namespace, NamespaceId, NatHook, PushChangesError,
    Resource, ResourceId, Routine, RoutineId, RoutineType, Rule, RuleId,
};
use fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext;
use fidl_fuchsia_net_matchers_ext as fnet_matchers_ext;

use anyhow::Context as _;
use log::info;

use crate::{FilterConfig, InterfaceType};

/// An error observed on the `fuchsia.net.filter` API.
#[derive(Debug)]
pub(crate) enum FilterError {
    Push(PushChangesError),
    Commit(CommitError),
}

// Filtering state on the current `fuchsia.net.filter` API.
pub(crate) struct FilterControl {
    controller: fnet_filter_ext::Controller,
    masquerade: MasqueradeState,
    // TODO(https://fxbug.dev/331469354): Add NAT routines when this
    // functionality has been added to fuchsia.net.filter.
}

impl FilterControl {
    pub(super) async fn new(proxy: fnet_filter::ControlProxy) -> Result<Self, anyhow::Error> {
        let controller_id = fnet_filter_ext::ControllerId(String::from("netcfg"));
        Ok(Self {
            controller: fnet_filter_ext::Controller::new(&proxy, &controller_id)
                .await
                .context("could not create controller from filter proxy")?,
            masquerade: MasqueradeState { routine_id: masquerade_routine(), next_rule_index: 0 },
        })
    }

    /// Updates the initial network filter configuration using
    /// fuchsia.net.filter.
    pub(super) async fn update_filters(
        &mut self,
        config: FilterConfig,
        filter_enabled_interface_types: &HashSet<InterfaceType>,
    ) -> Result<(), anyhow::Error> {
        let Self { controller, masquerade } = self;
        let uninstalled_ip_routines = filter_routines(false /* installed */);
        let installed_ip_routines = filter_routines(true /* installed */);
        let changes = generate_initial_filter_changes(
            &uninstalled_ip_routines,
            &installed_ip_routines,
            &masquerade.routine_id,
            config,
            filter_enabled_interface_types,
        )?;

        for batch in changes.chunks(usize::from(fnet_filter::MAX_BATCH_SIZE)) {
            controller
                .push_changes(batch.to_vec())
                .await
                .context("failed to push changes to filter controller")?;
        }

        controller.commit().await.context("failed to commit changes to filter controller")?;
        info!("initial filter configuration has been committed successfully");
        Ok(())
    }
}

// Filtering state for Masquerade NAT on the `fuchsia.net.filter` API.
struct MasqueradeState {
    // The routine that holds all masquerade rules.
    routine_id: RoutineId,
    // The index to use for the next masquerade rule.
    //
    // Note: By using a simple counter, we don't reuse indices that were once
    // used but are now available. The upside to this approach is that all
    // filtering config has a stable order: older filtering config will always
    // have a lower index (and therefore a higher priority) than newer filtering
    // config. On the other hand, we do run the risk of overflowing the index
    // if Netcfg were to add/remove u32::MAX filtering rules. That should only
    // happen under pathological circumstances, and thus is a non-concern.
    next_rule_index: u32,
}

// Netcfg's `FilterRoutines` to maintain the same namespace
// and routine for each of the filter `Rule`s across installed
// and uninstalled routines.
fn filter_routines(installed: bool) -> netfilter::parser::FilterRoutines {
    let suffix = if !installed { "_uninstalled" } else { "" };
    netfilter::parser::FilterRoutines {
        local_ingress: Some(RoutineId {
            namespace: namespace_id(),
            name: format!("local_ingress{suffix}"),
        }),
        local_egress: Some(RoutineId {
            namespace: namespace_id(),
            name: format!("local_egress{suffix}"),
        }),
    }
}

// Netcfg's masquerade NAT `RoutineId`.
//
// Masquerade NAT rules are always installed at the EGRESS hook.
fn masquerade_routine() -> RoutineId {
    RoutineId { namespace: namespace_id(), name: format!("egress_masquerade") }
}

fn namespace_id() -> NamespaceId {
    NamespaceId(String::from("netcfg"))
}

fn get_enabled_port_classes(
    interface_types: &HashSet<InterfaceType>,
) -> HashSet<fnet_interfaces_ext::PortClass> {
    let mut port_classes = HashSet::new();
    for interface_type in interface_types {
        port_classes.extend(interface_type.port_classes());
        // An AP device can be filtered by specifying AP or WLAN.
        if *interface_type == InterfaceType::WlanClient {
            let _replaced: bool = port_classes.insert(fnet_interfaces_ext::PortClass::WlanAp);
        }
    }
    return port_classes;
}

// Create a list of `fnet_filter_ext::Change`s that, when used with
// `fnet_filter_ext::Controller`, will initialize the filter namespace,
// routines, and rules for netcfg.
fn generate_initial_filter_changes(
    uninstalled_ip_routines: &netfilter::parser::FilterRoutines,
    installed_ip_routines: &netfilter::parser::FilterRoutines,
    masquerade_routine: &RoutineId,
    config: FilterConfig,
    filter_enabled_interface_types: &HashSet<InterfaceType>,
) -> Result<Vec<Change>, anyhow::Error> {
    let mut changes = Vec::new();
    let namespace = Namespace { id: namespace_id(), domain: Domain::AllIp };
    changes.push(Change::Create(Resource::Namespace(namespace)));

    // Push uninstalled routines first so that installed routines will have
    // a reference to an existing routine when they are installed and will
    // `Jump` to `Rule`s. There must be a separate uninstalled `Routine` for
    // each `IpHook` so that there are not issues with a `Rule` containing
    // a matcher that is not allowed in the installed `Routine`'s hook.
    // E.g., A `Rule` in an installed ingress hook `Routine` that `Jump`s to a
    // `Routine` with a `Rule` that specifies an out_interface matcher is not
    // permitted.
    let netfilter::parser::FilterRoutines { local_ingress, local_egress } = uninstalled_ip_routines;
    let uninstalled_local_ingress =
        local_ingress.clone().map(|id| Routine { id, routine_type: RoutineType::Ip(None) });
    let uninstalled_local_egress =
        local_egress.clone().map(|id| Routine { id, routine_type: RoutineType::Ip(None) });

    // Push installed routines so that netcfg can install `Jump` rules
    // rooted in these routines.
    fn installed_routine_from_id(id: RoutineId, hook: IpHook) -> Routine {
        Routine {
            id: id,
            routine_type: RoutineType::Ip(Some(InstalledIpRoutine { hook, priority: 0i32 })),
        }
    }
    let netfilter::parser::FilterRoutines { local_ingress, local_egress } = installed_ip_routines;
    let local_ingress =
        local_ingress.clone().map(|id| installed_routine_from_id(id, IpHook::LocalIngress));
    let local_egress =
        local_egress.clone().map(|id| installed_routine_from_id(id, IpHook::LocalEgress));

    let masquerade = Routine {
        id: masquerade_routine.clone(),
        routine_type: RoutineType::Nat(Some(InstalledNatRoutine {
            hook: NatHook::Egress,
            priority: 0i32,
        })),
    };

    let routine_changes = [
        uninstalled_local_ingress,
        local_ingress,
        uninstalled_local_egress,
        local_egress,
        Some(masquerade),
    ]
    .into_iter()
    .filter_map(|routine| routine)
    .map(|routine| Change::Create(Resource::Routine(routine)));
    changes.extend(routine_changes);

    // TODO(https://fxbug.dev/331469354): Handle NAT and NAT RDR rules when supported
    // by netfilter and filtering library
    let FilterConfig { rules, nat_rules: _, rdr_rules: _ } = config;
    if !rules.is_empty() {
        // Only insert the rules from the config into the uninstalled routine.
        // The rules inserted in the installed routines will be intended for
        // redirection to the uninstalled routines.
        let rules =
            netfilter::parser::parse_str_to_rules(&rules.join(""), &uninstalled_ip_routines)
                .context("error parsing filter rules")?;
        let rule_changes = rules.into_iter().map(|rule| Change::Create(Resource::Rule(rule)));
        changes.extend(rule_changes);
    }

    for (i, port_class) in
        get_enabled_port_classes(filter_enabled_interface_types).into_iter().enumerate()
    {
        let port_class_rules = generate_static_port_class_filter_rules(
            uninstalled_ip_routines,
            installed_ip_routines,
            port_class,
            u32::try_from(i).expect("rule index overflowed u32"),
        );
        changes
            .extend(port_class_rules.into_iter().map(|rule| Change::Create(Resource::Rule(rule))));
    }

    Ok(changes)
}

fn create_jump_rule(
    routine_id: RoutineId,
    index: u32,
    interface: fnet_matchers_ext::Interface,
    hook: IpHook,
    target_routine_name: &str,
) -> Rule {
    let (in_interface, out_interface) = match hook {
        IpHook::LocalIngress | IpHook::Ingress => (Some(interface), None),
        IpHook::LocalEgress | IpHook::Egress => (None, Some(interface)),
        IpHook::Forwarding => (Some(interface.clone()), Some(interface)),
    };

    Rule {
        id: RuleId { routine: routine_id, index },
        matchers: Matchers { in_interface, out_interface, ..Default::default() },
        action: Action::Jump(target_routine_name.to_string()),
    }
}

/// Generates static filter rules (jump rules) for a given `PortClass` to
/// redirect traffic to uninstalled routines.
fn generate_static_port_class_filter_rules(
    uninstalled_ip_routines: &netfilter::parser::FilterRoutines,
    installed_ip_routines: &netfilter::parser::FilterRoutines,
    port_class: fnet_interfaces_ext::PortClass,
    current_installed_rule_index: u32,
) -> Vec<Rule> {
    let netfilter::parser::FilterRoutines {
        local_ingress: uninstalled_local_ingress,
        local_egress: uninstalled_local_egress,
    } = uninstalled_ip_routines;
    let netfilter::parser::FilterRoutines { local_ingress, local_egress } = installed_ip_routines;

    let local_ingress_rule = local_ingress.clone().map(|routine_id| {
        create_port_class_matching_jump_rule(
            routine_id,
            current_installed_rule_index,
            port_class,
            IpHook::LocalIngress,
            &uninstalled_local_ingress
                .as_ref()
                .expect("there should be a corresponding uninstalled routine for local ingress")
                .name,
        )
    });
    let local_egress_rule = local_egress.clone().map(|routine_id| {
        create_port_class_matching_jump_rule(
            routine_id,
            current_installed_rule_index,
            port_class,
            IpHook::LocalEgress,
            &uninstalled_local_egress
                .as_ref()
                .expect("there should be a corresponding uninstalled routine for local egress")
                .name,
        )
    });

    [local_ingress_rule, local_egress_rule].into_iter().flatten().collect()
}

/// Helper to create a single rule matching a `PortClass` on input/output interfaces depending on
/// the hook.
fn create_port_class_matching_jump_rule(
    routine_id: RoutineId,
    index: u32,
    port_class: fnet_interfaces_ext::PortClass,
    hook: IpHook,
    target_routine_name: &str,
) -> Rule {
    create_jump_rule(
        routine_id,
        index,
        fnet_matchers_ext::Interface::PortClass(port_class),
        hook,
        target_routine_name,
    )
}

// Attempts to add a new masquerade NAT rule using `fuchsia.net.filter`.
pub(crate) async fn add_masquerade_rule(
    filter: &mut FilterControl,
    matchers: Matchers,
) -> Result<RuleId, FilterError> {
    let MasqueradeState { routine_id, next_rule_index } = &mut filter.masquerade;
    let rule_id = RuleId { routine: routine_id.clone(), index: *next_rule_index };
    let rule_changes = vec![Change::Create(Resource::Rule(Rule {
        id: rule_id.clone(),
        matchers: matchers,
        action: Action::Masquerade { src_port: None },
    }))];
    filter.controller.push_changes(rule_changes).await.map_err(FilterError::Push)?;
    filter.controller.commit().await.map_err(FilterError::Commit)?;
    *next_rule_index += 1;
    Ok(rule_id)
}

// Attempts to remove an existing masquerade NAT rule using `fuchsia.net.filter`.
pub(crate) async fn remove_masquerade_rule(
    filter: &mut FilterControl,
    rule: &RuleId,
) -> Result<(), FilterError> {
    let rule_changes = vec![Change::Remove(ResourceId::Rule(rule.clone()))];
    filter.controller.push_changes(rule_changes).await.map_err(FilterError::Push)?;
    filter.controller.commit().await.map_err(FilterError::Commit)
}

#[cfg(test)]
mod tests {
    use futures::StreamExt as _;
    use test_case::test_case;

    use super::*;

    const LOCAL_INGRESS: &str = "local_ingress";
    const UNINSTALLED_LOCAL_INGRESS: &str = "local_ingress_uninstalled";
    const LOCAL_EGRESS: &str = "local_egress";
    const UNINSTALLED_LOCAL_EGRESS: &str = "local_egress_uninstalled";
    const MASQUERADE: &str = "egress_masquerade";

    fn get_foundational_changes() -> Vec<Change> {
        let mut changes = vec![Change::Create(Resource::Namespace(Namespace {
            id: namespace_id(),
            domain: Domain::AllIp,
        }))];

        let local_ingress = (LOCAL_INGRESS, UNINSTALLED_LOCAL_INGRESS, IpHook::LocalIngress);
        let local_egress = (LOCAL_EGRESS, UNINSTALLED_LOCAL_EGRESS, IpHook::LocalEgress);

        let routine_changes = vec![local_ingress, local_egress]
            .into_iter()
            .map(|(installed_name, uninstalled_name, hook)| {
                vec![
                    Routine {
                        id: RoutineId {
                            namespace: namespace_id(),
                            name: String::from(uninstalled_name),
                        },
                        routine_type: RoutineType::Ip(None),
                    },
                    Routine {
                        id: RoutineId {
                            namespace: namespace_id(),
                            name: String::from(installed_name),
                        },
                        routine_type: RoutineType::Ip(Some(InstalledIpRoutine {
                            hook,
                            priority: 0i32,
                        })),
                    },
                ]
            })
            .flatten()
            .chain([Routine {
                id: RoutineId { namespace: namespace_id(), name: String::from(MASQUERADE) },
                routine_type: RoutineType::Nat(Some(InstalledNatRoutine {
                    hook: NatHook::Egress,
                    priority: 0i32,
                })),
            }])
            .map(|routine| Change::Create(Resource::Routine(routine)));
        changes.extend(routine_changes);

        changes
    }

    fn create_rule(routine: RoutineId, index: u32, action: Action) -> Rule {
        Rule { id: RuleId { routine, index }, matchers: Matchers::default(), action }
    }

    fn create_routine_id(name: &str) -> RoutineId {
        RoutineId { namespace: namespace_id(), name: String::from(name) }
    }

    fn create_filter_routines(
        namespace: NamespaceId,
        local_ingress: &str,
        local_egress: &str,
    ) -> netfilter::parser::FilterRoutines {
        netfilter::parser::FilterRoutines {
            local_ingress: Some(RoutineId {
                namespace: namespace.clone(),
                name: local_ingress.to_owned(),
            }),
            local_egress: Some(RoutineId { namespace, name: local_egress.to_owned() }),
        }
    }

    // This test only checks for `Ok` cases. The only possible failures for the function under
    // test are related to Rule parsing, which the netfilter library already tests.
    #[test_case(vec![], vec![]; "no_rules")]
    #[test_case(
        vec!["pass in;"],
        vec![create_rule(
                create_routine_id(UNINSTALLED_LOCAL_INGRESS),
                0,
                Action::Accept,
            )]; "ingress_accept")]
    #[test_case(
        vec!["drop out;"],
        vec![create_rule(
                create_routine_id(UNINSTALLED_LOCAL_EGRESS),
                0,
                Action::Drop,
            )]; "egress_drop")]
    #[test_case(
        vec!["pass in; drop out;"],
        vec![create_rule(
                create_routine_id(UNINSTALLED_LOCAL_INGRESS),
                0,
                Action::Accept),
            create_rule(
                create_routine_id(UNINSTALLED_LOCAL_EGRESS),
                1,
                Action::Drop,
            )]; "ingress_accept_egress_drop")]
    fn test_initial_filter_changes(rules_input: Vec<&str>, expected_rules: Vec<Rule>) {
        let namespace = namespace_id();
        let installed_filter_routines =
            create_filter_routines(namespace.clone(), LOCAL_INGRESS, LOCAL_EGRESS);
        let uninstalled_filter_routines =
            create_filter_routines(namespace, UNINSTALLED_LOCAL_INGRESS, UNINSTALLED_LOCAL_EGRESS);

        let changes = generate_initial_filter_changes(
            &uninstalled_filter_routines,
            &installed_filter_routines,
            &masquerade_routine(),
            FilterConfig {
                rules: rules_input.into_iter().map(|rule| rule.to_owned()).collect(),
                nat_rules: vec![],
                rdr_rules: vec![],
            },
            &HashSet::new(),
        )
        .expect("rules should be formatted correctly");

        let mut expected_changes = get_foundational_changes();
        let expected_rule_changes =
            expected_rules.into_iter().map(|rule| Change::Create(Resource::Rule(rule)));
        expected_changes.extend(expected_rule_changes);

        assert_eq!(changes, expected_changes);
    }

    #[test]
    fn test_generate_static_port_class_filter_rules() {
        let namespace = namespace_id();
        let installed_filter_routines =
            create_filter_routines(namespace.clone(), LOCAL_INGRESS, LOCAL_EGRESS);
        let uninstalled_filter_routines =
            create_filter_routines(namespace, UNINSTALLED_LOCAL_INGRESS, UNINSTALLED_LOCAL_EGRESS);

        let rules = generate_static_port_class_filter_rules(
            &uninstalled_filter_routines,
            &installed_filter_routines,
            fnet_interfaces_ext::PortClass::Lowpan,
            0,
        );

        let local_ingress = (
            installed_filter_routines.local_ingress.unwrap(),
            uninstalled_filter_routines.local_ingress.unwrap().name,
            IpHook::LocalIngress,
        );
        let local_egress = (
            installed_filter_routines.local_egress.unwrap(),
            uninstalled_filter_routines.local_egress.unwrap().name,
            IpHook::LocalEgress,
        );
        let expected_rules: Vec<_> = vec![local_ingress, local_egress]
            .into_iter()
            .map(|(installed_routine, uninstalled_routine_name, hook)| {
                create_port_class_matching_jump_rule(
                    installed_routine,
                    0,
                    fnet_interfaces_ext::PortClass::Lowpan,
                    hook,
                    &uninstalled_routine_name,
                )
            })
            .collect();

        assert_eq!(rules, expected_rules);
    }

    #[test]
    fn test_initial_filter_changes_with_lowpan() {
        let namespace = namespace_id();
        let installed_filter_routines =
            create_filter_routines(namespace.clone(), LOCAL_INGRESS, LOCAL_EGRESS);
        let uninstalled_filter_routines =
            create_filter_routines(namespace, UNINSTALLED_LOCAL_INGRESS, UNINSTALLED_LOCAL_EGRESS);

        let changes = generate_initial_filter_changes(
            &uninstalled_filter_routines,
            &installed_filter_routines,
            &masquerade_routine(),
            FilterConfig { rules: vec![], nat_rules: vec![], rdr_rules: vec![] },
            &[InterfaceType::Lowpan].into(),
        )
        .expect("rules should be formatted correctly");

        let mut expected_changes = get_foundational_changes();
        expected_changes.extend(
            generate_static_port_class_filter_rules(
                &uninstalled_filter_routines,
                &installed_filter_routines,
                fnet_interfaces_ext::PortClass::Lowpan,
                0,
            )
            .into_iter()
            .map(|rule| Change::Create(Resource::Rule(rule))),
        );

        assert_eq!(changes, expected_changes);
    }

    #[test_case(
        &[],
        &[];
        "empty"
    )]
    #[test_case(
        &[InterfaceType::Lowpan],
        &[fnet_interfaces_ext::PortClass::Lowpan];
        "lowpan"
    )]
    #[test_case(
        &[InterfaceType::Ethernet],
        &[
            fnet_interfaces_ext::PortClass::Virtual,
            fnet_interfaces_ext::PortClass::Ethernet,
            fnet_interfaces_ext::PortClass::Ppp,
            fnet_interfaces_ext::PortClass::Bridge,
        ];
        "ethernet"
    )]
    #[test_case(
        &[InterfaceType::WlanClient],
        &[
            fnet_interfaces_ext::PortClass::WlanClient,
            fnet_interfaces_ext::PortClass::WlanAp,
        ];
        "wlan_client_enables_wlan_client_and_ap"
    )]
    #[test_case(
        &[InterfaceType::WlanAp],
        &[fnet_interfaces_ext::PortClass::WlanAp];
        "wlan_ap"
    )]
    #[test_case(
        &[InterfaceType::WlanClient, InterfaceType::WlanAp],
        &[
            fnet_interfaces_ext::PortClass::WlanClient,
            fnet_interfaces_ext::PortClass::WlanAp,
        ];
        "wlan_client_and_ap_deduplicated"
    )]
    #[test_case(
        &[InterfaceType::Blackhole],
        &[fnet_interfaces_ext::PortClass::Blackhole];
        "blackhole"
    )]
    fn test_get_enabled_port_classes(
        interface_types: &[InterfaceType],
        expected_port_classes: &[fnet_interfaces_ext::PortClass],
    ) {
        let enabled: HashSet<_> =
            get_enabled_port_classes(&interface_types.iter().copied().collect());
        let expected: HashSet<_> = expected_port_classes.iter().copied().collect();
        assert_eq!(enabled, expected);
    }

    #[fuchsia::test]
    async fn test_update_filters_large_batch() {
        let (control_client, control_server) =
            fidl::endpoints::create_endpoints::<fnet_filter::ControlMarker>();
        let client_fut = FilterControl::new(control_client.into_proxy());
        let mut control_stream = control_server.into_stream();
        let control_server_fut = async move {
            match control_stream
                .next()
                .await
                .expect("stream shouldn't close")
                .expect("stream shouldn't have an error")
            {
                fnet_filter::ControlRequest::OpenController { id, request, control_handle: _ } => {
                    let (request_stream, control_handle) = request.into_stream_and_control_handle();
                    control_handle.send_on_id_assigned(id.as_str()).expect("failed to respond");
                    request_stream
                }
                _ => panic!("unexpected request"),
            }
        };
        let (filter_control, mut server_request_stream) =
            futures::join!(client_fut, control_server_fut);
        let mut filter_control = filter_control.expect("failed to create filter control");

        let config = FilterConfig {
            rules: std::iter::repeat("pass in;".to_string()).take(50).collect(),
            nat_rules: vec![],
            rdr_rules: vec![],
        };

        let server_fut = async move {
            let mut push_changes_count = 0;
            while let Some(req) = server_request_stream.next().await {
                match req.expect("stream shouldn't have an error") {
                    fnet_filter::NamespaceControllerRequest::PushChanges { changes, responder } => {
                        assert!(
                            changes.len() <= usize::from(fnet_filter::MAX_BATCH_SIZE),
                            "batch size {} exceeds MAX_BATCH_SIZE",
                            changes.len()
                        );
                        push_changes_count += 1;
                        responder
                            .send(fnet_filter::ChangeValidationResult::Ok(fnet_filter::Empty))
                            .expect("failed to respond");
                    }
                    fnet_filter::NamespaceControllerRequest::Commit { payload: _, responder } => {
                        responder
                            .send(fnet_filter::CommitResult::Ok(fnet_filter::Empty))
                            .expect("failed to respond");
                        break;
                    }
                    _ => panic!("unexpected request"),
                }
            }
            push_changes_count
        };

        let filter_enabled_interface_types = HashSet::new();
        let (client_res, push_changes_count) = futures::join!(
            filter_control.update_filters(config, &filter_enabled_interface_types),
            server_fut
        );

        client_res.expect("update_filters should succeed");
        assert_eq!(push_changes_count, 2);
    }
}
