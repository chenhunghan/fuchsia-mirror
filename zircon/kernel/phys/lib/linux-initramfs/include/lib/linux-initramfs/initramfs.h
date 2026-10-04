// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_LIB_LINUX_INITRAMFS_INCLUDE_LIB_LINUX_INITRAMFS_INITRAMFS_H_
#define ZIRCON_KERNEL_PHYS_LIB_LINUX_INITRAMFS_INCLUDE_LIB_LINUX_INITRAMFS_INITRAMFS_H_

#include <cassert>
#include <cstddef>
#include <iterator>
#include <ranges>
#include <span>
#include <string_view>

namespace linux_initramfs {

// This represents one file in the initramfs.  The initramfs (cpio) member
// header has more information than just the name, but nothing else matters.
struct Member {
  static constexpr size_t kHeaderSize = 110;

  constexpr bool empty() const { return name.empty() && contents.empty(); }

  constexpr explicit operator bool() const { return !empty(); }

  // This gives the whole size occupied in the image.
  constexpr size_t size_bytes() const {
    if (empty()) {
      return 0;
    }

    // The initial header size is fixed.  Note: it's not aligned!
    size_t bytes = kHeaderSize;
    auto align = [&bytes] { bytes = (bytes + 3) & -size_t{4}; };

    // The name doesn't need to be aligned after the (misaligned) header.  Add
    // the NUL terminator after the name and round up to align for the data.
    bytes += name.size() + 1;
    align();

    // The exact file size is just rounded up to align.
    bytes += contents.size_bytes();
    align();

    return bytes;
  }

  std::string_view name;
  std::span<const std::byte> contents;
};

// This is cheap view type that acts as a std::ranges::forward_range of
// linux_initramfs::Member objects:
// ```
// for (auto [name, contents] : linux_initramfs::Initramfs(raw_bytes)) {
//   ...
// }
// ```
class Initramfs {
 public:
  using value_type = Member;

  class iterator;
  using const_iterator = iterator;

  Initramfs() = default;
  Initramfs(const Initramfs&) = default;
  Initramfs& operator=(const Initramfs&) = default;

  // This implicitly accepts std::string_view et al too.
  constexpr explicit Initramfs(std::span<const char> contents)
      : contents_{contents.data(), contents.size()} {}

  explicit Initramfs(std::span<const std::byte> contents)
      : Initramfs(std::span{
            reinterpret_cast<const char*>(contents.data()),
            contents.size_bytes(),
        }) {}

  template <typename T>
  explicit Initramfs(std::span<T> contents) : Initramfs(std::as_bytes(contents)) {}

  iterator begin() const;
  constexpr iterator end() const;

  inline bool empty() const;

 private:
  std::string_view contents_;
};

class Initramfs::iterator {
 public:
  using difference_type = std::string_view::difference_type;
  using value_type = Member;
  using iterator_concept = std::forward_iterator_tag;

  constexpr const value_type& operator*() const { return value_; }
  constexpr const value_type* operator->() const { return &value_; }

  constexpr bool operator==(const iterator& other) const {
    AssertOtherIsBrother(other);
    return contents_.data() == other.contents_.data();
  }

  constexpr auto operator<=>(const iterator& other) const {
    AssertOtherIsBrother(other);
    return contents_.data() <=> other.contents_.data();
  }

  iterator& operator++();  // prefix

  iterator operator++(int) {  // postfix
    auto it = *this;
    ++*this;
    return it;
  }

 private:
  friend Initramfs;

  // Only called with other >= contents.data().
  [[maybe_unused]] static constexpr bool Contains(std::string_view contents, const char* other) {
    return static_cast<size_t>(other - contents.data());
  }

  constexpr void AssertOtherIsBrother(const iterator& other) const {
    if (contents_.empty() || other.contents_.empty()) {
      return;
    }
    if (contents_.data() > other.contents_.data()) {
      [[maybe_unused]] const char* ptr = other.contents_.data();
      assert(Contains(contents_, ptr));
    } else {
      [[maybe_unused]] const char* ptr = contents_.data();
      assert(Contains(other.contents_, ptr));
    }
  }

  std::string_view contents_;
  value_type value_;
};

constexpr auto Initramfs::end() const -> iterator { return iterator{}; }

inline bool Initramfs::empty() const { return begin() == end(); }

static_assert(std::ranges::forward_range<Initramfs>);
static_assert(std::same_as<Member, std::ranges::range_value_t<Initramfs>>);

}  // namespace linux_initramfs

#endif  // ZIRCON_KERNEL_PHYS_LIB_LINUX_INITRAMFS_INCLUDE_LIB_LINUX_INITRAMFS_INITRAMFS_H_
