// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_REALM_H_
#define LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_REALM_H_

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fidl/cpp/client.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <lib/sys/cpp/component_context.h>

#include <memory>
#include <string>

namespace component_testing {
namespace internal {

fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(
    fidl::UnownedClientEnd<fuchsia_io::Directory> svc);
fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(const sys::ComponentContext* context);
fidl::SyncClient<fuchsia_component::Realm> CreateRealmClient(
    std::shared_ptr<sys::ServiceDirectory> svc);

fidl::ClientEnd<fuchsia_io::Directory> OpenExposedDir(
    const fidl::SyncClient<fuchsia_component::Realm>& realm,
    const fuchsia_component_decl::ChildRef& child_ref);

void CreateChild(const fidl::SyncClient<fuchsia_component::Realm>& realm,
                 fidl::ServerEnd<fuchsia_component::Controller> controller, std::string collection,
                 std::string name, std::string url);

void DestroyChild(const fidl::SyncClient<fuchsia_component::Realm>& realm,
                  fuchsia_component_decl::ChildRef child_ref);
void DestroyChild(fidl::UnownedClientEnd<fuchsia_io::Directory> svc, async_dispatcher_t* dispatcher,
                  fuchsia_component_decl::ChildRef child_ref,
                  fit::function<void(fit::result<fuchsia_component::Error>)> callback);

}  // namespace internal
}  // namespace component_testing

#endif  // LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_REALM_H_
