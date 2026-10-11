// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::str;

const FREQUENCIES_HZ: [&'static str; 2] = ["1128000 1256000 1512000 2024000", "512000 1024000"];
const MAX_FREQUENCIES_HZ: [&'static str; 2] = ["2024000", "1024000"];
const MIN_FREQUENCIES_HZ: [&'static str; 2] = ["1128000", "512000"];

fn main() {
    println!("cpu_control_client started");
    check_cpufreq();
    println!("cpu_control_client done");
}

fn read_sysfs(path: &str) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

/// Domains discovered through `fuchsia.hardware.cpu.ctrl.Service` are not backed by
/// `DomainController`, so all writable cpufreq and `vote_manager/*` nodes fall back to `InertFile`.
fn check_inert_cpufreq_nodes(dir: &str, min_frequency_str: &str, max_frequency_str: &str) {
    for rel_path in [
        "scaling_min_freq",
        "vote_manager/powerhint_min_freq",
        "vote_manager/soft_min_freq",
        "vote_manager/debug_min_freq",
    ] {
        let path = format!("{dir}/{rel_path}");
        std::fs::write(&path, "1000000").unwrap();
        assert_eq!(&format!("{min_frequency_str}\n"), &read_sysfs(&path));
    }

    for rel_path in [
        "scaling_max_freq",
        "vote_manager/powerhint_max_freq",
        "vote_manager/thermal_max_freq",
        "vote_manager/soft_max_freq",
        "vote_manager/debug_max_freq",
    ] {
        let path = format!("{dir}/{rel_path}");
        std::fs::write(&path, "1000000").unwrap();
        assert_eq!(&format!("{max_frequency_str}\n"), &read_sysfs(&path));
    }

    std::fs::write(format!("{dir}/scaling_governor"), "schedutil").unwrap();
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_governor")));
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_available_governors")));
}

fn check_cpufreq() {
    assert_eq!(
        "0-5\n",
        str::from_utf8(&std::fs::read("/sys/devices/system/cpu/possible").unwrap()).unwrap()
    );
    assert_eq!(
        "0-5\n",
        str::from_utf8(&std::fs::read("/sys/devices/system/cpu/online").unwrap()).unwrap()
    );

    check_cpufreq_dir(0, 0);
    check_cpufreq_dir(1, 0);
    check_cpufreq_dir(2, 1);
    check_cpufreq_dir(3, 1);
    check_cpufreq_dir(4, 1);
    check_cpufreq_dir(5, 1);

    check_cpufreq_policy_dir(0, 0, "0 1");
    check_cpufreq_policy_dir(2, 1, "2 3 4 5");
}

fn check_cpufreq_policy_dir(policy_id: u64, cluster_id: u64, expected_related_cpus: &str) {
    let dir = format!("/sys/devices/system/cpu/cpufreq/policy{policy_id}");
    assert!(std::fs::exists(&dir).unwrap());

    let max_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(&format!("{max_frequency_str}\n"), &read_sysfs(&format!("{dir}/cpuinfo_max_freq")));

    let min_frequency_str = MIN_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(&format!("{min_frequency_str}\n"), &read_sysfs(&format!("{dir}/cpuinfo_min_freq")));

    let frequencies_str = FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{frequencies_str}\n"),
        &read_sysfs(&format!("{dir}/scaling_available_frequencies"))
    );

    assert_eq!(&format!("{expected_related_cpus}\n"), &read_sysfs(&format!("{dir}/related_cpus")));

    let cur_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(&format!("{cur_frequency_str}\n"), &read_sysfs(&format!("{dir}/scaling_cur_freq")));

    check_inert_cpufreq_nodes(&dir, min_frequency_str, max_frequency_str);
}

fn check_cpufreq_dir(core_id: u64, cluster_id: u64) {
    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}")).unwrap());
    let dir = format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq");
    assert!(std::fs::exists(&dir).unwrap());

    let max_frequency_str = MAX_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(&format!("{max_frequency_str}\n"), &read_sysfs(&format!("{dir}/cpuinfo_max_freq")));

    let min_frequency_str = MIN_FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(&format!("{min_frequency_str}\n"), &read_sysfs(&format!("{dir}/cpuinfo_min_freq")));

    let frequencies_str = FREQUENCIES_HZ[cluster_id as usize];
    assert_eq!(
        &format!("{frequencies_str}\n"),
        &read_sysfs(&format!("{dir}/scaling_available_frequencies"))
    );

    assert_eq!(&format!("{max_frequency_str}\n"), &read_sysfs(&format!("{dir}/scaling_cur_freq")));

    check_inert_cpufreq_nodes(&dir, min_frequency_str, max_frequency_str);

    assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}/topology")).unwrap());
    assert_eq!(
        &format!("{cluster_id}\n"),
        str::from_utf8(
            &std::fs::read(format!("/sys/devices/system/cpu/cpu{core_id}/topology/cluster_id"))
                .unwrap()
        )
        .unwrap()
    );
    assert_eq!(
        &format!("{cluster_id}\n"),
        str::from_utf8(
            &std::fs::read(format!(
                "/sys/devices/system/cpu/cpu{core_id}/topology/physical_package_id"
            ))
            .unwrap()
        )
        .unwrap()
    );
}
