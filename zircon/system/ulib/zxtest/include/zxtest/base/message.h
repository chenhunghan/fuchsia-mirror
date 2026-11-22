// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef ZXTEST_BASE_MESSAGE_H_
#define ZXTEST_BASE_MESSAGE_H_

#include <zircon/compiler.h>
#include <zircon/status.h>

#include <concepts>
#include <cstdarg>
#include <iterator>
#include <span>
#include <string>
#include <string_view>
#include <tuple>
#include <type_traits>

#include <zxtest/base/types.h>

namespace zxtest {

namespace internal {

// A contiguous character range with a known size, such as std::string_view,
// std::string, std::span<const char>, or a char array.
template <typename T>
concept SizedCharRange = requires(T&& str) {
  { std::data(str) } -> std::convertible_to<const char*>;
  { std::size(str) } -> std::convertible_to<size_t>;
};

// Strings are compared like C strings, so a sized range ends at its first NUL
// character (if any) but never reads past its size.
constexpr std::string_view ToStringView(SizedCharRange auto&& str) {
  std::string_view sv{std::data(str), std::size(str)};
  return sv.substr(0, sv.find('\0'));
}

// C strings, including nullptr and other types that convert to const char*.
template <std::convertible_to<const char*> T>
  requires(!SizedCharRange<T>)
constexpr std::string_view ToStringView(T&& str) {
  const char* cstr = std::forward<T>(str);
  return cstr ? std::string_view{cstr} : std::string_view{};
}

// Other types that convert (even explicitly) to std::string_view.
template <typename T>
  requires(!SizedCharRange<T> && !std::convertible_to<T, const char*> &&
           std::constructible_from<std::string_view, T>)
constexpr std::string_view ToStringView(T&& str) {
  return ToStringView(std::string_view(std::forward<T>(str)));
}

}  // namespace internal

// Helper class for handling the error information, plus some logic for printing the correct error
// messages.
class Message {
 public:
  Message() = delete;
  Message(std::string_view desc, const SourceLocation& location);
  Message(const Message&) = delete;
  Message(Message&&) noexcept;
  ~Message();

  Message& operator=(const Message&) = delete;
  Message& operator=(Message&&) = delete;

  // Returns the position at which the message was sent.
  const SourceLocation& location() const { return location_; }

  // Returns the text of the message.
  const std::string& text() const { return text_; }

 private:
  std::string text_;

  SourceLocation location_;
};

// Helper functions used on assertion reporting contexts.
namespace internal {
// Returns a string with the Hex representation of the contents of the buffer pointed by ptr. If
// |ptr| is nullptr, returns "<nullptr>". If |size| is 0 returns <empty>.
std::string ToHex(const void* ptr, size_t size);

// It's not necessarily safe to do pointer arithmetic on volatiles because of alignment issues, so
// just print whether the pointer is nullptr/empty/normal.
std::string_view PrintVolatile(volatile const void* ptr, size_t size);

std::string FormatDescriptionV(std::string_view desc, const char* fmt, va_list args);
std::string FormatDescription(std::string_view desc, const char* fmt, ...) __PRINTFLIKE(2, 3);
std::string PrintAsTuple(std::span<const std::string> tuple);

}  // namespace internal

// Specializations exist for primitive types, pointers and |std::string|.
template <typename T>
std::string PrintValue(const T& value) {
  // TODO(gevalentino): By default generate a hex representation of the memory contents of value.
  return internal::ToHex(&value, sizeof(value));
}

template <typename T>
std::string PrintValue(volatile const T& value) {
  return std::string{internal::PrintVolatile(&value, sizeof(value))};
}

// Explicit overloads for generic and char pointers, integers, floating point, and strings.
std::string PrintValue(int32_t value);
std::string PrintValue(uint32_t value);
std::string PrintValue(int64_t value);
std::string PrintValue(uint64_t value);
std::string PrintValue(float value);
std::string PrintValue(double value);
std::string PrintValue(const char* value);
std::string PrintValue(const void* value);
inline std::string PrintValue(std::string str) { return str; }
inline std::string PrintValue(std::string_view str) { return std::string{str}; }

// All other pointers are treated like a void pointer.
inline std::string PrintValue(const auto* value) {
  return PrintValue(static_cast<const void*>(value));
}

// Print a string form of the status, can't be a specialization of PrintValue because zx_status_t is
// a uint32_t.
std::string PrintStatus(zx_status_t status);

// For tuples, recursively print the individual components.
template <typename... Ts>
inline std::string PrintValue(const std::tuple<Ts...>& value) {
  if constexpr (sizeof...(Ts) == 0) {
    return "{ }";
  } else {
    return std::apply(
        []<typename... E>(E&&... elems) -> std::string {
          const std::string strings[]{PrintValue(std::forward<E>(elems))...};
          return internal::PrintAsTuple(strings);
        },
        value);
  }
}

template <typename StringTypeA, typename StringTypeB>
inline bool StrCmp(StringTypeA&& actual, StringTypeB&& expected) {
  std::string_view actual_sv = internal::ToStringView(actual);
  std::string_view expected_sv = internal::ToStringView(expected);
  return actual_sv == expected_sv;
}

template <typename StringTypeA, typename StringTypeB>
inline bool StrContain(StringTypeA&& str, StringTypeB&& substr) {
  std::string_view str_view = internal::ToStringView(str);
  std::string_view substr_view = internal::ToStringView(substr);
  return str_view.contains(substr_view);
}

}  // namespace zxtest

#endif  // ZXTEST_BASE_MESSAGE_H_
