# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Providers for bind libraries."""

FuchsiaBindLibraryInfo = provider(
    "Contains information about a Bind Library.",
    fields = {
        "name": "Name of the Bind Library.",
        "transitive_sources": "A depset containing transitive sources of the Bind Library.",
    },
)
