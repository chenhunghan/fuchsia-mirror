// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOT_PROPERTIES_H_
#define ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOT_PROPERTIES_H_

#include <lib/linux-boot-config/linux-boot-config.h>
#include <lib/zx/result.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <array>
#include <optional>
#include <span>
#include <string_view>

namespace boot_shim {

class BootProperties {
 public:
  explicit BootProperties(std::string_view cmdline,
                          linux_boot_config::LinuxBootConfig bootconfig = {})
      : cmdline_(cmdline), bootconfig_(bootconfig) {}

  // Extracts a property value (e.g. "kernel.param", "driver.option").
  // Precedence: Bootconfig first, then command line.
  zx::result<std::string_view> GetProperty(std::string_view key) const;

  // Extract multiple properties at once.  This is not only a shorthand for
  // calling GetProperty N times conveniently, but is more efficient.  e.g.
  // ```
  // auto [a, b, c] = bp.GetProperties("a", "b", "c");
  // if (a) { ... }
  // if (b) { ... }
  // if (c) { ... }
  // ```
  auto GetProperties(std::convertible_to<std::string_view> auto&&... keys)
      -> std::array<std::optional<std::string_view>, sizeof...(keys)> {
    std::array<std::string_view, sizeof...(keys)> keys_array = {keys...};
    std::array<std::optional<std::string_view>, keys_array.size()> result;
    std::array<bool, result.size()> flags{};
    GetPropertiesImpl(keys_array, result, flags);
    return result;
  }

  // This is the same, except it yields for each key just a std::string_view
  // that's "" for a missing property, not std::optional<std::string_view> that
  // distinguishes std::nullopt (missing) from "" (present with empty value).
  auto GetPropertiesOrEmpty(std::convertible_to<std::string_view> auto&&... keys)
      -> std::array<std::string_view, sizeof...(keys)> {
    std::array<std::string_view, sizeof...(keys)> keys_array = {keys...};
    std::array<std::string_view, keys_array.size()> result;
    std::array<bool, result.size()> flags{};
    GetPropertiesOrEmptyImpl(keys_array, result, flags);
    return result;
  }

  // Joins all values of a property with ',' into buffer and returns a view of
  // the result. Precedence is the same as GetProperty(), and a define or
  // override resets the value.
  //
  // BootConfig may contain unquoted values like "key:=a,b,c", which BootConfig
  // syntax parses as an array of values. Joining the elements recovers the full
  // value ("a,b,c").
  //
  // Values that do not fit in buffer are truncated.
  zx::result<std::string_view> JoinProperty(std::string_view key, std::span<char> buffer) const;

  // Invokes a callback for all matching property definitions/appends for a key.
  //
  // BootConfig is small enough that it's worthwhile to parse the entirety for
  // each key we care about to keep the API for each given item
  // straightforward. We could refactor this to scan once, but the ergonomics
  // change to Item classes wouldn't be worth it.
  void EnumerateProperty(std::string_view key, auto&& cb) const {
    bool found_in_bootconfig = false;
    std::ignore = bootconfig_.Parse(
        [&](const linux_boot_config::Key& k, const linux_boot_config::Value& val) {
          if (k == key) {
            found_in_bootconfig = true;
            cb(val.value, val.action);
          }
        });
    if (!found_in_bootconfig) {
      if (zx::result<std::string_view> res = GetFromCmdline(key); res.is_ok()) {
        cb(*res, linux_boot_config::Value::Action::kDefine);
      }
    }
  }

 private:
  void GetPropertiesImpl(std::span<std::string_view> keys,
                         std::span<std::optional<std::string_view>> results,
                         std::span<bool> flags) const;
  void GetPropertiesOrEmptyImpl(std::span<std::string_view> keys,
                                std::span<std::string_view> results, std::span<bool> flags) const;

  zx::result<std::string_view> GetFromCmdline(std::string_view key) const;

  std::string_view cmdline_;
  linux_boot_config::LinuxBootConfig bootconfig_;
};

}  // namespace boot_shim

#endif  // ZIRCON_KERNEL_PHYS_LIB_BOOT_SHIM_INCLUDE_LIB_BOOT_SHIM_BOOT_PROPERTIES_H_
