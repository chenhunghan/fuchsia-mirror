// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTS_UTILS_H_
#define LIB_SYS_COMPONENT_CPP_TESTS_UTILS_H_

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/fidl.h>
#include <fuchsia/component/decl/cpp/fidl.h>
#include <fuchsia/io/cpp/fidl.h>

#include <memory>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

namespace component {
namespace tests {

namespace fcdecl = fuchsia::component::decl;
namespace fio = fuchsia::io;

// Conversion functions for |fuchsia.component.decl| types.
std::shared_ptr<fcdecl::Ref> CreateFidlChildRef(std::string_view name);
std::shared_ptr<fcdecl::Ref> CreateFidlParentRef();

std::shared_ptr<fcdecl::Offer> CreateFidlProtocolOfferDecl(std::string_view source_name,
                                                           std::shared_ptr<fcdecl::Ref> source,
                                                           std::string_view target_name,
                                                           std::shared_ptr<fcdecl::Ref> target);
std::shared_ptr<fcdecl::Offer> CreateFidlServiceOfferDecl(std::string_view source_name,
                                                          std::shared_ptr<fcdecl::Ref> source,
                                                          std::string_view target_name,
                                                          std::shared_ptr<fcdecl::Ref> target);
std::shared_ptr<fcdecl::Offer> CreateFidlDirectoryOfferDecl(
    std::string_view source_name, std::shared_ptr<fcdecl::Ref> source, std::string_view target_name,
    std::shared_ptr<fcdecl::Ref> target, std::string_view subdir, fio::Operations rights);

std::shared_ptr<fcdecl::Offer> CreateFidlStorageOfferDecl(std::string_view source_name,
                                                          std::shared_ptr<fcdecl::Ref> source,
                                                          std::string_view target_name,
                                                          std::shared_ptr<fcdecl::Ref> target);

// Conversion functions for |fuchsia.component.test| natural types.
std::shared_ptr<fuchsia_component_test::ChildOptions> CreateFidlChildOptions(
    fuchsia_component_decl::StartupMode startup_mode, std::string_view environment,
    std::vector<std::pair<std::string, fuchsia_component_decl::ConfigValue>> config_overrides);

std::shared_ptr<fuchsia_component_test::Capability> CreateFidlProtocolCapability(
    std::string_view name, std::optional<std::string_view> as = std::nullopt,
    std::optional<fuchsia_component_decl::DependencyType> type = std::nullopt,
    std::optional<std::string_view> path = std::nullopt);

std::shared_ptr<fuchsia_component_test::Capability> CreateFidlServiceCapability(
    std::string_view name, std::optional<std::string_view> as = std::nullopt,
    std::optional<std::string_view> path = std::nullopt);

std::shared_ptr<fuchsia_component_test::Capability> CreateFidlDirectoryCapability(
    std::string_view name, std::optional<std::string_view> as = std::nullopt,
    std::optional<fuchsia_component_decl::DependencyType> type = std::nullopt,
    std::optional<std::string_view> subdir = std::nullopt,
    std::optional<fuchsia_io::Operations> rights = std::nullopt,
    std::optional<std::string_view> path = std::nullopt);

}  // namespace tests
}  // namespace component

#endif  // LIB_SYS_COMPONENT_CPP_TESTS_UTILS_H_
