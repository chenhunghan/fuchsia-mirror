// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/zxdump_preparer.h"

#include <fcntl.h>
#include <stdlib.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstddef>
#include <cstring>
#include <filesystem>
#include <memory>
#include <system_error>
#include <vector>

#include <llvm/BinaryFormat/ELF.h>
#include <zstd/zstd.h>

#include "src/lib/files/file_descriptor.h"

namespace zxdb {

namespace {

constexpr size_t kElfMagicSize = llvm::ELF::EI_MAG3 + 1;
constexpr size_t kElfTypeOffset = offsetof(llvm::ELF::Elf64_Ehdr, e_type);
constexpr size_t kProbeSize = kElfTypeOffset + sizeof(llvm::ELF::Elf64_Half);
static_assert(kProbeSize == 18);

constexpr std::array<uint8_t, 8> kArchiveMagic = {'!', '<', 'a', 'r', 'c', 'h', '>', '\n'};

bool StartsWithElfMagic(std::span<const uint8_t> data) {
  return data.size() >= kElfMagicSize &&
         std::memcmp(data.data(), llvm::ELF::ElfMagic, kElfMagicSize) == 0;
}

bool StartsWithArchiveMagic(std::span<const uint8_t> data) {
  return data.size() >= kArchiveMagic.size() &&
         std::memcmp(data.data(), kArchiveMagic.data(), kArchiveMagic.size()) == 0;
}

bool StartsWithZstdMagic(std::span<const uint8_t> data) {
  uint32_t magic = 0;
  if (data.size() < sizeof(magic)) {
    return false;
  }
  std::memcpy(&magic, data.data(), sizeof(magic));
  return magic == ZSTD_MAGICNUMBER;
}

Err ErrnoError(int err, const std::string& what) { return Err(what + ": " + strerror(err)); }

ErrOr<DumpFormat> ProbeDumpFd(int fd, const std::string& path) {
  std::array<uint8_t, kProbeSize> header = {};
  size_t total = 0;
  while (total < header.size()) {
    ssize_t n = pread(fd, header.data() + total, header.size() - total, static_cast<off_t>(total));
    if (n < 0) {
      if (errno == EINTR) {
        continue;
      }
      const int err = errno;
      return ErrnoError(err, "Could not read " + path);
    }
    if (n == 0) {
      break;
    }
    total += static_cast<size_t>(n);
  }
  return DetectDumpFormat(std::span<const uint8_t>(header.data(), total));
}

// Creates an unlinked temporary file and decompresses |input_fd| into it.
ErrOr<fbl::unique_fd> DecompressZstdToTempFd(int input_fd, const std::string& input_path) {
  std::error_code ec;
  std::filesystem::path temp_dir = std::filesystem::temp_directory_path(ec);
  if (ec) {
    temp_dir = "/tmp";
  }
  std::string pattern = (temp_dir / "zxdump.XXXXXX").string();
  fbl::unique_fd temp_fd(mkostemp(pattern.data(), O_CLOEXEC));  // NOLINT(bugprone-signed-bitwise)
  if (!temp_fd) {
    const int err = errno;
    return ErrnoError(err, "Could not create temporary file " + pattern);
  }
  // Unlink immediately so the OS kernel reclaims the blocks when the descriptor closes.
  unlink(pattern.c_str());

  std::unique_ptr<ZSTD_DStream, decltype(&ZSTD_freeDStream)> stream(ZSTD_createDStream(),
                                                                    &ZSTD_freeDStream);
  if (!stream) {
    return Err("Could not create a zstd decompression stream.");
  }
  ZSTD_initDStream(stream.get());

  std::vector<uint8_t> in_buffer(ZSTD_DStreamInSize());
  std::vector<uint8_t> out_buffer(ZSTD_DStreamOutSize());

  // ZSTD_decompressStream returns 0 when a frame is complete. Any other value at end of input
  // means the input is truncated.
  size_t last_result = 0;
  bool read_any = false;
  while (true) {
    ssize_t read = fxl::ReadFileDescriptor(input_fd, reinterpret_cast<char*>(in_buffer.data()),
                                           static_cast<ssize_t>(in_buffer.size()));
    if (read < 0) {
      const int err = errno;
      return ErrnoError(err, "Could not read " + input_path);
    }
    if (read == 0) {
      break;
    }
    read_any = true;

    ZSTD_inBuffer in = {
        .src = in_buffer.data(),
        .size = static_cast<size_t>(read),
        .pos = 0,
    };
    bool output_full = false;
    do {
      ZSTD_outBuffer out = {
          .dst = out_buffer.data(),
          .size = out_buffer.size(),
          .pos = 0,
      };
      last_result = ZSTD_decompressStream(stream.get(), &out, &in);
      if (ZSTD_isError(last_result)) {
        return Err(std::string("zstd decompression failed: ") + ZSTD_getErrorName(last_result));
      }
      if (out.pos > 0 &&
          !fxl::WriteFileDescriptor(temp_fd.get(), reinterpret_cast<const char*>(out_buffer.data()),
                                    static_cast<ssize_t>(out.pos))) {
        const int err = errno;
        return ErrnoError(err, "Could not write decompressed dump");
      }
      // A full output buffer means the decoder may still hold data for this input. A result of 0
      // means the frame is complete and fully flushed, so there is nothing left to drain.
      output_full = out.pos == out.size && last_result != 0;
    } while (in.pos < in.size || output_full);
  }

  if (!read_any) {
    return Err("The compressed dump " + input_path + " is empty.");
  }
  if (last_result != 0) {
    return Err("The compressed dump " + input_path + " is truncated.");
  }

  auto decompressed_format = ProbeDumpFd(temp_fd.get(), input_path);
  if (decompressed_format.has_error()) {
    return decompressed_format.err();
  }
  if (decompressed_format.value() != DumpFormat::kElfCore &&
      decompressed_format.value() != DumpFormat::kArchive) {
    return Err("Decompressed dump is not a Zircon core dump or archive: " + input_path);
  }

  if (lseek(temp_fd.get(), 0, SEEK_SET) < 0) {
    const int err = errno;
    return ErrnoError(err, "Could not seek decompressed dump");
  }
  return temp_fd;
}

}  // namespace

DumpFormat DetectDumpFormat(std::span<const uint8_t> header) {
  if (StartsWithZstdMagic(header)) {
    return DumpFormat::kZstd;
  }
  if (StartsWithArchiveMagic(header)) {
    return DumpFormat::kArchive;
  }
  if (StartsWithElfMagic(header) && header.size() >= kProbeSize) {
    const uint16_t b0 = header[kElfTypeOffset];
    const uint16_t b1 = header[kElfTypeOffset + 1];
    uint16_t type = 0;
    if (header[llvm::ELF::EI_DATA] == llvm::ELF::ELFDATA2LSB) {
      type = static_cast<uint16_t>(b0 | static_cast<uint16_t>(b1 << 8u));
    } else if (header[llvm::ELF::EI_DATA] == llvm::ELF::ELFDATA2MSB) {
      type = static_cast<uint16_t>(static_cast<uint16_t>(b0 << 8u) | b1);
    }
    if (type == llvm::ELF::ET_CORE) {
      return DumpFormat::kElfCore;
    }
  }
  return DumpFormat::kUnknown;
}

bool IsZxdumpFile(const std::string& path) {
  fbl::unique_fd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));  // NOLINT(bugprone-signed-bitwise)
  if (!fd) {
    return false;
  }
  auto format = ProbeDumpFd(fd.get(), path);
  return format.ok() && format.value() != DumpFormat::kUnknown;
}

ErrOr<fbl::unique_fd> PrepareDump(const std::string& path) {
  fbl::unique_fd fd(open(path.c_str(), O_RDONLY | O_CLOEXEC));  // NOLINT(bugprone-signed-bitwise)
  if (!fd) {
    const int err = errno;
    return ErrnoError(err, "Could not open " + path);
  }
  auto format = ProbeDumpFd(fd.get(), path);
  if (format.has_error()) {
    return format.err();
  }
  if (format.value() == DumpFormat::kUnknown) {
    return Err("Not a zxdump file: " + path);
  }
  if (format.value() == DumpFormat::kZstd) {
    return DecompressZstdToTempFd(fd.get(), path);
  }
  return fd;
}

}  // namespace zxdb
