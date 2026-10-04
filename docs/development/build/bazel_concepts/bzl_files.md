# Bazel extension (`.bzl`) files

Extension files contain extra definitions that can be imported into several
other files:

- Their name always ends with the `.bzl` file extension.

- They must belong to a Bazel package, and hence identified by a label.
  For example `//bazel_utils:defs.bzl`.

- They are written in the [Starlark language][starlark-language]{:.external},
  and should follow [specific guidelines][bzl-style-guide]{:.external}.

- They are the _only_ place where Starlark functions can be defined!
  In other words, one cannot define a function in a `BUILD.bazel` file!

- They are always _evaluated once_, even if they are imported multiple times,
  and the variables and functions they define are recorded as constants.

- They can be imported from other files using the
  [`load()`][bazel-load]{:.external} statement.

For example:

- From `$PROJECT/my_definitions.bzl`:

  ```py
  # The official release number
  release_version = "1.0.0"
  ```

- From `$PROJECT/BUILD.bazel`:

  ```py
  # Import the value of `release_version` from my_definitions.bzl
  load("//:my_definitions.bzl", "release_version")

  # Compile C++ executable, hard-coding its version number with a macro.
  cc_binary(
    name = "my_program",
    defines = [ "RELEASE_VERSION=" + release_version ],
    sources = [ … ],
  )
  ```

The [`load()`][bazel-load]{:.external} statement has special semantics:

- Its first argument must be a label string to a `.bzl` file (e.g.
  `"//src:definitions.bzl"`).

- Other arguments name imported constants or functions:

    - If the argument is a string, it must be the name of an imported symbol
      defined by the `.bzl` file. e.g.:

      ```py
      load("//src:defs.bzl", "my_var", "my_func")
      ```

    - If the argument is a variable assignment, it defines a local alias for an
      imported symbol. E.g.:

      ```py
      load("//src:defs.bzl", "my_var", func = "my_func")
      ```

    - _There are no wildcards_: all imported constants and functions must be
      named explicitly.

    - Imported symbols are never recorded when the `load()` appears within a
      `.bzl` file.

    - Similarly, symbols whose name begins with and underscore (e.g. `_foo`) are
      never recorded, and cannot be imported. I.e. they are private to the `.bzl`
      file that defines them.

Sometimes a `.bzl` file wants to import a symbol from another one, and
re-export it with the same name. This requires an alias as in:

```py
# From //src:utils.bzl

# Import "my_vars" from defs.bzl as '_my_var'.
load("//src:defs.bzl", _my_var = "my_var")

# Define my_var in the current scope as a copy of _my_var
# This symbol and its value will be recorded, and available for import
# to any other file that loads //src:utils.bzl.
my_var = _my_var
```

[starlark-language]: https://bazel.build/rules/language
[bzl-style-guide]: https://bazel.build/rules/bzl-style
[bazel-load]: https://bazel.build/concepts/build-files#load
