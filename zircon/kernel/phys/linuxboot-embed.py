#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import pathlib
import shutil

# See phys-end.ld for where this value comes from.
LINUXBOOT_TRAILER = 0xDEADD00DFEEDFACE.to_bytes(8, byteorder="little")


def padded_size(size: int) -> int:
    return size + (0 if size % 4 == 0 else (4 - (size % 4)))


def padding_for_size(size: int) -> bytes:
    return b"\x00" * (padded_size(size) - size)


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Embed image into Linux (arm64/riscv64) boot shim"
    )
    parser.add_argument(
        "--output",
        type=pathlib.Path,
        required=True,
        help="Output file (image bootable as a Linux kernel)",
    )
    parser.add_argument(
        "--shim",
        type=pathlib.Path,
        required=True,
        help="Linux-compatible boot shim binary",
    )
    parser.add_argument(
        "image",
        metavar="FILE",
        type=pathlib.Path,
        nargs="*",
        help="Image files to concatentate (initramfs, etc.)",
    )
    args = parser.parse_args()

    # Read the whole shim image into memory; it's not too big.
    shim_bytes = args.shim.read_bytes()

    # Extract the embedded memory size value.
    shim_memory_size = int.from_bytes(shim_bytes[16:24], byteorder="little")

    # Check that this looks like a linuxboot phys executable (see phys-end.ld).
    # The trailer is past the load image and into the bss, so it's not part of
    # the actual calculations, and the .bin file always winds up 8 bytes longer
    # than it really needs to be.  Chop that off before pasting.
    assert shim_bytes.endswith(LINUXBOOT_TRAILER)
    shim_bytes = shim_bytes[:-8]

    assert shim_memory_size >= len(shim_bytes)
    shim_bss_size = shim_memory_size - len(shim_bytes)

    # The concatenated images will be added into the total size in memory.
    image_total = sum(padded_size(image.stat().st_size) for image in args.image)
    shim_memory_size += image_total

    # Update the header field in place.
    new_shim_bytes = bytearray(shim_bytes)
    new_shim_bytes[16:24] = shim_memory_size.to_bytes(8, byteorder="little")

    with args.output.open("wb") as output:
        # Write out the whole original shim image, with updated size field.
        output.write(new_shim_bytes)
        if image_total > 0 and shim_bss_size > 0:
            # The bss becomes literal zero bytes in the image so that the
            # rest can be appended.
            output.write(b"\x00" * shim_bss_size)
        # Append on each image file, making sure each image is 4-byte aligned.
        for image in args.image:
            output.write(padding_for_size(output.tell()))
            with image.open("rb") as f:
                shutil.copyfileobj(f, output)


if __name__ == "__main__":
    main()
