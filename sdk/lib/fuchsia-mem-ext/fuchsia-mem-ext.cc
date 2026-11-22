// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fuchsia-mem-ext/fuchsia-mem-ext.h>

namespace fuchsia_mem_ext {

constexpr size_t kDefaultSizeThreshold = 16 * 1024;

namespace internal {

// Adapters that build the HLCPP and natural representations of fuchsia.mem/Data
// so that the creation logic below is shared between the two bindings.
struct HlcppData {
  using Type = fuchsia::mem::Data;
  static Type WithBytes(std::vector<uint8_t> bytes) {
    return fuchsia::mem::Data::WithBytes(std::move(bytes));
  }
  static Type WithBuffer(zx::vmo vmo, uint64_t size) {
    return fuchsia::mem::Data::WithBuffer({std::move(vmo), size});
  }
};

struct NaturalData {
  using Type = fuchsia_mem::Data;
  static Type WithBytes(std::vector<uint8_t> bytes) {
    return fuchsia_mem::Data::WithBytes(std::move(bytes));
  }
  static Type WithBuffer(zx::vmo vmo, uint64_t size) {
    return fuchsia_mem::Data::WithBuffer(fuchsia_mem::Buffer(std::move(vmo), size));
  }
};

template <typename DataAdapter>
typename DataAdapter::Type CreateInline(cpp20::span<const uint8_t> data) {
  std::vector<uint8_t> bytes(data.size());
  std::memcpy(bytes.data(), data.data(), data.size());
  return DataAdapter::WithBytes(std::move(bytes));
}

template <typename DataAdapter>
typename DataAdapter::Type CreateInline(std::vector<uint8_t> data) {
  return DataAdapter::WithBytes(std::move(data));
}

template <typename DataAdapter, typename T>
zx::result<typename DataAdapter::Type> Create(T data, size_t size_threshold,
                                              std::string_view vmo_name) {
  if (size_threshold > ZX_CHANNEL_MAX_MSG_BYTES) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }
  if (vmo_name.length() > ZX_MAX_NAME_LEN) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  if (data.size() <= size_threshold) {
    return zx::ok(CreateInline<DataAdapter>(std::move(data)));
  }

  zx::vmo vmo;
  uint32_t options = 0;
  zx_status_t status = zx::vmo::create(data.size(), options, &vmo);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  status = vmo.write(data.data(), 0u, data.size());
  if (status != ZX_OK) {
    return zx::error(status);
  }

  if (!vmo_name.empty()) {
    status = vmo.set_property(ZX_PROP_NAME, vmo_name.data(), vmo_name.length());
    if (status != ZX_OK) {
      return zx::error(status);
    }
  }

  return zx::ok(DataAdapter::WithBuffer(std::move(vmo), data.size()));
}

}  // namespace internal

zx::result<fuchsia::mem::Data> CreateWithData(cpp20::span<const uint8_t> data,
                                              std::string_view vmo_name) {
  return CreateWithData(data, kDefaultSizeThreshold, vmo_name);
}

zx::result<fuchsia::mem::Data> CreateWithData(std::vector<uint8_t> data,
                                              std::string_view vmo_name) {
  return CreateWithData(std::move(data), kDefaultSizeThreshold, vmo_name);
}

zx::result<fuchsia::mem::Data> CreateWithData(cpp20::span<const uint8_t> data,
                                              size_t size_threshold, std::string_view vmo_name) {
  return internal::Create<internal::HlcppData>(data, size_threshold, vmo_name);
}

zx::result<fuchsia::mem::Data> CreateWithData(std::vector<uint8_t> data, size_t size_threshold,
                                              std::string_view vmo_name) {
  return internal::Create<internal::HlcppData>(std::move(data), size_threshold, vmo_name);
}

zx::result<fuchsia_mem::Data> CreateNaturalWithData(cpp20::span<const uint8_t> data,
                                                    std::string_view vmo_name) {
  return CreateNaturalWithData(data, kDefaultSizeThreshold, vmo_name);
}

zx::result<fuchsia_mem::Data> CreateNaturalWithData(std::vector<uint8_t> data,
                                                    std::string_view vmo_name) {
  return CreateNaturalWithData(std::move(data), kDefaultSizeThreshold, vmo_name);
}

zx::result<fuchsia_mem::Data> CreateNaturalWithData(cpp20::span<const uint8_t> data,
                                                    size_t size_threshold,
                                                    std::string_view vmo_name) {
  return internal::Create<internal::NaturalData>(data, size_threshold, vmo_name);
}

zx::result<fuchsia_mem::Data> CreateNaturalWithData(std::vector<uint8_t> data,
                                                    size_t size_threshold,
                                                    std::string_view vmo_name) {
  return internal::Create<internal::NaturalData>(std::move(data), size_threshold, vmo_name);
}

zx::result<std::vector<uint8_t>> ExtractData(fuchsia::mem::Data data) {
  switch (data.Which()) {
    case fuchsia::mem::Data::Tag::kBytes: {
      return zx::ok(std::move(data).bytes());
    }
    case fuchsia::mem::Data::Tag::kBuffer: {
      const size_t data_size = data.buffer().size;
      std::vector<uint8_t> ret(data_size);
      zx_status_t status = data.buffer().vmo.read(ret.data(), 0u, data_size);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      return zx::ok(std::move(ret));
    }
    case fuchsia::mem::Data::kUnknown:
    case fuchsia::mem::Data::Invalid:
      return zx::error(ZX_ERR_BAD_STATE);
  }
}

zx::result<std::vector<uint8_t>> ExtractData(fuchsia_mem::Data data) {
  switch (data.Which()) {
    case fuchsia_mem::Data::Tag::kBytes: {
      return zx::ok(std::move(data.bytes().value()));
    }
    case fuchsia_mem::Data::Tag::kBuffer: {
      const fuchsia_mem::Buffer& buffer = data.buffer().value();
      const size_t data_size = buffer.size();
      std::vector<uint8_t> ret(data_size);
      zx_status_t status = buffer.vmo().read(ret.data(), 0u, data_size);
      if (status != ZX_OK) {
        return zx::error(status);
      }
      return zx::ok(std::move(ret));
    }
    default:
      return zx::error(ZX_ERR_BAD_STATE);
  }
}

}  // namespace fuchsia_mem_ext
