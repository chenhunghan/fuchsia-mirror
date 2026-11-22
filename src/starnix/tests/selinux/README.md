## SELinux tests for Starnix

### `bench`

This directory contains latency and memory microbenchmarks for SEStarnix
(`selinux_bench` and `sestarnix_memory_bench`). See
[bench/README.md](bench/README.md) for what each benchmark measures and how to
interpret results as diagnostic signals vs. optimization goals.

### `expectations`

This directory contains expectations for the `vendor/google` SELinux tests.
Please see [//vendor/google/starnix/tests/selinux/README.md](../../../../vendor/google/starnix/tests/selinux/README.md)
for more information about these tests.

### `userspace`

This directory contains userspace tests.
