// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::str;

fn main() {
    println!("kernel_client started");
    check_cpufreq_kernel_fallback();
    println!("kernel_client done");
}

fn read_sysfs(path: &str) -> String {
    String::from_utf8(std::fs::read(path).unwrap()).unwrap()
}

fn check_inert_cpufreq_nodes(dir: &str) {
    for rel_path in [
        "scaling_min_freq",
        "vote_manager/powerhint_min_freq",
        "vote_manager/soft_min_freq",
        "vote_manager/debug_min_freq",
        "scaling_max_freq",
        "vote_manager/powerhint_max_freq",
        "vote_manager/thermal_max_freq",
        "vote_manager/soft_max_freq",
        "vote_manager/debug_max_freq",
    ] {
        let path = format!("{dir}/{rel_path}");
        std::fs::write(&path, "1000000").unwrap();
        assert_eq!("\n", &read_sysfs(&path));
    }

    std::fs::write(format!("{dir}/scaling_governor"), "schedutil").unwrap();
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_governor")));
    assert_eq!("performance\n", read_sysfs(&format!("{dir}/scaling_available_governors")));
}

fn check_cpufreq_kernel_fallback() {
    let possible_bytes = std::fs::read("/sys/devices/system/cpu/possible").unwrap();
    let possible_str = str::from_utf8(&possible_bytes).unwrap().trim();
    assert!(possible_str.starts_with("0-"));
    let max_core: u64 = possible_str.strip_prefix("0-").unwrap().parse().unwrap();
    let cpu_count = max_core + 1;

    let online_bytes = std::fs::read("/sys/devices/system/cpu/online").unwrap();
    let online_str = str::from_utf8(&online_bytes).unwrap().trim();
    assert_eq!(online_str, possible_str);

    for core_id in 0..cpu_count {
        assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}")).unwrap());
        let dir = format!("/sys/devices/system/cpu/cpu{core_id}/cpufreq");
        assert!(std::fs::exists(&dir).unwrap());
        assert!(std::fs::exists(format!("/sys/devices/system/cpu/cpu{core_id}/topology")).unwrap());

        assert_eq!(
            "0\n",
            str::from_utf8(
                &std::fs::read(format!("/sys/devices/system/cpu/cpu{core_id}/topology/cluster_id"))
                    .unwrap()
            )
            .unwrap()
        );
        assert_eq!(
            "0\n",
            str::from_utf8(
                &std::fs::read(format!(
                    "/sys/devices/system/cpu/cpu{core_id}/topology/physical_package_id"
                ))
                .unwrap()
            )
            .unwrap()
        );

        assert_eq!("\n", &read_sysfs(&format!("{dir}/scaling_available_frequencies")));
        assert_eq!("\n", &read_sysfs(&format!("{dir}/cpuinfo_max_freq")));
        assert_eq!("\n", &read_sysfs(&format!("{dir}/cpuinfo_min_freq")));

        check_inert_cpufreq_nodes(&dir);

        assert!(std::fs::read(format!("{dir}/scaling_cur_freq")).is_err());
    }

    let policy_dir = "/sys/devices/system/cpu/cpufreq/policy0";
    assert!(std::fs::exists(policy_dir).unwrap());
    let expected_related_cpus = (0..cpu_count).map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
    assert_eq!(
        &format!("{expected_related_cpus}\n"),
        &read_sysfs(&format!("{policy_dir}/related_cpus"))
    );
    assert_eq!("\n", &read_sysfs(&format!("{policy_dir}/scaling_available_frequencies")));
    assert_eq!("\n", &read_sysfs(&format!("{policy_dir}/cpuinfo_max_freq")));
    assert_eq!("\n", &read_sysfs(&format!("{policy_dir}/cpuinfo_min_freq")));
    check_inert_cpufreq_nodes(policy_dir);
    assert!(std::fs::read(format!("{policy_dir}/scaling_cur_freq")).is_err());
}
