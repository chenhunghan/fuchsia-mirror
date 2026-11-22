// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
use super::manager::DEFAULT_SYSTEM_MODE;
use crate::task::SCHED_CAPACITY_SCALE;
use anyhow::{Error, ensure};
use bstr::ByteSlice;
use regex_lite::Regex;
use starnix_task_command::TaskCommand;

/// Task identity specificity layer in the role-override waterfall.
///
/// Rules are evaluated from the most specific `(process, thread)` identity to the most general:
/// 1. `ProcessAndThread`: explicit process and thread command filters.
/// 2. `Thread`: explicit thread command filter across any process.
/// 3. `Process`: explicit process command filter across any thread.
/// 4. `Unidentified`: no process or thread command filter (matched purely by state modifiers).
#[derive(Debug)]
enum IdentityLayer {
    ProcessAndThread { process: Regex, thread: Regex },
    Thread { thread: Regex },
    Process { process: Regex },
    Unidentified,
}

impl IdentityLayer {
    fn new(process: Option<Regex>, thread: Option<Regex>) -> Self {
        match (process, thread) {
            (Some(process), Some(thread)) => Self::ProcessAndThread { process, thread },
            (None, Some(thread)) => Self::Thread { thread },
            (Some(process), None) => Self::Process { process },
            (None, None) => Self::Unidentified,
        }
    }

    /// Returns the waterfall evaluation rank (`1..=4`), where lower ranks have higher precedence.
    #[inline]
    fn rank(&self) -> u8 {
        match self {
            Self::ProcessAndThread { .. } => 1,
            Self::Thread { .. } => 2,
            Self::Process { .. } => 3,
            Self::Unidentified => 4,
        }
    }

    #[inline]
    fn matches(&self, process_name: &str, thread_name: &str) -> bool {
        match self {
            Self::ProcessAndThread { process, thread } => {
                process.is_match(process_name) && thread.is_match(thread_name)
            }
            Self::Thread { thread } => thread.is_match(thread_name),
            Self::Process { process } => process.is_match(process_name),
            Self::Unidentified => true,
        }
    }
}

/// Dynamic state predicates that qualify a rule within an [`IdentityLayer`].
///
/// Within the same [`IdentityLayer`], rules that constrain `cgroup` are evaluated before rules
/// that leave `cgroup` unconstrained, and within each of those buckets, rules with additional
/// state modifiers (`is_leader`, `min_nice`, `max_nice`, `uclamp_min`, `system_mode`) are
/// evaluated before unqualified baseline rules. If a rule's state modifiers do not match the
/// task's current runtime state, evaluation falls through to subsequent rules.
#[derive(Debug, Default)]
struct StateModifiers {
    cgroup: Option<Regex>,
    is_leader: Option<bool>,
    min_nice: Option<i8>,
    max_nice: Option<i8>,
    uclamp_min: Option<(u32, u32)>,
    system_mode: Option<Regex>,
}

impl StateModifiers {
    #[inline]
    fn has_non_cgroup_modifiers(&self) -> bool {
        self.is_leader.is_some()
            || self.min_nice.is_some()
            || self.max_nice.is_some()
            || self.uclamp_min.is_some()
            || self.system_mode.is_some()
    }

    /// Returns `(is_cgroup_unconstrained, has_no_other_modifiers)` for waterfall sorting, where
    /// lower values (`false` before `true`) have higher precedence.
    #[inline]
    fn specificity_key(&self) -> (bool, bool) {
        (self.cgroup.is_none(), !self.has_non_cgroup_modifiers())
    }

    #[inline]
    fn matches_without_system_mode(
        &self,
        cgroup_path: &str,
        is_leader: bool,
        nice: i8,
        uclamp_min: u32,
    ) -> bool {
        self.is_leader.is_none_or(|expected| expected == is_leader)
            && self.uclamp_min.is_none_or(|(min, max)| uclamp_min >= min && uclamp_min <= max)
            && self.min_nice.is_none_or(|min| nice >= min)
            && self.max_nice.is_none_or(|max| nice <= max)
            && self.cgroup.as_ref().is_none_or(|r| r.is_match(cgroup_path))
    }
}

#[derive(Debug)]
struct RoleOverrideRule {
    identity: IdentityLayer,
    state: StateModifiers,
    role_name: String,
}

/// Per-container overrides for thread roles, ordered by [`IdentityLayer`] and [`StateModifiers`]
/// precedence.
#[derive(Debug, Default)]
pub struct RoleOverrides {
    rules: Vec<RoleOverrideRule>,
}

impl RoleOverrides {
    /// Create a new builder for role overrides.
    pub fn new() -> RoleOverridesBuilder {
        RoleOverridesBuilder::default()
    }

    /// Returns true if transitioning from `previous_mode` to `new_mode` changes the match outcome
    /// of at least one `system_mode` filter rule.
    pub(super) fn has_system_mode_transition(&self, previous_mode: &str, new_mode: &str) -> bool {
        if previous_mode == new_mode {
            return false;
        }
        self.rules
            .iter()
            .filter_map(|rule| rule.state.system_mode.as_ref())
            .any(|filter| filter.is_match(previous_mode) != filter.is_match(new_mode))
    }

    /// Returns the role names of all override rules that can match in `mode` (both
    /// `system_mode`-filtered rules matching `mode` and unconditional fallback rules).
    pub(super) fn role_names_for_system_mode<'a>(
        &'a self,
        mode: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.rules.iter().filter_map(move |rule| {
            rule.state
                .system_mode
                .as_ref()
                .is_none_or(|regex| regex.is_match(mode))
                .then_some(rule.role_name.as_str())
        })
    }

    /// Get the overridden role name (if any) for provided process, thread, and cgroup names using
    /// default task attributes (`is_leader = false`, `nice = 0`, `uclamp_min = 0`, and
    /// `DEFAULT_SYSTEM_MODE`).
    #[cfg(test)]
    fn get_role_name(
        &self,
        process_name: &TaskCommand,
        thread_name: &TaskCommand,
        cgroup_path: &str,
    ) -> Option<&str> {
        self.get_role_name_with_filters(
            process_name,
            thread_name,
            cgroup_path,
            false,
            0,
            0,
            DEFAULT_SYSTEM_MODE,
        )
        .0
    }

    /// Returns `(role_name, is_system_mode_dependent)` for the provided task attributes and active
    /// `system_mode` in a single pass.
    ///
    /// `is_system_mode_dependent` is `true` if the task's attributes (`is_leader`, `uclamp_min`,
    /// `nice`, `cgroup_path`, `process_name`, `thread_name`) match at least one `system_mode`-gated
    /// rule before any unconditional rule match.
    pub(super) fn get_role_name_with_filters(
        &self,
        process_name: &TaskCommand,
        thread_name: &TaskCommand,
        cgroup_path: &str,
        is_leader: bool,
        nice: i8,
        uclamp_min: u32,
        system_mode: &str,
    ) -> (Option<&str>, bool) {
        if self.rules.is_empty() {
            return (None, false);
        }
        let Some(process_name) = process_name.as_bytes().to_str().ok() else {
            return (None, false);
        };
        let Some(thread_name) = thread_name.as_bytes().to_str().ok() else {
            return (None, false);
        };
        let mut is_system_mode_dependent = false;
        for rule in &self.rules {
            if rule.identity.matches(process_name, thread_name)
                && rule.state.matches_without_system_mode(cgroup_path, is_leader, nice, uclamp_min)
            {
                if let Some(mode_filter) = rule.state.system_mode.as_ref() {
                    is_system_mode_dependent = true;
                    if mode_filter.is_match(system_mode) {
                        return (Some(rule.role_name.as_str()), true);
                    }
                } else {
                    return (Some(rule.role_name.as_str()), is_system_mode_dependent);
                }
            }
        }
        (None, is_system_mode_dependent)
    }
}

#[derive(Default)]
struct RuleSpec {
    role_name: String,
    process: Option<String>,
    thread: Option<String>,
    cgroup: Option<String>,
    is_leader: Option<bool>,
    min_nice: Option<i8>,
    max_nice: Option<i8>,
    uclamp_min: Option<(u32, u32)>,
    system_mode: Option<String>,
}

/// Fluent builder for configuring a single role override rule.
pub struct RuleBuilder<'a> {
    spec: &'a mut RuleSpec,
}

impl<'a> RuleBuilder<'a> {
    /// Filter by process command pattern.
    pub fn process(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.spec.process = Some(pattern.into());
        self
    }

    /// Filter by optional process command pattern.
    pub fn with_process(&mut self, pattern: Option<impl Into<String>>) -> &mut Self {
        self.spec.process = pattern.map(Into::into);
        self
    }

    /// Filter by thread command pattern.
    pub fn thread(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.spec.thread = Some(pattern.into());
        self
    }

    /// Filter by optional thread command pattern.
    pub fn with_thread(&mut self, pattern: Option<impl Into<String>>) -> &mut Self {
        self.spec.thread = pattern.map(Into::into);
        self
    }

    /// Filter by cgroup path pattern.
    pub fn cgroup(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.spec.cgroup = Some(pattern.into());
        self
    }

    /// Filter by optional cgroup path pattern.
    pub fn with_cgroup(&mut self, pattern: Option<impl Into<String>>) -> &mut Self {
        self.spec.cgroup = pattern.map(Into::into);
        self
    }

    /// Filter by whether the task is the thread-group leader (`tid == pid`).
    pub fn is_leader(&mut self, is_leader: bool) -> &mut Self {
        self.spec.is_leader = Some(is_leader);
        self
    }

    /// Filter by optional thread-group leader status.
    pub fn with_is_leader(&mut self, is_leader: Option<bool>) -> &mut Self {
        self.spec.is_leader = is_leader;
        self
    }

    /// Filter by minimum Linux nice value (`-20..=19`, inclusive).
    pub fn min_nice(&mut self, min_nice: i8) -> &mut Self {
        self.spec.min_nice = Some(min_nice);
        self
    }

    /// Filter by optional minimum Linux nice value.
    pub fn with_min_nice(&mut self, min_nice: Option<i8>) -> &mut Self {
        self.spec.min_nice = min_nice;
        self
    }

    /// Filter by maximum Linux nice value (`-20..=19`, inclusive).
    pub fn max_nice(&mut self, max_nice: i8) -> &mut Self {
        self.spec.max_nice = Some(max_nice);
        self
    }

    /// Filter by optional maximum Linux nice value.
    pub fn with_max_nice(&mut self, max_nice: Option<i8>) -> &mut Self {
        self.spec.max_nice = max_nice;
        self
    }

    /// Filter by inclusive `[min, max]` range (`0..=1024`) for `uclamp.min`.
    pub fn uclamp_min(&mut self, range: (u32, u32)) -> &mut Self {
        self.spec.uclamp_min = Some(range);
        self
    }

    /// Filter by optional inclusive `[min, max]` range for `uclamp.min`.
    pub fn with_uclamp_min(&mut self, range: Option<(u32, u32)>) -> &mut Self {
        self.spec.uclamp_min = range;
        self
    }

    /// Filter by active system power mode pattern.
    pub fn system_mode(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.spec.system_mode = Some(pattern.into());
        self
    }

    /// Filter by optional active system power mode pattern.
    pub fn with_system_mode(&mut self, pattern: Option<impl Into<String>>) -> &mut Self {
        self.spec.system_mode = pattern.map(Into::into);
        self
    }
}

fn compile_identity_regex(pattern: Option<&str>) -> Result<Option<Regex>, regex_lite::Error> {
    // Treat legacy catch-all patterns as unconstrained (`None`) so manifests that still specify
    // required `".+"` / `".*"` strings sort into the intended lower `IdentityLayer` during
    // cross-repository rolls.
    match pattern {
        None | Some(".*" | ".+" | "^.*$" | "^.+$") => Ok(None),
        Some(pattern) => Regex::new(pattern).map(Some),
    }
}

/// Builder for `RoleOverrides`.
#[derive(Default)]
pub struct RoleOverridesBuilder {
    rules: Vec<RuleSpec>,
}

impl RoleOverridesBuilder {
    /// Begin adding a new role override rule for `role_name`.
    pub fn add(&mut self, role_name: impl Into<String>) -> RuleBuilder<'_> {
        self.rules.push(RuleSpec { role_name: role_name.into(), ..Default::default() });
        RuleBuilder { spec: self.rules.last_mut().unwrap() }
    }

    /// Compile all of the provided regular expressions and return a `RoleOverrides` sorted by
    /// waterfall specificity.
    pub fn build(self) -> Result<RoleOverrides, Error> {
        const MIN_NICE: i8 = -20;
        const MAX_NICE: i8 = 19;

        let mut rules = Vec::with_capacity(self.rules.len());
        for spec in self.rules {
            if let Some(min_nice) = spec.min_nice {
                ensure!(
                    (MIN_NICE..=MAX_NICE).contains(&min_nice),
                    "min_nice={min_nice} outside valid Linux range [{MIN_NICE}, {MAX_NICE}]"
                );
            }
            if let Some(max_nice) = spec.max_nice {
                ensure!(
                    (MIN_NICE..=MAX_NICE).contains(&max_nice),
                    "max_nice={max_nice} outside valid Linux range [{MIN_NICE}, {MAX_NICE}]"
                );
            }
            if let (Some(min_nice), Some(max_nice)) = (spec.min_nice, spec.max_nice) {
                ensure!(
                    min_nice <= max_nice,
                    "nice range [{min_nice}, {max_nice}] must satisfy min_nice <= max_nice"
                );
            }
            if let Some((min, max)) = spec.uclamp_min {
                ensure!(
                    min <= max && max <= SCHED_CAPACITY_SCALE,
                    "uclamp_min range [{min}, {max}] must satisfy min <= max <= {SCHED_CAPACITY_SCALE}"
                );
            }
            let process = compile_identity_regex(spec.process.as_deref())?;
            let thread = compile_identity_regex(spec.thread.as_deref())?;
            let cgroup = spec.cgroup.as_deref().map(Regex::new).transpose()?;
            let system_mode = spec.system_mode.as_deref().map(Regex::new).transpose()?;

            let identity = IdentityLayer::new(process, thread);
            let state = StateModifiers {
                cgroup,
                is_leader: spec.is_leader,
                min_nice: spec.min_nice,
                max_nice: spec.max_nice,
                uclamp_min: spec.uclamp_min,
                system_mode,
            };
            rules.push(RoleOverrideRule { identity, state, role_name: spec.role_name });
        }

        // Stable sort by identity layer first, then cgroup-constrained rules before cgroup-agnostic
        // rules, then state-modified rules before baseline rules, preserving declaration order
        // within each bucket.
        rules.sort_by_key(|rule| (rule.identity.rank(), rule.state.specificity_key()));

        Ok(RoleOverrides { rules })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_role_name<'a>(
        mappings: &'a RoleOverrides,
        process_name: &str,
        thread_name: &str,
        cpuset_path: &str,
    ) -> Option<&'a str> {
        mappings.get_role_name(
            &TaskCommand::new(process_name.as_bytes()),
            &TaskCommand::new(thread_name.as_bytes()),
            cpuset_path,
        )
    }

    #[fuchsia::test]
    fn single_pattern() {
        let mut builder = RoleOverrides::new();
        builder.add("replacement_role").process("process_prefix_.+").thread("thread_prefix_.+");
        let mappings = builder.build().unwrap();

        assert_eq!(
            str_role_name(&mappings, "process_prefix_foo", "thread_prefix_bar", "/"),
            Some("replacement_role")
        );
        assert_eq!(str_role_name(&mappings, "process_prefix_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "process_prefix_bar", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "non_matching", "/"), None);
    }

    #[fuchsia::test]
    fn multiple_patterns() {
        let mut builder = RoleOverrides::new();
        builder.add("replace_one").process("pre_one.+").thread("pre_one.+");
        builder.add("replace_two").process("pre_two.+").thread("pre_two.+");
        builder.add("replace_three").process("pre_three.+").thread("pre_three.+");
        builder.add("replace_four").process("pre_four.+").thread("pre_four.+");
        let mappings = builder.build().unwrap();

        assert_eq!(
            str_role_name(&mappings, "pre_one_foo", "pre_one_bar", "/"),
            Some("replace_one")
        );
        assert_eq!(str_role_name(&mappings, "pre_one_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_one_bar", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "non_matching", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_two_foo", "pre_two_bar", "/"),
            Some("replace_two")
        );
        assert_eq!(str_role_name(&mappings, "pre_two_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_two_bar", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_three_foo", "pre_three_bar", "/"),
            Some("replace_three")
        );
        assert_eq!(str_role_name(&mappings, "pre_three_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_three_bar", "/"), None);

        assert_eq!(
            str_role_name(&mappings, "pre_four_foo", "pre_four_bar", "/"),
            Some("replace_four")
        );
        assert_eq!(str_role_name(&mappings, "pre_four_foo", "non_matching", "/"), None);
        assert_eq!(str_role_name(&mappings, "non_matching", "pre_four_bar", "/"), None);
    }

    #[fuchsia::test]
    fn cgroup_patterns() {
        let mut builder = RoleOverrides::new();
        builder.add("bg_role").process("proc").thread("thread").cgroup("/background");
        builder.add("fg_role").process("proc").thread("thread").cgroup("/foreground");
        builder.add("default_role").process("proc").thread("thread");
        let mappings = builder.build().unwrap();

        assert_eq!(str_role_name(&mappings, "proc", "thread", "/background"), Some("bg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/foreground"), Some("fg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/other"), Some("default_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/"), Some("default_role"));
    }

    #[fuchsia::test]
    fn leader_and_nice_filters() {
        let mut builder = RoleOverrides::new();
        builder.add("leader_role").cgroup("^/active$").is_leader(true);
        builder.add("low_prio_worker_role").cgroup("^/active$").min_nice(1);
        builder.add("boosted_worker_role").cgroup("^/active$").max_nice(-1);
        builder.add("default_worker_role").cgroup("^/active$");
        let mappings = builder.build().unwrap();

        let proc_cmd = TaskCommand::new(b"app");
        let worker_cmd = TaskCommand::new(b"worker");

        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc_cmd,
                &proc_cmd,
                "/active",
                true,
                0,
                0,
                "NOMINAL",
            ),
            (Some("leader_role"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc_cmd,
                &worker_cmd,
                "/active",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("default_worker_role"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc_cmd,
                &worker_cmd,
                "/active",
                false,
                10,
                0,
                "NOMINAL",
            ),
            (Some("low_prio_worker_role"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc_cmd,
                &worker_cmd,
                "/active",
                false,
                -2,
                0,
                "NOMINAL",
            ),
            (Some("boosted_worker_role"), false)
        );
    }

    #[fuchsia::test]
    fn uclamp_min_and_system_mode_patterns() {
        let mut builder = RoleOverrides::new();
        builder
            .add("render_thread_high_boost")
            .thread("^RenderThread.*")
            .cgroup("^/top-app$")
            .uclamp_min((512, 1024));
        builder
            .add("render_thread_low_boost")
            .thread("^RenderThread.*")
            .cgroup("^/top-app$")
            .uclamp_min((1, 511));
        builder
            .add("render_thread_launch")
            .thread("^RenderThread.*")
            .system_mode("^(LAUNCH|LAUNCH_EXTEND)$");
        builder.add("render_thread_default").thread("^RenderThread.*");
        builder
            .add("launch_only_worker_role")
            .thread("^LaunchOnlyWorker$")
            .system_mode("^(LAUNCH|LAUNCH_EXTEND)$");
        let mappings = builder.build().unwrap();

        let proc = TaskCommand::new(b"com.example.app");
        let thread = TaskCommand::new(b"RenderThread");
        let launch_only_thread = TaskCommand::new(b"LaunchOnlyWorker");
        let other_thread = TaskCommand::new(b"WorkerThread");

        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc,
                &launch_only_thread,
                "/top-app",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (None, true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc,
                &launch_only_thread,
                "/top-app",
                false,
                0,
                0,
                "LAUNCH",
            ),
            (Some("launch_only_worker_role"), true)
        );

        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 0, "NOMINAL",),
            (Some("render_thread_default"), true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 0, "LAUNCH",),
            (Some("render_thread_launch"), true)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 1, "NOMINAL",),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 256, "NOMINAL",),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 511, "NOMINAL",),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 512, "NOMINAL",),
            (Some("render_thread_high_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 768, "NOMINAL",),
            (Some("render_thread_high_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 1024, "NOMINAL",),
            (Some("render_thread_high_boost"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc,
                &thread,
                "/background",
                false,
                0,
                768,
                "NOMINAL",
            ),
            (Some("render_thread_default"), true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &proc,
                &other_thread,
                "/top-app",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (None, false)
        );

        assert!(!mappings.has_system_mode_transition("NOMINAL", "NOMINAL"));
        assert!(!mappings.has_system_mode_transition("NOMINAL", "DISPLAY_INACTIVE"));
        assert!(!mappings.has_system_mode_transition("LAUNCH", "LAUNCH_EXTEND"));
        assert!(mappings.has_system_mode_transition("NOMINAL", "LAUNCH"));
        assert!(mappings.has_system_mode_transition("LAUNCH_EXTEND", "NOMINAL"));
        assert_eq!(
            mappings.role_names_for_system_mode("LAUNCH").collect::<Vec<_>>(),
            vec![
                "render_thread_high_boost",
                "render_thread_low_boost",
                "render_thread_launch",
                "launch_only_worker_role",
                "render_thread_default",
            ]
        );
        assert_eq!(
            mappings.role_names_for_system_mode("LAUNCH_EXTEND").collect::<Vec<_>>(),
            vec![
                "render_thread_high_boost",
                "render_thread_low_boost",
                "render_thread_launch",
                "launch_only_worker_role",
                "render_thread_default",
            ]
        );
        assert_eq!(
            mappings.role_names_for_system_mode("NOMINAL").collect::<Vec<_>>(),
            vec!["render_thread_high_boost", "render_thread_low_boost", "render_thread_default"]
        );

        let mut bad_regex_builder = RoleOverrides::new();
        bad_regex_builder.add("bad_regex_role").system_mode("[unclosed");
        let err = bad_regex_builder.build().expect_err("expected invalid system_mode regex");
        assert!(
            format!("{err:#}").contains("non-empty character class has no closing bracket"),
            "unexpected error: {err:#}"
        );
    }

    #[fuchsia::test]
    fn waterfall_layer_and_modifier_precedence_independent_of_declaration_order() {
        let mut builder = RoleOverrides::new();
        // Declare from least specific to most specific and baseline before state-modified to
        // verify build-time waterfall sorting.
        builder.add("cgroup_default").cgroup("^/top-app$");
        builder.add("cgroup_leader").cgroup("^/top-app$").is_leader(true);
        builder.add("process_default").process("^app$");
        builder.add("process_boosted").process("^app$").uclamp_min((1, 1024));
        builder.add("process_in_bg").process("^app$").cgroup("^/background$");
        builder.add("any_shared_worker").thread("^SharedWorker$");
        builder.add("any_render_thread").thread("^RenderThread$");
        builder.add("render_thread_bg").thread("^RenderThread$").cgroup("^/background$");
        builder.add("app_render_thread").process("^app$").thread("^RenderThread$");
        builder
            .add("app_render_thread_launch")
            .process("^app$")
            .thread("^RenderThread$")
            .system_mode("^LAUNCH$");
        let mappings = builder.build().unwrap();

        let app = TaskCommand::new(b"app");
        let other_proc = TaskCommand::new(b"other");
        let render = TaskCommand::new(b"RenderThread");
        let shared_worker = TaskCommand::new(b"SharedWorker");
        let worker = TaskCommand::new(b"worker");

        // Layer 1 (ProcessAndThread): state-modified beats unqualified, and beats Layer 2 (Thread).
        assert_eq!(
            mappings.get_role_name_with_filters(&app, &render, "/top-app", false, 0, 0, "LAUNCH",),
            (Some("app_render_thread_launch"), true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&app, &render, "/top-app", false, 0, 0, "NOMINAL",),
            (Some("app_render_thread"), true)
        );

        // Layer 2 (Thread): cgroup-constrained beats cgroup-agnostic, and beats Layer 3 (Process)
        // when no Layer 1 rule matches `(app, SharedWorker)`.
        assert_eq!(
            mappings.get_role_name_with_filters(
                &app,
                &shared_worker,
                "/background",
                false,
                0,
                512,
                "NOMINAL",
            ),
            (Some("any_shared_worker"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &other_proc,
                &render,
                "/background",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("render_thread_bg"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &other_proc,
                &render,
                "/top-app",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("any_render_thread"), false)
        );

        // Layer 3 (Process): cgroup-constrained beats state-modified (`uclamp_min = 512`), which
        // beats baseline, and all beat Layer 4 (Unidentified).
        assert_eq!(
            mappings.get_role_name_with_filters(
                &app,
                &worker,
                "/background",
                false,
                0,
                512,
                "NOMINAL",
            ),
            (Some("process_in_bg"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&app, &worker, "/top-app", false, 0, 512, "NOMINAL",),
            (Some("process_boosted"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&app, &worker, "/top-app", false, 0, 0, "NOMINAL",),
            (Some("process_default"), false)
        );

        // Layer 4 (Unidentified): state-modified (is_leader) beats unqualified cgroup baseline.
        assert_eq!(
            mappings.get_role_name_with_filters(
                &other_proc,
                &other_proc,
                "/top-app",
                true,
                0,
                0,
                "NOMINAL",
            ),
            (Some("cgroup_leader"), false)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(
                &other_proc,
                &worker,
                "/top-app",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("cgroup_default"), false)
        );
    }

    #[fuchsia::test]
    fn waterfall_cross_layer_fallthrough() {
        let mut builder = RoleOverrides::new();
        builder.add("layer4_cgroup").cgroup("^/top-app$");
        builder.add("layer3_proc_boosted").process("^app$").uclamp_min((512, 1024));
        builder.add("layer2_thread_launch").thread("^Worker$").system_mode("^LAUNCH$");
        builder
            .add("layer1_proc_thread_bg")
            .process("^app$")
            .thread("^Worker$")
            .cgroup("^/background$");
        let mappings = builder.build().unwrap();

        let app = TaskCommand::new(b"app");
        let worker = TaskCommand::new(b"Worker");
        let other_thread = TaskCommand::new(b"OtherThread");

        // Layer 1 matches when in `/background`.
        assert_eq!(
            mappings.get_role_name_with_filters(
                &app,
                &worker,
                "/background",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("layer1_proc_thread_bg"), false)
        );

        // Layer 1 (`cgroup`) fails -> falls through to Layer 2 (`system_mode == LAUNCH`).
        assert_eq!(
            mappings.get_role_name_with_filters(&app, &worker, "/top-app", false, 0, 0, "LAUNCH"),
            (Some("layer2_thread_launch"), true)
        );

        // Layer 1 (`cgroup`) and Layer 2 (`system_mode`) fail -> falls through to Layer 3
        // (`uclamp_min`), preserving `is_system_mode_dependent = true`.
        assert_eq!(
            mappings
                .get_role_name_with_filters(&app, &worker, "/top-app", false, 0, 768, "NOMINAL"),
            (Some("layer3_proc_boosted"), true)
        );

        // Layers 1, 2 (`system_mode`), and 3 (`uclamp_min`) all fail -> falls through to Layer 4
        // (`cgroup`), preserving `is_system_mode_dependent = true`.
        assert_eq!(
            mappings.get_role_name_with_filters(&app, &worker, "/top-app", false, 0, 0, "NOMINAL"),
            (Some("layer4_cgroup"), true)
        );

        // Non-`system_mode` modifier failure in Layer 3 (`uclamp_min`) without matching any
        // `system_mode`-gated rule falls through to Layer 4 with `is_system_mode_dependent = false`.
        assert_eq!(
            mappings.get_role_name_with_filters(
                &app,
                &other_thread,
                "/top-app",
                false,
                0,
                0,
                "NOMINAL",
            ),
            (Some("layer4_cgroup"), false)
        );
    }

    #[fuchsia::test]
    fn legacy_catch_all_patterns_normalized_to_unconstrained() {
        let mut builder = RoleOverrides::new();
        builder.add("bg_catch_all_plus").process(".+").thread(".+").cgroup("^/background$");
        builder.add("fg_catch_all_star").process(".*").thread(".*").cgroup("^/foreground$");
        builder.add("top_anchored_catch_all").process("^.+$").thread("^.*$").cgroup("^/top-app$");
        builder.add("render_thread").process(".+").thread("^RenderThread$");
        builder.add("composer_proc").process("^composer$").thread(".*");
        builder.add("sf_main").process("^surfaceflinger$").thread("^surfaceflinger$");
        let mappings = builder.build().unwrap();

        // Layer 1 beats legacy `.*` Layer 4 cgroup catch-all.
        assert_eq!(
            str_role_name(&mappings, "surfaceflinger", "surfaceflinger", "/foreground"),
            Some("sf_main")
        );
        // Layer 2 (`.+` process normalized to `None`) beats Layer 3 and Layer 4 (`^.+$` / `^.*$`).
        assert_eq!(
            str_role_name(&mappings, "composer", "RenderThread", "/top-app"),
            Some("render_thread")
        );
        // Layer 3 (`.*` thread normalized to `None`) beats Layer 4 (`.+` / `.+` cgroup catch-all).
        assert_eq!(
            str_role_name(&mappings, "composer", "binder:1", "/background"),
            Some("composer_proc")
        );
        // Layer 4 catch-alls still match tasks with no higher-layer identity match.
        assert_eq!(
            str_role_name(&mappings, "other", "worker", "/background"),
            Some("bg_catch_all_plus")
        );
        assert_eq!(
            str_role_name(&mappings, "other", "worker", "/foreground"),
            Some("fg_catch_all_star")
        );
        assert_eq!(
            str_role_name(&mappings, "other", "worker", "/top-app"),
            Some("top_anchored_catch_all")
        );
    }

    #[fuchsia::test]
    fn invalid_uclamp_range_rejected() {
        for (range, expected_err) in [
            ((600, 500), "uclamp_min range [600, 500] must satisfy min <= max <= 1024"),
            ((0, 1025), "uclamp_min range [0, 1025] must satisfy min <= max <= 1024"),
            ((1025, 2048), "uclamp_min range [1025, 2048] must satisfy min <= max <= 1024"),
        ] {
            let mut builder = RoleOverrides::new();
            builder.add("bad_range").uclamp_min(range);
            let err = builder.build().expect_err("expected invalid uclamp_min range to fail");
            let err_msg = format!("{err:#}");
            assert!(
                err_msg.contains(expected_err),
                "expected error containing {expected_err:?}, got {err_msg:?}"
            );
        }
    }

    #[fuchsia::test]
    fn invalid_nice_bounds_rejected() {
        for (min_nice, max_nice, expected_err) in [
            (Some(-21), None, "min_nice=-21 outside valid Linux range [-20, 19]"),
            (Some(20), None, "min_nice=20 outside valid Linux range [-20, 19]"),
            (None, Some(-21), "max_nice=-21 outside valid Linux range [-20, 19]"),
            (None, Some(20), "max_nice=20 outside valid Linux range [-20, 19]"),
            (Some(5), Some(-5), "nice range [5, -5] must satisfy min_nice <= max_nice"),
        ] {
            let mut builder = RoleOverrides::new();
            builder.add("bad_nice").with_min_nice(min_nice).with_max_nice(max_nice);
            let err = builder.build().expect_err("expected invalid nice bounds to fail");
            let err_msg = format!("{err:#}");
            assert!(
                err_msg.contains(expected_err),
                "expected error containing {expected_err:?}, got {err_msg:?}"
            );
        }

        // Valid inclusive boundaries must build cleanly.
        let mut valid = RoleOverrides::new();
        valid.add("full_range").min_nice(-20).max_nice(19);
        valid.add("exact_zero").min_nice(0).max_nice(0);
        valid.build().expect("valid nice bounds should succeed");
    }
}
