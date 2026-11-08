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

/// Per-container overrides for thread roles.
#[derive(Debug, Default)]
pub struct RoleOverrides {
    process_filter: Vec<Regex>,
    thread_filter: Vec<Regex>,
    cgroup_filter: Vec<Option<Regex>>,
    is_leader_filter: Vec<Option<bool>>,
    min_nice_filter: Vec<Option<i8>>,
    max_nice_filter: Vec<Option<i8>>,
    uclamp_min_ranges: Vec<Option<(u32, u32)>>,
    system_mode_filter: Vec<Option<Regex>>,
    role_names: Vec<String>,
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
        self.system_mode_filter
            .iter()
            .flatten()
            .any(|filter| filter.is_match(previous_mode) != filter.is_match(new_mode))
    }

    /// Returns the role names of all override rules that can match in `mode` (both
    /// `system_mode`-filtered rules matching `mode` and unconditional fallback rules).
    pub(super) fn role_names_for_system_mode<'a>(
        &'a self,
        mode: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.system_mode_filter.iter().zip(self.role_names.iter()).filter_map(
            move |(filter, role_name)| {
                filter
                    .as_ref()
                    .is_none_or(|regex| regex.is_match(mode))
                    .then_some(role_name.as_str())
            },
        )
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
        debug_assert_eq!(self.process_filter.len(), self.role_names.len());
        debug_assert_eq!(self.thread_filter.len(), self.role_names.len());
        debug_assert_eq!(self.cgroup_filter.len(), self.role_names.len());
        debug_assert_eq!(self.is_leader_filter.len(), self.role_names.len());
        debug_assert_eq!(self.min_nice_filter.len(), self.role_names.len());
        debug_assert_eq!(self.max_nice_filter.len(), self.role_names.len());
        debug_assert_eq!(self.uclamp_min_ranges.len(), self.role_names.len());
        debug_assert_eq!(self.system_mode_filter.len(), self.role_names.len());

        // NOTE(https://fxbug.dev/483609435): This used to be more elegantly expressed
        // via use of regex::bytes::RegexSet, but regex_lite doesn't (yet?) offer RegexSet.
        let Some(process_name) = process_name.as_bytes().to_str().ok() else {
            return (None, false);
        };
        let Some(thread_name) = thread_name.as_bytes().to_str().ok() else {
            return (None, false);
        };
        let mut is_system_mode_dependent = false;
        for index in 0..self.process_filter.len() {
            if self.is_leader_filter[index].is_none_or(|expected| expected == is_leader)
                && self.uclamp_min_ranges[index]
                    .is_none_or(|(min, max)| uclamp_min >= min && uclamp_min <= max)
                && self.min_nice_filter[index].is_none_or(|min| nice >= min)
                && self.max_nice_filter[index].is_none_or(|max| nice <= max)
                && self.cgroup_filter[index].as_ref().is_none_or(|r| r.is_match(cgroup_path))
                && self.process_filter[index].is_match(process_name)
                && self.thread_filter[index].is_match(thread_name)
            {
                if let Some(mode_filter) = self.system_mode_filter[index].as_ref() {
                    is_system_mode_dependent = true;
                    if mode_filter.is_match(system_mode) {
                        return (Some(self.role_names[index].as_str()), true);
                    }
                } else {
                    return (Some(self.role_names[index].as_str()), is_system_mode_dependent);
                }
            }
        }
        (None, is_system_mode_dependent)
    }
}

/// Builder for `RoleOverrides`.
#[derive(Default)]
pub struct RoleOverridesBuilder {
    process_patterns: Vec<String>,
    thread_patterns: Vec<String>,
    cgroup_patterns: Vec<Option<String>>,
    is_leader_filters: Vec<Option<bool>>,
    min_nice_filters: Vec<Option<i8>>,
    max_nice_filters: Vec<Option<i8>>,
    uclamp_min_ranges: Vec<Option<(u32, u32)>>,
    system_mode_patterns: Vec<Option<String>>,
    role_names: Vec<String>,
}

impl RoleOverridesBuilder {
    /// Add a new override to the configuration.
    pub fn add(
        &mut self,
        process: impl Into<String>,
        thread: impl Into<String>,
        cgroup: Option<String>,
        role_name: impl Into<String>,
    ) {
        self.add_with_filters(process, thread, cgroup, None, None, None, None, None, role_name);
    }

    /// Add a new override with optional thread-group leader, nice range, `uclamp_min` range, and
    /// `system_mode` filters.
    pub fn add_with_filters(
        &mut self,
        process: impl Into<String>,
        thread: impl Into<String>,
        cgroup: Option<String>,
        is_leader: Option<bool>,
        min_nice: Option<i8>,
        max_nice: Option<i8>,
        uclamp_min_range: Option<(u32, u32)>,
        system_mode: Option<String>,
        role_name: impl Into<String>,
    ) {
        self.process_patterns.push(process.into());
        self.thread_patterns.push(thread.into());
        self.cgroup_patterns.push(cgroup);
        self.is_leader_filters.push(is_leader);
        self.min_nice_filters.push(min_nice);
        self.max_nice_filters.push(max_nice);
        self.uclamp_min_ranges.push(uclamp_min_range);
        self.system_mode_patterns.push(system_mode);
        self.role_names.push(role_name.into());
    }

    /// Compile all of the provided regular expressions and return a `RoleOverrides`.
    pub fn build(self) -> Result<RoleOverrides, Error> {
        let cgroup_filter = self
            .cgroup_patterns
            .into_iter()
            .map(|opt| opt.map(|p| Regex::new(p.as_str())).transpose())
            .collect::<Result<Vec<Option<Regex>>, regex_lite::Error>>()?;

        for &(min, max) in self.uclamp_min_ranges.iter().flatten() {
            ensure!(
                min <= max && max <= SCHED_CAPACITY_SCALE,
                "uclamp_min range [{min}, {max}] must satisfy min <= max <= {SCHED_CAPACITY_SCALE}"
            );
        }

        let system_mode_filter = self
            .system_mode_patterns
            .into_iter()
            .map(|opt| opt.map(|p| Regex::new(p.as_str())).transpose())
            .collect::<Result<Vec<Option<Regex>>, regex_lite::Error>>()?;

        Ok(RoleOverrides {
            process_filter: self
                .process_patterns
                .into_iter()
                .map(|pattern| Regex::new(pattern.as_str()))
                .collect::<Result<Vec<Regex>, regex_lite::Error>>()?,
            thread_filter: self
                .thread_patterns
                .into_iter()
                .map(|pattern| Regex::new(pattern.as_str()))
                .collect::<Result<Vec<Regex>, regex_lite::Error>>()?,
            cgroup_filter,
            is_leader_filter: self.is_leader_filters,
            min_nice_filter: self.min_nice_filters,
            max_nice_filter: self.max_nice_filters,
            uclamp_min_ranges: self.uclamp_min_ranges,
            system_mode_filter,
            role_names: self.role_names,
        })
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
        builder.add("process_prefix_.+", "thread_prefix_.+", None, "replacement_role");
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
        builder.add("pre_one.+", "pre_one.+", None, "replace_one");
        builder.add("pre_two.+", "pre_two.+", None, "replace_two");
        builder.add("pre_three.+", "pre_three.+", None, "replace_three");
        builder.add("pre_four.+", "pre_four.+", None, "replace_four");
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
        builder.add("proc", "thread", Some("/background".to_string()), "bg_role");
        builder.add("proc", "thread", Some("/foreground".to_string()), "fg_role");
        builder.add("proc", "thread", None, "default_role");
        let mappings = builder.build().unwrap();

        assert_eq!(str_role_name(&mappings, "proc", "thread", "/background"), Some("bg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/foreground"), Some("fg_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/other"), Some("default_role"));
        assert_eq!(str_role_name(&mappings, "proc", "thread", "/"), Some("default_role"));
    }

    #[fuchsia::test]
    fn leader_and_nice_filters() {
        let mut builder = RoleOverrides::new();
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            Some(true),
            None,
            None,
            None,
            None,
            "leader_role",
        );
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            None,
            Some(1),
            None,
            None,
            None,
            "low_prio_worker_role",
        );
        builder.add_with_filters(
            ".+",
            ".+",
            Some("^/active$".to_string()),
            None,
            None,
            Some(-1),
            None,
            None,
            "boosted_worker_role",
        );
        builder.add(".+", ".+", Some("^/active$".to_string()), "default_worker_role");
        let mappings = builder.build().unwrap();

        let proc_cmd = TaskCommand::new(b"app");
        let worker_cmd = TaskCommand::new(b"worker");

        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc_cmd, &proc_cmd, "/active", true, 0, 0, "NOMINAL"),
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
                "NOMINAL"
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
                "NOMINAL"
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
                "NOMINAL"
            ),
            (Some("boosted_worker_role"), false)
        );
    }

    #[fuchsia::test]
    fn uclamp_min_and_system_mode_patterns() {
        let mut builder = RoleOverrides::new();
        builder.add_with_filters(
            ".*",
            "^RenderThread.*",
            Some("^/top-app$".to_string()),
            None,
            None,
            None,
            Some((512, 1024)),
            None,
            "render_thread_high_boost",
        );
        builder.add_with_filters(
            ".*",
            "^RenderThread.*",
            Some("^/top-app$".to_string()),
            None,
            None,
            None,
            Some((1, 511)),
            None,
            "render_thread_low_boost",
        );
        builder.add_with_filters(
            ".*",
            "^RenderThread.*",
            None,
            None,
            None,
            None,
            None,
            Some("^(LAUNCH|LAUNCH_EXTEND)$".to_string()),
            "render_thread_launch",
        );
        builder.add(".*", "^RenderThread.*", None, "render_thread_default");
        builder.add_with_filters(
            ".*",
            "^LaunchOnlyWorker$",
            None,
            None,
            None,
            None,
            None,
            Some("^(LAUNCH|LAUNCH_EXTEND)$".to_string()),
            "launch_only_worker_role",
        );
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
                "NOMINAL"
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
                "LAUNCH"
            ),
            (Some("launch_only_worker_role"), true)
        );

        assert_eq!(
            mappings.get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 0, "NOMINAL"),
            (Some("render_thread_default"), true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 0, "LAUNCH"),
            (Some("render_thread_launch"), true)
        );
        assert_eq!(
            mappings.get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 1, "NOMINAL"),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 256, "NOMINAL"),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 511, "NOMINAL"),
            (Some("render_thread_low_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 512, "NOMINAL"),
            (Some("render_thread_high_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 768, "NOMINAL"),
            (Some("render_thread_high_boost"), false)
        );
        assert_eq!(
            mappings
                .get_role_name_with_filters(&proc, &thread, "/top-app", false, 0, 1024, "NOMINAL"),
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
                "NOMINAL"
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
                "NOMINAL"
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
                "render_thread_default",
                "launch_only_worker_role",
            ]
        );
        assert_eq!(
            mappings.role_names_for_system_mode("LAUNCH_EXTEND").collect::<Vec<_>>(),
            vec![
                "render_thread_high_boost",
                "render_thread_low_boost",
                "render_thread_launch",
                "render_thread_default",
                "launch_only_worker_role",
            ]
        );
        assert_eq!(
            mappings.role_names_for_system_mode("NOMINAL").collect::<Vec<_>>(),
            vec!["render_thread_high_boost", "render_thread_low_boost", "render_thread_default"]
        );

        let mut bad_regex_builder = RoleOverrides::new();
        bad_regex_builder.add_with_filters(
            ".*",
            ".*",
            None,
            None,
            None,
            None,
            None,
            Some("[unclosed".to_string()),
            "bad_regex_role",
        );
        let err = bad_regex_builder.build().expect_err("expected invalid system_mode regex");
        assert!(
            format!("{err:#}").contains("non-empty character class has no closing bracket"),
            "unexpected error: {err:#}"
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
            builder.add_with_filters(
                ".*",
                ".*",
                None,
                None,
                None,
                None,
                Some(range),
                None,
                "bad_range",
            );
            let err = builder.build().expect_err("expected invalid uclamp_min range to fail");
            let err_msg = format!("{err:#}");
            assert!(
                err_msg.contains(expected_err),
                "expected error containing {expected_err:?}, got {err_msg:?}"
            );
        }
    }
}
