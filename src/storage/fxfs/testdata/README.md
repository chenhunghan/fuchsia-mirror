# Fxfs Golden Images

This directory contains a collection of "golden" filesystem images that have
been compressed with zstd.

These images are used to ensure that the latest version of Fxfs can continue to
read filesystem images written by previous versions of the code.

The images themselves are generated with `fx fxfs create_golden` by developers
whenever the on-disk Fxfs version is changed. A unit test ensures this step is
not forgotten and provides instructions if it is.

## images.gni and images.bzl

These are auto-generated files. Both are rewritten by `fx fxfs create_golden`
and include all images found in the output directory: `images.gni` for GN and
`images.bzl` for Bazel.

They are required because neither build system lets us use wildcard
dependencies here, but we need a way to ensure all images are included in tests
run on CQ. If you remove old images by hand, update both files.

## House keeping

We have a hard requirement that an image exists for the *current* version of
Fxfs but beyond that, engineers are free to manually remove old images as they see fit.

