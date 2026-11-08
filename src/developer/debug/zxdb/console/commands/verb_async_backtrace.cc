// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_async_backtrace.h"

#include <utility>

#include "src/developer/debug/zxdb/client/async_task_provider.h"
#include "src/developer/debug/zxdb/client/async_task_tree.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/stack.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/console/command.h"
#include "src/developer/debug/zxdb/console/format_async_task.h"
#include "src/developer/debug/zxdb/console/verbs.h"
#include "src/developer/debug/zxdb/expr/eval_context.h"
#include "src/developer/debug/zxdb/symbols/target_symbols.h"
#include "src/lib/fxl/memory/ref_ptr.h"
#include "src/lib/fxl/memory/weak_ptr.h"

namespace zxdb {

namespace {

// NOTE: This supports async Rust (fuchsia_async) on Fuchsia.
//
// An asynchronous backtrace is fundamentally different from a synchronous
// backtrace:
//
//   * An asynchronous backtrace is a tree of tasks/futures rather than a flat
//     list of frames, as an executor or dispatcher coordinates multiple tasks.
//   * To show a synchronous backtrace, the unwinder walks the stack and uncovers
//     register values of previous frames. To show an asynchronous backtrace, the
//     debugger locates the executor or dispatcher, lists active, queued, and
//     delayed tasks, and displays their hierarchy recursively.
//   * For Rust, non-running futures store state in memory rather than preserving
//     registers/stack.

constexpr int kVerbose = 1;
constexpr int kMoreVerbose = 2;

const char kAsyncBacktraceShortHelp[] = "async-backtrace / abt: Display all async tasks.";
const char kAsyncBacktraceUsage[] = "async-backtrace";
const char kAsyncBacktraceHelp[] = R"(
  Alias: "abt"

  Print a tree of async tasks from the main future or dispatcher in the current
  thread.

  Supports Rust (fuchsia_async).

Arguments

  -v
  --verbose
      Include extra information, such as captured lambda variables.

  --more-verbose
      Include more extra information

Examples

  abt
  abt -v
  process 2 abt
)";

FormatTaskOptions GetFormatTaskOptions(const Command& cmd) {
  FormatTaskOptions options;
  if (cmd.HasSwitch(kMoreVerbose)) {
    options.verbose = true;
    options.variable.verbosity = FormatBufferOptions::Verbosity::kMedium;
    options.variable.wrapping = FormatBufferOptions::Wrapping::kSmart;
    options.variable.pointer_expand_depth = 3;
    options.variable.max_depth = 6;
  } else if (cmd.HasSwitch(kVerbose)) {
    options.verbose = true;
    options.variable.verbosity = FormatBufferOptions::Verbosity::kMedium;
    options.variable.pointer_expand_depth = 1;
    options.variable.max_depth = 3;
  }
  return options;
}

const Frame* FindAsyncTaskProviderFrame(Process* process, Stack& stack) {
  for (size_t i = 0; i < stack.size(); ++i) {
    Frame* frame = stack[i];
    auto providers =
        process->GetAsyncTaskProvidersForLanguage(frame->GetEvalContext()->GetLanguage());
    for (auto* provider : providers) {
      if (provider->CanHandle(frame)) {
        return frame;
      }
    }
  }
  return stack[0];
}

void SyncAndFormatAsyncTaskTree(fxl::WeakPtr<Thread> weak_thread, const FormatTaskOptions& options,
                                fxl::RefPtr<CommandContext> cmd_context) {
  if (!weak_thread) {
    cmd_context->ReportError(Err("There is no thread to show backtrace."));
    return;
  }

  Process* process = weak_thread->GetProcess();
  if (!process || !process->GetTarget()) {
    cmd_context->ReportError(Err("Process gone."));
    return;
  }

  auto& tree = weak_thread->GetAsyncTaskTree();
  if (tree.has_tasks()) {
    if (weak_thread->GetStack().empty()) {
      cmd_context->ReportError(Err("Executor's stack frame is gone."));
      return;
    }
    const Frame* frame = FindAsyncTaskProviderFrame(process, weak_thread->GetStack());
    const TargetSymbols* symbols = process->GetTarget()->GetSymbols();
    cmd_context->Output(FormatAsyncTaskTree(tree, symbols, options, frame->GetEvalContext()));
    return;
  }

  tree.Sync(weak_thread->GetStack(), [weak_thread, cmd_context = std::move(cmd_context), options](
                                         const Err& err, const Frame* frame) mutable {
    if (err.has_error()) {
      cmd_context->ReportError(err);
    } else if (!weak_thread || !weak_thread->GetProcess() ||
               !weak_thread->GetProcess()->GetTarget()) {
      cmd_context->ReportError(Err("Thread gone."));
    } else if (!frame) {
      cmd_context->ReportError(Err("Executor's stack frame is gone."));
    } else {
      const TargetSymbols* symbols = weak_thread->GetProcess()->GetTarget()->GetSymbols();
      cmd_context->Output(FormatAsyncTaskTree(weak_thread->GetAsyncTaskTree(), symbols, options,
                                              frame->GetEvalContext()));
    }
  });
}

void RunVerbAsyncBacktrace(const Command& cmd, fxl::RefPtr<CommandContext> cmd_context) {
  if (Err err = cmd.ValidateNouns({Noun::kProcess, Noun::kThread}); err.has_error())
    return cmd_context->ReportError(err);

  if (!cmd.thread())
    return cmd_context->ReportError(Err("There is no thread to show backtrace."));

  FormatTaskOptions tree_options = GetFormatTaskOptions(cmd);

  if (cmd.thread()->GetStack().has_all_frames()) {
    SyncAndFormatAsyncTaskTree(cmd.thread()->GetWeakPtr(), tree_options, std::move(cmd_context));
  } else {
    cmd.thread()->GetStack().SyncFrames(
        {}, [weak_thread = cmd.thread()->GetWeakPtr(), cmd_context = std::move(cmd_context),
             tree_options](const Err& err) mutable {
          if (err.has_error()) {
            cmd_context->ReportError(err);
          } else {
            SyncAndFormatAsyncTaskTree(weak_thread, tree_options, std::move(cmd_context));
          }
        });
  }
}

}  // namespace

VerbRecord GetAsyncBacktraceVerbRecord() {
  VerbRecord abt(&RunVerbAsyncBacktrace, {"async-backtrace", "abt"}, kAsyncBacktraceShortHelp,
                 kAsyncBacktraceUsage, kAsyncBacktraceHelp, CommandGroup::kQuery);
  abt.switches.emplace_back(kVerbose, false, "verbose", 'v');
  abt.switches.emplace_back(kMoreVerbose, false, "more-verbose", 0);
  return abt;
}

}  // namespace zxdb
