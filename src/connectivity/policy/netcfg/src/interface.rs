// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use either::Either;
use serde::{Deserialize, Deserializer};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};

use fidl_fuchsia_driver_framework as fdf;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;

use crate::DeviceClass;

const INTERFACE_PREFIX_WLAN: &str = "wlan";
const INTERFACE_PREFIX_ETHERNET: &str = "eth";
const INTERFACE_PREFIX_AP: &str = "ap";
const INTERFACE_PREFIX_BLACKHOLE: &str = "blackhole";

#[derive(PartialEq, Eq, Debug, Clone, Hash)]
enum BusAddress {
    Int(u8),
    ArrayInt(Vec<u8>),
    CharInt(String),
    ArrayCharInt(Vec<String>),
    String(String),
    Unknown,
}

impl From<&fdf::DeviceAddress> for BusAddress {
    fn from(address: &fdf::DeviceAddress) -> Self {
        match address {
            fdf::DeviceAddress::IntValue(v) => Self::Int(*v),
            fdf::DeviceAddress::ArrayIntValue(v) => Self::ArrayInt(v.clone()),
            fdf::DeviceAddress::CharIntValue(v) => Self::CharInt(v.clone()),
            fdf::DeviceAddress::ArrayCharIntValue(v) => Self::ArrayCharInt(v.clone()),
            fdf::DeviceAddress::StringValue(v) => Self::String(v.clone()),
            fdf::DeviceAddress::__SourceBreaking { .. } => Self::Unknown,
        }
    }
}

/// A normalized, hashable representation of a single segment in a driver node's
/// bus topology (`fuchsia.driver.framework.BusInfo`), obtained from
/// `fuchsia.driver.token.NodeBusTopology.Get`.
///
/// Common formats include:
/// - PCI: `bus = BusType::Pci`, `address = ArrayInt([bus, device, function])`
/// - Platform / DeviceTree: `bus = BusType::Platform` or `DeviceTree`,
///   `address = String("mmc-ffe05000")` or `ArrayCharInt(["05", "00", "6"])`
/// - SDIO / USB: `bus = BusType::Sdio` or `Usb`, `address = Int(func_or_port)`
#[derive(PartialEq, Eq, Debug, Clone, Hash)]
pub(crate) struct BusSegment {
    bus: Option<u32>,
    address: Option<BusAddress>,
}

impl From<&fdf::BusInfo> for BusSegment {
    fn from(info: &fdf::BusInfo) -> Self {
        Self {
            bus: info.bus.map(fdf::BusType::into_primitive),
            address: info.address.as_ref().map(Into::into),
        }
    }
}

// Interfaces with different InterfaceNamingIdentifiers are expected to have
// different names.
//
// If two interfaces have the same MAC, they are expected to produce a different
// name in two possible ways: 1) have a different port class, or 2) use the
// NormalizedMac naming scheme (which avoids conflicts via retries).
#[derive(PartialEq, Eq, Debug, Clone, Hash)]
pub(crate) struct InterfaceNamingIdentifier {
    pub(crate) mac: fidl_fuchsia_net_ext::MacAddress,
    pub(crate) bus_segments: Vec<BusSegment>,
}

pub(crate) fn generate_identifier(
    mac_address: &fidl_fuchsia_net_ext::MacAddress,
    bus_info: &[fdf::BusInfo],
) -> InterfaceNamingIdentifier {
    InterfaceNamingIdentifier {
        mac: *mac_address,
        bus_segments: bus_info.iter().map(Into::into).collect(),
    }
}

// Get the NormalizedMac using the last octet of the MAC address. The offset
// modifies the last_byte in an attempt to avoid naming conflicts.
// For example, a MAC of `[0x1, 0x1, 0x1, 0x1, 0x1, 0x9]` with offset 0
// becomes `9`.
fn get_mac_identifier_from_octets(
    octets: &[u8; 6],
    interface_type: crate::InterfaceType,
    offset: u8,
) -> Result<u8, anyhow::Error> {
    if offset == u8::MAX {
        return Err(anyhow::format_err!(
            "could not find unique identifier for mac={:?}, interface_type={:?}",
            octets,
            interface_type
        ));
    }

    let last_byte = octets[octets.len() - 1];
    let (identifier, _) = last_byte.overflowing_add(offset);
    Ok(identifier)
}

/// Wrapper around a device's `NodeBusTopology` `BusInfo` segments.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BusTopology<'a>(pub(crate) &'a [fdf::BusInfo]);

impl<'a> BusTopology<'a> {
    fn format_address(bus: Option<fdf::BusType>, address: &fdf::DeviceAddress) -> Option<String> {
        match address {
            fdf::DeviceAddress::IntValue(v) => Some(format!("{v:03}")),
            fdf::DeviceAddress::ArrayIntValue(v) => match (bus, v.as_slice()) {
                // Match DFv2 PCI node naming (`%02x_%02x_%01x` in `pci::Device::node_name` in
                // `//src/devices/pci/drivers/pci/device.cc`).
                (Some(fdf::BusType::Pci), [b, d, f]) => Some(format!("{b:02x}_{d:02x}_{f:x}")),
                _ => Some(v.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")),
            },
            fdf::DeviceAddress::CharIntValue(v) => Some(v.clone()),
            fdf::DeviceAddress::ArrayCharIntValue(v) => Some(v.join(":")),
            fdf::DeviceAddress::StringValue(v) => Some(v.clone()),
            fdf::DeviceAddress::__SourceBreaking { .. } => None,
        }
    }

    // Extract the `BusType` for a device directly from its bus topology segments.
    pub(crate) fn bus_type(&self) -> BusType {
        // Walk from the innermost (leaf-most) bus segment toward the root so
        // bridged devices (e.g. USB or VirtIO over a PCI controller) resolve
        // to their immediate attachment bus rather than the host bridge.
        self.0
            .iter()
            .rev()
            .find_map(|info| match info.bus {
                Some(fdf::BusType::Pci) => Some(BusType::PCI),
                Some(fdf::BusType::Usb | fdf::BusType::UsbPeripheral) => Some(BusType::USB),
                Some(fdf::BusType::Sdio) => Some(BusType::SDIO),
                Some(fdf::BusType::Virtio) => Some(BusType::VirtIo),
                Some(_) | None => None,
            })
            .unwrap_or(BusType::Unknown)
    }

    // Get the normalized bus path from a device's bus topology segments.
    // For example, a PCI device at `[0x02, 0x00, 0x01]` (`02_00_1`) becomes `02001`,
    // and `[0x00, 0x14, 0x00]` (`00_14_0`) becomes `0014`.
    //
    // Note: `BusPath` only produces deterministic bus IDs for PCI; non-PCI DFv2
    // buses (`DeviceTree`, `Sdio`, `Usb`, `UsbPeripheral`) do not carry multi-byte
    // bus addresses and fall back to `PATH_UNIQ_MARKER`.
    //
    // When no bus address segment can be normalized (or trimming trailing zeroes
    // leaves an empty string), the function returns a value unlikely to conflict
    // with any existing bus path by starting at `ffffff` and decrementing on each
    // call.
    pub(crate) fn normalized_bus_path(&self) -> String {
        static PATH_UNIQ_MARKER: AtomicU32 = AtomicU32::new(0xffffff);
        self.0
            .iter()
            .filter_map(|info| {
                let addr = info.address.as_ref()?;
                let s = Self::format_address(info.bus, addr)?;
                (s.len() >= 7
                    && s.chars().all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.' || c == '_'))
                .then(|| s.replace(&[':', '.', '_'], ""))
            })
            .next()
            .map(|s| s.trim_end_matches('0').to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("{:06x}", PATH_UNIQ_MARKER.fetch_sub(1, Ordering::SeqCst)))
    }

    pub(crate) fn matches_pattern(&self, pattern: &glob::Pattern) -> bool {
        let mut path = String::new();
        for info in self.0 {
            let Some(raw) =
                info.address.as_ref().and_then(|addr| Self::format_address(info.bus, addr))
            else {
                continue;
            };
            let segment = raw.trim_matches('/');
            if !segment.is_empty() {
                path.push('/');
                path.push_str(segment);
            }
        }
        if path.is_empty() {
            pattern.matches("")
        } else {
            pattern.matches(&path) || pattern.matches(&format!("{path}/"))
        }
    }
}

#[derive(Debug)]
pub struct InterfaceNamingConfig {
    naming_rules: Vec<NamingRule>,
    interfaces: HashMap<InterfaceNamingIdentifier, String>,
}

impl InterfaceNamingConfig {
    pub(crate) fn from_naming_rules(naming_rules: Vec<NamingRule>) -> InterfaceNamingConfig {
        InterfaceNamingConfig { naming_rules, interfaces: HashMap::new() }
    }

    /// Returns a stable interface name for the specified interface.
    pub(crate) fn generate_stable_name(
        &mut self,
        bus_info: &[fdf::BusInfo],
        mac: &fidl_fuchsia_net_ext::MacAddress,
        device_class: DeviceClass,
    ) -> Result<(&str, InterfaceNamingIdentifier), NameGenerationError> {
        let interface_naming_id = generate_identifier(mac, bus_info);
        let info = DeviceInfoRef { bus_info, mac, device_class };

        // Interfaces that are named using the NormalizedMac naming rule are
        // named to avoid MAC address final octet collisions. When a device
        // with the same identifier is re-installed, re-attempt name generation
        // since the MAC identifiers used may have changed.
        match self.interfaces.remove(&interface_naming_id) {
            Some(name) => log::info!(
                "{name} already existed for this identifier\
            {interface_naming_id:?}. inserting a new one."
            ),
            None => {
                // This interface naming id will have a new entry
            }
        }

        let generated_name = self.generate_name(&info)?;
        if let Some(name) =
            self.interfaces.insert(interface_naming_id.clone(), generated_name.clone())
        {
            log::error!(
                "{name} was unexpectedly found for {interface_naming_id:?} \
            when inserting a new name"
            );
        }

        // Need to grab a reference to appease the borrow checker.
        let generated_name = match self.interfaces.get(&interface_naming_id) {
            Some(name) => Ok(name),
            None => Err(NameGenerationError::GenerationError(anyhow::format_err!(
                "expected to see name {generated_name} present since it was just added"
            ))),
        }?;

        Ok((generated_name, interface_naming_id))
    }

    fn generate_name(&self, info: &DeviceInfoRef<'_>) -> Result<String, NameGenerationError> {
        generate_name_from_naming_rules(&self.naming_rules, &self.interfaces, &info)
    }
}

/// An error observed when generating a new name.
#[derive(Debug)]
pub enum NameGenerationError {
    GenerationError(anyhow::Error),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum BusType {
    PCI,
    SDIO,
    USB,
    Unknown,
    VirtIo,
}

impl BusType {
    // Retrieve the list of composition rules that comprise the default name
    // for the interface based on BusType.
    // Example names for the following default rules:
    // * USB device: "ethx5"
    // * PCI/SDIO device: "ethp4", "wlans5"
    fn get_default_name_composition_rules(&self) -> Vec<NameCompositionRule> {
        match *self {
            BusType::USB | BusType::Unknown => vec![
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
                NameCompositionRule::Static { value: String::from("x") },
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
            ],
            BusType::PCI | BusType::SDIO | BusType::VirtIo => vec![
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusType },
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
            ],
        }
    }
}

impl std::fmt::Display for BusType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match *self {
            Self::PCI => "p",
            Self::SDIO => "s",
            Self::USB => "u",
            Self::Unknown => "unk",
            Self::VirtIo => "v",
        };
        write!(f, "{}", name)
    }
}

fn deserialize_glob_pattern<'de, D>(deserializer: D) -> Result<glob::Pattern, D::Error>
where
    D: Deserializer<'de>,
{
    let buf = String::deserialize(deserializer)?;
    glob::Pattern::new(&buf).map_err(serde::de::Error::custom)
}

/// The matching rules available for a `NamingRule`.
#[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum MatchingRule {
    BusTypes(Vec<BusType>),
    // TODO(https://fxbug.dev/42085144): Use a lightweight regex crate with the basic
    // regex features to allow for more configurations than glob.
    #[serde(deserialize_with = "deserialize_glob_pattern")]
    TopologicalPath(glob::Pattern),
    DeviceClasses(Vec<DeviceClass>),
    // Signals whether this rule should match any interface.
    Any(bool),
}

/// The matching rules available for a `ProvisoningRule`.
#[derive(Debug, Deserialize, Eq, Hash, PartialEq)]
#[serde(untagged)]
pub enum ProvisioningMatchingRule {
    // TODO(github.com/serde-rs/serde/issues/912): Use `other` once it supports
    // deserializing into non-unit variants. `untagged` can only be applied
    // to the entire enum, so `interface_name` is used as a field to ensure
    // stability across configuration matching rules.
    InterfaceName {
        #[serde(rename = "interface_name", deserialize_with = "deserialize_glob_pattern")]
        pattern: glob::Pattern,
    },
    Common(MatchingRule),
}

impl MatchingRule {
    fn does_interface_match(&self, info: &DeviceInfoRef<'_>) -> Result<bool, anyhow::Error> {
        match &self {
            MatchingRule::BusTypes(type_list) => {
                // Match the interface if the interface under comparison
                // matches any of the types included in the list.
                let bus_type = BusTopology(info.bus_info).bus_type();
                Ok(type_list.contains(&bus_type))
            }
            MatchingRule::TopologicalPath(pattern) => {
                // Match the interface if the provided pattern matches the
                // interface's formatted bus address segments.
                Ok(BusTopology(info.bus_info).matches_pattern(pattern))
            }
            MatchingRule::DeviceClasses(class_list) => {
                // Match the interface if the interface under comparison
                // matches any of the types included in the list.
                Ok(class_list.contains(&info.device_class))
            }
            MatchingRule::Any(matches_any_interface) => Ok(*matches_any_interface),
        }
    }
}

impl ProvisioningMatchingRule {
    fn does_interface_match(
        &self,
        info: &DeviceInfoRef<'_>,
        interface_name: &str,
    ) -> Result<bool, anyhow::Error> {
        match &self {
            ProvisioningMatchingRule::InterfaceName { pattern } => {
                // Match the interface if the provided pattern finds any
                // matches in the interface under comparison's name.
                Ok(pattern.matches(interface_name))
            }
            ProvisioningMatchingRule::Common(matching_rule) => {
                // Handle the other `MatchingRule`s the same as the naming
                // policy matchers.
                matching_rule.does_interface_match(info)
            }
        }
    }
}

// TODO(https://fxbug.dev/42084785): Create dynamic naming rules
// A naming rule that uses device information to produce a component of
// the interface's name.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum DynamicNameCompositionRule {
    BusPath,
    BusType,
    DeviceClass,
    // A unique value seeded by the final octet of the interface's MAC address.
    NormalizedMac,
}

impl DynamicNameCompositionRule {
    // `true` when a rule can be re-tried to produce a different name.
    fn supports_retry(&self) -> bool {
        match *self {
            DynamicNameCompositionRule::BusPath
            | DynamicNameCompositionRule::BusType
            | DynamicNameCompositionRule::DeviceClass => false,
            DynamicNameCompositionRule::NormalizedMac => true,
        }
    }

    fn get_name(&self, info: &DeviceInfoRef<'_>, attempt_num: u8) -> Result<String, anyhow::Error> {
        Ok(match *self {
            DynamicNameCompositionRule::BusPath => BusTopology(info.bus_info).normalized_bus_path(),
            DynamicNameCompositionRule::BusType => {
                BusTopology(info.bus_info).bus_type().to_string()
            }
            DynamicNameCompositionRule::DeviceClass => match info.device_class.into() {
                crate::InterfaceType::WlanClient => INTERFACE_PREFIX_WLAN,
                crate::InterfaceType::Ethernet => INTERFACE_PREFIX_ETHERNET,
                // Lowpan interfaces are not managed or installed by Netcfg.
                crate::InterfaceType::Lowpan => {
                    unreachable!("unexpected to install lowpan interface")
                }
                crate::InterfaceType::WlanAp => INTERFACE_PREFIX_AP,
                crate::InterfaceType::Blackhole => INTERFACE_PREFIX_BLACKHOLE,
            }
            .to_string(),
            DynamicNameCompositionRule::NormalizedMac => {
                let fidl_fuchsia_net_ext::MacAddress { octets } = info.mac;
                let mac_identifier =
                    get_mac_identifier_from_octets(octets, info.device_class.into(), attempt_num)?;
                format!("{mac_identifier:x}")
            }
        })
    }
}

// A rule that dictates a component of an interface's name. An interface's name
// is determined by extracting the name of each rule, in order, and
// concatenating the results.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "lowercase", tag = "type")]
pub enum NameCompositionRule {
    Static { value: String },
    Dynamic { rule: DynamicNameCompositionRule },
    // The default name composition rules based on the device's BusType.
    // Defined in `BusType::get_default_name_composition_rules`.
    Default,
}

/// A rule that dictates how interfaces that align with the property matching
/// rules should be named.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub struct NamingRule {
    /// A set of rules to check against an interface's properties. All rules
    /// must apply for the naming scheme to take effect.
    pub matchers: HashSet<MatchingRule>,
    /// The rules to apply to the interface to produce the interface's name.
    pub naming_scheme: Vec<NameCompositionRule>,
}

impl NamingRule {
    // An interface's name is determined by extracting the name of each rule,
    // in order, and concatenating the results. Returns an error if the
    // interface name cannot be generated.
    fn generate_name(
        &self,
        interfaces: &HashMap<InterfaceNamingIdentifier, String>,
        info: &DeviceInfoRef<'_>,
    ) -> Result<String, NameGenerationError> {
        // When a bus type cannot be found for a device, use the USB
        // default naming policy which uses a MAC address.
        let bus_type = BusTopology(info.bus_info).bus_type();

        // Expand any `Default` rules into the `Static` and `Dynamic` rules in a single vector.
        // If this was being consumed once, we could avoid the call to `collect`. However, since we
        // want to use it twice, we need to convert it to a form where the items can be itererated
        // over without consuming them.
        let expanded_rules = self
            .naming_scheme
            .iter()
            .map(|rule| {
                if let NameCompositionRule::Default = rule {
                    Either::Right(bus_type.get_default_name_composition_rules().into_iter())
                } else {
                    Either::Left(std::iter::once(rule.clone()))
                }
            })
            .flatten()
            .collect::<Vec<_>>();

        // Determine whether any rules present support retrying for a unique name.
        let should_reattempt_on_conflict = expanded_rules.iter().any(|rule| {
            if let NameCompositionRule::Dynamic { rule } = rule {
                rule.supports_retry()
            } else {
                false
            }
        });

        let mut attempt_num = 0u8;
        loop {
            let name = expanded_rules
                .iter()
                .map(|rule| match rule {
                    NameCompositionRule::Static { value } => Ok(value.clone()),
                    // Dynamic rules require the knowledge of `DeviceInfo` properties.
                    NameCompositionRule::Dynamic { rule } => rule
                        .get_name(info, attempt_num)
                        .map_err(NameGenerationError::GenerationError),
                    NameCompositionRule::Default => {
                        unreachable!(
                            "Default naming rules should have been pre-expanded. \
                             Nested default rules are not supported."
                        );
                    }
                })
                .collect::<Result<String, NameGenerationError>>()?;

            if interfaces.values().any(|existing_name| existing_name == &name) {
                if should_reattempt_on_conflict {
                    attempt_num += 1;
                    // Try to generate another name with the modified attempt number.
                    continue;
                }

                log::warn!(
                    "name ({name}) already used for an interface installed by netcfg. \
                 using name since it is possible that the interface using this name is no \
                 longer active"
                );
            }
            return Ok(name);
        }
    }

    // An interface must align with all specified `MatchingRule`s.
    fn does_interface_match(&self, info: &DeviceInfoRef<'_>) -> bool {
        self.matchers.iter().all(|rule| rule.does_interface_match(info).unwrap_or_default())
    }
}

// Find the first `NamingRule` that matches the device and attempt to
// construct a name from the provided `NameCompositionRule`s.
fn generate_name_from_naming_rules(
    naming_rules: &[NamingRule],
    interfaces: &HashMap<InterfaceNamingIdentifier, String>,
    info: &DeviceInfoRef<'_>,
) -> Result<String, NameGenerationError> {
    // TODO(https://fxbug.dev/42086002): Consider adding an option to the rules to allow
    // fallback rules when name generation fails.
    // Use the first naming rule that matches the interface to enforce consistent
    // interface names, even if there are other matching rules.
    let fallback_rule = fallback_naming_rule();
    let first_matching_rule =
        naming_rules.iter().find(|rule| rule.does_interface_match(&info)).unwrap_or(
            // When there are no `NamingRule`s that match the device,
            // use a fallback rule that has the Default naming scheme.
            &fallback_rule,
        );

    first_matching_rule.generate_name(interfaces, &info)
}

// Matches any device and uses the default naming rule.
fn fallback_naming_rule() -> NamingRule {
    NamingRule {
        matchers: HashSet::from([MatchingRule::Any(true)]),
        naming_scheme: vec![NameCompositionRule::Default],
    }
}

/// The provision action to take if the matchers are satisfied.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub struct ProvisioningAction {
    /// The type of the provisioning.
    pub provisioning: ProvisioningType,
    /// Where the netstack managed routes should be installed.
    pub netstack_managed_routes_designation: Option<NetstackManagedRoutesDesignation>,
}

/// Whether the interface should be provisioned locally by netcfg, or
/// delegated. Provisioning is the set of events that occurs after
/// interface enumeration, such as starting a DHCP client and assigning
/// an IP to the interface. Provisioning actions work to support
/// Internet connectivity.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Default)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub enum ProvisioningType {
    /// Netcfg will provision the interface
    #[default]
    Local,
    /// Netcfg will not provision the interface. The provisioning
    /// of the interface will occur elsewhere
    Delegated,
}

impl ProvisioningType {
    /// Whether the interface should be tracked in the network registry.
    ///
    /// Locally provisioned interfaces are tracked in the network registry because their
    /// lifetime is managed by netcfg. Delegated interfaces are not tracked in the network
    /// registry because their presence is communicated by the network-socket-proxy service.
    pub fn track_in_network_registry(&self) -> bool {
        match self {
            ProvisioningType::Local => true,
            ProvisioningType::Delegated => false,
        }
    }
}

/// Where the netstack managed routes should be stored.
///
/// Mirrors [`fnet_interfaces_admin::NetstackManagedRoutesDesignation`].
#[derive(Copy, Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum NetstackManagedRoutesDesignation {
    Main,
    InterfaceLocal,
}

impl From<NetstackManagedRoutesDesignation>
    for fnet_interfaces_admin::NetstackManagedRoutesDesignation
{
    fn from(value: NetstackManagedRoutesDesignation) -> Self {
        match value {
            NetstackManagedRoutesDesignation::Main => Self::Main(fnet_interfaces_admin::Empty),
            NetstackManagedRoutesDesignation::InterfaceLocal => {
                Self::InterfaceLocal(fnet_interfaces_admin::Empty)
            }
        }
    }
}

/// A rule that dictates how interfaces that align with the property matching
/// rules should be provisioned.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "lowercase")]
pub struct ProvisioningRule {
    /// A set of rules to check against an interface's properties. All rules
    /// must apply for the provisioning action to take effect.
    pub matchers: HashSet<ProvisioningMatchingRule>,
    /// The provisioning policy that netcfg applies to a matching
    /// interface.
    #[serde(flatten)]
    pub action: ProvisioningAction,
}

// A ref version of `devices::DeviceInfo` to avoid the need to clone data
// unnecessarily. Devices without MAC are not supported yet, see
// `add_new_device` in `lib.rs`. This makes mac into a required field for
// ease of use.
pub(super) struct DeviceInfoRef<'a> {
    pub(super) device_class: DeviceClass,
    pub(super) mac: &'a fidl_fuchsia_net_ext::MacAddress,
    pub(super) bus_info: &'a [fdf::BusInfo],
}

impl<'a> DeviceInfoRef<'a> {
    pub(super) fn interface_type(&self) -> crate::InterfaceType {
        let DeviceInfoRef { device_class, mac: _, bus_info: _ } = self;
        (*device_class).into()
    }

    pub(super) fn is_wlan_ap(&self) -> bool {
        let DeviceInfoRef { device_class, mac: _, bus_info: _ } = self;
        match device_class {
            DeviceClass::WlanAp => true,
            DeviceClass::WlanClient
            | DeviceClass::Virtual
            | DeviceClass::Ethernet
            | DeviceClass::Bridge
            | DeviceClass::Ppp
            | DeviceClass::Lowpan
            | DeviceClass::Blackhole => false,
        }
    }
}

impl ProvisioningRule {
    // An interface must align with all specified `MatchingRule`s.
    fn does_interface_match(&self, info: &DeviceInfoRef<'_>, interface_name: &str) -> bool {
        self.matchers
            .iter()
            .all(|rule| rule.does_interface_match(info, interface_name).unwrap_or_default())
    }
}

// Find the first `ProvisioningRule` that matches the device and get
// the associated `ProvisioningAction`. By default, use Local provisioning
// so that Netcfg will provision interfaces unless configuration
// indicates otherwise.
pub(crate) fn find_provisioning_action_from_provisioning_rules(
    provisioning_rules: &[ProvisioningRule],
    info: &DeviceInfoRef<'_>,
    interface_name: &str,
) -> ProvisioningAction {
    provisioning_rules
        .iter()
        .find_map(|rule| {
            if rule.does_interface_match(&info, &interface_name) { Some(rule.action) } else { None }
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use test_case::test_case;

    // This is a lossy conversion between `InterfaceType` and `DeviceClass`
    // that allows tests to use a `devices::DeviceInfo` struct instead of
    // handling the fields individually.
    fn device_class_from_interface_type(ty: crate::InterfaceType) -> DeviceClass {
        match ty {
            crate::InterfaceType::Ethernet => DeviceClass::Ethernet,
            crate::InterfaceType::Lowpan => DeviceClass::Lowpan,
            crate::InterfaceType::WlanClient => DeviceClass::WlanClient,
            crate::InterfaceType::WlanAp => DeviceClass::WlanAp,
            crate::InterfaceType::Blackhole => DeviceClass::Blackhole,
        }
    }

    fn pci_bus(b: u8, d: u8, f: u8) -> Vec<fdf::BusInfo> {
        vec![fdf::BusInfo {
            bus: Some(fdf::BusType::Pci),
            address: Some(fdf::DeviceAddress::ArrayIntValue(vec![b, d, f])),
            ..Default::default()
        }]
    }

    fn usb_over_pci_bus(b: u8, d: u8, f: u8, port: u8) -> Vec<fdf::BusInfo> {
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::Pci),
                address: Some(fdf::DeviceAddress::ArrayIntValue(vec![b, d, f])),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::Usb),
                address: Some(fdf::DeviceAddress::IntValue(port)),
                ..Default::default()
            },
        ]
    }

    fn usb_peripheral_dwc_bus() -> Vec<fdf::BusInfo> {
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::Platform),
                address: Some(fdf::DeviceAddress::ArrayIntValue(vec![0x05, 0x00, 0x18])),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::UsbPeripheral),
                address: Some(fdf::DeviceAddress::IntValue(0)),
                ..Default::default()
            },
        ]
    }

    fn sdio_platform_char_bus() -> Vec<fdf::BusInfo> {
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::Platform),
                address: Some(fdf::DeviceAddress::ArrayCharIntValue(vec![
                    "05".to_string(),
                    "00".to_string(),
                    "6".to_string(),
                ])),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::Sdio),
                address: Some(fdf::DeviceAddress::IntValue(1)),
                ..Default::default()
            },
        ]
    }

    fn sdio_dfv2_mmc_bus(func: u8) -> Vec<fdf::BusInfo> {
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::Platform),
                address: Some(fdf::DeviceAddress::StringValue("mmc-ffe05000".to_string())),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::Sdio),
                address: Some(fdf::DeviceAddress::IntValue(func)),
                ..Default::default()
            },
        ]
    }

    fn platform_eth_bus() -> Vec<fdf::BusInfo> {
        vec![fdf::BusInfo {
            bus: Some(fdf::BusType::Platform),
            address: Some(fdf::DeviceAddress::StringValue("ethernet-phy-ff634000".to_string())),
            ..Default::default()
        }]
    }

    fn virtio_over_pci_bus(b: u8, d: u8, f: u8) -> Vec<fdf::BusInfo> {
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::Pci),
                address: Some(fdf::DeviceAddress::ArrayIntValue(vec![b, d, f])),
                ..Default::default()
            },
            fdf::BusInfo { bus: Some(fdf::BusType::Virtio), address: None, ..Default::default() },
        ]
    }

    // usb interfaces
    #[test_case(
        usb_over_pci_bus(0, 0x14, 0, 4),
        [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        crate::InterfaceType::WlanClient,
        "wlanx1";
        "usb_wlan"
    )]
    #[test_case(
        usb_over_pci_bus(0, 0x15, 0, 4),
        [0x02, 0x02, 0x02, 0x02, 0x02, 0x02],
        crate::InterfaceType::Ethernet,
        "ethx2";
        "usb_eth"
    )]
    // pci interfaces
    #[test_case(
        pci_bus(0, 0x14, 0),
        [0x03, 0x03, 0x03, 0x03, 0x03, 0x03],
        crate::InterfaceType::WlanClient,
        "wlanp3";
        "pci_wlan"
    )]
    #[test_case(
        pci_bus(0, 0x15, 0),
        [0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        crate::InterfaceType::Ethernet,
        "ethp4";
        "pci_eth"
    )]
    // platform interfaces (ethernet jack and sdio devices)
    #[test_case(
        sdio_platform_char_bus(),
        [0x05, 0x05, 0x05, 0x05, 0x05, 0x05],
        crate::InterfaceType::WlanClient,
        "wlans5";
        "platform_wlan"
    )]
    #[test_case(
        sdio_dfv2_mmc_bus(1),
        [0x06, 0x06, 0x06, 0x06, 0x06, 0x59],
        crate::InterfaceType::WlanClient,
        "wlans59";
        "platform_wlan_dfv2_mmc"
    )]
    #[test_case(
        platform_eth_bus(),
        [0x07, 0x07, 0x07, 0x07, 0x07, 0x07],
        crate::InterfaceType::Ethernet,
        "ethx7";
        "platform_eth"
    )]
    // unknown interfaces
    #[test_case(
        vec![],
        [0x08, 0x08, 0x08, 0x08, 0x08, 0x08],
        crate::InterfaceType::WlanClient,
        "wlanx8";
        "unknown_wlan1"
    )]
    #[test_case(
        vec![fdf::BusInfo {
            bus: None,
            address: Some(fdf::DeviceAddress::StringValue("unknown".to_string())),
            ..Default::default()
        }],
        [0x09, 0x09, 0x09, 0x09, 0x09, 0x09],
        crate::InterfaceType::WlanClient,
        "wlanx9";
        "unknown_wlan2"
    )]
    #[test_case(
        vec![],
        [0x0a, 0x0a, 0x0a, 0x0a, 0x0a, 0x0a],
        crate::InterfaceType::WlanAp,
        "apxa";
        "unknown_ap"
    )]
    #[test_case(
        virtio_over_pci_bus(0, 0x1e, 0),
        [0x0b, 0x0b, 0x0b, 0x0b, 0x0b, 0x0b],
        crate::InterfaceType::Ethernet,
        "ethvb";
        "virtio_attached_ethernet"
    )]
    // NB: name generation for blackhole interfaces is never expected to be invoked.
    #[test_case(
        usb_over_pci_bus(0, 0x15, 0, 4),
        [0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c],
        crate::InterfaceType::Blackhole,
        "blackholexc";
        "usb_blackhole"
    )]
    fn test_generate_name(
        bus_info: Vec<fdf::BusInfo>,
        mac: [u8; 6],
        interface_type: crate::InterfaceType,
        want_name: &'static str,
    ) {
        let interface_naming_config = InterfaceNamingConfig::from_naming_rules(vec![]);
        let name = interface_naming_config
            .generate_name(&DeviceInfoRef {
                device_class: device_class_from_interface_type(interface_type),
                mac: &fidl_fuchsia_net_ext::MacAddress { octets: mac },
                bus_info: &bus_info,
            })
            .expect("failed to generate the name");
        assert_eq!(name, want_name);
    }

    struct StableNameTestCase {
        bus_info: Vec<fdf::BusInfo>,
        mac: [u8; 6],
        interface_type: crate::InterfaceType,
        want_name: &'static str,
        expected_size: usize,
    }

    // Base case. Interface should be added to config.
    #[test_case([StableNameTestCase {
        bus_info: pci_bus(0, 0x14, 0),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlanp1",
        expected_size: 1 }];
        "single_interface"
    )]
    // Test case that shares the same bus topology and different MAC, but same
    // last octet. Expect to see second interface added with different name.
    #[test_case([StableNameTestCase {
        bus_info: pci_bus(0, 0x14, 0),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlanp1",
        expected_size: 1 }, StableNameTestCase {
        bus_info: pci_bus(0, 0x14, 0),
        mac: [0xFE, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::WlanAp,
        want_name: "app1",
        expected_size: 2 }];
        "two_interfaces_same_topo_path_different_mac"
    )]
    #[test_case([StableNameTestCase {
        bus_info: pci_bus(0, 0x14, 0),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlanp1",
        expected_size: 1 }, StableNameTestCase {
        bus_info: pci_bus(1, 0, 0),
        mac: [0xFE, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::Ethernet,
        want_name: "ethp1",
        expected_size: 2 }];
        "two_distinct_interfaces"
    )]
    // Test case that labels iwlwifi as ethernet, then changes the device
    // class to wlan. The test should detect that the device class doesn't
    // match the interface name, and overwrite with the new interface name
    // that does match.
    #[test_case([StableNameTestCase {
        bus_info: pci_bus(1, 0, 0),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::Ethernet,
        want_name: "ethp1",
        expected_size: 1 }, StableNameTestCase {
        bus_info: pci_bus(1, 0, 0),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x01],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlanp1",
        expected_size: 1 }];
        "two_interfaces_different_device_class"
    )]
    // Recreating an SDIO WLAN interface with a distinct SDIO function address
    // and the same MAC address retries via NormalizedMac rather than
    // duplicating the previous interface's name.
    #[test_case([StableNameTestCase {
        bus_info: sdio_dfv2_mmc_bus(1),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x59],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlans59",
        expected_size: 1 }, StableNameTestCase {
        bus_info: sdio_dfv2_mmc_bus(2),
        mac: [0x01, 0x01, 0x01, 0x01, 0x01, 0x59],
        interface_type: crate::InterfaceType::WlanClient,
        want_name: "wlans5a",
        expected_size: 2 }];
        "sdio_wlan_recreate_retries_mac_offset"
    )]
    fn test_generate_stable_name(test_cases: impl IntoIterator<Item = StableNameTestCase>) {
        let mut interface_naming_config = InterfaceNamingConfig::from_naming_rules(vec![]);

        for StableNameTestCase { bus_info, mac, interface_type, want_name, expected_size } in
            test_cases
        {
            let (name, _identifier) = interface_naming_config
                .generate_stable_name(
                    &bus_info,
                    &fidl_fuchsia_net_ext::MacAddress { octets: mac },
                    device_class_from_interface_type(interface_type),
                )
                .expect("failed to get the interface name");
            assert_eq!(name, want_name);
            assert_eq!(interface_naming_config.interfaces.len(), expected_size);
        }
    }

    #[test_case(
        usb_over_pci_bus(0, 0x14, 0, 4),
        crate::InterfaceType::Ethernet,
        "ethx";
        "usb_ethernet"
    )]
    #[test_case(
        sdio_dfv2_mmc_bus(1),
        crate::InterfaceType::WlanClient,
        "wlans";
        "sdio_wlan"
    )]
    fn test_get_default_naming_retries_255(
        bus_info: Vec<fdf::BusInfo>,
        interface_type: crate::InterfaceType,
        expected_prefix: &'static str,
    ) {
        let mut config = InterfaceNamingConfig::from_naming_rules(vec![]);
        for n in 0u8..255u8 {
            let octets = [n, 0x01, 0x01, 0x01, 0x01, 0x00];

            let interface_naming_id =
                generate_identifier(&fidl_fuchsia_net_ext::MacAddress { octets }, &bus_info);

            let name = config
                .generate_name(&DeviceInfoRef {
                    device_class: device_class_from_interface_type(interface_type),
                    mac: &fidl_fuchsia_net_ext::MacAddress { octets },
                    bus_info: &bus_info,
                })
                .expect("failed to generate the name");
            assert_eq!(name, format!("{expected_prefix}{n:x}"));
            assert_matches!(config.interfaces.insert(interface_naming_id, name), None);
        }

        let octets = [0x00, 0x00, 0x01, 0x01, 0x01, 0x00];
        assert!(
            config
                .generate_name(&DeviceInfoRef {
                    device_class: device_class_from_interface_type(interface_type),
                    mac: &fidl_fuchsia_net_ext::MacAddress { octets },
                    bus_info: &bus_info,
                })
                .is_err()
        );
    }

    #[test]
    fn test_get_usb_255_with_naming_rule() {
        let usb_bus = usb_over_pci_bus(0, 0x14, 0, 4);

        let naming_rule = NamingRule {
            matchers: HashSet::new(),
            naming_scheme: vec![
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
                NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
            ],
        };

        let mut config = InterfaceNamingConfig::from_naming_rules(vec![naming_rule]);
        for n in 0u8..255u8 {
            let octets = [n, 0x01, 0x01, 0x01, 0x01, 0x00];
            let interface_naming_id =
                generate_identifier(&fidl_fuchsia_net_ext::MacAddress { octets }, &usb_bus);

            let info = DeviceInfoRef {
                device_class: DeviceClass::Ethernet,
                mac: &fidl_fuchsia_net_ext::MacAddress { octets },
                bus_info: &usb_bus,
            };

            let name = config.generate_name(&info).expect("failed to generate the name");
            assert_eq!(name, format!("{n:x}{n:x}"));

            assert_matches!(config.interfaces.insert(interface_naming_id, name), None);
        }

        let octets = [0x00, 0x00, 0x01, 0x01, 0x01, 0x00];
        assert!(
            config
                .generate_name(&DeviceInfoRef {
                    device_class: DeviceClass::Ethernet,
                    mac: &fidl_fuchsia_net_ext::MacAddress { octets },
                    bus_info: &usb_bus,
                })
                .is_err()
        );
    }

    // Arbitrary values for devices::DeviceInfo for cases where DeviceInfo has
    // no impact on the test.
    fn default_device_info() -> DeviceInfoRef<'static> {
        DeviceInfoRef {
            device_class: DeviceClass::Ethernet,
            mac: &fidl_fuchsia_net_ext::MacAddress { octets: [0x1, 0x1, 0x1, 0x1, 0x1, 0x1] },
            bus_info: &[],
        }
    }

    #[test_case(
        pci_bus(0, 0x14, 0),
        vec![BusType::PCI],
        BusType::PCI,
        true,
        "0014";
        "pci_match"
    )]
    #[test_case(
        pci_bus(0, 0x14, 0),
        vec![BusType::USB, BusType::SDIO],
        BusType::PCI,
        false,
        "0014";
        "pci_no_match"
    )]
    #[test_case(
        usb_over_pci_bus(0, 0x14, 0, 4),
        vec![BusType::USB],
        BusType::USB,
        true,
        "0014";
        "pci_usb_match"
    )]
    #[test_case(
        usb_peripheral_dwc_bus(),
        vec![BusType::USB],
        BusType::USB,
        true,
        "050018";
        "dwc_usb_match"
    )]
    // Ensure that even though PCI is an ancestor bus for USB-over-PCI,
    // it resolves to USB rather than matching a PCI rule.
    #[test_case(
        usb_over_pci_bus(0, 0x14, 0, 4),
        vec![BusType::PCI, BusType::SDIO],
        BusType::USB,
        false,
        "0014";
        "usb_no_match"
    )]
    #[test_case(
        sdio_platform_char_bus(),
        vec![BusType::SDIO],
        BusType::SDIO,
        true,
        "05006";
        "sdio_match"
    )]
    #[test_case(
        sdio_dfv2_mmc_bus(1),
        vec![BusType::SDIO],
        BusType::SDIO,
        true,
        "ffffff";
        "sdio_dfv2_mmc_match"
    )]
    #[test_case(
        virtio_over_pci_bus(0, 0x1e, 0),
        vec![BusType::VirtIo],
        BusType::VirtIo,
        true,
        "001e";
        "virtio_match_alternate_location"
    )]
    #[test_case(
        virtio_over_pci_bus(0, 0x1e, 1),
        vec![BusType::VirtIo],
        BusType::VirtIo,
        true,
        "001e1";
        "virtio_match_alternate_no_trim"
    )]
    #[test_case(
        vec![fdf::BusInfo {
            bus: Some(fdf::BusType::Platform),
            address: Some(fdf::DeviceAddress::StringValue("unrecognized-bus-path".to_string())),
            ..Default::default()
        }],
        vec![BusType::Unknown],
        BusType::Unknown,
        true,
        "ffffff";
        "unknown_bus_match_unrecognized"
    )]
    fn test_interface_matching_and_naming_by_bus_properties(
        bus_info: Vec<fdf::BusInfo>,
        bus_types: Vec<BusType>,
        expected_bus_type: BusType,
        want_match: bool,
        want_name: &'static str,
    ) {
        let device_info = DeviceInfoRef { bus_info: &bus_info, ..default_device_info() };

        let bus_type = BusTopology(device_info.bus_info).bus_type();
        assert_eq!(bus_type, expected_bus_type);

        let matching_rule = MatchingRule::BusTypes(bus_types);
        let does_interface_match = matching_rule.does_interface_match(&device_info).unwrap();
        assert_eq!(does_interface_match, want_match);

        let name = BusTopology(device_info.bus_info).normalized_bus_path();
        assert_eq!(name, want_name);

        if want_name == "ffffff" {
            let name = BusTopology(device_info.bus_info).normalized_bus_path();
            assert_eq!(name, "fffffe");
        }
    }

    #[test_case(
        pci_bus(0, 0x14, 0),
        r"*[0-9][0-9]_[0-9][0-9]*",
        true;
        "pattern_matches"
    )]
    #[test_case(
        virtio_over_pci_bus(0, 4, 0),
        "**/00_04_0/**",
        true;
        "pci_bdf_glob_with_trailing_recursive_wildcard"
    )]
    #[test_case(
        pci_bus(0, 4, 0),
        "**/00_04_0",
        true;
        "pci_bdf_exact_leaf_glob"
    )]
    #[test_case(
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::DeviceTree),
                address: Some(fdf::DeviceAddress::StringValue("pci0".to_string())),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::Pci),
                address: Some(fdf::DeviceAddress::ArrayIntValue(vec![0, 3, 0])),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: Some(fdf::BusType::Virtio),
                address: None,
                ..Default::default()
            },
        ],
        "**/pci0/**/00_03_0/**",
        true;
        "devicetree_pci_root_matches"
    )]
    #[test_case(
        virtio_over_pci_bus(0, 3, 0),
        "**/pci0/**/00_03_0/**",
        false;
        "acpi_pci_does_not_match_devicetree_pci0"
    )]
    #[test_case(
        platform_eth_bus(),
        "**/ethernet-phy-*",
        true;
        "platform_eth_phy_prefix_matches"
    )]
    #[test_case(
        vec![
            fdf::BusInfo {
                bus: Some(fdf::BusType::DeviceTree),
                address: Some(fdf::DeviceAddress::StringValue("/netemul".to_string())),
                ..Default::default()
            },
            fdf::BusInfo {
                bus: None,
                address: Some(fdf::DeviceAddress::StringValue("ep".to_string())),
                ..Default::default()
            },
        ],
        "/netemul/ep",
        true;
        "leading_slash_trimmed_between_segments"
    )]
    #[test_case(pci_bus(0, 0x14, 0), r"*", true; "pattern_matches_any")]
    #[test_case(
        pci_bus(0, 0x14, 0),
        r"*[0-9][0-9]_00*",
        false;
        "no_matches"
    )]
    fn test_interface_matching_by_topological_path(
        bus_info: Vec<fdf::BusInfo>,
        glob_str: &'static str,
        want_match: bool,
    ) {
        let device_info = DeviceInfoRef { bus_info: &bus_info, ..default_device_info() };

        let matching_rule = MatchingRule::TopologicalPath(glob::Pattern::new(glob_str).unwrap());
        let does_interface_match = matching_rule.does_interface_match(&device_info).unwrap();
        assert_eq!(does_interface_match, want_match);
    }

    // Glob matches the default naming by MAC address.
    #[test_case(
        "ethx5",
        r"ethx[0-9]*",
        true;
        "pattern_matches"
    )]
    #[test_case("arbitraryname", r"*", true; "pattern_matches_any")]
    // Glob matches default naming by SDIO + bus path.
    #[test_case(
        "wlans1002",
        r"eths[0-9][0-9][0-9][0-9]*",
        false;
        "no_matches"
    )]
    fn test_interface_matching_by_interface_name(
        interface_name: &'static str,
        glob_str: &'static str,
        want_match: bool,
    ) {
        let provisioning_matching_rule = ProvisioningMatchingRule::InterfaceName {
            pattern: glob::Pattern::new(glob_str).unwrap(),
        };
        let does_interface_match = provisioning_matching_rule
            .does_interface_match(&default_device_info(), interface_name)
            .unwrap();
        assert_eq!(does_interface_match, want_match);
    }

    #[test_case(
        DeviceClass::Ethernet,
        vec![DeviceClass::Ethernet],
        true;
        "eth_match"
    )]
    #[test_case(
        DeviceClass::Ethernet,
        vec![DeviceClass::WlanClient, DeviceClass::WlanAp],
        false;
        "eth_no_match"
    )]
    #[test_case(
        DeviceClass::WlanClient,
        vec![DeviceClass::WlanClient],
        true;
        "wlan_match"
    )]
    #[test_case(
        DeviceClass::WlanClient,
        vec![DeviceClass::Ethernet, DeviceClass::WlanAp],
        false;
        "wlan_no_match"
    )]
    #[test_case(
        DeviceClass::WlanAp,
        vec![DeviceClass::WlanAp],
        true;
        "ap_match"
    )]
    #[test_case(
        DeviceClass::WlanAp,
        vec![DeviceClass::Ethernet, DeviceClass::WlanClient],
        false;
        "ap_no_match"
    )]
    fn test_interface_matching_by_device_class(
        device_class: DeviceClass,
        device_classes: Vec<DeviceClass>,
        want_match: bool,
    ) {
        let device_info = DeviceInfoRef { device_class, ..default_device_info() };

        let matching_rule = MatchingRule::DeviceClasses(device_classes);
        let does_interface_match = matching_rule.does_interface_match(&device_info).unwrap();
        assert_eq!(does_interface_match, want_match);
    }

    #[test_case(DeviceClass::Ethernet, usb_over_pci_bus(0, 0x15, 0, 4))]
    #[test_case(DeviceClass::WlanClient, pci_bus(0, 0x14, 0))]
    fn test_interface_matching_by_any_matching_rule(
        device_class: DeviceClass,
        bus_info: Vec<fdf::BusInfo>,
    ) {
        let device_info = DeviceInfoRef {
            device_class,
            mac: &fidl_fuchsia_net_ext::MacAddress { octets: [0x1, 0x1, 0x1, 0x1, 0x1, 0x1] },
            bus_info: &bus_info,
        };

        let matching_rule = MatchingRule::Any(true);
        let does_interface_match = matching_rule.does_interface_match(&device_info).unwrap();
        assert!(does_interface_match);

        let matching_rule = MatchingRule::Any(false);
        let does_interface_match = matching_rule.does_interface_match(&device_info).unwrap();
        assert!(!does_interface_match);
    }

    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        vec![MatchingRule::DeviceClasses(vec![DeviceClass::WlanClient])],
        false;
        "false_single_rule"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        vec![MatchingRule::DeviceClasses(vec![DeviceClass::WlanClient]), MatchingRule::Any(true)],
        false;
        "false_one_rule_of_multiple"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        vec![MatchingRule::Any(true)],
        true;
        "true_single_rule"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        vec![MatchingRule::DeviceClasses(vec![DeviceClass::Ethernet]), MatchingRule::Any(true)],
        true;
        "true_multiple_rules"
    )]
    fn test_does_interface_match(
        info: DeviceInfoRef<'_>,
        matching_rules: Vec<MatchingRule>,
        want_match: bool,
    ) {
        let naming_rule =
            NamingRule { matchers: HashSet::from_iter(matching_rules), naming_scheme: Vec::new() };
        assert_eq!(naming_rule.does_interface_match(&info), want_match);
    }

    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        "",
        vec![
            ProvisioningMatchingRule::Common(
                MatchingRule::DeviceClasses(vec![DeviceClass::WlanClient])
            )
        ],
        false;
        "false_single_rule"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::WlanClient, ..default_device_info() },
        "wlanx5009",
        vec![
            ProvisioningMatchingRule::InterfaceName {
                pattern: glob::Pattern::new("ethx*").unwrap()
            },
            ProvisioningMatchingRule::Common(MatchingRule::Any(true))
        ],
        false;
        "false_one_rule_of_multiple"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        "",
        vec![ProvisioningMatchingRule::Common(MatchingRule::Any(true))],
        true;
        "true_single_rule"
    )]
    #[test_case(
        DeviceInfoRef { device_class: DeviceClass::Ethernet, ..default_device_info() },
        "wlanx5009",
        vec![
            ProvisioningMatchingRule::Common(
                MatchingRule::DeviceClasses(vec![DeviceClass::Ethernet])
            ),
            ProvisioningMatchingRule::InterfaceName {
                pattern: glob::Pattern::new("wlanx*").unwrap()
            }
        ],
        true;
        "true_multiple_rules"
    )]
    fn test_does_interface_match_provisioning_rule(
        info: DeviceInfoRef<'_>,
        interface_name: &str,
        matching_rules: Vec<ProvisioningMatchingRule>,
        want_match: bool,
    ) {
        let provisioning_rule = ProvisioningRule {
            matchers: HashSet::from_iter(matching_rules),
            action: ProvisioningAction {
                provisioning: ProvisioningType::Local,
                ..Default::default()
            },
        };
        assert_eq!(provisioning_rule.does_interface_match(&info, interface_name), want_match);
    }

    #[test_case(
        vec![NameCompositionRule::Static { value: String::from("x") }],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "x";
        "single_static"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Static { value: String::from("eth") },
            NameCompositionRule::Static { value: String::from("x") },
            NameCompositionRule::Static { value: String::from("100") },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "ethx100";
        "multiple_static"
    )]
    #[test_case(
        vec![NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac }],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "1";
        "normalized_mac"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Static { value: String::from("eth") },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x9],
        vec![],
        "eth9";
        "normalized_mac_with_static"
    )]
    #[test_case(
        vec![NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass }],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "eth";
        "eth_device_class"
    )]
    #[test_case(
        vec![NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass }],
        DeviceClass::WlanClient,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "wlan";
        "wlan_device_class"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
            NameCompositionRule::Static { value: String::from("x") },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        vec![],
        "ethx";
        "device_class_with_static"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
            NameCompositionRule::Static { value: String::from("x") },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::NormalizedMac },
        ],
        DeviceClass::WlanClient,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x8],
        vec![],
        "wlanx8";
        "device_class_with_static_with_normalized_mac"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusType },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusPath },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        pci_bus(0, 0x14, 0),
        "ethp0014";
        "device_class_with_pci_bus_type_with_bus_path"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusType },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusPath },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        usb_over_pci_bus(0, 0x14, 0, 4),
        "ethu0014";
        "device_class_with_pci_usb_bus_type_with_bus_path"
    )]
    #[test_case(
        vec![
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::DeviceClass },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusType },
            NameCompositionRule::Dynamic { rule: DynamicNameCompositionRule::BusPath },
        ],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        usb_peripheral_dwc_bus(),
        "ethu050018";
        "device_class_with_dwc_usb_bus_type_with_bus_path"
    )]
    #[test_case(
        vec![NameCompositionRule::Default],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x2],
        usb_over_pci_bus(0, 0x14, 0, 4),
        "ethx2";
        "default_usb_pci"
    )]
    #[test_case(
        vec![NameCompositionRule::Default],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x3],
        usb_peripheral_dwc_bus(),
        "ethx3";
        "default_usb_dwc"
    )]
    #[test_case(
        vec![NameCompositionRule::Default],
        DeviceClass::Ethernet,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        sdio_platform_char_bus(),
        "eths1";
        "default_sdio_ethernet"
    )]
    #[test_case(
        vec![NameCompositionRule::Default],
        DeviceClass::WlanClient,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        sdio_platform_char_bus(),
        "wlans1";
        "default_sdio_wlan"
    )]
    #[test_case(
        vec![NameCompositionRule::Default],
        DeviceClass::WlanClient,
        [0x1, 0x1, 0x1, 0x1, 0x1, 0x1],
        sdio_dfv2_mmc_bus(1),
        "wlans1";
        "default_sdio_wlan_dfv2"
    )]
    fn test_naming_rules(
        composition_rules: Vec<NameCompositionRule>,
        device_class: DeviceClass,
        mac_octets: [u8; 6],
        bus_info: Vec<fdf::BusInfo>,
        expected_name: &'static str,
    ) {
        let info = DeviceInfoRef {
            device_class,
            mac: &fidl_fuchsia_net_ext::MacAddress { octets: mac_octets },
            bus_info: &bus_info,
        };
        let naming_rule = NamingRule { matchers: HashSet::new(), naming_scheme: composition_rules };

        let name = naming_rule.generate_name(&HashMap::new(), &info);
        assert_eq!(name.unwrap(), expected_name.to_owned());
    }

    #[test]
    fn test_get_normalized_bus_path_all_zeroes_falls_back_to_uniq_marker() {
        let bus = pci_bus(0, 0, 0);
        let first = BusTopology(&bus).normalized_bus_path();
        let second = BusTopology(&bus).normalized_bus_path();
        assert!(!first.is_empty());
        assert_eq!(first.len(), 6);
        assert!(u32::from_str_radix(&first, 16).is_ok());
        assert_ne!(first, second);
    }

    #[test]
    fn test_generate_name_from_naming_rule_interface_name_exists_no_reattempt() {
        let usb_bus = usb_over_pci_bus(0, 0x14, 0, 4);

        let shared_interface_name = "x".to_owned();
        let mut interfaces = HashMap::new();
        assert_matches!(
            interfaces.insert(
                generate_identifier(
                    &fidl_fuchsia_net_ext::MacAddress { octets: [0x1, 0x1, 0x1, 0x1, 0x1, 0x1] },
                    &usb_bus,
                ),
                shared_interface_name.clone(),
            ),
            None
        );

        let naming_rule = NamingRule {
            matchers: HashSet::new(),
            naming_scheme: vec![NameCompositionRule::Static {
                value: shared_interface_name.clone(),
            }],
        };

        let name = naming_rule.generate_name(&interfaces, &default_device_info()).unwrap();
        assert_eq!(name, shared_interface_name);
    }

    #[test]
    fn test_generate_name_from_naming_rule_many_unique_macs() {
        let usb_bus = usb_over_pci_bus(0, 0x14, 0, 4);

        let naming_rule = NamingRule {
            matchers: HashSet::new(),
            naming_scheme: vec![NameCompositionRule::Dynamic {
                rule: DynamicNameCompositionRule::NormalizedMac,
            }],
        };

        let mut interfaces = HashMap::new();

        for n in 0u8..255u8 {
            let octets = [0x01, 0x01, 0x01, 0x01, 0x01, n];
            let interface_naming_id =
                generate_identifier(&fidl_fuchsia_net_ext::MacAddress { octets }, &usb_bus);
            let info = DeviceInfoRef {
                device_class: DeviceClass::Ethernet,
                mac: &fidl_fuchsia_net_ext::MacAddress { octets },
                bus_info: &usb_bus,
            };

            let name =
                naming_rule.generate_name(&interfaces, &info).expect("failed to generate the name");
            assert_eq!(name, format!("{n:x}"));

            assert_matches!(interfaces.insert(interface_naming_id, name.clone()), None);
        }
    }

    #[test_case(true, "x"; "matches_first_rule")]
    #[test_case(false, "ethx1"; "fallback_default")]
    fn test_generate_name_from_naming_rules(match_first_rule: bool, expected_name: &'static str) {
        let usb_bus = usb_over_pci_bus(0, 0x14, 0, 4);
        let info = DeviceInfoRef {
            device_class: DeviceClass::Ethernet,
            mac: &fidl_fuchsia_net_ext::MacAddress { octets: [0x1, 0x1, 0x1, 0x1, 0x1, 0x1] },
            bus_info: &usb_bus,
        };
        let name = generate_name_from_naming_rules(
            &[
                NamingRule {
                    matchers: HashSet::from([MatchingRule::Any(match_first_rule)]),
                    naming_scheme: vec![NameCompositionRule::Static { value: String::from("x") }],
                },
                NamingRule {
                    matchers: HashSet::from([MatchingRule::Any(false)]),
                    naming_scheme: vec![NameCompositionRule::Static { value: String::from("y") }],
                },
            ],
            &HashMap::new(),
            &info,
        )
        .unwrap();
        assert_eq!(name, expected_name.to_owned());
    }

    #[test_case(true, ProvisioningType::Delegated; "matches_first_rule")]
    #[test_case(false, ProvisioningType::Local; "fallback_default")]
    fn test_find_provisioning_action_from_provisioning_rules(
        match_first_rule: bool,
        expected: ProvisioningType,
    ) {
        let provisioning_action = find_provisioning_action_from_provisioning_rules(
            &[ProvisioningRule {
                matchers: HashSet::from([ProvisioningMatchingRule::Common(MatchingRule::Any(
                    match_first_rule,
                ))]),
                action: ProvisioningAction {
                    provisioning: ProvisioningType::Delegated,
                    ..Default::default()
                },
            }],
            &DeviceInfoRef {
                device_class: DeviceClass::WlanClient,
                mac: &fidl_fuchsia_net_ext::MacAddress { octets: [0x1, 0x1, 0x1, 0x1, 0x1, 0x1] },
                bus_info: &[],
            },
            "wlans5009",
        );
        assert_eq!(provisioning_action.provisioning, expected);
    }
}
