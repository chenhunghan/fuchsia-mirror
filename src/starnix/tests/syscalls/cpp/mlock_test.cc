// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <optional>
#include <string_view>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/files/file.h"
#include "src/lib/fxl/strings/split_string.h"
#include "src/starnix/tests/syscalls/cpp/proc_test_base.h"
#include "src/starnix/tests/syscalls/cpp/syscall_matchers.h"
#include "src/starnix/tests/syscalls/cpp/test_helper.h"

#ifndef MLOCK_ONFAULT
#define MLOCK_ONFAULT 0x01
#endif

// Whether the container is configured to never populate memory on mlock().
#ifdef MLOCK_ALWAYS_ONFAULT
constexpr bool kMlockAlwaysOnFault = true;
#else
constexpr bool kMlockAlwaysOnFault = false;
#endif

class MlockProcTest : public ProcTestBase {
 protected:
  // Returns the /proc/self/smaps entry of the mapping that contains `addr`.
  std::optional<test_helper::MemoryMappingExt> GetSmapsEntry(const volatile void* addr) {
    std::string smaps;
    if (!files::ReadFileToString(proc_path() + "/self/smaps", &smaps)) {
      return std::nullopt;
    }
    return test_helper::find_memory_mapping_ext(reinterpret_cast<uintptr_t>(addr), smaps);
  }

  // Returns the RSS in bytes of the mapping that contains `addr`, from /proc/self/smaps.
  std::optional<size_t> GetRss(const volatile void* addr) {
    auto mapping = GetSmapsEntry(addr);
    if (!mapping) {
      return std::nullopt;
    }
    return mapping->rss * 1024;
  }

  // Maps `size` bytes of read-write private anonymous memory between two PROT_NONE guard pages,
  // so that its /proc/self/smaps entry is not merged with neighboring mappings. Returns the start
  // of the read-write memory in `start`.
  static fit::result<int, test_helper::ScopedMMap> MapIsolated(size_t size, uint8_t** start) {
    const size_t page_size = sysconf(_SC_PAGE_SIZE);
    auto mapping = test_helper::ScopedMMap::MMap(nullptr, size + 2 * page_size, PROT_NONE,
                                                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (mapping.is_error()) {
      return mapping;
    }
    *start = reinterpret_cast<uint8_t*>(mapping->mapping()) + page_size;
    if (mprotect(*start, size, PROT_READ | PROT_WRITE) != 0) {
      return fit::error(errno);
    }
    return mapping;
  }

  std::optional<size_t> GetVmLckInKb() {
    std::string status;
    if (!files::ReadFileToString(proc_path() + "/self/status", &status)) {
      return std::nullopt;
    }

    const std::string_view prefix = "VmLck";
    for (const auto& line :
         fxl::SplitString(status, "\n", fxl::kTrimWhitespace, fxl::kSplitWantNonEmpty)) {
      std::vector<std::string_view> fields =
          fxl::SplitString(line, ":", fxl::kTrimWhitespace, fxl::kSplitWantNonEmpty);
      if (fields[0] == prefix) {
        if (fields.size() != 2) {
          return std::nullopt;
        }
        return test_helper::parse_field_in_kb(fields[1]);
      }
    }
    return std::nullopt;
  }
};

TEST_F(MlockProcTest, UnalignedAddress) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  auto mapped_page = reinterpret_cast<uint8_t*>(mapping->mapping());
  ASSERT_EQ(mlock(reinterpret_cast<void*>(mapped_page + 10), page_size - 10), 0) << strerror(errno);

  std::string smaps;
  ASSERT_TRUE(files::ReadFileToString(proc_path() + "/self/smaps", &smaps));

  auto smap = test_helper::find_memory_mapping_ext(reinterpret_cast<uintptr_t>(mapped_page), smaps);
  ASSERT_NE(smap, std::nullopt);
  ASSERT_TRUE(smap->ContainsFlag("lo")) << *smap;
}

TEST_F(MlockProcTest, UnlignedLength) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  ASSERT_EQ(mlock(mapping->mapping(), page_size - 10), 0) << strerror(errno);

  std::string smaps;
  ASSERT_TRUE(files::ReadFileToString(proc_path() + "/self/smaps", &smaps));

  auto smap =
      test_helper::find_memory_mapping_ext(reinterpret_cast<uintptr_t>(mapping->mapping()), smaps);
  ASSERT_NE(smap, std::nullopt);
  ASSERT_TRUE(smap->ContainsFlag("lo")) << *smap;
}

TEST_F(MlockProcTest, RoundingIsCorrect) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size * 2, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  // Ask to lock two bytes, starting one byte before the page boundary. This should produce a 2-page
  // lock.
  auto midpoint = reinterpret_cast<uint8_t*>(mapping->mapping()) + page_size;
  ASSERT_EQ(mlock(midpoint - 1, 2), 0) << strerror(errno);

  std::string smaps;
  ASSERT_TRUE(files::ReadFileToString(proc_path() + "/self/smaps", &smaps));

  auto smap =
      test_helper::find_memory_mapping_ext(reinterpret_cast<uintptr_t>(mapping->mapping()), smaps);
  ASSERT_NE(smap, std::nullopt);
  ASSERT_EQ(smap->start, reinterpret_cast<uintptr_t>(mapping->mapping()));
  ASSERT_EQ(smap->end, reinterpret_cast<uintptr_t>(mapping->mapping()) + (2 * page_size));
  ASSERT_TRUE(smap->ContainsFlag("lo")) << *smap;
}

TEST_F(MlockProcTest, SplitMapping) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, page_size * 3, PROT_READ,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  auto first_page = reinterpret_cast<uint8_t*>(mapping->mapping());
  auto first_addr = reinterpret_cast<uintptr_t>(first_page);
  auto second_page = reinterpret_cast<void*>(first_page + page_size);
  auto second_addr = reinterpret_cast<uintptr_t>(second_page);
  auto third_addr = second_addr + page_size;

  // Lock the middle page of the mapping.
  ASSERT_EQ(mlock(second_page, page_size), 0) << strerror(errno);

  std::string smaps;
  ASSERT_TRUE(files::ReadFileToString(proc_path() + "/self/smaps", &smaps));

  auto first_mapping = test_helper::find_memory_mapping_ext(first_addr, smaps);
  ASSERT_NE(first_mapping, std::nullopt);
  ASSERT_EQ(first_mapping->start, first_addr) << *first_mapping;
  ASSERT_EQ(first_mapping->end, second_addr) << *first_mapping;
  ASSERT_FALSE(first_mapping->ContainsFlag("lo")) << *first_mapping;

  auto second_mapping = test_helper::find_memory_mapping_ext(second_addr, smaps);
  ASSERT_NE(second_mapping, std::nullopt);
  ASSERT_EQ(second_mapping->start, second_addr) << *second_mapping;
  ASSERT_EQ(second_mapping->end, third_addr) << *second_mapping;
  ASSERT_TRUE(second_mapping->ContainsFlag("lo")) << *second_mapping;

  auto third_mapping = test_helper::find_memory_mapping_ext(third_addr, smaps);
  ASSERT_NE(third_mapping, std::nullopt);
  ASSERT_EQ(third_mapping->start, third_addr) << *third_mapping;
  ASSERT_EQ(third_mapping->end, third_addr + page_size) << *third_mapping;
  ASSERT_FALSE(third_mapping->ContainsFlag("lo")) << *third_mapping;
}

TEST_F(MlockProcTest, MlockOnePageIncreasesVmLck) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 2 * page_size;

  // Map 2 private-anonymous pages.
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, map_size, PROT_READ | PROT_WRITE,
                                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  // Touch them to ensure non-zero number of committed bytes.
  reinterpret_cast<volatile char*>(mapping->mapping())[0] = 'a';
  reinterpret_cast<volatile char*>(mapping->mapping())[page_size] = 'a';

  // Record the `VmLck` value.
  auto vmlck_before = GetVmLckInKb();
  ASSERT_NE(vmlck_before, std::nullopt);

  // Test that locking any of the two pages results in the increase of VmLck by one page
  for (int i = 0; i < 2; i++) {
    // Lock one page.
    ASSERT_EQ(mlock(reinterpret_cast<uint8_t*>(mapping->mapping()) + (i * page_size), page_size), 0)
        << strerror(errno);

    // Record the `VmLck` value again.
    auto vmlck_after = GetVmLckInKb();
    ASSERT_NE(vmlck_after, std::nullopt);

    // Check that the new value is greater than the base one by exactly one page.
    EXPECT_GT(*vmlck_after, *vmlck_before);
    EXPECT_EQ(*vmlck_after - *vmlck_before, page_size / 1024);

    ASSERT_EQ(munlock(reinterpret_cast<uint8_t*>(mapping->mapping()) + (i * page_size), page_size),
              0)
        << strerror(errno);
  }
}

TEST_F(MlockProcTest, MlockPrefaultsPrivateAnonymousMemory) {
  if (kMlockAlwaysOnFault) {
    GTEST_SKIP() << "mlock() does not populate memory in this configuration";
  }
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 4 * page_size;
  uint8_t* start;
  auto mapping = MapIsolated(map_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  ASSERT_EQ(GetRss(start), 0u);

  ASSERT_THAT(mlock(start, map_size), SyscallSucceeds());
  EXPECT_EQ(GetRss(start), map_size);
}

TEST_F(MlockProcTest, MlockOnFaultDoesNotPrefault) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 4 * page_size;
  uint8_t* start;
  auto mapping = MapIsolated(map_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  ASSERT_THAT(syscall(SYS_mlock2, start, map_size, MLOCK_ONFAULT), SyscallSucceeds());
  EXPECT_EQ(GetRss(start), 0u);
}

TEST_F(MlockProcTest, MlockAfterMlockOnFaultPrefaults) {
  if (kMlockAlwaysOnFault) {
    GTEST_SKIP() << "mlock() does not populate memory in this configuration";
  }
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 2 * page_size;
  uint8_t* start;
  auto mapping = MapIsolated(map_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());

  ASSERT_THAT(syscall(SYS_mlock2, start, map_size, MLOCK_ONFAULT), SyscallSucceeds());
  ASSERT_EQ(GetRss(start), 0u);

  // Locking the already locked range again without MLOCK_ONFAULT populates it.
  ASSERT_THAT(mlock(start, map_size), SyscallSucceeds());
  EXPECT_EQ(GetRss(start), map_size);
}

TEST_F(MlockProcTest, MlockPrefaultsOnlyWritablePrivateAnonymousMappings) {
  if (kMlockAlwaysOnFault) {
    GTEST_SKIP() << "mlock() does not populate memory in this configuration";
  }
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 3 * page_size;
  uint8_t* start;
  auto mapping = MapIsolated(map_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  uint8_t* read_only_page = start + 2 * page_size;
  ASSERT_THAT(mprotect(read_only_page, page_size, PROT_READ), SyscallSucceeds());

  ASSERT_THAT(mlock(start, map_size), SyscallSucceeds());
  // Writable pages get their own memory, read-only ones are backed by the shared zero page,
  // which is not accounted in RSS.
  EXPECT_EQ(GetRss(start), 2 * page_size);
  EXPECT_EQ(GetRss(read_only_page), 0u);
}

TEST_F(MlockProcTest, MlockAfterForkCopiesPrivatePages) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t map_size = 2 * page_size;
  uint8_t* start;
  auto mapping = MapIsolated(map_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  auto base = reinterpret_cast<volatile char*>(start);
  base[0] = 'a';
  base[page_size] = 'a';

  test_helper::ForkHelper helper;
  helper.RunInForkedProcess([&] {
    ASSERT_THAT(mlock(start, map_size), SyscallSucceeds());
    if (!kMlockAlwaysOnFault) {
      EXPECT_EQ(GetRss(start), map_size);
    }
    // Populating the copy-on-write pages preserves their contents...
    EXPECT_EQ(base[0], 'a');
    EXPECT_EQ(base[page_size], 'a');
    base[0] = 'b';
    EXPECT_EQ(base[0], 'b');
  });
  ASSERT_TRUE(helper.WaitForChildren());

  // ...and does not affect the parent.
  EXPECT_EQ(base[0], 'a');
  EXPECT_EQ(base[page_size], 'a');
}

TEST_F(MlockProcTest, MlockStopsPopulatingAtProtNoneMapping) {
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  uint8_t* start;
  auto mapping = MapIsolated(3 * page_size, &start);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  uint8_t* prot_none_page = start + page_size;
  uint8_t* last_page = start + 2 * page_size;
  ASSERT_THAT(mprotect(prot_none_page, page_size, PROT_NONE), SyscallSucceeds());

  EXPECT_THAT(mlock(start, 3 * page_size), SyscallFailsWithErrno(ENOMEM));

  // The whole range is locked, but only the pages before the PROT_NONE mapping are populated.
  for (uint8_t* page : {start, prot_none_page, last_page}) {
    auto smap = GetSmapsEntry(page);
    ASSERT_NE(smap, std::nullopt);
    EXPECT_TRUE(smap->ContainsFlag("lo")) << *smap;
  }
  if (!kMlockAlwaysOnFault) {
    EXPECT_EQ(GetRss(start), page_size);
  }
  EXPECT_EQ(GetRss(last_page), 0u);
}

class MlockProtTest : public MlockProcTest,
                      public ::testing::WithParamInterface<std::tuple<int, int, bool>> {};

TEST_P(MlockProtTest, MlockApplied) {
  const auto [prot, sharing, file_backed] = GetParam();
  const size_t page_size = SAFE_SYSCALL(sysconf(_SC_PAGE_SIZE));
  const size_t size = 2 * page_size;

  test_helper::ScopedTempFD file;
  int fd = -1;
  int flags = sharing;
  if (file_backed) {
    ASSERT_TRUE(file);
    std::string contents(size, 'f');
    ASSERT_THAT(write(file.fd(), contents.data(), size), SyscallSucceedsWithValue(size));
    fd = file.fd();
  } else {
    flags |= MAP_ANONYMOUS;
  }
  auto mapping = test_helper::ScopedMMap::MMap(nullptr, size, prot, flags, fd, 0);
  ASSERT_THAT(mapping, SyscallResultIsOk());
  auto base = reinterpret_cast<volatile char*>(mapping->mapping());

  // PROT_NONE mappings cannot be populated, so mlock() fails, but the range is locked in all
  // cases.
  if (prot == PROT_NONE) {
    EXPECT_THAT(mlock(mapping->mapping(), size), SyscallFailsWithErrno(ENOMEM));
  } else {
    EXPECT_THAT(mlock(mapping->mapping(), size), SyscallSucceeds());
  }
  auto smap = GetSmapsEntry(base);
  ASSERT_NE(smap, std::nullopt);
  EXPECT_TRUE(smap->ContainsFlag("lo")) << *smap;

  // Locking preserves the contents of the mapping and its sharing semantics.
  const char initial = file_backed ? 'f' : 0;
  if (prot & PROT_READ) {
    EXPECT_EQ(base[0], initial);
    EXPECT_EQ(base[page_size], initial);
  }
  if (prot & PROT_WRITE) {
    base[0] = 'w';
    if (file_backed) {
      char c;
      ASSERT_THAT(pread(fd, &c, 1, 0), SyscallSucceedsWithValue(1));
      EXPECT_EQ(c, sharing == MAP_SHARED ? 'w' : 'f');
    }
  }
}

INSTANTIATE_TEST_SUITE_P(
    All, MlockProtTest,
    ::testing::Combine(::testing::Values(PROT_NONE, PROT_READ, PROT_WRITE, PROT_READ | PROT_WRITE),
                       ::testing::Values(MAP_PRIVATE, MAP_SHARED), ::testing::Bool()),
    [](const ::testing::TestParamInfo<MlockProtTest::ParamType>& info) {
      // Structured bindings would be split by the macro at their commas.
      const int prot = std::get<0>(info.param);
      const int sharing = std::get<1>(info.param);
      const bool file_backed = std::get<2>(info.param);
      std::string name;
      name += (prot & PROT_READ) ? "R" : "";
      name += (prot & PROT_WRITE) ? "W" : "";
      if (prot == PROT_NONE) {
        name += "None";
      }
      name += sharing == MAP_SHARED ? "_Shared" : "_Private";
      name += file_backed ? "_File" : "_Anon";
      return name;
    });
