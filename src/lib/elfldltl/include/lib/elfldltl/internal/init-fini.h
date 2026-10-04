// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INTERNAL_INIT_FINI_H_
#define SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INTERNAL_INIT_FINI_H_

#include <concepts>
#include <ranges>
#include <type_traits>

namespace elfldltl::internal {

// This is instantiated with a function type solely to be partially
// specialized as the means of getting that type's argument list into a
// parameter pack.  By having the precise argument signature unpacked as the
// call signature of operator(), arguments are handled at call sites entirely
// like arguments in a call site using the function pointer type directly.
// If instead it just defined operator()(auto&&...), then all the things like
// interpreting literals as particular integer types, or {...} as an implicit
// constructor for the given type, would not work the same way.
template <typename F>
  requires std::is_function_v<F>
struct SignatureOf;

template <typename R, typename... Args>
struct SignatureOf<R(Args...)> {
  // Instantiate Template with the argument types.
  template <template <typename...> class Template>
  using Arguments = Template<Args...>;

  // Instantiate Template with the return type and argument types.
  template <template <typename, typename...> class Template>
  using Invoke = Template<R, Args...>;

  // Just the return type, same as std::invoke_result_t<F, Args...>.
  using Result = R;
  static_assert(std::same_as<Result, std::invoke_result_t<R(Args...), Args...>>);
};

// Certain attributes are importantly part of the function type, and thus
// part of the function pointer type.  This matters a lot for attributes that
// control the calling convention and so forth.  So the actual call is always
// made via the function pointer type F that the explicit template parameter
// specified.  F needs to be decomposed into R(Args...) so that Args... is
// available as a parameter pack.  Two issues arise when F has attributes:
//
//  * F is not the same as R(Args...), it's actually R(Args...) [[...]] so
//    the operator()(Args...) implementation's actual call better use `F*`
//    and not `(R*)(Args...)`.  This is not an issue here because the inner
//    template is instantiated on the range type whose range_value_t is the
//    correct `F*`.  The `R(Args...)` _type_ is not used anywhere, just the
//    `Args...` _pack_.
//
//  * Partial specialization matching doesn't have a way to ignore the
//    attributes.  The partial specialization `R(Args...)` does _not_ match
//    some `R(Args...) [[Attrs...]]` and there's no way to spell that kind of
//    matching generically.  Attributes can be stripped via partial template
//    specialization, by only individual known ones.  (The same is true of
//    qualifiers, but the set of qualifiers is known and fixed; and the
//    tedium has been implemented already with std::remove_cv_t and such.)
//
// Unfortunately, thus each attribute that might be part of an F type must be
// handled here individually.  If the compiler ignores an attribute it
// doesn't grok as meaningful for a function type, then it will diagnose an
// error for a partial specialization using it as being redundant.
#ifdef __clang__
template <typename R, typename... Args>
struct SignatureOf<R(Args...) [[clang::cfi_unchecked_callee]]> : public SignatureOf<R(Args...)> {};
#endif

// Instantiated via SignatureOf<F>::Arguments.
template <typename... Args>
struct CallableWith {
  // Range adaptor adding callability with those argument types.
  template <std::ranges::input_range Range>
    requires std::invocable<std::ranges::range_value_t<Range>, Args...>
  struct CallableRange : public Range {
    constexpr CallableRange() = default;
    constexpr CallableRange(const CallableRange&) = default;
    constexpr CallableRange(CallableRange&&) noexcept = default;

    constexpr explicit CallableRange(std::convertible_to<Range> auto&& range)
        : Range(std::forward<decltype(range)>(range)) {}

    constexpr CallableRange& operator=(const CallableRange&) = default;
    constexpr CallableRange& operator=(CallableRange&&) noexcept = default;

    constexpr void operator()(this auto&& funcs, Args... args) {
      static_assert(std::default_initializable<CallableRange> == std::default_initializable<Range>);
      static_assert(std::copyable<CallableRange> == std::copyable<Range>);
      static_assert(std::movable<CallableRange> == std::movable<Range>);
      for (auto&& func : funcs) {
        func(args...);
      }
    }
  };
};

// Range adaptor adding callability with the argument signature of F.
template <typename F, std::ranges::input_range R>
  requires std::is_function_v<F>
using CallableRange = SignatureOf<F>::template Arguments<CallableWith>::template CallableRange<R>;

// Used to perform std::invocable<...> via CallableWith.
template <typename T>
struct InvocableOn {
  template <typename... Args>
  using type = std::is_invocable<T, Args...>;
};

}  // namespace elfldltl::internal

#endif  // SRC_LIB_ELFLDLTL_INCLUDE_LIB_ELFLDLTL_INTERNAL_INIT_FINI_H_
