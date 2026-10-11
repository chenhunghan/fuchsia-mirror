// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_SCENE_DUMPER_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_SCENE_DUMPER_H_

#include <ostream>
#include <span>

#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"

namespace flatland {

// Dumps information about a flatland scene to an output stream.
void DumpScene(const UberStruct::InstanceMap& snapshot, const GlobalTopologyData& topology_data,
               std::span<const ResolvedLayer> layers, std::ostream& output);

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_SCENE_DUMPER_H_
