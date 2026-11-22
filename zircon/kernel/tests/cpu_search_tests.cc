// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/unittest/unittest.h>
#include <stddef.h>

#include <kernel/cpu.h>
#include <kernel/cpu_distance_map.h>
#include <ktl/algorithm.h>
#include <ktl/forward.h>
#include <ktl/limits.h>
#include <ktl/optional.h>

#include <ktl/enforce.h>

// Define test type with friend access to CpuDistanceMap.
struct CpuDistanceMapTestAccess {
  template <typename Callable>
  static ktl::optional<CpuDistanceMap> Create(size_t cpu_count, Callable&& callable) {
    return CpuDistanceMap::Create(cpu_count, ktl::forward<Callable>(callable));
  }
  static void LaunderEntries(ktl::optional<CpuDistanceMap>& map) {
    if (map) {
      if (auto* entries = map->entries_.release()) {
        // Launder the pointer returned by malloc (via operator new[]) so Clang
        // cannot prove the allocation never escapes, optimize away the
        // malloc/free pair, and unconditionally assume the malloc succeeded.
        // This is only needed for test code that intentionally tests allocation
        // failure without reading back the entries, not production code.
        __asm__("" : "+r"(entries));
        map->entries_.reset(entries);
      }
    }
  }
  static size_t EntryCountFromCpuCount(size_t cpu_count) {
    return CpuDistanceMap::EntryCountFromCpuCount(cpu_count);
  }
  static size_t LinearIndex(CpuDistanceMap::Index index, size_t cpu_count) {
    return CpuDistanceMap::LinearIndex(index, cpu_count);
  }
};

static bool distance_map_linear_index_test() {
  BEGIN_TEST;

  // Test that the linear index function does not underflow over a large set of
  // values. The function is mathematically proven to never underflow, but we
  // test anyway in case someone changes the function.
  const size_t cpu_count = 8192;
  const size_t entry_count = CpuDistanceMapTestAccess::EntryCountFromCpuCount(cpu_count);
  for (cpu_num_t i = 0; i < cpu_count; i++) {
    for (cpu_num_t j = i + 1; j < cpu_count; j++) {
      if (i != j) {
        EXPECT_GT(entry_count, CpuDistanceMapTestAccess::LinearIndex({i, j}, cpu_count));
      }
    }
  }

  END_TEST;
}

static bool allocate_distance_map_tests() {
  BEGIN_TEST;

  {
    const size_t cpu_count = 0u;
    size_t invocations = 0;
    auto map = CpuDistanceMapTestAccess::Create(cpu_count, [&invocations](cpu_num_t, cpu_num_t) {
      invocations++;
      return 1u;
    });
    EXPECT_FALSE(map.has_value());
    EXPECT_EQ(0u, invocations);
  }

  {
    const size_t cpu_count = 1u;
    const size_t entry_count = CpuDistanceMapTestAccess::EntryCountFromCpuCount(cpu_count);
    size_t invocations = 0;
    auto map = CpuDistanceMapTestAccess::Create(cpu_count, [&invocations](cpu_num_t, cpu_num_t) {
      invocations++;
      return 1u;
    });
    ASSERT_TRUE(map.has_value());
    EXPECT_EQ(cpu_count, map->cpu_count());
    EXPECT_EQ(entry_count, map->entry_count());
    EXPECT_EQ(entry_count, invocations);
  }

  {
    const size_t cpu_count = 2u;
    const size_t entry_count = CpuDistanceMapTestAccess::EntryCountFromCpuCount(cpu_count);
    size_t invocations = 0;
    auto map = CpuDistanceMapTestAccess::Create(cpu_count, [&invocations](cpu_num_t, cpu_num_t) {
      invocations++;
      return 1u;
    });
    ASSERT_TRUE(map.has_value());
    EXPECT_EQ(cpu_count, map->cpu_count());
    EXPECT_EQ(entry_count, map->entry_count());
    EXPECT_EQ(entry_count, invocations);
  }

  {
    const size_t cpu_count = 32u;
    const size_t entry_count = CpuDistanceMapTestAccess::EntryCountFromCpuCount(cpu_count);
    size_t invocations = 0;
    auto map = CpuDistanceMapTestAccess::Create(cpu_count, [&invocations](cpu_num_t, cpu_num_t) {
      invocations++;
      return 1u;
    });
    ASSERT_TRUE(map.has_value());
    EXPECT_EQ(cpu_count, map->cpu_count());
    EXPECT_EQ(entry_count, map->entry_count());
    EXPECT_EQ(entry_count, invocations);
  }

  {
    // Request a large number of CPUs without triggering the integer overflow ASSERT.
    const size_t cpu_count = ktl::numeric_limits<cpu_num_t>::max();
    size_t invocations = 0;
    auto map = CpuDistanceMapTestAccess::Create(cpu_count, [&invocations](cpu_num_t, cpu_num_t) {
      invocations++;
      return 1u;
    });
    CpuDistanceMapTestAccess::LaunderEntries(map);
    EXPECT_FALSE(map.has_value());
    EXPECT_EQ(0u, invocations);
  }

  END_TEST;
}

static bool distance_map_entry_tests() {
  BEGIN_TEST;

  {
    const size_t cpu_count = 1u;
    auto maybe_map =
        CpuDistanceMapTestAccess::Create(cpu_count, [](cpu_num_t, cpu_num_t) { return 1u; });
    ASSERT_TRUE(maybe_map.has_value());
    CpuDistanceMap& map = maybe_map.value();

    for (cpu_num_t i = 0; i < cpu_count; i++) {
      for (cpu_num_t j = 0; j < cpu_count; j++) {
        if (i == j) {
          EXPECT_EQ(0u, (map[{i, j}]));
        } else {
          EXPECT_EQ(1u, (map[{i, j}]));
        }
      }
    }
  }

  {
    const size_t cpu_count = 2u;
    auto maybe_map =
        CpuDistanceMapTestAccess::Create(cpu_count, [](cpu_num_t, cpu_num_t) { return 1u; });
    ASSERT_TRUE(maybe_map.has_value());
    CpuDistanceMap& map = maybe_map.value();

    for (cpu_num_t i = 0; i < cpu_count; i++) {
      for (cpu_num_t j = 0; j < cpu_count; j++) {
        if (i == j) {
          EXPECT_EQ(0u, (map[{i, j}]));
        } else {
          EXPECT_EQ(1u, (map[{i, j}]));
        }
      }
    }
  }

  {
    const size_t cpu_count = 32u;
    auto maybe_map =
        CpuDistanceMapTestAccess::Create(cpu_count, [](cpu_num_t, cpu_num_t) { return 1u; });
    ASSERT_TRUE(maybe_map.has_value());
    CpuDistanceMap& map = maybe_map.value();

    for (cpu_num_t i = 0; i < cpu_count; i++) {
      for (cpu_num_t j = 0; j < cpu_count; j++) {
        if (i == j) {
          EXPECT_EQ(0u, (map[{i, j}]));
        } else {
          EXPECT_EQ(1u, (map[{i, j}]));
        }
      }
    }
  }

  {
    const size_t cpu_count = 1u;
    auto maybe_map = CpuDistanceMapTestAccess::Create(
        cpu_count, [](cpu_num_t i, cpu_num_t j) { return ktl::max(i, j); });
    ASSERT_TRUE(maybe_map.has_value());
    CpuDistanceMap& map = maybe_map.value();

    for (cpu_num_t i = 0; i < cpu_count; i++) {
      for (cpu_num_t j = 0; j < cpu_count; j++) {
        if (i == j) {
          EXPECT_EQ(0u, (map[{i, j}]));
        } else {
          EXPECT_EQ(ktl::max(i, j), (map[{i, j}]));
        }
      }
    }
  }

  {
    const size_t cpu_count = 32u;
    auto maybe_map = CpuDistanceMapTestAccess::Create(
        cpu_count, [](cpu_num_t i, cpu_num_t j) { return ktl::max(i, j); });
    ASSERT_TRUE(maybe_map.has_value());
    CpuDistanceMap& map = maybe_map.value();

    for (cpu_num_t i = 0; i < cpu_count; i++) {
      for (cpu_num_t j = 0; j < cpu_count; j++) {
        if (i == j) {
          EXPECT_EQ(0u, (map[{i, j}]));
        } else {
          EXPECT_EQ(ktl::max(i, j), (map[{i, j}]));
        }
      }
    }
  }

  END_TEST;
}

UNITTEST_START_TESTCASE(cpu_distance_map_tests)
UNITTEST("distance_map_linear_index", distance_map_linear_index_test)
UNITTEST("allocate_distance_map", allocate_distance_map_tests)
UNITTEST("distance_map_entries", distance_map_entry_tests)
UNITTEST_END_TESTCASE(cpu_distance_map_tests, "cpu_distance_map_tests", "cpu_distance_map_tests")
