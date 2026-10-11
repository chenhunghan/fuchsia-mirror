// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::BTreeMap;
use std::sync::{Arc, LazyLock, Mutex};

use crate::inject::passthrough::Passthrough;
use crate::inject::{GenericVmoMemoryHandler, VmoMemoryHandler, VmoMemoryWrapper, VmoOpHelper};
use crate::operand::MmioOperand;

/// A trait marking a [`VmoMemoryHandler`] as suitable for use within a
/// [`Registry`].
pub trait RegistryHandler: VmoMemoryHandler + Send + Sync + 'static {}

impl<H: VmoMemoryHandler + Send + Sync + 'static> RegistryHandler for H {}

static REGISTRY: LazyLock<Mutex<BTreeMap<zx::Koid, Arc<dyn RegistryHandler>>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

/// A VMO handler registry that retrieves registered handlers by VMO KOID.
///
/// If `STRICT` is true, creating a handler for an unregistered VMO panics.
/// Otherwise, it falls back to [`Passthrough`].
///
/// Use [`BaseRegistry::register_handler`] or
/// [`ScopedRegistry::register_handler`] to register [`RegistryHandler`]
/// implementations that can be retrieved at-a-distance based on the VMO KOID.
///
/// Use this handler implementation in testing situations where the MMIO VMO
/// might come over FIDL from an injected dependency. Register the VMO in a
/// `Registry` before returning the VMO in an injected dependency fake.
pub struct BaseRegistry<const STRICT: bool>(Arc<dyn RegistryHandler>);

/// A registry that falls back to passthrough behavior for unregistered VMOs.
pub type Registry = BaseRegistry<false>;

/// A registry that panics when encountering an unregistered VMO.
pub type StrictRegistry = BaseRegistry<true>;

impl<const STRICT: bool> BaseRegistry<STRICT> {
    /// Registers `handler` for `vmo`.
    ///
    /// The handler is installed in the global registry and never removed. See
    /// [`ScopedRegistry`] for alternatives.
    pub fn register_handler<H: RegistryHandler>(vmo: &zx::Vmo, handler: H) {
        Self::register_shared_handler(vmo, Arc::new(handler))
    }

    /// Registers a shared reference-counted `handler` for `vmo`.
    ///
    /// The handler is installed in the global registry and never removed. See
    /// [`ScopedRegistry`] for alternatives.
    pub fn register_shared_handler(vmo: &zx::Vmo, handler: Arc<dyn RegistryHandler>) {
        let _: zx::Koid = register_global_handler(vmo, handler);
    }

    /// Clears all the registered VMOs.
    pub fn clear() {
        REGISTRY.lock().unwrap().clear();
    }
}

impl<const STRICT: bool> VmoMemoryWrapper for BaseRegistry<STRICT> {
    fn new(vmo: &zx::Vmo) -> Self {
        let koid = vmo.koid().expect("getting koid");
        let handler = REGISTRY.lock().unwrap().get(&koid).cloned();
        let handler = handler.unwrap_or_else(|| {
            if STRICT {
                panic!("no handler for vmo {koid:?}")
            }
            Arc::new(Passthrough)
        });
        Self(handler)
    }
}

impl<const STRICT: bool> GenericVmoMemoryHandler for BaseRegistry<STRICT> {
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T {
        T::load(&*self.0, op)
    }

    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T) {
        T::store(&*self.0, op, value)
    }
}

fn register_global_handler(vmo: &zx::Vmo, handler: Arc<dyn RegistryHandler>) -> zx::Koid {
    let koid = vmo.koid().expect("getting koid");
    assert!(REGISTRY.lock().unwrap().insert(koid, handler).is_none(), "already registered");
    koid
}

/// A helper to add VMOs to a global registry that removes all installed VMOs on
/// drop.
#[derive(Default, Clone, Debug)]
pub struct ScopedRegistry(Vec<zx::Koid>);

impl ScopedRegistry {
    /// Creates a new registry for registering VMOs.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `handler` for `vmo`.
    pub fn register_handler<H: RegistryHandler>(&mut self, vmo: &zx::Vmo, handler: H) {
        self.register_shared_handler(vmo, Arc::new(handler))
    }

    /// Registers a shared reference-counted `handler` for `vmo`.
    pub fn register_shared_handler(&mut self, vmo: &zx::Vmo, handler: Arc<dyn RegistryHandler>) {
        self.0.push(register_global_handler(vmo, handler));
    }

    /// Drops this `ScopedRegistry` without unregistering the VMOs.
    pub fn detach(mut self) {
        self.0.clear();
    }
}

impl Drop for ScopedRegistry {
    fn drop(&mut self) {
        let mut registry = REGISTRY.lock().unwrap();
        for koid in std::mem::take(&mut self.0) {
            let _ = registry.remove(&koid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inject::{MockVmoMemoryHandler, VmoMemory};
    use mmio::MmioExt as _;

    const VMO_SIZE: usize = 4096;

    #[test]
    fn test_registry_registered_vmo() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");
        let expected_val = 0x1234_5678_u32;
        let mut mock = MockVmoMemoryHandler::new();
        let _ = mock
            .expect_load32()
            .once()
            .withf(|op| op.offset() == 0 && op.range() == (0..4))
            .return_const(expected_val);
        // Scoped registry ensures we're going to run drop on `mock`.
        let mut registry = ScopedRegistry::new();
        registry.register_handler(&vmo, mock);

        let mmio = VmoMemory::<Registry>::map(0, VMO_SIZE, vmo).expect("Failed to map VMO.");
        assert_eq!(mmio.load::<u32>(0), expected_val);
    }

    #[test]
    fn test_registry_unregistered_vmo_fallback() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");

        let mut mmio = VmoMemory::<Registry>::map(0, VMO_SIZE, vmo).expect("Failed to map VMO.");
        let val = 0x8765_4321_u32;
        mmio.store::<u32>(0, val);
        assert_eq!(mmio.load::<u32>(0), val);
    }

    #[test]
    fn test_strict_registry_registered_vmo() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");
        let expected_val = 0xabcd_ef01_u32;
        let mut mock = MockVmoMemoryHandler::new();
        let _ = mock
            .expect_load32()
            .once()
            .withf(|op| op.offset() == 0 && op.range() == (0..4))
            .return_const(expected_val);
        // Scoped registry ensures we're going to run drop on `mock`.
        let mut registry = ScopedRegistry::new();
        registry.register_handler(&vmo, mock);

        let mmio = VmoMemory::<StrictRegistry>::map(0, VMO_SIZE, vmo).expect("Failed to map VMO.");
        assert_eq!(mmio.load::<u32>(0), expected_val);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "already registered")]
    fn test_registry_already_registered_panics() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");
        Registry::register_handler(&vmo, MockVmoMemoryHandler::new());
        Registry::register_handler(&vmo, MockVmoMemoryHandler::new());
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "no handler for vmo")]
    fn test_strict_registry_unregistered_vmo_panics() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");
        let _ = VmoMemory::<StrictRegistry>::map(0, VMO_SIZE, vmo);
    }

    #[test]
    fn test_scoped_registry_drop() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");

        {
            let expected_val = 0xabcd_ef01_u32;
            let mut mock = MockVmoMemoryHandler::new();
            let _ = mock
                .expect_load32()
                .once()
                .withf(|op| op.offset() == 0 && op.range() == (0..4))
                .return_const(expected_val);
            let mut scoped = ScopedRegistry::new();
            scoped.register_handler(&vmo, mock);

            let mmio = VmoMemory::<StrictRegistry>::map(
                0,
                VMO_SIZE,
                vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            )
            .expect("Failed to map VMO.");
            assert_eq!(mmio.load::<u32>(0), expected_val);
        }

        let mmio = VmoMemory::<Registry>::map(0, VMO_SIZE, vmo).expect("Failed to map VMO.");
        assert_eq!(mmio.load::<u32>(0), 0);
    }

    #[test]
    fn test_scoped_registry_detach() {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap()).expect("Failed to create VMO.");
        let expected_val = 0xabcd_ef01_u32;

        let mut mock = MockVmoMemoryHandler::new();
        let _ = mock
            .expect_load32()
            .times(2)
            .withf(|op| op.offset() == 0 && op.range() == (0..4))
            .return_const(expected_val);
        let mut scoped = ScopedRegistry::new();
        scoped.register_handler(&vmo, mock);

        let mmio = VmoMemory::<StrictRegistry>::map(
            0,
            VMO_SIZE,
            vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .expect("Failed to map VMO.");
        assert_eq!(mmio.load::<u32>(0), expected_val);

        scoped.detach();

        let mmio = VmoMemory::<StrictRegistry>::map(0, VMO_SIZE, vmo).expect("Failed to map VMO.");
        assert_eq!(mmio.load::<u32>(0), expected_val);

        // Clear the registry to run drop on mock.
        Registry::clear();
    }
}
