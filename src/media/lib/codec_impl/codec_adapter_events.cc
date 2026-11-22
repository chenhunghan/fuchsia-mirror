// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.media/cpp/hlcpp_conversion.h>
#include <lib/media/codec_impl/codec_adapter_events.h>
#include <zircon/assert.h>

void CodecAdapterEvents::onCoreCodecFailStream(fuchsia_media::StreamError error) {
  onCoreCodecFailStream(fidl::NaturalToHLCPP(error));
}

void CodecAdapterEvents::onCoreCodecFailStream(fuchsia::media::StreamError error) {
  ZX_PANIC("onCoreCodecFailStream not implemented");
}
