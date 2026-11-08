# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""The list of `fx create` template files."""

visibility(["//tools/create/..."])

# All template files available to fx create.
# LINT.IfChange(templates)
TEMPLATES = [
    "_copyright.tmpl",
    "_generated.tmpl",
    "component-default/BUILD.gn.tmpl-cpp",
    "component-default/BUILD.gn.tmpl-rust",
    "component-default/README.md.tmpl",
    "component-default/main.cc.tmpl-cpp",
    "component-default/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-cpp",
    "component-default/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-rust",
    "component-default/src/main.rs.tmpl-rust",
    "component-default/{{snake_case PROJECT_NAME}}.cc.tmpl-cpp",
    "component-default/{{snake_case PROJECT_NAME}}.h.tmpl-cpp",
    "component-default/{{snake_case PROJECT_NAME}}_unittest.cc.tmpl-cpp",
    "component-test/BUILD.gn.tmpl-cpp",
    "component-test/BUILD.gn.tmpl-rust",
    "component-test/README.md.tmpl",
    "component-test/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-cpp",
    "component-test/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-rust",
    "component-test/src/lib.rs.tmpl-rust",
    "component-test/{{snake_case PROJECT_NAME}}.cc.tmpl-cpp",
    "devicetree-visitor/BUILD.gn.tmpl-cpp",
    "devicetree-visitor/test/dts/{{PROJECT_NAME}}.dts.tmpl",
    "devicetree-visitor/test/{{PROJECT_NAME}}-test.cc.tmpl-cpp",
    "devicetree-visitor/{{PROJECT_NAME}}.cc.tmpl-cpp",
    "devicetree-visitor/{{PROJECT_NAME}}.h.tmpl-cpp",
    "devicetree-visitor/{{PROJECT_NAME}}.yaml.tmpl",
    "driver-default/BUILD.bazel.tmpl-cpp",
    "driver-default/BUILD.bazel.tmpl-rust",
    "driver-default/BUILD.gn.tmpl-cpp",
    "driver-default/BUILD.gn.tmpl-rust",
    "driver-default/README.md.tmpl",
    "driver-default/meta/{{snake_case PROJECT_NAME}}.bind.tmpl-cpp",
    "driver-default/meta/{{snake_case PROJECT_NAME}}.bind.tmpl-rust",
    "driver-default/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-cpp",
    "driver-default/meta/{{snake_case PROJECT_NAME}}.cml.tmpl-rust",
    "driver-default/meta/{{snake_case PROJECT_NAME}}_info.json.tmpl-cpp",
    "driver-default/meta/{{snake_case PROJECT_NAME}}_info.json.tmpl-rust",
    "driver-default/src/lib.rs.tmpl-rust",
    "driver-default/tests/{{snake_case PROJECT_NAME}}_test.cc.tmpl-cpp",
    "driver-default/{{snake_case PROJECT_NAME}}.cc.tmpl-cpp",
    "driver-default/{{snake_case PROJECT_NAME}}.h.tmpl-cpp",
]
# LINT.ThenChange(BUILD.gn:templates)
