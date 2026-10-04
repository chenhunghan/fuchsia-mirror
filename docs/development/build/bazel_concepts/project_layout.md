# Bazel project layout and organization

## Bazel projects

A Bazel project is a collection of source and build files that describe:

- How to *build* artifacts, like binaries or data files, and their dependencies.
- How to *run* specific commands, i.e. scripts or executables.
- How to *test* said build artifacts.

A Bazel project is materialized by _a top-level directory_, whose
content follows a _specific layout_ and _conventions_.

Note: Bazel does not store build artifacts in the project's directory.
      For more details, see [this page][bazel-build-outputs].

## Bazel workspaces

A Bazel workspace is a directory tree that contains a top-level
`WORKSPACE.bazel`[^1] file. This defines the root of a collection of sources
and related build files. A Bazel project can use several workspaces:

- The project's root directory is called the _root_ _workspace_, and thus must
  contain a `WORKSPACE.bazel` file.

- A project can also reference _other workspaces_, called _external_
  _repositories_, which correspond to third-party project dependencies.

For example, in:

```
/home/user/project/
    WORKSPACE.bazel
    src/
        BUILD.bazel
        extra/
            extra.cc
        lib/
            BUILD.bazel
            foo.cc
            foo.h
        main.cc
```

The directory `/home/user/project` is a root Bazel workspace, which contains
all the files inside it.

The `WORKSPACE.bazel` file can be empty, but can also contain directives
that reference other workspaces, as explained later.

[^1]: For legacy reasons, this file can also be simply called `WORKSPACE`

## Bazel packages

Within a workspace, a directory that contains a `BUILD.bazel`[^2] file defines
a _package_, which is a boundary around a collection of source files and items
that Bazel knows about.

For example, this file layout:

```
/home/user/project/
  WORKSPACE.bazel
  BUILD.bazel
  main.cc
```

Defines a root workspace, located at `/home/user/project` with a single
top-level package, which contains the files `BUILD.bazel` and `main.cc`.

The `BUILD.bazel` file can also contain directives to define named _items_,
such as targets, config conditions and others, that also technically belong
to the package.

Several packages can exist in a single workspace, and _each file can only
belong to one package_. For example, with the following file layout:

```
/home/user/project/
  WORKSPACE.bazel
  BUILD.bazel
  main.cc
  lib/
    BUILD.bazel
    foo.cc
```

The root workspace contains two different packages:

- The top-level package, which still contains the files `BUILD.bazel`
  and `main.cc` (relative to the root workspace directory).

- A second package, which contains the files `lib/BUILD.bazel` and
  `lib/foo.cc`.

Note that the file at `/home/user/project/lib/foo.cc` only belongs
to the second package, not to the first one. This is because
_package_ _boundaries_ _never_ _overlap_.

[^2]: Also for legacy reasons, the file can also be simply called `BUILD`.

## `BUILD.bazel` versus `BUILD`

Bazel originates from Google's Blaze, which only works on Linux with
case-sensitive filesystems. Blaze only used the file name `BUILD` to store
build directives.

However, Bazel also needs to run on Windows which has
_case-insensitive_ filesystems, and many Google, or non-Google projects
already use a directory named "`build`", which then collides with a file
named "`BUILD`" on such systems.

To solve the issue, Bazel uses `BUILD.bazel` and `WORKSPACE.bazel` as the
default file names, while still supporting `BUILD` and `WORKSPACE` as fallbacks.

Note: For portability, prefer `BUILD.bazel` over `BUILD`, and `WORKSPACE.bazel`
      over `WORKSPACE` in your own Bazel projects.

## Workspace directives

The root `WORKSPACE.bazel` can be empty, or it can contain directives which
reference other Bazel workspaces, which are called _external repositories_.
These directives always give a name to the repository. For example:

```py
local_repository(
  name = "my_ssl",
  path = "/home/user/src/openssl-bazel",
)
```

Associates the name `my_ssl` with the workspace located at
`/home/user/src/openssl-bazel` on the build machine. This directory must also
contain a `WORKSPACE.bazel` (or `WORKSPACE`) file.

A repository is just an external Bazel workspace with a name. This name is
local to your project, and can later be used to reference items from the
external workspace (see below).

Bazel also supports other directives to download repositories from the
network, or even generate their content programmatically.

## Bazel labels

See [this page][bazel-labels] for an introduction to Bazel labels.

## Bazel extension (`.bzl`) files

See [this page][bazel-bzl-files] for an introduction to Bazel extension files.

[bazel-build-outputs]: /docs/development/build/bazel_concepts/build_outputs.md
[bazel-external-repositories]: /docs/development/build/bazel_concepts/external_repositories.md
[bazel-labels]: /docs/development/build/bazel_concepts/labels.md
[bazel-bzl-files]: /docs/development/build/bazel_concepts/bzl_files.md
