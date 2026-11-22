// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <zircon/assert.h>

#include <cstdarg>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <format>
#include <span>

#include <zxtest/base/message.h>

namespace zxtest {

Message::Message(std::string_view desc, const SourceLocation& location)
    : text_(desc), location_(location) {}

Message::Message(Message&& other) noexcept = default;
Message::~Message() = default;

namespace internal {

std::string ToHex(const void* ptr, size_t size) {
  if (size == 0) {
    return "<empty>";
  }

  if (ptr == nullptr) {
    return "<nullptr>";
  }

  constexpr std::string_view kHexDigits = "0123456789ABCDEF";
  std::span bytes{static_cast<const uint8_t*>(ptr), size};
  std::string result;
  result.reserve((bytes.size() * 3) - 1);
  for (uint8_t byte : bytes.subspan(0, bytes.size() - 1)) {
    result += kHexDigits[(byte >> 4) & 0xf];
    result += kHexDigits[(byte >> 0) & 0xf];
    result += ' ';
  }
  result += kHexDigits[(bytes.back() >> 4) & 0xf];
  result += kHexDigits[(bytes.back() >> 0) & 0xf];

  return result;
}

std::string_view PrintVolatile(volatile const void* ptr, size_t size) {
  if (size == 0) {
    return "<empty>";
  }

  if (ptr == nullptr) {
    return "<nullptr>";
  }

  return "<ptr>";
}

std::string PrintAsTuple(std::span<const std::string> tuple) {
  if (tuple.empty()) {
    return "{ }";
  }

  size_t total_size = 4 + ((tuple.size() - 1) * 2);
  for (const std::string& elem : tuple) {
    total_size += elem.size();
  }

  std::string result;
  result.reserve(total_size);
  result += "{ ";
  for (const std::string& elem : tuple.subspan(0, tuple.size() - 1)) {
    result += elem;
    result += ", ";
  }
  result += tuple.back();
  result += " }";
  return result;
}

std::string FormatDescriptionV(std::string_view desc, const char* fmt, va_list args) {
  if (fmt[0] == ' ' && fmt[1] == '\0') {
    std::string result;
    result.reserve(desc.size() + 1);
    result.append(desc);
    result.push_back(' ');
    return result;
  }
  char* malloc_str = nullptr;
  int len = vasprintf(&malloc_str, fmt, args);
  ZX_ASSERT(len >= 0 && malloc_str != nullptr);
  std::string result;
  result.reserve(desc.size() + static_cast<size_t>(len));
  result.append(desc);
  result.append(malloc_str, static_cast<size_t>(len));
  free(malloc_str);
  return result;
}

std::string FormatDescription(std::string_view desc, const char* fmt, ...) {
  va_list args;
  va_start(args, fmt);
  std::string result = FormatDescriptionV(desc, fmt, args);
  va_end(args);
  return result;
}

}  // namespace internal

std::string PrintValue(int32_t value) { return std::to_string(value); }

std::string PrintValue(uint32_t value) { return std::to_string(value); }

std::string PrintValue(int64_t value) { return std::to_string(value); }

std::string PrintValue(uint64_t value) { return std::to_string(value); }

std::string PrintValue(float value) { return std::to_string(value); }

std::string PrintValue(double value) { return std::to_string(value); }

std::string PrintValue(const char* value) {
  if (value == nullptr) {
    return "<nullptr>";
  }
  return std::string{value};
}

std::string PrintValue(const void* value) {
  if (value == nullptr) {
    return "<nullptr>";
  }
  return std::format("{}", value);
}

std::string PrintStatus(zx_status_t status) {
#ifdef __Fuchsia__
  return std::format("{}({})", zx_status_get_string(status), status);
#else
  return std::to_string(status);
#endif
}

}  // namespace zxtest
