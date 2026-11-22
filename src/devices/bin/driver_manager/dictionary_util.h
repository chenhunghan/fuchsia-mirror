// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_DICTIONARY_UTIL_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_DICTIONARY_UTIL_H_

#include <fidl/fuchsia.component.runtime/cpp/fidl.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/fit/function.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/result.h>

namespace driver_manager {

// Returns a new handle to the same object as |handle|, with the same rights.
zx::result<zx::eventpair> DuplicateEventpair(const zx::eventpair& handle);

// Wraps the parts of fuchsia.component.runtime.Capabilities that driver manager uses. Dictionaries
// and other capabilities are referenced by eventpair handles; see that protocol for details.
class DictionaryUtil {
 public:
  // Opens a new connection to fuchsia.component.runtime.Capabilities.
  using CapabilitiesConnector =
      fit::function<zx::result<fidl::ClientEnd<fuchsia_component_runtime::Capabilities>>()>;

  // |connector| opens the dedicated connection that each route operation uses. If it is null, the
  // protocol is connected to through the component's incoming namespace.
  DictionaryUtil(fidl::ClientEnd<fuchsia_component_runtime::Capabilities> capabilities,
                 async_dispatcher_t* dispatcher, CapabilitiesConnector connector = nullptr);

  virtual ~DictionaryUtil() = default;

  // Creates a shallow copy of |dictionary|. Component manager drains the dictionary that it is
  // given in fuchsia.component/CreateChildArgs.additional_inputs, so each component gets a copy.
  virtual void CopyDictionary(const zx::eventpair& dictionary,
                              fit::callback<void(zx::result<zx::eventpair>)> callback);

  // Looks up |key| in |dictionary| and opens the directory it refers to. The entry may be a
  // directory connector, or a directory connector router that is routed first.
  virtual void DictionaryDirConnectorOpen(
      const zx::eventpair& dictionary, std::string_view key,
      fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback);

  // Creates a dictionary that contains a directory connector for each entry in |receivers|.
  virtual void CreateDictionaryWith(
      std::unordered_map<std::string, fidl::ClientEnd<fuchsia_component_runtime::DirReceiver>>
          receivers,
      fit::callback<void(zx::result<zx::eventpair>)> callback);

 private:
  // Routes |router| and opens the resulting directory connector. |key| is used for logging.
  void RouteDirConnectorRouter(
      zx::eventpair router, std::string key,
      fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback);

  void OpenDirConnector(
      zx::eventpair connector,
      fit::callback<void(zx::result<fidl::ClientEnd<fuchsia_io::Directory>>)> callback);

  fidl::WireClient<fuchsia_component_runtime::Capabilities> capabilities_;
  CapabilitiesConnector connector_;
  async_dispatcher_t* dispatcher_;
};

struct DirInfo {
  fidl::ClientEnd<fuchsia_io::Directory> dir;
  std::string target_service_name;
  std::unordered_map<std::string, std::string> target_to_source_instance_mapping;
  std::string parent_name;
  bool is_primary;
};

class DirReceiverImpl : public fidl::WireServer<fuchsia_component_runtime::DirReceiver> {
 public:
  DirReceiverImpl(fidl::ServerEnd<fuchsia_component_runtime::DirReceiver> dir_receiver,
                  std::vector<DirInfo> dir_infos, async_dispatcher_t* dispatcher)
      : dir_receiver_binding_(dispatcher, std::move(dir_receiver), this,
                              fidl::kIgnoreBindingClosure),
        dir_infos_(std::move(dir_infos)),
        out_(dispatcher) {}

 private:
  void Receive(ReceiveRequestView request, ReceiveCompleter::Sync& completer) override;

  fidl::ServerBinding<fuchsia_component_runtime::DirReceiver> dir_receiver_binding_;
  std::vector<DirInfo> dir_infos_;
  component::OutgoingDirectory out_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_DICTIONARY_UTIL_H_
