// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Host controller quirks.

use crate::constants::{INTEL_VENDOR_ID, QEMU_UFS_DEVICE_ID, REDHAT_VENDOR_ID};

/// Host controller quirks, detected from the PCI vendor and device IDs or implied by the platform
/// device bus.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HostQuirks {
    /// Intel host: 19.2 MHz `bRefClkFreq` and the link timing workarounds.
    pub intel: bool,
    /// QEMU: no UniPro attributes, power mode change or auto-hibernate.
    pub qemu: bool,
}

impl HostQuirks {
    /// Returns the quirks for a platform device host.
    pub fn for_pdev() -> Self {
        Self::default()
    }

    /// Returns the quirks for a PCI function.
    pub fn from_pci_ids(vendor_id: u16, device_id: u16) -> Self {
        let qemu = vendor_id == REDHAT_VENDOR_ID && device_id == QEMU_UFS_DEVICE_ID;
        Self { intel: !qemu && vendor_id == INTEL_VENDOR_ID, qemu }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_quirks_from_pci_ids() {
        assert_eq!(
            HostQuirks::from_pci_ids(REDHAT_VENDOR_ID, QEMU_UFS_DEVICE_ID),
            HostQuirks { qemu: true, ..Default::default() }
        );
        assert_eq!(HostQuirks::from_pci_ids(REDHAT_VENDOR_ID, 0x0001), HostQuirks::default());
        assert_eq!(
            HostQuirks::from_pci_ids(INTEL_VENDOR_ID, 0x98FA),
            HostQuirks { intel: true, ..Default::default() }
        );
        assert_eq!(HostQuirks::from_pci_ids(0x144D, QEMU_UFS_DEVICE_ID), HostQuirks::default());
    }
}
