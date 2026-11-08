// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/testing/stubs/data_provider.h"

#include <lib/fpromise/result.h>
#include <lib/syslog/cpp/macros.h>
#include <zircon/errors.h>

#include <map>
#include <string>

#include "src/developer/forensics/utils/vmo.h"
#include "src/lib/fsl/vmo/strings.h"

namespace forensics {
namespace stubs {
namespace {

using fuchsia_feedback::Annotation;
using fuchsia_feedback::Attachment;
using fuchsia_feedback::Snapshot;

std::vector<Annotation> BuildFidlAnnotations(
    const std::map<std::string, std::string>& annotations) {
  std::vector<Annotation> ret_annotations;
  for (const auto& [key, value] : annotations) {
    ret_annotations.push_back(Annotation{{.key = key, .value = value}});
  }
  return ret_annotations;
}

feedback::Annotations BuildFeedbackAnnotations(
    const std::map<std::string, std::string>& annotations) {
  feedback::Annotations ret_annotations;
  for (const auto& [key, value] : annotations) {
    ret_annotations.insert({key, ErrorOrString(value)});
  }
  return ret_annotations;
}

Attachment BuildAttachment(const std::string& key) {
  return Attachment{{
      .key = key,
      .value = BufferFromString("").value(),
  }};
}

}  // namespace

void DataProvider::GetSnapshot(GetSnapshotRequest& request, GetSnapshotCompleter::Sync& completer) {
  Snapshot snapshot;
  snapshot.annotations2(BuildFidlAnnotations(annotations_));
  snapshot.archive(BuildAttachment(snapshot_key_));
  completer.Reply(std::move(snapshot));
}

void DataProvider::GetSnapshotInternal(zx::duration timeout, const std::string& uuid,
                                       GetSnapshotInternalCallback callback) {
  callback(BuildFeedbackAnnotations(annotations_), BuildAttachment(snapshot_key_));
}

void DataProviderReturnsNoAttachment::GetSnapshot(GetSnapshotRequest& request,
                                                  GetSnapshotCompleter::Sync& completer) {
  completer.Reply(std::move(Snapshot().annotations2(BuildFidlAnnotations(annotations_))));
}

void DataProviderReturnsNoAttachment::GetSnapshotInternal(zx::duration timeout,
                                                          const std::string& uuid,
                                                          GetSnapshotInternalCallback callback) {
  callback(BuildFeedbackAnnotations(annotations_), {});
}

void DataProviderReturnsEmptySnapshot::GetSnapshot(GetSnapshotRequest& request,
                                                   GetSnapshotCompleter::Sync& completer) {
  completer.Reply(Snapshot());
}

void DataProviderReturnsEmptySnapshot::GetSnapshotInternal(zx::duration timeout,
                                                           const std::string& uuid,
                                                           GetSnapshotInternalCallback callback) {
  callback({}, {});
}

DataProviderTracksNumCalls::~DataProviderTracksNumCalls() {
  FX_CHECK(expected_num_calls_ == num_calls_) << "Expected " << expected_num_calls_ << " calls\n"
                                              << "Made " << num_calls_ << " calls";
}

void DataProviderTracksNumCalls::GetSnapshot(GetSnapshotRequest& request,
                                             GetSnapshotCompleter::Sync& completer) {
  ++num_calls_;
  completer.Reply(Snapshot());
}

void DataProviderTracksNumCalls::GetSnapshotInternal(zx::duration timeout, const std::string& uuid,
                                                     GetSnapshotInternalCallback callback) {
  ++num_calls_;
  callback({}, {});
}

void DataProviderReturnsOnDemand::GetSnapshot(GetSnapshotRequest& request,
                                              GetSnapshotCompleter::Sync& completer) {
  snapshot_callbacks_.push(completer.ToAsync());
}

void DataProviderReturnsOnDemand::GetSnapshotInternal(const zx::duration timeout,
                                                      const std::string& uuid,
                                                      GetSnapshotInternalCallback callback) {
  snapshot_internal_callbacks_.push(std::move(callback));
  pending_uuids_.push_back(uuid);
}

std::deque<std::string> DataProviderReturnsOnDemand::GetPendingUuids() { return pending_uuids_; }

void DataProviderReturnsOnDemand::PopSnapshotCallback() {
  FX_CHECK(!snapshot_callbacks_.empty());

  Snapshot snapshot;

  snapshot.annotations2(BuildFidlAnnotations(annotations_));
  snapshot.archive(BuildAttachment(snapshot_key_));

  snapshot_callbacks_.front().Reply(std::move(snapshot));
  snapshot_callbacks_.pop();
}

void DataProviderReturnsOnDemand::PopSnapshotInternalCallback() {
  FX_CHECK(!snapshot_internal_callbacks_.empty());

  snapshot_internal_callbacks_.front()(BuildFeedbackAnnotations(annotations_),
                                       BuildAttachment(snapshot_key_));
  snapshot_internal_callbacks_.pop();
  pending_uuids_.pop_front();
}

void DataProviderSnapshotOnly::GetSnapshot(GetSnapshotRequest& request,
                                           GetSnapshotCompleter::Sync& completer) {
  completer.Reply(std::move(Snapshot().archive(std::move(snapshot_))));
}

void DataProviderSnapshotOnly::GetSnapshotInternal(zx::duration timeout, const std::string& uuid,
                                                   GetSnapshotInternalCallback callback) {
  callback({}, std::move(snapshot_));
}

}  // namespace stubs
}  // namespace forensics
