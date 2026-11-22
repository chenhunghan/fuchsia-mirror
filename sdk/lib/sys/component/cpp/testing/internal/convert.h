// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_CONVERT_H_
#define LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_CONVERT_H_

#include <fidl/fuchsia.component.decl/cpp/fidl.h>
#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/realm_builder_types.h>

#include <string>
#include <utility>
#include <vector>

namespace component_testing {
namespace internal {

enum RefContext : uint8_t {
  SOURCE,
  TARGET,
};

using RefPathPair = std::pair<fuchsia_component_decl::Ref, std::string>;

fuchsia_component_test::ChildOptions ConvertToFidl(const ChildOptions& options);
fuchsia_component_test::Capability ConvertToFidl(Capability capability);
// Returns reference and dictionary path (may be ".")
RefPathPair ConvertRefToFidl(const Ref& ref, RefContext ctx);
std::vector<RefPathPair> ConvertRefToFidlVec(const std::vector<Ref>& inputs, RefContext ctx);

template <class Input, class Output>
std::vector<Output> ConvertToFidlVec(std::vector<Input> inputs) {
  std::vector<Output> result;
  result.reserve(inputs.size());
  for (const Input& input : inputs) {
    result.push_back(ConvertToFidl(input));
  }
  return result;
}

}  // namespace internal
}  // namespace component_testing

#endif  // LIB_SYS_COMPONENT_CPP_TESTING_INTERNAL_CONVERT_H_
