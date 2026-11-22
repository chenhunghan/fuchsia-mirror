// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/elfldltl/diagnostics.h>
#include <lib/elfldltl/perfect-symbol-table.h>
#include <lib/elfldltl/testing/diagnostics.h>
#include <lib/elfldltl/testing/typed-test.h>

#include <ranges>
#include <utility>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

namespace {

using ::testing::Pair;
using ::testing::UnorderedElementsAre;

constexpr auto kFooSymbols = elfldltl::PerfectSymbolTable({
    "foo",
    "bar",
});

// Both "a" (177670) and "c" (177672) have even GNU hashes, so N=2 collides
// while N=3 has no collisions (TableSize=3, Overhead=1, OverheadRatio=0.5).
constexpr auto kOverheadSymbols = elfldltl::PerfectSymbolTable({
    "a",
    "c",
});
using OverheadSet = elfldltl::PerfectSymbolSet<kOverheadSymbols>;
static_assert(OverheadSet::TableSize() == 3);
static_assert(OverheadSet::Overhead() == 1);
static_assert(OverheadSet::OverheadRatio() == 0.5);

TEST(ElfldltlPerfectSymbolTableTests, PerfectSymbolMap) {
  // The gmock container matchers don't like plain ranges, so copy the map's
  // contents results into a real container for checking.
  using MapContents = std::vector<std::pair<elfldltl::SymbolName, char>>;

  elfldltl::PerfectSymbolMap<char, kFooSymbols> fronts;
  auto all_fronts = fronts.Enumerate();
  for (auto [s, c] : all_fronts) {
    static_assert(std::is_same_v<decltype(c), char&>);
    c = s.front();
  }

  EXPECT_EQ(fronts["foo"], 'f');
  EXPECT_EQ(fronts["bar"], 'b');
  EXPECT_EQ(fronts["baz"], '\0');

  const auto& const_fronts = fronts;
  auto const_all_fronts = const_fronts.Enumerate();
  for (auto [s, c] : const_all_fronts) {
    static_assert(std::is_same_v<decltype(c), const char&>);
    EXPECT_EQ(c, s.front());
  }

  EXPECT_THAT(MapContents(const_all_fronts.begin(), const_all_fronts.end()),
              UnorderedElementsAre(Pair("foo", 'f'), Pair("bar", 'b')));
}

FORMAT_TYPED_TEST_SUITE(ElfldltlPerfectSymbolFilterTests);

template <class Sym>
struct FakeModule {
  const Sym* a = nullptr;
  const Sym* c = nullptr;

  fit::result<bool, const Sym*> Lookup(auto& diag, elfldltl::SymbolName& name) const {
    if (name == "a") {
      return fit::ok(a);
    }
    if (name == "c") {
      return fit::ok(c);
    }
    return fit::ok(nullptr);
  }
};

TYPED_TEST(ElfldltlPerfectSymbolFilterTests, Filter) {
  using Elf = TestFixture::Elf;
  using Sym = Elf::Sym;
  using Filter = elfldltl::PerfectSymbolFilter<kOverheadSymbols, Elf, 0.5>;
  using Map = elfldltl::PerfectSymbolMap<const Sym*, kOverheadSymbols, 0.5>;
  static_assert(sizeof(Filter) == sizeof(Map));

  constexpr Sym kSymA{.value = 1};
  constexpr Sym kSymC{.value = 2};

  {
    auto diag = elfldltl::testing::ExpectOkDiagnostics();
    FakeModule<Sym> module{.a = &kSymA, .c = &kSymC};
    Filter filter;
    ASSERT_TRUE(filter.Init(diag, module));
    EXPECT_EQ(filter("a"), &kSymA);
    EXPECT_EQ(filter("c"), &kSymC);
    EXPECT_EQ(filter("other"), nullptr);

    elfldltl::SymbolName name_a{"a"};
    EXPECT_EQ(filter(name_a), &kSymA);
    auto res = filter(diag, name_a);
    ASSERT_TRUE(res.is_ok());
    EXPECT_EQ(*res, &kSymA);
  }

  {
    auto diag = elfldltl::testing::ExpectOkDiagnostics();
    FakeModule<Sym> partial_module{.a = &kSymA};
    Filter filter;
    ASSERT_TRUE(filter.Init(diag, partial_module, /*undef_ok=*/true));
    EXPECT_EQ(filter("a"), &kSymA);
    EXPECT_EQ(filter("c"), nullptr);
  }

  {
    elfldltl::testing::ExpectedSingleError expected{"undefined symbol: ", "c"};
    FakeModule<Sym> partial_module{.a = &kSymA};
    Filter filter;
    EXPECT_TRUE(filter.Init(expected, partial_module, /*undef_ok=*/false));
  }
}

}  // namespace
