// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "symbol-tests.h"

#include <lib/elfldltl/testing/typed-test.h>

#include <set>

#include <gmock/gmock.h>

namespace {

using namespace std::literals;

using ::testing::UnorderedElementsAreArray;

constexpr std::string_view kEmpty{};
constexpr elfldltl::SymbolName kEmptySymbol(kEmpty);
constexpr uint32_t kEmptyCompatHash = 0;
constexpr uint32_t kEmptyGnuHash = 5381;

constexpr uint32_t kFoobarCompatHash = 0x06d65882;
constexpr uint32_t kFoobarGnuHash = 0xfde460be;

struct NotConstructibleFromSpan {
  NotConstructibleFromSpan() = delete;
  static constexpr bool Valid(std::span<const uint32_t>) { return false; }
  uint32_t symtab_size() const;
  std::span<const uint32_t> Bucket(uint32_t) const;
  std::span<const std::span<const uint32_t>> AllBuckets() const;
};
static_assert(!elfldltl::SymbolHashTableApi<NotConstructibleFromSpan, uint32_t>);

FORMAT_TYPED_TEST_SUITE(ElfldltlSymbolTests);

TEST(ElfldltlSymbolTests, CompatHash) {
  EXPECT_EQ(kEmptyCompatHash, elfldltl::SymbolName(kEmpty).compat_hash());
  EXPECT_EQ(kFoobarCompatHash, elfldltl::SymbolName(kFoobar).compat_hash());
}

static_assert(kEmptySymbol.compat_hash() == kEmptyCompatHash);
static_assert(kFoobarSymbol.compat_hash() == kFoobarCompatHash);

TEST(ElfldltlSymbolTests, GnuHash) {
  EXPECT_EQ(kEmptyGnuHash, elfldltl::SymbolName(kEmpty).gnu_hash());
  EXPECT_EQ(kFoobarGnuHash, elfldltl::SymbolName(kFoobar).gnu_hash());
}

static_assert(kEmptySymbol.gnu_hash() == kEmptyGnuHash);
static_assert(kFoobarSymbol.gnu_hash() == kFoobarGnuHash);

TYPED_TEST(ElfldltlSymbolTests, CompatHashSize) {
  using Elf = typename TestFixture::Elf;

  elfldltl::SymbolInfo<Elf> si;
  kTestSymbols<Elf>.SetInfo(si);
  si.set_compat_hash(kTestCompatHash<typename Elf::Word>);

  EXPECT_EQ(si.safe_symtab().size(), kTestSymbolCount);
}

TYPED_TEST(ElfldltlSymbolTests, GnuHashSize) {
  using Elf = typename TestFixture::Elf;

  elfldltl::SymbolInfo<Elf> si;
  kTestSymbols<Elf>.SetInfo(si);
  si.set_gnu_hash(kTestGnuHash<typename Elf::Addr>);

  EXPECT_EQ(si.safe_symtab().size(), kTestSymbolCount);
}

TYPED_TEST(ElfldltlSymbolTests, LookupCompatHash) {
  using Elf = typename TestFixture::Elf;

  elfldltl::SymbolInfo<Elf> si;
  kTestSymbols<Elf>.SetInfo(si);
  si.set_compat_hash(kTestCompatHash<typename Elf::Word>);

  EXPECT_EQ(kNotFoundSymbol.Lookup(si), nullptr);

  EXPECT_EQ(kQuuxSymbol.Lookup(si), nullptr);  // Undefined should be skipped.

  const auto* foo = kFooSymbol.Lookup(si);
  ASSERT_NE(foo, nullptr);
  EXPECT_EQ(foo->value(), kFooAddress);

  const auto* bar = kBarSymbol.Lookup(si);
  ASSERT_NE(bar, nullptr);
  EXPECT_EQ(bar->value(), kBarAddress);

  const auto* foobar = kFoobarSymbol.Lookup(si);
  ASSERT_NE(foobar, nullptr);
  EXPECT_EQ(foobar->value(), kFoobarAddress);
}

TYPED_TEST(ElfldltlSymbolTests, LookupGnuHash) {
  using Elf = typename TestFixture::Elf;

  elfldltl::SymbolInfo<Elf> si;
  kTestSymbols<Elf>.SetInfo(si);
  si.set_gnu_hash(kTestGnuHash<typename Elf::Addr>);

  EXPECT_EQ(kNotFoundSymbol.Lookup(si), nullptr);

  EXPECT_EQ(kQuuxSymbol.Lookup(si), nullptr);  // Undefined should be skipped.

  const auto* foo = kFooSymbol.Lookup(si);
  ASSERT_NE(foo, nullptr);
  EXPECT_EQ(foo->value(), kFooAddress);

  const auto* bar = kBarSymbol.Lookup(si);
  ASSERT_NE(bar, nullptr);
  EXPECT_EQ(bar->value(), kBarAddress);

  const auto* foobar = kFoobarSymbol.Lookup(si);
  ASSERT_NE(foobar, nullptr);
  EXPECT_EQ(foobar->value(), kFoobarAddress);
}

// The enumeration tests use the same symbol table with both flavors of hash
// table.

template <class Elf>
struct CompatHash {
  using Table = typename elfldltl::CompatHash<Elf>;
  static constexpr void SetInfo(elfldltl::SymbolInfo<Elf>& si) {
    si.set_compat_hash(kTestCompatHash<typename Elf::Word>);
  }
  static constexpr Table Get(const elfldltl::SymbolInfo<Elf>& si) { return *si.compat_hash(); }
  static constexpr std::string_view kNames[] = {
      "bar",
      "foo",
      "foobar",
      "quux",
  };
};

template <class Elf>
struct GnuHash {
  using Table = typename elfldltl::GnuHash<Elf>;
  static constexpr void SetInfo(elfldltl::SymbolInfo<Elf>& si) {
    si.set_gnu_hash(kTestGnuHash<typename Elf::Addr>);
  }
  static constexpr Table Get(const elfldltl::SymbolInfo<Elf>& si) { return *si.gnu_hash(); }
  static constexpr std::string_view kNames[] = {
      // The DT_GNU_HASH table omits the undefined symbols.
      "bar",
      "foo",
      "foobar",
  };
};

template <class Elf, template <class ElfLayout> class HashTable>
void EnumerateHashTable() {
  using Sym = Elf::Sym;

  elfldltl::SymbolInfo<Elf> si;
  kTestSymbols<Elf>.SetInfo(si);
  HashTable<Elf>::SetInfo(si);
  const auto hash_table = HashTable<Elf>::Get(si);

  // Collect all the symbols in a set that doesn't remove duplicates.
  std::multiset<std::string_view> symbol_names;
  for (const Sym& sym : si.HashedSymbols(hash_table)) {
    std::string_view name = si.string(sym.name);
    ASSERT_FALSE(name.empty());
    symbol_names.insert(name);
  }

  EXPECT_THAT(symbol_names, UnorderedElementsAreArray(HashTable<Elf>::kNames));
}

TYPED_TEST(ElfldltlSymbolTests, EnumerateCompatHash) {
  EnumerateHashTable<typename TestFixture::Elf, CompatHash>();
}

TYPED_TEST(ElfldltlSymbolTests, EnumerateGnuHash) {
  EnumerateHashTable<typename TestFixture::Elf, GnuHash>();
}

TYPED_TEST(ElfldltlSymbolTests, OnSymbols) {
  using Elf = TestFixture::Elf;
  using SymbolInfo = elfldltl::SymbolInfo<Elf>;
  using Sym = Elf::Sym;

  {
    SymbolInfo si;
    kTestSymbols<Elf>.SetInfo(si);
    si.set_compat_hash(kTestCompatHash<typename Elf::Word>);
    std::multiset<std::string_view> symbol_names;
    EXPECT_TRUE(si.OnSymbols([&si, &symbol_names](const Sym& sym) {
      std::string_view name = si.string(sym.name);
      EXPECT_FALSE(name.empty());
      symbol_names.insert(name);
      return true;
    }));
    EXPECT_THAT(symbol_names, UnorderedElementsAreArray(CompatHash<Elf>::kNames))
        << "symbols from DT_HASH";
  }

  {
    SymbolInfo si;
    kTestSymbols<Elf>.SetInfo(si);
    si.set_gnu_hash(kTestGnuHash<typename Elf::Addr>);
    std::multiset<std::string_view> symbol_names;
    EXPECT_TRUE(si.OnSymbols([&si, &symbol_names](const Sym& sym) {
      std::string_view name = si.string(sym.name);
      EXPECT_FALSE(name.empty());
      symbol_names.insert(name);
      return true;
    }));
    EXPECT_THAT(symbol_names, UnorderedElementsAreArray(GnuHash<Elf>::kNames))
        << "symbols from DT_GNU_HASH";
    ;
  }
}

template <class Elf, template <class ElfLayout> class HashTable>
void LookupVaddrTest() {
  using SymbolInfo = elfldltl::SymbolInfo<Elf>;
  using Sym = Elf::Sym;

  SymbolInfo si;
  kTestSymbols<Elf>.SetInfo(si);
  HashTable<Elf>::SetInfo(si);

  auto sym_name = [&si](const Sym* sym) -> std::string_view {
    if (!sym) {
      return "<no symbol found>"sv;
    };
    std::string_view name = si.string(sym->name);
    if (name.empty()) {
      return "<invalid st_name>"sv;
    }
    return name;
  };

  // This is below the lowest symbol address.
  EXPECT_EQ(sym_name(si.LookupVaddr(0x050)), "<no symbol found>"sv);

  // This is exactly the quux symbol, and not in the range of any other symbol.
  // Even though it's an SHN_UNDEF symbol, it's got a nonzero value/size and so
  // could be a PLT entry and thus will be considered--but only in DT_HASH!  In
  // DT_GNU_HASH, _all_ SHN_UNDEF symbols are omitted.  Since the hash table is
  // the only (safe and efficient) means of enumerating the whole symbol table,
  // the PLT entries won't be seen.
  if constexpr (std::same_as<HashTable<Elf>, GnuHash<Elf>>) {
    EXPECT_EQ(sym_name(si.LookupVaddr(kQuuxAddress)), "<no symbol found>"sv);
  } else {
    EXPECT_EQ(sym_name(si.LookupVaddr(kQuuxAddress)), "quux"sv);
  }

  // Somewhere in the middle of the foo symbol's size range.
  EXPECT_EQ(sym_name(si.LookupVaddr(kFooAddress + 0x10)), "foo"sv);

  // The foobar symbol size range is wholly inside the bar symbol's size range,
  // but an exact match will prefer it.
  EXPECT_EQ(sym_name(si.LookupVaddr(kFoobarAddress)), "foobar"sv);
}

TYPED_TEST(ElfldltlSymbolTests, LookupVaddrCompatHash) {
  LookupVaddrTest<typename TestFixture::Elf, CompatHash>();
}

TYPED_TEST(ElfldltlSymbolTests, LookupVaddrGnuHash) {
  LookupVaddrTest<typename TestFixture::Elf, GnuHash>();
}

TYPED_TEST(ElfldltlSymbolTests, SymbolInfoForSingleLookup) {
  using Elf = typename TestFixture::Elf;

  constexpr static elfldltl::SymbolInfoForSingleLookup<Elf> si{"sym"};

  elfldltl::SymbolName name{si, si.symbol()};
  EXPECT_EQ(name, "sym");
}

TYPED_TEST(ElfldltlSymbolTests, Remote) {
  using Elf = typename TestFixture::Elf;

  using RemoteSymbolInfo = elfldltl::SymbolInfo<Elf, elfldltl::RemoteAbiTraits>;

  RemoteSymbolInfo si;
  si = RemoteSymbolInfo(si);
}

#ifdef __APPLE__
#define SECTION_NAME "__DATA,__bss"
#else
#define SECTION_NAME ".bss"
#endif

TYPED_TEST(ElfldltlSymbolTests, ZeroInitialized) {
  using Elf = typename TestFixture::Elf;

  // Test that this object can be zero initialized by putting it in .bss
  [[gnu::section(SECTION_NAME)]] static elfldltl::SymbolInfo<Elf> foo{
      elfldltl::kLinkerZeroInitialized};
  foo.InitLinkerZeroInitialized();

  EXPECT_EQ(foo.strtab(), "\0"sv);
}

template <typename Addr>
constexpr void AppendGnuHashWords(std::vector<Addr>& out, std::span<const uint32_t> words) {
  if constexpr (sizeof(Addr) == sizeof(uint32_t)) {
    for (uint32_t w : words) {
      out.push_back(w);
    }
  } else {
    for (size_t i = 0; i < words.size(); i += 2) {
      uint32_t second = i + 1 < words.size() ? words[i + 1] : 0;
      out.push_back(WordPair<Addr>(words[i], second));
    }
  }
}

TYPED_TEST(ElfldltlSymbolTests, GnuHashMultiBucketAndTrailingData) {
  using Elf = TestFixture::Elf;
  using Addr = Elf::Addr;
  using Sym = Elf::Sym;
  using size_type = Elf::size_type;

  // Two buckets: "a" (even hash) in bucket 0 at symndx 2, "b" (odd hash) and
  // "d" (odd hash) in bucket 1 at symndx 3..4, so max_symndx (3) > bias (2)
  // and total 32-bit words in buckets + chain is odd (2 + 3 = 5).
  constexpr elfldltl::SymbolName kSymA("a"sv);
  constexpr elfldltl::SymbolName kSymB("b"sv);
  constexpr elfldltl::SymbolName kSymD("d"sv);
  static_assert(kSymA.gnu_hash() % 2 == 0);
  static_assert(kSymB.gnu_hash() % 2 == 1);
  static_assert(kSymD.gnu_hash() % 2 == 1);

  TestSymtab<Elf> symtab;
  symtab.AddSymbol("undef"sv, 0, 0, elfldltl::ElfSymBind::kGlobal, elfldltl::ElfSymType::kFunc, 0)
      .AddSymbol(kSymA, 0x100, 0x10, elfldltl::ElfSymBind::kGlobal, elfldltl::ElfSymType::kFunc, 1)
      .AddSymbol(kSymB, 0x200, 0x10, elfldltl::ElfSymBind::kGlobal, elfldltl::ElfSymType::kFunc, 1)
      .AddSymbol(kSymD, 0x300, 0x10, elfldltl::ElfSymBind::kGlobal, elfldltl::ElfSymType::kFunc, 1);

  // Add trailing garbage symbols to simulate an unbounded DT_SYMTAB read.
  std::vector<Sym> raw_syms(symtab.symtab().begin(), symtab.symtab().end());
  raw_syms.resize(raw_syms.size() + 4);

  std::vector<Addr> raw_gnu_hash;
  const uint32_t header[] = {2, 2, 1, 5};  // nbucket=2, bias=2, nfilter=1, shift=5
  AppendGnuHashWords(raw_gnu_hash, header);
  raw_gnu_hash.push_back(~size_type{});  // All Bloom filter bits set.
  const uint32_t buckets_and_chains[] = {
      2,                       // bucket 0 -> symndx 2 ("a")
      3,                       // bucket 1 -> symndx 3 ("b", "d")
      kSymA.gnu_hash() | 1u,   // symndx 2: end of bucket 0
      kSymB.gnu_hash() & ~1u,  // symndx 3: continues bucket 1
      kSymD.gnu_hash() | 1u,   // symndx 4: end of bucket 1
      0xdeadbeef,              // Trailing RODATA after .gnu.hash
      0xcafebabe,
      0xfeedface,
  };
  AppendGnuHashWords(raw_gnu_hash, buckets_and_chains);

  elfldltl::SymbolInfo<Elf> si;
  si.set_symtab(raw_syms);
  si.set_strtab(symtab.strtab());
  si.set_gnu_hash(raw_gnu_hash);

  const auto gnu_hash = si.gnu_hash();
  ASSERT_TRUE(gnu_hash.has_value());
  EXPECT_EQ(gnu_hash->symtab_size(), 5u);
  EXPECT_EQ(si.safe_symtab().size(), 5u);

  ASSERT_NE(kSymA.Lookup(si), nullptr);
  EXPECT_EQ(kSymA.Lookup(si)->value(), 0x100u);
  ASSERT_NE(kSymB.Lookup(si), nullptr);
  EXPECT_EQ(kSymB.Lookup(si)->value(), 0x200u);
  ASSERT_NE(kSymD.Lookup(si), nullptr);
  EXPECT_EQ(kSymD.Lookup(si)->value(), 0x300u);
  EXPECT_EQ(kNotFoundSymbol.Lookup(si), nullptr);

  std::multiset<std::string_view> enumerated;
  for (const auto& sym : si.HashedSymbols(*gnu_hash)) {
    enumerated.insert(si.string(sym.name));
  }
  EXPECT_THAT(enumerated, ::testing::UnorderedElementsAre("a"sv, "b"sv, "d"sv));
}

TYPED_TEST(ElfldltlSymbolTests, GnuHashCollisionsAndEmptyBuckets) {
  using Elf = TestFixture::Elf;
  using Addr = Elf::Addr;
  using size_type = Elf::size_type;

  // Test 3 consecutive identical hashes in one bucket followed by another
  // bucket with the same hash value, ensuring 32-bit and 64-bit TableIterator
  // neither duplicate non-terminal collision entries nor skip across a bucket
  // end marker.
  constexpr uint32_t kHash = 0x12345678;
  std::vector<Addr> collision_table;
  const uint32_t header[] = {2, 1, 1, 5};  // nbucket=2, bias=1, nfilter=1, shift=5
  AppendGnuHashWords(collision_table, header);
  collision_table.push_back(~size_type{});
  const uint32_t body[] = {
      1,            // bucket 0 -> symndx 1..3
      4,            // bucket 1 -> symndx 4
      kHash & ~1u,  // symndx 1
      kHash & ~1u,  // symndx 2
      kHash | 1u,   // symndx 3 (end of bucket 0)
      kHash | 1u,   // symndx 4 (end of bucket 1)
  };
  AppendGnuHashWords(collision_table, body);

  ASSERT_TRUE(elfldltl::GnuHash<Elf>::Valid(collision_table));
  elfldltl::GnuHash<Elf> hash_table(collision_table);
  EXPECT_EQ(hash_table.symtab_size(), 5u);

  std::vector<uint32_t> indices;
  for (auto bucket : hash_table.AllBuckets()) {
    for (uint32_t symndx : bucket) {
      indices.push_back(symndx);
    }
  }
  EXPECT_THAT(indices, ::testing::ElementsAre(1u, 2u, 3u, 4u));

  // All-zero buckets with zero chain words (no exported defined symbols).
  std::vector<Addr> empty_table;
  const uint32_t empty_header[] = {2, 3, 1, 5};  // nbucket=2, bias=3, nfilter=1
  AppendGnuHashWords(empty_table, empty_header);
  empty_table.push_back(Addr{0});
  const uint32_t empty_buckets[] = {0, 0};
  AppendGnuHashWords(empty_table, empty_buckets);

  ASSERT_TRUE(elfldltl::GnuHash<Elf>::Valid(empty_table));
  elfldltl::GnuHash<Elf> empty_gnu_hash(empty_table);
  EXPECT_EQ(empty_gnu_hash.symtab_size(), 3u);
  EXPECT_TRUE(std::ranges::empty(empty_gnu_hash.AllBuckets()));
}

TYPED_TEST(ElfldltlSymbolTests, GnuHashOddWordCounts) {
  using Elf = TestFixture::Elf;
  using Addr = Elf::Addr;
  using size_type = Elf::size_type;

  constexpr uint32_t kHash = 0x12345678;

  // Odd nbucket (1) with empty chain (1 uint32_t zero-padded to Addr).
  {
    std::vector<Addr> table;
    const uint32_t header[] = {1, 2, 1, 5};
    AppendGnuHashWords(table, header);
    table.push_back(Addr{0});
    const uint32_t body[] = {0};
    AppendGnuHashWords(table, body);

    ASSERT_TRUE(elfldltl::GnuHash<Elf>::Valid(table));
    elfldltl::GnuHash<Elf> gnu_hash(table);
    EXPECT_EQ(gnu_hash.symtab_size(), 2u);
    EXPECT_TRUE(std::ranges::empty(gnu_hash.AllBuckets()));
    EXPECT_TRUE(std::ranges::empty(gnu_hash.Bucket(kHash)));
  }

  // Odd nbucket (1) with 2 chain entries (3 uint32_t words total).
  {
    std::vector<Addr> table;
    const uint32_t header[] = {1, 1, 1, 5};
    AppendGnuHashWords(table, header);
    table.push_back(~size_type{});
    const uint32_t body[] = {
        1,            // bucket 0 -> symndx 1..2
        kHash & ~1u,  // symndx 1
        kHash | 1u,   // symndx 2 (end of bucket 0)
    };
    AppendGnuHashWords(table, body);

    ASSERT_TRUE(elfldltl::GnuHash<Elf>::Valid(table));
    elfldltl::GnuHash<Elf> gnu_hash(table);
    EXPECT_EQ(gnu_hash.symtab_size(), 3u);

    std::vector<uint32_t> indices;
    for (auto bucket : gnu_hash.AllBuckets()) {
      for (uint32_t symndx : bucket) {
        indices.push_back(symndx);
      }
    }
    EXPECT_THAT(indices, ::testing::ElementsAre(1u, 2u));

    std::vector<uint32_t> bucket_indices;
    for (uint32_t symndx : gnu_hash.Bucket(kHash)) {
      bucket_indices.push_back(symndx);
    }
    EXPECT_THAT(bucket_indices, ::testing::ElementsAre(1u, 2u));
  }

  // Even nbucket (2) with 3 chain entries (5 uint32_t words total, no trailing
  // RODATA).
  {
    std::vector<Addr> table;
    const uint32_t header[] = {2, 1, 1, 5};
    AppendGnuHashWords(table, header);
    table.push_back(~size_type{});
    const uint32_t body[] = {
        1,            // bucket 0 -> symndx 1..2
        3,            // bucket 1 -> symndx 3
        kHash & ~1u,  // symndx 1
        kHash | 1u,   // symndx 2 (end of bucket 0)
        kHash | 1u,   // symndx 3 (end of bucket 1)
    };
    AppendGnuHashWords(table, body);

    ASSERT_TRUE(elfldltl::GnuHash<Elf>::Valid(table));
    elfldltl::GnuHash<Elf> gnu_hash(table);
    EXPECT_EQ(gnu_hash.symtab_size(), 4u);

    std::vector<uint32_t> indices;
    for (auto bucket : gnu_hash.AllBuckets()) {
      for (uint32_t symndx : bucket) {
        indices.push_back(symndx);
      }
    }
    EXPECT_THAT(indices, ::testing::ElementsAre(1u, 2u, 3u));
  }
}

}  // namespace
