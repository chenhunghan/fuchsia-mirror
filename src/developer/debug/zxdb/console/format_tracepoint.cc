// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/format_tracepoint.h"

#include <inttypes.h>

#include <string>
#include <vector>

#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/client/tracepoint_capture.h"
#include "src/developer/debug/zxdb/console/console_context.h"
#include "src/developer/debug/zxdb/console/format_table.h"
#include "src/developer/debug/zxdb/format/string_util.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

namespace {

std::string FormatTracepointLocation(const TracepointSpec& spec) {
  if (spec.function.has_value() && !spec.function->empty())
    return *spec.function;
  if (spec.file_line.has_value() && spec.file_line->is_valid()) {
    return fxl::StringPrintf("%s:%d", spec.file_line->file().c_str(), spec.file_line->line());
  }
  if (spec.address.has_value()) {
    return fxl::StringPrintf("*0x%" PRIx64, *spec.address);
  }
  if (spec.watch.has_value()) {
    if (!spec.watch->expression.empty())
      return "watch " + spec.watch->expression;
    if (spec.watch->address.has_value())
      return fxl::StringPrintf("watch *0x%" PRIx64, *spec.watch->address);
  }
  return "(any)";
}

std::string FormatExpressions(const std::vector<std::string>& exprs) {
  return fxl::JoinStrings(exprs, ", ");
}

void FormatCapturedEvaluations(const std::map<std::string, TracepointEvaluation>& evaluations,
                               OutputBuffer* out) {
  for (const auto& [expr, eval] : evaluations) {
    out->Append("  ");
    out->Append(Syntax::kVariable, expr);
    out->Append(" = ");
    if (eval.result.has_value()) {
      out->Append(*eval.result);
    } else if (eval.error.has_value()) {
      out->Append(Syntax::kError, "<error: " + *eval.error + ">");
    } else {
      out->Append(Syntax::kComment, "<unavailable>");
    }
    out->Append("\n");
  }
}

void FormatCapturedFrameAndVariables(const TracepointFrameCapture& capture, OutputBuffer* out) {
  if (capture.frame.has_value()) {
    std::string loc_str =
        capture.frame->file.has_value() && capture.frame->line.has_value()
            ? fxl::StringPrintf("%s:%d", capture.frame->file->c_str(), *capture.frame->line)
            : fxl::StringPrintf("0x%" PRIx64, capture.frame->ip);
    out->Append(
        fxl::StringPrintf("  Frame: %s at %s\n", capture.frame->name.c_str(), loc_str.c_str()));
  }

  if (capture.locals.has_value()) {
    for (const auto& [name, val] : *capture.locals) {
      out->Append("    ");
      out->Append(Syntax::kVariable, name);
      out->Append(" = ");
      out->Append(val.value);
      out->Append("\n");
    }
  }

  if (capture.registers.has_value()) {
    for (const auto& [name, val] : *capture.registers) {
      out->Append("    ");
      out->Append(Syntax::kComment, name);
      out->Append(" = ");
      out->Append(val);
      out->Append("\n");
    }
  }
}

void FormatCapturedBacktrace(const std::vector<TracepointFrameRecord>& backtrace,
                             OutputBuffer* out) {
  std::string current_elide_description;
  bool in_elided = false;
  for (const auto& frame : backtrace) {
    if (frame.elided) {
      if (!in_elided) {
        in_elided = true;
        current_elide_description = frame.elide_description;
      }
      continue;
    }
    if (in_elided) {
      std::string desc = current_elide_description.empty() ? "elided" : current_elide_description;
      out->Append(fxl::StringPrintf("  ... (%s)\n", desc.c_str()));
      in_elided = false;
    }
    std::string loc_str = frame.file.has_value() && frame.line.has_value()
                              ? fxl::StringPrintf("%s:%d", frame.file->c_str(), *frame.line)
                              : fxl::StringPrintf("0x%" PRIx64, frame.ip);
    out->Append(fxl::StringPrintf("    #%zu %s at %s\n", frame.frame_index, frame.name.c_str(),
                                  loc_str.c_str()));
  }
  if (in_elided) {
    std::string desc = current_elide_description.empty() ? "elided" : current_elide_description;
    out->Append(fxl::StringPrintf("  ... (%s)\n", desc.c_str()));
  }
}

}  // namespace

OutputBuffer FormatTracepoint(const ConsoleContext* context, const Tracepoint* tracepoint) {
  OutputBuffer out("Tracepoint ");
  int id = context ? context->IdForTracepoint(tracepoint) : static_cast<int>(tracepoint->id());
  out.Append(Syntax::kSpecial, std::to_string(id));

  out.Append(" state=");
  if (tracepoint->auto_disabled()) {
    out.Append(Syntax::kWarning, "auto-disabled");
  } else if (!tracepoint->enabled()) {
    out.Append(Syntax::kComment, "disabled");
  } else {
    out.Append("enabled");
  }

  out.Append(" hits=");
  out.Append(std::to_string(tracepoint->hit_count()) + "/" +
             std::to_string(tracepoint->max_hits()));

  out.Append(" location=");
  out.Append(Syntax::kHeading, FormatTracepointLocation(tracepoint->spec()));

  const auto& spec = tracepoint->spec();
  if (spec.condition.has_value() && !spec.condition->empty()) {
    out.Append(" condition=");
    out.Append(Syntax::kVariable, *spec.condition);
  }

  std::vector<std::string> flags;
  if (spec.locals)
    flags.push_back("locals");
  if (spec.registers)
    flags.push_back("registers");
  if (spec.backtrace)
    flags.push_back("backtrace");
  if (spec.raw_backtrace)
    flags.push_back("raw-backtrace");

  if (!flags.empty()) {
    out.Append(" capture=[" + fxl::JoinStrings(flags, ", ") + "]");
  }

  if (!spec.expressions.empty()) {
    out.Append(" exprs=[");
    out.Append(FormatExpressions(spec.expressions));
    out.Append("]");
  }

  return out;
}

OutputBuffer FormatTracepointList(ConsoleContext* context, int indent) {
  std::string indent_str(indent, ' ');

  int active_tracepoint_id = context->GetActiveTracepointId();
  auto tracepoints = context->session()->system().GetTracepoints();

  std::vector<std::vector<std::string>> rows;
  for (auto* tracepoint : tracepoints) {
    int id = context->IdForTracepoint(tracepoint);
    std::vector<std::string>& row = rows.emplace_back();

    if (id == active_tracepoint_id) {
      row.push_back(indent_str + GetCurrentRowMarker());
    } else {
      row.emplace_back(indent_str);
    }

    row.push_back(std::to_string(id));

    std::string state_str = "enabled";
    if (tracepoint->auto_disabled()) {
      state_str = "auto-disabled";
    } else if (!tracepoint->enabled()) {
      state_str = "disabled";
    }
    row.push_back(std::move(state_str));

    row.push_back(std::to_string(tracepoint->hit_count()) + "/" +
                  std::to_string(tracepoint->max_hits()));
    row.push_back(FormatTracepointLocation(tracepoint->spec()));
    row.push_back(FormatExpressions(tracepoint->spec().expressions));
  }

  OutputBuffer out;
  if (rows.empty()) {
    out.Append(indent_str + "No tracepoints.\n");
  } else {
    FormatTable({ColSpec(Align::kLeft), ColSpec(Align::kRight, 0, "#", 0, Syntax::kSpecial),
                 ColSpec(Align::kLeft, 0, "State"), ColSpec(Align::kRight, 0, "Hits"),
                 ColSpec(Align::kLeft, 0, "Location"), ColSpec(Align::kLeft, 0, "Expressions")},
                rows, &out);
  }

  return out;
}

OutputBuffer FormatTracepointHit(const ConsoleContext* context, const Tracepoint* tracepoint,
                                 const TracepointHitRecord& hit) {
  OutputBuffer out;
  int id = (context && tracepoint) ? context->IdForTracepoint(tracepoint) : 0;
  if (id == 0) {
    id = static_cast<int>(hit.tracepoint_id);
  }
  out.Append(Syntax::kHeading, fxl::StringPrintf("Tracepoint %d", id));
  out.Append(fxl::StringPrintf(" hit #%u on thread %" PRIu64 " at %s (%.2f ms)\n", hit.hit_count,
                               hit.thread_koid, hit.location.c_str(), hit.dwell_ms));

  for (const auto& error : hit.capture.errors) {
    out.Append("  ");
    out.Append(Syntax::kError, fxl::StringPrintf("<error: %s>\n", error.c_str()));
  }

  if (hit.truncated) {
    out.Append("  ");
    out.Append(Syntax::kWarning, "<warning: matching frames truncated>\n");
  }

  FormatCapturedEvaluations(hit.capture.evaluations, &out);
  FormatCapturedFrameAndVariables(hit.capture, &out);

  if (hit.capture.backtrace.has_value()) {
    FormatCapturedBacktrace(*hit.capture.backtrace, &out);
  }

  if (hit.auto_disabled) {
    out.Append(Syntax::kWarning,
               fxl::StringPrintf("Tracepoint %d auto-disabled after reaching %u hits.\n", id,
                                 hit.max_hits));
  }

  return out;
}

}  // namespace zxdb
