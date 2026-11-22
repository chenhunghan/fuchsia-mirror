// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.test/cpp/fidl.h>
#include <fidl/fuchsia.component/cpp/fidl.h>
#include <lib/sys/component/cpp/testing/internal/errors.h>
#include <zircon/assert.h>
#include <zircon/status.h>

namespace component_testing {
namespace internal {

const char* ConvertToString(fuchsia_component_test::RealmBuilderError error) {
  switch (error) {
    case fuchsia_component_test::RealmBuilderError::kChildAlreadyExists:
      return "CHILD_ALREADY_EXISTS";
    case fuchsia_component_test::RealmBuilderError::kInvalidManifestExtension:
      return "INVALID_MANIFEST_EXTENSION";
    case fuchsia_component_test::RealmBuilderError::kInvalidComponentDecl:
      return "INVALID_COMPONENT_DECL";
    case fuchsia_component_test::RealmBuilderError::kNoSuchChild:
      return "NO_SUCH_CHILD";
    case fuchsia_component_test::RealmBuilderError::kChildDeclNotVisible:
      return "CHILD_DECL_NOT_VISIBLE";
    case fuchsia_component_test::RealmBuilderError::kNoSuchSource:
      return "NO_SUCH_SOURCE";
    case fuchsia_component_test::RealmBuilderError::kNoSuchTarget:
      return "NO_SUCH_TARGET";
    case fuchsia_component_test::RealmBuilderError::kCapabilitiesEmpty:
      return "CAPABILITIES_EMPTY";
    case fuchsia_component_test::RealmBuilderError::kTargetsEmpty:
      return "TARGETS_EMPTY";
    case fuchsia_component_test::RealmBuilderError::kSourceAndTargetMatch:
      return "SOURCE_AND_TARGET_MATCH";
    case fuchsia_component_test::RealmBuilderError::kDeclNotFound:
      return "DECL_NOT_FOUND";
    case fuchsia_component_test::RealmBuilderError::kDeclReadError:
      return "DECL_READ_ERROR";
    case fuchsia_component_test::RealmBuilderError::kBuildAlreadyCalled:
      return "BUILD_ALREADY_CALLED";
    case fuchsia_component_test::RealmBuilderError::kCapabilityInvalid:
      return "CAPABILITY_INVALID";
    case fuchsia_component_test::RealmBuilderError::kInvalidChildRealmHandle:
      return "INVALID_CHILD_REALM_HANDLE";
    case fuchsia_component_test::RealmBuilderError::kImmutableProgram:
      return "IMMUTABLE_PROGRAM";
    case fuchsia_component_test::RealmBuilderError::kUrlIsNotRelative:
      return "URL_IS_NOT_RELATIVE";
    case fuchsia_component_test::RealmBuilderError::kInvalidPkgDirHandle:
      return "INVALID_PKG_DIR_HANDLE";
    case fuchsia_component_test::RealmBuilderError::kNoConfigSchema:
      return "NO_CONFIG_SCHEMA";
    case fuchsia_component_test::RealmBuilderError::kNoSuchConfigField:
      return "NO_SUCH_CONFIG_FIELD";
    case fuchsia_component_test::RealmBuilderError::kConfigValueInvalid:
      return "CONFIG_VALUE_INVALID";
    case fuchsia_component_test::RealmBuilderError::kConfigOverrideUnsupported:
      return "CONFIG_OVERRIDE_UNSUPPORTED";
    default:
      return "UNKNOWN";
  }
}

const char* ConvertToString(fuchsia_component::Error error) {
  switch (error) {
    case fuchsia_component::Error::kInternal:
      return "INTERNAL";
    case fuchsia_component::Error::kInvalidArguments:
      return "INVALID_ARGUMENTS";
    case fuchsia_component::Error::kUnsupported:
      return "UNSUPPORTED";
    case fuchsia_component::Error::kAccessDenied:
      return "ACCESS_DENIED";
    case fuchsia_component::Error::kInstanceNotFound:
      return "INSTANCE_NOT_FOUND";
    case fuchsia_component::Error::kInstanceAlreadyExists:
      return "INSTANCE_ALREADY_EXISTS";
    case fuchsia_component::Error::kInstanceCannotStart:
      return "INSTANCE_CANNOT_START";
    case fuchsia_component::Error::kInstanceCannotResolve:
      return "INSTANCE_CANNOT_RESOLVE";
    case fuchsia_component::Error::kCollectionNotFound:
      return "COLLECTION_NOT_FOUND";
    case fuchsia_component::Error::kResourceUnavailable:
      return "RESOURCE_UNAVAILABLE";
    case fuchsia_component::Error::kInstanceDied:
      return "INSTANCE_DIED";
    case fuchsia_component::Error::kResourceNotFound:
      return "RESOURCE_NOT_FOUND";
    case fuchsia_component::Error::kInstanceCannotUnresolve:
      return "INSTANCE_CANNOT_UNRESOLVE";
    case fuchsia_component::Error::kInstanceAlreadyStarted:
      return "INSTANCE_ALREADY_STARTED";
    case fuchsia_component::Error::kDependencyCycle:
      return "DEPENDENCY_CYCLE";
    default:
      return "UNKNOWN";
  }
}

void PanicWithMessage(const char* stacktrace, const char* context, zx_status_t status) {
  ZX_PANIC("[%s] FIDL method %s failed with status: %s", stacktrace, context,
           zx_status_get_string(status));
}

void PanicWithMessage(const char* stacktrace, const char* context,
                      fuchsia_component_test::RealmBuilderError error) {
  ZX_PANIC("[%s] FIDL method %s failed with error: %s", stacktrace, context,
           ConvertToString(error));
}

void PanicWithMessage(const char* stacktrace, const char* context, fuchsia_component::Error error) {
  ZX_PANIC("[%s] FIDL method %s failed with error: %s", stacktrace, context,
           ConvertToString(error));
}

void PanicWithFidlError(const char* stacktrace, const char* context, const fidl::Status& error) {
  PanicWithMessage(stacktrace, context, error.status());
}

}  // namespace internal
}  // namespace component_testing
