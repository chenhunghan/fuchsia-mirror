// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_SHDR_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_SHDR_H_

#include <concepts>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <ranges>
#include <span>
#include <string_view>
#include <utility>

#include "diagnostics.h"
#include "load.h"

namespace elfldltl {

// These functions for reading section headers from the file are modelled on
// the <lib/elfldltl/load.h> functions for reading program headers.  Reading
// section headers is usually only done by offline tools, whereas the
// <lib/elfldltl/load.h> API is meant to support the critical runtime cases.

// This is used as a template template parameter in load.h (which see).
template <ElfApi Elf>
using ShdrFromElf = Elf::Shdr;

// Read section headers from an ELF file using the File API (see memory.h),
// given its file header (Ehdr) already read and validated.
//
// This takes a Diagnostics object (see diagnostics.h) and a File object (see
// memory.h) by reference.  It ultimately calls `file.ReadArrayFromFile<Shdr>`
// and returns that value if all goes well.  The Allocator argument is just
// forwarded to File::ReadArrayFromFile; see memory.h for API details.
//
// Per the ELF gABI, an empty section header table is represented by both
// e_shnum and e_shoff being zero, while e_shnum == 0 with a nonzero e_shoff
// indicates an extended section count (>= SHN_LORESERVE) stored in
// shdr[0].size.
//
// All the template parameters are normally deduced, including the Elf layout
// to use based on the Ehdr type used for the last argument.  This is a
// captureless lambda rather than a template function so that it can be used as
// a non-type template parameter to WithLoadHeadersFromFile (see below).
constexpr auto ReadShdrsFromFile =
    []<class Diagnostics, class Ehdr,
       ReadArrayFromFileAllocator<typename Ehdr::ElfLayout::Shdr, uint32_t> Allocator>(
        Diagnostics& diagnostics,
        CanReadArrayFromFile<typename Ehdr::ElfLayout::Shdr, Allocator,
                             typename Ehdr::ElfLayout::size_type> auto& file,
        Allocator&& allocator, const Ehdr& ehdr) {
      using namespace std::literals::string_view_literals;

      using Elf = Ehdr::ElfLayout;
      using size_type = Elf::size_type;
      using Shdr = Elf::Shdr;

      const size_type shoff = ehdr.shoff;
      auto read_shdrs = [&](size_t shnum) {
        auto result = file.template ReadArrayFromFile<Shdr>(  //
            shoff, std::forward<Allocator>(allocator), shnum);
        if (!result) {
          diagnostics.FormatError("cannot read section headers from ELF file"sv, FileOffset{shoff});
        }
        return result;
      };
      using Result = std::decay_t<decltype(read_shdrs(0).value())>;
      using OptionalResult = std::optional<Result>;
      static_assert(std::same_as<OptionalResult, decltype(read_shdrs(0))>);

      // An empty shdr table (both e_shnum and e_shoff zero) is not an error,
      // just an empty result.
      if (ehdr.shnum == 0 && shoff == 0) {
        return OptionalResult(Result());
      }

      // Validate the Ehdr fields related to the shdrs.

      if (ehdr.shentsize != sizeof(Shdr)) [[unlikely]] {
        diagnostics.FormatError("e_shentsize has unexpected value"sv, ehdr.shentsize());
        return OptionalResult();
      }

      if (shoff < sizeof(Ehdr)) [[unlikely]] {
        diagnostics.FormatError("e_shoff overlaps with ELF file header"sv, FileOffset{shoff});
        return OptionalResult();
      }

      if (shoff % alignof(Shdr) != 0) [[unlikely]] {
        diagnostics.FormatError("e_shoff has insufficient alignment"sv, FileOffset{shoff});
        return OptionalResult();
      }

      // Things look valid, so find the count.  Usually it's directly in the field.
      if (ehdr.shnum != 0) [[likely]] {
        return read_shdrs(ehdr.shnum);
      }

      // When e_shnum is 0 with a nonzero e_shoff, the count >= SHN_LORESERVE
      // (0xff00) is stored in the special stub section header at index 0.
      if (auto read_shdr = file.template ReadFromFile<Shdr>(shoff)) [[likely]] {
        const Shdr& shdr = *read_shdr;
        if (shdr.size == 0) [[unlikely]] {
          return OptionalResult(Result());
        }
        if (shdr.size() > std::numeric_limits<size_t>::max()) [[unlikely]] {
          diagnostics.FormatError("cannot read section headers from ELF file"sv, FileOffset{shoff});
          return OptionalResult();
        }
        return read_shdrs(static_cast<size_t>(shdr.size()));
      }

      diagnostics.FormatError("cannot read section header 0 from ELF file"sv, FileOffset{shoff});
      return OptionalResult();
    };

// Read the ELF file header (Ehdr) and section headers (Shdr) from an ELF file
// using the File API (see memory.h), and validate for this Elf layout and
// optionally for a specific machine.  Unlike LoadHeadersFromFile (see load.h),
// this checks Ehdr::Valid rather than Ehdr::Loadable (so ET_REL, ET_CORE, and
// ET_EXEC files are accepted as well as ET_DYN) and defaults `machine` to
// std::nullopt to accept any architecture.
//
// This just combines LoadEhdrFromFile (see load.h) and ReadShdrsFromFile.
// It returns std::optional<std::pair<E, S>> where LoadEhdrFromFile returns
// std::optional<E> and ReadShdrsFromFile returns std::optional<S>.  e.g.,
// ```
//   auto headers = elfldltl::LoadShdrsFromFile<elfldltl::Elf<>>(...);
//   if (headers) {
//     auto [ehdr_owner, shdrs_owner] = *headers;
//     const Ehdr& ehdr = ehdr_owner;
//     const std::span<const Shdr> shdrs = shdrs_owner;
//     ...
//   }
// ```
template <ElfApi Elf, class Diagnostics, class File,
          ReadArrayFromFileAllocator<typename Elf::Shdr, uint32_t> ShdrAllocator>
constexpr auto LoadShdrsFromFile(Diagnostics& diagnostics, File& file,
                                 ShdrAllocator&& shdr_allocator,
                                 std::optional<ElfMachine> machine = std::nullopt)
    -> decltype(std::make_optional(std::make_pair(
        *file.template ReadFromFile<typename Elf::Ehdr>(0),
        *file.template ReadArrayFromFile<typename Elf::Shdr>(0, shdr_allocator, 0)))) {
  if (auto read_ehdr = LoadEhdrFromFile<Elf>(diagnostics, file, machine, false)) [[likely]] {
    const typename Elf::Ehdr& ehdr = *read_ehdr;
    if (auto read_shdrs = ReadShdrsFromFile(diagnostics, file,
                                            std::forward<ShdrAllocator>(shdr_allocator), ehdr)) {
      return std::make_pair(*std::move(read_ehdr), *std::move(read_shdrs));
    }
  }
  return std::nullopt;
}

// This does the same work as LoadShdrsFromFile, but handles different ELFCLASS
// (and optionally, ELFDATA) formats in the ELF file.  Like LoadShdrsFromFile,
// it checks Ehdr::Valid rather than Ehdr::Loadable.  It returns false if the
// file is invalid, or else returns the result of invoking the callback as
// `bool(const Ehdr&, std::span<const Shdr>)` for the particular Elf64<...> or
// Elf32<...> instantiation chosen.
//
// The MetaAllocator is a lambda <typename T>(size_t) that is called with the
// specific elfldltl::Elf<...>::Shdr type as an explicit template parameter and
// should otherwise act like the Allocator API object for ReadArrayFromFile<T>.
//
// The optional expected_data argument is omitted (or std::nullopt) to permit
// callbacks with either data format (byte order) as well as either class.  The
// final optional argument can give the machine architecture to match, and
// likewise can be omitted or std::nullopt to accept any machine.  With both
// omitted, this handles any valid ELF file of whatever format and machine.
template <class Diagnostics, class File, class MetaAllocator, typename Callback>
constexpr bool WithShdrsFromFile(Diagnostics& diagnostics, File& file,
                                 MetaAllocator&& meta_allocator, Callback&& callback,
                                 std::optional<ElfData> expected_data = std::nullopt,
                                 std::optional<ElfMachine> machine = std::nullopt) {
  return WithLoadHeadersFromFile<ShdrFromElf, ReadShdrsFromFile,
                                 [](const auto& ehdr, auto& diag, auto machine) -> bool {
                                   return ehdr.Valid(diag, machine);
                                 }>(diagnostics, file, std::forward<MetaAllocator>(meta_allocator),
                                    std::forward<Callback>(callback), expected_data, machine);
}

// Read the contents of a section described by `shdr` as an array of `T`.
//
// `read_array` is invoked as `read_array(shdr.offset, shdr.size / sizeof(T))`,
// e.g. a lambda wrapping `file.ReadArrayFromFile<T>(offset, allocator, count)`
// (see memory.h).  Its return value is returned directly: typically a
// `std::optional` whose `value()` owns or views the storage and is implicitly
// convertible to `std::span<const T>` (or `std::span<T>`) for the lifetime of
// that returned object.
//
// Note that this does not inspect `shdr.type` (callers must check for
// `ElfShdrType::kNobits`, whose `shdr.offset` does not correspond to file
// data) or check that `shdr.size` is an exact multiple of `sizeof(T)` (any
// trailing partial-element bytes are ignored).
template <typename T = std::byte, class Shdr, typename ReadArray>
constexpr auto ReadSection(const Shdr& shdr, ReadArray&& read_array) {
  return std::forward<ReadArray>(read_array)(shdr.offset, shdr.size / sizeof(T));
}

// Read and validate the `.shstrtab` section contents given the Ehdr and shdrs.
//
// This resolves `ehdr.shstrndx` (including `Ehdr::kShnXindex` via
// `shdrs[0].link`), checks that it names a valid non-zero entry in `shdrs`,
// reads that section as `char` elements via `ReadSection<char>(..., read_array)`
// (so `read_array(offset, count)` must return a `std::optional` whose value
// converts to `std::span<const char>`), and verifies that the table is
// non-empty and NUL-terminated so it is safe to pass to `ShdrWithName`.
// Format or read failures are reported to `diagnostics` and yield
// `std::nullopt`.
template <class Diagnostics, class Ehdr, typename ReadArray>
constexpr auto ReadShstrtab(Diagnostics& diagnostics, const Ehdr& ehdr,
                            std::span<const typename Ehdr::ElfLayout::Shdr> shdrs,
                            ReadArray&& read_array)
    -> decltype(std::forward<ReadArray>(read_array)(shdrs.back().offset, shdrs.back().size())) {
  using namespace std::literals::string_view_literals;

  uint32_t shstrndx;
  // This is the marker that the index might exceed 16 bits.  In that case,
  // it's instead stored in the special stub section header at index 0.
  if (ehdr.shstrndx == Ehdr::kShnXindex) {
    if (shdrs.empty()) [[unlikely]] {
      diagnostics.FormatError("ELF header has SHN_XINDEX in e_shstrndx with no sections"sv);
      return std::nullopt;
    }
    shstrndx = shdrs.front().link;
  } else {
    shstrndx = ehdr.shstrndx;
  }

  if (shstrndx == 0) [[unlikely]] {
    diagnostics.FormatError("ELF header has no e_shstrndx"sv);
    return std::nullopt;
  }

  if (shstrndx >= shdrs.size()) [[unlikely]] {
    diagnostics.FormatError("ELF header has invalid e_shstrndx "sv, shstrndx, " with only "sv,
                            shdrs.size(), " sections"sv);
    return std::nullopt;
  }

  auto result = ReadSection<char>(shdrs[shstrndx], std::forward<ReadArray>(read_array));
  if (!result) [[unlikely]] {
    diagnostics.FormatError("cannot read .shstrtab section from ELF file"sv);
    return std::nullopt;
  }

  std::span<const char> chars = *result;
  if (chars.empty()) [[unlikely]] {
    diagnostics.FormatError("e_shstrndx section is empty"sv);
    return std::nullopt;
  }
  if (chars.back() != '\0') [[unlikely]] {
    diagnostics.FormatError("e_shstrndx section is not NUL-terminated"sv);
    return std::nullopt;
  }

  return result;
}

// Return a C++20 range adaptor closure that pairs each section header with its
// name from `.shstrtab`, transforming an input range of `const Shdr&` into a
// range of `std::pair<std::string_view, const Shdr&>`:
// ```
//   for (const auto& [name, shdr] : shdrs | elfldltl::ShdrWithName(shstrtab)) {
//     ...
//   }
// ```
// `shstrtab` should be a NUL-terminated string table (as validated by
// `ReadShstrtab`); the `std::string_view` is captured by value, so the backing
// `.shstrtab` storage must outlive iteration and any use of the yielded name
// views.  If `shdr.name` is 0 (assuming a standard leading NUL byte) or out of
// bounds (`>= shstrtab.size()`), the yielded name is an empty `std::string_view`.
constexpr auto ShdrWithName(std::string_view shstrtab) {
  return std::views::transform([shstrtab]<class Shdr>(const Shdr& shdr) {
    std::string_view name;
    if (shdr.name < shstrtab.size()) [[likely]] {
      name = shstrtab.substr(shdr.name);
      name = name.substr(0, name.find_first_of('\0'));
    }
    return std::pair<std::string_view, const Shdr&>(name, shdr);
  });
}

constexpr auto ShdrWithName(std::span<const char> shstrtab) {
  return ShdrWithName(std::string_view{shstrtab.data(), shstrtab.size()});
}

}  // namespace elfldltl

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_SHDR_H_
