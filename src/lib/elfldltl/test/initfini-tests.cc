// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/diagnostics.h>
#include <lib/elfldltl/dynamic.h>
#include <lib/elfldltl/init-fini.h>
#include <lib/elfldltl/memory.h>
#include <lib/elfldltl/testing/typed-test.h>

#include <array>
#include <string>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using NativeInfo = elfldltl::InitFiniInfo<elfldltl::Elf<>>;

template <class Elf>
constexpr Elf::size_type kImageAddr = 0x1234000;

template <class Elf>
constexpr Elf::Addr kImageData[] = {1, 2, 3, 4};

template <class Elf>
constexpr std::span kImage(kImageData<Elf>);

template <class Elf>
const auto kImageBytes = std::as_bytes(kImage<Elf>);

template <typename Dyn, size_t N>
constexpr std::span<const Dyn> DynSpan(const std::array<Dyn, N>& dyn) {
  return {dyn};
}

constexpr elfldltl::DiagnosticsFlags kDiagFlags = {.multiple_errors = true};

FORMAT_TYPED_TEST_SUITE(ElfldltlInitFiniTests);

TYPED_TEST(ElfldltlInitFiniTests, Empty) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(0u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(0u, errors.size());

  EXPECT_EQ(0u, info.size());
  EXPECT_TRUE(info.empty());
}

TYPED_TEST(ElfldltlInitFiniTests, ArrayOnly) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      Dyn{.tag = elfldltl::ElfDynTag::kInitArray, .val = kImageAddr<Elf>},
      Dyn{.tag = elfldltl::ElfDynTag::kInitArraySz, .val = kImageBytes<Elf>.size()},
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(0u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(0u, errors.size());

  EXPECT_EQ(4u, info.size());
}

TYPED_TEST(ElfldltlInitFiniTests, LegacyOnly) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      Dyn{.tag = elfldltl::ElfDynTag::kInit, .val = 0x5678},
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(0u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(0u, errors.size());

  EXPECT_EQ(1u, info.size());
  EXPECT_EQ(0x5678u, info.legacy());
}

TYPED_TEST(ElfldltlInitFiniTests, ArrayWithLegacy) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      Dyn{.tag = elfldltl::ElfDynTag::kInit, .val = 0x5678},
      Dyn{.tag = elfldltl::ElfDynTag::kInitArray, .val = kImageAddr<Elf>},
      Dyn{.tag = elfldltl::ElfDynTag::kInitArraySz, .val = kImageBytes<Elf>.size()},
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(0u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(0u, errors.size());

  EXPECT_EQ(5u, info.size());
}

TYPED_TEST(ElfldltlInitFiniTests, MissingArray) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      // DT_INIT_ARRAY missing with DT_INIT_ARRAYSZ present.
      Dyn{.tag = elfldltl::ElfDynTag::kInitArraySz, .val = kImageBytes<Elf>.size()},
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(1u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(1u, errors.size());

  EXPECT_EQ(0u, info.size());
}

TYPED_TEST(ElfldltlInitFiniTests, MissingSize) {
  using Elf = TestFixture::Elf;
  using Dyn = Elf::Dyn;

  std::vector<std::string> errors;
  auto diag = elfldltl::CollectStringsDiagnostics(errors, kDiagFlags);
  elfldltl::DirectMemory memory{
      {
          const_cast<std::byte*>(kImageBytes<Elf>.data()),
          kImageBytes<Elf>.size(),
      },
      kImageAddr<Elf>,
  };

  constexpr std::array dyn{
      Dyn{.tag = elfldltl::ElfDynTag::kInitArray, .val = kImageAddr<Elf>},
      // DT_INIT_ARRAYSZ missing with DT_INIT_ARRAY present.
      Dyn{.tag = elfldltl::ElfDynTag::kNull},
  };

  elfldltl::InitFiniInfo<Elf> info;
  EXPECT_TRUE(
      elfldltl::DecodeDynamic(diag, memory, DynSpan(dyn), elfldltl::DynamicInitObserver(info)));

  EXPECT_EQ(1u, diag.errors());
  EXPECT_EQ(0u, diag.warnings());
  EXPECT_EQ(1u, errors.size());

  EXPECT_EQ(0u, info.size());
}

TYPED_TEST(ElfldltlInitFiniTests, RawInitTests) {
  using Elf = TestFixture::Elf;
  using size_type = Elf::size_type;
  using RawVector = std::vector<std::pair<size_type, bool>>;

  constexpr typename Elf::Addr array[] = {2, 3, 4, 5};
  elfldltl::InitFiniInfo<Elf> info;
  info.set_array(std::span(array));
  info.set_legacy(1);

  ASSERT_EQ(5u, info.size());

  const RawVector relocated{std::from_range, info.raw_init(true)};
  constexpr typename RawVector::value_type kExpectedRelocated[] = {
      {1, false}, {2, true}, {3, true}, {4, true}, {5, true},
  };
  EXPECT_THAT(relocated, ::testing::ElementsAreArray(kExpectedRelocated));

  const RawVector unrelocated{std::from_range, info.raw_init(false)};
  constexpr typename RawVector::value_type kExpectedUnrelocated[] = {
      {1, false}, {2, false}, {3, false}, {4, false}, {5, false},
  };
  EXPECT_THAT(unrelocated, ::testing::ElementsAreArray(kExpectedUnrelocated));
}

TYPED_TEST(ElfldltlInitFiniTests, RawFiniTests) {
  using Elf = TestFixture::Elf;
  using size_type = Elf::size_type;
  using RawVector = std::vector<std::pair<size_type, bool>>;

  constexpr typename Elf::Addr array[] = {2, 3, 4, 5};
  elfldltl::InitFiniInfo<Elf> info;
  info.set_array(std::span(array));
  info.set_legacy(1);

  ASSERT_EQ(5u, info.size());

  const RawVector relocated{std::from_range, info.raw_fini(true)};
  constexpr typename RawVector::value_type kExpectedRelocated[] = {
      {5, true}, {4, true}, {3, true}, {2, true}, {1, false},
  };
  EXPECT_THAT(relocated, ::testing::ElementsAreArray(kExpectedRelocated));

  const RawVector unrelocated{std::from_range, info.raw_fini(false)};
  constexpr typename RawVector::value_type kExpectedUnrelocated[] = {
      {5, false}, {4, false}, {3, false}, {2, false}, {1, false},
  };
  EXPECT_THAT(unrelocated, ::testing::ElementsAreArray(kExpectedUnrelocated));
}

TYPED_TEST(ElfldltlInitFiniTests, InitTests) {
  using Elf = TestFixture::Elf;

  constexpr typename Elf::Addr array[] = {2, 3, 4, 5};
  elfldltl::InitFiniInfo<Elf> info;
  info.set_array(std::span(array));
  info.set_legacy(1);

  ASSERT_EQ(5u, info.size());

  const std::vector relocated{std::from_range, info.init(0x1000, true)};
  EXPECT_THAT(relocated, ::testing::ElementsAre(0x1001, 2, 3, 4, 5));

  const std::vector unrelocated{std::from_range, info.init(0x1000, false)};
  EXPECT_THAT(unrelocated, ::testing::ElementsAre(0x1001, 0x1002, 0x1003, 0x1004, 0x1005));
}

TYPED_TEST(ElfldltlInitFiniTests, FiniTests) {
  using Elf = TestFixture::Elf;

  constexpr typename Elf::Addr array[] = {2, 3, 4, 5};
  elfldltl::InitFiniInfo<Elf> info;
  info.set_array(std::span(array));
  info.set_legacy(1);

  ASSERT_EQ(5u, info.size());

  const std::vector relocated{std::from_range, info.fini(0x1000, true)};
  EXPECT_THAT(relocated, ::testing::ElementsAre(5, 4, 3, 2, 0x1001));

  const std::vector unrelocated{std::from_range, info.fini(0x1000, false)};
  EXPECT_THAT(unrelocated, ::testing::ElementsAre(0x1005, 0x1004, 0x1003, 0x1002, 0x1001));
}

TYPED_TEST(ElfldltlInitFiniTests, Remote) {
  using Elf = TestFixture::Elf;

  using RemoteInitFiniInfo = elfldltl::InitFiniInfo<Elf, elfldltl::RemoteAbiTraits>;

  RemoteInitFiniInfo info;
  info = RemoteInitFiniInfo(info);
}

template <typename T, size_t N>
auto ToAddrArray(T* (&&ptrs)[N]) {
  std::array<elfldltl::Elf<>::Addr, N> addrs;
  for (auto&& [addr, ptr] : std::views::zip(addrs, std::to_array<T*>(ptrs))) {
    addr = reinterpret_cast<uintptr_t>(ptr);
  }
  return addrs;
}

class ElfldltlInitFiniCallTests : public ::testing::Test {
 protected:
  struct Mock {
    MOCK_METHOD(void, Call, (int));
  };
  using Strict = ::testing::StrictMock<Mock>;

  // The tests using InitFiniFunction must use global state since the callees
  // are simple function pointers taking no arguments.

  void SetUp() override { ASSERT_EQ(mock_, nullptr); }

  void TearDown() override { mock_ = nullptr; }

  static void GlobalMock(Strict& mock) {
    ASSERT_EQ(mock_, nullptr);
    mock_ = &mock;
  }

  template <int I>
  static void CallGlobalMock() {
    mock_->Call(I);
  }

  static inline const auto gThreeCalls = ToAddrArray<void()>({
      &CallGlobalMock<1>,
      &CallGlobalMock<2>,
      &CallGlobalMock<3>,
  });

  template <int I>
  static void CallArgumentMock(Strict& mock) {
    mock.Call(I);
  }

  static inline const auto gThreeCallsWithArg = ToAddrArray<void(Strict&)>({
      &CallArgumentMock<1>,
      &CallArgumentMock<2>,
      &CallArgumentMock<3>,
  });

 private:
  ::testing::InSequence seq_;  // Just this existing makes mocks require order.
  static inline Mock* mock_;
};

TEST_F(ElfldltlInitFiniCallTests, CallInitNoLegacy) {
  Strict mock;
  GlobalMock(mock);
  EXPECT_CALL(mock, Call(1));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(3));

  NativeInfo info;
  info.set_array(gThreeCalls);

  info.callable_init_no_legacy()();
}

TEST_F(ElfldltlInitFiniCallTests, CallInitWithLegacy) {
  Strict mock;
  GlobalMock(mock);
  EXPECT_CALL(mock, Call(0));
  EXPECT_CALL(mock, Call(1));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(3));

  NativeInfo info;
  info.set_array(gThreeCalls);

  constexpr auto kRelocationAdjustment = kImageAddr<elfldltl::Elf<>>;

  info.set_legacy(reinterpret_cast<uintptr_t>(&CallGlobalMock<0>) - kRelocationAdjustment);

  info.callable_init(kRelocationAdjustment)();
}

TEST_F(ElfldltlInitFiniCallTests, CallFiniNoLegacy) {
  Strict mock;
  GlobalMock(mock);
  EXPECT_CALL(mock, Call(3));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(1));

  NativeInfo info;
  info.set_array(gThreeCalls);

  info.callable_fini_no_legacy()();
}

TEST_F(ElfldltlInitFiniCallTests, CallFiniWithLegacy) {
  Strict mock;
  GlobalMock(mock);
  EXPECT_CALL(mock, Call(3));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(1));
  EXPECT_CALL(mock, Call(0));

  NativeInfo info;
  info.set_array(gThreeCalls);

  constexpr auto kRelocationAdjustment = kImageAddr<elfldltl::Elf<>>;

  info.set_legacy(reinterpret_cast<uintptr_t>(&CallGlobalMock<0>) - kRelocationAdjustment);

  info.callable_fini(kRelocationAdjustment)();
}

TEST_F(ElfldltlInitFiniCallTests, CallInitWithArgs) {
  Strict mock;

  EXPECT_CALL(mock, Call(0));
  EXPECT_CALL(mock, Call(1));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(3));

  NativeInfo info;
  info.set_array(gThreeCallsWithArg);

  constexpr auto kRelocationAdjustment = kImageAddr<elfldltl::Elf<>>;

  info.set_legacy(reinterpret_cast<uintptr_t>(&CallArgumentMock<0>) - kRelocationAdjustment);

  info.callable_init<elfldltl::InitFiniFunctionWithArgs<Strict&>>(kRelocationAdjustment)(mock);
}

TEST_F(ElfldltlInitFiniCallTests, CallFiniWithArgs) {
  Strict mock;

  EXPECT_CALL(mock, Call(3));
  EXPECT_CALL(mock, Call(2));
  EXPECT_CALL(mock, Call(1));
  EXPECT_CALL(mock, Call(0));

  NativeInfo info;
  info.set_array(gThreeCallsWithArg);

  constexpr auto kRelocationAdjustment = kImageAddr<elfldltl::Elf<>>;

  info.set_legacy(reinterpret_cast<uintptr_t>(&CallArgumentMock<0>) - kRelocationAdjustment);

  info.callable_fini<elfldltl::InitFiniFunctionWithArgs<Strict&>>(kRelocationAdjustment)(mock);
}

}  // namespace
