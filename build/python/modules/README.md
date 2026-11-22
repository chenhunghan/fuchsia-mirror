# Build-Time Python Modules

These are python modules for use by build-time scripts, which help codify
various formats and conventions.

- [`assembly`](assembly/) - A module for working with various configuration
  file formats that are used by the image and product assembly operations.

- [`bazel_labels`](bazel_labels/) - A module for normalizing Bazel labels,
  e.g. ones passed on the command line.

- [`depfile`](depfile/) - A module for writing depfiles that ninja can read.

- [`serialization`](serialization/) - A module for serializing/deserializing
  @dataclass objects to/from JSON.
