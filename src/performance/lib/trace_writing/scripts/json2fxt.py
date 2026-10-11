# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Host tool and library for converting legacy JSON traces into FXT format."""

import argparse
import json
import os
from typing import Any

from trace_writing import fxt


class FxtConverter:
    """Encapsulates JSON trace file conversion to Fuchsia's binary FXT format."""

    def __init__(self) -> None:
        """Initialize a new FxtConverter instance."""
        self._builder = fxt.Builder()

    @staticmethod
    def _align(size: int) -> int:
        return fxt.Builder._align(size)

    @staticmethod
    def _pad_bytes(b: bytes) -> bytes:
        return fxt.Builder._pad_bytes(b)

    @staticmethod
    def _make_string_ref(s: str) -> int:
        return fxt.Builder._make_string_ref(s)

    @staticmethod
    def _pack_word(val: int) -> bytes:
        return fxt.Builder._pack_word(val)

    @staticmethod
    def _pack_argument(name: str, val: Any) -> bytes:
        return fxt.Builder._pack_argument(name, val)

    @staticmethod
    def _write_word(val: int) -> bytes:
        return fxt.Builder._pack_word(val)

    @staticmethod
    def _write_argument(name: str, val: Any) -> bytes:
        return fxt.Builder._pack_argument(name, val)

    def convert(
        self,
        json_file: str | os.PathLike[Any],
        fxt_file: str | os.PathLike[Any],
    ) -> None:
        """Convert a JSON trace file to FXT format.

        Args:
            json_file: Path to the input Chrome JSON trace file.
            fxt_file: Path where the output binary FXT file will be written.

        Raises:
            OSError: If reading input or writing output fails.
            json.JSONDecodeError: If json_file contains invalid JSON.
        """
        with open(json_file, "r", encoding="utf-8") as f:
            data = json.load(f)
        builder = fxt.Builder.from_json_dict(data)
        builder.write_to_file(fxt_file)


def main() -> None:
    """CLI entry point for converting JSON trace files to FXT format."""
    parser = argparse.ArgumentParser(description="Convert JSON trace to FXT")
    parser.add_argument("json_file", help="Path to input JSON file")
    parser.add_argument("fxt_file", help="Path to output FXT file")
    args = parser.parse_args()

    converter = FxtConverter()
    converter.convert(args.json_file, args.fxt_file)
    print(f"Successfully converted {args.json_file} to {args.fxt_file}")


if __name__ == "__main__":
    main()
