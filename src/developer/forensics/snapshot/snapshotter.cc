// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/snapshot/snapshotter.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>

#include <cstdio>

namespace forensics {
namespace snapshot {

bool MakeSnapshot(std::shared_ptr<sys::ServiceDirectory> services, const char* out_filename) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_feedback::DataProvider>::Create();
  services->Connect(fidl::DiscoverableProtocolName<fuchsia_feedback::DataProvider>,
                    server_end.TakeChannel());
  fidl::SyncClient feedback_data_provider(std::move(client_end));

  fuchsia_feedback::GetSnapshotParameters params;
  params.collection_timeout_per_data(zx::min(1).get());

  const fidl::Result result = feedback_data_provider->GetSnapshot(std::move(params));
  if (result.is_error()) {
    fprintf(stderr, "Failed to get data from fuchsia.feedback.DataProvider: %s\n",
            result.error_value().FormatDescription().c_str());
    return false;
  }

  const fuchsia_feedback::Snapshot& snapshot = result->snapshot();
  if (!snapshot.archive().has_value()) {
    fprintf(stderr, "Failed to get snapshot from fuchsia.feedback.DataProvider");
    return false;
  }

  const uint64_t size = snapshot.archive()->value().size();
  auto data = std::make_unique<uint8_t[]>(size);
  if (const zx_status_t status = snapshot.archive()->value().vmo().read(data.get(), 0u, size);
      status != ZX_OK) {
    fprintf(stderr, "Failed to read VMO archive from fuchsia.feedback.DataProvider");
    return false;
  }

  if (out_filename) {
    FILE* out_file = fopen(out_filename, "w");
    if (!out_file) {
      fprintf(stderr, "Failed to open output file %s\n", out_filename);
      return false;
    }
    fwrite(data.get(), 1, size, out_file);
    fclose(out_file);
    return true;
  } else {
    fwrite(data.get(), 1, size, stdout);
    return true;
  }
}

}  // namespace snapshot
}  // namespace forensics
