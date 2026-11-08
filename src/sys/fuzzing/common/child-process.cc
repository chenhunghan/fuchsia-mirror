// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/sys/fuzzing/common/child-process.h"

#include <fuchsia/io/cpp/fidl.h>
#include <fuchsia/process/cpp/fidl.h>
#include <lib/async/cpp/executor.h>
#include <lib/fdio/directory.h>
#include <lib/fdio/fd.h>
#include <lib/fdio/io.h>
#include <lib/fdio/namespace.h>
#include <lib/fdio/spawn.h>
#include <lib/fit/defer.h>
#include <lib/syslog/cpp/macros.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <zircon/dlfcn.h>
#include <zircon/process.h>
#include <zircon/processargs.h>
#include <zircon/status.h>
#include <zircon/syscalls.h>
#include <zircon/utc.h>

#include <algorithm>

#include "src/lib/files/eintr_wrapper.h"

namespace fuzzing {
namespace {

constexpr const size_t kBufSize = 0x400;

zx_status_t ReadAndSend(int fd, AsyncSender<std::string> sender) {
  if (fd < 0) {
    FX_LOGS(ERROR) << "Invalid fd: " << fd;
    return ZX_ERR_INVALID_ARGS;
  }
  std::array<char, kBufSize> buf;
  auto start = buf.begin();
  auto end = start;
  std::string line;
  while (true) {
    // Has data; repeatedly send strings up to a newline.
    while (start != end) {
      auto newline = std::find(start, end, '\n');
      if (newline == end) {
        break;
      }
      line += std::string(&*start, newline - start);
      start = newline + 1;
      // Forward the data. If the receiver closes; just keeping draining the pipe.
      if (auto status = sender.Send(std::move(line));
          status != ZX_OK && status != ZX_ERR_PEER_CLOSED) {
        return status;
      }
      line.clear();
    }
    // Need more data. First move any remaining data to the start of the buffer.
    if (start != buf.begin()) {
      auto tmp = start;
      start = buf.begin();
      memmove(&*start, &*tmp, end - tmp);
      end -= tmp - start;
    } else if (end == buf.end()) {
      // A log line filled the buffer. Add it to `line` and keep going.
      line += std::string(&*start, end - start);
      end = start;
    }
    // Now try to read more data from the file descriptor.
    auto bytes_read = HANDLE_EINTR(read(fd, &*end, buf.end() - end));
    if (bytes_read < 0) {
      if (errno == EBADF) {
        // Stream was closed because process exited.
        return ZX_ERR_PEER_CLOSED;
      }
      FX_LOGS(ERROR) << "Failed to read output from process (fd=" << fd << "): " << strerror(errno);
      return ZX_ERR_IO;
    }
    if (bytes_read == 0) {
      // File descriptor is closed.just send whatever's left.
      if (start != end) {
        line += std::string(&*start, end - start);
        if (auto status = sender.Send(std::move(line)); status != ZX_OK) {
          return status;
        }
      }
      return ZX_ERR_PEER_CLOSED;
    }
    end += bytes_read;
  }
}

}  // namespace

ChildProcess::ChildProcess(ExecutorPtr executor) : executor_(std::move(executor)) { Reset(); }

ChildProcess::~ChildProcess() { KillSync(); }

zx_status_t ChildProcess::AddArg(std::string_view arg) {
  static const char* kPkgPrefix = "/pkg/";
  auto arg_len = (args_.empty() ? strlen(kPkgPrefix) : 0) + arg.size();
  if (auto status = ReserveBytes(arg_len); status != ZX_OK) {
    FX_LOGS(WARNING) << "Failed to add argument: '" << arg << "'";
    return status;
  }
  if (args_.empty()) {
    args_.emplace_back(std::string(kPkgPrefix) + std::string(arg));
  } else {
    args_.emplace_back(arg);
  }
  return ZX_OK;
}

zx_status_t ChildProcess::AddArgs(std::initializer_list<const char*> args) {
  auto orig_num_args = args_.size();
  auto orig_num_bytes = num_bytes_;
  for (const auto* arg : args) {
    if (auto status = AddArg(arg); status != ZX_OK) {
      args_.resize(orig_num_args);
      num_bytes_ = orig_num_bytes;
      return status;
    }
  }
  return ZX_OK;
}

zx_status_t ChildProcess::SetEnvVar(std::string_view name, std::string_view value) {
  if (auto status = ReserveBytes(name.size() + 1 + value.size()); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to add environment variable: '" << name << "=" << value << "'";
    return status;
  }
  envvars_[std::string(name)] = value;
  return ZX_OK;
}

zx_status_t ChildProcess::AddStdinPipe() {
  int wpipe = -1;
  if (auto status = AddPipe(STDIN_FILENO, nullptr, &wpipe); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create pipe to process stdin: " << zx_status_get_string(status);
    return status;
  }
  {
    std::lock_guard lock(mutex_);
    input_closed_ = false;
  }
  stdin_thread_ = std::thread([this, wpipe] {
    bool close_input = false;
    std::vector<std::string> input_lines;
    while (!close_input) {
      {
        std::unique_lock lock(mutex_);
        input_cond_.wait(lock, [this, &close_input, &input_lines]() FXL_REQUIRE(mutex_) {
          close_input = input_closed_;
          input_lines = std::move(input_lines_);
          input_lines_.clear();
          return true;
        });
      }
      for (auto& line : input_lines) {
        const char* buf = line.data();
        size_t off = 0;
        size_t len = line.size();
        while (off < len) {
          auto num_written = HANDLE_EINTR(write(wpipe, &buf[off], len - off));
          if (num_written < 0) {
            FX_LOGS(ERROR) << "Failed to write input to process: " << strerror(errno);
            close_input = true;
            break;
          }
          off += num_written;
        }
      }
    }
    close(wpipe);
  });
  return ZX_OK;
}

zx_status_t ChildProcess::AddStdoutPipe() {
  int rpipe = -1;
  if (auto status = AddPipe(STDOUT_FILENO, &rpipe, nullptr); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create pipe from process stdout: " << zx_status_get_string(status);
    return status;
  }
  AsyncSender<std::string> sender;
  stdout_ = AsyncReceiver<std::string>::MakePtr(&sender);
  stdout_thread_ = std::thread([this, rpipe, sender = std::move(sender)]() mutable {
    stdout_result_ = ReadAndSend(rpipe, std::move(sender));
  });
  return ZX_OK;
}

zx_status_t ChildProcess::AddStderrPipe() {
  int rpipe = -1;
  if (auto status = AddPipe(STDERR_FILENO, &rpipe, nullptr); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to create pipe from process stderr: " << zx_status_get_string(status);
    return status;
  }
  AsyncSender<std::string> sender;
  stderr_ = AsyncReceiver<std::string>::MakePtr(&sender);
  stderr_thread_ = std::thread([this, rpipe, sender = std::move(sender)]() mutable {
    stderr_result_ = ReadAndSend(rpipe, std::move(sender));
  });
  return ZX_OK;
}

zx_status_t ChildProcess::AddPipe(int target_fd, int* out_rpipe, int* out_wpipe) {
  if (auto status = ReserveHandles(1); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to add pipe with fd=" << target_fd;
    return status;
  }
  if (spawned_) {
    FX_LOGS(ERROR) << "Cannot add stdio pipes after spawning.";
    return ZX_ERR_BAD_STATE;
  }
  int fds[2];
  if (pipe(fds) != 0) {
    FX_LOGS(ERROR) << "Failed to transfer file descriptor: " << strerror(errno);
    return ZX_ERR_IO;
  }
  fdio_spawn_action_t action = {.action = FDIO_SPAWN_ACTION_TRANSFER_FD,
                                .fd = {
                                    .local_fd = -1,
                                    .target_fd = target_fd,
                                }};
  if (out_rpipe && !out_wpipe) {
    *out_rpipe = fds[0];
    action.fd.local_fd = fds[1];
  } else if (!out_rpipe && out_wpipe) {
    action.fd.local_fd = fds[0];
    *out_wpipe = fds[1];
  } else {
    FX_NOTREACHED() << "Exactly one of [out_rpipe, out_wpipe] must be non-null";
  }
  actions_.emplace_back(std::move(action));
  return ZX_OK;
}

zx_status_t ChildProcess::AddChannel(uint32_t id, zx::channel channel) {
  if (auto status = ReserveHandles(1); status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to add channel with id=" << id;
    return status;
  }
  fdio_spawn_action_t action{.action = FDIO_SPAWN_ACTION_ADD_HANDLE,
                             .h = {
                                 .id = PA_HND(PA_USER0, id),
                                 .handle = channel.release(),
                             }};
  actions_.emplace_back(std::move(action));
  return ZX_OK;
}

zx_status_t ChildProcess::AddVmoVdso() {
  static const zx_handle_t vdso = zx_take_startup_handle(PA_HND(PA_VMO_VDSO, 0));
  zx_handle_t vdso_copy;
  zx_status_t status = zx_handle_duplicate(vdso, ZX_RIGHT_SAME_RIGHTS, &vdso_copy);
  if (status != ZX_OK) {
    return status;
  }

  fdio_spawn_action_t action{.action = FDIO_SPAWN_ACTION_ADD_HANDLE,
                             .h = {
                                 .id = PA_HND(PA_VMO_VDSO, 0),
                                 .handle = vdso_copy,
                             }};
  actions_.emplace_back(std::move(action));
  return ZX_OK;
}

zx_status_t ChildProcess::ReserveBytes(size_t num_bytes) {
  if (num_bytes_ + num_bytes > ZX_CHANNEL_MAX_MSG_BYTES / 2) {
    FX_LOGS(WARNING) << "Spawn message bytes limit exceeded; " << num_bytes_
                     << " bytes previously added";
    return ZX_ERR_OUT_OF_RANGE;
  }
  num_bytes_ += num_bytes;
  return ZX_OK;
}

zx_status_t ChildProcess::ReserveHandles(size_t num_handles) {
  if (num_handles_ + num_handles > ZX_CHANNEL_MAX_MSG_HANDLES / 2) {
    FX_LOGS(WARNING) << "Spawn message handles limit exceeded; " << num_handles_
                     << " handles previously added";
    return ZX_ERR_OUT_OF_RANGE;
  }
  num_handles_ += num_handles;
  return ZX_OK;
}

zx_status_t ChildProcess::Spawn() {
  // Convert args and envvars to C-style strings.
  // The envvars vector holds the constructed strings backing the pointers in environ.
  std::vector<std::string> envvars;
  std::vector<const char*> environ;
  std::ostringstream log_str;
  for (const auto& [key, value] : envvars_) {
    std::ostringstream oss;
    oss << key << "=" << value;
    envvars.push_back(oss.str());
    log_str << envvars.back() << " ";
    environ.push_back(envvars.back().c_str());
  }
  environ.push_back(nullptr);

  std::vector<const char*> argv;
  argv.reserve(args_.size() + 1);
  for (const auto& arg : args_) {
    log_str << arg << " ";
    argv.push_back(arg.c_str());
  }
  argv.push_back(nullptr);
  FX_LOGS(INFO) << log_str.str();

  // Build spawn actions
  if (spawned_) {
    FX_LOGS(ERROR) << "ChildProcess must be reset before it can be respawned.";
    return ZX_ERR_BAD_STATE;
  }
  spawned_ = true;

  // Spawn the process!
  const char* shared_env = getenv("FUZZER_SHARED_PROCESS");
  if (shared_env != nullptr && strcmp(shared_env, "1") == 0) {
    return SpawnShared(envvars);
  }

  auto flags = FDIO_SPAWN_CLONE_ALL & (~FDIO_SPAWN_CLONE_STDIO);
  auto* handle = process_.reset_and_get_address();
  char err_msg[FDIO_SPAWN_ERR_MSG_MAX_LENGTH];
  if (auto status = fdio_spawn_etc(ZX_HANDLE_INVALID, flags, argv[0], &argv[0], &environ[0],
                                   actions_.size(), actions_.data(), handle, err_msg);
      status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to spawn process: " << err_msg << " (" << zx_status_get_string(status)
                   << ")";
    return status;
  }
  return ZX_OK;
}

zx_status_t ChildProcess::SpawnShared(const std::vector<std::string>& envvars) {
  if (args_.empty()) {
    return ZX_ERR_INVALID_ARGS;
  }
  const std::string& path = args_[0];

  // Collect action handles/fds up front into RAII wrappers so they are always closed on error.
  std::vector<fuchsia::process::HandleInfo> handle_infos;
  std::vector<fuchsia::process::NameInfo> extra_names;
  zx_status_t action_status = ZX_OK;
  for (const auto& action : actions_) {
    switch (action.action) {
      case FDIO_SPAWN_ACTION_CLONE_FD: {
        zx_handle_t fd_handle = ZX_HANDLE_INVALID;
        if (zx_status_t status = fdio_fd_clone(action.fd.local_fd, &fd_handle);
            status != ZX_OK && action_status == ZX_OK) {
          action_status = status;
        } else if (fd_handle != ZX_HANDLE_INVALID) {
          handle_infos.push_back(fuchsia::process::HandleInfo{
              .handle = zx::handle(fd_handle),
              .id = PA_HND(PA_FD, action.fd.target_fd),
          });
        }
        break;
      }
      case FDIO_SPAWN_ACTION_TRANSFER_FD: {
        zx_handle_t fd_handle = ZX_HANDLE_INVALID;
        if (zx_status_t status = fdio_fd_transfer(action.fd.local_fd, &fd_handle);
            status != ZX_OK && action_status == ZX_OK) {
          action_status = status;
        } else if (fd_handle != ZX_HANDLE_INVALID) {
          handle_infos.push_back(fuchsia::process::HandleInfo{
              .handle = zx::handle(fd_handle),
              .id = PA_HND(PA_FD, action.fd.target_fd),
          });
        }
        break;
      }
      case FDIO_SPAWN_ACTION_ADD_HANDLE: {
        handle_infos.push_back(fuchsia::process::HandleInfo{
            .handle = zx::handle(action.h.handle),
            .id = action.h.id,
        });
        break;
      }
      case FDIO_SPAWN_ACTION_ADD_NS_ENTRY: {
        extra_names.push_back(fuchsia::process::NameInfo{
            .path = action.ns.prefix,
            .directory =
                fidl::InterfaceHandle<fuchsia::io::Directory>(zx::channel(action.ns.handle)),
        });
        break;
      }
      default:
        break;
    }
  }
  actions_.clear();
  if (action_status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to prepare spawn actions: " << zx_status_get_string(action_status);
    return action_status;
  }

  int exec_fd = -1;
  if (zx_status_t status = fdio_open3_fd(
          path.c_str(),
          static_cast<uint64_t>(fuchsia::io::PERM_READABLE | fuchsia::io::PERM_EXECUTABLE),
          &exec_fd);
      status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to open executable '" << path
                   << "': " << zx_status_get_string(status);
    return status;
  }
  zx::vmo executable;
  zx_status_t vmo_status = fdio_get_vmo_exec(exec_fd, executable.reset_and_get_address());
  close(exec_fd);
  if (vmo_status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to get executable VMO: " << zx_status_get_string(vmo_status);
    return vmo_status;
  }
  std::string_view vmo_name(path);
  if (vmo_name.size() >= ZX_MAX_NAME_LEN) {
    if (size_t pos = vmo_name.find_last_of('/'); pos != std::string_view::npos) {
      vmo_name.remove_prefix(pos + 1);
    }
  }
  executable.set_property(ZX_PROP_NAME, vmo_name.data(),
                          std::min(vmo_name.size(), size_t{ZX_MAX_NAME_LEN - 1}));

  // Create an isolated child job so that if the fuzzed process marks any restricted-mode task as
  // critical to its default job (as Starnix does via ZX_JOB_CRITICAL_PROCESS_RETCODE_NONZERO), a
  // panic or crash terminates only `child_job` rather than killing the parent fuzzer runner job.
  zx::job child_job;
  if (zx_status_t status = zx::job::create(*zx::job::default_job(), 0, &child_job);
      status != ZX_OK) {
    return status;
  }
  zx::job job_for_child;
  if (zx_status_t status = child_job.duplicate(ZX_RIGHT_SAME_RIGHTS, &job_for_child);
      status != ZX_OK) {
    return status;
  }
  handle_infos.push_back(fuchsia::process::HandleInfo{
      .handle = std::move(job_for_child),
      .id = PA_JOB_DEFAULT,
  });

  zx::channel ldsvc;
  if (zx_status_t status = dl_clone_loader_service(ldsvc.reset_and_get_address());
      status != ZX_OK) {
    return status;
  }
  handle_infos.push_back(fuchsia::process::HandleInfo{
      .handle = std::move(ldsvc),
      .id = PA_LDSVC_LOADER,
  });

  if (zx_handle_t utc_clock = zx_utc_reference_get(); utc_clock != ZX_HANDLE_INVALID) {
    zx::handle utc_dup;
    if (zx_status_t status =
            zx_handle_duplicate(utc_clock, ZX_RIGHT_SAME_RIGHTS, utc_dup.reset_and_get_address());
        status != ZX_OK) {
      return status;
    }
    handle_infos.push_back(fuchsia::process::HandleInfo{
        .handle = std::move(utc_dup),
        .id = PA_CLOCK_UTC,
    });
  }

  fdio_flat_namespace_t* flat = nullptr;
  if (zx_status_t status = fdio_ns_export_root(&flat); status != ZX_OK) {
    return status;
  }
  auto flat_cleanup = fit::defer([flat]() { fdio_ns_free_flat_ns(flat); });
  std::vector<fuchsia::process::NameInfo> names;
  names.reserve(flat->count + extra_names.size());
  for (size_t i = 0; i < flat->count; ++i) {
    names.push_back(fuchsia::process::NameInfo{
        .path = flat->path[i],
        .directory = fidl::InterfaceHandle<fuchsia::io::Directory>(zx::channel(flat->handle[i])),
    });
    flat->handle[i] = ZX_HANDLE_INVALID;
  }
  for (auto& extra : extra_names) {
    names.push_back(std::move(extra));
  }

  fuchsia::process::LauncherSyncPtr launcher;
  if (zx_status_t status = fdio_service_connect("/svc/fuchsia.process.Launcher",
                                                launcher.NewRequest().TakeChannel().release());
      status != ZX_OK) {
    FX_LOGS(ERROR) << "Failed to connect to fuchsia.process.Launcher: "
                   << zx_status_get_string(status);
    return status;
  }

  std::vector<std::vector<uint8_t>> fidl_args;
  fidl_args.reserve(args_.size());
  for (const auto& arg : args_) {
    fidl_args.emplace_back(arg.begin(), arg.end());
  }
  if (zx_status_t status = launcher->AddArgs(std::move(fidl_args)); status != ZX_OK) {
    return status;
  }

  std::vector<std::vector<uint8_t>> fidl_env;
  fidl_env.reserve(envvars.size());
  for (const auto& var : envvars) {
    fidl_env.emplace_back(var.begin(), var.end());
  }
  if (zx_status_t status = launcher->AddEnvirons(std::move(fidl_env)); status != ZX_OK) {
    return status;
  }

  if (zx_status_t status = launcher->AddHandles(std::move(handle_infos)); status != ZX_OK) {
    return status;
  }
  if (zx_status_t status = launcher->AddNames(std::move(names)); status != ZX_OK) {
    return status;
  }
  if (zx_status_t status = launcher->SetOptions(ZX_PROCESS_SHARED); status != ZX_OK) {
    return status;
  }

  fuchsia::process::LaunchInfo launch_info;
  launch_info.executable = std::move(executable);
  if (zx_status_t status = child_job.duplicate(ZX_RIGHT_SAME_RIGHTS, &launch_info.job);
      status != ZX_OK) {
    return status;
  }
  launch_info.name = path.substr(0, ZX_MAX_NAME_LEN - 1);

  zx_status_t launch_status = ZX_OK;
  zx::process spawned_proc;
  if (zx_status_t status = launcher->Launch(std::move(launch_info), &launch_status, &spawned_proc);
      status != ZX_OK) {
    FX_LOGS(ERROR) << "Launcher::Launch FIDL call failed: " << zx_status_get_string(status);
    return status;
  }
  if (launch_status != ZX_OK) {
    FX_LOGS(ERROR) << "Launcher::Launch failed: " << zx_status_get_string(launch_status);
    return launch_status;
  }
  if (!spawned_proc.is_valid()) {
    return ZX_ERR_BAD_HANDLE;
  }
  job_ = std::move(child_job);
  process_ = std::move(spawned_proc);
  return ZX_OK;
}

bool ChildProcess::IsAlive() {
  if (!process_) {
    return false;
  }
  auto status = process_.get_info(ZX_INFO_PROCESS, &info_, sizeof(info_), nullptr, nullptr);
  if (status == ZX_ERR_BAD_HANDLE) {
    return false;
  }
  FX_CHECK(status == ZX_OK);
  return (info_.flags & ZX_INFO_PROCESS_FLAG_EXITED) == 0;
}

zx_status_t ChildProcess::Duplicate(zx::process* out) {
  return process_.duplicate(ZX_RIGHT_SAME_RIGHTS, out);
}

zx_status_t ChildProcess::WriteToStdin(std::string_view line) {
  if (!IsAlive()) {
    FX_LOGS(WARNING) << "Cannot write to process standard input: not running";
    return ZX_ERR_BAD_STATE;
  }
  {
    std::lock_guard lock(mutex_);
    if (input_closed_) {
      FX_LOGS(WARNING) << "Cannot write to process standard input: closed";
      return ZX_ERR_PEER_CLOSED;
    }
    input_lines_.emplace_back(std::string(line));
  }
  input_cond_.notify_one();
  return ZX_OK;
}

ZxPromise<std::string> ChildProcess::ReadFromStdout() {
  if (killed_ || !stdout_) {
    return fpromise::make_promise(
        []() -> ZxResult<std::string> { return fpromise::error(ZX_ERR_BAD_STATE); });
  }
  return stdout_->Receive()
      .or_else([this]() -> ZxResult<std::string> {
        if (stdout_thread_.joinable()) {
          stdout_thread_.join();
        }
        return fpromise::error(stdout_result_);
      })
      .wrap_with(scope_);
}

ZxPromise<std::string> ChildProcess::ReadFromStderr() {
  if (killed_ || !stderr_) {
    return fpromise::make_promise(
        []() -> ZxResult<std::string> { return fpromise::error(ZX_ERR_BAD_STATE); });
  }
  return stderr_->Receive()
      .or_else([this]() -> ZxResult<std::string> {
        if (stderr_thread_.joinable()) {
          stderr_thread_.join();
        }
        return fpromise::error(stderr_result_);
      })
      .wrap_with(scope_);
}

void ChildProcess::CloseStdin() {
  {
    std::lock_guard lock(mutex_);
    input_closed_ = true;
  }
  input_cond_.notify_one();
}

ZxResult<ProcessStats> ChildProcess::GetStats() {
  ProcessStats stats;
  if (auto status = GetStatsForProcess(process_, &stats); status != ZX_OK) {
    return fpromise::error(status);
  }
  return fpromise::ok(std::move(stats));
}

ZxPromise<int64_t> ChildProcess::Wait() {
  return fpromise::make_promise([this, terminated = ZxFuture<zx_packet_signal_t>()](
                                    Context& context) mutable -> ZxResult<int64_t> {
           if (!IsAlive()) {
             return fpromise::ok(info_.return_code);
           }
           if (!terminated) {
             terminated = executor_->MakePromiseWaitHandle(zx::unowned_handle(process_.get()),
                                                           ZX_PROCESS_TERMINATED);
           }
           if (!terminated(context)) {
             return fpromise::pending();
           }
           if (terminated.is_error()) {
             auto status = terminated.error();
             FX_LOGS(WARNING) << "Failed to wait for process to terminate: "
                              << zx_status_get_string(status);
             return fpromise::error(status);
           }
           if (IsAlive()) {
             FX_LOGS(WARNING) << "Failed to terminate process.";
             return fpromise::error(ZX_ERR_BAD_STATE);
           }
           return fpromise::ok(info_.return_code);
         })
      .wrap_with(scope_);
}

ZxPromise<> ChildProcess::Kill() {
  return fpromise::make_promise(
             [this, wait = ZxFuture<int64_t>()](Context& context) mutable -> ZxResult<> {
               if (!wait) {
                 KillSync();
                 wait = Wait();
               }
               if (!wait(context)) {
                 return fpromise::pending();
               }
               if (wait.is_error()) {
                 return fpromise::error(wait.take_error());
               }
               return fpromise::ok();
             })
      .wrap_with(scope_);
}

void ChildProcess::KillSync() {
  if (process_) {
    process_.kill();
  }
  if (job_) {
    job_.kill();
  }

  CloseStdin();
  if (stdin_thread_.joinable()) {
    stdin_thread_.join();
  }

  if (stdout_thread_.joinable()) {
    stdout_thread_.join();
  }

  if (stderr_thread_.joinable()) {
    stderr_thread_.join();
  }

  killed_ = true;
}

void ChildProcess::Reset() {
  KillSync();
  spawned_ = false;
  killed_ = false;
  num_bytes_ = 0;
  num_handles_ = 0;
  args_.clear();
  envvars_.clear();
  process_.reset();
  job_.reset();
  memset(&info_, 0, sizeof(info_));
  {
    std::lock_guard lock(mutex_);
    input_closed_ = false;
    input_lines_.clear();
  }
  stdout_.reset();
  stdout_result_ = ZX_OK;
  stderr_.reset();
  stderr_result_ = ZX_OK;
  actions_.clear();
}

}  // namespace fuzzing
