// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.decl/cpp/hlcpp_conversion.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.io/cpp/hlcpp_conversion.h>
#include <lib/sys/component/cpp/testing/internal/convert.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>
#include <zircon/availability.h>

namespace component_testing {
namespace internal {

// Convenience macro to check if a std::optional |field| is present
// and if so, populate the |fidl_type| with its natural conversion. This is only
// used in |ConvertToFidl|.
#define ZX_COMPONENT_ADD_IF_PRESENT(cpp_type, field, fidl_type) \
  if ((cpp_type)->field.has_value()) {                          \
    auto _val = (cpp_type)->field.value();                      \
    (fidl_type).field(fidl::HLCPPToNatural(std::move(_val)));   \
  }

// Same as above but wraps |field| value in a std::string.
#define ZX_COMPONENT_ADD_STR_IF_PRESENT(cpp_type, field, fidl_type) \
  if ((cpp_type)->field.has_value()) {                              \
    (fidl_type).field(std::string((cpp_type)->field.value()));      \
  }

fuchsia_component_test::ChildOptions ConvertToFidl(const ChildOptions& options) {
  fuchsia_component_test::ChildOptions result;
  auto startup_mode = options.startup_mode;
  result.startup(fidl::HLCPPToNatural(std::move(startup_mode)));
  if (!options.environment.empty()) {
    result.environment(std::string(options.environment));
  }

  if (!options.config_overrides.empty()) {
    std::vector<fuchsia_component_decl::ConfigOverride> overrides;
    overrides.reserve(options.config_overrides.size());

    for (const auto& config_override : options.config_overrides) {
      ZX_ASSERT(!config_override.IsEmpty());
      fuchsia::component::decl::ConfigOverride override_clone;
      ZX_COMPONENT_ASSERT_STATUS_OK("ConfigValue/Clone", config_override.Clone(&override_clone));

      overrides.push_back(fidl::HLCPPToNatural(std::move(override_clone)));
    }
    result.config_overrides(std::move(overrides));
  }

  return result;
}

RefPathPair ConvertRefToFidl(const Ref& ref, RefContext ctx) {
  if (auto child_ref = std::get_if<ChildRef>(&ref)) {
    return std::make_pair(fuchsia_component_decl::Ref::WithChild(fuchsia_component_decl::ChildRef{
                              {.name = std::string(child_ref->name)}}),
                          ".");
  }
  if (auto _ = std::get_if<ParentRef>(&ref)) {
    return std::make_pair(
        fuchsia_component_decl::Ref::WithParent(fuchsia_component_decl::ParentRef{}), ".");
  }
  if (auto collection_ref = std::get_if<CollectionRef>(&ref)) {
    return std::make_pair(
        fuchsia_component_decl::Ref::WithCollection(
            fuchsia_component_decl::CollectionRef{{.name = std::string(collection_ref->name)}}),
        ".");
  }
  if (auto _ = std::get_if<FrameworkRef>(&ref)) {
    return std::make_pair(
        fuchsia_component_decl::Ref::WithFramework(fuchsia_component_decl::FrameworkRef{}), ".");
  }
  if (auto _ = std::get_if<VoidRef>(&ref)) {
    return std::make_pair(
        fuchsia_component_decl::Ref::WithVoidType(fuchsia_component_decl::VoidRef{}), ".");
  }
  if (auto _ = std::get_if<SelfRef>(&ref)) {
    return std::make_pair(fuchsia_component_decl::Ref::WithSelf(fuchsia_component_decl::SelfRef{}),
                          ".");
  }
  if (auto dictionary_ref = std::get_if<DictionaryRef>(&ref)) {
    auto path = std::string(dictionary_ref->path);
    const auto* base_ref = &dictionary_ref->base_ref;
    switch (ctx) {
      case SOURCE:
        if (auto child_ref = std::get_if<ChildRef>(base_ref)) {
          return std::make_pair(
              fuchsia_component_decl::Ref::WithChild(
                  fuchsia_component_decl::ChildRef{{.name = std::string(child_ref->name)}}),
              std::move(path));
        }
        if (auto _ = std::get_if<ParentRef>(base_ref)) {
          return std::make_pair(
              fuchsia_component_decl::Ref::WithParent(fuchsia_component_decl::ParentRef{}),
              std::move(path));
        }
        if (auto _ = std::get_if<FrameworkRef>(base_ref)) {
          return std::make_pair(
              fuchsia_component_decl::Ref::WithFramework(fuchsia_component_decl::FrameworkRef{}),
              std::move(path));
        }
        if (auto _ = std::get_if<VoidRef>(base_ref)) {
          return std::make_pair(
              fuchsia_component_decl::Ref::WithVoidType(fuchsia_component_decl::VoidRef{}),
              std::move(path));
        }
        if (auto _ = std::get_if<SelfRef>(base_ref)) {
          return std::make_pair(
              fuchsia_component_decl::Ref::WithSelf(fuchsia_component_decl::SelfRef{}),
              std::move(path));
        }
        ZX_PANIC("ConvertToFidl(Ref) reached unreachable block!");
      case TARGET:
        if (dictionary_ref->path.find('/') != std::string_view::npos ||
            !std::holds_alternative<SelfRef>(*base_ref)) {
          ZX_PANIC(
              "DictionaryRef path must have the form "
              "{.base_ref=SelfRef(), .path=<dictionary_name>}");
        }
        return std::make_pair(
            fuchsia_component_decl::Ref::WithCapability(
                fuchsia_component_decl::CapabilityRef{{.name = std::string(dictionary_ref->path)}}),
            ".");
    }
  }

  ZX_PANIC("ConvertToFidl(Ref) reached unreachable block!");
}

std::vector<RefPathPair> ConvertRefToFidlVec(const std::vector<Ref>& inputs, RefContext ctx) {
  std::vector<RefPathPair> result;
  result.reserve(inputs.size());
  for (const auto& input : inputs) {
    result.push_back(ConvertRefToFidl(input, ctx));
  }
  return result;
}

fuchsia_component_test::Capability ConvertToFidl(Capability capability) {
  if (auto protocol = std::get_if<Protocol>(&capability)) {
    fuchsia_component_test::Protocol fidl_capability;

    fidl_capability.name(std::string(protocol->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(protocol, as, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(protocol, path, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(protocol, type, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(protocol, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithProtocol(std::move(fidl_capability));
  }
  if (auto service = std::get_if<Service>(&capability)) {
    fuchsia_component_test::Service fidl_capability;

    fidl_capability.name(std::string(service->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(service, as, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(service, path, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(service, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithService(std::move(fidl_capability));
  }
  if (auto directory = std::get_if<Directory>(&capability)) {
    fuchsia_component_test::Directory fidl_capability;

    fidl_capability.name(std::string(directory->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(directory, as, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(directory, type, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(directory, subdir, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(directory, rights, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(directory, path, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(directory, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithDirectory(std::move(fidl_capability));
  }
  if (auto storage = std::get_if<Storage>(&capability)) {
    fuchsia_component_test::Storage fidl_capability;

    fidl_capability.name(std::string(storage->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(storage, as, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(storage, path, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(storage, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithStorage(std::move(fidl_capability));
  }
  if (auto dictionary = std::get_if<Dictionary>(&capability)) {
    fuchsia_component_test::Dictionary fidl_capability;

    fidl_capability.name(std::string(dictionary->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(dictionary, as, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(dictionary, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithDictionary(std::move(fidl_capability));
  }
  if (auto config = std::get_if<Config>(&capability)) {
    fuchsia_component_test::Config fidl_capability;

    fidl_capability.name(std::string(config->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(config, as, fidl_capability);
    ZX_COMPONENT_ADD_IF_PRESENT(config, availability, fidl_capability);

    return fuchsia_component_test::Capability::WithConfig(std::move(fidl_capability));
  }

  if (auto resolver = std::get_if<Resolver>(&capability)) {
    fuchsia_component_test::Resolver fidl_capability;

    fidl_capability.name(std::string(resolver->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(resolver, as, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(resolver, path, fidl_capability);

    return fuchsia_component_test::Capability::WithResolver(std::move(fidl_capability));
  }

  if (auto runner = std::get_if<Runner>(&capability)) {
    fuchsia_component_test::Runner fidl_capability;

    fidl_capability.name(std::string(runner->name));
    ZX_COMPONENT_ADD_STR_IF_PRESENT(runner, as, fidl_capability);
    ZX_COMPONENT_ADD_STR_IF_PRESENT(runner, path, fidl_capability);

    return fuchsia_component_test::Capability::WithRunner(std::move(fidl_capability));
  }

  ZX_PANIC("ConvertToFidl(Capability) reached unreachable block!");
}

}  // namespace internal
}  // namespace component_testing
