// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/lib/files/file.h"

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdint.h>
#include <sys/stat.h>
#include <unistd.h>

#ifdef __Fuchsia__
#include <zircon/availability.h>
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/unsafe.h>
#include <lib/fit/defer.h>
#include <zircon/types.h>
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
#endif  // __Fuchsia__

#define FILE_CREATE_MODE 0666
#define BINARY_MODE 0

#include <fbl/unique_fd.h>

#include "src/lib/files/eintr_wrapper.h"
#include "src/lib/files/file_descriptor.h"
#include "src/lib/files/path.h"

namespace files {
namespace {

enum class UnnamedTempFileResult : uint8_t {
  kOk,
  kError,
  kNotSupported,
};

bool WriteAndSyncFileDescriptor(int fd, std::string_view data) {
  if (data.size() > static_cast<size_t>(SSIZE_MAX)) {
    return false;
  }
  if (!fxl::WriteFileDescriptor(fd, data.data(), static_cast<ssize_t>(data.size()))) {
    return false;
  }
  const int sync_result = HANDLE_EINTR(fsync(fd));
  return sync_result == 0;
}

#ifdef __Fuchsia__
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
UnnamedTempFileResult WriteAndLinkUnnamedTempFile(int dirfd, const std::string& filename,
                                                  std::string_view data) {
  auto error_from_status = [](zx_status_t status) {
    return status == ZX_ERR_NOT_SUPPORTED ? UnnamedTempFileResult::kNotSupported
                                          : UnnamedTempFileResult::kError;
  };

  constexpr fuchsia_io::wire::Flags kFlags =
      fuchsia_io::wire::Flags::kProtocolFile | fuchsia_io::wire::kPermReadable |
      fuchsia_io::wire::kPermWritable | fuchsia_io::wire::Flags::kFlagCreateAsUnnamedTemporary;
  int raw_fd = -1;
  if (zx_status_t status = fdio_open3_fd_at(dirfd, ".", uint64_t{kFlags}, &raw_fd);
      status != ZX_OK) {
    return error_from_status(status);
  }
  fbl::unique_fd fd(raw_fd);
  if (!WriteAndSyncFileDescriptor(fd.get(), data)) {
    return UnnamedTempFileResult::kError;
  }

  fdio_t* dir_io = fdio_unsafe_fd_to_io(dirfd);
  if (dir_io == nullptr) {
    return UnnamedTempFileResult::kError;
  }
  auto release_dir_io = fit::defer([dir_io] { fdio_unsafe_release(dir_io); });
  fidl::UnownedClientEnd<fuchsia_io::Directory> dir_client(fdio_unsafe_borrow_channel(dir_io));
  if (!dir_client.is_valid()) {
    return UnnamedTempFileResult::kNotSupported;
  }

  fdio_t* file_io = fdio_unsafe_fd_to_io(fd.get());
  if (file_io == nullptr) {
    return UnnamedTempFileResult::kError;
  }
  auto release_file_io = fit::defer([file_io] { fdio_unsafe_release(file_io); });
  fidl::UnownedClientEnd<fuchsia_io::File> file_client(fdio_unsafe_borrow_channel(file_io));
  if (!file_client.is_valid()) {
    return UnnamedTempFileResult::kNotSupported;
  }

  const fidl::WireResult token_result = fidl::WireCall(dir_client)->GetToken();
  if (!token_result.ok()) {
    return error_from_status(token_result.status());
  }
  if (token_result->is_error()) {
    return error_from_status(token_result->error_value());
  }

  fidl::WireTableFrame<fuchsia_io::wire::LinkIntoOptions> options_frame;
  fuchsia_io::wire::LinkIntoFlags flags = fuchsia_io::wire::LinkIntoFlags::kReplaceExisting;
  auto options =
      fuchsia_io::wire::LinkIntoOptions::ExternalBuilder(
          fidl::ObjectView<fidl::WireTableFrame<fuchsia_io::wire::LinkIntoOptions>>::FromExternal(
              &options_frame))
          .flags(fidl::ObjectView<fuchsia_io::wire::LinkIntoFlags>::FromExternal(&flags))
          .Build();
  const fidl::WireResult link_result =
      fidl::WireCall(file_client)
          ->LinkInto(std::move(token_result->value()->token),
                     fidl::StringView::FromExternal(filename), options);
  if (!link_result.ok()) {
    return error_from_status(link_result.status());
  }
  if (link_result->is_error()) {
    return error_from_status(link_result->error_value());
  }

  return UnnamedTempFileResult::kOk;
}
#else
UnnamedTempFileResult WriteAndLinkUnnamedTempFile(int /*dirfd*/, const std::string& /*filename*/,
                                                  std::string_view /*data*/) {
  return UnnamedTempFileResult::kNotSupported;
}
#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
#elifdef __linux__
UnnamedTempFileResult WriteAndLinkUnnamedTempFile(int dirfd, const std::string& filename,
                                                  std::string_view data) {
  const std::string temp_filename = filename + ".tmp";
  if (!IsValidName(temp_filename)) {
    return UnnamedTempFileResult::kError;
  }

  fbl::unique_fd fd(HANDLE_EINTR(openat(dirfd, ".", O_WRONLY | O_TMPFILE, FILE_CREATE_MODE)));
  if (!fd.is_valid()) {
    if (errno == EOPNOTSUPP || errno == ENOSYS || errno == EISDIR || errno == EINVAL) {
      return UnnamedTempFileResult::kNotSupported;
    }
    return UnnamedTempFileResult::kError;
  }
  if (!WriteAndSyncFileDescriptor(fd.get(), data)) {
    return UnnamedTempFileResult::kError;
  }

  // Clean up |temp_filename| in case it still existed from a previous attempt.
  unlinkat(dirfd, temp_filename.c_str(), 0);

  std::string proc_path = "/proc/self/fd/" + std::to_string(fd.get());
  if (linkat(AT_FDCWD, proc_path.c_str(), dirfd, temp_filename.c_str(), AT_SYMLINK_FOLLOW) != 0) {
    if (errno == ENOENT || errno == EOPNOTSUPP || errno == EXDEV || errno == EPERM ||
        errno == EACCES) {
      // /proc may not be mounted/accessible, or hard-linking from /proc/self/fd may not be
      // supported by the filesystem.
      return UnnamedTempFileResult::kNotSupported;
    }
    return UnnamedTempFileResult::kError;
  }

  if (renameat(dirfd, temp_filename.c_str(), dirfd, filename.c_str()) != 0) {
    unlinkat(dirfd, temp_filename.c_str(), 0);
    return UnnamedTempFileResult::kError;
  }
  return UnnamedTempFileResult::kOk;
}
#else
UnnamedTempFileResult WriteAndLinkUnnamedTempFile(int /*dirfd*/, const std::string& /*filename*/,
                                                  std::string_view /*data*/) {
  return UnnamedTempFileResult::kNotSupported;
}
#endif

template <typename T>
bool ReadFileDescriptor(int fd, T* result) {
  assert(result);
  result->clear();

  struct stat s;
  if (int res = fstat(fd, &s); res < 0) {
    return false;
  }
  // Some (many?) file implementations on Fuchsia do not report their size.
  //
  // See e.g. https://fxbug.dev/42070489.
  const size_t size = s.st_size;
  if (size != 0) {
    result->resize(size);
  }

  for (size_t offset = 0;;) {
    size_t count = [&]() {
      if (size != 0) {
        return size - offset;
      }
      constexpr size_t kBufferSize = 1 << 16;
      result->resize(offset + kBufferSize);
      return kBufferSize;
    }();
    ssize_t bytes_read = HANDLE_EINTR(read(fd, &result->data()[offset], count));
    if (bytes_read < 0) {
      result->clear();
      return false;
    }
    if (bytes_read == 0) {
      result->resize(offset);
      return true;
    }
    offset += bytes_read;
  }
}

}  // namespace

bool WriteFile(const std::string& path, std::string_view data) {
  return WriteFile(path, data.data(), data.size());
}

bool WriteFile(const std::string& path, const char* data, ssize_t size) {
  return WriteFileAt(AT_FDCWD, path, data, size);
}

bool WriteFileAt(int dirfd, const std::string& path, const char* data, ssize_t size) {
  fbl::unique_fd fd(
      HANDLE_EINTR(openat(dirfd, path.c_str(), O_CREAT | O_TRUNC | O_WRONLY, FILE_CREATE_MODE)));
  if (!fd.is_valid())
    return false;
  return fxl::WriteFileDescriptor(fd.get(), data, size);
}

bool WriteFileInTwoPhases(int dirfd, const std::string& filename, std::string_view data) {
  if (dirfd < 0 || !IsValidName(filename)) {
    return false;
  }

  switch (WriteAndLinkUnnamedTempFile(dirfd, filename, data)) {
    case UnnamedTempFileResult::kOk:
      break;
    case UnnamedTempFileResult::kError:
      return false;
    case UnnamedTempFileResult::kNotSupported: {
      const std::string temp_filename = filename + ".tmp";
      if (!IsValidName(temp_filename)) {
        return false;
      }
      fbl::unique_fd fd(HANDLE_EINTR(
          openat(dirfd, temp_filename.c_str(), O_CREAT | O_TRUNC | O_WRONLY, FILE_CREATE_MODE)));
      if (!fd.is_valid()) {
        return false;
      }
      if (!WriteAndSyncFileDescriptor(fd.get(), data)) {
        fd.reset();
        unlinkat(dirfd, temp_filename.c_str(), 0);
        return false;
      }
      fd.reset();
      if (renameat(dirfd, temp_filename.c_str(), dirfd, filename.c_str()) != 0) {
        unlinkat(dirfd, temp_filename.c_str(), 0);
        return false;
      }
      break;
    }
  }

  const int dir_sync_result = HANDLE_EINTR(fsync(dirfd));
  return dir_sync_result == 0;
}

bool ReadFileToString(const std::string& path, std::string* result) {
  return ReadFileToStringAt(AT_FDCWD, path, result);
}

bool ReadFileDescriptorToString(int fd, std::string* result) {
  return ReadFileDescriptor(fd, result);
}

bool ReadFileToStringAt(int dirfd, const std::string& path, std::string* result) {
  fbl::unique_fd fd(openat(dirfd, path.c_str(), O_RDONLY));
  return ReadFileDescriptor(fd.get(), result);
}

bool ReadFileToVector(const std::string& path, std::vector<uint8_t>* result) {
  fbl::unique_fd fd(open(path.c_str(), O_RDONLY | BINARY_MODE));
  return ReadFileDescriptor(fd.get(), result);
}

bool ReadFileDescriptorToVector(int fd, std::vector<uint8_t>* result) {
  return ReadFileDescriptor(fd, result);
}

bool ReadFileToVectorAt(int dirfd, const std::string& path, std::vector<uint8_t>* result) {
  fbl::unique_fd fd(openat(dirfd, path.c_str(), O_RDONLY));
  return ReadFileDescriptor(fd.get(), result);
}

bool IsFile(const std::string& path) { return IsFileAt(AT_FDCWD, path); }

bool IsFileAt(int dirfd, const std::string& path) {
  struct stat stat_buffer;
  if (fstatat(dirfd, path.c_str(), &stat_buffer, /* flags = */ 0) != 0)
    return false;
  return S_ISREG(stat_buffer.st_mode);
}

bool GetFileSize(const std::string& path, uint64_t* size) {
  return GetFileSizeAt(AT_FDCWD, path, size);
}

bool GetFileSizeAt(int dirfd, const std::string& path, uint64_t* size) {
  struct stat stat_buffer;
  if (fstatat(dirfd, path.c_str(), &stat_buffer, /* flags = */ 0) != 0)
    return false;
  *size = stat_buffer.st_size;
  return true;
}

}  // namespace files
