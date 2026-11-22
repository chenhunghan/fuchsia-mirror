// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/commands/verb_tracepoint.h"

#include <memory>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/console/commands/breakpoint_command_test.h"
#include "src/developer/debug/zxdb/console/console_test.h"

namespace zxdb {

namespace {

class VerbTracepoint : public BreakpointCommandTest {};

TEST_F(VerbTracepoint, Trace) {
  console().ProcessInputLine("trace *0x1230 + 4");

  const auto& trace_req = breakpoint_remote_api()->last_request;
  ASSERT_TRUE(trace_req.has_value());
  EXPECT_EQ(debug_ipc::Stop::kNone, trace_req->breakpoint.stop);
  ASSERT_EQ(1u, trace_req->breakpoint.locations.size());
  EXPECT_EQ(0x1234u, trace_req->breakpoint.locations[0].address);

  auto event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ("Created Tracepoint 1 state=enabled hits=0/20 location=*0x1234",
            event.output.AsString());

  ASSERT_TRUE(breakpoint_remote_api()->last_cb);
  breakpoint_remote_api()->last_cb(Err(), debug_ipc::AddOrChangeBreakpointReply());
  EXPECT_FALSE(console().HasOutputEvent());

  console().ProcessInputLine("tr MyFunction");
  event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ("Created Tracepoint 2 state=enabled hits=0/20 location=MyFunction",
            event.output.AsString());

  auto tracepoints = session().system().GetTracepoints();
  ASSERT_EQ(2u, tracepoints.size());
  EXPECT_EQ(Tracepoint::kDefaultMaxHits, tracepoints[1]->max_hits());
  EXPECT_EQ(10u, tracepoints[1]->spec().max_matching_frames);
  EXPECT_FALSE(tracepoints[1]->spec().locals);
  EXPECT_FALSE(tracepoints[1]->spec().registers);
  EXPECT_FALSE(tracepoints[1]->spec().backtrace);
  EXPECT_FALSE(tracepoints[1]->spec().raw_backtrace);
  EXPECT_EQ(ExecutionScope::kSystem, tracepoints[1]->scope().type());

  // Backend installation failures surface as internal breakpoint errors.
  console().ProcessInputLine("trace *0x2345");
  event = console().GetOutputEvent();
  EXPECT_EQ("Created Tracepoint 3 state=enabled hits=0/20 location=*0x2345",
            event.output.AsString());

  ASSERT_TRUE(breakpoint_remote_api()->last_cb);
  debug_ipc::AddOrChangeBreakpointReply reply;
  reply.status = debug::Status("Bad handle");
  breakpoint_remote_api()->last_cb(Err(), reply);

  event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ("Error updating internal breakpoint:\nBad handle", event.output.AsString());
}

TEST_F(VerbTracepoint, Switches) {
  console().ProcessInputLine("trace -e x -e y -l --registers -b --max-frames 5 -m 50 MyFunction");

  auto event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ(
      "Created Tracepoint 1 state=enabled hits=0/50 location=MyFunction "
      "capture=[locals, registers, backtrace] exprs=[x, y]",
      event.output.AsString());

  console().ProcessInputLine("trace -c \" x > 0 \" -r main.cc:42");
  event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ(
      "Created Tracepoint 2 state=enabled hits=0/20 location=main.cc:42 condition=x > 0 "
      "capture=[backtrace, raw-backtrace]",
      event.output.AsString());

  auto tracepoints = session().system().GetTracepoints();
  ASSERT_EQ(2u, tracepoints.size());
  EXPECT_EQ(50u, tracepoints[0]->max_hits());
  EXPECT_EQ(5u, tracepoints[0]->spec().max_matching_frames);
  EXPECT_TRUE(tracepoints[0]->spec().locals);
  EXPECT_TRUE(tracepoints[0]->spec().registers);
  EXPECT_TRUE(tracepoints[0]->spec().backtrace);
  EXPECT_FALSE(tracepoints[0]->spec().raw_backtrace);
  ASSERT_EQ(2u, tracepoints[0]->spec().expressions.size());
  EXPECT_EQ("x", tracepoints[0]->spec().expressions[0]);
  EXPECT_EQ("y", tracepoints[0]->spec().expressions[1]);

  EXPECT_EQ(FileLine("main.cc", 42), tracepoints[1]->spec().file_line);
  EXPECT_EQ("x > 0", tracepoints[1]->spec().condition);
  EXPECT_TRUE(tracepoints[1]->spec().backtrace);
  EXPECT_TRUE(tracepoints[1]->spec().raw_backtrace);

  console().ProcessInputLine("trace -m 0 main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--max-hits must be greater than 0.", event.output.AsString());

  console().ProcessInputLine("trace --max-frames 0 main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--max-frames must be greater than 0.", event.output.AsString());

  console().ProcessInputLine("trace -c \"\" main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--condition (-c) requires a non-empty expression.", event.output.AsString());

  console().ProcessInputLine("trace -c \"   \" main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--condition (-c) requires a non-empty expression.", event.output.AsString());

  console().ProcessInputLine("trace -e \"\" main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--expression (-e) requires a non-empty expression.", event.output.AsString());

  console().ProcessInputLine("trace -e \"   \" main");
  event = console().GetOutputEvent();
  EXPECT_EQ("--expression (-e) requires a non-empty expression.", event.output.AsString());

  EXPECT_EQ(2u, session().system().GetTracepoints().size());
}

TEST_F(VerbTracepoint, Watch) {
  console().ProcessInputLine("trace watch");
  auto event = console().GetOutputEvent();
  EXPECT_EQ("Expected an expression after \"watch\".", event.output.AsString());
  EXPECT_TRUE(session().system().GetTracepoints().empty());

  console().ProcessInputLine("trace watch 0x12345");
  event = console().GetOutputEvent();
  EXPECT_EQ("Can't set a watchpoint on a non-memory value.", event.output.AsString());
  EXPECT_TRUE(session().system().GetTracepoints().empty());

  console().ProcessInputLine("trace watch nonexistent_var");
  event = console().GetOutputEvent();
  EXPECT_EQ("No variable 'nonexistent_var' found.", event.output.AsString());
  EXPECT_TRUE(session().system().GetTracepoints().empty());

  constexpr uint64_t kAddress = 0x1234;
  breakpoint_remote_api()->AddMemory(kAddress, {0, 0});

  console().ProcessInputLine("trace watch *(uint16_t*)0x1234");
  event = console().GetOutputEvent();
  ASSERT_EQ(MockConsole::OutputEvent::Type::kOutput, event.type);
  EXPECT_EQ("Created Tracepoint 1 state=enabled hits=0/20 location=watch *(uint16_t*)0x1234",
            event.output.AsString());

  ASSERT_EQ(1u, session().system().GetTracepoints().size());
  const auto& watch_req = breakpoint_remote_api()->last_request;
  ASSERT_TRUE(watch_req.has_value());
  EXPECT_EQ(debug_ipc::Stop::kNone, watch_req->breakpoint.stop);
  EXPECT_EQ(debug_ipc::BreakpointType::kWrite, watch_req->breakpoint.type);
  ASSERT_EQ(1u, watch_req->breakpoint.locations.size());
  EXPECT_EQ(0u, watch_req->breakpoint.locations[0].address);
  EXPECT_EQ(kAddress, watch_req->breakpoint.locations[0].address_range.begin());
  EXPECT_EQ(2u, watch_req->breakpoint.locations[0].address_range.size());
}

TEST_F(VerbTracepoint, ConditionalAndFrame) {
  console().ProcessInputLine("frame trace");
  auto event = console().GetOutputEvent();
  EXPECT_EQ("There isn't a current frame to take the tracepoint location from.",
            event.output.AsString());
  EXPECT_TRUE(session().system().GetTracepoints().empty());

  console().ProcessInputLine("trace MyFunction if i >= 2");
  event = console().GetOutputEvent();
  EXPECT_EQ("Created Tracepoint 1 state=enabled hits=0/20 location=MyFunction condition=i >= 2",
            event.output.AsString());

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), 0x1000, 0x2000, "Func",
                                               FileLine("bar.cc", 25)));
  InjectExceptionWithStack(ConsoleTest::kProcessKoid, ConsoleTest::kThreadKoid,
                           debug_ipc::ExceptionType::kSingleStep, std::move(frames), true);
  console().FlushOutputEvents();

  console().ProcessInputLine("trace if x > 0");
  event = console().GetOutputEvent();
  EXPECT_EQ("Created Tracepoint 2 state=enabled hits=0/20 location=bar.cc:25 condition=x > 0",
            event.output.AsString());

  console().ProcessInputLine("trace -c \"a > 0\" MyFunc if b > 0");
  event = console().GetOutputEvent();
  EXPECT_EQ("Cannot specify both --condition (-c) and \"if\" condition.", event.output.AsString());

  console().ProcessInputLine("trace MyFunc if");
  event = console().GetOutputEvent();
  EXPECT_EQ("Expected an expression after \"if\".", event.output.AsString());

  EXPECT_EQ(2u, session().system().GetTracepoints().size());
}

TEST_F(VerbTracepoint, Scope) {
  console().ProcessInputLine("process 1 trace *0x1234");
  auto event = console().GetOutputEvent();
  EXPECT_EQ("Created Tracepoint 1 state=enabled hits=0/20 location=*0x1234",
            event.output.AsString());

  const auto& proc_req = breakpoint_remote_api()->last_request;
  ASSERT_TRUE(proc_req.has_value());
  ASSERT_EQ(1u, proc_req->breakpoint.locations.size());
  EXPECT_EQ(ConsoleTest::kProcessKoid, proc_req->breakpoint.locations[0].id.process);
  EXPECT_EQ(0u, proc_req->breakpoint.locations[0].id.thread);

  console().ProcessInputLine("process 1 thread 1 trace *0x5678");
  event = console().GetOutputEvent();
  EXPECT_EQ("Created Tracepoint 2 state=enabled hits=0/20 location=*0x5678",
            event.output.AsString());

  const auto& thread_req = breakpoint_remote_api()->last_request;
  ASSERT_TRUE(thread_req.has_value());
  ASSERT_EQ(1u, thread_req->breakpoint.locations.size());
  EXPECT_EQ(ConsoleTest::kProcessKoid, thread_req->breakpoint.locations[0].id.process);
  EXPECT_EQ(ConsoleTest::kThreadKoid, thread_req->breakpoint.locations[0].id.thread);

  auto tracepoints = session().system().GetTracepoints();
  ASSERT_EQ(2u, tracepoints.size());
  EXPECT_EQ(ExecutionScope::kTarget, tracepoints[0]->scope().type());
  EXPECT_EQ(process()->GetTarget(), tracepoints[0]->scope().target());
  EXPECT_EQ(ExecutionScope::kThread, tracepoints[1]->scope().type());
  EXPECT_EQ(thread(), tracepoints[1]->scope().thread());
}

}  // namespace
}  // namespace zxdb
