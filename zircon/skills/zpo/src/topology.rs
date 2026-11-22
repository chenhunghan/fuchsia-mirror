// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The device's CPU topology and the roles it loaded, as read from the device.
//!
//! The kernel charges deadline capacity in *normalized* work:
//! `time_slice_used_ns += runtime_ns * processing_rate(cpu)`, where the rate is
//! 1.0 for the fastest core at its top operating point. zpo reads the rates
//! the kernel used from the trace's `kernel:power` `Rate:CPU:n` counters; this
//! module supplies what the trace cannot: which CPUs share a performance
//! domain, each domain's top rate, and the roles in `/boot/config/profiles/`.
//!
//! Both come from the device (`cpuctl describe`, and the profiles files), and
//! each measurement saves its copy, so a measurement is always analyzed
//! against the topology and roles it ran with.
//!
//! Older `cpuctl` builds do not print which CPUs are in each domain. zpo then
//! still has the domains' rates, core counts and the roles, and only skips the
//! checks that need the mapping (see [`Topology::knows_cpus`]).

use crate::profiles_file::{Affinity, NS_PER_US, file_scope, parse_affinity, parse_us};
use crate::roles::{RoleParams, as_traced_us};
use anyhow::{Context as _, bail};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

/// A set of CPUs that change operating point together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Domain {
    pub id: i64,
    /// Empty when the device's `cpuctl` does not report core IDs.
    pub cpus: Vec<i64>,
    /// The number of CPUs in the domain, which every `cpuctl` reports.
    #[serde(default)]
    pub core_count: usize,
    /// The processing rate at the domain's top operating point, relative to
    /// the fastest domain (1.0).
    pub max_rate: f64,
}

/// A role as the device loaded it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Role {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capacity_us: Option<i64>,
    /// The period, or the deadline when the role has no period (RoleManager
    /// reads them that way).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_us: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<i64>,
    /// None means every CPU (no affinity, or `"all"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity: Option<Vec<i64>>,
    /// The file on the device that defines it, e.g. "board.profiles".
    pub file: String,
}

impl Role {
    /// (capacity_us, period_us) of a deadline role.
    pub fn deadline(&self) -> Option<(i64, i64)> {
        match (self.capacity_us, self.period_us) {
            (Some(c), Some(p)) if c > 0 && p > 0 => Some((c, p)),
            _ => None,
        }
    }
}

/// The device's performance domains and loaded roles.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Topology {
    /// Sorted by id.
    pub domains: Vec<Domain>,
    pub roles: IndexMap<String, Role>,
}

/// The lines `cpuctl describe` prints after a domain's core count, one per
/// CPU: ` + logical core id: 5`.
const CORE_ID: &str = "+ logical core id:";

/// Shown before measuring on a device whose `cpuctl` does not report core IDs.
pub const NO_CORE_IDS_TITLE: &str = "CPU fit checks unavailable";
/// See [`NO_CORE_IDS_TITLE`].
pub const NO_CORE_IDS_WARNING: &str = "This image's cpuctl doesn't report each performance \
domain's CPUs, so proposals won't check whether they fit the chosen CPUs. A newer image enables \
the check.";
/// The note on each proposal derived without the CPU mapping.
pub const NO_CORE_IDS_NOTE: &str =
    "CPU fit not checked: this image's cpuctl does not report core IDs";

/// The command that prints every profiles file the device loaded, each after
/// a `==> <path>` line.
pub const PROFILES_DUMP_CMD: &str =
    "for f in /boot/config/profiles/*.profiles; do echo \"==> $f\"; cat \"$f\"; echo; done";

impl Topology {
    /// Parses `cpuctl describe` (every domain) and the profiles files the
    /// device loaded, as (file name, text) in load order.
    pub fn parse(cpuctl_describe: &str, profiles: &[(String, String)]) -> anyhow::Result<Topology> {
        Ok(Topology { domains: parse_domains(cpuctl_describe)?, roles: parse_roles(profiles) })
    }

    /// The topology a measurement saved (`measurement.json`'s "topology").
    pub fn from_measurement(info: &serde_json::Value) -> anyhow::Result<Topology> {
        let t = info.get("topology").context(
            "this measurement has no saved CPU topology (it was recorded by an older zpo); \
             measure it again",
        )?;
        serde_json::from_value(t.clone()).context("the measurement's saved topology is malformed")
    }

    /// Whether every domain lists its CPUs. Without that, zpo cannot tell
    /// which domain (and so which top rate) a CPU has.
    pub fn knows_cpus(&self) -> bool {
        !self.domains.is_empty() && self.domains.iter().all(|d| !d.cpus.is_empty())
    }

    /// The number of CPUs: the domains' core counts, which do not need the
    /// CPU mapping (or the highest CPU listed, if that is more).
    pub fn ncpu(&self) -> i64 {
        let counted: usize = self.domains.iter().map(|d| d.core_count.max(d.cpus.len())).sum();
        let listed =
            1 + self.domains.iter().flat_map(|d| d.cpus.iter().copied()).max().unwrap_or(-1);
        listed.max(counted as i64)
    }

    /// The domain a CPU belongs to.
    pub fn domain_of(&self, cpu: i64) -> Option<&Domain> {
        self.domains.iter().find(|d| d.cpus.contains(&cpu))
    }

    /// The CPU's top processing rate. 1.0 (the fastest core) for a CPU that no
    /// domain lists, including every CPU when `cpuctl` does not report core
    /// IDs: no rate is higher, so charging it can only overstate usage, which
    /// errs toward reporting throttling.
    pub fn rate(&self, cpu: i64) -> f64 {
        self.domain_of(cpu).map_or(1.0, |d| d.max_rate)
    }

    /// The slowest top rate among `cpus` (1.0 when empty).
    pub fn min_rate(&self, cpus: &[i64]) -> f64 {
        cpus.iter().map(|&c| self.rate(c)).reduce(f64::min).unwrap_or(1.0)
    }

    /// Every CPU, sorted: 0..ncpu, which does not need the CPU mapping.
    pub fn cpus(&self) -> Vec<i64> {
        (0..self.ncpu()).collect()
    }

    /// The affinity for a new deadline role, and why: the CPU set most loaded
    /// deadline roles share. Whoever wrote the device's roles chose it for
    /// deadline work, so a new role following it competes on the same terms.
    /// Ties go to the smaller set. Every CPU when no loaded deadline role
    /// names one.
    pub fn default_affinity(&self) -> (Vec<i64>, String) {
        let mut count: IndexMap<Vec<i64>, usize> = IndexMap::new();
        let mut total = 0;
        for r in self.roles.values().filter(|r| r.deadline().is_some()) {
            total += 1;
            if let Some(a) = &r.affinity {
                let mut a = a.clone();
                a.sort_unstable();
                *count.entry(a).or_default() += 1;
            }
        }
        let best = count
            .iter()
            .max_by(|(a, x), (b, y)| x.cmp(y).then(b.len().cmp(&a.len())).then(b.cmp(a)));
        match best {
            Some((a, n)) => (
                a.clone(),
                format!("CPUs {a:?}: the set {n} of the {total} loaded deadline roles use"),
            ),
            None => (self.cpus(), "every CPU: no loaded deadline role names an affinity".into()),
        }
    }

    /// The loaded deadline roles whose parameters, as a trace shows them,
    /// equal `(capacity_ns, period_ns)`, when they all share one affinity:
    /// (role names, affinity, None for every CPU). A thread running with
    /// those parameters most likely has one of these roles.
    pub fn affinity_for_params(
        &self,
        capacity_ns: i64,
        period_ns: i64,
    ) -> Option<(String, Option<Vec<i64>>)> {
        let (cap_us, per_us) = (capacity_ns.div_euclid(NS_PER_US), period_ns.div_euclid(NS_PER_US));
        let matches: Vec<(&String, &Role)> = self
            .roles
            .iter()
            .filter(|(_, r)| {
                r.deadline()
                    .is_some_and(|(c, p)| as_traced_us(c) == cap_us && as_traced_us(p) == per_us)
            })
            .collect();
        let (_, first) = matches.first()?;
        let aff = first.affinity.clone();
        if matches.iter().any(|(_, r)| r.affinity != aff) {
            return None;
        }
        let names: Vec<&str> = matches.iter().map(|(n, _)| n.as_str()).collect();
        Some((names.join(", "), aff))
    }

    /// The loaded roles as role suggestions take them.
    pub fn role_params(&self) -> IndexMap<String, RoleParams> {
        self.roles
            .iter()
            .map(|(n, r)| {
                let p = RoleParams {
                    capacity: r.capacity_us,
                    period: r.period_us,
                    deadline: None,
                    priority: r.priority,
                };
                (n.clone(), p)
            })
            .collect()
    }
}

/// Splits the output of [`PROFILES_DUMP_CMD`] into (file name, text).
pub fn split_profiles_dump(dump: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for line in dump.lines() {
        if let Some(path) = line.strip_prefix("==> ") {
            let name = path.rsplit('/').next().unwrap_or(path).trim().to_string();
            out.push((name, String::new()));
        } else if let Some((_, text)) = out.last_mut() {
            text.push_str(line);
            text.push('\n');
        }
    }
    out
}

fn parse_domains(text: &str) -> anyhow::Result<Vec<Domain>> {
    struct Raw {
        name: String,
        id: Option<i64>,
        count: Option<usize>,
        cpus: Vec<i64>,
        perf: Option<f64>,
    }
    let mut raws: Vec<Raw> = Vec::new();
    for line in text.lines().map(str::trim) {
        if let Some(name) = line.strip_prefix("Domain ") {
            raws.push(Raw {
                name: name.trim().into(),
                id: None,
                count: None,
                cpus: Vec::new(),
                perf: None,
            });
            continue;
        }
        let Some(r) = raws.last_mut() else { continue };
        let num = |v: &str| v.trim().parse::<i64>().ok();
        if let Some(v) = line.strip_prefix("logical core count:") {
            r.count = num(v).and_then(|n| usize::try_from(n).ok());
        } else if let Some(v) = line.strip_prefix(CORE_ID) {
            let Some(id) = num(v) else {
                bail!("`cpuctl describe`: domain {} has a malformed core id line", r.name);
            };
            r.cpus.push(id);
        } else if let Some(v) = line
            .strip_prefix("relative performance:")
            .or_else(|| line.strip_prefix("relative performance (deprecated):"))
        {
            r.perf = num(v).map(|p| p as f64);
        } else if let Some(v) = line.strip_prefix("domain_id:") {
            r.id = num(v);
        }
    }
    if raws.is_empty() {
        bail!("`cpuctl describe` on the device listed no CPU performance domains");
    }
    let top = raws.iter().filter_map(|r| r.perf).fold(0.0, f64::max);
    // The reported core IDs are used only when they are distinct across all
    // domains and are exactly 0..N-1 for the N cores in total; anything else
    // counts as no mapping, like an older cpuctl that prints no IDs. Some
    // drivers reply a placeholder instead of real IDs (aml-cpu replies 0 for
    // every core), and cpuctl only catches a repeat within one domain. It is
    // all or nothing: a partial mapping would charge the unlisted CPUs as the
    // fastest.
    let total: usize = raws.iter().map(|r| r.count.unwrap_or(r.cpus.len())).sum();
    let counts_match = raws.iter().all(|r| r.count.is_none_or(|n| n == r.cpus.len()));
    let mut all: Vec<i64> = raws.iter().flat_map(|r| r.cpus.iter().copied()).collect();
    all.sort_unstable();
    let mapped = counts_match && all.iter().copied().eq(0..total as i64);
    let mut domains = Vec::new();
    for (i, r) in raws.iter().enumerate() {
        let cpus = if mapped { r.cpus.clone() } else { Vec::new() };
        let Some(perf) = r.perf.filter(|_| top > 0.0) else {
            bail!("`cpuctl describe`: domain {} has no relative performance", r.name);
        };
        let core_count = r.count.unwrap_or(r.cpus.len());
        domains.push(Domain {
            id: r.id.unwrap_or(i as i64),
            cpus,
            core_count,
            max_rate: perf / top,
        });
    }
    domains.sort_by_key(|d| d.id);
    Ok(domains)
}

/// RoleManager's file scopes, lowest first: a role defined at a higher scope
/// replaces the same role at a lower one, and within a scope the first
/// definition wins.
const SCOPES: [&str; 4] = ["bringup", "core", "board", "product"];

fn scope_of(text: &str) -> usize {
    file_scope(text).and_then(|s| SCOPES.iter().position(|x| *x == s)).map_or(0, |i| i + 1)
}

fn parse_roles(files: &[(String, String)]) -> IndexMap<String, Role> {
    let mut roles: IndexMap<String, (usize, Role)> = IndexMap::new();
    for (file, text) in files {
        let scope = scope_of(text);
        for (name, r) in crate::roles::parse_roles_full(text) {
            let us = |v: &Option<String>| v.as_deref().and_then(|s| parse_us(s).ok());
            let role = Role {
                capacity_us: us(&r.capacity),
                // The kernel traces the relative deadline, which RoleManager
                // takes from "deadline" when it is given.
                period_us: us(&r.deadline).or_else(|| us(&r.period)),
                priority: r.priority,
                // Unreadable counts as every CPU, as RoleManager would not
                // pin the role to anything zpo could name.
                affinity: match r.affinity.as_deref().and_then(parse_affinity) {
                    Some(Affinity::Cpus(cpus)) => Some(cpus),
                    Some(Affinity::All) | None => None,
                },
                file: file.clone(),
            };
            match roles.get(&name) {
                Some((s, _)) if *s >= scope => {}
                _ => {
                    roles.insert(name, (scope, role));
                }
            }
        }
    }
    roles.into_iter().map(|(n, (_, r))| (n, r)).collect()
}

/// Output captured from a device with 8 CPUs in 4 performance domains, trimmed
/// to two operating points per domain and a sample of its roles. The
/// "logical core id" lines are added: the device's cpuctl predates them.
#[cfg(test)]
pub mod testing {
    use super::*;

    /// [`CPUCTL_DESCRIBE`] as a cpuctl without core IDs prints it.
    pub fn cpuctl_describe_without_ids() -> String {
        CPUCTL_DESCRIBE
            .lines()
            .filter(|l| !l.trim().starts_with(CORE_ID))
            .map(|l| format!("{l}\n"))
            .collect()
    }

    /// [`sample`] read through a cpuctl without core IDs.
    pub fn sample_without_ids() -> Topology {
        Topology::parse(&cpuctl_describe_without_ids(), &split_profiles_dump(PROFILES_DUMP))
            .unwrap()
    }

    pub const CPUCTL_DESCRIBE: &str = "\
Domain 1d2fd6418b8707e0c2dce7df423a2c05
logical core count: 1
 + logical core id: 7
relative performance: 1000
domain_id: 3
 + opps: 0
   - freq: 3782000000hz
   - volt: (unknown)
 + opps: 1
   - freq: 3590000000hz
   - volt: (unknown)
Domain 6ee7266066ad238f38d159ff91a692b2
logical core count: 2
 + logical core id: 5
 + logical core id: 6
relative performance: 806
domain_id: 2
 + opps: 0
   - freq: 3052000000hz
   - volt: (unknown)
Domain c22611a4df58dde531649b138f36d0cc
logical core count: 3
 + logical core id: 2
 + logical core id: 3
 + logical core id: 4
relative performance: 805
domain_id: 1
 + opps: 0
   - freq: 3052000000hz
   - volt: (unknown)
Domain df25b38f78172fdb5e5b05529e090592
logical core count: 2
 + logical core id: 0
 + logical core id: 1
relative performance: 202
domain_id: 0
 + opps: 0
   - freq: 2246000000hz
   - volt: (unknown)
";

    /// The same device's profiles: a core-scope file as written in the
    /// checkout (comments, trailing commas) and the board file as installed
    /// (one line of JSON).
    pub const PROFILES_DUMP: &str = r#"==> /boot/config/profiles/core.profiles
// Profiles common to core and derivatives.
{
  "scope": "core",
  "profiles": {
    // Fuchsia platform profiles.
    "fuchsia.scenic.input": { "capacity": "450us", "period": "4ms" },
    "fuchsia.netstack.go-worker": { "priority": 25 },
  },
}

==> /boot/config/profiles/sample.profiles
{"scope":"board","profiles":{"fuchsia.scenic.input":{"capacity":"240us","period":"4ms","critical":true,"affinity":[2,3,4,5,6]},"fuchsia.devices.input.synaptics.s3920t":{"capacity":"245us","period":"850us","affinity":[2,3,4,5,6]},"fuchsia.devices.spi.drivers.spi.bus":{"capacity":"245us","period":"850us","affinity":[2,3,4,5,6]},"fuchsia.devices.i2c.drivers.i2c.bus":{"capacity":"270us","period":"1ms","affinity":[2,3,4,5,6]},"fuchsia.ui.scene_manager":{"capacity":"270us","period":"4ms","critical":true,"affinity":[2,3,4,5,6]},"sample.render_thread":{"capacity":"2600us","period":"3ms","affinity":[2,3,4,5,6,7]},"sample.top_app.main":{"priority":28,"affinity":[7]},"sample.background":{"priority":11,"affinity":[0,1]}},"memory":{"fuchsia.ui.scene_manager":{"priority":24}}}
==> /boot/config/profiles/starnix.profiles
{
  "scope": "core",
  "profiles": {
    "fuchsia.starnix.fair.0": { "priority": 0, "affinity": "all" },
    "fuchsia.starnix.realtime": { "capacity": "19ms", "period": "20ms", "affinity": "all" },
  },
}
"#;

    /// The topology the fixtures describe.
    pub fn sample() -> Topology {
        Topology::parse(CPUCTL_DESCRIBE, &split_profiles_dump(PROFILES_DUMP)).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn reads_domains_from_cpuctl() {
        let t = sample();
        let got: Vec<(i64, Vec<i64>, f64)> =
            t.domains.iter().map(|d| (d.id, d.cpus.clone(), d.max_rate)).collect();
        assert_eq!(
            got,
            vec![
                (0, vec![0, 1], 0.202),
                (1, vec![2, 3, 4], 0.805),
                (2, vec![5, 6], 0.806),
                (3, vec![7], 1.0),
            ]
        );
        assert_eq!(t.ncpu(), 8);
        assert_eq!(t.rate(3), 0.805);
        assert_eq!(t.rate(99), 1.0);
        assert_eq!(t.min_rate(&[5, 7]), 0.806);
        assert_eq!(t.min_rate(&[]), 1.0);
    }

    #[test]
    fn an_old_cpuctl_gives_rates_and_counts_but_no_cpus() {
        let t = sample_without_ids();
        assert!(sample().knows_cpus());
        assert!(!t.knows_cpus());
        let got: Vec<(i64, Vec<i64>, usize, f64)> =
            t.domains.iter().map(|d| (d.id, d.cpus.clone(), d.core_count, d.max_rate)).collect();
        assert_eq!(
            got,
            vec![
                (0, vec![], 2, 0.202),
                (1, vec![], 3, 0.805),
                (2, vec![], 2, 0.806),
                (3, vec![], 1, 1.0),
            ]
        );
        // The CPU count, the default affinity and the roles do not need the
        // mapping; rates do, so every CPU is charged as the fastest.
        assert_eq!(t.ncpu(), 8);
        assert_eq!(t.default_affinity(), sample().default_affinity());
        assert_eq!(t.roles, sample().roles);
        assert_eq!(t.rate(0), 1.0);
        let none = Topology { domains: t.domains.clone(), roles: IndexMap::new() };
        assert_eq!(none.default_affinity().0, (0..8).collect::<Vec<_>>());
        // It survives the round trip through measurement.json.
        let info = serde_json::json!({"topology": t});
        assert_eq!(Topology::from_measurement(&info).unwrap(), t);
    }

    #[test]
    fn malformed_cpuctl_output_is_an_error() {
        assert!(Topology::parse("", &[]).is_err());
        let bad = CPUCTL_DESCRIBE.replace("logical core id: 6", "logical core id: six");
        assert!(Topology::parse(&bad, &[]).unwrap_err().to_string().contains("malformed"));
    }

    /// The sample device's `cpuctl describe` with `ids[d]` as the core IDs of the
    /// d-th domain it lists (big, mid, mid, little).
    fn describe_with_ids(ids: &[&[i64]]) -> String {
        let mut out = String::new();
        let mut d = 0;
        for l in cpuctl_describe_without_ids().lines() {
            out += l;
            out += "\n";
            if l.starts_with("logical core count:") {
                for id in ids[d] {
                    out += &format!(" + logical core id: {id}\n");
                }
                d += 1;
            }
        }
        out
    }

    #[test]
    fn core_ids_count_only_when_they_are_exactly_the_cpus() {
        let parse = |ids: &[&[i64]]| Topology::parse(&describe_with_ids(ids), &[]).unwrap();
        // Distinct and exactly 0..7: the mapping is used.
        let t = parse(&[&[7], &[5, 6], &[2, 3, 4], &[0, 1]]);
        assert!(t.knows_cpus());
        assert_eq!(t.domains, sample().domains);
        let degraded = |ids: &[&[i64]], why: &str| {
            let t = parse(ids);
            assert!(!t.knows_cpus(), "{why}");
            assert!(t.domains.iter().all(|d| d.cpus.is_empty()), "{why}");
            // Rates and core counts are kept.
            assert_eq!(t.domains, sample_without_ids().domains, "{why}");
            assert_eq!(t.ncpu(), 8, "{why}");
        };
        degraded(&[&[0], &[0, 0], &[0, 0, 0], &[0, 0]], "a placeholder 0 for every core");
        degraded(&[&[0], &[], &[], &[]], "cpuctl dropped the domains that repeat 0");
        degraded(&[&[7], &[5, 6], &[2, 3, 4], &[0, 2]], "2 in two domains");
        degraded(&[&[7], &[5, 6], &[2, 3, 4], &[0, 9]], "a gap: 1 missing, 9 past the end");
        degraded(&[&[7], &[5, 6], &[2, 3], &[0, 1]], "a domain lists fewer IDs than cores");
    }

    #[test]
    fn reads_roles_like_role_manager() {
        let t = sample();
        // The board file's definition replaces the core one.
        let si = &t.roles["fuchsia.scenic.input"];
        assert_eq!((si.capacity_us, si.period_us), (Some(240), Some(4000)));
        assert_eq!(si.affinity, Some(vec![2, 3, 4, 5, 6]));
        assert_eq!(si.file, "sample.profiles");
        // "all" is every CPU; fair roles have no deadline.
        assert_eq!(t.roles["fuchsia.starnix.realtime"].affinity, None);
        assert_eq!(t.roles["fuchsia.netstack.go-worker"].deadline(), None);
        assert_eq!(t.roles["sample.top_app.main"].affinity, Some(vec![7]));
        // Memory roles are not thread roles.
        assert_eq!(t.roles.len(), 11);
    }

    #[test]
    fn reads_scopes_affinities_and_deadlines_like_role_manager() {
        let files = [
            (
                "a.profiles".to_string(),
                r#"{
  "profiles": {
    "mask": { "capacity": "1ms", "period": "10ms", "affinity": 6 },
    "list": { "capacity": "1ms", "period": "10ms", "affinity": [
      4, // big
      5,
    ] },
    "constrained": { "capacity": "6ms", "deadline": "15ms", "period": "20ms" },
    "dup": { "priority": 1, "output_parameters": { "scope": "board" } },
  },
  "scope": "core",
}"#
                .to_string(),
            ),
            // No top-level scope: the nested "product" does not count.
            (
                "b.profiles".to_string(),
                r#"{ "profiles": { "dup": { "priority": 2, "output_parameters": { "scope": "product" } } } }"#
                    .to_string(),
            ),
        ];
        let t = Topology::parse(CPUCTL_DESCRIBE, &files).unwrap();
        assert_eq!(t.roles["mask"].affinity, Some(vec![1, 2]));
        assert_eq!(t.roles["list"].affinity, Some(vec![4, 5]));
        // The kernel traces the relative deadline, not the period.
        assert_eq!(t.roles["constrained"].deadline(), Some((6000, 15000)));
        // a.profiles is core scope (its top-level key, not the nested
        // "board"), so its definition beats the unscoped b.profiles.
        assert_eq!(t.roles["dup"].priority, Some(1));
    }

    #[test]
    fn new_roles_get_the_cpus_most_deadline_roles_share() {
        let (aff, why) = sample().default_affinity();
        assert_eq!(aff, vec![2, 3, 4, 5, 6]);
        assert_eq!(why, "CPUs [2, 3, 4, 5, 6]: the set 5 of the 7 loaded deadline roles use");
        let none = Topology { domains: sample().domains, roles: IndexMap::new() };
        assert_eq!(none.default_affinity().0, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn a_thread_matching_a_role_finds_its_affinity() {
        let t = sample();
        let (names, aff) = t.affinity_for_params(240_000, 4_000_000).unwrap();
        assert_eq!(names, "fuchsia.scenic.input");
        assert_eq!(aff, Some(vec![2, 3, 4, 5, 6]));
        assert_eq!(
            t.affinity_for_params(2_600_000, 3_000_000).unwrap().1,
            Some(vec![2, 3, 4, 5, 6, 7])
        );
        assert!(t.affinity_for_params(1_000_000, 3_000_000).is_none());
        // A role with no affinity (or "all") matches as unrestricted.
        let (names, aff) = t.affinity_for_params(19_000_000, 20_000_000).unwrap();
        assert_eq!((names.as_str(), aff), ("fuchsia.starnix.realtime", None));
    }

    #[test]
    fn round_trips_through_a_measurement() {
        let t = sample();
        let info = serde_json::json!({"topology": t});
        assert_eq!(Topology::from_measurement(&info).unwrap(), t);
        let err = Topology::from_measurement(&serde_json::json!({})).unwrap_err().to_string();
        assert!(err.contains("no saved CPU topology"), "{err}");
    }
}
