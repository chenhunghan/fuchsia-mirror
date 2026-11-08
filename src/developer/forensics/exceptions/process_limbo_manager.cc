// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/developer/forensics/exceptions/process_limbo_manager.h"

#include <fidl/fuchsia.exception/cpp/fidl.h>
#include <lib/stdcompat/string_view.h>
#include <lib/syslog/cpp/macros.h>

#include "src/lib/fsl/handles/object_info.h"

namespace forensics {
namespace exceptions {

namespace {

using fuchsia_exception::kMaxExceptions;
using fuchsia_exception::ProcessException;
using fuchsia_exception::ProcessExceptionInfo;
using fuchsia_exception::ProcessExceptionMetadata;

// Removes all stale weak pointers from the handler list.
void PruneStaleHandlers(std::vector<fxl::WeakPtr<ProcessLimboHandler>>* handlers) {
  // We only move active handlers to the new list.
  std::vector<fxl::WeakPtr<ProcessLimboHandler>> new_handlers;
  for (fxl::WeakPtr<ProcessLimboHandler>& handler : *handlers) {
    if (handler)
      new_handlers.push_back(std::move(handler));
  }

  *handlers = std::move(new_handlers);
}

// This verification was getting repeated for every call.
template <typename Completer>
bool VerifyState(const fxl::WeakPtr<ProcessLimboManager>& limbo_manager, Completer& completer) {
  if (!limbo_manager) {
    completer.Reply(fit::error(ZX_ERR_UNAVAILABLE));
    return false;
  }
  return true;
}

std::vector<std::string> CreateFilterVector(const std::set<std::string>& filter_set) {
  std::vector<std::string> filters;
  filters.reserve(filter_set.size());

  for (const std::string& filter : filter_set) {
    filters.emplace_back(filter);
  }

  return filters;
}

}  // namespace

ProcessLimboManager::ProcessLimboManager() : weak_factory_(this) {
  // Set the default function for getting process names.
  obtain_process_name_fn_ = [](zx_handle_t handle) -> std::string {
    return fsl::GetObjectName(handle);
  };
}

fxl::WeakPtr<ProcessLimboManager> ProcessLimboManager::GetWeakPtr() {
  return weak_factory_.GetWeakPtr();
}

void ProcessLimboManager::AddToLimbo(ProcessException process_exception) {
  // Check filters.
  const zx_handle_t process_handle = process_exception.process().has_value()
                                         ? process_exception.process()->get()
                                         : ZX_HANDLE_INVALID;
  const std::string process_name = obtain_process_name_fn_(process_handle);

  // Empty names will be stored within the limbo.
  if (!process_name.empty() && !filters_.empty()) {
    // Search for a partial match over the filters.
    bool filter_found = false;
    for (const std::string& filter : filters_) {
      if (cpp23::contains(process_name, filter)) {
        filter_found = true;
        break;
      }
    }

    // If a matching filter was found, we will not store this process.
    if (filter_found)
      return;
  }

  limbo_[process_exception.info()->process_koid()] = std::move(process_exception);

  NotifyLimboChanged();
}

void ProcessLimboManager::NotifyLimboChanged() {
  // Notify the handlers of the new list of processes in limbo.
  PruneStaleHandlers(&handlers_);
  for (fxl::WeakPtr<ProcessLimboHandler>& handler : handlers_) {
    std::vector<ProcessExceptionMetadata> limbo_list = ListProcessesInLimbo();
    handler->LimboChanged(std::move(limbo_list));
  }
}

void ProcessLimboManager::AddHandler(fxl::WeakPtr<ProcessLimboHandler> handler) {
  handlers_.push_back(std::move(handler));
}

void ProcessLimboManager::AppendFiltersForTesting(const std::vector<std::string>& filters) {
  for (const std::string& filter : filters) {
    filters_.insert(filter);
  }
}

std::vector<ProcessExceptionMetadata> ProcessLimboManager::ListProcessesInLimbo() {
  std::vector<ProcessExceptionMetadata> exceptions;

  size_t max_size = limbo_.size() <= kMaxExceptions ? limbo_.size() : kMaxExceptions;
  exceptions.reserve(max_size);

  // The new rights of the handles we're going to duplicate.
  zx_rights_t rights =
      ZX_RIGHT_READ | ZX_RIGHT_GET_PROPERTY | ZX_RIGHT_TRANSFER | ZX_RIGHT_DUPLICATE;
  for (const auto& [process_koid, limbo_exception] : limbo_) {
    if (!limbo_exception.process().has_value() || !limbo_exception.thread().has_value()) {
      FX_LOGS(WARNING) << "Skipping process " << process_koid << " without process/thread handle.";
      continue;
    }

    ProcessExceptionMetadata metadata = {};

    zx::process process;
    if (zx_status_t res = limbo_exception.process()->duplicate(rights, &process); res != ZX_OK) {
      FX_PLOGS(ERROR, res) << "Could not duplicate process handle.";
      continue;
    }

    zx::thread thread;
    if (zx_status_t res = limbo_exception.thread()->duplicate(rights, &thread); res != ZX_OK) {
      FX_PLOGS(ERROR, res) << "Could not duplicate thread handle.";
      continue;
    }

    metadata.info(limbo_exception.info());
    metadata.process(std::move(process));
    metadata.thread(std::move(thread));

    exceptions.push_back(std::move(metadata));

    if (exceptions.size() >= kMaxExceptions)
      break;
  }

  return exceptions;
}

bool ProcessLimboManager::SetActive(bool active) {
  // Ignore if no change.
  if (active == active_)
    return false;
  active_ = active;

  // If the limbo was disabled, free all the exceptions.
  if (!active_)
    limbo_.clear();

  // Notify the handlers of the new activa state.
  PruneStaleHandlers(&handlers_);
  for (fxl::WeakPtr<ProcessLimboHandler>& handler : handlers_) {
    handler->ActiveStateChanged(active);
  }

  return true;
}

// ProcessLimboHandler -----------------------------------------------------------------------------

ProcessLimboHandler::ProcessLimboHandler(fxl::WeakPtr<ProcessLimboManager> limbo_manager)
    : limbo_manager_(std::move(limbo_manager)), weak_factory_(this) {}

fxl::WeakPtr<ProcessLimboHandler> ProcessLimboHandler::GetWeakPtr() {
  return weak_factory_.GetWeakPtr();
}

void ProcessLimboHandler::SetActive(SetActiveRequest& request,
                                    SetActiveCompleter::Sync& completer) {
  // Reply before so that the response of this call is sent before any hanging gets.
  completer.Reply();
  limbo_manager_->SetActive(request.active());
}

void ProcessLimboHandler::ActiveStateChanged(bool active) {
  if (!watch_active_completer_.has_value()) {
    // Reset the WatchActive state as the state is different from the last time the get was called.
    watch_active_dirty_bit_ = true;
  } else {
    watch_active_completer_->Reply(active);
    watch_active_completer_ = std::nullopt;
    watch_active_dirty_bit_ = false;
  }

  // If there is a limbo call waiting, we tell them that it's canceled.
  if (!active) {
    if (watch_limbo_completer_.has_value()) {
      watch_limbo_completer_->Reply(fit::error(ZX_ERR_CANCELED));
      watch_limbo_completer_ = std::nullopt;
      watch_limbo_dirty_bit_ = false;
    } else {
      watch_limbo_dirty_bit_ = true;
    }
  }
}

void ProcessLimboHandler::LimboChanged(std::vector<ProcessExceptionMetadata> limbo_list) {
  if (!watch_limbo_completer_.has_value()) {
    // Reset the hanging get state as the state is different from the first time the get was called.
    watch_limbo_dirty_bit_ = true;
    return;
  }

  watch_limbo_completer_->Reply(fit::ok(std::move(limbo_list)));
  watch_limbo_completer_ = std::nullopt;
  watch_limbo_dirty_bit_ = false;
}

void ProcessLimboHandler::GetActive(GetActiveCompleter::Sync& completer) {
  completer.Reply(limbo_manager_ ? limbo_manager_->active() : false);
}

void ProcessLimboHandler::WatchActive(WatchActiveCompleter::Sync& completer) {
  if (watch_active_completer_.has_value()) {
    // Only one outstanding hanging get is allowed per connection.
    FX_LOGS(WARNING) << "WatchActive called while another call is pending. Closing connection.";
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }

  if (watch_active_dirty_bit_) {
    watch_active_dirty_bit_ = false;

    bool is_active = !!limbo_manager_ ? limbo_manager_->active() : false;
    completer.Reply(is_active);
    return;
  }

  // We store the completer for when the active state changes.
  watch_active_completer_ = completer.ToAsync();
}

void ProcessLimboHandler::ListProcessesWaitingOnException(
    ListProcessesWaitingOnExceptionCompleter::Sync& completer) {
  if (!limbo_manager_) {
    completer.Reply(fit::error(ZX_ERR_BAD_STATE));
    return;
  }
  if (!limbo_manager_->active()) {
    completer.Reply(fit::error(ZX_ERR_UNAVAILABLE));
    return;
  }
  std::vector<ProcessExceptionInfo> exceptions;
  for (const auto& [process_koid, limbo_exception] : limbo_manager_->limbo()) {
    if (!limbo_exception.process().has_value() || !limbo_exception.thread().has_value()) {
      FX_LOGS(WARNING) << "Skipping process " << process_koid << " without process/thread handle.";
      continue;
    }

    ProcessExceptionInfo info;
    info.info(limbo_exception.info());

    char name[ZX_MAX_NAME_LEN];
    if (zx_status_t res = limbo_exception.process()->get_property(ZX_PROP_NAME, name, sizeof(name));
        res != ZX_OK) {
      FX_PLOGS(ERROR, res) << "Could not get process name.";
      continue;
    }
    info.process_name(name);

    if (zx_status_t res = limbo_exception.thread()->get_property(ZX_PROP_NAME, name, sizeof(name));
        res != ZX_OK) {
      FX_PLOGS(ERROR, res) << "Could not get thread name.";
      continue;
    }
    info.thread_name(name);

    exceptions.push_back(std::move(info));

    if (exceptions.size() >= kMaxExceptions)
      break;
  }
  completer.Reply(fit::ok(std::move(exceptions)));
}

void ProcessLimboHandler::WatchProcessesWaitingOnException(
    WatchProcessesWaitingOnExceptionCompleter::Sync& completer) {
  if (watch_limbo_completer_.has_value()) {
    // Only one outstanding hanging get is allowed per connection.
    FX_LOGS(WARNING) << "WatchProcessesWaitingOnException called while another call is pending. "
                        "Closing connection.";
    completer.Close(ZX_ERR_BAD_STATE);
    return;
  }

  if (watch_limbo_dirty_bit_) {
    watch_limbo_dirty_bit_ = false;

    if (!limbo_manager_) {
      completer.Reply(fit::error(ZX_ERR_BAD_STATE));
      return;
    }

    if (!limbo_manager_->active()) {
      completer.Reply(fit::error(ZX_ERR_UNAVAILABLE));
      return;
    }

    std::vector<ProcessExceptionMetadata> processes = limbo_manager_->ListProcessesInLimbo();
    completer.Reply(fit::ok(std::move(processes)));
    return;
  }

  // Store the completer for when the processes enter the limbo.
  watch_limbo_completer_ = completer.ToAsync();
}

void ProcessLimboHandler::RetrieveException(RetrieveExceptionRequest& request,
                                            RetrieveExceptionCompleter::Sync& completer) {
  if (!VerifyState(limbo_manager_, completer))
    return;

  std::map<zx_koid_t, ProcessException>& limbo = limbo_manager_->limbo_;

  const zx_koid_t process_koid = request.process_koid();
  auto it = limbo.find(process_koid);
  if (it == limbo.end()) {
    FX_LOGS(WARNING) << "Could not find process " << process_koid << " in limbo.";
    completer.Reply(fit::error(ZX_ERR_NOT_FOUND));
    return;
  }

  ProcessException process_exception = std::move(it->second);
  limbo.erase(it);
  completer.Reply(fit::ok(std::move(process_exception)));

  limbo_manager_->NotifyLimboChanged();
}

void ProcessLimboHandler::ReleaseProcess(ReleaseProcessRequest& request,
                                         ReleaseProcessCompleter::Sync& completer) {
  if (!VerifyState(limbo_manager_, completer))
    return;

  std::map<zx_koid_t, ProcessException>& limbo = limbo_manager_->limbo_;

  auto it = limbo.find(request.process_koid());
  if (it == limbo.end()) {
    completer.Reply(fit::error(ZX_ERR_NOT_FOUND));
    return;
  }

  limbo.erase(it);
  completer.Reply(fit::ok());

  limbo_manager_->NotifyLimboChanged();
}

void ProcessLimboHandler::GetFilters(GetFiltersCompleter::Sync& completer) {
  if (!limbo_manager_) {
    completer.Reply(std::vector<std::string>());
    return;
  }

  completer.Reply(CreateFilterVector(limbo_manager_->filters_));
}

void ProcessLimboHandler::AppendFilters(AppendFiltersRequest& request,
                                        AppendFiltersCompleter::Sync& completer) {
  if (!VerifyState(limbo_manager_, completer))
    return;

  std::set<std::string> current_filters = limbo_manager_->filters_;

  for (const std::string& filter : request.filters()) {
    current_filters.insert(filter);
    if (current_filters.size() >= ProcessLimboManager::kMaxFilters) {
      completer.Reply(fit::error(ZX_ERR_NO_RESOURCES));
      return;
    }
  }

  limbo_manager_->filters_ = std::move(current_filters);
  completer.Reply(fit::ok());
}

void ProcessLimboHandler::RemoveFilters(RemoveFiltersRequest& request,
                                        RemoveFiltersCompleter::Sync& completer) {
  if (!VerifyState(limbo_manager_, completer))
    return;

  for (const std::string& filter : request.filters()) {
    limbo_manager_->filters_.erase(filter);
  }

  completer.Reply(fit::ok());
}

}  // namespace exceptions
}  // namespace forensics
