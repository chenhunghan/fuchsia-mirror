// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INIT_FINI_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INIT_FINI_H_

#include <cassert>
#include <concepts>
#include <iterator>
#include <optional>
#include <ranges>
#include <span>
#include <type_traits>

#include "abi-ptr.h"
#include "abi-span.h"
#include "internal/init-fini.h"
#include "layout.h"

// elfldltl::InitFiniInfo represents the information about either initializers
// or finalizers for one ELF module.  Two separate InitFiniInfo objects are
// used for a module's initializers and finalizers.  (An initializers list
// might not be needed after loading, while finalizer info must be stored.)
//
// This is normally populated by a call to elfldltl::DecodeDynamic using an
// elfldltl::DynamicInitObserver or elfldltl::DynamicFiniObserver observer.
//
// The various `*init()` and `*fini()` methods provide provide general ordered
// access to the function addresses in each list via random_access_range
// objects.  The correct method should be used for each kind of list to get the
// appropriate ordering of elements.
//
// When dealing with the lists, the caller must indicate whether the ELF
// segment data (i.e. where DT_INIT_ARRAY / DT_FINI_ARRAY would point) has
// already had relocations applied or not.  Because of the need to support the
// legacy DT_INIT / DT_FINI encoding, the runtime load bias must always be
// supplied to the complete runtime address list even when relocations have
// already applied.

namespace elfldltl {

// A module initializer or finalizer function usually has type void().  The
// function pointers emitted into the special section by the compiler don't
// usually get wrapped by -fsanitize=cfi, so their signatures can't be checked.
//
// If a different protocol of arguments passed to each function is needed, the
// template alias produces function type with the CFI-disabling attribute.  If
// CFI checking on the pointers is appropriate, just use `void(Args...)` as is.
//
// Non-void return types can be used with the range-based interfaces, though
// the convenience methods for calling a list in order all just ignore any
// return values.
template <typename... Args>
using InitFiniFunctionWithArgs = void(Args...) [[clang::cfi_unchecked_callee]];
using InitFiniFunction = InitFiniFunctionWithArgs<>;

// The common view of an init or fini range is just a flat range of addresses /
// function pointers.  It's a random_access_range but not a contiguous_range.
template <class Range, typename FnPtr = uintptr_t>
concept InitFiniRangeApi = std::ranges::random_access_range<Range> &&
                           std::convertible_to<std::ranges::range_value_t<Range>, FnPtr>;

// A helper concept: `InvocableAs<T, F>` is `std::invocable<T, Args...>` for
// the argument signature of F.
template <typename T, typename F>
concept InvocableAs =
    internal::SignatureOf<F>::template Arguments<internal::InvocableOn<T>::template type>::value;

// A "callable" view of an init or fini range for a given function type F is
// both a view of the range as F* and also itself callable with the same
// arguments as a function of type F to iterate over the range calling each
// with the same arguments.  While it's valid for F to have a non-void return
// type, calling the range object itself as a function always ignores all the
// return values and returns void.
template <class Range, typename F = InitFiniFunction>
concept InitFiniCallableApi =
    // std::ranges::transform_view doesn't preserve the random-access category,
    // only the bidirectional category.
    std::ranges::bidirectional_range<Range> &&
    std::convertible_to<std::ranges::range_value_t<Range>, F*> && InvocableAs<Range, F>;

// The InitFiniInfo object's basic functionality is to produce a range that
// meets the InitFiniRawRangeApi<Elf::size_type> contract.  Each element of the
// range is a pair of address and "relocated" flag.  The flag indicates whether
// that address is a plain "file relative" (a la elfldltl::FileAddress) address
// (unrelocated) that needs a load bias added, or has already been adjusted to
// an absolute runtime address.
template <class Range, typename Address = uintptr_t>
concept InitFiniRawRangeApi = InitFiniRangeApi<Range, std::pair<Address, bool>>;

// This turns a "raw" init or fini range (of {address, bool} pairs) into a
// range of plain addresses with the bias applied where appropriate.  If the
// bias is zero, then this is effectively the same as std::views::keys(range).
template <std::unsigned_integral Address, InitFiniRawRangeApi<Address> Range>
constexpr InitFiniRangeApi<Address> auto RelocatedInitFiniRange(  //
    Range&& range, Address bias) {
  auto relocate = [bias](std::pair<Address, bool> elt) -> Address {
    auto [addr, relocated] = elt;
    return relocated ? addr : addr + bias;
  };
  return std::views::transform(std::forward<Range>(range), relocate);
}

// Convert a range of any other InvocableAs<F> into that same range, but also
// itself callable like F.
template <typename F, std::ranges::input_range R>
  requires InvocableAs<std::ranges::range_value_t<R>, F>
[[nodiscard]] static constexpr std::ranges::input_range auto CallableAs(R&& range) {
  return internal::CallableRange<F, R>{std::forward<R>(range)};
}

// Convert the range of absolute address values as from init() / fini() into
// a range of F* that's also itself callable like F.
template <typename F, InitFiniRangeApi<uintptr_t> R>
  requires std::is_function_v<F>
[[nodiscard]] static constexpr InitFiniCallableApi<F> auto CallableAs(R&& range) {
  constexpr auto cast = [](uintptr_t fnptr) { return reinterpret_cast<F*>(fnptr); };
  return CallableAs<F>(std::views::transform(std::forward<R>(range), cast));
}

// The object itself is a very small and cheaply-copied container (a span plus
// another word) with stable ABI (see <lib/elfldltl/abi-ptr.h> for details).
template <ElfApi Elf = Elf<>,
          AbiPtrTraitsApi<const typename Elf::Addr, Elf> AbiTraits = LocalAbiTraits>
struct InitFiniInfo {
 public:
  using Addr = Elf::Addr;
  using size_type = Elf::size_type;

  // The DT_INIT_ARRAY / DT_FINI_ARRAY looks like this in memory.
  using Array = std::span<const Addr>;

  // When true, it's actually usable directly in memory.  Otherwise, this is
  // just a container giving the bounds of the array residing elsewhere.
  static constexpr bool kLocal = AbiPtrLocalTraitsApi<AbiTraits, const Addr, Elf>;

  constexpr InitFiniInfo() = default;
  constexpr InitFiniInfo(const InitFiniInfo&) = default;

  // InitFiniInfo can be default-constructed and then set_* called, or it can
  // be explicitly constructed from an array with no legacy singleton.
  constexpr explicit InitFiniInfo(Array array) : array_{array} {
    static_assert(InitFiniRangeApi<Array, Addr>);
    static_assert(std::default_initializable<InitFiniInfo>);
    static_assert(std::copyable<InitFiniInfo>);
  }

  constexpr InitFiniInfo& operator=(const InitFiniInfo&) = default;

  // An array of function pointers, in the .init_array or .fini_array section,
  // which is normally part of the RELRO segment.  So the pointers here are
  // unrelocated in the file, but dynamic relocation records apply simple
  // fixup.  As this points directly into the load image in the Memory object,
  // if that image is being relocated in place, then these values will be
  // absolute function pointers after relocation.  If the original file data
  // (or the load image before relocation) is being read, the these addresses
  // need the load bias added.
  constexpr Array array() const { return array_; }

  // A single function pointer, from the legacy DT_INIT or DT_FINI entry.  This
  // is not contiguous with the array and is stored separately in the ELF
  // headers where no relocation records apply.  So this address always needs
  // the load bias added to yield a runtime function pointer.
  constexpr std::optional<Addr> legacy() const {
    if (legacy_ != 0) {
      return legacy_;
    }
    return std::nullopt;
  }

  constexpr InitFiniInfo& set_array(Array array) {
    array_ = array;
    return *this;
  }

  constexpr InitFiniInfo& set_legacy(Addr legacy) {
    legacy_ = legacy;
    return *this;
  }

  // Return the number of function pointers present.
  constexpr Array::size_type size() const { return array_.size() + (legacy_ != 0 ? 1 : 0); }

  constexpr bool empty() const { return size() == 0; }

  // When using local pointers, the methods returning InitFiniRangeApi objects
  // are available.  The raw ranges are just the data as it sits in memory,
  // plus the flag distinguishing the unrelocated single legacy entry from the
  // (usually) relocated array entries.  The other adapters turn these into a
  // simple uniform range of relocated addresses, and can also treat those as
  // local function pointers.  Returned range objects and their iterators are
  // self-contained and not tied to the lifetime of this InitFiniInfo object.
  // All are fairly small, copyable objects like InitFiniInfo self, that all
  // copy the same unowned array().data() pointer.

  // Return a range of {address, bool} pairs for initializer functions to call;
  // the bool in each pair says whether the load bias has already been applied.
  constexpr InitFiniRawRangeApi<Addr> auto raw_init(bool relocated = true) const
    requires kLocal
  {
    Iterator begin, end;
    begin.array_ = end.array_ = array_;
    begin.legacy_ = end.legacy_ = legacy_;
    begin.relocated_ = end.relocated_ = relocated;
    begin.idx_ = 0;
    end.idx_ = end.size();
    return std::ranges::subrange{begin, end};
  }

  // The same, but reversed for finalizer calls.
  constexpr InitFiniRawRangeApi<Addr> auto raw_fini(bool relocated = true) const
    requires kLocal
  {
    return std::views::reverse(raw_init(relocated));
  }

  // Return a range of addresses, all with the bias applied.  If the relocated
  // flag is true, then DT_INIT_ARRAY / DT_FINI_ARRAY elements already have the
  // bias applied (but a legacy DT_INIT / DT_FINI may still need it).
  constexpr InitFiniRangeApi<size_type> auto init(size_type bias, bool relocated = true) const
    requires kLocal
  {
    return RelocatedInitFiniRange(raw_init(relocated), bias);
  }

  // The same, but reversed for finalizer calls.
  constexpr InitFiniRangeApi<size_type> auto fini(size_type bias, bool relocated = true) const
    requires kLocal
  {
    return std::views::reverse(init(bias, relocated));
  }

  // Like init(), but each address taken as the F* function pointer type.  The
  // return object is both a range of pointer values (InitFiniRangeApi<F*>) and
  // can itself be called with the same arguments as F to call each in turn.
  template <typename F = InitFiniFunction>
    requires kLocal && std::is_function_v<F>
  [[nodiscard]] constexpr InitFiniCallableApi<F> auto callable_init(  //
      size_type bias, bool relocated = true) const {
    return CallableAs<F>(init(bias, relocated));
  }

  // The same, but reversed for finalizer calls.
  template <typename F = InitFiniFunction>
    requires kLocal && std::is_function_v<F>
  [[nodiscard]] constexpr InitFiniCallableApi<F> auto callable_fini(  //
      size_type bias, bool relocated = true) const {
    return CallableAs<F>(fini(bias, relocated));
  }

  // The load bias is moot when relocated and no legacy DT_INIT / DT_FINI can
  // be set.  These no-argument versions must only be used when it's a known
  // invariant that legacy() would definitely return std::nullopt.  They are
  // provided in case environments with such invariants find it convenient not
  // to plumb through the load bias at all.

  constexpr InitFiniRangeApi<size_type> auto init_no_legacy() const
    requires kLocal
  {
    assert(legacy_ == 0);
    return init(0);
  }

  constexpr InitFiniRangeApi<size_type> auto fini_no_legacy() const
    requires kLocal
  {
    assert(legacy_ == 0);
    return fini(0);
  }

  template <typename F = InitFiniFunction>
    requires kLocal && std::is_function_v<F>
  [[nodiscard]] constexpr InitFiniCallableApi<F> auto callable_init_no_legacy() const {
    return CallableAs<F>(init_no_legacy());
  }

  template <typename F = InitFiniFunction>
    requires kLocal && std::is_function_v<F>
  [[nodiscard]] constexpr InitFiniCallableApi<F> auto callable_fini_no_legacy() const {
    return CallableAs<F>(fini_no_legacy());
  }

 private:
  // This iterates in init order: legacy first, then array in memory order.
  // The std::ranges::reverse_view adapter produces fini order.
  class Iterator {
   public:
    using difference_type = Array::iterator::difference_type;
    using value_type = std::pair<Addr, bool>;
    using iterator_concept = std::random_access_iterator_tag;

    constexpr value_type operator*() const {
      auto i = idx_;
      if (legacy_ != 0) {
        if (i == 0) {
          return {legacy_, false};
        }
        --i;
      }
      return {array_[i], relocated_};
    }

    constexpr value_type operator[](difference_type n) const { return *(*this + n); }

    constexpr auto operator<=>(const Iterator& other) const {
      AssertOtherIsBrother(other);
      return idx_ <=> other.idx_;
    }

    constexpr bool operator==(const Iterator& other) const {
      AssertOtherIsBrother(other);
      return idx_ == other.idx_;
    }

    constexpr Iterator& operator+=(difference_type n) {
      assert(n < 0 ? (static_cast<difference_type>(idx_) >= -n)
                   : (static_cast<difference_type>(size() - idx_) >= n));
      idx_ += n;
      return *this;
    }

    constexpr Iterator& operator-=(difference_type n) {
      *this += -n;
      return *this;
    }

    constexpr Iterator& operator++() {  // prefix
      *this += 1;
      return *this;
    }

    constexpr Iterator operator++(int) {  // postfix
      auto it = *this;
      ++*this;
      return it;
    }

    constexpr Iterator& operator--() {  // prefix
      *this -= 1;
      return *this;
    }

    constexpr Iterator operator--(int) {  // postfix
      auto it = *this;
      --*this;
      return it;
    }

    constexpr Iterator operator+(difference_type n) const { return Iterator{*this} += n; }

    friend constexpr Iterator operator+(difference_type n, Iterator it) { return it += n; }

    constexpr Iterator operator-(difference_type n) const { return Iterator{*this} -= n; }

    constexpr difference_type operator-(const Iterator& it) const {
      AssertOtherIsBrother(it);
      return static_cast<difference_type>(idx_ - it.idx_);
    }

   private:
    friend InitFiniInfo;

    constexpr Array::size_type size() const { return array_.size() + (legacy_ != 0 ? 1 : 0); }

    constexpr void AssertOtherIsBrother(const Iterator& other) const {
      assert(array_.data() == other.array_.data());
      assert(array_.size() == other.array_.size());
      assert(legacy_ == other.legacy_);
      assert(relocated_ == other.relocated_);
    }

    Array array_;
    Array::size_type idx_ = 0;
    Addr legacy_ = 0;
    bool relocated_ = false;
  };
  AbiSpan<const Addr, std::dynamic_extent, Elf, AbiTraits> array_;
  Addr legacy_ = 0;

 public:
  // <lib/ld/remote-abi-transcriber.h> introspection API.  These aliases must
  // be public, but can't be defined lexically before the private: section that
  // declares the members; so this special public: section is at the end.

  using AbiLocal = InitFiniInfo<Elf, LocalAbiTraits>;

  template <template <class...> class Template>
  using AbiBases = Template<>;

  template <template <auto...> class Template>
  using AbiMembers = Template<&InitFiniInfo::array_, &InitFiniInfo::legacy_>;
};

}  // namespace elfldltl

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INIT_FINI_H_
