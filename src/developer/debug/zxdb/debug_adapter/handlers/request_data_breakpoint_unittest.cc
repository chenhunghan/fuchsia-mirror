// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_data_breakpoint.h"

#include <inttypes.h>

#include <gtest/gtest.h>
#include <llvm/BinaryFormat/Dwarf.h>

#include "src/developer/debug/zxdb/client/breakpoint.h"
#include "src/developer/debug/zxdb/client/breakpoint_settings.h"
#include "src/developer/debug/zxdb/client/execution_scope.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/target_impl.h"
#include "src/developer/debug/zxdb/console/console.h"
#include "src/developer/debug/zxdb/console/console_context.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"
#include "src/developer/debug/zxdb/expr/format_node.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/index_test_support.h"
#include "src/developer/debug/zxdb/symbols/mock_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/mock_symbol_data_provider.h"
#include "src/developer/debug/zxdb/symbols/modified_type.h"
#include "src/developer/debug/zxdb/symbols/type_test_support.h"
#include "src/developer/debug/zxdb/symbols/variable_test_support.h"
#include "src/lib/fxl/strings/string_printf.h"

namespace zxdb {

namespace {

class RequestDataBreakpointTest : public DebugAdapterContextTest {
 public:
  void SetUp() override {
    DebugAdapterContextTest::SetUp();
    InitializeDebugging();
  }

  dap::ResponseOrError<dap::SetDataBreakpointsResponse> SetDataBreakpoints(
      std::vector<dap::DataBreakpoint> breakpoints) {
    dap::SetDataBreakpointsRequest req = {};
    req.breakpoints = std::move(breakpoints);
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();
    return response.get();
  }

  dap::ResponseOrError<dap::DataBreakpointInfoResponse> GetDataBreakpointInfo(
      const dap::DataBreakpointInfoRequest& req) {
    auto response = client().send(req);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();
    return response.get();
  }
  int64_t InjectStoppedFrame(MockSymbolDataProvider** out_data_provider = nullptr,
                             uint64_t process_koid = kProcessKoid,
                             uint64_t thread_koid = kThreadKoid) {
    Thread* thread = InjectThread(process_koid, thread_koid);
    RunPendingClientCalls();

    Location location(Location::State::kSymbolized, 0x10010);
    auto mock_frame = std::make_unique<MockFrame>(&session(), thread, location, 0x7890);
    if (out_data_provider) {
      *out_data_provider = mock_frame->GetMockSymbolDataProvider();
    }
    std::vector<std::unique_ptr<Frame>> frames;
    frames.push_back(std::move(mock_frame));
    InjectExceptionWithStack(process_koid, thread_koid, debug_ipc::ExceptionType::kSingleStep,
                             std::move(frames), true);
    RunPendingClientCalls();
    return context().IdForFrame(thread_koid, 0);
  }

  static std::string MakeDataId(uint64_t address, uint32_t byte_size,
                                uint64_t process_koid = kProcessKoid) {
    return fxl::StringPrintf("%" PRIu64 ":0x%" PRIx64 ":%u", process_koid, address, byte_size);
  }
};

TEST_F(RequestDataBreakpointTest, DataBreakpointInfoRoundTrip) {
  constexpr uint64_t kAddress = 0x1234;

  InjectProcess(kProcessKoid);
  MockSymbolDataProvider* data_provider = nullptr;
  int64_t frame_id = InjectStoppedFrame(&data_provider);

  // The whole expression is evaluated and its address taken, so the address must hold readable
  // data.
  data_provider->AddMemory(kAddress, {0, 0, 0, 0});

  dap::DataBreakpointInfoRequest info_req = {};
  info_req.frameId = frame_id;
  info_req.name = "*(uint32_t*)0x1234";
  auto info = GetDataBreakpointInfo(info_req);

  ASSERT_FALSE(info.error);
  ASSERT_TRUE(info.response.dataId.is<dap::string>());
  EXPECT_EQ(info.response.dataId.get<dap::string>(), MakeDataId(kAddress, 4));
  ASSERT_TRUE(info.response.accessTypes.has_value());
  EXPECT_EQ(info.response.accessTypes.value(),
            dap::array<dap::DataBreakpointAccessType>({"write", "readWrite"}));
  EXPECT_EQ(info.response.canPersist, dap::optional<dap::boolean>(false));

  // The dataId this handler produces must be accepted by the handler that consumes it.
  auto got =
      SetDataBreakpoints({dap::DataBreakpoint{.dataId = info.response.dataId.get<dap::string>()}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_TRUE(got.response.breakpoints[0].verified);
  EXPECT_FALSE(got.response.breakpoints[0].message.has_value());

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  ASSERT_TRUE(data_bps[0]);
  BreakpointSettings settings = data_bps[0]->GetSettings();
  EXPECT_EQ(settings.byte_size, 4u);
  ASSERT_EQ(settings.locations.size(), 1u);
  EXPECT_EQ(settings.locations[0].address, kAddress);
}

TEST_F(RequestDataBreakpointTest, DataBreakpointInfoNonMemoryValue) {
  InjectProcess(kProcessKoid);
  int64_t frame_id = InjectStoppedFrame();

  // A literal has no address, so there is nothing to watch.
  dap::DataBreakpointInfoRequest req = {};
  req.frameId = frame_id;
  req.name = "1 + 1";
  auto got = GetDataBreakpointInfo(req);

  ASSERT_FALSE(got.error);
  EXPECT_TRUE(got.response.dataId.is<dap::null>());
  EXPECT_FALSE(got.response.description.empty());
}

TEST_F(RequestDataBreakpointTest, DataBreakpointInfoEvalError) {
  InjectProcess(kProcessKoid);
  int64_t frame_id = InjectStoppedFrame();

  dap::DataBreakpointInfoRequest req = {};
  req.frameId = frame_id;
  req.name = "nonexistent_symbol";
  auto got = GetDataBreakpointInfo(req);

  ASSERT_TRUE(got.error);
  EXPECT_FALSE(got.error.message.empty());
}

TEST_F(RequestDataBreakpointTest, MissingContextAndNoActiveProcess) {
  dap::DataBreakpointInfoRequest info_req = {};
  info_req.name = "*(uint32_t*)0x1234";
  auto info = GetDataBreakpointInfo(info_req);
  ASSERT_TRUE(info.error);
  EXPECT_EQ(info.error.message, "Either variablesReference or frameId must be specified.");

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(),
            "No active process to set a data breakpoint on.");
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsCreatesWatchpoints) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({
      dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)},
      dap::DataBreakpoint{.accessType = "readWrite", .dataId = MakeDataId(0x2000, 8)},
  });

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 2u);
  EXPECT_TRUE(got.response.breakpoints[0].id.has_value());
  EXPECT_TRUE(got.response.breakpoints[0].verified);
  EXPECT_TRUE(got.response.breakpoints[1].id.has_value());
  EXPECT_TRUE(got.response.breakpoints[1].verified);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 2u);
  ASSERT_TRUE(data_bps[0]);
  ASSERT_TRUE(data_bps[1]);

  BreakpointSettings settings0 = data_bps[0]->GetSettings();
  EXPECT_EQ(settings0.type, BreakpointSettings::Type::kWrite);
  EXPECT_EQ(settings0.byte_size, 4u);
  ASSERT_EQ(settings0.locations.size(), 1u);
  EXPECT_EQ(settings0.locations[0].address, 0x1000u);

  BreakpointSettings settings1 = data_bps[1]->GetSettings();
  EXPECT_EQ(settings1.type, BreakpointSettings::Type::kReadWrite);
  EXPECT_EQ(settings1.byte_size, 8u);
  ASSERT_EQ(settings1.locations.size(), 1u);
  EXPECT_EQ(settings1.locations[0].address, 0x2000u);
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsReplacesPrevious) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({
      dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)},
      dap::DataBreakpoint{.dataId = MakeDataId(0x2000, 4)},
  });
  ASSERT_FALSE(got.error);
  ASSERT_EQ(context().GetDataBreakpoints().size(), 2u);

  got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x3000, 2)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_TRUE(got.response.breakpoints[0].verified);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  ASSERT_TRUE(data_bps[0]);
  ASSERT_EQ(data_bps[0]->GetSettings().locations.size(), 1u);
  EXPECT_EQ(data_bps[0]->GetSettings().locations[0].address, 0x3000u);
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsEmptyClearsAll) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(context().GetDataBreakpoints().size(), 1u);

  got = SetDataBreakpoints({});
  ASSERT_FALSE(got.error);
  EXPECT_TRUE(got.response.breakpoints.empty());
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsInvalidDataId) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({
      dap::DataBreakpoint{.dataId = "not_an_id"},
      dap::DataBreakpoint{.dataId = "0x1000:4"},
      dap::DataBreakpoint{.dataId = "0:0x1000:4"},
      dap::DataBreakpoint{.dataId = "1234:0x1000:-4"},
      dap::DataBreakpoint{.dataId = "1234:0x1000:-4294967292"},
      dap::DataBreakpoint{.dataId = "1234:0x1000:4294967297"},
      dap::DataBreakpoint{.dataId = "1234:0x1000:+4"},
      dap::DataBreakpoint{.dataId = " 1234:0x1000:4"},
      dap::DataBreakpoint{.dataId = "1234:0xfffffffffffffffe:4"},
  });

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 9u);
  for (const auto& bp : got.response.breakpoints) {
    EXPECT_FALSE(bp.verified);
    ASSERT_TRUE(bp.message.has_value());
    EXPECT_TRUE(bp.message.value().starts_with("Invalid dataId format: "));
  }
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsReadAccessTypeUnsupported) {
  auto got = SetDataBreakpoints(
      {dap::DataBreakpoint{.accessType = "read", .dataId = MakeDataId(0x1000, 4)}});

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(),
            "Read-only data breakpoints are not supported on this platform.");
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsUnknownAccessType) {
  auto got = SetDataBreakpoints(
      {dap::DataBreakpoint{.accessType = "sideways", .dataId = MakeDataId(0x1000, 4)}});

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(), "Unknown accessType: sideways");
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsInvalidSize) {
  // Hardware breakpoints only support sizes of 1, 2, 4, and 8 bytes.
  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 3)}});

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(),
            BreakpointSettings::ValidateSize(session().arch(), BreakpointSettings::Type::kWrite, 3)
                .msg());
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsTrailingGarbageInDataId) {
  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = "1234:0x1000:4junk"}});

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(),
            "Invalid dataId format: 1234:0x1000:4junk");
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsHitConditionUnsupported) {
  auto got = SetDataBreakpoints(
      {dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4), .hitCondition = "> 5"}});

  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(got.response.breakpoints[0].verified);
  ASSERT_TRUE(got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(got.response.breakpoints[0].message.value(),
            "Hit conditions are not supported for data breakpoints.");
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsScopedToProcessTarget) {
  Process* proc = InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_TRUE(got.response.breakpoints[0].verified);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  ASSERT_TRUE(data_bps[0]);

  // A raw address is only meaningful in one address space, so the watchpoint must be scoped to the
  // target of the process encoded in the dataId.
  BreakpointSettings settings = data_bps[0]->GetSettings();
  EXPECT_EQ(settings.scope.type(), ExecutionScope::kTarget);
  EXPECT_EQ(settings.scope.target(), proc->GetTarget());
}

TEST_F(RequestDataBreakpointTest, SetDataBreakpointsPropagatesCondition) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints(
      {dap::DataBreakpoint{.condition = "i == 2", .dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_TRUE(got.response.breakpoints[0].verified);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  ASSERT_TRUE(data_bps[0]);
  EXPECT_EQ(data_bps[0]->GetSettings().condition, "i == 2");
}

TEST_F(RequestDataBreakpointTest, DataBreakpointInfoInvalidFrameId) {
  dap::DataBreakpointInfoRequest req = {};
  req.name = "i";
  req.frameId = 9999;

  auto got = GetDataBreakpointInfo(req);

  ASSERT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Invalid frame ID.");
}

TEST_F(RequestDataBreakpointTest, DataBreakpointInfoInvalidVariablesReference) {
  dap::DataBreakpointInfoRequest req = {};
  req.name = "i";
  req.variablesReference = 9999;

  auto got = GetDataBreakpointInfo(req);

  ASSERT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Invalid variables reference.");
}

TEST_F(RequestDataBreakpointTest, VariablesReferenceScopesAndShadowing) {
  Process* process = InjectProcess(kProcessKoid);
  fxl::RefPtr<MockModuleSymbols> mod_sym = InjectMockModule(process);
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);
  RunPendingClientCalls();

  // Global variable `x` at 0x4000 (shadowed by parameter `x` at 0x2000 and local `x` at 0x3000),
  // plus unshadowed global variable `g_only` at 0x5000.
  auto global_x = fxl::MakeRefCounted<Variable>(
      DwarfTag::kVariable, "x", MakeInt32Type(),
      VariableLocation(
          DwarfExpr({llvm::dwarf::DW_OP_addr, 0x00, 0x40, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00})));
  TestIndexedSymbol indexed_global_x(mod_sym.get(), &mod_sym->index().root(), "x", global_x);

  auto global_only = fxl::MakeRefCounted<Variable>(
      DwarfTag::kVariable, "g_only", MakeInt32Type(),
      VariableLocation(
          DwarfExpr({llvm::dwarf::DW_OP_addr, 0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00})));
  TestIndexedSymbol indexed_global_only(mod_sym.get(), &mod_sym->index().root(), "g_only",
                                        global_only);

  // Parameter `x` at 0x2000, unnamed parameter at 0x2004, shadowed by local variable `x` at 0x3000.
  auto param_x = MakeVariableForTest(
      "x", MakeInt32Type(), 0x10000, 0x10020,
      DwarfExpr({llvm::dwarf::DW_OP_addr, 0x00, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}));
  auto unnamed_param = MakeVariableForTest(
      "", MakeInt32Type(), 0x10000, 0x10020,
      DwarfExpr({llvm::dwarf::DW_OP_addr, 0x04, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}));
  auto local_x = MakeVariableForTest(
      "x", MakeInt32Type(), 0x10000, 0x10020,
      DwarfExpr({llvm::dwarf::DW_OP_addr, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}));

  auto function = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  function->set_assigned_name("test_func");
  function->set_code_ranges(AddressRanges(AddressRange(0x10000, 0x10020)));
  function->set_parameters({LazySymbol(unnamed_param), LazySymbol(param_x)});
  function->set_variables({LazySymbol(local_x)});

  Location location(0x10010, FileLine("test.cc", 10), 0, SymbolContext::ForRelativeAddresses(),
                    function);
  auto mock_frame = std::make_unique<MockFrame>(&session(), thread, location, 0x7890);
  mock_frame->GetMockSymbolDataProvider()->set_ip(0x10010);
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x2000, {10, 0, 0, 0});
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x2004, {99, 0, 0, 0});
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x3000, {20, 0, 0, 0});
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x4000, {30, 0, 0, 0});
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x5000, {40, 0, 0, 0});

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(mock_frame));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);
  RunPendingClientCalls();

  int64_t frame_id = context().IdForFrame(kThreadKoid, 0);

  // 1. Valid frameId evaluation resolves innermost lexical scope (local_x at 0x3000), while "::x"
  // resolves shadowed global_x at 0x4000 and "g_only" resolves unshadowed global_only at 0x5000.
  {
    dap::DataBreakpointInfoRequest req = {};
    req.frameId = frame_id;
    req.name = "x";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x3000:4", kProcessKoid));

    dap::DataBreakpointInfoRequest global_shadowed_req = {};
    global_shadowed_req.frameId = frame_id;
    global_shadowed_req.name = "::x";
    auto global_shadowed_got = GetDataBreakpointInfo(global_shadowed_req);
    ASSERT_FALSE(global_shadowed_got.error);
    ASSERT_TRUE(global_shadowed_got.response.dataId.is<dap::string>());
    EXPECT_EQ(global_shadowed_got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x4000:4", kProcessKoid));

    dap::DataBreakpointInfoRequest global_unshadowed_req = {};
    global_unshadowed_req.frameId = frame_id;
    global_unshadowed_req.name = "g_only";
    auto global_unshadowed_got = GetDataBreakpointInfo(global_unshadowed_req);
    ASSERT_FALSE(global_unshadowed_got.error);
    ASSERT_TRUE(global_unshadowed_got.response.dataId.is<dap::string>());
    EXPECT_EQ(global_unshadowed_got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x5000:4", kProcessKoid));
  }

  // 2. VariablesType::kLocal resolves local_x at 0x3000.
  {
    int64_t locals_ref = context().IdForVariables(frame_id, VariablesType::kLocal);
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = locals_ref;
    req.name = "x";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x3000:4", kProcessKoid));
  }

  // 3. VariablesType::kArguments resolves param_x at 0x2000 despite local shadowing, and rejects
  // empty name even when an unnamed parameter exists.
  {
    int64_t args_ref = context().IdForVariables(frame_id, VariablesType::kArguments);
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = args_ref;
    req.name = "x";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x2000:4", kProcessKoid));

    dap::DataBreakpointInfoRequest empty_req = {};
    empty_req.variablesReference = args_ref;
    empty_req.name = "";
    auto empty_got = GetDataBreakpointInfo(empty_req);
    ASSERT_TRUE(empty_got.error);
    EXPECT_EQ(empty_got.error.message, "Function parameter '' not found.");
  }

  // 4. VariablesType::kRegister cannot be watched.
  {
    int64_t reg_ref = context().IdForVariables(frame_id, VariablesType::kRegister);
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = reg_ref;
    req.name = "rax";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    EXPECT_TRUE(got.response.dataId.is<dap::null>());
    EXPECT_EQ(got.response.description, "Registers cannot be watched.");
  }
}

TEST_F(RequestDataBreakpointTest, ChildVariableStructAndReferenceMembers) {
  InjectProcess(kProcessKoid);
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);
  RunPendingClientCalls();

  Location location(Location::State::kSymbolized, 0x10010);
  auto mock_frame = std::make_unique<MockFrame>(&session(), thread, location, 0x7890);
  // Target int at 0x5000 referenced by struct reference member `ref_member` stored at 0x4008.
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x5000, {42, 0, 0, 0});

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(mock_frame));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);
  RunPendingClientCalls();

  int64_t frame_id = context().IdForFrame(kThreadKoid, 0);

  auto parent_node = std::make_unique<FormatNode>("my_struct");
  parent_node->children().push_back(std::make_unique<FormatNode>(
      "val_member", ExprValue(MakeInt32Type(), {1, 0, 0, 0}, ExprValueSource(0x4000))));

  auto ref_type = fxl::MakeRefCounted<ModifiedType>(DwarfTag::kReferenceType, MakeInt32Type());
  parent_node->children().push_back(std::make_unique<FormatNode>(
      "ref_member", ExprValue(ref_type, {0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00},
                              ExprValueSource(0x4008))));
  parent_node->children().push_back(std::make_unique<FormatNode>(
      "uneval_ref_member",
      [ref_type](const fxl::RefPtr<EvalContext>&, fit::callback<void(const Err&, ExprValue)> cb) {
        cb(Err(), ExprValue(ref_type, {0x00, 0x50, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00},
                            ExprValueSource(0x4010)));
      }));

  int64_t child_ref =
      context().IdForVariables(frame_id, VariablesType::kChildVariable, std::move(parent_node));

  // Value member watches its own field address (0x4000, 4 bytes).
  {
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = child_ref;
    req.name = "val_member";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x4000:4", kProcessKoid));
  }

  // Reference member resolves reference to watch target memory (0x5000, 4 bytes) instead of 0x4008.
  {
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = child_ref;
    req.name = "ref_member";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x5000:4", kProcessKoid));
  }

  // Unevaluated reference member also resolves reference after FillFormatNodeValue completes.
  {
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = child_ref;
    req.name = "uneval_ref_member";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_FALSE(got.error);
    ASSERT_TRUE(got.response.dataId.is<dap::string>());
    EXPECT_EQ(got.response.dataId.get<dap::string>(),
              fxl::StringPrintf("%" PRIu64 ":0x5000:4", kProcessKoid));
  }

  // A name that is not a child of the container is bad input.
  {
    dap::DataBreakpointInfoRequest req = {};
    req.variablesReference = child_ref;
    req.name = "missing_member";
    auto got = GetDataBreakpointInfo(req);
    ASSERT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Child variable 'missing_member' not found.");
  }
}

TEST_F(RequestDataBreakpointTest, MultiProcessAddressSpaceScoping) {
  constexpr uint64_t kProcess2Koid = 999999;
  constexpr uint64_t kThread2Koid = 888888;

  Process* proc1 = InjectProcess(kProcessKoid);
  Target* target1 = proc1->GetTarget();

  session().system().CreateNewTarget(nullptr);
  Process* proc2 = InjectProcess(kProcess2Koid);
  Target* target2 = proc2->GetTarget();
  Thread* thread2 = InjectThread(kProcess2Koid, kThread2Koid);
  RunPendingClientCalls();

  // Ensure target1 remains the active console target while evaluating in proc2/thread2.
  context().console()->context().SetActiveTarget(target1);
  ASSERT_EQ(context().console()->context().GetActiveTarget(), target1);
  ASSERT_NE(target1, target2);

  Location location(Location::State::kSymbolized, 0x10010);
  auto mock_frame = std::make_unique<MockFrame>(&session(), thread2, location, 0x7890);
  mock_frame->GetMockSymbolDataProvider()->AddMemory(0x6000, {7, 0, 0, 0});

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(mock_frame));
  InjectExceptionWithStack(kProcess2Koid, kThread2Koid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);
  RunPendingClientCalls();

  int64_t frame2_id = context().IdForFrame(kThread2Koid, 0);

  dap::DataBreakpointInfoRequest info_req = {};
  info_req.frameId = frame2_id;
  info_req.name = "*(uint32_t*)0x6000";
  auto info = GetDataBreakpointInfo(info_req);
  ASSERT_FALSE(info.error);
  ASSERT_TRUE(info.response.dataId.is<dap::string>());
  std::string proc2_data_id = info.response.dataId.get<dap::string>();
  EXPECT_EQ(proc2_data_id, fxl::StringPrintf("%" PRIu64 ":0x6000:4", kProcess2Koid));

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = proc2_data_id}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  EXPECT_TRUE(got.response.breakpoints[0].verified);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  ASSERT_TRUE(data_bps[0]);
  EXPECT_EQ(data_bps[0]->GetSettings().scope.type(), ExecutionScope::kTarget);
  EXPECT_EQ(data_bps[0]->GetSettings().scope.target(), target2);

  // If proc2 exits before SetDataBreakpoints is called with proc2's dataId, it must not fall back
  // to installing the watchpoint on active target1.
  static_cast<TargetImpl*>(target2)->OnProcessExiting(0, 0);
  RunPendingClientCalls();
  auto stale_got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = proc2_data_id}});
  ASSERT_FALSE(stale_got.error);
  ASSERT_EQ(stale_got.response.breakpoints.size(), 1u);
  EXPECT_FALSE(stale_got.response.breakpoints[0].verified);
  ASSERT_TRUE(stale_got.response.breakpoints[0].message.has_value());
  EXPECT_EQ(stale_got.response.breakpoints[0].message.value(),
            "No active process to set a data breakpoint on.");
  EXPECT_TRUE(context().GetDataBreakpoints().empty());
}

TEST_F(RequestDataBreakpointTest, BreakpointUpdateFailureEmitsChangedEvent) {
  InjectProcess(kProcessKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  ASSERT_TRUE(got.response.breakpoints[0].id.has_value());
  dap::integer bp_id = got.response.breakpoints[0].id.value();

  int event_count = 0;
  client().registerHandler([&](const dap::BreakpointEvent& event) {
    EXPECT_EQ(event.reason, "changed");
    EXPECT_FALSE(event.breakpoint.verified);
    ASSERT_TRUE(event.breakpoint.id.has_value());
    EXPECT_EQ(event.breakpoint.id.value(), bp_id);
    ASSERT_TRUE(event.breakpoint.message.has_value());
    EXPECT_EQ(event.breakpoint.message.value(), "No hardware debug registers available.");
    event_count++;
  });

  // Internal breakpoints must not emit BreakpointEvents on update failure.
  Breakpoint* internal_bp = session().system().CreateNewInternalBreakpoint();
  context().OnBreakpointUpdateFailure(internal_bp, Err("Internal failure"));
  context().OnBreakpointUpdateFailure(nullptr, Err("Null failure"));
  session().system().DeleteBreakpoint(internal_bp);

  const auto& data_bps = context().GetDataBreakpoints();
  ASSERT_EQ(data_bps.size(), 1u);
  context().OnBreakpointUpdateFailure(data_bps[0].get(),
                                      Err("No hardware debug registers available."));
  RunPendingClientCalls();
  EXPECT_EQ(event_count, 1);
}

TEST_F(RequestDataBreakpointTest, WatchpointStopEmitsDataBreakpointReason) {
  InjectProcessWithModule(kProcessKoid);
  InjectThread(kProcessKoid, kThreadKoid);
  RunPendingClientCalls();

  auto got = SetDataBreakpoints({dap::DataBreakpoint{.dataId = MakeDataId(0x1000, 4)}});
  ASSERT_FALSE(got.error);
  ASSERT_EQ(got.response.breakpoints.size(), 1u);
  ASSERT_TRUE(got.response.breakpoints[0].id.has_value());
  dap::integer dap_bp_id = got.response.breakpoints[0].id.value();

  uint32_t backend_bp_id = mock_remote_api()->last_breakpoint_add().breakpoint.id;

  bool stopped_received = false;
  client().registerHandler([&](const dap::StoppedEvent& event) {
    EXPECT_EQ(event.reason, "data breakpoint");
    ASSERT_TRUE(event.description.has_value());
    EXPECT_EQ(event.description.value(), "Data breakpoint hit");
    ASSERT_TRUE(event.hitBreakpointIds.has_value());
    ASSERT_EQ(event.hitBreakpointIds->size(), 1u);
    EXPECT_EQ(event.hitBreakpointIds.value()[0], dap_bp_id);
    stopped_received = true;
  });

  debug_ipc::NotifyException exception;
  exception.type = debug_ipc::ExceptionType::kWatchpoint;
  exception.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  exception.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  exception.thread.frames.emplace_back(0x10010, 0x7890, 0x7890);
  exception.hit_breakpoints.push_back({.id = backend_bp_id});
  InjectException(exception);

  RunPendingClientCalls();
  EXPECT_TRUE(stopped_received);
}

}  // namespace

}  // namespace zxdb
