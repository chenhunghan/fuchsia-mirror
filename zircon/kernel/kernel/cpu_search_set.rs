// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::cell::UnsafeCell;
use core::cmp::Ordering as CmpOrdering;
use core::convert::Infallible;
use core::mem::{align_of, offset_of, size_of};
use core::ops::Index;

use boot_options::BootOptions;
use debug::dprintf;
use lazy_init::LazyInit;
use percpu_bindings as bindings;
use pin_init::PinInit;

use crate::kernel::cpu_distance_map::CpuDistanceMap;
use crate::kernel::types::{BOOT_CPU_ID, SMP_MAX_CPUS, cpu_num_t};

// The following initializations depend on the boot CPU having logical id 0.
zr::static_assert!(BOOT_CPU_ID == 0);

/// Entry type for the list of CPUs.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Logical CPU number.
    pub cpu: cpu_num_t,
    /// Logical cluster index for `cpu`.
    pub cluster: usize,
}

/// Type representing a logical CPU cluster and its members.
#[derive(Default)]
struct Cluster {
    id: usize,
    members: fbl::Vector<cpu_num_t>,
}

/// Entry type for the logical CPU id to cluster map.
#[derive(Clone, Copy, Default)]
struct MapEntry {
    /// The cluster the logical CPU id belongs to (index in [`ClusterSet::clusters`]).
    cluster: usize,

    /// The index of the logical CPU in the [`Cluster::members`] list.
    index: usize,
}

/// Represents a set of logical CPU clusters.
struct ClusterSet {
    /// The list of logical clusters computed by auto-clustering.
    clusters: fbl::Vector<Cluster>,

    /// Map from logical CPU id to logical cluster.
    cpu_to_cluster_map: fbl::Vector<MapEntry>,
}

// TODO(https://fxbug.dev/575987745): Drop once `kalloc::Box` (and `fbl::Vector`) implements
// `Sync` when `T: Sync`.
// SAFETY: `ClusterSet` exclusively owns its `fbl::Vector` heap allocations of plain integer data
// (`usize` and `cpu_num_t`) with no interior mutability, so shared references `&ClusterSet` only
// permit concurrent read-only access.
unsafe impl Sync for ClusterSet {}

/// Allocates a [`fbl::Vector`] populated with `len` default elements, panicking on boot OOM.
fn new_default_vector<T: Default>(len: usize) -> fbl::Vector<T> {
    let mut vec = fbl::Vector::new();
    assert!(
        vec.resize_with_default(len).is_ok(),
        "failed to allocate CPU cluster vector (len = {len})"
    );
    vec
}

/// Wrapper around CPU performance scales written during single-threaded boot initialization and
/// read-only thereafter.
#[repr(transparent)]
struct PerfScales(
    // Safety invariant: Only mutated via `CpuSearchSet::set_perf_scale` under exclusive access
    // before any reads in `CpuSearchSet::initialize`.
    UnsafeCell<[i64; SMP_MAX_CPUS]>,
);

// SAFETY: `PerfScales` is only mutated via `unsafe fn CpuSearchSet::set_perf_scale`, whose safety
// contract requires exclusive access happening-before any reads.
unsafe impl Sync for PerfScales {}

/// The relative performance scales of each CPU. Each relevant scale must be set before
/// initializing the search sets.
static PERF_SCALES: PerfScales = PerfScales(UnsafeCell::new([0; SMP_MAX_CPUS]));

/// The global set of CPU clusters initialized during auto clustering.
static CLUSTER_SET: LazyInit<ClusterSet> = LazyInit::uninit();

/// Utility type to compute CPU clusters using a disjoint set structure.
struct ClusterMap {
    elements: fbl::Vector<cpu_num_t>,
}

impl ClusterMap {
    /// Creates a `ClusterMap` with the given number of CPUs, with each CPU initially
    /// in its own cluster.
    fn create(element_count: usize) -> Self {
        let mut elements = new_default_vector(element_count);

        for i in 0..(element_count as cpu_num_t) {
            elements[i as usize] = i;
        }

        Self { elements }
    }

    fn find_set(&mut self, mut node: cpu_num_t) -> cpu_num_t {
        loop {
            let parent = self.elements[node as usize];
            let grandparent = self.elements[parent as usize];
            if parent == grandparent {
                return parent;
            }
            self.elements[node as usize] = grandparent;
            node = parent;
        }
    }

    fn union_sets(&mut self, a: cpu_num_t, b: cpu_num_t) {
        loop {
            let root_a = self.find_set(a);
            let root_b = self.find_set(b);

            if root_a < root_b {
                self.elements[root_b as usize] = root_a;
            } else if root_a > root_b {
                self.elements[root_a as usize] = root_b;
            } else {
                return;
            }
        }
    }

    fn cluster_count(&self) -> usize {
        let mut count = 0;
        for i in 0..self.elements.len() {
            if self.elements[i] as usize == i {
                count += 1;
            }
        }
        count
    }

    fn member_count(&mut self, root: cpu_num_t) -> usize {
        let mut count = 0;
        for i in 0..(self.elements.len() as cpu_num_t) {
            if self.find_set(i) == root {
                count += 1;
            }
        }
        count
    }
}

impl Index<usize> for ClusterMap {
    type Output = cpu_num_t;

    fn index(&self, index: usize) -> &Self::Output {
        &self.elements[index]
    }
}

/// Inner state of [`CpuSearchSet`].
///
/// Invariant: `cpu_count <= SMP_MAX_CPUS`.
#[repr(C)]
struct CpuSearchSetInner {
    /// Each search set is initially populated by `BOOT_CPU_ID` so that the boot
    /// processor has a valid search set during early kernel init.
    cpu_count: usize,
    ordered_cpus: [Entry; SMP_MAX_CPUS],

    /// The CPU this search set is for.
    this_cpu: cpu_num_t,
}

impl CpuSearchSetInner {
    /// Returns the mutable, populated prefix of the search list.
    fn iterator_mut(&mut self) -> &mut [Entry] {
        &mut self.ordered_cpus[..self.cpu_count]
    }

    /// Initializes the search set with a unique CPU order that minimizes cache level
    /// crossings while attempting to maximize distribution across CPUs.
    ///
    /// Note that unstable sorting (`sort_unstable_by`) produces the exact same order as a stable
    /// sort here because every distinct CPU has a unique `(a_cluster_prime, a_index_prime)` pair
    /// and never compares `Equal`.
    fn do_initialize(
        &mut self,
        this_cpu: cpu_num_t,
        cpu_count: usize,
        cluster_set: &ClusterSet,
        map: &CpuDistanceMap,
        prefer_little_cpus: bool,
        perf_scales: &[i64; SMP_MAX_CPUS],
    ) {
        // Enforce `(this_cpu as usize) < cpu_count` and `CpuSearchSetInner`'s
        // `cpu_count <= SMP_MAX_CPUS` invariant (relied upon by `const_iterator` and
        // `iterator_mut`) before mutating `self`.
        assert!((this_cpu as usize) < cpu_count && cpu_count <= SMP_MAX_CPUS);

        self.this_cpu = this_cpu;

        // Initialize the search set in increasing ordinal order.
        self.cpu_count = cpu_count;
        for i in 0..(cpu_count as cpu_num_t) {
            let cluster_idx = cluster_set.cpu_to_cluster_map[i as usize].cluster;
            let cluster = cluster_set.clusters[cluster_idx].id;
            self.ordered_cpus[i as usize] = Entry { cpu: i, cluster };
        }

        // The search sets are sorted based on policy.
        //
        // If the policy is to prefer little CPUs (see `scheduler_prefer_little_cpus`) CPUs with
        // smaller performance scales are sorted before those with larger scales.
        //
        // When `scheduler_prefer_little_cpus` is false, the search set is sorted by these criteria:
        //   1. Cache distance from this CPU.
        //   2. Modular cluster order, offset by this CPU's cluster id.
        //   3. Modular cluster member order, offset by this CPU's cluster member index.
        //
        // The above criteria result in a relaxed Latin Square with the following properties:
        //   * A CPU is always at the front of its own search list (distance is 0).
        //   * The search list is ordered by increasing cache distance.
        //   * The search order is reasonably unique compared to other CPUs (a CPU is
        //     found as few times as possible at a given offset in all search lists).
        //
        let comparator = |a: &Entry, b: &Entry| -> CmpOrdering {
            // If the system is configured to prefer little CPUs to improve idle power consumption,
            // place them at the front of the search sets.
            if prefer_little_cpus && perf_scales[a.cpu as usize] != perf_scales[b.cpu as usize] {
                return perf_scales[a.cpu as usize].cmp(&perf_scales[b.cpu as usize]);
            }

            let distance_a = map.distance((this_cpu, a.cpu));
            let distance_b = map.distance((this_cpu, b.cpu));
            if distance_a != distance_b {
                return distance_a.cmp(&distance_b);
            }

            let MapEntry { cluster: this_cluster_idx, index: this_index } =
                cluster_set.cpu_to_cluster_map[this_cpu as usize];
            let MapEntry { cluster: a_cluster_idx, index: a_index } =
                cluster_set.cpu_to_cluster_map[a.cpu as usize];
            let MapEntry { cluster: b_cluster_idx, index: b_index } =
                cluster_set.cpu_to_cluster_map[b.cpu as usize];
            let this_cluster = &cluster_set.clusters[this_cluster_idx];
            let a_cluster = &cluster_set.clusters[a_cluster_idx];
            let b_cluster = &cluster_set.clusters[b_cluster_idx];

            let cluster_count = cluster_set.clusters.len();
            let a_cluster_prime = (this_cluster.id + cluster_count - a_cluster.id) % cluster_count;
            let b_cluster_prime = (this_cluster.id + cluster_count - b_cluster.id) % cluster_count;
            if a_cluster_prime != b_cluster_prime {
                return a_cluster_prime.cmp(&b_cluster_prime);
            }

            let a_count = a_cluster.members.len();
            let b_count = b_cluster.members.len();
            let a_index_prime = a_cluster.members[(this_index + a_count - a_index) % a_count];
            let b_index_prime = b_cluster.members[(this_index + b_count - b_index) % b_count];
            a_index_prime.cmp(&b_index_prime)
        };

        self.iterator_mut().sort_unstable_by(comparator);
    }

    fn dump(&self) {
        dprintf!(INFO, "CPU {:2}: ", self.this_cpu);
        for i in 0..self.cpu_count {
            let sep = if i < self.cpu_count - 1 { ", " } else { "" };
            dprintf!(INFO, "{:2}{}", self.ordered_cpus[i].cpu, sep);
        }
        dprintf!(INFO, "\n");
    }
}

/// `CpuSearchSet` is a cache/cluster-aware search list that minimizes cache
/// crossings and maximizes remote CPU access distribution when searching for a
/// target CPU to place a task.
#[repr(transparent)]
pub struct CpuSearchSet {
    /// Wrapped in [`UnsafeCell`] so secondary CPU init can populate each CPU's search set through
    /// shared `&PerCpu` references without ever forming `&mut PerCpu`.
    // Safety invariant: Always holds an initialized `CpuSearchSetInner` with
    // `cpu_count <= SMP_MAX_CPUS`, and is only mutated through `&self` inside `unsafe fn
    // initialize` under exclusive access with no live borrows.
    inner: UnsafeCell<CpuSearchSetInner>,
}

// SAFETY: `CpuSearchSet` is only mutated through `&self` in `unsafe fn initialize`, whose safety
// contract requires exclusive access ordered by happens-before relative to all prior and subsequent
// accesses; all other `&self` methods only perform shared reads of `self.inner` (and test-only
// mutation takes `&mut Self`).
unsafe impl Sync for CpuSearchSet {}

zr::static_assert!(size_of::<Entry>() == 2 * size_of::<usize>());
zr::static_assert!(align_of::<Entry>() == align_of::<usize>());
zr::static_assert!(size_of::<CpuSearchSet>() == size_of::<bindings::CpuSearchSet>());
zr::static_assert!(align_of::<CpuSearchSet>() == align_of::<bindings::CpuSearchSet>());
zr::static_assert!(offset_of!(CpuSearchSetInner, cpu_count) == 0);
zr::static_assert!(offset_of!(CpuSearchSetInner, ordered_cpus) == size_of::<usize>());
zr::static_assert!(
    offset_of!(CpuSearchSetInner, this_cpu)
        == size_of::<usize>() + SMP_MAX_CPUS * size_of::<Entry>()
);

impl CpuSearchSet {
    /// Constructs a default `CpuSearchSet` in place without allocating on the stack.
    #[inline]
    pub fn init() -> impl PinInit<Self, Infallible> {
        // SAFETY: `pin_init_from_closure` guarantees `slot` is valid for writes of `Self`. Zeroing
        // `*slot` sets `ordered_cpus` to `{BOOT_CPU_ID (0), 0}` and `this_cpu` to `BOOT_CPU_ID (0)`
        // (verified by `zr::static_assert!(BOOT_CPU_ID == 0)`), and writing `1` to `cpu_count` via
        // `UnsafeCell::raw_get` completes initialization of `*slot` (`1 <= SMP_MAX_CPUS`) before
        // returning `Ok(())` without materializing an intermediate reference.
        unsafe {
            pin_init::pin_init_from_closure(|slot: *mut Self| {
                slot.write_bytes(0, 1);
                let inner = UnsafeCell::raw_get(&raw const (*slot).inner);
                (&raw mut (*inner).cpu_count).write(1);
                Ok(())
            })
        }
    }

    #[inline]
    fn inner_ref(&self) -> &CpuSearchSetInner {
        // SAFETY: Mutation through `&self` only occurs inside `unsafe fn initialize`, whose caller
        // guarantees no concurrent reads or live borrows of `self` exist during that call.
        unsafe { &*self.inner.get() }
    }

    /// Returns an iterator over the CPU search list. Forward iteration produces
    /// entries in order of decreasing preference (i.e. earlier entries are more
    /// optimal).
    #[inline]
    pub fn const_iterator(&self) -> &[Entry] {
        let inner = self.inner_ref();
        &inner.ordered_cpus[..inner.cpu_count]
    }

    /// Dumps the CPU search list for this set to the debug log.
    pub fn dump(&self) {
        self.inner_ref().dump();
    }

    /// Dumps the CPU clusters to the debug log.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::auto_cluster`] has not completed.
    pub fn dump_clusters() {
        dprintf!(INFO, "CPU clusters:\n");
        for cluster in &*CLUSTER_SET.clusters {
            dprintf!(INFO, "Cluster {:2}: ", cluster.id);
            for j in 0..cluster.members.len() {
                let sep = if j < cluster.members.len() - 1 { ", " } else { "" };
                dprintf!(INFO, "{}{}", cluster.members[j], sep);
            }
            dprintf!(INFO, "\n");
        }
    }

    /// Returns the number of CPUs in this search set.
    #[inline]
    pub fn cpu_count(&self) -> usize {
        self.inner_ref().cpu_count
    }

    /// Returns the logical cluster index for this CPU's search set.
    ///
    /// # Panics
    ///
    /// Panics if [`Self::auto_cluster`] has not completed, or if `this_cpu` is out of bounds of
    /// the auto-clustered CPU map.
    #[inline]
    pub fn cluster(&self) -> usize {
        let cluster_set = &*CLUSTER_SET;
        let this_cpu = self.inner_ref().this_cpu as usize;
        cluster_set.clusters[cluster_set.cpu_to_cluster_map[this_cpu].cluster].id
    }

    /// Sets the relative performance scale for the given CPU.
    ///
    /// # Panics
    ///
    /// Panics if `(cpu as usize) >= SMP_MAX_CPUS`.
    ///
    /// # Safety
    ///
    /// The caller must ensure exclusive access during single-threaded boot initialization with no
    /// concurrent calls to [`Self::set_perf_scale`] or [`Self::initialize`], and that all calls to
    /// [`Self::set_perf_scale`] happen-before any calls to [`Self::initialize`].
    #[inline]
    pub unsafe fn set_perf_scale(cpu: cpu_num_t, perf_scale: i64) {
        let cpu = cpu as usize;
        // SAFETY: `PERF_SCALES` holds a live, aligned `[i64; SMP_MAX_CPUS]`, built-in array
        // indexing bounds-checks `cpu < SMP_MAX_CPUS` before projecting the element place without
        // forming `&mut [i64; SMP_MAX_CPUS]`, and the caller guarantees exclusive single-threaded
        // boot access happening-before all reads.
        unsafe {
            (*PERF_SCALES.0.get())[cpu] = perf_scale;
        }
    }

    /// Called once at percpu secondary init to compute the logical clusters from
    /// the topology-derived distance map.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `CpuDistanceMap::Initialize` (and any
    /// `CpuDistanceMap::set_distance_threshold` calls) succeeded and happened-before this call with
    /// no concurrent mutation of the global distance map, and that [`Self::auto_cluster`] is called
    /// at most once and happens-before any calls to [`Self::initialize`], [`Self::cluster`], or
    /// [`Self::dump_clusters`].
    pub unsafe fn auto_cluster(cpu_count: usize) {
        // SAFETY: The caller guarantees `CpuDistanceMap::Initialize` succeeded and happened-before
        // this call with no concurrent mutation of the global distance map.
        let map = unsafe { CpuDistanceMap::get() };
        let cluster_set = Self::do_auto_cluster(cpu_count, map);
        // SAFETY: The caller guarantees `auto_cluster` is called at most once and happens-before
        // any calls to `initialize`, `cluster`, or `dump_clusters`.
        unsafe {
            CLUSTER_SET.init(cluster_set);
        }
    }

    /// Testable body of [`Self::auto_cluster`].
    fn do_auto_cluster(cpu_count: usize, map: &CpuDistanceMap) -> ClusterSet {
        let mut cluster_map = ClusterMap::create(cpu_count);

        // Perform a single pass of agglomerative clustering, joining CPUs with
        // distances below the significant distance threshold into the same cluster.
        let distance_threshold = map.distance_threshold();
        for i in 0..(cpu_count as cpu_num_t) {
            for j in (i + 1)..(cpu_count as cpu_num_t) {
                if map.distance((i, j)) < distance_threshold {
                    cluster_map.union_sets(i, j);
                }
            }
        }

        // Allocate vector of Cluster structures with an entry for each computed
        // cluster.
        let mut clusters = new_default_vector::<Cluster>(cluster_map.cluster_count());

        // Allocate a vector of MapEntry structures with an entry for each CPU.
        let mut cpu_to_cluster_map = new_default_vector::<MapEntry>(cpu_count);

        // Fill in the Cluster structures and CPU-to-cluster map.
        let mut cluster_index = 0;
        for i in 0..(cpu_count as cpu_num_t) {
            let member_count = cluster_map.member_count(i);
            if member_count != 0 {
                let cluster = &mut clusters[cluster_index];
                cluster.id = cluster_index;
                cluster.members = new_default_vector(member_count);

                let mut member_index = 0;
                for j in 0..(cpu_count as cpu_num_t) {
                    if cluster_map[j as usize] == i {
                        cluster.members[member_index] = j;
                        cpu_to_cluster_map[j as usize] =
                            MapEntry { cluster: cluster_index, index: member_index };
                        member_index += 1;
                    }
                }
                cluster_index += 1;
            }
        }

        ClusterSet { clusters, cpu_to_cluster_map }
    }

    /// Called once per CPU at percpu secondary init to compute the unique, cache-
    /// aware CPU search order for the CPUs.
    ///
    /// # Panics
    ///
    /// Panics if `(this_cpu as usize) >= cpu_count` or `cpu_count > SMP_MAX_CPUS`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `CpuDistanceMap::Initialize` (and any `CpuDistanceMap::set_distance_threshold` calls),
    ///   [`Self::set_perf_scale`] calls, and [`Self::auto_cluster`] have completed and
    ///   happened-before this call with no concurrent mutation of the global distance map or
    ///   performance scales.
    /// - All prior accesses to `self` happened-before this call, no concurrent or re-entrant (e.g.,
    ///   interrupt-handler) reads or writes to `self` and no live slice returned by
    ///   [`Self::const_iterator`] on `self` exist during this call, and this call happens-before
    ///   all subsequent accesses to `self`.
    pub unsafe fn initialize(&self, this_cpu: cpu_num_t, cpu_count: usize) {
        // SAFETY: The caller guarantees `CpuDistanceMap::Initialize` succeeded and happened-before
        // this call with no concurrent mutation of the global distance map.
        let map = unsafe { CpuDistanceMap::get() };
        // SAFETY: `PERF_SCALES` is only mutated via `unsafe fn set_perf_scale`, and the caller
        // guarantees all `set_perf_scale` calls completed and happened-before this call with no
        // concurrent writes.
        let perf_scales = unsafe { &*PERF_SCALES.0.get() };
        let prefer_little_cpus = BootOptions::get().scheduler_prefer_little_cpus;
        // SAFETY: `self.inner.get()` derives a mutable pointer via `UnsafeCell`, and the caller
        // guarantees exclusive access to `self` with no concurrent or re-entrant accesses or live
        // borrows, and that `auto_cluster` has completed and happened-before this call.
        let inner = unsafe { &mut *self.inner.get() };
        inner.do_initialize(
            this_cpu,
            cpu_count,
            &CLUSTER_SET,
            map,
            prefer_little_cpus,
            perf_scales,
        );
    }

    /// Testable body of [`Self::initialize`].
    #[cfg(ktest)]
    fn do_initialize(
        &mut self,
        this_cpu: cpu_num_t,
        cpu_count: usize,
        cluster_set: &ClusterSet,
        map: &CpuDistanceMap,
        prefer_little_cpus: bool,
        perf_scales: &[i64; SMP_MAX_CPUS],
    ) {
        self.inner.get_mut().do_initialize(
            this_cpu,
            cpu_count,
            cluster_set,
            map,
            prefer_little_cpus,
            perf_scales,
        );
    }
}

/// CPU search set unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "cpu_search_set_tests")]
mod tests {
    use core::pin::Pin;
    use unittest::{
        assert_eq, assert_false, assert_gt, expect_eq, expect_true, subtest, unwrap_some,
    };

    use super::{CpuSearchSet, SMP_MAX_CPUS, cpu_num_t};
    use crate::kernel::cpu_distance_map::{CpuDistanceMap, TestCpuDistanceMap};
    use crate::kernel::types::{cpu_mask_t, cpu_num_to_mask};

    const DEFAULT_PERF_SCALES: [i64; SMP_MAX_CPUS] = [0; SMP_MAX_CPUS];

    /// Returns true if first `count` bits in the CPU mask are set.
    fn cpu_set_check(mask: cpu_mask_t, count: usize) -> bool {
        let mut set = 0;
        for i in 0..(count as cpu_num_t) {
            if (mask & cpu_num_to_mask(i)) != 0 {
                set += 1;
            }
        }
        set == count
    }

    /// Verifies that `search_set` contains `cpu_count` entries covering every CPU in
    /// `0..cpu_count`.
    #[must_use]
    fn check_all_cpus_present(search_set: &CpuSearchSet, cpu_count: usize) -> bool {
        (subtest!(|search_set: &CpuSearchSet, cpu_count: usize| {
            expect_eq!(cpu_count, search_set.cpu_count());
            let mut cpu_set: cpu_mask_t = 0;
            for entry in search_set.const_iterator() {
                assert_gt!(cpu_count, entry.cpu as usize);
                cpu_set |= cpu_num_to_mask(entry.cpu);
            }
            expect_true!(cpu_set_check(cpu_set, cpu_count));
        }))(search_set, cpu_count)
    }

    /// Verifies that `search_set` visits CPUs in the exact `expected` order.
    #[must_use]
    fn check_search_order(search_set: &CpuSearchSet, expected: &[cpu_num_t]) -> bool {
        (subtest!(|search_set: &CpuSearchSet, expected: &[cpu_num_t]| {
            let iter = search_set.const_iterator();
            assert_eq!(expected.len(), iter.len());
            for (entry, &expected_cpu) in iter.iter().zip(expected) {
                expect_eq!(expected_cpu, entry.cpu);
            }
        }))(search_set, expected)
    }

    /// Allocates a search set on the heap to avoid overflowing the kernel stack.
    fn new_search_set() -> Option<kalloc::Box<CpuSearchSet>> {
        kalloc::Box::try_pin_init(CpuSearchSet::init()).ok().map(Pin::into_inner)
    }

    /// default_search_set_test
    #[test]
    fn default_search_set_test() {
        // A default constructed search set must have one CPU that must be CPU 0.
        let search_set = unwrap_some!(new_search_set());
        expect_eq!(1, search_set.cpu_count());
        let iter = search_set.const_iterator();
        assert_false!(iter.is_empty());
        expect_eq!(0, iter[0].cpu);
    }

    /// cpu_search_set_test_1
    #[test]
    fn cpu_search_set_test_1() {
        let mut search_set = unwrap_some!(new_search_set());

        let cpu_count: usize = 1;
        let mut map = unwrap_some!(TestCpuDistanceMap::create(cpu_count, |_, _| 1));

        let distance_threshold: u32 = 2;
        map.set_distance_threshold(distance_threshold);
        let map: &CpuDistanceMap = &map;

        let cpu0: cpu_num_t = 0;

        // A cluster set for one CPU should contain one cluster with CPU 0 in it.
        let cluster_set = CpuSearchSet::do_auto_cluster(cpu_count, map);
        assert_eq!(1, cluster_set.clusters.len());
        expect_eq!(0, cluster_set.clusters[0].id);
        assert_eq!(cpu_count, cluster_set.clusters[0].members.len());
        expect_eq!(cpu0, cluster_set.clusters[0].members[0]);

        search_set.do_initialize(cpu0, cpu_count, &cluster_set, map, false, &DEFAULT_PERF_SCALES);
        expect_true!(check_all_cpus_present(&search_set, cpu_count));
        expect_true!(check_search_order(&search_set, &[0]));
    }

    /// cpu_search_set_test_2
    #[test]
    fn cpu_search_set_test_2() {
        let mut search_set = unwrap_some!(new_search_set());

        let cpu_count: usize = 2;
        let mut map = unwrap_some!(TestCpuDistanceMap::create(cpu_count, |_, _| 1));

        let distance_threshold: u32 = 2;
        map.set_distance_threshold(distance_threshold);
        let map: &CpuDistanceMap = &map;

        let cpu0: cpu_num_t = 0;
        let cpu1: cpu_num_t = 1;

        // A cluster set for two unit distance CPUs should contain one cluster with
        // CPU 0 and 1 in it.
        let cluster_set = CpuSearchSet::do_auto_cluster(cpu_count, map);
        assert_eq!(1, cluster_set.clusters.len());
        expect_eq!(0, cluster_set.clusters[0].id);
        assert_eq!(cpu_count, cluster_set.clusters[0].members.len());
        expect_eq!(cpu0, cluster_set.clusters[0].members[0]);
        expect_eq!(cpu1, cluster_set.clusters[0].members[1]);

        {
            // The search set for CPU 0 should have two entries ordered [0, 1].
            search_set.do_initialize(
                cpu0,
                cpu_count,
                &cluster_set,
                map,
                false,
                &DEFAULT_PERF_SCALES,
            );
            expect_true!(check_all_cpus_present(&search_set, cpu_count));
            expect_true!(check_search_order(&search_set, &[0, 1]));
        }

        {
            // The search set for CPU 1 should have two entries ordered [1, 0].
            search_set.do_initialize(
                cpu1,
                cpu_count,
                &cluster_set,
                map,
                false,
                &DEFAULT_PERF_SCALES,
            );
            expect_true!(check_all_cpus_present(&search_set, cpu_count));
            expect_true!(check_search_order(&search_set, &[1, 0]));
        }
    }

    /// cpu_search_set_test_4
    #[test]
    fn cpu_search_set_test_4() {
        let mut search_set = unwrap_some!(new_search_set());

        let cpu_count: usize = 4;
        let mut map = unwrap_some!(TestCpuDistanceMap::create(cpu_count, |i, j| {
            if (i == 0 && j == 1) || (i == 2 && j == 3) { 1 } else { 2 }
        }));

        let distance_threshold: u32 = 2;
        map.set_distance_threshold(distance_threshold);
        let map: &CpuDistanceMap = &map;

        let cpu0: cpu_num_t = 0;
        let cpu1: cpu_num_t = 1;
        let cpu2: cpu_num_t = 2;
        let cpu3: cpu_num_t = 3;

        // A cluster set for two sets of two equidistant CPUs should contain two
        // clusters with pairs CPU 0/1 and 2/3.
        let cluster_set = CpuSearchSet::do_auto_cluster(cpu_count, map);
        assert_eq!(2, cluster_set.clusters.len());
        expect_eq!(0, cluster_set.clusters[0].id);
        expect_eq!(1, cluster_set.clusters[1].id);
        assert_eq!(2, cluster_set.clusters[0].members.len());
        assert_eq!(2, cluster_set.clusters[1].members.len());
        expect_eq!(cpu0, cluster_set.clusters[0].members[0]);
        expect_eq!(cpu3, cluster_set.clusters[1].members[1]);

        for (cpu, expected) in
            [(cpu0, [0, 1, 2, 3]), (cpu1, [1, 0, 3, 2]), (cpu2, [2, 3, 0, 1]), (cpu3, [3, 2, 1, 0])]
        {
            search_set.do_initialize(
                cpu,
                cpu_count,
                &cluster_set,
                map,
                false,
                &DEFAULT_PERF_SCALES,
            );
            expect_true!(check_all_cpus_present(&search_set, cpu_count));
            expect_true!(check_search_order(&search_set, &expected));
        }

        // Verify `prefer_little_cpus` places lower-scale CPUs (cluster 1: CPUs 2/3) ahead of
        // higher-scale CPUs (cluster 0: CPUs 0/1) while breaking equal-scale ties by distance and
        // modular cluster/member order.
        let mut perf_scales = DEFAULT_PERF_SCALES;
        perf_scales[0] = 200;
        perf_scales[1] = 200;
        perf_scales[2] = 100;
        perf_scales[3] = 100;
        search_set.do_initialize(cpu0, cpu_count, &cluster_set, map, true, &perf_scales);
        expect_true!(check_all_cpus_present(&search_set, cpu_count));
        expect_true!(check_search_order(&search_set, &[2, 3, 0, 1]));
    }

    /// cpu_search_set_test_max
    #[test]
    fn cpu_search_set_test_max() {
        let mut search_set = unwrap_some!(new_search_set());

        let cpu_count: usize = SMP_MAX_CPUS;
        let mut map = unwrap_some!(TestCpuDistanceMap::create(cpu_count, |_, _| 1));

        let distance_threshold: u32 = 2;
        map.set_distance_threshold(distance_threshold);
        let map: &CpuDistanceMap = &map;

        let cpu0: cpu_num_t = 0;
        let cpu_max: cpu_num_t = (cpu_count - 1) as cpu_num_t;

        // A cluster set for `SMP_MAX_CPUS` equidistant CPUs should contain one cluster with
        // all CPUs in it.
        let cluster_set = CpuSearchSet::do_auto_cluster(cpu_count, map);
        assert_eq!(1, cluster_set.clusters.len());
        expect_eq!(0, cluster_set.clusters[0].id);
        assert_eq!(cpu_count, cluster_set.clusters[0].members.len());
        expect_eq!(cpu0, cluster_set.clusters[0].members[0]);
        expect_eq!(cpu_max, cluster_set.clusters[0].members[cpu_max as usize]);

        {
            // The search set for CPU 0 should have max entries starting with CPU 0.
            search_set.do_initialize(
                cpu0,
                cpu_count,
                &cluster_set,
                map,
                false,
                &DEFAULT_PERF_SCALES,
            );
            expect_true!(check_all_cpus_present(&search_set, cpu_count));
            expect_eq!(cpu0, search_set.const_iterator()[0].cpu);
        }

        {
            // The search set for the last CPU should have max entries starting with `cpu_max`.
            search_set.do_initialize(
                cpu_max,
                cpu_count,
                &cluster_set,
                map,
                false,
                &DEFAULT_PERF_SCALES,
            );
            expect_true!(check_all_cpus_present(&search_set, cpu_count));
            expect_eq!(cpu_max, search_set.const_iterator()[0].cpu);
        }
    }
}
