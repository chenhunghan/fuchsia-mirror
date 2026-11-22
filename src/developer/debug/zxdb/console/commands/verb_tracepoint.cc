// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_tracepoint.h"

#include <limits>
#include <string>
#include <string_view>
#include <vector>

#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/client/tracepoint_spec.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/command_utils.h"
#include "src/developer/debug/zxdb/console/console_context.h"
#include "src/developer/debug/zxdb/console/format_tracepoint.h"
#include "src/developer/debug/zxdb/console/input_location_parser.h"
#include "src/developer/debug/zxdb/console/verbs.h"
#include "src/developer/debug/zxdb/expr/expr.h"
#include "src/lib/fxl/strings/trim.h"

namespace zxdb {

namespace {

constexpr int kConditionSwitch = 1;
constexpr int kExpressionSwitch = 2;
constexpr int kLocalsSwitch = 3;
constexpr int kRegistersSwitch = 4;
constexpr int kBacktraceSwitch = 5;
constexpr int kRawBacktraceSwitch = 6;
constexpr int kMaxHitsSwitch = 7;
constexpr int kMaxFramesSwitch = 8;

const char kTracepointShortHelp[] = "trace / tr: Create a non-stopping tracepoint.";
const char kTracepointUsage[] =
    "trace [ -c <condition> ] [ -e <expr> ... ] [ -l ] [ --registers ] [ -b ] [ -r ] [ "
    "--max-frames <count> ] [ -m <max-hits> ] [ <location> ] [ if <condition> ]";
const char kTracepointHelp[] = R"(
  Alias: "tr"

  Creates a non-stopping tracepoint at the specified location. When reached, the
  target briefly evaluates requested expressions, captures frame variables, registers,
  or stack frames, and immediately resumes without halting the target program.

Location arguments

  Current frame's address (no input)
      frame trace
      trace if x > 0

  Function name
      trace MyFunction
      tr MyNamespace::MyClass::MyMethod

  File/line
      trace main.cc:42

  Address
      trace *0x12345678

  Watch expression
      trace watch my_variable

Options

  -c <expression>
  --condition=<expression>
      Only fires when the expression evaluates to true. You can also append
      "if <expression>" after the location.

  -e <expression>
  --expression=<expression>
      Expression to evaluate upon hit. Can be specified multiple times to
      evaluate multiple expressions.

  -l
  --locals
      Capture local variables in the current frame.

  --registers
      Capture general-purpose registers in the current frame.

  -b
  --backtrace
      Capture symbolic stack frames.

  -r
  --raw-backtrace
      Capture raw stack frames without eliding library internals. Implies backtrace.

  --max-frames=<count>
      Maximum matching target frames to evaluate per hit (default: 10).

  -m <count>
  --max-hits=<count>
      Maximum number of hits before auto-disabling (default: 20).
)";

void OutputCreatedMessage(const Tracepoint* tracepoint,
                          const fxl::RefPtr<CommandContext>& cmd_context) {
  if (ConsoleContext* console_context = cmd_context->GetConsoleContext()) {
    OutputBuffer out("Created ");
    out.Append(FormatTracepoint(console_context, tracepoint));
    cmd_context->Output(out);
  }
}

void CreateAndActivateTracepoint(ConsoleContext* console_context, const ExecutionScope& scope,
                                 const TracepointSpec& spec, uint32_t max_hits,
                                 const fxl::RefPtr<CommandContext>& cmd_context) {
  if (Err err = spec.Validate(); err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  Tracepoint* tp = console_context->session()->system().CreateNewTracepoint();
  tp->set_max_hits(max_hits);
  tp->SetScope(scope);
  console_context->SetActiveTracepoint(tp);
  tp->SetSpec(spec);
  OutputCreatedMessage(tp, cmd_context);
}

Err ParseTracepointSwitches(const Command& cmd, TracepointSpec* spec, uint32_t* max_hits) {
  if (cmd.HasSwitch(kConditionSwitch)) {
    std::string raw_cond = cmd.GetSwitchValue(kConditionSwitch);
    std::string_view trimmed = fxl::TrimString(raw_cond, " ");
    if (trimmed.empty()) {
      return Err(ErrType::kInput, "--condition (-c) requires a non-empty expression.");
    }
    spec->condition = std::string(trimmed);
  }

  if (cmd.HasSwitch(kExpressionSwitch)) {
    for (const std::string& raw_expr : cmd.GetSwitchValues(kExpressionSwitch)) {
      std::string_view trimmed = fxl::TrimString(raw_expr, " ");
      if (trimmed.empty()) {
        return Err(ErrType::kInput, "--expression (-e) requires a non-empty expression.");
      }
      spec->expressions.emplace_back(trimmed);
    }
  }

  spec->locals = cmd.HasSwitch(kLocalsSwitch);
  spec->registers = cmd.HasSwitch(kRegistersSwitch);
  spec->backtrace = cmd.HasSwitch(kBacktraceSwitch);
  spec->raw_backtrace = cmd.HasSwitch(kRawBacktraceSwitch);
  if (spec->raw_backtrace)
    spec->backtrace = true;

  spec->max_matching_frames = 10;
  if (cmd.HasSwitch(kMaxFramesSwitch)) {
    uint32_t max_frames = 0;
    if (Err err = StringToUint32(cmd.GetSwitchValue(kMaxFramesSwitch), &max_frames);
        err.has_error()) {
      return err;
    }
    if (max_frames == 0) {
      return Err(ErrType::kInput, "--max-frames must be greater than 0.");
    }
    spec->max_matching_frames = max_frames;
  }

  *max_hits = Tracepoint::kDefaultMaxHits;
  if (cmd.HasSwitch(kMaxHitsSwitch)) {
    if (Err err = StringToUint32(cmd.GetSwitchValue(kMaxHitsSwitch), max_hits); err.has_error()) {
      return err;
    }
    if (*max_hits == 0) {
      return Err(ErrType::kInput, "--max-hits must be greater than 0.");
    }
  }
  return Err();
}

Err ExtractLocationAndCondition(const Command& cmd, TracepointSpec* spec,
                                std::string* location_out) {
  location_out->clear();
  if (cmd.args().empty())
    return Err();

  std::string_view trimmed = fxl::TrimString(cmd.args()[0], " ");
  std::string_view loc_part = trimmed;
  std::string_view cond_part;
  bool has_if = false;

  if (trimmed == "if" || trimmed.starts_with("if ")) {
    has_if = true;
    loc_part = {};
    cond_part = fxl::TrimString(trimmed.substr(2), " ");
  } else if (size_t if_pos = trimmed.find(" if "); if_pos != std::string_view::npos) {
    has_if = true;
    loc_part = fxl::TrimString(trimmed.substr(0, if_pos), " ");
    cond_part = fxl::TrimString(trimmed.substr(if_pos + 4), " ");
  } else if (trimmed.ends_with(" if")) {
    has_if = true;
    loc_part = fxl::TrimString(trimmed.substr(0, trimmed.size() - 3), " ");
  }

  if (has_if) {
    if (cond_part.empty()) {
      return Err(ErrType::kInput, "Expected an expression after \"if\".");
    }
    if (cmd.HasSwitch(kConditionSwitch)) {
      return Err(ErrType::kInput, "Cannot specify both --condition (-c) and \"if\" condition.");
    }
    spec->condition = std::string(cond_part);
  }

  *location_out = std::string(loc_part);
  return Err();
}

void RunFrameTracepoint(const Command& cmd, const ExecutionScope& scope, TracepointSpec spec,
                        uint32_t max_hits, const fxl::RefPtr<CommandContext>& cmd_context) {
  if (!cmd.frame()) {
    cmd_context->ReportError(
        Err(ErrType::kInput, "There isn't a current frame to take the tracepoint location from."));
    return;
  }

  const Location& frame_loc = cmd.frame()->GetLocation();
  if (frame_loc.has_symbols() && frame_loc.file_line().is_valid()) {
    spec.file_line = frame_loc.file_line();
  } else {
    spec.address = cmd.frame()->GetAddress();
  }

  ConsoleContext* console_context = cmd_context->GetConsoleContext();
  if (!console_context)
    return;

  CreateAndActivateTracepoint(console_context, scope, spec, max_hits, cmd_context);
}

void RunWatchTracepoint(const Command& cmd, const ExecutionScope& scope,
                        const std::string& watch_expr, TracepointSpec spec, uint32_t max_hits,
                        const fxl::RefPtr<CommandContext>& cmd_context) {
  fxl::RefPtr<EvalContext> eval_context = GetEvalContextForCommand(cmd);
  EvalExpression(
      watch_expr, eval_context, true,
      [scope, spec, max_hits, watch_expr, cmd_context](ErrOrValue result) mutable {
        ConsoleContext* console_context = cmd_context->GetConsoleContext();
        if (!console_context)
          return;
        if (result.has_error()) {
          cmd_context->ReportError(result.err());
          return;
        }

        const ExprValue& value = result.value();
        const ExprValueSource& source = value.source();
        if (source.type() != ExprValueSource::Type::kMemory) {
          cmd_context->ReportError(Err("Can't set a watchpoint on a non-memory value."));
          return;
        }
        if (source.is_bitfield()) {
          cmd_context->ReportError(
              Err("This expression's result is a bitfield which can't be watched."));
          return;
        }

        size_t raw_size = value.data().size();
        if (raw_size > std::numeric_limits<uint32_t>::max()) {
          cmd_context->ReportError(Err("Attempting to watch a variable of size " +
                                       std::to_string(raw_size) +
                                       ", which exceeds the maximum supported size."));
          return;
        }
        TracepointWatchSpec watch_spec;
        watch_spec.expression = watch_expr;
        watch_spec.address = source.address();
        watch_spec.byte_size = static_cast<uint32_t>(raw_size);
        watch_spec.access_type = debug_ipc::BreakpointType::kWrite;
        if (Err err = BreakpointSettings::ValidateSize(
                console_context->session()->arch(), watch_spec.access_type, watch_spec.byte_size);
            err.has_error()) {
          cmd_context->ReportError(Err("Attempting to watch a variable of size " +
                                       std::to_string(raw_size) + ".\n\n" + err.msg()));
          return;
        }
        spec.watch = std::move(watch_spec);

        CreateAndActivateTracepoint(console_context, scope, spec, max_hits, cmd_context);
      });
}

void RunLocationTracepoint(const Command& cmd, const ExecutionScope& scope,
                           const std::string& args_str, TracepointSpec spec, uint32_t max_hits,
                           const fxl::RefPtr<CommandContext>& cmd_context) {
  EvalLocalInputLocation(
      GetEvalContextForCommand(cmd), cmd.frame(), args_str,
      [scope, spec, max_hits, cmd_context](ErrOr<std::vector<InputLocation>> locs,
                                           std::optional<uint32_t> /*expr_size*/) mutable {
        if (locs.has_error()) {
          cmd_context->ReportError(locs.err());
          return;
        }

        if (locs.value().empty()) {
          cmd_context->ReportError(Err("No location found for tracepoint."));
          return;
        }

        const InputLocation& first_loc = locs.value()[0];
        if (first_loc.type == InputLocation::Type::kLine) {
          spec.file_line = first_loc.line;
        } else if (first_loc.type == InputLocation::Type::kName) {
          spec.function = first_loc.name.GetFullName();
        } else if (first_loc.type == InputLocation::Type::kAddress) {
          spec.address = first_loc.address;
        } else {
          cmd_context->ReportError(Err("No location found for tracepoint."));
          return;
        }

        ConsoleContext* console_context = cmd_context->GetConsoleContext();
        if (!console_context)
          return;

        CreateAndActivateTracepoint(console_context, scope, spec, max_hits, cmd_context);
      });
}

void RunVerbTracepoint(const Command& cmd, const fxl::RefPtr<CommandContext>& cmd_context) {
  if (Err err = cmd.ValidateNouns({Noun::kProcess, Noun::kThread, Noun::kFrame}); err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  TracepointSpec spec;
  uint32_t max_hits = Tracepoint::kDefaultMaxHits;
  if (Err err = ParseTracepointSwitches(cmd, &spec, &max_hits); err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  std::string args_str;
  if (Err err = ExtractLocationAndCondition(cmd, &spec, &args_str); err.has_error()) {
    cmd_context->ReportError(err);
    return;
  }

  ExecutionScope scope = ExecutionScopeForCommand(cmd);
  if (args_str.empty()) {
    RunFrameTracepoint(cmd, scope, spec, max_hits, cmd_context);
    return;
  }

  if (args_str == "watch" || args_str.starts_with("watch ")) {
    std::string watch_expr;
    if (args_str.size() > 5) {
      watch_expr = std::string(fxl::TrimString(args_str.substr(5), " "));
    }
    if (watch_expr.empty()) {
      cmd_context->ReportError(Err(ErrType::kInput, "Expected an expression after \"watch\"."));
      return;
    }
    RunWatchTracepoint(cmd, scope, watch_expr, spec, max_hits, cmd_context);
    return;
  }

  RunLocationTracepoint(cmd, scope, args_str, spec, max_hits, cmd_context);
}

}  // namespace

VerbRecord GetTraceVerbRecord() {
  SwitchRecord condition_switch(kConditionSwitch, true, "condition", 'c');
  SwitchRecord expr_switch = SwitchRecord::MultiValue(kExpressionSwitch, "expression", 'e');
  SwitchRecord locals_switch(kLocalsSwitch, false, "locals", 'l');
  SwitchRecord registers_switch(kRegistersSwitch, false, "registers");
  SwitchRecord backtrace_switch(kBacktraceSwitch, false, "backtrace", 'b');
  SwitchRecord raw_backtrace_switch(kRawBacktraceSwitch, false, "raw-backtrace", 'r');
  SwitchRecord max_frames_switch(kMaxFramesSwitch, true, "max-frames");
  SwitchRecord max_hits_switch(kMaxHitsSwitch, true, "max-hits", 'm');

  VerbRecord record(&RunVerbTracepoint, &CompleteInputLocation, {"trace", "tr"},
                    kTracepointShortHelp, kTracepointUsage, kTracepointHelp,
                    CommandGroup::kBreakpoint);
  record.param_type = VerbRecord::kOneParam;

  record.switches.push_back(condition_switch);
  record.switches.push_back(expr_switch);
  record.switches.push_back(locals_switch);
  record.switches.push_back(registers_switch);
  record.switches.push_back(backtrace_switch);
  record.switches.push_back(raw_backtrace_switch);
  record.switches.push_back(max_frames_switch);
  record.switches.push_back(max_hits_switch);
  return record;
}

}  // namespace zxdb
