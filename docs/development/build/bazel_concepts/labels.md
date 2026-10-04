# Bazel labels

Bazel labels are _string references_ to source files and items defined in
`BUILD.bazel` files. Their general format is:

```
@<repository_name>//<package_name>:<target_name>
```

Where:

- `@<repository_name>//` designates the directory of a named Bazel workspace.

  As a convenience, this can be abbreviated as simply **`//`** for the current
  workspace (the one that contains the current `BUILD.bazel` file). Note also
  that **`@//`** is used to designate the project's root workspace, even when
  used in external repositories.

- `<package_name>` is the package's directory path, relative to the
  workspace directory. For example, in the labels `//src:main.cc` or
  `//src/lib:foo`, the package names are `src` and `src/lib` respectively.

  This can be empty, e.g. `//:BUILD.bazel` points to the build file in the
  current workspace's top-level directory.

- For source files, `<target_name>` is the file path relative to its parent
  package's directory, and may include a sub-directory part. For example for
  `//src:main.cc`or `//src:extra/extra.cc`, the target name is `main.cc` and
  `extra/extra.cc` respectively.

  Note: Bazel _target names_ can include sub-directories, which are always
  relative to the package they belong to.

- For other items, `<target_name>` corresponds to an item (build artifact,
  build setting, configuration condition, etc) defined in a `BUILD.bazel`
  file.

  By convention, its `name` attribute  _should not include a directory
  separator_, except in very rare cases, to avoid confusion with sources.

  Note: The Bazel documentation uses the term _target_ to refer to _any_
  item that can be reached with a label. Many of these _do not correspond to_
  _buildable artifacts_ (source files, build variable definitions,
  configuration conditions, platform constraints, and many more).

  This can be confusing to developers coming from other build systems which
  differentiate the type of items in their build graph (e.g. `GN` uses
  "Targets", "Configs", "Toolchains" and "Pools" to designate different
  things).

Shortened expressions for labels are also supported:

- If the label begins with a repository name  and does not include a colon,
  it is a package path, and points to an item with the same name.
  For example `//src/foo` is equivalent to `//src/foo:foo`.

- If the label begins with a colon, it is a name relative to the current
  package. For example "`:bar`" and "`:extra/bar.cc`" that appear in
  `src/foo/BUILD.bazel` are equivalent to `//src/foo:bar` and
  `//src/foo:extra/bar.cc` respectively.

- If the label has no repository name and no colon, it is always a name
  relative to the current package, even if it includes a directory separator.
  E.g. "`bar/bar.cc`" in `src/foo/BUILD.bazel` always refer to
  `//src/foo:bar/bar.cc`.

  Note that this is not the same as `//src/foo/bar:bar.cc`

## Relative labels and package ownership

Since each source file can only belong to a single package, relative labels can
be invalid. For example, in a project that looks like the following:

```
/home/user/project/
    WORKSPACE.bazel
    src/
        BUILD.bazel
        main.cc
        extra/
            extra.cc
        lib/
            BUILD.bazel
            foo.cc
            foo.h

```

The `foo.cc` file belongs to the package `src/lib`, so its label _must be_
`//src/lib:foo.cc`.

Using a label like `src:lib/foo.cc` in `src/BUILD.bazel` is an error:

```py
# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "lib/foo.cc",       # Error: Label '//src:lib/foo.cc' is invalid because 'src/lib' is a subpackage
    "lib/foo.h"         # Error: Label '//src:lib/foo.h' is invalid because 'src/lib' is a subpackage
    "main.cc",
  ],
)
```

## Source file access from other packages

By default, the source files of a given package cannot be accessed from
other packages, and _relative package labels are invalid_, as in:

```py
# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "lib:foo.cc",    # Error: invalid label 'lib:foo.cc': absolute label must begin with '@' or '//'
    "lib:foo.h"      # Error: invalid label 'lib:foo.h': absolute label must begin with '@' or '//'
    "main.cc",
  ],
)
```

And even when using the right absolute label, and error happens:

```py
# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "//src/lib:foo.cc",    # Error: no such target '//src/lib:foo.cc': target 'foo.h' not declared in package 'src/lib'
    "//src/lib:foo.h"      # Error: no such target '//src/lib:foo.h': target 'foo.h' not declared in package 'src/lib'
    "main.cc",
  ],
)
```

Direct access to files across package boundaries can be granted by `export_files()`:

```py
# From src/lib/BUILD.bazel
export_files([
  "foo.cc" ,
  "foo.h" ,
])

# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "//src/lib:foo.cc",    # OK
    "//src/lib:foo.h"      # OK
    "main.cc",
  ],
)
```

## Target access from other packages

Labelled items defined in a `BUILD.bazel`  file that are not source files
need no export, but their `visibility` attribute must allow their use
outside of their own package:

```py
# From src/lib/BUILD.bazel
cc_library(
  name = " lib" ,
  srcs = [ " foo.cc"  ],
  hdrs = [ " foo.h"  ],
  visibility = [ " //visibility:public" ],  # Anyone can reference this directly!
)

# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "main.cc",
  ],
  deps = [ "lib" ],   # OK!
)
```

By default, items are only visible to other items in the same package.
This can be changed by using a [`package()`][bazel-package]{:.external}
directive to change the default visibility of all items defined in a package:

```py
# From src/lib/BUILD.bazel

# Ensure that all items defined in this file are visible to anyone
package(default_visibility = ["//visibility:public"])

cc_library(
  name = " lib" ,
  srcs = [ "foo.cc" ],
  hdrs = [ "foo.h" ],
)

# From src/BUILD.bazel
cc_binary(
  name = "program",
  srcs = [
    "extra/extra.cc",
    "main.cc",
  ],
  deps = [ "lib" ],   # OK!
)
```

## A warning about virtual packages

Avoid creating top-level directories in a project with the following names:

- `conditions`
- `command_line_option`
- `external`
- `visibility`

Because Bazel uses a number of _hard-coded "virtual packages"_ in labels within
`BUILD.bazel` files. For example:

```py
  //visibility:public
  //conditions:default
  //command_line_option:copt
```

The case of `external` is a bit different: it does not appear in
`BUILD.bazel` files, but used internally to manage external repositories.
[This confuses Bazel when used as a project directory][bazel-external-bug]{:.external}

## Canonical repository names

Since Bazel 6.0, repository names in labels can also begin with **`@@`**.

When the optional [BzlMod][bzlmod] feature is enabled, these labels
are used as alternative but _unique_ label names for external repositories,
which becomes important when complex transitive dependency trees are used in a
project.

For example, `@@com_acme_anvil.1.0.3` could be a canonical name for the
workspace directory identified by `@anvil` in the project's own `BUILD.bazel`
files, and by `@acme_anvil` when it appears in an external repository
(e.g. inside `@foo//:BUILD.bazel`). All three labels would refer to the content
of the same directory.

Canonical repository names do not appear in `BUILD.bazel` files, however, they
will appear during the analysis phase (when executing Starlark functions that
look at label values), or when looking at the result of Bazel queries.

[bazel-package]: https://bazel.build/reference/be/functions#package
[bazel-external-bug]: https://github.com/bazelbuild/bazel/issues/16220
[bzlmod]: https://bazel.build/external/overview#bzlmod
