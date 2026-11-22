// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Early-boot entropy collector quality test.
//!
//! When the kernel is built with `ENABLE_ENTROPY_COLLECTOR_TEST` (which Rust sees as
//! `cfg(enable_entropy_collector_test)`), `early_boot_test` draws a large sample from the entropy
//! source selected by `kernel.entropy-test.src` and later publishes it to userspace as a VMO so
//! that its quality can be analyzed offline.

#[cfg(enable_entropy_collector_test)]
pub use enabled::{ENTROPY_VMO, ENTROPY_VMO_STREAM_SIZE, ENTROPY_WAS_LOST, early_boot_test};

/// This function is always defined. If `ENABLE_ENTROPY_COLLECTOR_TEST` is set,
/// this function runs the early boot test. Otherwise, it does nothing.
#[cfg(not(enable_entropy_collector_test))]
pub fn early_boot_test() {}

#[cfg(enable_entropy_collector_test)]
mod enabled {
    use crate::crypto::entropy::collector::Collector;
    use crate::crypto::entropy::hw_rng_collector::HwRngCollector;
    use crate::crypto::entropy::jitterentropy_collector::JitterentropyCollector;
    use crate::platform_rs::timer::current_mono_time;
    use crate::vm::pmm::ALLOC_FLAG_ANY;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use boot_options::{BootOptions, EntropyTestSource};
    use core::cell::UnsafeCell;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use fbl::RefPtr;
    use kprint::kprintln;

    const MAX_ENTROPY_LENGTH: usize = 1 << 20;

    const fn get_max_entropy_length() -> usize {
        match option_env!("ENTROPY_COLLECTOR_TEST_MAXLEN") {
            Some(value) => zr::parse_usize(value).expect("ENTROPY_COLLECTOR_TEST_MAXLEN invalid"),
            None => MAX_ENTROPY_LENGTH,
        }
    }

    #[repr(transparent)]
    struct EntropyBuf(UnsafeCell<[u8; get_max_entropy_length()]>);

    // SAFETY: `ENTROPY_BUF` is written once in `early_boot_test`
    // (`LK_INIT_LEVEL_PLATFORM_EARLY + 2`) and read once in `setup_entropy_vmo`
    // (`LK_INIT_LEVEL_VM + 1`), both on the boot CPU before concurrent access is possible.
    unsafe impl Sync for EntropyBuf {}

    static ENTROPY_BUF: EntropyBuf = EntropyBuf(UnsafeCell::new([0; get_max_entropy_length()]));

    // `ENTROPY_LEN`, `ENTROPY_VMO_STREAM_SIZE` and `ENTROPY_WAS_LOST` are only ever accessed with
    // `Ordering::Relaxed`: every access happens on the boot CPU's main thread, in program order,
    // with no concurrent accessors. `early_boot_test` writes `ENTROPY_LEN` from `EarlyBootSeed`
    // (`LK_INIT_LEVEL_PLATFORM_EARLY + 2`), `setup_entropy_vmo` reads it and writes the other two
    // at `LK_INIT_LEVEL_VM + 1`, and `userboot_init` reads them from `lk_main` after
    // `LK_INIT_LEVEL_USER - 1`. The atomics exist to make the statics `Sync` without `unsafe`,
    // not to synchronize threads.
    static ENTROPY_LEN: AtomicUsize = AtomicUsize::new(0);

    /// Wrapper around `Option<RefPtr<VmObjectPaged>>` for the global `ENTROPY_VMO` static.
    #[repr(transparent)]
    pub struct EntropyVmo(pub UnsafeCell<Option<RefPtr<VmObjectPaged>>>);

    // SAFETY: `VmObjectPaged` is an internally synchronized C++ object with an atomic refcount,
    // and `ENTROPY_VMO` is written once in `setup_entropy_vmo` (`LK_INIT_LEVEL_VM + 1`) before
    // `userboot` reads it.
    unsafe impl Sync for EntropyVmo {}

    // These statics are read in `kernel/lib/userabi/userabi.rs`, in order to pass the `VmObject`
    // on to userboot (where it's added to the filesystem).
    pub static ENTROPY_VMO: EntropyVmo = EntropyVmo(UnsafeCell::new(None));
    pub static ENTROPY_VMO_STREAM_SIZE: AtomicUsize = AtomicUsize::new(0);
    pub static ENTROPY_WAS_LOST: AtomicBool = AtomicBool::new(false);

    fn setup_entropy_vmo(_level: init::LkInitLevel) {
        let entropy_len = ENTROPY_LEN.load(Ordering::Relaxed);
        let Ok(entropy_vmo) = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, entropy_len as u64) else {
            kprintln!("entropy-boot-test: Failed to create entropy_vmo (data lost)");
            ENTROPY_WAS_LOST.store(true, Ordering::Relaxed);
            return;
        };
        // SAFETY: `setup_entropy_vmo` runs once at `LK_INIT_LEVEL_VM + 1` before `userboot` reads
        // `ENTROPY_VMO`.
        unsafe { *ENTROPY_VMO.0.get() = Some(entropy_vmo.clone()) };
        // SAFETY: `ENTROPY_BUF` was initialized in `early_boot_test` and is not mutated afterwards.
        let entropy_buf = unsafe { &*ENTROPY_BUF.0.get() };
        if entropy_vmo.write(0, &entropy_buf[..entropy_len]).is_err() {
            kprintln!("entropy-boot-test: Failed to write to entropy_vmo (data lost)");
            ENTROPY_WAS_LOST.store(true, Ordering::Relaxed);
            return;
        }
        ENTROPY_VMO_STREAM_SIZE.store(entropy_len, Ordering::Relaxed);

        const NAME: &[u8] = b"debug/entropy.bin";
        if entropy_vmo.set_name(NAME).is_err() {
            // The name is needed because devmgr uses it to add the VMO as a file in
            // the /boot filesystem.
            kprintln!("entropy-boot-test: could not name entropy_vmo (data lost)");
            ENTROPY_WAS_LOST.store(true, Ordering::Relaxed);
        }
    }

    /// Run the entropy collector test.
    pub fn early_boot_test() {
        let collector_result: Result<&'static dyn Collector, _> = match BootOptions::get()
            .entropy_test_src
        {
            EntropyTestSource::HwRng => HwRngCollector::get_instance().map(|c| c as &dyn Collector),
            EntropyTestSource::JitterEntropy => {
                JitterentropyCollector::get_instance().map(|c| c as &dyn Collector)
            }
        };

        // TODO(andrewkrieger): add other entropy collectors.

        let Ok(collector) = collector_result else {
            kprintln!("entropy-boot-test: Failed to obtain entropy collector. Skipping test.");
            return;
        };

        // SAFETY: `early_boot_test` runs once during single-threaded early boot before
        // `setup_entropy_vmo` reads `ENTROPY_BUF`.
        let entropy_buf = unsafe { &mut *ENTROPY_BUF.0.get() };

        let mut entropy_len = BootOptions::get().entropy_test_len as usize;
        if entropy_len > entropy_buf.len() {
            entropy_len = entropy_buf.len();
            kprintln!(
                "entropy-boot-test: only recording {} bytes (try defining \
                 ENTROPY_COLLECTOR_TEST_MAXLEN)",
                entropy_buf.len()
            );
        }

        let start = current_mono_time();
        let result = collector.draw_entropy(&mut entropy_buf[..entropy_len]);
        let end = current_mono_time();

        if result < entropy_len {
            kprintln!("entropy-boot-test: source only returned {} bytes.", result);
            entropy_len = result;
        } else {
            kprintln!(
                "entropy-boot-test: successful draw in {} nanoseconds.",
                (end - start).into_nanos()
            );
        }
        ENTROPY_LEN.store(entropy_len, Ordering::Relaxed);
    }

    init::lk_init_hook!(
        setup_entropy_vmo,
        setup_entropy_vmo,
        init::LkInitLevel(init::LK_INIT_LEVEL_VM.0 + 1)
    );
}
