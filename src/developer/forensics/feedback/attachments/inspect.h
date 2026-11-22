// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_FEEDBACK_ATTACHMENTS_INSPECT_H_
#define SRC_DEVELOPER_FORENSICS_FEEDBACK_ATTACHMENTS_INSPECT_H_

#include <fidl/fuchsia.diagnostics/cpp/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fpromise/promise.h>
#include <lib/sys/cpp/service_directory.h>

#include <memory>

#include "src/developer/forensics/feedback/attachments/provider.h"
#include "src/developer/forensics/feedback/attachments/types.h"
#include "src/developer/forensics/feedback_data/inspect_data_budget.h"
#include "src/developer/forensics/utils/redact/redactor.h"
#include "src/lib/backoff/backoff.h"
#include "src/lib/fxl/memory/weak_ptr.h"

namespace forensics::feedback {

// Collects the Inspect data.
//
// fuchsia.diagnostics.ArchiveAccessor.feedback is expected to be in |services|.
class Inspect : public AttachmentProvider,
                public fidl::AsyncEventHandler<fuchsia_diagnostics::ArchiveAccessor> {
 public:
  Inspect(async_dispatcher_t* dispatcher, std::shared_ptr<sys::ServiceDirectory> services,
          std::unique_ptr<backoff::Backoff> backoff, feedback_data::InspectDataBudget* data_budget,
          RedactorBase* redactor);

  // Returns a promise to the inspect data and allows collection to be terminated early with
  // |ticket|.
  ::fpromise::promise<AttachmentData> Get(uint64_t ticket) override;

  // Completes the inspect data collection promise associated with |ticket| early, if it hasn't
  // already completed.
  void ForceCompletion(uint64_t ticket, Error error) override;

  void on_fidl_error(fidl::UnbindInfo error) override;

  void handle_unknown_event(
      fidl::UnknownEventMetadata<fuchsia_diagnostics::ArchiveAccessor> metadata) override;

 private:
  void Connect();

  async_dispatcher_t* dispatcher_;
  std::shared_ptr<sys::ServiceDirectory> services_;
  std::unique_ptr<backoff::Backoff> backoff_;
  feedback_data::InspectDataBudget* data_budget_;
  RedactorBase* redactor_;

  fidl::Client<fuchsia_diagnostics::ArchiveAccessor> archive_accessor_;

  std::map<uint64_t, ::fit::callback<void(std::optional<Error>)>> completers_;

  fxl::WeakPtrFactory<Inspect> ptr_factory_{this};
};

}  // namespace forensics::feedback

#endif  // SRC_DEVELOPER_FORENSICS_FEEDBACK_ATTACHMENTS_INSPECT_H_
