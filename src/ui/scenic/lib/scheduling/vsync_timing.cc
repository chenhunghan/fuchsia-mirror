// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/scheduling/vsync_timing.h"

namespace scheduling {

VsyncTiming::VsyncTiming() { FX_DCHECK(vsync_interval().get() > 0); }

}  // namespace scheduling
