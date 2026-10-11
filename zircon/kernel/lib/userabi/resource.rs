// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{HandleOwner, ResourceDispatcher};
#[cfg(target_arch = "x86_64")]
use zx_types::ZX_RSRC_KIND_IOPORT;
#[cfg(target_arch = "aarch64")]
use zx_types::ZX_RSRC_KIND_SMC;
use zx_types::{ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_SYSTEM, zx_rsrc_kind_t};

pub fn get_resource_handle(kind: zx_rsrc_kind_t) -> Option<HandleOwner> {
    let name: &[u8] = match kind {
        ZX_RSRC_KIND_MMIO => b"mmio",
        ZX_RSRC_KIND_IRQ => b"irq",
        #[cfg(target_arch = "x86_64")]
        ZX_RSRC_KIND_IOPORT => b"io_port",
        #[cfg(target_arch = "aarch64")]
        ZX_RSRC_KIND_SMC => b"smc",
        ZX_RSRC_KIND_SYSTEM => b"system",
        _ => {
            panic!("userboot doesn't get zx_rsrc_kind_t {kind}");
        }
    };
    let result = ResourceDispatcher::create_ranged_root(kind, name);
    assert!(result.is_ok());
    let (rsrc, rights) = result.unwrap();
    HandleOwner::make(rsrc, rights)
}

/// userboot tests
#[cfg(ktest)]
#[unittest::suite(name = "userboot")]
mod tests {
    use unittest::assert_true;

    /// get_ranged_resource
    #[test]
    fn get_ranged_resource() {
        let rsrc_handle = get_resource_handle(ZX_RSRC_KIND_MMIO).unwrap();
        let rsrc_dispatcher =
            rsrc_handle.dispatcher_ref().downcast::<ResourceDispatcher>().unwrap();

        let info = rsrc_dispatcher.get_info();
        assert_true!(info.kind == ZX_RSRC_KIND_MMIO);
        assert_true!(info.base == 0);
        assert_true!(info.size == 0);
    }
}
