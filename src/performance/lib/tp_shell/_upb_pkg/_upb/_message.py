# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Loader shim for the packaged `google._upb._message` C extension."""

import atexit
import contextlib
import importlib.machinery
import importlib.resources
import importlib.util
import sys
import types

_EXIT_STACK = contextlib.ExitStack()
atexit.register(_EXIT_STACK.close)


def _load_upb_extension() -> types.ModuleType:
    resource = importlib.resources.files("google._upb_bin").joinpath(
        "_upb_message.so"
    )
    so_path = _EXIT_STACK.enter_context(importlib.resources.as_file(resource))
    loader = importlib.machinery.ExtensionFileLoader(
        "google._upb._message", str(so_path)
    )
    spec = importlib.util.spec_from_loader("google._upb._message", loader)
    if spec is None:
        raise ImportError(
            f"Failed to create module spec for google._upb._message from {so_path}"
        )
    mod = importlib.util.module_from_spec(spec)
    sys.modules["google._upb._message"] = mod
    loader.exec_module(mod)
    return mod


_MOD = _load_upb_extension()
globals().update(_MOD.__dict__)
