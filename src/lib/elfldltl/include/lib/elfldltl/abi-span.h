// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_ABI_SPAN_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_ABI_SPAN_H_

#include <concepts>

#include "internal/abi-span.h"
#include "layout.h"

namespace elfldltl {
namespace internal {

template <typename>
inline constexpr bool kIsAbiSpan = false;
template <typename T, size_t N, ElfApi Elf, AbiPtrTraitsApi<T, Elf> Traits>
inline constexpr bool kIsAbiSpan<AbiSpan<T, N, Elf, Traits>> = true;

template <class Ptr, typename U, size_t Extent, ElfApi Elf, AbiPtrTraitsApi<U, Elf> Traits>
constexpr Ptr ConvertAbiPtr(const AbiSpan<U, Extent, Elf, Traits>& other) {
  if constexpr (Ptr::kLocal) {
    return Ptr{other.data()};
  } else {
    return Ptr{other.ptr()};
  }
}

}  // namespace internal

// elfldltl::AbiSpan<T> is like a C++20 std::span<T> but with the same
// properties as elfldltl::AbiPtr<T> (see <lib/elfldltl/abi-ptr.h>): it can be
// used to store pointers and lengths that use a different byte order, address
// size, or address space, than the current process.
//
// It always supports the std::span constructors and operations that don't use
// take T* pointers or dereference into the span (size, subspan, etc.), but
// not the iterator methods (begin, end, etc.).
//
// When instantiated for the native byte order, address size, and address
// space, it supports all the other std::span<T> methods (though not all the
// fancy constructors), can be copy-constructed from std::span<T>, and also has
// an explicit `get()` method to return it.  The iterator methods (begin, end,
// etc.) return the std::span<T> iterator types directly.

template <typename T, size_t N = std::dynamic_extent, ElfApi Elf = elfldltl::Elf<>,
          AbiPtrTraitsApi<T, Elf> Traits = LocalAbiTraits>
class AbiSpan : public internal::AbiSpanImpl<T, N, Elf, Traits> {
 public:
  using typename internal::AbiSpanImpl<T, N, Elf, Traits>::Ptr;
  using typename internal::AbiSpanImpl<T, N, Elf, Traits>::size_type;

  constexpr AbiSpan() = default;

  constexpr AbiSpan(const AbiSpan&) = default;

  explicit constexpr AbiSpan(Ptr ptr, size_type size) noexcept
      : internal::AbiSpanImpl<T, N, Elf, Traits>(ptr) {
    assert(size == N);
  }

  template <typename U>
    requires std::constructible_from<std::span<T, N>, U*, size_t>
  explicit constexpr AbiSpan(AbiPtr<U, Elf, Traits> ptr, size_type size) noexcept
      : AbiSpan{Ptr{ptr}, size} {}

  // Allow whatever conversions to related AbiSpan types std::span would allow.
  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T, N>, std::span<U, Extent>> &&
             std::convertible_to<AbiPtr<U, Elf, Traits>, Ptr>
  constexpr explicit(!std::convertible_to<std::span<U, Extent>, std::span<T, N>>)
      AbiSpan(AbiSpan<U, Extent, Elf, Traits> other)
      : internal::AbiSpanImpl<T, N, Elf, Traits>{internal::ConvertAbiPtr<Ptr>(other)} {
    assert(other.size() == N);
  }

  // If pointers are local, allow whatever kinds of construction span would.
  template <typename Other>
    requires(!internal::kIsAbiSpan<std::remove_cvref_t<Other>> &&
             !std::same_as<std::remove_cvref_t<Other>, std::span<T, N>> && Ptr::kLocal &&
             std::constructible_from<std::span<T, N>, Other>)
  constexpr explicit(!std::convertible_to<Other, std::span<T, N>>) AbiSpan(Other&& other)
      : AbiSpan{std::span<T, N>{std::forward<Other>(other)}} {}

  // Allow copy-construction from the corresponding span type if it admits
  // construction from a pointer.
  explicit(false) constexpr AbiSpan(std::span<T, N> other)
    requires Ptr::kLocal
      : internal::AbiSpanImpl<T, N, Elf, Traits>(Ptr{other.data()}) {}

  // If pointers are local, convert implicitly to the plain std::span.
  constexpr explicit(false) operator std::span<T, N>() const
    requires Ptr::kLocal
  {
    return std::span<T, N>{this->data(), size()};
  }

  constexpr AbiSpan& operator=(const AbiSpan&) = default;

  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T, N>, std::span<U, Extent>>
  constexpr AbiSpan& operator=(const AbiSpan<U, Extent, Elf, Traits>& other) {
    *this = AbiSpan{other.ptr(), other.size()};
    return *this;
  }

  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T, N>, std::span<U, Extent>> && Ptr::kLocal
  constexpr AbiSpan& operator=(std::span<U, Extent> other) {
    *this = AbiSpan{other};
    return *this;
  }

  constexpr size_type size() const { return static_cast<size_type>(N); }
};

// Deduction guide, equivalent to the std::span deduction guide.
template <typename... Args>
AbiSpan(Args&&... args)
    -> AbiSpan<typename decltype(std::span{std::forward<Args>(args)...})::element_type,
               decltype(std::span{std::forward<Args>(args)...})::extent>;

template <typename T, ElfApi Elf, class Traits>
class AbiSpan<T, std::dynamic_extent, Elf, Traits>
    : public internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits> {
 public:
  using typename internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>::Ptr;
  using typename internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>::Addr;
  using typename internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>::size_type;

  constexpr AbiSpan() = default;

  constexpr AbiSpan(const AbiSpan&) = default;

  constexpr AbiSpan(Ptr ptr, size_type size) noexcept
      : internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>{ptr}, size_(size) {}

  template <typename U>
    requires std::constructible_from<std::span<T>, U*, size_t>
  constexpr AbiSpan(AbiPtr<U, Elf, Traits> ptr, size_type size) noexcept
      : AbiSpan{Ptr{ptr}, size} {}

  // Allow whatever conversions to related AbiSpan types std::span would allow.
  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T>, std::span<U, Extent>> &&
                 std::convertible_to<AbiPtr<U, Elf, Traits>, Ptr>
  constexpr explicit(!std::convertible_to<std::span<U, Extent>, std::span<T>>)
      AbiSpan(AbiSpan<U, Extent, Elf, Traits> other)
      : internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>{
            internal::ConvertAbiPtr<Ptr>(other)},
        size_{other.size()} {}

  // If pointers are local, allow whatever kinds of construction span would.
  template <typename Other>
    requires(!internal::kIsAbiSpan<std::remove_cvref_t<Other>> &&
             !std::same_as<std::remove_cvref_t<Other>, std::span<T>> && Ptr::kLocal &&
             std::constructible_from<std::span<T>, Other>)
  constexpr explicit(!std::convertible_to<Other, std::span<T>>) AbiSpan(Other&& other)
      : AbiSpan{std::span<T>{std::forward<Other>(other)}} {}

  // See comment above.
  explicit(false) constexpr AbiSpan(std::span<T> other)
    requires Ptr::kLocal
      : internal::AbiSpanImpl<T, std::dynamic_extent, Elf, Traits>{Ptr{other.data()}},
        size_{static_cast<size_type>(other.size())} {}

  // If pointers are local, convert implicitly to the plain std::span.
  constexpr explicit(false) operator std::span<T>() const
    requires Ptr::kLocal
  {
    return std::span<T>{this->data(), size()};
  }

  constexpr AbiSpan& operator=(const AbiSpan&) = default;

  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T>, std::span<U, Extent>>
  constexpr AbiSpan& operator=(const AbiSpan<U, Extent, Elf, Traits>& other) {
    *this = AbiSpan{other.ptr(), other.size()};
    return *this;
  }

  template <typename U, size_t Extent>
    requires std::constructible_from<std::span<T>, std::span<U, Extent>> && Ptr::kLocal
  constexpr AbiSpan& operator=(std::span<U, Extent> other) {
    *this = AbiSpan{other};
    return *this;
  }

  constexpr size_type size() const { return size_; }

 private:
  Addr size_ = 0;
};

// elfldltl::AbiStringView stands in for std::string_view, but is actually
// based on elfldltl::AbiSpan<const char>.
//
// It only replicates the most basic methods of std::string_view.  When
// dereferencing is supported by the Traits type, it just provides a `get()`
// method and implicit conversion to and from std::string_view so it can be
// copied into a std::string_view to call the various methods.
template <ElfApi Elf = elfldltl::Elf<>, class Traits = LocalAbiTraits>
class AbiStringView {
 public:
  using Span = AbiSpan<const char, std::dynamic_extent, Elf, Traits>;
  using Ptr = Span::Ptr;
  using size_type = Span::size_type;

  constexpr AbiStringView() = default;

  constexpr AbiStringView(const AbiStringView&) = default;

  constexpr explicit AbiStringView(Span contents) : contents_(contents) {}

  constexpr AbiStringView(Ptr ptr, size_type length) : contents_{ptr, length} {}

  explicit(false) constexpr AbiStringView(std::string_view str)
    requires Ptr::kLocal
      : contents_{std::span{str}} {}

  constexpr AbiStringView& operator=(const AbiStringView&) = default;

  constexpr AbiStringView& operator=(std::string_view other)
    requires Ptr::kLocal
  {
    *this = AbiStringView{other};
    return *this;
  }

  constexpr bool empty() const { return contents_.empty(); }

  constexpr size_t size() const { return contents_.size(); }
  constexpr size_t length() const { return contents_.size(); }

  constexpr Ptr ptr() const { return contents_.ptr(); }

  constexpr Span as_span() const { return contents_; }

  explicit(false) constexpr operator std::string_view() const
    requires Ptr::kLocal
  {
    return get();
  }

  constexpr std::string_view get() const
    requires Ptr::kLocal
  {
    return {data(), size()};
  }

  constexpr const char* data() const
    requires Ptr::kLocal
  {
    return contents_.data();
  }

 private:
  Span contents_;
};

}  // namespace elfldltl

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_ABI_SPAN_H_
