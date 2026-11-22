# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Providers for Fuchsia devicetree rules."""

FuchsiaDeviceTreeSegmentInfo = provider(
    doc = "Contains information about a Fuchsia devicetree fragment (`.dtsi` or `.dtsi.S`).",
    fields = {
        "includes": "A depset of include directory paths used when compiling the devicetree binary.",
        "files": "A depset of transitive dependencies needed for future devicetree compile.",
    },
)
