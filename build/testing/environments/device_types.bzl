# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Swarming device types for `fx_test_environment(dimensions = ...)`."""

# LINT.IfChange(device_types)
# Supported device types. Like all environment dimension values, these are
# known to infra.
device_types = struct(
    aemu = "AEMU",
    astro = "Astro",
    atlas = "Atlas",
    crosvm = "crosvm",
    gce = "GCE",
    iris = "Iris",
    kola = "Kola",
    lilac = "Lilac",
    luis = "Luis",
    maple = "Maple",
    nelson = "Nelson",
    nuc7 = "Intel NUC Kit NUC7i5DNHE",
    nuc11 = "Intel NUC Kit NUC11TNHv5",
    qemu = "QEMU",
    sherlock = "Sherlock",
    sorrel = "Sorrel",
    vim3 = "Vim3",
)

# Supported host device types, for tests that need a specific emulator host.
host_device_types = struct(
    ampere_altra = "AmpereAltraMax-M128-30",
    ampere_one = "AmpereOne-A192-32X",
    gcp_c4a_highmem_96_bm = "GCP_C4A_HIGHMEM_96_BM",
    lattepanda_sigma_cell = "LATTEPANDA_SIGMA_CELL",
)
# LINT.ThenChange(//build/testing/environments.gni:device_types)
