# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Common devicetree preprocessing helper."""

def preprocess_dts(
        ctx,
        *,
        preprocessor,
        source,
        output_file,
        includes = depset(),
        quote_includes = depset(),
        system_includes = depset(),
        defines = depset(),
        extra_include_dirs = [],
        inputs = depset(),
        mnemonic = "DtsPreprocess",
        progress_message = None):
    """Preprocesses a `.dts.S` or `.dtsi.S` source file with Clang.

    Args:
      ctx: The rule context.
      preprocessor: The `clang-cpp` executable File.
      source: The `.S` source File to preprocess.
      output_file: The declared preprocessed output File.
      includes: Depset of `-I` include directory paths.
      quote_includes: Depset of `-iquote` include directory paths.
      system_includes: Depset of `-isystem` include directory paths.
      defines: Depset of preprocessor `-D` define strings.
      extra_include_dirs: Additional `-I` directory strings prepended before `includes`.
      inputs: Depset of additional input Files beyond `[source]`.
      mnemonic: Action mnemonic.
      progress_message: Optional action progress message.

    Returns:
      The preprocessed `output_file`.
    """
    pp_args = ctx.actions.args()
    pp_args.add("-undef")
    pp_args.add("-P")
    pp_args.add_all(extra_include_dirs, before_each = "-I")
    pp_args.add_all(includes, before_each = "-I")
    pp_args.add_all(quote_includes, before_each = "-iquote")
    pp_args.add_all(system_includes, before_each = "-isystem")
    pp_args.add_all(defines, format_each = "-D%s")
    pp_args.add("-x", "assembler-with-cpp")
    pp_args.add(source)
    pp_args.add("-o", output_file)

    run_kwargs = dict(
        executable = preprocessor,
        arguments = [pp_args],
        inputs = depset(direct = [source], transitive = [inputs]),
        outputs = [output_file],
        mnemonic = mnemonic,
    )
    if progress_message != None:
        run_kwargs["progress_message"] = progress_message

    ctx.actions.run(**run_kwargs)
    return output_file
