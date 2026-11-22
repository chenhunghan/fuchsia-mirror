// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/crash_reports/report_util.h"

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <fidl/fuchsia.mem/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>
#include <zircon/errors.h>

#include <string>
#include <variant>

#include "src/developer/forensics/crash_reports/annotation_map.h"
#include "src/developer/forensics/crash_reports/crash_register.h"
#include "src/developer/forensics/crash_reports/dart_module_parser.h"
#include "src/developer/forensics/crash_reports/program_shortname.h"
#include "src/developer/forensics/crash_reports/report.h"
#include "src/developer/forensics/feedback/annotations/constants.h"
#include "src/lib/fsl/vmo/strings.h"
#include "src/lib/uuid/uuid.h"

namespace forensics {
namespace crash_reports {

namespace {

// The crash server expects certain keys from the client for certain fields.
const char kProgramUptimeMillisKey[] = "ptime";
const char kEventIdKey[] = "comments";
const char kCrashSignatureKey[] = "signature";
const char kDartTypeKey[] = "type";
const char kDartTypeValue[] = "DartError";
const char kDartExceptionMessageKey[] = "error_message";
const char kDartExceptionRuntimeTypeKey[] = "error_runtime_type";
const char kDartExceptionStackTraceKey[] = "DartError";
const char kDartModulesKey[] = "dart_modules";
const char kReportTimeMillis[] = "reportTimeMillis";
const char kIsFatalKey[] = "isFatal";
const char kProcessNameKey[] = "crash.process.name";
const char kThreadNameKey[] = "crash.thread.name";
const char kWeightKey[] = "weight";
const char kBacktraceFilenameKey[] = "backtrace.txt";

// Extra keys that the crash server does *not* have a dependency on.
const char kProcessKoidKey[] = "crash.process.koid";
const char kThreadKoidKey[] = "crash.thread.koid";

std::pair<bool, std::optional<std::string>> ParseDartModules(
    const fuchsia_mem::Buffer& stack_trace) {
  if (!stack_trace.vmo().is_valid()) {
    return {false, std::nullopt};
  }

  std::string text_stack_trace(stack_trace.size(), '\0');

  if (!fsl::StringFromVmo(stack_trace, &text_stack_trace)) {
    FX_LOGS(ERROR) << "Failed to read Dart stack trace vmo";
    return {false, std::nullopt};
  }

  return ParseDartModulesFromStackTrace(text_stack_trace);
}

void ExtractAnnotationsAndAttachments(fuchsia_feedback::CrashReport report,
                                      AnnotationMap* annotations,
                                      std::map<std::string, fuchsia_mem::Buffer>* attachments,
                                      std::optional<fuchsia_mem::Buffer>* minidump) {
  // Default annotations common to all crash reports.
  if (report.annotations().has_value()) {
    annotations->Set(*report.annotations());
  }

  if (report.program_uptime().has_value()) {
    annotations->Set(kProgramUptimeMillisKey, zx::duration(*report.program_uptime()).to_msecs());
  }

  if (report.event_id().has_value()) {
    annotations->Set(kEventIdKey, *report.event_id());
  }

  if (report.crash_signature().has_value()) {
    annotations->Set(kCrashSignatureKey, *report.crash_signature());
  }

  if (report.is_fatal().has_value()) {
    annotations->Set(kIsFatalKey, *report.is_fatal());
  }

  if (report.weight().has_value()) {
    annotations->Set(kWeightKey, *report.weight());
  }

  // Dart-specific annotations.
  if (report.specific_report().has_value() && report.specific_report()->dart().has_value()) {
    annotations->Set(kDartTypeKey, kDartTypeValue);

    const fuchsia_feedback::RuntimeCrashReport& dart_report =
        report.specific_report()->dart().value();
    if (dart_report.exception_type().has_value()) {
      annotations->Set(kDartExceptionRuntimeTypeKey, *dart_report.exception_type());
    } else {
      FX_LOGS(WARNING) << "no Dart exception type to attach to crash report";
    }

    if (dart_report.exception_message().has_value()) {
      annotations->Set(kDartExceptionMessageKey, *dart_report.exception_message());
    } else {
      FX_LOGS(WARNING) << "no Dart exception message to attach to crash report";
    }

    if (dart_report.exception_stack_trace().has_value()) {
      if (const auto [is_unsymbolicated, dart_modules] =
              ParseDartModules(*dart_report.exception_stack_trace());
          dart_modules.has_value()) {
        annotations->Set(kDartModulesKey, *dart_modules);
      } else if (is_unsymbolicated) {
        FX_LOGS(WARNING) << "Failed to parse Dart modules from stack trace";
      }
    }
  }

  // FuchsiaTextBacktrace-specific annotations.
  if (report.specific_report().has_value() &&
      report.specific_report()->text_backtrace().has_value()) {
    const fuchsia_feedback::TextBacktraceCrashReport& text_backtrace_report =
        report.specific_report()->text_backtrace().value();
    if (text_backtrace_report.process_name().has_value()) {
      annotations->Set(kProcessNameKey, *text_backtrace_report.process_name());
    }
    if (text_backtrace_report.process_koid().has_value()) {
      annotations->Set(kProcessKoidKey, *text_backtrace_report.process_koid());
    }
    if (text_backtrace_report.thread_name().has_value()) {
      annotations->Set(kThreadNameKey, *text_backtrace_report.thread_name());
    }
    if (text_backtrace_report.thread_koid().has_value()) {
      annotations->Set(kThreadKoidKey, *text_backtrace_report.thread_koid());
    }
  }

  // Native-specific annotations.
  if (report.specific_report().has_value() && report.specific_report()->native().has_value()) {
    const fuchsia_feedback::NativeCrashReport& native_report =
        report.specific_report()->native().value();
    if (native_report.process_name().has_value()) {
      annotations->Set(kProcessNameKey, *native_report.process_name());
    }
    if (native_report.process_koid().has_value()) {
      annotations->Set(kProcessKoidKey, *native_report.process_koid());
    }
    if (native_report.thread_name().has_value()) {
      annotations->Set(kThreadNameKey, *native_report.thread_name());
    }
    if (native_report.thread_koid().has_value()) {
      annotations->Set(kThreadKoidKey, *native_report.thread_koid());
    }

    // TODO(https://fxbug.dev/42144363): add module annotations from minidump.
  }

  // Default attachments common to all crash reports.
  if (report.attachments().has_value()) {
    for (fuchsia_feedback::Attachment& attachment : *report.attachments()) {
      (*attachments)[attachment.key()] = std::move(attachment.value());
    }
  }

  // Native-specific attachment (minidump).
  if (report.specific_report().has_value() && report.specific_report()->native().has_value()) {
    fuchsia_feedback::NativeCrashReport& native_report = report.specific_report()->native().value();
    if (native_report.minidump().has_value()) {
      *minidump = std::move(*native_report.minidump());
    } else {
      // We don't want to overwrite the client-provided signature.
      if (!report.crash_signature().has_value()) {
        annotations->Set(kCrashSignatureKey, "fuchsia-no-minidump");
      }
    }
  }

  // Dart-specific attachment (text stack trace).
  if (report.specific_report().has_value() && report.specific_report()->dart().has_value()) {
    fuchsia_feedback::RuntimeCrashReport& dart_report = report.specific_report()->dart().value();
    if (dart_report.exception_stack_trace().has_value()) {
      (*attachments)[kDartExceptionStackTraceKey] = std::move(*dart_report.exception_stack_trace());
    } else {
      FX_LOGS(WARNING) << "no Dart exception stack trace to attach to crash report";
      annotations->Set(kCrashSignatureKey, "fuchsia-no-dart-stack-trace");
    }
  }

  // FuchsiaTextBacktrace-specific attachment.
  if (report.specific_report().has_value() &&
      report.specific_report()->text_backtrace().has_value()) {
    fuchsia_feedback::TextBacktraceCrashReport& text_backtrace_report =
        report.specific_report()->text_backtrace().value();
    if (text_backtrace_report.fuchsia_backtrace().has_value()) {
      (*attachments)[kBacktraceFilenameKey] = std::move(*text_backtrace_report.fuchsia_backtrace());
    } else {
      FX_LOGS(WARNING) << "no text backtrace to attach to crash report";
      annotations->Set(kCrashSignatureKey, "fuchsia-no-fuchsia-text-backtrace");
    }
  }
}

void AddCrashServerAnnotations(const std::string& program_name,
                               const std::optional<timekeeper::time_utc>& current_time,
                               AnnotationMap* annotations) {
  // Program.
  // TODO(https://fxbug.dev/42135356): for historical reasons, we used ptype to benefit from
  // Chrome's "Process type" handling in the crash server UI. Remove once the UI can fallback on
  // "Program".
  annotations->Set("ptype", program_name);
  annotations->Set("program", program_name);

  // We set the report time only if we were able to get an accurate one.
  if (current_time.has_value()) {
    annotations->Set(kReportTimeMillis, current_time.value().get() / zx::msec(1).get());
  } else {
    annotations->Set("debug.report-time.set", false);
  }

  // We set the device's global unique identifier only if the device has one.
  if (annotations->Contains(feedback::kDeviceFeedbackIdKey)) {
    annotations->Set("guid", annotations->Get(feedback::kDeviceFeedbackIdKey));
  } else {
    annotations->Set("debug.guid.set", false).Set("debug.device-id.error", Error::kMissingValue);
  }
}
}  // namespace

AnnotationMap GetReportAnnotations(Product product, const AnnotationMap& annotations) {
  AnnotationMap added_annotations;

  // Update the default product with the immediately available annotations (which should
  // contain the version and channel).
  if (product.IsDefaultPlatformProduct()) {
    CrashRegister::AddVersionAndChannel(product, annotations);
  }

  added_annotations.Set("product", product.name)
      .Set("version", product.version)
      .Set("channel", product.channel);

  return added_annotations;
}

AnnotationMap GetReportAnnotations(const feedback::Annotations& snapshot_annotations) {
  // The underlying snapshot may have been garbage collected or its collection timed out
  // (possibly due to shutdown). If so, add the annotations that the snapshot manager could collect
  // itself and annotations indicating why the annotations and archive collected from
  // fuchsia.feedback.DataProvider aren't present.
  //
  // If the underlying snapshot was successfully collected and not all of its data was dropped by
  // the snapshot manager (due to garbage collection), add the annotations collected from
  // fuchsia.feedback.DataProvider and any annotations about why the collected archive may be
  // missing.
  //
  // Snapshots will not be missing due to reasons like not being persisted or not having a valid
  // snapshot uuid because neither can occur without a report entering the store and this flow is
  // triggered before the store is used.
  AnnotationMap added_annotations;

  auto Get = [&snapshot_annotations](const std::string& key) -> ErrorOrString {
    if (snapshot_annotations.contains(key)) {
      return snapshot_annotations.at(key);
    }

    return ErrorOrString(Error::kMissingValue);
  };

  added_annotations.Set(snapshot_annotations)
      .Set(feedback::kOSVersionKey, Get(feedback::kBuildPlatformVersionKey))
      .Set(feedback::kOSChannelKey, Get(feedback::kSystemUpdateChannelCurrentKey));

  return added_annotations;
}

fpromise::result<Report> MakeReport(fuchsia_feedback::CrashReport report,
                                    const ProgramShortname& program_shortname,
                                    const ReportId report_id, const std::string& snapshot_uuid,
                                    const feedback::Annotations& snapshot_annotations,
                                    const std::optional<timekeeper::time_utc>& current_time,
                                    Product product, const bool is_hourly_report) {
  FX_CHECK(report.program_name().has_value());
  const std::string program_name = *report.program_name();

  AnnotationMap annotations = {
      {feedback::kDebugReportUuid, uuid::Generate()},
      {feedback::kOSNameKey, "Fuchsia"},
  };
  std::map<std::string, fuchsia_mem::Buffer> attachments;
  std::optional<fuchsia_mem::Buffer> minidump;

  // Optional annotations and attachments filled by the client.
  ExtractAnnotationsAndAttachments(std::move(report), &annotations, &attachments, &minidump);

  // Snapshot annotations specific to this crash report.
  annotations.Set(GetReportAnnotations(snapshot_annotations));
  annotations.Set(GetReportAnnotations(std::move(product), annotations));

  // Crash server annotations common to all crash reports.
  AddCrashServerAnnotations(program_name, current_time, &annotations);

  return Report::MakeReport(report_id, program_shortname, annotations, std::move(attachments),
                            snapshot_uuid, std::move(minidump), is_hourly_report);
}

}  // namespace crash_reports
}  // namespace forensics
