// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/format_tracepoint.h"

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/client/tracepoint_capture.h"
#include "src/developer/debug/zxdb/common/test_with_loop.h"
#include "src/developer/debug/zxdb/console/console_context.h"

namespace zxdb {

namespace {

class FormatTracepointTest : public TestWithLoop {};

TEST_F(FormatTracepointTest, FormatTracepoint) {
  Session session;
  ConsoleContext context(&session);

  Tracepoint* tp = session.system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.expressions = {"x + 1", "y"};
  spec.locals = true;
  spec.registers = true;
  spec.backtrace = true;
  tp->SetSpec(spec);

  std::string formatted = FormatTracepoint(&context, tp).AsString();
  EXPECT_NE(std::string::npos, formatted.find("Tracepoint 1"));
  EXPECT_NE(std::string::npos, formatted.find("state=enabled"));
  EXPECT_NE(std::string::npos, formatted.find("location=MyFunction"));
  EXPECT_NE(std::string::npos, formatted.find("capture=[locals, registers, backtrace]"));
  EXPECT_NE(std::string::npos, formatted.find("exprs=[x + 1, y]"));
  EXPECT_EQ(std::string::npos, formatted.find('\n'));
}

TEST_F(FormatTracepointTest, FormatTracepointList) {
  Session session;
  ConsoleContext context(&session);

  EXPECT_EQ("No tracepoints.\n", FormatTracepointList(&context).AsString());

  Tracepoint* tp = session.system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "TargetFunc";
  tp->SetSpec(spec);

  std::string list_out = FormatTracepointList(&context).AsString();
  EXPECT_NE(std::string::npos, list_out.find('1'));
  EXPECT_NE(std::string::npos, list_out.find("TargetFunc"));
}

TEST_F(FormatTracepointTest, FormatTracepointHit) {
  Session session;
  ConsoleContext context(&session);
  Tracepoint* tp = session.system().CreateNewTracepoint();

  TracepointHitRecord hit;
  hit.tracepoint_id = 1;
  hit.hit_count = 3;
  hit.max_hits = 20;
  hit.thread_koid = 1001;
  hit.location = "MyFunc";
  hit.dwell_ms = 0.42;

  TracepointEvaluation eval;
  eval.result = "42";
  hit.capture.evaluations["x"] = eval;

  TracepointFrameSummary frame;
  frame.name = "MyFunc";
  frame.file = "test.cc";
  frame.line = 10;
  hit.capture.frame = frame;

  TracepointValue val;
  val.value = "100";
  hit.capture.locals = std::map<std::string, TracepointValue>{{"var", val}};

  std::string hit_out = FormatTracepointHit(&context, tp, hit).AsString();
  EXPECT_NE(std::string::npos,
            hit_out.find("Tracepoint 1 hit #3 on thread 1001 at MyFunc (0.42 ms)"));
  EXPECT_NE(std::string::npos, hit_out.find("x = 42"));
  EXPECT_NE(std::string::npos, hit_out.find("Frame: MyFunc at test.cc:10"));
  EXPECT_NE(std::string::npos, hit_out.find("var = 100"));
}

TEST_F(FormatTracepointTest, FormatTracepointHitElidedBacktrace) {
  Session session;
  ConsoleContext context(&session);
  Tracepoint* tp = session.system().CreateNewTracepoint();

  TracepointHitRecord hit;
  hit.tracepoint_id = 1;
  hit.hit_count = 1;
  hit.max_hits = 10;
  hit.thread_koid = 1001;
  hit.location = "MyFunc";
  hit.dwell_ms = 0.1;

  TracepointFrameRecord f0;
  f0.frame_index = 0;
  f0.name = "top";
  f0.ip = 0x1000;

  TracepointFrameRecord f1;
  f1.frame_index = 1;
  f1.name = "internal_1";
  f1.ip = 0x2000;
  f1.elided = true;
  f1.elide_description = "2 frames hidden";

  TracepointFrameRecord f2;
  f2.frame_index = 2;
  f2.name = "internal_2";
  f2.ip = 0x3000;
  f2.elided = true;
  f2.elide_description = "2 frames hidden";

  TracepointFrameRecord f3;
  f3.frame_index = 3;
  f3.name = "bottom";
  f3.ip = 0x4000;

  hit.capture.backtrace = std::vector<TracepointFrameRecord>{f0, f1, f2, f3};

  std::string hit_out = FormatTracepointHit(&context, tp, hit).AsString();
  EXPECT_NE(std::string::npos, hit_out.find("  ... (2 frames hidden)\n"));
  EXPECT_NE(std::string::npos, hit_out.find("#0 top at 0x1000"));
  EXPECT_NE(std::string::npos, hit_out.find("#3 bottom at 0x4000"));
  EXPECT_EQ(std::string::npos, hit_out.find("internal_1"));
}

TEST_F(FormatTracepointTest, FormatTracepointHitTruncated) {
  Session session;
  ConsoleContext context(&session);
  Tracepoint* tp = session.system().CreateNewTracepoint();

  TracepointHitRecord hit;
  hit.tracepoint_id = 1;
  hit.hit_count = 1;
  hit.max_hits = 10;
  hit.thread_koid = 1001;
  hit.location = "MyFunc";
  hit.dwell_ms = 0.1;
  hit.truncated = true;

  std::string hit_out = FormatTracepointHit(&context, tp, hit).AsString();
  EXPECT_NE(std::string::npos, hit_out.find("<warning: matching frames truncated>\n"));
}

TEST_F(FormatTracepointTest, FormatTracepointHitConditionErrorAutoDisabled) {
  Session session;
  ConsoleContext context(&session);
  Tracepoint* tp = session.system().CreateNewTracepoint();

  TracepointHitRecord hit;
  hit.tracepoint_id = 1;
  hit.hit_count = 2;
  hit.max_hits = 2;
  hit.auto_disabled = true;
  hit.thread_koid = 1001;
  hit.location = "MyFunc";
  hit.dwell_ms = 0.1;
  hit.capture.matched = false;
  hit.capture.errors = {"Condition error: unknown_ident"};

  std::string hit_out = FormatTracepointHit(&context, tp, hit).AsString();
  EXPECT_NE(std::string::npos, hit_out.find("<error: Condition error: unknown_ident>\n"));
  EXPECT_NE(std::string::npos, hit_out.find("Tracepoint 1 auto-disabled after reaching 2 hits.\n"));
}

TEST_F(FormatTracepointTest, FormatTracepointDisabledAndAutoDisabledStates) {
  Session session;
  ConsoleContext context(&session);

  Tracepoint* tp = session.system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);

  // Explicit user disable shows "disabled".
  tp->SetEnabled(false);
  EXPECT_NE(std::string::npos, FormatTracepoint(&context, tp).AsString().find("state=disabled"));
  EXPECT_NE(std::string::npos, FormatTracepointList(&context).AsString().find("disabled"));
  EXPECT_EQ(std::string::npos, FormatTracepointList(&context).AsString().find("auto-disabled"));

  // Auto-disable sets enabled=false AND auto_disabled=true; both formatters must show
  // "auto-disabled" rather than "disabled".
  tp->set_auto_disabled(true);
  std::string single = FormatTracepoint(&context, tp).AsString();
  EXPECT_NE(std::string::npos, single.find("state=auto-disabled"));
  EXPECT_EQ(std::string::npos, single.find("state=disabled"));

  std::string list = FormatTracepointList(&context).AsString();
  EXPECT_NE(std::string::npos, list.find("auto-disabled"));

  // Re-enabling clears auto_disabled and restores "enabled".
  tp->SetEnabled(true);
  EXPECT_NE(std::string::npos, FormatTracepoint(&context, tp).AsString().find("state=enabled"));
}

}  // namespace
}  // namespace zxdb
