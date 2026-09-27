"""TODO: digit - Write module docstring."""

load("@fuchsia_build_info//:args.bzl", "compilation_mode")

is_release = compilation_mode == "release"
