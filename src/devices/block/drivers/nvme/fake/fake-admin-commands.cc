// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/nvme/fake/fake-admin-commands.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/fake-bti/bti.h>
#include <lib/fzl/vmo-mapper.h>

#include <sdk/lib/driver/logging/cpp/logger.h>

#include "src/devices/block/drivers/nvme/commands.h"
#include "src/devices/block/drivers/nvme/commands/features.h"
#include "src/devices/block/drivers/nvme/commands/identify.h"
#include "src/devices/block/drivers/nvme/commands/queue.h"
#include "src/devices/block/drivers/nvme/fake/fake-controller.h"
#include "src/devices/block/drivers/nvme/queue-pair.h"
#include "src/lib/testing/predicates/status.h"

namespace fake_nvme {
using nvme::Completion;
using nvme::GenericStatus;
using nvme::StatusCodeType;

FakeAdminCommands::FakeAdminCommands(FakeController& controller) : controller_(controller) {
  controller.AddDefaultAdminCommand(nvme::AdminCommandOpcode::kIdentify,
                                    fit::bind_member(this, &FakeAdminCommands::Identify));
  controller.AddDefaultAdminCommand(
      nvme::AdminCommandOpcode::kCreateIoCompletionQueue,
      fit::bind_member(this, &FakeAdminCommands::ConfigureIoCompletionQueue));
  controller.AddDefaultAdminCommand(
      nvme::AdminCommandOpcode::kCreateIoSubmissionQueue,
      fit::bind_member(this, &FakeAdminCommands::ConfigureIoSubmissionQueue));
  controller.AddDefaultAdminCommand(nvme::AdminCommandOpcode::kDeleteIoCompletionQueue,
                                    fit::bind_member(this, &FakeAdminCommands::DeleteIoQueue));
  controller.AddDefaultAdminCommand(nvme::AdminCommandOpcode::kDeleteIoSubmissionQueue,
                                    fit::bind_member(this, &FakeAdminCommands::DeleteIoQueue));
  controller.AddDefaultAdminCommand(nvme::AdminCommandOpcode::kSetFeatures,
                                    fit::bind_member(this, &FakeAdminCommands::SetFeatures));
  controller.AddDefaultAdminCommand(nvme::AdminCommandOpcode::kGetFeatures,
                                    fit::bind_member(this, &FakeAdminCommands::GetFeatures));
}

namespace {
void MakeIdentifyController(uint8_t max_data_transfer, nvme::IdentifyController* out) {
  out->set_cqes_max_log2(__builtin_ctzl(sizeof(nvme::Completion)));
  out->set_cqes_min_log2(__builtin_ctzl(sizeof(nvme::Completion)));
  out->set_sqes_max_log2(__builtin_ctzl(sizeof(nvme::Submission)));
  out->set_sqes_min_log2(__builtin_ctzl(sizeof(nvme::Submission)));
  out->num_namespaces = 256;
  out->max_data_transfer = max_data_transfer;
  out->vwc = 1;

  memset(out->serial_number, ' ', sizeof(out->serial_number));
  memset(out->model_number, ' ', sizeof(out->model_number));
  memset(out->firmware_rev, ' ', sizeof(out->firmware_rev));

  memcpy(out->serial_number, FakeAdminCommands::kSerialNumber,
         strlen(FakeAdminCommands::kSerialNumber));
  memcpy(out->model_number, FakeAdminCommands::kModelNumber,
         strlen(FakeAdminCommands::kModelNumber));
  memcpy(out->firmware_rev, FakeAdminCommands::kFirmwareRev,
         strlen(FakeAdminCommands::kFirmwareRev));
}
}  // namespace

void FakeAdminCommands::Identify(nvme::Submission& default_submission,
                                 const nvme::TransactionData& data, Completion& completion) {
  using nvme::IdentifySubmission;
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kSuccess);
  IdentifySubmission& submission = default_submission.GetSubmission<IdentifySubmission>();

  fzl::VmoMapper mapper;
  zx_status_t status = mapper.Map(*data.data_vmo.value());
  ASSERT_OK(status);
  auto buffer = mapper.start();

  uint64_t size;
  data.data_vmo.value()->get_size(&size);
  memset(buffer, 0, size);

  switch (submission.structure()) {
    case IdentifySubmission::IdentifyCns::kIdentifyController: {
      MakeIdentifyController(controller_.max_data_transfer(),
                             static_cast<nvme::IdentifyController*>(buffer));
      break;
    }
    case IdentifySubmission::IdentifyCns::kActiveNamespaceList: {
      uint32_t* list = static_cast<uint32_t*>(buffer);
      size_t i = 0;
      for (auto& ns : controller_.namespaces()) {
        list[i++] = ns.first;
        if (i >= size / sizeof(ns.first)) {
          break;
        }
      }
      break;
    }
    case IdentifySubmission::IdentifyCns::kIdentifyNamespace: {
      uint32_t nsid = submission.namespace_id;
      auto entry = controller_.namespaces().find(nsid);
      if (entry == controller_.namespaces().cend()) {
        completion.set_status_code(GenericStatus::kInvalidNamespaceOrFormat);
        break;
      }

      entry->second.Identify(static_cast<nvme::IdentifyNvmeNamespace*>(buffer));
      break;
    }
    default:
      fdf::error("unsupported identify structure");
      completion.set_status_code(GenericStatus::kInvalidField);
      break;
  }
}

void FakeAdminCommands::ConfigureIoSubmissionQueue(nvme::Submission& submission,
                                                   const nvme::TransactionData& data,
                                                   nvme::Completion& completion) {
  ZX_ASSERT(submission.data_pointer[0] == FAKE_BTI_PHYS_ADDR);
  auto id = submission.GetSubmission<nvme::CreateIoSubmissionQueueSubmission>().queue_id();
  controller_.AddQueuePair(id, nullptr, &controller_.nvme()->io_queue()->submission());
  controller_.registers().SetUpSubmissionDoorbell(id);
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kSuccess);
}
void FakeAdminCommands::ConfigureIoCompletionQueue(nvme::Submission& submission,
                                                   const nvme::TransactionData& data,
                                                   nvme::Completion& completion) {
  ZX_ASSERT(submission.data_pointer[0] == FAKE_BTI_PHYS_ADDR);
  auto& cmd = submission.GetSubmission<nvme::CreateIoCompletionQueueSubmission>();
  controller_.AddQueuePair(cmd.queue_id(), &controller_.nvme()->io_queue()->completion(), nullptr,
                           static_cast<uint16_t>(cmd.interrupt_vector()), cmd.interrupt_en());
  controller_.registers().SetUpCompletionDoorbell(cmd.queue_id());
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kSuccess);
}
void FakeAdminCommands::DeleteIoQueue(nvme::Submission& submission,
                                      const nvme::TransactionData& data,
                                      nvme::Completion& completion) {
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kSuccess);
}

void FakeAdminCommands::SetFeatures(nvme::Submission& submission, const nvme::TransactionData& data,
                                    nvme::Completion& completion) {
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kInvalidOpcode);
  auto feature = submission.GetSubmission<nvme::SetFeaturesSubmission>().feature_id();
  switch (feature) {
    case nvme::Feature::kFeatureNumberOfQueues: {
      auto& requested = submission.GetSubmission<nvme::SetIoQueueCountSubmission>();
      auto& result = completion.GetCompletion<nvme::SetIoQueueCountCompletion>();
      result.set_num_completion_queues(static_cast<uint16_t>(requested.num_completion_queues()));
      result.set_num_submission_queues(static_cast<uint16_t>(requested.num_submission_queues()));
      result.set_status_code(GenericStatus::kSuccess);
      break;
    }
    default:
      break;
  }
}

void FakeAdminCommands::GetFeatures(nvme::Submission& submission, const nvme::TransactionData& data,
                                    nvme::Completion& completion) {
  completion.set_status_code_type(StatusCodeType::kGeneric)
      .set_status_code(GenericStatus::kInvalidOpcode);
  auto feature = submission.GetSubmission<nvme::GetFeaturesSubmission>().feature_id();
  switch (feature) {
    case nvme::Feature::kFeatureVolatileWriteCache: {
      auto& result = completion.GetCompletion<nvme::GetVolatileWriteCacheCompletion>();
      result.set_volatile_write_cache_enabled(true);
      result.set_status_code(GenericStatus::kSuccess);
      break;
    }
    default:
      break;
  }
}

}  // namespace fake_nvme
