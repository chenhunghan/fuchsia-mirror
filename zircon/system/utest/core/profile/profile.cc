// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/boot-options/boot-options.h>
#include <lib/fit/defer.h>
#include <lib/standalone-test/standalone.h>
#include <lib/zx/channel.h>
#include <lib/zx/exception.h>
#include <lib/zx/job.h>
#include <lib/zx/pager.h>
#include <lib/zx/profile.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>
#include <lib/zx/thread.h>
#include <lib/zx/timer.h>
#include <threads.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/exception.h>
#include <zircon/syscalls/object.h>
#include <zircon/syscalls/profile.h>
#include <zircon/syscalls/resource.h>
#include <zircon/syscalls/types.h>
#include <zircon/threads.h>
#include <zircon/time.h>

#include <algorithm>
#include <atomic>
#include <thread>

#include <zxtest/zxtest.h>

namespace profile {
namespace {

zx::result<zx::resource> GetSystemProfileResource() {
  zx::resource system_profile_resource;
  const zx_status_t status =
      zx::resource::create(*standalone::GetSystemResource(), ZX_RSRC_KIND_SYSTEM,
                           ZX_RSRC_SYSTEM_PROFILE_BASE, 1, nullptr, 0, &system_profile_resource);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(system_profile_resource));
}

zx_profile_info_t MakeSchedulerProfileInfo(int32_t priority) {
  zx_profile_info_t info = {};
  info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY;
  info.priority = priority;
  return info;
}

zx_profile_info_t MakeSchedulerProfileInfo(const zx_sched_deadline_params_t& params) {
  zx_profile_info_t info = {};
  info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE;
  info.deadline_params = params;
  return info;
}

zx_profile_info_t MakeCpuMaskProfile(uint64_t mask) {
  zx_profile_info_t info = {};
  info.flags = ZX_PROFILE_INFO_FLAG_CPU_MASK;
  info.cpu_affinity_mask.mask[0] = mask;
  return info;
}

zx_profile_info_t MakeMemoryPriorityProfile(int32_t priority) {
  zx_profile_info_t info = {};
  info.flags = ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY;
  info.priority = priority;
  return info;
}

size_t GetCpuCount() {
  size_t actual, available;
  auto system_resource = standalone::GetSystemResource();
  zx::result<zx::resource> result =
      standalone::GetSystemResourceWithBase(system_resource, ZX_RSRC_SYSTEM_INFO_BASE);
  ZX_ASSERT(result.status_value() == ZX_OK);
  zx::resource info_resource = std::move(result.value());

  zx_status_t status = info_resource.get_info(ZX_INFO_CPU_STATS, nullptr, 0, &actual, &available);
  ZX_ASSERT(status == ZX_OK);
  return available;
}

uint64_t GetAffinityMask(const zx::thread& thread) {
  zx_info_thread_t info;
  zx_status_t status = thread.get_info(ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr);
  ZX_ASSERT(status == ZX_OK);
  return info.cpu_affinity_mask.mask[0];
}

uint32_t GetLastScheduledCpu(const zx::thread& thread) {
  zx_info_thread_stats_t info;
  zx_status_t status = thread.get_info(ZX_INFO_THREAD_STATS, &info, sizeof(info), nullptr, nullptr);
  ZX_ASSERT(status == ZX_OK);
  return info.last_scheduled_cpu;
}

// Tests in this file rely that the default job is the root job.
TEST(SchedulerProfileTest, CreateProfileWithDefaultPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreatedProfileHasDefaultRights) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  zx::profile profile;
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));

  zx_info_handle_basic_t info = {};
  ASSERT_OK(profile.get_info(ZX_INFO_HANDLE_BASIC, &info, sizeof(info), nullptr, nullptr));
  EXPECT_EQ(ZX_DEFAULT_PROFILE_RIGHTS, info.rights);
}

TEST(SchedulerProfileTest, CreateProfileWithLowestPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_LOWEST);
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithLowPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_LOW);
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithHighPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_HIGH);
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithHighestPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_HIGHEST);
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateFairProfileWithNoInheritIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  zx::profile profile;

  profile_info.flags |= ZX_PROFILE_INFO_FLAG_NO_INHERIT;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithPriorityExceedingHighestIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_HIGHEST + 1);
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithPriorityBelowLowestIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_LOWEST - 1);
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlineIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(1), ZX_MSEC(8), ZX_MSEC(10)});
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithCriticalDeadlineIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(1), ZX_MSEC(8), ZX_MSEC(10)});
  profile_info.flags |= ZX_PROFILE_INFO_FLAG_CRITICAL;
  zx::profile profile;

  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithCriticalWithoutDeadlineIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  profile_info.flags |= ZX_PROFILE_INFO_FLAG_CRITICAL;
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithZeroCapacityIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(0), ZX_MSEC(8), ZX_MSEC(10)});
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlineBelowCapacityIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(8), ZX_MSEC(1), ZX_MSEC(10)});
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithPeriodBelowDeadlineIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(8), ZX_MSEC(10), ZX_MSEC(1)});
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileOnNonProfileResourceIsAccessDenied) {
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  zx::profile profile;
  zx::resource power_resource;
  zx::resource::create(*standalone::GetSystemResource(), ZX_RSRC_KIND_SYSTEM,
                       ZX_RSRC_SYSTEM_POWER_BASE, 1, nullptr, 0, &power_resource);

  ASSERT_EQ(ZX_ERR_ACCESS_DENIED, zx::profile::create(power_resource, 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithNonZeroOptionsIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo(ZX_PRIORITY_DEFAULT);
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 1u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlineAndNoInheritIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = MakeSchedulerProfileInfo({ZX_MSEC(1), ZX_MSEC(8), ZX_MSEC(10)});
  zx::profile profile;

  profile_info.flags |= ZX_PROFILE_INFO_FLAG_NO_INHERIT;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, SetThreadPriorityIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  std::atomic<const char*> error = nullptr;
  std::atomic<zx_status_t> result = ZX_OK;

  zx::profile profile_1;
  zx_profile_info_t info_1 = MakeSchedulerProfileInfo(ZX_PRIORITY_LOWEST);
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &info_1, &profile_1));

  zx::profile profile_2;
  zx_profile_info_t info_2 = MakeSchedulerProfileInfo(ZX_PRIORITY_HIGH);
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &info_2, &profile_2));

  zx::profile profile_3;
  zx_profile_info_t info_3 = MakeSchedulerProfileInfo({ZX_MSEC(8), ZX_MSEC(16), ZX_MSEC(16)});
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &info_3, &profile_3));

  // Operate on a background thread, just in case a failure changes the priority of the main
  // thread.
  std::thread worker(
      [](zx::profile first, zx::profile second, zx::profile third, std::atomic<const char*>* error,
         std::atomic<zx_status_t>* result) {
        *result = zx::thread::self()->set_profile(first, 0);
        if (*result != ZX_OK) {
          *error = "Failed to set first profile on thread";
          return;
        }
        std::this_thread::yield();

        *result = zx::thread::self()->set_profile(second, 0);
        if (*result != ZX_OK) {
          *error = "Failed to set second profile on thread";
          return;
        }
        std::this_thread::yield();

        *result = zx::thread::self()->set_profile(third, 0);
        if (*result != ZX_OK) {
          *error = "Failed to set third profile on thread";
          return;
        }
      },
      std::move(profile_1), std::move(profile_2), std::move(profile_3), &error, &result);

  // Wait until is completed.
  worker.join();

  ASSERT_OK(result.load(), "%s", error.load());
}

TEST(SchedulerProfileTest, SetProfileWithNonZeroOptionsIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  zx::profile profile;
  zx_profile_info_t info = MakeSchedulerProfileInfo(ZX_PRIORITY_LOWEST);
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &info, &profile));

  ASSERT_EQ(ZX_ERR_INVALID_ARGS, zx::thread::self()->set_profile(profile, 1u));
}

TEST(ProfileTest, CreateProfileWithDefaultInitializedProfileInfoIsError) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = {};
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(ProfileTest, CreateProfileWithMutuallyExclusiveFlagsIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx_profile_info_t profile_info = {};
  profile_info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY | ZX_PROFILE_INFO_FLAG_DEADLINE;
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(ProfileTest, CreateProfileWithNoProfileInfoIsInvalidArgs) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_INVALID_ARGS,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, nullptr, &profile));
}

TEST(ProfileTest, CreateProfileWithInvalidHandleIsBadHandle) {
  zx::profile profile;

  ASSERT_EQ(ZX_ERR_BAD_HANDLE,
            zx::profile::create(zx::resource(ZX_HANDLE_INVALID), 0u, nullptr, &profile));
}

zx_status_t RunThreadWithProfile(const zx::profile& profile,
                                 const std::function<zx_status_t()>& body) {
  zx_status_t result;
  std::thread worker([&body, &result, &profile]() {
    result = zx::thread::self()->set_profile(profile, 0);
    if (result != ZX_OK) {
      return;
    }
    result = body();
  });
  worker.join();
  return result;
}

TEST(CpuMaskProfile, EmptyMaskIsValid) {
  zx::profile profile;
  zx_profile_info_t profile_info = MakeCpuMaskProfile(0);
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));

  // Ensure that the thread can still run, despite the affinity mask
  // having no valid CPUs in it. (The kernel will just fall back to
  // its own choice of CPUs if this mask can't be respected.)
  ASSERT_OK(RunThreadWithProfile(profile, []() {
    EXPECT_EQ(GetAffinityMask(*zx::thread::self()), 0);
    EXPECT_NE(GetLastScheduledCpu(*zx::thread::self()), ZX_INFO_INVALID_CPU);
    return ZX_OK;
  }));
}

TEST(CpuMaskProfile, ApplyProfile) {
  const size_t num_cpus = GetCpuCount();
  ASSERT_LT(num_cpus, ZX_CPU_SET_BITS_PER_WORD,
            "Test assumes system running with less than %d cores.", ZX_CPU_SET_BITS_PER_WORD);
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  for (size_t i = 0; i < num_cpus; i++) {
    zx_profile_info_t profile_info = MakeCpuMaskProfile(1 << i);
    zx::profile profile;
    ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));

    // Ensure that the correct mask was applied.
    ASSERT_OK(RunThreadWithProfile(profile, [i]() {
      EXPECT_EQ(GetAffinityMask(*zx::thread::self()), (1 << i));
      EXPECT_EQ(GetLastScheduledCpu(*zx::thread::self()), i);
      return ZX_OK;
    }));
  }
}

TEST(MemoryPriorityProfile, InvalidPriorities) {
  constexpr int32_t kBadPriorities[] = {ZX_PRIORITY_LOWEST, ZX_PRIORITY_LOW, ZX_PRIORITY_HIGHEST};
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  for (const int32_t prio : kBadPriorities) {
    zx::profile profile;

    zx_profile_info_t profile_info = MakeMemoryPriorityProfile(prio);
    EXPECT_EQ(ZX_ERR_INVALID_ARGS,
              zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
  }
}

TEST(MemoryPriorityProfile, MemoryOrThread) {
  const uint32_t kInvalidWith[] = {ZX_PROFILE_INFO_FLAG_PRIORITY, ZX_PROFILE_INFO_FLAG_CPU_MASK,
                                   ZX_PROFILE_INFO_FLAG_DEADLINE};
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  for (const uint32_t invalid_with : kInvalidWith) {
    zx_profile_info_t profile_info = {};
    profile_info.flags = ZX_PROFILE_INFO_FLAG_MEMORY_PRIORITY | invalid_with;
    zx::profile profile;

    ASSERT_EQ(ZX_ERR_INVALID_ARGS,
              zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
  }
}

TEST(MemoryPriorityProfile, ApplyProfile) {
  // Create the two profiles we will need.
  zx::profile profile_high, profile_default;
  zx_profile_info_t profile_info = MakeMemoryPriorityProfile(ZX_PRIORITY_HIGH);
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile_high));
  profile_info = MakeMemoryPriorityProfile(ZX_PRIORITY_DEFAULT);
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile_default));

  // To ensure there are some candidate reclaimable pages, create and map in a pager backed VMO.
  zx::port port;
  ASSERT_OK(zx::port::create(0, &port));
  zx::pager pager;
  ASSERT_OK(zx::pager::create(0, &pager));
  zx::vmo pager_vmo;
  ASSERT_OK(pager.create_vmo(0, port, 0, zx_system_get_page_size(), &pager_vmo));
  zx_vaddr_t addr;
  ASSERT_OK(zx::vmar::root_self()->map(0, 0, pager_vmo, 0, zx_system_get_page_size(), &addr));
  auto unmap = fit::defer([&addr]() {
    // Cleanup the mapping we created.
    zx::vmar::root_self()->unmap(addr, zx_system_get_page_size());
  });

  // Helper to supply pages to the VMO. Since these pages are reclaimable there is a small chance
  // that they get evicted during the execution of the test so we re-supply at a few different
  // points.
  auto supply = [&]() {
    zx::vmo aux_vmo;
    ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &aux_vmo));
    uint64_t val = 42;
    EXPECT_OK(aux_vmo.write(&val, 0, sizeof(val)));
    EXPECT_OK(pager.supply_pages(pager_vmo, 0, zx_system_get_page_size(), aux_vmo, 0));
  };

  // Start with the pages supplied so that they are there for the initial query.
  supply();

  zx_info_kmem_stats_extended_t stats;
  auto system_resource = standalone::GetSystemResource();
  zx::result<zx::resource> result =
      standalone::GetSystemResourceWithBase(system_resource, ZX_RSRC_SYSTEM_INFO_BASE);
  ASSERT_OK(result.status_value());
  zx::resource info_resource = std::move(result.value());

  EXPECT_OK(
      info_resource.get_info(ZX_INFO_KMEM_STATS_EXTENDED, &stats, sizeof(stats), nullptr, nullptr));

  const uint64_t prev = stats.vmo_reclaim_disabled_bytes;

  EXPECT_OK(zx::vmar::root_self()->set_profile(profile_high, 0));

  // Applying the profile should have caused our pages to no longer be reclaimable. The pager backed
  // VMO we mapped means we know it's definitely non-zero, but we do not know if there are more.
  // In between the previous supply and setting the profile, the pages could have been evicted, so
  // re-supply them.
  supply();
  EXPECT_OK(
      info_resource.get_info(ZX_INFO_KMEM_STATS_EXTENDED, &stats, sizeof(stats), nullptr, nullptr));
  EXPECT_GT(stats.vmo_reclaim_disabled_bytes, prev);

  // Applying the default priority should undo the reclamation change.
  EXPECT_OK(zx::vmar::root_self()->set_profile(profile_default, 0));

  EXPECT_OK(
      info_resource.get_info(ZX_INFO_KMEM_STATS_EXTENDED, &stats, sizeof(stats), nullptr, nullptr));
  EXPECT_EQ(stats.vmo_reclaim_disabled_bytes, prev);
}

TEST(MemoryPriorityProfile, Rights) {
  zx::profile profile;
  zx_profile_info_t profile_info = MakeMemoryPriorityProfile(ZX_PRIORITY_DEFAULT);
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));

  // Duplicate the vmar handle to have valid and invalid permissions.
  zx::vmar vmar_valid, vmar_invalid;
  ASSERT_OK(zx::vmar::root_self()->duplicate(ZX_RIGHT_OP_CHILDREN, &vmar_valid));
  ASSERT_OK(zx::vmar::root_self()->duplicate(0, &vmar_invalid));

  EXPECT_OK(vmar_valid.set_profile(profile, 0));
  EXPECT_EQ(ZX_ERR_ACCESS_DENIED, vmar_invalid.set_profile(profile, 0));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlineCapacityOutOfRangeIsOutOfRange) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  constexpr int64_t kOutOfRangeDuration = static_cast<int64_t>(INT32_MAX) + 1;

  zx_profile_info_t profile_info = {};
  profile_info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE;
  profile_info.deadline_params.capacity = kOutOfRangeDuration;
  profile_info.deadline_params.relative_deadline = kOutOfRangeDuration + 1;
  profile_info.deadline_params.period = kOutOfRangeDuration + 2;
  zx::profile profile;
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlineRelativeDeadlineOutOfRangeIsOutOfRange) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  constexpr int64_t kOutOfRangeDuration = static_cast<int64_t>(INT32_MAX) + 1;

  zx_profile_info_t profile_info = {};
  profile_info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE;
  profile_info.deadline_params.capacity = 10;
  profile_info.deadline_params.relative_deadline = kOutOfRangeDuration;
  profile_info.deadline_params.period = kOutOfRangeDuration + 1;
  zx::profile profile;
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(SchedulerProfileTest, CreateProfileWithDeadlinePeriodOutOfRangeIsOutOfRange) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  constexpr int64_t kOutOfRangeDuration = static_cast<int64_t>(INT32_MAX) + 1;

  zx_profile_info_t profile_info = {};
  profile_info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE;
  profile_info.deadline_params.capacity = 10;
  profile_info.deadline_params.relative_deadline = 20;
  profile_info.deadline_params.period = kOutOfRangeDuration;
  zx::profile profile;
  EXPECT_EQ(ZX_ERR_OUT_OF_RANGE,
            zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));
}

TEST(ProfileTest, ApplySchedulingProfileToVmarIsOk) {
  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  zx_profile_info_t profile_info = {};
  profile_info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY;
  profile_info.priority = 10;
  zx::profile profile;
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &profile_info, &profile));

  // Applying scheduling profile to VMAR should succeed (doing nothing)
  EXPECT_OK(zx::vmar::root_self()->set_profile(profile, 0));
}

// Regression test for b/511528549: verifies that remote priority inheritance propagation on an
// idle CPU does not cause an assertion failure in energy statistics calculation due to mismatched
// per-CPU idle time attribution.
TEST(SchedulerProfileTest, DeadlinePiUtilizationPanicRepro) {
  if (GetCpuCount() < 2) {
    ZXTEST_SKIP("At least 2 CPUs are required for this test");
  }

  zx::result<zx::resource> maybe_profile_rsrc = GetSystemProfileResource();
  ASSERT_OK(maybe_profile_rsrc.status_value());

  zx::profile prof_dl;
  zx_profile_info_t pinfo = MakeSchedulerProfileInfo({ZX_MSEC(2), ZX_MSEC(3), ZX_MSEC(3)});
  ASSERT_OK(zx::profile::create(maybe_profile_rsrc.value(), 0u, &pinfo, &prof_dl));

  zx::channel ch1, ch2;
  ASSERT_OK(zx::channel::create(0, &ch1, &ch2));

  zx_futex_t futex1 = 1;
  std::atomic<bool> ready = false;
  std::atomic<bool> done = false;
  std::atomic<bool> failed = false;

  zx::thread main_thread_handle;
  ASSERT_OK(zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &main_thread_handle));

  auto t1_func = [&]() {
    if (zx_status_t status = zx::thread::self()->set_profile(prof_dl, 0); status != ZX_OK) {
      failed = true;
      EXPECT_OK(status);
      return;
    }
    while (!ready && !failed) {
      zx::nanosleep(zx::deadline_after(zx::usec(1)));
    }
    while (!done && !failed) {
      zx_futex_wait(&futex1, 1, main_thread_handle.get(), zx_deadline_after(ZX_USEC(10)));
    }
  };

  std::thread t1(t1_func);
  std::thread t3(t1_func);

  std::thread t2([&]() {
    while (!ready && !failed) {
      zx::nanosleep(zx::deadline_after(zx::usec(1)));
    }
    char msg[16];
    uint32_t act_bytes, act_handles;
    while (!done && !failed) {
      if (ch2.wait_one(ZX_CHANNEL_READABLE, zx::deadline_after(zx::msec(10)), nullptr) == ZX_OK) {
        if (ch2.read(0, msg, nullptr, sizeof(msg), 0, &act_bytes, &act_handles) == ZX_OK) {
          ch2.write(0, msg, 8, nullptr, 0);
        }
      }
    }
  });

  zx::nanosleep(zx::deadline_after(zx::msec(10)));
  ready = true;

  char tx_msg[8] = "msg";
  char rx_msg[8] = {0};
  zx_channel_call_args_t args = {
      .wr_bytes = tx_msg,
      .wr_handles = nullptr,
      .rd_bytes = rx_msg,
      .rd_handles = nullptr,
      .wr_num_bytes = 8,
      .wr_num_handles = 0,
      .rd_num_bytes = 8,
      .rd_num_handles = 0,
  };

  uint32_t act_bytes, act_handles;
  for (int i = 0; i < 50000 && !done && !failed; i++) {
    ch1.call(0, zx::deadline_after(zx::msec(50)), &args, &act_bytes, &act_handles);
    if (i % 50 == 0) {
      zx::nanosleep(zx::deadline_after(zx::usec(1)));
    }
  }

  done = true;
  zx_futex_wake(&futex1, 0xFFFFFFFF);
  t1.join();
  t3.join();
  t2.join();

  EXPECT_FALSE(failed.load());
}

// Returns a profile that lets a thread run on every CPU. The fair starvation tests use it to undo
// the affinity they give the thread that runs the tests.
zx_profile_info_t MakeAllCpusProfile() {
  const size_t num_cpus = GetCpuCount();
  return MakeCpuMaskProfile(num_cpus >= ZX_CPU_SET_BITS_PER_WORD ? ~uint64_t{0}
                                                                 : (uint64_t{1} << num_cpus) - 1);
}

// Resumes every exception that arrives on `exceptions` until `thread` has terminated. A fair
// starvation report can still come after a test has stopped waiting for one. Standalone core
// tests have no other exception handler, and an unhandled policy exception would terminate the
// process instead of failing the test.
void ResumeExceptionsUntilTerminated(const zx::channel& exceptions, const zx::thread& thread) {
  for (;;) {
    zx_wait_item_t items[] = {
        {.handle = exceptions.get(), .waitfor = ZX_CHANNEL_READABLE, .pending = 0},
        {.handle = thread.get(), .waitfor = ZX_THREAD_TERMINATED, .pending = 0},
    };
    ZX_ASSERT(zx::handle::wait_many(items, 2, zx::time::infinite()) == ZX_OK);
    if (items[0].pending & ZX_CHANNEL_READABLE) {
      zx_exception_info_t info = {};
      zx::exception exception;
      ZX_ASSERT(exceptions.read(0, &info, exception.reset_and_get_address(), sizeof(info), 1,
                                nullptr, nullptr) == ZX_OK);
      const uint32_t handled = ZX_EXCEPTION_STATE_HANDLED;
      ZX_ASSERT(exception.set_property(ZX_PROP_EXCEPTION_STATE, &handled, sizeof(handled)) ==
                ZX_OK);
      continue;
    }
    if (items[1].pending & ZX_THREAD_TERMINATED) {
      return;
    }
  }
}

// Blocks until `futex` is set with `SetAndWakeFutex`. The fair threads of the fair starvation
// tests wait this way so that they are not runnable, and so not counted as starved, before the
// test starts them.
void WaitForFutex(const std::atomic<zx_futex_t>& futex) {
  while (futex.load() == 0) {
    zx_futex_wait(reinterpret_cast<const zx_futex_t*>(&futex), 0, ZX_HANDLE_INVALID,
                  ZX_TIME_INFINITE);
  }
}

void SetAndWakeFutex(std::atomic<zx_futex_t>& futex) {
  futex.store(1);
  zx_futex_wake(reinterpret_cast<const zx_futex_t*>(&futex), UINT32_MAX);
}

// Exercises the lockup detector's fair starvation check: a deadline thread that polls with yield
// keeps a fair thread on the same CPU from running, the detector reports it, and raises a policy
// exception on the polling thread.
//
// The check is off by default, so this test only runs when booted with:
//   kernel.lockup-detector.fair-starvation-threshold-ms=<nonzero>
//   kernel.lockup-detector.fair-starvation-exception=true
TEST(SchedulerProfileTest, FairStarvationRaisesPolicyException) {
  const BootOptions& boot_options = standalone::GetBootOptions();
  if (boot_options.lockup_detector_fair_starvation_threshold_ms == 0 ||
      !boot_options.lockup_detector_fair_starvation_exception ||
      boot_options.lockup_detector_heartbeat_period_ms == 0) {
    ZXTEST_SKIP("fair starvation detection is not enabled");
  }
  if (GetCpuCount() < 2) {
    ZXTEST_SKIP("test requires two CPUs");
  }

  constexpr uint32_t kBusyCpu = 1;
  constexpr uint64_t kBusyCpuMask = uint64_t{1} << kBusyCpu;
  constexpr uint64_t kTestCpuMask = 1;

  zx::result<zx::resource> profile_resource = GetSystemProfileResource();
  ASSERT_OK(profile_resource.status_value());

  // A deadline profile at 100% utilization and a fair profile, both pinned to the busy CPU, and
  // a profile that keeps this thread off it.
  zx_profile_info_t spinner_info = {};
  spinner_info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE | ZX_PROFILE_INFO_FLAG_CPU_MASK;
  spinner_info.deadline_params = zx_sched_deadline_params_t{ZX_MSEC(2), ZX_MSEC(2), ZX_MSEC(2)};
  spinner_info.cpu_affinity_mask.mask[0] = kBusyCpuMask;
  zx_profile_info_t waiter_info = {};
  waiter_info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY | ZX_PROFILE_INFO_FLAG_CPU_MASK;
  waiter_info.priority = ZX_PRIORITY_DEFAULT;
  waiter_info.cpu_affinity_mask.mask[0] = kBusyCpuMask;
  zx_profile_info_t test_info = MakeCpuMaskProfile(kTestCpuMask);
  zx_profile_info_t all_cpus_info = MakeAllCpusProfile();

  zx::profile spinner_profile, waiter_profile, test_profile, all_cpus_profile;
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &spinner_info, &spinner_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &waiter_info, &waiter_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &test_info, &test_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &all_cpus_info, &all_cpus_profile));
  // This thread runs the rest of the tests too, so let it run anywhere again afterwards.
  ASSERT_OK(zx::thread::self()->set_profile(test_profile, 0));
  auto restore_affinity =
      fit::defer([&] { ZX_ASSERT(zx::thread::self()->set_profile(all_cpus_profile, 0) == ZX_OK); });

  struct State {
    std::atomic<bool> spinner_go{false};
    std::atomic<zx_futex_t> waiter_go{0};
    std::atomic<bool> stop{false};
  } state;

  auto spinner_entry = [](void* arg) -> int {
    auto* state = static_cast<State*>(arg);
    while (!state->spinner_go.load()) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    while (!state->stop.load()) {
      zx_thread_legacy_yield(0);
    }
    return 0;
  };
  auto waiter_entry = [](void* arg) -> int {
    auto* state = static_cast<State*>(arg);
    WaitForFutex(state->waiter_go);
    return 0;
  };

  // The threads use `state`, so they are stopped and joined however the test ends. The spinner
  // is stopped first: the waiter may not get to run until then. Exceptions are handled until
  // the spinner is gone, because a report can still come after the test has given up on it.
  zx::channel exception_channel;
  thrd_t spinner, waiter;
  ASSERT_EQ(thrd_success, thrd_create(&waiter, waiter_entry, &state));
  auto join_waiter = fit::defer([&] {
    SetAndWakeFutex(state.waiter_go);
    ZX_ASSERT(thrd_join(waiter, nullptr) == thrd_success);
  });
  ASSERT_EQ(thrd_success, thrd_create(&spinner, spinner_entry, &state));
  zx::unowned_thread spinner_thread{thrd_get_zx_handle(spinner)};
  zx::unowned_thread waiter_thread{thrd_get_zx_handle(waiter)};
  auto join_spinner = fit::defer([&] {
    state.spinner_go.store(true);
    state.stop.store(true);
    if (exception_channel) {
      ResumeExceptionsUntilTerminated(exception_channel, *spinner_thread);
    }
    ZX_ASSERT(thrd_join(spinner, nullptr) == thrd_success);
  });

  ASSERT_OK(spinner_thread->create_exception_channel(0, &exception_channel));
  ASSERT_OK(spinner_thread->set_profile(spinner_profile, 0));
  ASSERT_OK(waiter_thread->set_profile(waiter_profile, 0));

  // Let any cool-down left on the busy CPU by an earlier test run out. During a cool-down the
  // waiter would run and nothing would be reported.
  zx::nanosleep(zx::deadline_after(
      zx::msec(static_cast<int64_t>(boot_options.lockup_detector_fair_starvation_relief_ms)) +
      zx::msec(100)));

  // Start the spinner, then let the waiter become runnable behind it.
  state.spinner_go.store(true);
  zx::nanosleep(zx::deadline_after(zx::msec(50)));
  SetAndWakeFutex(state.waiter_go);

  // The detector needs the threshold plus up to two heartbeat periods to report.
  const zx::duration timeout =
      zx::msec(static_cast<int64_t>(boot_options.lockup_detector_fair_starvation_threshold_ms +
                                    3 * boot_options.lockup_detector_heartbeat_period_ms)) +
      zx::sec(5);
  const zx_status_t wait_status =
      exception_channel.wait_one(ZX_CHANNEL_READABLE, zx::deadline_after(timeout), nullptr);

  // Read the exception and resume the thread, let both threads finish, then check the results.
  zx_exception_info_t exception_info = {};
  zx_exception_report_t report = {};
  zx_status_t read_status = ZX_ERR_BAD_STATE;
  zx_status_t report_status = ZX_ERR_BAD_STATE;
  if (wait_status == ZX_OK) {
    zx::exception exception;
    read_status = exception_channel.read(0, &exception_info, exception.reset_and_get_address(),
                                         sizeof(exception_info), 1, nullptr, nullptr);
    report_status = spinner_thread->get_info(ZX_INFO_THREAD_EXCEPTION_REPORT, &report,
                                             sizeof(report), nullptr, nullptr);
    if (read_status == ZX_OK) {
      const uint32_t handled = ZX_EXCEPTION_STATE_HANDLED;
      EXPECT_OK(exception.set_property(ZX_PROP_EXCEPTION_STATE, &handled, sizeof(handled)));
    }
  }
  join_spinner.call();
  join_waiter.call();

  ASSERT_OK(wait_status, "no exception was raised on the polling thread");
  ASSERT_OK(read_status);
  ASSERT_OK(report_status);
  EXPECT_EQ(ZX_EXCP_POLICY_ERROR, exception_info.type);
  EXPECT_EQ(ZX_EXCP_POLICY_CODE_SPIN_DETECTION, report.context.synth_code);
  EXPECT_EQ(kBusyCpu, report.context.synth_data);
}

// Exercises the relief cool-down that follows a fair starvation report: fair threads on a CPU
// held by a deadline thread get nothing before the report, a share of the CPU for the cool-down
// after it, and nothing again once the cool-down ends, which leads to a second report. The
// deadline thread's capacity and period are both `spinner_period`.
//
// The share is measured over the middle half of the cool-down, so the test is skipped unless
// the cool-down is at least four relief windows long.
//
// The check is off by default, so this only runs when booted with:
//   kernel.lockup-detector.fair-starvation-threshold-ms=<nonzero>
//   kernel.lockup-detector.fair-starvation-exception=true
//   kernel.lockup-detector.fair-starvation-relief-ms=<nonzero>
void FairStarvationReliefCoolDown(zx_duration_t spinner_period) {
  const BootOptions& boot_options = standalone::GetBootOptions();
  if (boot_options.lockup_detector_fair_starvation_threshold_ms == 0 ||
      !boot_options.lockup_detector_fair_starvation_exception ||
      boot_options.lockup_detector_fair_starvation_relief_ms == 0 ||
      boot_options.lockup_detector_heartbeat_period_ms == 0) {
    ZXTEST_SKIP("fair starvation relief is not enabled");
  }
  if (GetCpuCount() < 2) {
    ZXTEST_SKIP("test requires two CPUs");
  }

  constexpr uint64_t kBusyCpuMask = uint64_t{1} << 1;
  constexpr uint64_t kTestCpuMask = 1;
  const zx::duration relief =
      zx::msec(static_cast<int64_t>(boot_options.lockup_detector_fair_starvation_relief_ms));
  const zx::duration heartbeat =
      zx::msec(static_cast<int64_t>(boot_options.lockup_detector_heartbeat_period_ms));
  // The kernel raises a threshold that is shorter than the heartbeat period to it.
  const zx::duration threshold = std::max(
      zx::msec(static_cast<int64_t>(boot_options.lockup_detector_fair_starvation_threshold_ms)),
      heartbeat);
  const int64_t percent =
      std::clamp<int64_t>(boot_options.lockup_detector_fair_starvation_relief_percent, 1, 50);
  const int64_t window_ms = std::clamp<int64_t>(
      static_cast<int64_t>(boot_options.lockup_detector_fair_starvation_relief_window_ms), 1, 1000);
  if (relief < zx::msec(window_ms * 4)) {
    ZXTEST_SKIP("the relief cool-down is shorter than four relief windows");
  }

  zx::result<zx::resource> profile_resource = GetSystemProfileResource();
  ASSERT_OK(profile_resource.status_value());

  zx_profile_info_t spinner_info = {};
  spinner_info.flags = ZX_PROFILE_INFO_FLAG_DEADLINE | ZX_PROFILE_INFO_FLAG_CPU_MASK;
  spinner_info.deadline_params =
      zx_sched_deadline_params_t{spinner_period, spinner_period, spinner_period};
  spinner_info.cpu_affinity_mask.mask[0] = kBusyCpuMask;
  zx_profile_info_t worker_info = {};
  worker_info.flags = ZX_PROFILE_INFO_FLAG_PRIORITY | ZX_PROFILE_INFO_FLAG_CPU_MASK;
  worker_info.priority = ZX_PRIORITY_DEFAULT;
  worker_info.cpu_affinity_mask.mask[0] = kBusyCpuMask;
  zx_profile_info_t test_info = MakeCpuMaskProfile(kTestCpuMask);
  zx_profile_info_t all_cpus_info = MakeAllCpusProfile();

  zx::profile spinner_profile, worker_profile, test_profile, all_cpus_profile;
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &spinner_info, &spinner_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &worker_info, &worker_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &test_info, &test_profile));
  ASSERT_OK(zx::profile::create(*profile_resource, 0u, &all_cpus_info, &all_cpus_profile));
  // This thread runs the rest of the tests too, so let it run anywhere again afterwards.
  ASSERT_OK(zx::thread::self()->set_profile(test_profile, 0));
  auto restore_affinity =
      fit::defer([&] { ZX_ASSERT(zx::thread::self()->set_profile(all_cpus_profile, 0) == ZX_OK); });

  struct State {
    std::atomic<bool> spinner_go{false};
    std::atomic<zx_futex_t> worker_go{0};
    std::atomic<bool> stop{false};
    // When the worker first ran after it was started; 0 until then.
    std::atomic<zx_time_t> worker_first_ran{0};
  } state;

  // Both threads are CPU-bound once started; neither makes a syscall in its loop.
  auto spinner_entry = [](void* arg) -> int {
    auto* state = static_cast<State*>(arg);
    while (!state->spinner_go.load()) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
    while (!state->stop.load()) {
    }
    return 0;
  };
  auto worker_entry = [](void* arg) -> int {
    auto* state = static_cast<State*>(arg);
    WaitForFutex(state->worker_go);
    state->worker_first_ran.store(zx_clock_get_monotonic());
    while (!state->stop.load()) {
    }
    return 0;
  };

  // The threads use `state`, so they are stopped and joined however the test ends. The spinner
  // is stopped first: the worker may not get to run until then. Exceptions are handled until
  // the spinner is gone, because a report can still come after the test has given up on it.
  zx::channel exception_channel;
  thrd_t spinner, worker;
  ASSERT_EQ(thrd_success, thrd_create(&worker, worker_entry, &state));
  auto join_worker = fit::defer([&] {
    state.stop.store(true);
    SetAndWakeFutex(state.worker_go);
    ZX_ASSERT(thrd_join(worker, nullptr) == thrd_success);
  });
  ASSERT_EQ(thrd_success, thrd_create(&spinner, spinner_entry, &state));
  zx::unowned_thread spinner_thread{thrd_get_zx_handle(spinner)};
  zx::unowned_thread worker_thread{thrd_get_zx_handle(worker)};
  auto join_spinner = fit::defer([&] {
    state.spinner_go.store(true);
    state.stop.store(true);
    if (exception_channel) {
      ResumeExceptionsUntilTerminated(exception_channel, *spinner_thread);
    }
    ZX_ASSERT(thrd_join(spinner, nullptr) == thrd_success);
  });

  ASSERT_OK(spinner_thread->create_exception_channel(0, &exception_channel));
  ASSERT_OK(spinner_thread->set_profile(spinner_profile, 0));
  ASSERT_OK(worker_thread->set_profile(worker_profile, 0));

  auto worker_runtime = [&]() {
    zx_info_thread_stats_t stats = {};
    ZX_ASSERT(worker_thread->get_info(ZX_INFO_THREAD_STATS, &stats, sizeof(stats), nullptr,
                                      nullptr) == ZX_OK);
    return zx::duration(stats.total_runtime);
  };
  // Waits for a report's exception on the spinner and resumes it.
  auto take_exception = [&](zx::duration timeout) {
    zx_status_t status =
        exception_channel.wait_one(ZX_CHANNEL_READABLE, zx::deadline_after(timeout), nullptr);
    if (status != ZX_OK) {
      return status;
    }
    zx_exception_info_t info = {};
    zx::exception exception;
    status = exception_channel.read(0, &info, exception.reset_and_get_address(), sizeof(info), 1,
                                    nullptr, nullptr);
    if (status != ZX_OK) {
      return status;
    }
    const uint32_t handled = ZX_EXCEPTION_STATE_HANDLED;
    return exception.set_property(ZX_PROP_EXCEPTION_STATE, &handled, sizeof(handled));
  };

  // Let any cool-down left on the busy CPU by an earlier test run out, so that this test starts
  // with no relief in effect.
  zx::nanosleep(zx::deadline_after(relief + zx::msec(100)));

  // Start the spinner, then let the worker become runnable behind it.
  state.spinner_go.store(true);
  zx::nanosleep(zx::deadline_after(zx::msec(50)));
  const zx::time worker_started = zx::clock::get_monotonic();
  SetAndWakeFutex(state.worker_go);

  const zx::duration report_timeout = threshold + heartbeat * 3 + zx::sec(5);
  const zx_status_t first_status = take_exception(report_timeout);
  const zx::time first_report = zx::clock::get_monotonic();

  // Measure the worker's share over the middle of the cool-down.
  zx::duration share_runtime, share_elapsed;
  zx_status_t second_status = ZX_ERR_BAD_STATE;
  zx::time second_report;
  if (first_status == ZX_OK) {
    zx::nanosleep(zx::deadline_after(relief / 4));
    const zx::time share_start = zx::clock::get_monotonic();
    const zx::duration runtime_start = worker_runtime();
    zx::nanosleep(zx::deadline_after(relief / 2));
    share_runtime = worker_runtime() - runtime_start;
    share_elapsed = zx::clock::get_monotonic() - share_start;

    // Once the cool-down ends the worker is starved again, which is reported again.
    second_status = take_exception(relief + report_timeout);
    second_report = zx::clock::get_monotonic();
  }

  join_spinner.call();
  join_worker.call();

  ASSERT_OK(first_status, "no report for the spinning deadline thread");
  // No relief before the report: the worker first ran a full threshold or more after it was
  // started. The worker records the time itself, so this does not depend on how quickly this
  // thread is scheduled.
  const zx::time worker_first_ran{state.worker_first_ran.load()};
  ASSERT_NE(worker_first_ran.get(), 0, "the fair thread never ran");
  EXPECT_GE((worker_first_ran - worker_started).to_msecs(), threshold.to_msecs(),
            "the fair thread ran before the report");

  // During the cool-down the worker gets about the configured share. The bounds are loose to
  // tolerate emulators: at least a third of it, at most three times it.
  const int64_t share_permille = share_runtime.to_usecs() * 1000 / share_elapsed.to_usecs();
  printf("fair share during cool-down: %ld.%ld%% (configured %ld%%)\n", share_permille / 10,
         share_permille % 10, percent);
  EXPECT_GE(share_permille, percent * 10 / 3, "the fair thread got too little during cool-down");
  EXPECT_LE(share_permille, percent * 10 * 3, "the fair thread got too much during cool-down");

  ASSERT_OK(second_status, "no second report after the cool-down ended");
  // The second report needs the cool-down to end and a full threshold of starvation after it:
  // detection starts over when the cool-down ends.
  EXPECT_GE((second_report - first_report).to_msecs(), (relief + threshold).to_msecs() - 100,
            "the second report came too early");
}

TEST(SchedulerProfileTest, FairStarvationReliefCoolDown) {
  ASSERT_NO_FATAL_FAILURE(FairStarvationReliefCoolDown(ZX_MSEC(2)));
}

// The deadline thread's time slices are several relief windows long, so fair threads only get
// their share if the deadline thread is preempted at the end of each window.
TEST(SchedulerProfileTest, FairStarvationReliefCoolDownLongSlices) {
  ASSERT_NO_FATAL_FAILURE(FairStarvationReliefCoolDown(ZX_MSEC(100)));
}

}  // namespace
}  // namespace profile
