// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_tracepoint.h"

#include <algorithm>
#include <cstdint>
#include <limits>
#include <map>
#include <optional>
#include <set>
#include <string>
#include <vector>

#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/common/checked_math.h"
#include "src/developer/debug/zxdb/common/err_or.h"
#include "src/developer/debug/zxdb/debug_adapter/context.h"
#include "src/developer/debug/zxdb/debug_adapter/data_id.h"

namespace dap {

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointWatchSpec, "", DAP_FIELD(data_id, "dataId"),
                              DAP_FIELD(address, "address"), DAP_FIELD(expression, "expression"),
                              DAP_FIELD(byte_size, "byteSize"),
                              DAP_FIELD(access_type, "accessType"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointSpec, "", DAP_FIELD(id, "id"),
                              DAP_FIELD(function, "function"), DAP_FIELD(file, "file"),
                              DAP_FIELD(line, "line"), DAP_FIELD(address, "address"),
                              DAP_FIELD(watch, "watch"), DAP_FIELD(condition, "condition"),
                              DAP_FIELD(frame, "frame"), DAP_FIELD(backtrace, "backtrace"),
                              DAP_FIELD(raw_backtrace, "rawBacktrace"), DAP_FIELD(locals, "locals"),
                              DAP_FIELD(registers, "registers"),
                              DAP_FIELD(expressions, "expressions"),
                              DAP_FIELD(max_matching_frames, "maxMatchingFrames"),
                              DAP_FIELD(max_hits, "maxHits"), DAP_FIELD(enabled, "enabled"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointInfo, "", DAP_FIELD(id, "id"),
                              DAP_FIELD(verified, "verified"),
                              DAP_FIELD(max_matching_frames, "maxMatchingFrames"),
                              DAP_FIELD(hit_count, "hitCount"), DAP_FIELD(max_hits, "maxHits"),
                              DAP_FIELD(enabled, "enabled"),
                              DAP_FIELD(auto_disabled, "autoDisabled"),
                              DAP_FIELD(message, "message"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbSetTracepointsResponse, "", DAP_FIELD(tracepoints, "tracepoints"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbSetTracepointsRequest, "zxdb.SetTracepoints",
                              DAP_FIELD(tracepoints, "tracepoints"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameSummary, "", DAP_FIELD(name, "name"),
                              DAP_FIELD(file, "file"), DAP_FIELD(line, "line"), DAP_FIELD(ip, "ip"),
                              DAP_FIELD(sp, "sp"), DAP_FIELD(trust, "trust"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameRecord, "", DAP_FIELD(frame_index, "frameIndex"),
                              DAP_FIELD(name, "name"), DAP_FIELD(file, "file"),
                              DAP_FIELD(line, "line"), DAP_FIELD(ip, "ip"), DAP_FIELD(sp, "sp"),
                              DAP_FIELD(trust, "trust"), DAP_FIELD(elided, "elided"),
                              DAP_FIELD(elide_description, "elideDescription"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointFrameCapture, "", DAP_FIELD(matched, "matched"),
                              DAP_FIELD(frame, "frame"), DAP_FIELD(backtrace, "backtrace"),
                              DAP_FIELD(locals, "locals"), DAP_FIELD(registers, "registers"),
                              DAP_FIELD(evaluations, "evaluations"), DAP_FIELD(errors, "errors"))

DAP_IMPLEMENT_STRUCT_TYPEINFO(ZxdbTracepointEvent, "zxdb.Tracepoint",
                              DAP_FIELD(tracepoint_id, "tracepointId"),
                              DAP_FIELD(location, "location"), DAP_FIELD(hit_count, "hitCount"),
                              DAP_FIELD(max_hits, "maxHits"),
                              DAP_FIELD(auto_disabled, "autoDisabled"),
                              DAP_FIELD(truncated, "truncated"),
                              DAP_FIELD(thread_koid, "threadKoid"), DAP_FIELD(dwell_ms, "dwellMs"),
                              DAP_FIELD(capture, "capture"))

}  // namespace dap

namespace zxdb {

namespace {

struct AssignedClientIds {
  std::vector<int64_t> assigned_ids;
  std::set<int64_t> incoming_ids;
};

struct BuiltTracepoint {
  TracepointSpec spec;
  ExecutionScope scope;
};

Err ValidateSpecIntegerBounds(const dap::ZxdbTracepointSpec& dap_spec) {
  constexpr int64_t kMaxInt = std::numeric_limits<int>::max();
  constexpr int64_t kMaxUint32 = std::numeric_limits<uint32_t>::max();

  if (dap_spec.line.has_value() && (*dap_spec.line <= 0 || *dap_spec.line > kMaxInt)) {
    return Err("Line number must be between 1 and INT_MAX.");
  }
  if (dap_spec.max_matching_frames.has_value() &&
      (*dap_spec.max_matching_frames <= 0 || *dap_spec.max_matching_frames > kMaxUint32)) {
    return Err("Tracepoint maxMatchingFrames must be a positive 32-bit integer.");
  }
  if (dap_spec.max_hits.has_value() &&
      (*dap_spec.max_hits <= 0 || *dap_spec.max_hits > kMaxUint32)) {
    return Err("maxHits must be a positive 32-bit integer.");
  }
  if (dap_spec.watch.has_value() && dap_spec.watch->byte_size.has_value() &&
      (*dap_spec.watch->byte_size <= 0 || *dap_spec.watch->byte_size > kMaxUint32)) {
    return Err("watch byteSize must be a positive 32-bit integer.");
  }
  return Err();
}

ErrOr<AssignedClientIds> AssignAndValidateClientIds(
    const dap::array<dap::ZxdbTracepointSpec>& specs,
    const std::map<int64_t, fxl::WeakPtr<Tracepoint>>& existing) {
  constexpr int64_t kMaxId = std::numeric_limits<int64_t>::max();
  int64_t next_id = 1;

  AssignedClientIds result;
  result.assigned_ids.reserve(specs.size());

  for (const auto& dap_spec : specs) {
    if (dap_spec.id.has_value()) {
      if (*dap_spec.id <= 0) {
        return Err("Tracepoint id must be positive.");
      }
      if (*dap_spec.id == kMaxId) {
        return Err("Tracepoint id exceeds maximum supported value.");
      }
      if (!result.incoming_ids.insert(*dap_spec.id).second) {
        return Err("Duplicate tracepoint id in zxdb.SetTracepoints request.");
      }
      next_id = std::max(next_id, *dap_spec.id + 1);
    }
    if (Err err = ValidateSpecIntegerBounds(dap_spec); err.has_error()) {
      return err;
    }
  }

  for (const auto& dap_spec : specs) {
    if (dap_spec.id.has_value()) {
      result.assigned_ids.push_back(*dap_spec.id);
      continue;
    }
    while (next_id < kMaxId &&
           (existing.contains(next_id) || result.incoming_ids.contains(next_id))) {
      next_id++;
    }
    if (next_id == kMaxId) {
      return Err("Exhausted available tracepoint client IDs.");
    }
    result.assigned_ids.push_back(next_id);
    result.incoming_ids.insert(next_id);
    next_id++;
  }

  return result;
}

void PruneRemovedTracepoints(Session* session, const std::set<int64_t>& incoming_ids,
                             std::map<int64_t, fxl::WeakPtr<Tracepoint>>& existing) {
  std::vector<Tracepoint*> to_delete;
  for (const auto& [cid, tp] : existing) {
    if (!incoming_ids.contains(cid) && tp) {
      to_delete.push_back(tp.get());
    }
  }
  for (Tracepoint* tp : to_delete) {
    if (session) {
      session->system().DeleteTracepoint(tp);
    }
  }
  for (auto it = existing.begin(); it != existing.end();) {
    if (!incoming_ids.contains(it->first)) {
      it = existing.erase(it);
    } else {
      ++it;
    }
  }
}

ErrOr<TracepointWatchSpec> BuildWatchSpec(DebugAdapterContext* ctx,
                                          const dap::ZxdbTracepointWatchSpec& dap_watch,
                                          ExecutionScope& out_scope) {
  out_scope = ExecutionScope();
  if (!dap_watch.data_id.has_value() && !dap_watch.address.has_value()) {
    return Err("Watch tracepoint requires either dataId or address.");
  }

  TracepointWatchSpec watch;
  if (dap_watch.expression.has_value()) {
    watch.expression = *dap_watch.expression;
  }

  std::optional<DataId> parsed_data_id;
  if (dap_watch.data_id.has_value()) {
    ErrOr<DataId> parse_res = DataId::Parse(*dap_watch.data_id);
    if (parse_res.has_error()) {
      return parse_res.err();
    }
    parsed_data_id = parse_res.value();
    watch.address = parsed_data_id->address();
    watch.byte_size = parsed_data_id->byte_size();
  }
  if (dap_watch.address.has_value()) {
    if (*dap_watch.address < 0) {
      return Err("Invalid negative watch address.");
    }
    uint64_t explicit_addr = static_cast<uint64_t>(*dap_watch.address);
    if (parsed_data_id.has_value() && explicit_addr != parsed_data_id->address()) {
      return Err("Conflicting dataId and address in watch tracepoint.");
    }
    watch.address = explicit_addr;
  }
  if (dap_watch.byte_size.has_value()) {
    watch.byte_size = static_cast<uint32_t>(*dap_watch.byte_size);
  }
  if (dap_watch.access_type.has_value()) {
    ErrOr<BreakpointSettings::Type> access_res = DataId::ParseAccessType(*dap_watch.access_type);
    if (access_res.has_error()) {
      return access_res.err();
    }
    watch.access_type = access_res.value();
  }

  // Make sure we'll never overflow, even though |BreakpointSettings::ValidateSize| below will
  // validate that the byte_size is a limited set of small values.
  if (watch.address &&
      !CheckedAdd(*watch.address, static_cast<uint64_t>(watch.byte_size)).has_value()) {
    return Err("Watch address range overflows 64-bit address space.");
  }
  if (Err err = BreakpointSettings::ValidateSize(ctx->session()->arch(), watch.access_type,
                                                 watch.byte_size);
      err.has_error()) {
    return err;
  }

  if (parsed_data_id.has_value()) {
    Process* process = ctx->session()->system().ProcessFromKoid(parsed_data_id->process_koid());
    if (!process || !process->GetTarget()) {
      return Err("No active process to set a watch tracepoint on.");
    }
    out_scope = ExecutionScope(process->GetTarget());
  }
  return watch;
}

ErrOr<BuiltTracepoint> BuildTracepointSpec(DebugAdapterContext* ctx,
                                           const dap::ZxdbTracepointSpec& dap_spec) {
  if (dap_spec.file.has_value() != dap_spec.line.has_value()) {
    return Err("Both file and line must be specified together for a line tracepoint.");
  }

  BuiltTracepoint built;
  TracepointSpec& spec = built.spec;
  if (dap_spec.function.has_value()) {
    spec.function = *dap_spec.function;
  }
  if (dap_spec.address.has_value()) {
    if (*dap_spec.address < 0) {
      return Err("Invalid negative tracepoint address.");
    }
    spec.address = static_cast<uint64_t>(*dap_spec.address);
  }
  if (dap_spec.file.has_value() && dap_spec.line.has_value()) {
    spec.file_line = FileLine(*dap_spec.file, static_cast<int>(*dap_spec.line));
  }
  if (dap_spec.watch.has_value()) {
    auto watch_res = BuildWatchSpec(ctx, *dap_spec.watch, built.scope);
    if (watch_res.has_error()) {
      return watch_res.err();
    }
    spec.watch = watch_res.value();
  }
  if (dap_spec.condition.has_value()) {
    spec.condition = *dap_spec.condition;
  }
  if (dap_spec.frame.has_value()) {
    spec.frame = *dap_spec.frame;
  }
  if (dap_spec.backtrace.has_value()) {
    spec.backtrace = *dap_spec.backtrace;
  }
  if (dap_spec.raw_backtrace.has_value()) {
    spec.raw_backtrace = *dap_spec.raw_backtrace;
    if (spec.raw_backtrace) {
      spec.backtrace = true;
    }
  }
  if (dap_spec.locals.has_value()) {
    spec.locals = *dap_spec.locals;
  }
  if (dap_spec.registers.has_value()) {
    spec.registers = *dap_spec.registers;
  }
  if (dap_spec.expressions.has_value()) {
    spec.expressions = *dap_spec.expressions;
  }
  if (dap_spec.max_matching_frames.has_value()) {
    spec.max_matching_frames = static_cast<uint32_t>(*dap_spec.max_matching_frames);
  }
  return built;
}

dap::ZxdbTracepointInfo CreateUnverifiedTracepointInfo(int64_t client_id,
                                                       const dap::ZxdbTracepointSpec& dap_spec,
                                                       const std::string& error_message) {
  dap::ZxdbTracepointInfo info;
  info.id = client_id;
  info.verified = false;
  info.message = error_message;
  if (dap_spec.max_matching_frames.has_value()) {
    info.max_matching_frames = *dap_spec.max_matching_frames;
  }
  if (dap_spec.enabled.has_value()) {
    info.enabled = *dap_spec.enabled;
  }
  if (dap_spec.max_hits.has_value()) {
    info.max_hits = *dap_spec.max_hits;
  }
  return info;
}

dap::ZxdbTracepointInfo UpsertTracepoint(DebugAdapterContext* ctx, int64_t client_id,
                                         const dap::ZxdbTracepointSpec& dap_spec,
                                         const BuiltTracepoint& built) {
  auto& existing = ctx->dap_tracepoints();
  auto existing_it = existing.find(client_id);
  bool is_new = (existing_it == existing.end() || !existing_it->second);
  Tracepoint* tp = nullptr;
  if (!is_new) {
    tp = existing_it->second.get();
  } else {
    tp = ctx->session()->system().CreateNewTracepoint();
    existing[client_id] = tp->GetWeakPtr();
  }

  const uint32_t target_max_hits = dap_spec.max_hits.has_value()
                                       ? static_cast<uint32_t>(*dap_spec.max_hits)
                                       : Tracepoint::kDefaultMaxHits;
  tp->set_max_hits(target_max_hits);

  if (dap_spec.enabled.has_value()) {
    if (*dap_spec.enabled) {
      if (!tp->enabled()) {
        tp->SetEnabled(true);
      }
    } else {
      tp->SetEnabled(false);
      tp->set_auto_disabled(false);
    }
  } else if (!tp->auto_disabled() && !tp->enabled()) {
    tp->SetEnabled(true);
  }

  tp->SetScope(built.scope);
  if (is_new || !(tp->spec() == built.spec)) {
    tp->SetSpec(built.spec);
  }

  dap::ZxdbTracepointInfo info;
  info.id = client_id;
  info.verified = tp->verified();
  if (tp->spec().max_matching_frames.has_value()) {
    info.max_matching_frames = static_cast<int64_t>(*tp->spec().max_matching_frames);
  }
  info.hit_count = tp->hit_count();
  info.max_hits = tp->max_hits();
  info.enabled = tp->enabled();
  info.auto_disabled = tp->auto_disabled();
  return info;
}

}  // namespace

dap::ResponseOrError<dap::ZxdbSetTracepointsResponse> OnRequestZxdbSetTracepoints(
    DebugAdapterContext* ctx, const dap::ZxdbSetTracepointsRequest& req) {
  if (!ctx || !ctx->session()) {
    return dap::Error("No active debug session.");
  }

  auto& existing = ctx->dap_tracepoints();
  auto assign_res = AssignAndValidateClientIds(req.tracepoints, existing);
  if (assign_res.has_error()) {
    return dap::Error(assign_res.err().msg());
  }

  const auto& assigned_ids = assign_res.value().assigned_ids;
  const auto& incoming_ids = assign_res.value().incoming_ids;

  PruneRemovedTracepoints(ctx->session(), incoming_ids, existing);

  dap::ZxdbSetTracepointsResponse response;
  response.tracepoints.reserve(req.tracepoints.size());

  for (size_t i = 0; i < req.tracepoints.size(); ++i) {
    const auto& dap_spec = req.tracepoints[i];
    int64_t client_id = assigned_ids[i];

    auto cleanup_existing = [&]() {
      auto existing_it = existing.find(client_id);
      if (existing_it != existing.end()) {
        if (existing_it->second) {
          ctx->session()->system().DeleteTracepoint(existing_it->second.get());
        }
        existing.erase(client_id);
      }
    };

    auto built_or_err = BuildTracepointSpec(ctx, dap_spec);
    if (built_or_err.has_error()) {
      cleanup_existing();
      response.tracepoints.push_back(
          CreateUnverifiedTracepointInfo(client_id, dap_spec, built_or_err.err().msg()));
      continue;
    }

    const BuiltTracepoint& built = built_or_err.value();
    Err valid_err = built.spec.Validate();
    if (valid_err.has_error()) {
      cleanup_existing();
      response.tracepoints.push_back(
          CreateUnverifiedTracepointInfo(client_id, dap_spec, valid_err.msg()));
      continue;
    }

    response.tracepoints.push_back(UpsertTracepoint(ctx, client_id, dap_spec, built));
  }

  return response;
}

}  // namespace zxdb
