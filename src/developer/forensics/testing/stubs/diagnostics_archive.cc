// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/stubs/diagnostics_archive.h"

namespace forensics {
namespace stubs {

void DiagnosticsArchive::StreamDiagnostics(StreamDiagnosticsRequest& request,
                                           StreamDiagnosticsCompleter::Sync& completer) {
  if (request.stream_parameters().format().has_value()) {
    batch_iterator_->SetFormat(request.stream_parameters().format().value());
  }
  batch_iterator_->Bind(std::move(request.result_stream()), dispatcher_);
}

void DiagnosticsArchiveSequentialIterators::StreamDiagnostics(
    StreamDiagnosticsRequest& request, StreamDiagnosticsCompleter::Sync& completer) {
  FX_CHECK(current_iterator_index_ < batch_iterators_.size());
  if (request.stream_parameters().format().has_value()) {
    batch_iterators_[current_iterator_index_]->SetFormat(
        request.stream_parameters().format().value());
  }
  batch_iterators_[current_iterator_index_++]->Bind(std::move(request.result_stream()),
                                                    dispatcher_);
}

void DiagnosticsArchiveClosesIteratorConnection::StreamDiagnostics(
    StreamDiagnosticsRequest& request, StreamDiagnosticsCompleter::Sync& completer) {
  request.result_stream().Close(ZX_ERR_PEER_CLOSED);
}

}  // namespace stubs
}  // namespace forensics
