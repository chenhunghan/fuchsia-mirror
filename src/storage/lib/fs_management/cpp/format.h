// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FORMAT_H_
#define SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FORMAT_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <memory>
#include <string>
#include <string_view>

namespace fs_management {

constexpr std::string_view kBlobfsComponentUrl = "#meta/blobfs.cm";
constexpr std::string_view kFxfsComponentUrl = "#meta/fxfs.cm";
constexpr std::string_view kMinfsComponentUrl = "#meta/minfs.cm";
constexpr std::string_view kF2fsComponentUrl = "#meta/f2fs.cm";
constexpr std::string_view kFatfsComponentUrl = "#meta/fatfs.cm";
constexpr std::string_view kFvmComponentUrl = "#meta/fvm2.cm";

enum DiskFormat {
  kDiskFormatUnknown = 0,
  kDiskFormatGpt = 1,
  kDiskFormatMbr = 2,
  kDiskFormatMinfs = 3,
  kDiskFormatFat = 4,
  kDiskFormatBlobfs = 5,
  kDiskFormatFvm = 6,
  kDiskFormatZxcrypt = 7,
  kDiskFormatFactoryfs = 8,
  kDiskFormatBlockVerity = 9,
  kDiskFormatVbmeta = 10,
  kDiskFormatFxfs = 11,
  kDiskFormatF2fs = 12,
  kDiskFormatNandBroker = 13,
  kDiskFormatCount,
};

std::string_view DiskFormatString(DiskFormat fs_type);

// Get the component url for the disk format, if it's known. If it's not known (i.e. the format
// doesn't run as a component), this returns an empty string.
std::string_view DiskFormatComponentUrl(DiskFormat fs_type);

class __EXPORT CustomDiskFormat {
 public:
  static DiskFormat Register(std::unique_ptr<CustomDiskFormat> format);
  static const CustomDiskFormat* Get(DiskFormat);

  CustomDiskFormat(std::string name, std::string_view binary_path,
                   std::string_view component_url = "")
      : name_(std::move(name)), binary_path_(binary_path), component_url_(component_url) {}
  CustomDiskFormat(CustomDiskFormat&&) = default;

  const std::string& name() const { return name_; }
  const std::string& binary_path() const { return binary_path_; }
  const std::string& url() const { return component_url_; }

 private:
  std::string name_;
  std::string binary_path_;
  std::string component_url_;
};

}  // namespace fs_management

#endif  // SRC_STORAGE_LIB_FS_MANAGEMENT_CPP_FORMAT_H_
