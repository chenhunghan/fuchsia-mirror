// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/boot-shim/boot-properties.h"

#include <lib/boot-options/word-view.h>

#include <cassert>
#include <ranges>
#include <string_view>

namespace boot_shim {
namespace {

constexpr bool MissingProperty(std::optional<std::string_view> value) { return !value; }

constexpr bool MissingProperty(std::string_view value) { return value.empty(); }

template <class Value>
void FromBootConfig(const linux_boot_config::LinuxBootConfig& linux_boot_config,
                    std::span<std::string_view> keys, std::span<Value> result) {
  assert(keys.size() == result.size());
  auto on_item = [keys, result](const linux_boot_config::Key& config_key,
                                const linux_boot_config::Value& config_value) {
    for (auto&& [key, value] : std::views::zip(keys, result)) {
      if (config_key == key) {
        switch (config_value.action) {
          case linux_boot_config::Value::Action::kDefine:
          case linux_boot_config::Value::Action::kOverride:
            value = config_value.value;
            break;
          default:
            if (MissingProperty(value)) {
              value = config_value.value;
            }
            break;
        }
        break;
      }
    }
  };
  std::ignore = linux_boot_config.Parse(on_item);
}

// Strip surrounding quotes if present.
std::string_view StripQuotes(std::string_view word) {
  return (word.size() >= 2 && word.starts_with('"') && word.ends_with('"'))
             ? word.substr(1, word.size() - 2)
             : word;
}

// All the flags are initially false, and only set and checked inside this
// function.  They track whether a non-missing value for each key was from the
// boot-config and so should be kept, or was from the cmdline and so should be
// overridden by a later redundant cmdline word.
template <class Value>
void FromCmdline(std::string_view cmdline, std::span<std::string_view> keys,
                 std::span<Value> result, std::span<bool> flags) {
  for (std::string_view word : WordView(cmdline)) {
    bool any_missing = false;
    for (auto&& [key, value, flag] : std::views::zip(keys, result, flags)) {
      const bool missing = flag || MissingProperty(value);
      any_missing = any_missing || missing;
      if (missing && word.starts_with(key)) {
        word.remove_prefix(key.size());
        if (word.empty() || word.front() == '=') {
          if (!word.empty()) {
            word.remove_prefix(1);
            word = StripQuotes(word);
          }

          // In the case of multiple entries the last one wins, so we continue
          // iterating.
          value = word;

          // Mark that the pending value came from the cmdline rather than the
          // boot-config and so should be overridden.
          flag = true;
        }
      }
    }
    if (!any_missing) {
      // Nothing more to find in the cmdline.
      break;
    }
  }
}

}  // namespace

zx::result<std::string_view> BootProperties::GetProperty(std::string_view key) const {
  std::optional<std::string_view> result;
  bool flag = false;
  GetPropertiesImpl(std::span{&key, 1}, std::span{&result, 1}, std::span{&flag, 1});
  if (result.has_value()) {
    return zx::ok(*result);
  }
  return zx::error(ZX_ERR_NOT_FOUND);
}

void BootProperties::GetPropertiesImpl(std::span<std::string_view> keys,
                                       std::span<std::optional<std::string_view>> results,
                                       std::span<bool> flags) const {
  FromBootConfig(bootconfig_, keys, results);
  FromCmdline(cmdline_, keys, results, flags);
}

void BootProperties::GetPropertiesOrEmptyImpl(std::span<std::string_view> keys,
                                              std::span<std::string_view> results,
                                              std::span<bool> flags) const {
  FromBootConfig(bootconfig_, keys, results);
  FromCmdline(cmdline_, keys, results, flags);
}

zx::result<std::string_view> BootProperties::JoinProperty(std::string_view key,
                                                          std::span<char> buffer) const {
  std::optional<size_t> size;

  EnumerateProperty(key, [&](std::string_view val, linux_boot_config::Value::Action action) {
    // An append extends the current value after a ','. Anything else (a define or override)
    // replaces it, so writing starts over at the beginning of the buffer.
    if (size.has_value() && action == linux_boot_config::Value::Action::kAppend) {
      if (*size < buffer.size()) {
        buffer[(*size)++] = ',';
      }
    } else {
      size = 0;
    }
    // Copies as much as fits; the rest is truncated.
    *size += val.copy(buffer.data() + *size, buffer.size() - *size);
  });

  if (size.has_value()) {
    return zx::ok(std::string_view(buffer.data(), *size));
  }
  return zx::error(ZX_ERR_NOT_FOUND);
}

zx::result<std::string_view> BootProperties::GetFromCmdline(std::string_view key) const {
  std::optional<std::string_view> result;
  bool flag = false;
  FromCmdline(cmdline_, std::span{&key, 1}, std::span{&result, 1}, std::span{&flag, 1});
  if (!result.has_value()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  return zx::ok(*result);
}

}  // namespace boot_shim
