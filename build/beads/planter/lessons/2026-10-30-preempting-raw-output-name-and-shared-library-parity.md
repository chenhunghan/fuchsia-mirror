# Preempting raw output name directives, forbidding shared library to fx cc binary abuse, and documenting pruned dependencies

- **Learned from:** review on [CL 1933427](https://fuchsia-review.googlesource.com/c/fuchsia/+/1933427), [CL 1933442](https://fuchsia-review.googlesource.com/c/fuchsia/+/1933442), [CL 1927486](https://fuchsia-review.googlesource.com/c/fuchsia/+/1927486), and reviewer guidance from John Wittrock
- **Date:** 2026-10-30
- **Changed:** `machinery/prompts/coder.md`

## Root cause

Reviewers frequently spend round-trips resolving findings that can be
preempted at authoring time, or catching invalid rule mappings:

1. **`output_name` on raw `fx_cc_binary`**: Bazel's `fx_cc_binary` does not
   accept an `output_name` attribute. When migrating an `executable()` whose
   GN output name differs from its target name (e.g. `executable("bin")` with
   `output_name = "activity_ctl"` or `echo_client_placeholder`) and is not
   packaged via `fx_packaged_binary(binary_name = ...)`, `bazel2gn` drops the
   output name in the generated GN target unless instructed otherwise. Hayden
   Nix demonstrated the clean pattern: adding
   `# @bazel2gn:raw_attribute:output_name = "<name>"` with an explanatory
   inline comment directly above the `fx_cc_binary`.

2. **`shared_library` mapping abuse**: In-tree Fuchsia C/C++ libraries are
   built statically. When migrating a GN `shared_library()`, agents mistakenly
   mapped it to `fx_cc_binary(linkshared = True)`. Because `fx_cc_binary` is
   an executable rule, it does not provide `CcInfo` headers or transitive
   compilation metadata, breaking downstream C++ libraries in Bazel. As John
   Wittrock noted, this is prohibited for library code: in-tree libraries must
   become `fx_cc_library`, system libraries `cc_shared_library_zx`, and drivers
   `fuchsia_cc_driver`. `fx_cc_binary(linkshared = True)` is strictly reserved
   for standalone fixture `.so` files loaded exclusively by tests via `dlopen()`.

3. **Retaining unmigrated packages above sentinel**: When migrating an
   executable `:bin` while an enclosing `fuchsia_package` must remain in GN
   (e.g., waiting for `//bundles/tools` to migrate), the package must be
   retained above the sentinel in `BUILD.gn` with an explicit `TODO` and
   documented in the commit message.

4. **Pruning unused dependencies (`//sdk/lib/fdio`)**: When an `fx_cc_binary`
   does not include `<lib/fdio/...>` directly, dropping explicit `//sdk/lib/fdio`
   is valid dead-code pruning because `fx_cc_binary` links Fuchsia's default
   C runtime. Explaining this in the commit message body preempts reviewer
   questions about dependency drops.

## Why not a one-off fix

Without explicit rules in `coder.md`, agents will continue generating raw
`fx_cc_binary` without `output_name` directives (breaking GN callers and
generating diff viewer warnings), attempting `shared_library` &rarr;
`fx_cc_binary` conversions that break Bazel dependency graphs, and omitting
commit message explanations for pruned dependencies.
