// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/storage/lib/fs_management/cpp/format.h"

#include <mutex>
#include <unordered_map>

#include "src/lib/fxl/synchronization/thread_annotations.h"

namespace fs_management {
namespace {

class Registry {
 public:
  DiskFormat Register(std::unique_ptr<CustomDiskFormat> format) {
    std::scoped_lock lock(mutex_);
    int format_id = next_id_++;
    map_[format_id] = std::move(format);
    return static_cast<DiskFormat>(format_id);
  }

  const CustomDiskFormat* Get(DiskFormat format) {
    std::scoped_lock lock(mutex_);
    auto iter = map_.find(format);
    if (iter == map_.end()) {
      return nullptr;
    }
    return iter->second.get();
  }

 private:
  std::mutex mutex_;
  int next_id_ FXL_GUARDED_BY(mutex_) = kDiskFormatCount;
  std::unordered_map<int, std::unique_ptr<CustomDiskFormat>> map_ FXL_GUARDED_BY(mutex_);
};

Registry& GetRegistry() {
  static Registry& registry = *new Registry;
  return registry;
}

}  // namespace

DiskFormat CustomDiskFormat::Register(std::unique_ptr<CustomDiskFormat> format) {
  return GetRegistry().Register(std::move(format));
}

const CustomDiskFormat* CustomDiskFormat::Get(DiskFormat format) {
  return GetRegistry().Get(format);
}

__EXPORT std::string_view DiskFormatString(DiskFormat fs_type) {
  switch (fs_type) {
    case kDiskFormatCount:
    case kDiskFormatUnknown:
      break;
    case kDiskFormatGpt:
      return "gpt";
    case kDiskFormatMbr:
      return "mbr";
    case kDiskFormatMinfs:
      return "minfs";
    case kDiskFormatFat:
      return "fat";
    case kDiskFormatBlobfs:
      return "blobfs";
    case kDiskFormatFvm:
      return "fvm";
    case kDiskFormatZxcrypt:
      return "zxcrypt";
    case kDiskFormatFactoryfs:
      return "factoryfs";
    case kDiskFormatBlockVerity:
      return "block verity";
    case kDiskFormatVbmeta:
      return "vbmeta";
    case kDiskFormatFxfs:
      return "fxfs";
    case kDiskFormatF2fs:
      return "f2fs";
    case kDiskFormatNandBroker:
      return "nand broker";
  }

  auto format = CustomDiskFormat::Get(fs_type);
  if (format == nullptr) {
    return "unknown!";
  }

  return format->name().c_str();
}

__EXPORT std::string_view DiskFormatComponentUrl(DiskFormat fs_type) {
  switch (fs_type) {
    case kDiskFormatBlobfs:
      return kBlobfsComponentUrl;
    case kDiskFormatFxfs:
      return kFxfsComponentUrl;
    case kDiskFormatMinfs:
      return kMinfsComponentUrl;
    case kDiskFormatF2fs:
      return kF2fsComponentUrl;
    case kDiskFormatFat:
      return kFatfsComponentUrl;
    case kDiskFormatFvm:
      return kFvmComponentUrl;
    case kDiskFormatCount:
    case kDiskFormatUnknown:
    case kDiskFormatGpt:
    case kDiskFormatMbr:
    case kDiskFormatZxcrypt:
    case kDiskFormatFactoryfs:
    case kDiskFormatBlockVerity:
    case kDiskFormatVbmeta:
    case kDiskFormatNandBroker:
      break;
  }

  auto format = CustomDiskFormat::Get(fs_type);
  if (format == nullptr) {
    return "";
  }

  return format->url().c_str();
}

}  // namespace fs_management
