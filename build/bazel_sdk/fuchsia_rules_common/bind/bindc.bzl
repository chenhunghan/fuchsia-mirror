# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Rule implementation helper for generating bind library bindings with bindc."""

def bindc_generate(ctx, *, bindc, language, library_name, src, mnemonic, toolchain = None):
    """Declares a rule action that runs `bindc generate-cpp` or `bindc generate-rust`.

    The output is `{target_name}/bind/{library/name/with/slashes}/cpp/bind.h`
    for C++, so that it can be included as
    `<bind/{library/name/with/slashes}/cpp/bind.h>`, and
    `{target_name}/rust/{library_name_with_underscores}_lib.rs` for Rust.

    Args:
      ctx: The rule context.
      bindc: The bindc executable.
      language: "cpp" or "rust".
      library_name: Name of the bind library, e.g. `fuchsia.example`.
      src: The `.bind` source file of the library.
      mnemonic: The mnemonic of the action.
      toolchain: The toolchain type that provides `bindc`, if any.

    Returns:
      The generated File.
    """
    if language == "cpp":
        out = ctx.actions.declare_file("%s/bind/%s/cpp/bind.h" % (ctx.label.name, library_name.replace(".", "/")))
    elif language == "rust":
        out = ctx.actions.declare_file("%s/rust/%s_lib.rs" % (ctx.label.name, library_name.replace(".", "_")))
    else:
        fail("Unknown bind library bindings language: %s" % language)

    ctx.actions.run(
        executable = bindc,
        arguments = [
            "generate-" + language,
            "--lint",
            "--output",
            out.path,
            src.path,
        ],
        inputs = [src],
        outputs = [out],
        mnemonic = mnemonic,
        toolchain = toolchain,
    )
    return out
