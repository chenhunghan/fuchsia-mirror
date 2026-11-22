// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "symbols.h"

#include <elf.h>
#include <lib/zircon-internal/align.h>
#include <lib/zx/vmar.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include <algorithm>
#include <cstring>
#include <span>
#include <string>
#include <string_view>
#include <vector>

#include "restricted_symbols.h"

namespace {

const uint8_t kElfMagic[] = {0x7f, 'E', 'L', 'F'};

// Holds a read-only VMO region mapped into memory.
template <typename T>
class MappedTable {
 public:
  // Maps |byte_len| bytes located at |offset| in |driver_vmo|.
  static zx::result<MappedTable<T>> Create(const zx::unowned_vmo& driver_vmo, uint64_t offset,
                                           uint64_t byte_len) {
    if (byte_len == 0) {
      return zx::ok(MappedTable<T>(nullptr, 0, 0));
    }
    if (offset % alignof(T) != 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    uint64_t end_offset = 0;
    if (__builtin_add_overflow(offset, byte_len, &end_offset)) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }

    const size_t kPageSize = zx_system_get_page_size();
    // Mapping only works on page-aligned values.
    uint64_t mapped_offset = ZX_ROUNDDOWN(offset, kPageSize);
    uint64_t intra_page_offset = offset - mapped_offset;
    uint64_t unaligned_size = 0;
    if (__builtin_add_overflow(byte_len, intra_page_offset, &unaligned_size)) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    uint64_t mapped_size = ZX_ROUNDUP(unaligned_size, kPageSize);
    if (mapped_size < unaligned_size) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }

    zx_vaddr_t mapped_addr = 0;
    zx_status_t status = zx::vmar::root_self()->map(ZX_VM_PERM_READ, 0, *driver_vmo, mapped_offset,
                                                    mapped_size, &mapped_addr);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    const T* table = reinterpret_cast<const T*>(mapped_addr + intra_page_offset);
    return zx::ok(MappedTable<T>(table, mapped_addr, mapped_size));
  }

  ~MappedTable() {
    if (!mapped_addr_) {
      return;
    }
    [[maybe_unused]] zx_status_t status = zx::vmar::root_self()->unmap(mapped_addr_, mapped_size_);
    ptr_ = nullptr;
    mapped_addr_ = 0;
    mapped_size_ = 0;
  }

  // Disallow copying.
  MappedTable(const MappedTable&) = delete;
  MappedTable& operator=(const MappedTable&) = delete;

  MappedTable(MappedTable&& other) noexcept
      : MappedTable(other.ptr_, other.mapped_addr_, other.mapped_size_) {
    other.ptr_ = nullptr;
    other.mapped_addr_ = 0;
    other.mapped_size_ = 0;
  }

  MappedTable& operator=(MappedTable&& other) noexcept {
    if (this != &other) {
      if (mapped_addr_) {
        [[maybe_unused]] zx_status_t status =
            zx::vmar::root_self()->unmap(mapped_addr_, mapped_size_);
      }
      ptr_ = other.ptr_;
      mapped_addr_ = other.mapped_addr_;
      mapped_size_ = other.mapped_size_;
      other.ptr_ = nullptr;
      other.mapped_addr_ = 0;
      other.mapped_size_ = 0;
    }
    return *this;
  }

  const T* ptr() const { return ptr_; }

 private:
  MappedTable(const T* ptr, uint64_t mapped_addr, uint64_t mapped_size)
      : ptr_(ptr), mapped_addr_(mapped_addr), mapped_size_(mapped_size) {}

  const T* ptr_ = nullptr;
  uint64_t mapped_addr_ = 0;
  uint64_t mapped_size_ = 0;
};

zx::result<Elf64_Ehdr> GetElfHeader(const zx::unowned_vmo& driver_vmo) {
  Elf64_Ehdr elf_header;
  zx_status_t status = driver_vmo->read(&elf_header, 0, sizeof(elf_header));
  if (status != ZX_OK) {
    return zx::error(status);
  }
  if (memcmp(elf_header.e_ident, kElfMagic, sizeof(kElfMagic)) != 0) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (elf_header.e_shoff == 0 || elf_header.e_shnum == 0 ||
      elf_header.e_shentsize != sizeof(Elf64_Shdr)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(elf_header);
}

// Reads the entire section header table in a single VMO read.
zx::result<std::vector<Elf64_Shdr>> ReadSectionHeaders(const zx::unowned_vmo& driver_vmo,
                                                       const Elf64_Ehdr& elf_header) {
  std::vector<Elf64_Shdr> headers(elf_header.e_shnum);
  zx_status_t status =
      driver_vmo->read(headers.data(), elf_header.e_shoff, headers.size() * sizeof(Elf64_Shdr));
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(std::move(headers));
}

zx::result<std::string_view> GetRelativeUrl(std::string_view url) {
  constexpr std::string_view kPrefix = "#meta/";

  size_t prefix_start_idx = url.find(kPrefix);
  if (prefix_start_idx == std::string_view::npos) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  return zx::ok(url.substr(prefix_start_idx));
}

}  // namespace

namespace driver_symbols {

zx::result<std::vector<std::string>> FindRestrictedSymbols(zx::unowned_vmo driver_vmo,
                                                           std::string_view driver_url) {
  auto relative_url = GetRelativeUrl(driver_url);
  if (relative_url.is_error()) {
    return relative_url.take_error();
  }

  zx::result<Elf64_Ehdr> elf_header = GetElfHeader(driver_vmo);
  if (elf_header.is_error()) {
    return elf_header.take_error();
  }

  zx::result<std::vector<Elf64_Shdr>> section_headers = ReadSectionHeaders(driver_vmo, *elf_header);
  if (section_headers.is_error()) {
    return section_headers.take_error();
  }

  // Find the section header for the dynamic symbols table.
  const Elf64_Shdr* dynsym_header = nullptr;
  for (const auto& header : *section_headers) {
    if (header.sh_type == SHT_DYNSYM) {
      dynsym_header = &header;
      break;
    }
  }
  if (dynsym_header == nullptr) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  // Find the section header for the dynamic symbols string table.
  if (dynsym_header->sh_link == 0 || dynsym_header->sh_link >= section_headers->size()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  const Elf64_Shdr& dynsym_strings_header = (*section_headers)[dynsym_header->sh_link];
  if (dynsym_header->sh_size < sizeof(Elf64_Sym) || dynsym_strings_header.sh_size == 0) {
    return zx::ok(std::vector<std::string>{});
  }

  // Map both the dynamic symbols table and its string table rather than issuing one VMO read per
  // symbol.
  auto dynsym_table =
      MappedTable<Elf64_Sym>::Create(driver_vmo, dynsym_header->sh_offset, dynsym_header->sh_size);
  if (dynsym_table.is_error()) {
    return dynsym_table.take_error();
  }
  auto dynsym_strings_table = MappedTable<char>::Create(driver_vmo, dynsym_strings_header.sh_offset,
                                                        dynsym_strings_header.sh_size);
  if (dynsym_strings_table.is_error()) {
    return dynsym_strings_table.take_error();
  }

  const bool runtime_allowed =
      std::ranges::binary_search(kRestrictedDriverRuntimeSymbolsDriversAllowlist, *relative_url);
  const bool thread_allowed =
      std::ranges::binary_search(kCreateThreadSymbolsDriversAllowlist, *relative_url);

  std::vector<std::string> matches;
  const size_t symbol_count = dynsym_header->sh_size / sizeof(Elf64_Sym);
  const uint64_t strtab_size = dynsym_strings_header.sh_size;
  std::span<const Elf64_Sym> symbols(dynsym_table->ptr(), symbol_count);
  for (const Elf64_Sym& symbol : symbols) {
    if (symbol.st_name == 0 || symbol.st_name >= strtab_size) {
      continue;
    }
    const char* raw_name = dynsym_strings_table->ptr() + symbol.st_name;
    size_t max_len = static_cast<size_t>(strtab_size - symbol.st_name);
    size_t name_len = strnlen(raw_name, max_len);
    if (name_len == max_len) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    std::string_view name(raw_name, name_len);
    if (std::ranges::binary_search(kRestrictedLibcSymbols, name)) {
      matches.emplace_back(name);
    } else if (!runtime_allowed &&
               std::ranges::binary_search(kRestrictedDriverRuntimeSymbols, name)) {
      matches.emplace_back(name);
    } else if (!thread_allowed && std::ranges::binary_search(kCreateThreadSymbols, name)) {
      matches.emplace_back(name);
    }
  }
  return zx::ok(std::move(matches));
}

}  // namespace driver_symbols

extern "C" {

struct restricted_symbols {
  std::vector<std::string> symbols;
};

zx_status_t restricted_symbols_find(zx_handle_t driver_vmo, const char* driver_url,
                                    restricted_symbols_t** out_symbols, size_t* out_symbols_found) {
  auto result = driver_symbols::FindRestrictedSymbols(zx::unowned_vmo(driver_vmo), driver_url);
  if (result.is_error()) {
    return result.status_value();
  }

  *out_symbols_found = result.value().size();
  *out_symbols = new restricted_symbols_t(std::move(result.value()));

  return ZX_OK;
}

const char* restricted_symbols_get(restricted_symbols_t* symbols, size_t index) {
  return symbols->symbols[index].c_str();
}

void restricted_symbols_free(restricted_symbols_t* symbols) { delete symbols; }
}
