// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/fdf_dispatcher_task_provider.h"

#include <algorithm>
#include <cstring>
#include <optional>
#include <vector>

#include <gtest/gtest.h>

#include "llvm/BinaryFormat/Dwarf.h"
#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/process_impl.h"
#include "src/developer/debug/zxdb/client/remote_api_test.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/common/address_ranges.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/developer/debug/zxdb/symbols/array_type.h"
#include "src/developer/debug/zxdb/symbols/base_type.h"
#include "src/developer/debug/zxdb/symbols/collection.h"
#include "src/developer/debug/zxdb/symbols/data_member.h"
#include "src/developer/debug/zxdb/symbols/dwarf_expr.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/index_node.h"
#include "src/developer/debug/zxdb/symbols/index_test_support.h"
#include "src/developer/debug/zxdb/symbols/mock_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/mock_symbol_data_provider.h"
#include "src/developer/debug/zxdb/symbols/modified_type.h"
#include "src/developer/debug/zxdb/symbols/namespace.h"
#include "src/developer/debug/zxdb/symbols/symbol_test_parent_setter.h"
#include "src/developer/debug/zxdb/symbols/template_parameter.h"
#include "src/developer/debug/zxdb/symbols/variable_test_support.h"

namespace zxdb {

namespace {

fxl::RefPtr<DataMember> MakeField(const char* name, fxl::RefPtr<Type> type, uint32_t offset) {
  auto dm = fxl::MakeRefCounted<DataMember>(name, std::move(type), offset);
  return dm;
}

// Registers the DWARF struct definitions required by FdfDispatcherTaskProvider in mod.
fxl::RefPtr<Collection> RegisterMockRuntimeTypes(MockModuleSymbols* mod) {
  auto uint64_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 8, "uint64_t");
  auto uint8_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 1, "uint8_t");
  auto char_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeSignedChar, 1, "char");

  IndexNode& root = mod->index().root();
  IndexNode* ns_driver_runtime = root.AddChild(IndexNode::Kind::kNamespace, "driver_runtime");
  IndexNode* ns_async = root.AddChild(IndexNode::Kind::kNamespace, "async");
  IndexNode* ns_async_internal = ns_async->AddChild(IndexNode::Kind::kNamespace, "internal");
  IndexNode* ns_fit = root.AddChild(IndexNode::Kind::kNamespace, "fit");
  IndexNode* ns_fit_internal = ns_fit->AddChild(IndexNode::Kind::kNamespace, "internal");

  auto char_arr_32 = fxl::MakeRefCounted<ArrayType>(char_type, 32);
  auto string_buf_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "StringBuffer");
  string_buf_coll->set_byte_size(48);
  string_buf_coll->set_data_members({
      LazySymbol(MakeField("length_", uint64_type, 0)),
      LazySymbol(MakeField("data_", char_arr_32, 8)),
  });

  auto dll_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "DoublyLinkedList");
  dll_coll->set_byte_size(8);
  dll_coll->set_data_members({
      LazySymbol(MakeField("head_", uint64_type, 0)),
  });

  auto disp_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "Dispatcher");
  disp_coll->set_byte_size(264);
  disp_coll->set_data_members({
      LazySymbol(MakeField("name_", string_buf_coll, 16)),
      LazySymbol(MakeField("registered_callbacks_", dll_coll, 136)),
      LazySymbol(MakeField("callback_queue_", dll_coll, 144)),
      LazySymbol(MakeField("delayed_tasks_", dll_coll, 216)),
      LazySymbol(MakeField("always_on_delayed_tasks_", dll_coll, 256)),
  });
  TestIndexedSymbol indexed_disp(mod, ns_driver_runtime, "Dispatcher", disp_coll);

  auto node_state_coll =
      fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "DoublyLinkedListNodeState");
  node_state_coll->set_byte_size(16);
  node_state_coll->set_data_members({
      LazySymbol(MakeField("next_", uint64_type, 0)),
      LazySymbol(MakeField("prev_", uint64_type, 8)),
  });

  auto bits_arr_24 = fxl::MakeRefCounted<ArrayType>(uint8_type, 24);
  auto fit_callback_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "function_base");
  fit_callback_coll->set_byte_size(32);
  fit_callback_coll->set_data_members({
      LazySymbol(MakeField("bits_", bits_arr_24, 0)),
      LazySymbol(MakeField("ops_", uint64_type, 24)),
  });

  auto opt_void_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "optional");
  opt_void_coll->set_byte_size(16);
  opt_void_coll->set_data_members({
      LazySymbol(MakeField("__val_", uint64_type, 0)),
      LazySymbol(MakeField("__engaged_", uint8_type, 8)),
  });

  auto cb_req_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "CallbackRequest");
  cb_req_coll->set_byte_size(96);
  cb_req_coll->set_data_members({
      LazySymbol(MakeField("dll_node_state_", node_state_coll, 16)),
      LazySymbol(MakeField("callback_", fit_callback_coll, 48)),
      LazySymbol(MakeField("async_operation_", opt_void_coll, 80)),
  });
  TestIndexedSymbol indexed_cb_req(mod, ns_driver_runtime, "CallbackRequest", cb_req_coll);

  auto state_arr_16 = fxl::MakeRefCounted<ArrayType>(uint8_type, 16);
  auto async_task_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "async_task");
  async_task_coll->set_byte_size(32);
  async_task_coll->set_data_members({
      LazySymbol(MakeField("state", state_arr_16, 0)),
      LazySymbol(MakeField("handler", uint64_type, 16)),
      LazySymbol(MakeField("deadline", uint64_type, 24)),
  });
  TestIndexedSymbol indexed_async_task(mod, &root, "async_task", async_task_coll);

  auto bits_arr_16 = fxl::MakeRefCounted<ArrayType>(uint8_type, 16);
  auto fit_closure_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "closure");
  fit_closure_coll->set_byte_size(24);
  fit_closure_coll->set_data_members({
      LazySymbol(MakeField("bits_", bits_arr_16, 0)),
      LazySymbol(MakeField("ops_", uint64_type, 16)),
  });

  auto retained_task_coll =
      fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "RetainedTask");
  retained_task_coll->set_byte_size(64);
  retained_task_coll->set_data_members({
      LazySymbol(MakeField("handler", fit_closure_coll, 32)),
  });
  TestIndexedSymbol indexed_retained_task(mod, ns_async_internal, "RetainedTask",
                                          retained_task_coll);

  auto task_closure_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "TaskClosure");
  task_closure_coll->set_byte_size(64);
  task_closure_coll->set_data_members({
      LazySymbol(MakeField("handler_", fit_closure_coll, 40)),
  });
  TestIndexedSymbol indexed_task_closure(mod, ns_async, "TaskClosure", task_closure_coll);

  auto target_ops_coll =
      fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "target_ops<void>");
  target_ops_coll->set_byte_size(24);
  target_ops_coll->set_data_members({
      LazySymbol(MakeField("move", uint64_type, 0)),
      LazySymbol(MakeField("destroy", uint64_type, 8)),
      LazySymbol(MakeField("invoke", uint64_type, 16)),
  });
  TestIndexedSymbol indexed_target_ops(mod, ns_fit_internal, "target_ops<void>", target_ops_coll);

  auto list_node_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "list_node");
  list_node_coll->set_byte_size(16);
  list_node_coll->set_data_members({
      LazySymbol(MakeField("prev", uint64_type, 0)),
      LazySymbol(MakeField("next", uint64_type, 8)),
  });
  auto async_loop_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kStructureType, "async_loop");
  async_loop_coll->set_byte_size(96);
  async_loop_coll->set_data_members({
      LazySymbol(MakeField("wait_list", list_node_coll, 72)),
  });
  TestIndexedSymbol indexed_async_loop(mod, &root, "async_loop", async_loop_coll);

  auto ref_ptr_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "RefPtr");
  ref_ptr_coll->set_byte_size(8);
  ref_ptr_coll->set_data_members({
      LazySymbol(MakeField("ptr_", uint64_type, 0)),
  });
  auto event_waiter_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "EventWaiter");
  event_waiter_coll->set_byte_size(32);
  event_waiter_coll->set_data_members({
      LazySymbol(MakeField("dispatcher_ref_", ref_ptr_coll, 24)),
  });
  TestIndexedSymbol indexed_event_waiter(mod, ns_driver_runtime, "EventWaiter", event_waiter_coll);

  return disp_coll;
}

DwarfExpr MakeConstantAddressExpr(uint64_t addr) {
  return DwarfExpr({
      llvm::dwarf::DW_OP_addr,
      static_cast<uint8_t>(addr & 0xffu),
      static_cast<uint8_t>((addr >> 8u) & 0xffu),
      static_cast<uint8_t>((addr >> 16u) & 0xffu),
      static_cast<uint8_t>((addr >> 24u) & 0xffu),
      static_cast<uint8_t>((addr >> 32u) & 0xffu),
      static_cast<uint8_t>((addr >> 40u) & 0xffu),
      static_cast<uint8_t>((addr >> 48u) & 0xffu),
      static_cast<uint8_t>((addr >> 56u) & 0xffu),
      llvm::dwarf::DW_OP_stack_value,
  });
}

// Helper to construct a mock Function symbol with an optional "this" pointer variable.
fxl::RefPtr<Function> CreateMockDispatcherFunction(const std::string& name,
                                                   std::optional<uint64_t> this_ptr_addr,
                                                   fxl::RefPtr<Type> pointed_to_type = nullptr) {
  auto func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  func->set_assigned_name(name);
  func->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));
  if (this_ptr_addr) {
    if (!pointed_to_type) {
      pointed_to_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 8, "uint64_t");
    }
    auto disp_ptr_type =
        fxl::MakeRefCounted<ModifiedType>(DwarfTag::kPointerType, std::move(pointed_to_type));
    auto this_var = MakeVariableForTest("this", disp_ptr_type,
                                        VariableLocation(MakeConstantAddressExpr(*this_ptr_addr)));
    func->set_parameters({LazySymbol(this_var)});
    func->set_object_pointer(this_var);
  }
  return func;
}

// Configures mock memory for a 264-byte Dispatcher object with queues initialized to sentinels or
// explicit head addresses.
void SetupMockDispatcher(MockSymbolDataProvider* mem_provider, uint64_t disp_addr,
                         const std::string& name, size_t reported_name_len,
                         bool use_odd_sentinel = true,
                         std::optional<uint64_t> callback_queue_head = std::nullopt,
                         std::optional<uint64_t> delayed_tasks_head = std::nullopt,
                         std::optional<uint64_t> registered_callbacks_head = std::nullopt) {
  std::vector<uint8_t> disp_bytes(264, 0);

  std::memcpy(disp_bytes.data() + 16, &reported_name_len, sizeof(size_t));
  if (!name.empty()) {
    std::memcpy(disp_bytes.data() + 24, name.data(), std::min(name.size(), size_t{32}));
  }

  auto make_sentinel = [disp_addr, use_odd_sentinel](uint64_t offset) -> uint64_t {
    return use_odd_sentinel ? ((disp_addr + offset) | 1ULL) : 0ULL;
  };

  uint64_t reg_head = registered_callbacks_head.value_or(make_sentinel(136));
  std::memcpy(disp_bytes.data() + 136, &reg_head, sizeof(uint64_t));

  uint64_t cb_head = callback_queue_head.value_or(make_sentinel(144));
  std::memcpy(disp_bytes.data() + 144, &cb_head, sizeof(uint64_t));

  uint64_t del_head = delayed_tasks_head.value_or(make_sentinel(216));
  std::memcpy(disp_bytes.data() + 216, &del_head, sizeof(uint64_t));

  uint64_t always_head = make_sentinel(256);
  std::memcpy(disp_bytes.data() + 256, &always_head, sizeof(uint64_t));

  mem_provider->AddMemory(disp_addr, std::move(disp_bytes));
}

class FdfDispatcherTaskProviderTest : public RemoteAPITest {
 public:
  FdfDispatcherTaskProviderTest() = default;

  void EnsureTestTargetInitialized() {
    if (!process_) {
      process_ = InjectProcess(0x1234);
      thread_ = InjectThread(0x1234, 0x5678);
      mock_mod_ = InjectMockModule(process_, 0x1000);
      disp_coll_ = RegisterMockRuntimeTypes(mock_mod_.get());
    }
  }

 protected:
  Process* process_ = nullptr;
  Thread* thread_ = nullptr;
  fxl::RefPtr<MockModuleSymbols> mock_mod_;
  fxl::RefPtr<Collection> disp_coll_;
};

TEST_F(FdfDispatcherTaskProviderTest, CanHandle) {
  FdfDispatcherTaskProvider provider;

  auto check_can_handle = [&](const std::string& name, bool expected) {
    auto func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
    func->set_assigned_name(name);
    Location loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(nullptr, nullptr, loc, 0);
    EXPECT_EQ(expected, provider.CanHandle(&frame)) << "For function: " << name;
  };

  check_can_handle("driver_runtime::Dispatcher::DispatchCallback", true);
  check_can_handle("driver_runtime::Dispatcher::DispatchCallbacks", true);
  check_can_handle("driver_runtime::Dispatcher::QueueRegisteredCallback", true);
  check_can_handle("driver_runtime::Dispatcher::DispatchCallbacks<int>", true);
  check_can_handle("driver_runtime::DispatcherCoordinator::TestingRun", true);
  check_can_handle("driver_runtime::DispatcherCoordinator::TestingRunUntilIdle", true);
  check_can_handle("fdf_testing::DriverRuntime::Run", true);
  check_can_handle("some::other::function", false);
  check_can_handle("async_loop_run_once", false);
}

TEST_F(FdfDispatcherTaskProviderTest, ProcessRegistration) {
  Process* process = InjectProcess(0x1234);
  ASSERT_NE(nullptr, process);

  auto c_providers = process->GetAsyncTaskProvidersForLanguage(ExprLanguage::kC);
  EXPECT_EQ(1u, c_providers.size());

  auto rust_providers = process->GetAsyncTaskProvidersForLanguage(ExprLanguage::kRust);
  EXPECT_EQ(1u, rust_providers.size());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksQueuedTask) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kQueueAddr = kDispatcherAddr + 144;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);

  auto handler_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  handler_func->set_assigned_name("HandleData");
  Location handler_loc(kHandlerAddr, FileLine("driver.cc", 42), 0,
                       SymbolContext::ForRelativeAddresses(), handler_func);
  mock_mod_->AddSymbolLocations(kHandlerAddr, {handler_loc});

  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "my_driver", 9,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  // CallbackRequest at kReqAddr.
  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));  // dll_node_state_.next_
  std::memcpy(req_bytes.data() + 56, &kTaskAddr, sizeof(uint64_t));      // callback_.bits_[8..16]
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));      // async_operation_.__val_
  req_bytes[88] = 1;  // async_operation_.__engaged_
  mem_provider->AddMemory(kReqAddr, req_bytes);

  // async_task at kTaskAddr.
  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));  // handler
  uint64_t deadline = 0;
  std::memcpy(task_bytes.data() + 24, &deadline, sizeof(uint64_t));  // deadline
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("my_driver", root->GetState());
  EXPECT_EQ(kDispatcherAddr, root->GetId());

  auto children = root->GetChildren();
  ASSERT_EQ(1u, children.size());

  const auto& task = children[0].get();
  EXPECT_EQ(AsyncTask::Type::kTask, task.GetType());
  EXPECT_EQ(kTaskAddr, task.GetId());

  auto task_children = task.GetChildren();
  ASSERT_EQ(1u, task_children.size());

  const auto& func_node = task_children[0].get();
  EXPECT_EQ(AsyncTask::Type::kFunction, func_node.GetType());
  EXPECT_EQ("Pending", func_node.GetState());
  EXPECT_EQ("HandleData", func_node.GetIdentifier().GetFullNameNoQual());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksDelayedTask) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kDelayedQueueAddr = kDispatcherAddr + 216;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;
  constexpr uint64_t kDeadline = 987654321;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallbacks",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "delayed", 7,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/std::nullopt,
                      /*delayed_tasks_head=*/kReqAddr);

  // CallbackRequest at kReqAddr.
  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kDelayedQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));
  req_bytes[88] = 1;
  mem_provider->AddMemory(kReqAddr, req_bytes);

  // async_task at kTaskAddr with deadline.
  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  uint64_t dl = kDeadline;
  std::memcpy(task_bytes.data() + 24, &dl, sizeof(uint64_t));
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("delayed", root->GetState());

  auto children = root->GetChildren();
  ASSERT_EQ(1u, children.size());

  const auto& task = children[0].get();
  EXPECT_EQ(AsyncTask::Type::kTask, task.GetType());

  auto task_children = task.GetChildren();
  ASSERT_EQ(1u, task_children.size());

  const auto& func_node = task_children[0].get();
  EXPECT_EQ("Delayed", func_node.GetState());

  bool found_deadline = false;
  for (const auto& val : func_node.GetValues()) {
    if (val.name && *val.name == "deadline") {
      found_deadline = true;
      uint64_t v = 0;
      EXPECT_TRUE(val.value.PromoteTo64(&v).ok());
      EXPECT_EQ(kDeadline, v);
    }
  }
  EXPECT_TRUE(found_deadline);
}

TEST_F(FdfDispatcherTaskProviderTest, CanHandleEdgeCases) {
  FdfDispatcherTaskProvider provider;

  EXPECT_FALSE(provider.CanHandle(nullptr));

  Location empty_loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), LazySymbol());
  MockFrame empty_frame(nullptr, nullptr, empty_loc, 0);
  EXPECT_FALSE(provider.CanHandle(&empty_frame));

  auto empty_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  empty_func->set_assigned_name("");
  Location empty_name_loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), empty_func);
  MockFrame empty_name_frame(nullptr, nullptr, empty_name_loc, 0);
  EXPECT_FALSE(provider.CanHandle(&empty_name_frame));

  auto check_non_matching = [&](const std::string& name) {
    auto func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
    func->set_assigned_name(name);
    Location loc(0x1234, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(nullptr, nullptr, loc, 0);
    EXPECT_FALSE(provider.CanHandle(&frame)) << "Should not handle: " << name;
  };

  check_non_matching("main");
  check_non_matching("std::vector<int>::push_back");
  check_non_matching("fuchsia_async::runtime::fuchsia::executor::local::LocalExecutor::run");
  check_non_matching("driver_runtime::UnknownFunction");
  check_non_matching("fdf::Unknown");
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksNullAndUnmappedThis) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  // When the this pointer evaluates to 0x0.
  {
    auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback", 0x0,
                                             disp_coll_);
    Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(&session(), thread_, frame_loc, 0x8000);

    bool called = false;
    provider.GetTasks(&frame,
                      [&](const Err& err, const std::vector<std::unique_ptr<AsyncTask>>& tasks) {
                        called = true;
                        EXPECT_TRUE(err.has_error());
                      });

    loop().RunUntilNoTasks();
    EXPECT_TRUE(called);
  }

  // When the this pointer points to unmapped memory.
  {
    constexpr uint64_t kUnmappedAddr = 0xdeadbeef0000;
    auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                             kUnmappedAddr, disp_coll_);
    Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(&session(), thread_, frame_loc, 0x8000);

    bool called = false;
    provider.GetTasks(&frame,
                      [&](const Err& err, const std::vector<std::unique_ptr<AsyncTask>>& tasks) {
                        called = true;
                        EXPECT_TRUE(err.has_error());
                      });

    loop().RunUntilNoTasks();
    EXPECT_TRUE(called);
  }

  // When the frame has no this parameter or object pointer.
  {
    auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                             std::nullopt, disp_coll_);
    Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(&session(), thread_, frame_loc, 0x8000);

    bool called = false;
    provider.GetTasks(&frame,
                      [&](const Err& err, const std::vector<std::unique_ptr<AsyncTask>>& tasks) {
                        called = true;
                        EXPECT_TRUE(err.has_error());
                      });

    loop().RunUntilNoTasks();
    EXPECT_TRUE(called);
  }
}

TEST_F(FdfDispatcherTaskProviderTest, EmptyQueueSentinelHandling) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "sentinel_driver", 15,
                      /*use_odd_sentinel=*/true);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("sentinel_driver", root->GetState());
  EXPECT_EQ(kDispatcherAddr, root->GetId());
  EXPECT_EQ(0u, root->GetChildren().size());
}

TEST_F(FdfDispatcherTaskProviderTest, EmptyQueueZeroHeadHandling) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "zero_head_driver", 16,
                      /*use_odd_sentinel=*/false);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("zero_head_driver", root->GetState());
  EXPECT_EQ(0u, root->GetChildren().size());
}

TEST_F(FdfDispatcherTaskProviderTest, CyclicQueueSelfLoopHandling) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "cycle_driver", 12,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  std::vector<uint8_t> req_bytes(96, 0);
  std::memcpy(req_bytes.data() + 16, &kReqAddr, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 56, &kTaskAddr, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));
  req_bytes[88] = 1;
  mem_provider->AddMemory(kReqAddr, req_bytes);

  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(1u, root->GetChildren().size());
}

TEST_F(FdfDispatcherTaskProviderTest, CyclicQueueTwoCycleHandling) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kReq1Addr = 0x30000;
  constexpr uint64_t kReq2Addr = 0x31000;
  constexpr uint64_t kTask1Addr = 0x40000;
  constexpr uint64_t kTask2Addr = 0x41000;
  constexpr uint64_t kHandlerAddr = 0x50000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "cycle2_driver", 13,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReq1Addr);

  std::vector<uint8_t> req1_bytes(96, 0);
  std::memcpy(req1_bytes.data() + 16, &kReq2Addr, sizeof(uint64_t));
  std::memcpy(req1_bytes.data() + 80, &kTask1Addr, sizeof(uint64_t));
  req1_bytes[88] = 1;
  mem_provider->AddMemory(kReq1Addr, req1_bytes);

  std::vector<uint8_t> req2_bytes(96, 0);
  std::memcpy(req2_bytes.data() + 16, &kReq1Addr, sizeof(uint64_t));
  std::memcpy(req2_bytes.data() + 80, &kTask2Addr, sizeof(uint64_t));
  req2_bytes[88] = 1;
  mem_provider->AddMemory(kReq2Addr, req2_bytes);

  std::vector<uint8_t> task1_bytes(64, 0);
  std::memcpy(task1_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTask1Addr, task1_bytes);

  std::vector<uint8_t> task2_bytes(64, 0);
  std::memcpy(task2_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTask2Addr, task2_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(2u, root->GetChildren().size());
}

TEST_F(FdfDispatcherTaskProviderTest, TruncatedOrUnmappedCallbackRequest) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;
  constexpr uint64_t kUnmappedNextAddr = 0xdeadbeef0000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "truncated_driver", 16,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  std::vector<uint8_t> req_bytes(96, 0);
  std::memcpy(req_bytes.data() + 16, &kUnmappedNextAddr, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));
  req_bytes[88] = 1;
  mem_provider->AddMemory(kReqAddr, req_bytes);

  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(1u, root->GetChildren().size());
  EXPECT_EQ(kTaskAddr, root->GetChildren()[0].get().GetId());
}

TEST_F(FdfDispatcherTaskProviderTest, DispatcherNameEdgeCases) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  // Empty name scenario.
  {
    constexpr uint64_t kDispatcherAddr = 0x20000;
    auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                             kDispatcherAddr, disp_coll_);
    Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(&session(), thread_, frame_loc, 0x8000);

    auto* mem_provider = frame.GetMockSymbolDataProvider();
    SetupMockDispatcher(mem_provider, kDispatcherAddr, "", 0, /*use_odd_sentinel=*/true);

    bool called = false;
    std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
    provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
      called = true;
      EXPECT_FALSE(err.has_error()) << err.msg();
      retrieved_tasks = std::move(tasks);
    });

    loop().RunUntilNoTasks();
    EXPECT_TRUE(called);
    ASSERT_EQ(1u, retrieved_tasks.size());
    EXPECT_TRUE(retrieved_tasks[0]->GetState().empty());
  }

  // Oversized reported length must be clamped safely without out-of-bounds reading.
  {
    constexpr uint64_t kDispatcherAddr = 0x20000;
    auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                             kDispatcherAddr, disp_coll_);
    Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
    MockFrame frame(&session(), thread_, frame_loc, 0x8000);

    auto* mem_provider = frame.GetMockSymbolDataProvider();
    SetupMockDispatcher(mem_provider, kDispatcherAddr, "clamped_driver_name", 999999,
                        /*use_odd_sentinel=*/true);

    bool called = false;
    std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
    provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
      called = true;
      EXPECT_FALSE(err.has_error()) << err.msg();
      retrieved_tasks = std::move(tasks);
    });

    loop().RunUntilNoTasks();
    EXPECT_TRUE(called);
    ASSERT_EQ(1u, retrieved_tasks.size());
    EXPECT_LE(retrieved_tasks[0]->GetState().size(), 32u);
  }
}

TEST_F(FdfDispatcherTaskProviderTest, CallbackRequestNullTarget) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kQueueAddr = kDispatcherAddr + 144;
  constexpr uint64_t kReqAddr = 0x30000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);
  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "null_target_driver", 18,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));
  mem_provider->AddMemory(kReqAddr, req_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());
  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksLambdaTask) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kQueueAddr = kDispatcherAddr + 144;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;
  constexpr uint64_t kClosureOpsAddr = 0x60000;
  constexpr uint64_t kInvokeAddr = 0x70000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);

  // Handler function symbol for RetainedTask::Handler.
  auto handler_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  handler_func->set_assigned_name("async::internal::RetainedTask::Handler");
  Location handler_loc(kHandlerAddr, FileLine("retained_task.cc", 10), 0,
                       SymbolContext::ForRelativeAddresses(), handler_func);

  // Invoke function symbol with a Clang DWARF lambda name and a template
  // parameter whose lambda Collection has a default invalid parent
  // UncachedLazySymbol.
  auto lambda_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType);
  auto tparam = fxl::MakeRefCounted<TemplateParameter>("Callable", lambda_coll, false);
  auto invoke_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  invoke_func->set_assigned_name(
      "fit::internal::target<(lambda at ../../src/driver.cc:42:15), false, false, void>::invoke");
  invoke_func->set_template_params({LazySymbol(tparam)});
  Location invoke_loc(kInvokeAddr, FileLine("internal/function.h", 114), 0,
                      SymbolContext::ForRelativeAddresses(), invoke_func);

  mock_mod_->AddSymbolLocations(kHandlerAddr, {handler_loc});
  mock_mod_->AddSymbolLocations(kInvokeAddr, {invoke_loc});

  MockFrame frame(&session(), thread_, frame_loc, 0x8000);

  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "lambda_driver", 13,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 56, &kTaskAddr, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));
  req_bytes[88] = 1;
  mem_provider->AddMemory(kReqAddr, req_bytes);

  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  std::memcpy(task_bytes.data() + 48, &kClosureOpsAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  std::vector<uint8_t> ops_bytes(24, 0);
  std::memcpy(ops_bytes.data() + 16, &kInvokeAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kClosureOpsAddr, ops_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("lambda_driver", root->GetState());

  auto children = root->GetChildren();
  ASSERT_EQ(1u, children.size());

  const auto& task = children[0].get();
  EXPECT_EQ(AsyncTask::Type::kTask, task.GetType());

  auto task_children = task.GetChildren();
  ASSERT_EQ(1u, task_children.size());

  const auto& func_node = task_children[0].get();
  EXPECT_EQ(AsyncTask::Type::kFunction, func_node.GetType());
  EXPECT_EQ("Pending", func_node.GetState());
  EXPECT_EQ("{closure}", func_node.GetIdentifier().GetFullNameNoQual());
  EXPECT_EQ("../../src/driver.cc", func_node.GetLocation().file_line().file());
  EXPECT_EQ(42, func_node.GetLocation().file_line().line());
}

TEST_F(FdfDispatcherTaskProviderTest, FallbackToDispatchCallbacksWhenThisUnavailable) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;

  // Top frame: DispatchCallback with no this parameter or object pointer,
  // simulating a clobbered x0 register on ARM64.
  auto func_callback = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  func_callback->set_assigned_name("driver_runtime::Dispatcher::DispatchCallback");
  Location frame0_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func_callback);

  // Caller frame: DispatchCallbacks with a valid this pointer pointing to
  // kDispatcherAddr.
  auto func_callbacks = CreateMockDispatcherFunction(
      "driver_runtime::Dispatcher::DispatchCallbacks", kDispatcherAddr, disp_coll_);
  Location frame1_loc(0x2000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func_callbacks);

  std::vector<std::unique_ptr<Frame>> stack_frames;
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame0_loc, 0x8000));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame1_loc, 0x8100));

  Frame* top_frame = stack_frames[0].get();
  auto* mem_provider0 = static_cast<MockFrame*>(stack_frames[0].get())->GetMockSymbolDataProvider();
  auto* mem_provider1 = static_cast<MockFrame*>(stack_frames[1].get())->GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider0, kDispatcherAddr, "arm64_driver", 12,
                      /*use_odd_sentinel=*/true);
  SetupMockDispatcher(mem_provider1, kDispatcherAddr, "arm64_driver", 12,
                      /*use_odd_sentinel=*/true);

  thread_->GetStack().SetFramesForTest(std::move(stack_frames), true);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(top_frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());
  const auto& root = retrieved_tasks[0];
  EXPECT_EQ(AsyncTask::Type::kScope, root->GetType());
  EXPECT_EQ("arm64_driver", root->GetState());
  EXPECT_EQ(kDispatcherAddr, root->GetId());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksActiveRunningTaskAndQueuedTasks) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kQueueAddr = kDispatcherAddr + 144;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kTaskAddr = 0x40000;
  constexpr uint64_t kHandlerAddr = 0x50000;

  // Frame 0 (innermost): CrashInDriverTask.
  auto crash_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  crash_func->set_assigned_name("CrashInDriverTask");
  Location frame0_loc(0x1100, FileLine("driver.cc", 14), 0, SymbolContext::ForRelativeAddresses(),
                      crash_func);

  // Frame 1: user task entry frame MyDriver::OnWork.
  auto entry_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  entry_func->set_assigned_name("MyDriver::OnWork");
  Location frame1_loc(0x1200, FileLine("driver.cc", 28), 0, SymbolContext::ForRelativeAddresses(),
                      entry_func);

  // Frame 2: runtime frame fit::internal::target::invoke (filtered by IsRuntimeDispatchFrame).
  auto runtime_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  runtime_func->set_assigned_name("fit::internal::target::invoke");
  Location frame2_loc(0x1300, FileLine("function.h", 100), 0, SymbolContext::ForRelativeAddresses(),
                      runtime_func);

  // Frame 3: driver_runtime::Dispatcher::DispatchCallback with valid this pointer.
  auto disp_func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                                kDispatcherAddr, disp_coll_);
  Location frame3_loc(0x1400, FileLine("dispatcher.cc", 500), 0,
                      SymbolContext::ForRelativeAddresses(), disp_func);

  auto handler_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  handler_func->set_assigned_name("PendingTaskHandler");
  Location handler_loc(kHandlerAddr, FileLine("driver.cc", 60), 0,
                       SymbolContext::ForRelativeAddresses(), handler_func);
  mock_mod_->AddSymbolLocations(kHandlerAddr, {handler_loc});

  std::vector<std::unique_ptr<Frame>> stack_frames;
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame0_loc, 0x8000));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame1_loc, 0x8100));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame2_loc, 0x8200));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame3_loc, 0x8300));

  Frame* disp_frame = stack_frames[3].get();
  auto* mem_provider = static_cast<MockFrame*>(disp_frame)->GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "active_driver", 13,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/kReqAddr);

  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 80, &kTaskAddr, sizeof(uint64_t));
  req_bytes[88] = 1;
  mem_provider->AddMemory(kReqAddr, req_bytes);

  std::vector<uint8_t> task_bytes(64, 0);
  std::memcpy(task_bytes.data() + 16, &kHandlerAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kTaskAddr, task_bytes);

  thread_->GetStack().SetFramesForTest(std::move(stack_frames), true);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(disp_frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());

  const auto& root = retrieved_tasks[0];
  EXPECT_EQ("active_driver", root->GetState());
  auto children = root->GetChildren();
  ASSERT_EQ(2u, children.size());

  const auto& running_task = children[0].get();
  EXPECT_EQ(kDispatcherAddr + 1, running_task.GetId());
  ASSERT_EQ(1u, running_task.GetChildren().size());

  const auto& entry_node = running_task.GetChildren()[0].get();
  EXPECT_EQ("Running", entry_node.GetState());
  EXPECT_EQ("MyDriver::OnWork", entry_node.GetIdentifier().GetFullNameNoQual());
  ASSERT_EQ(1u, entry_node.GetChildren().size());

  const auto& top_node = entry_node.GetChildren()[0].get();
  EXPECT_EQ("Running", top_node.GetState());
  EXPECT_EQ("CrashInDriverTask", top_node.GetIdentifier().GetFullNameNoQual());

  const auto& pending_task = children[1].get();
  ASSERT_EQ(1u, pending_task.GetChildren().size());
  EXPECT_EQ("Pending", pending_task.GetChildren()[0].get().GetState());
  EXPECT_EQ("PendingTaskHandler",
            pending_task.GetChildren()[0].get().GetIdentifier().GetFullNameNoQual());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksDirectCallbackRequestLambda) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kDispatcherAddr = 0x20000;
  constexpr uint64_t kRegQueueAddr = kDispatcherAddr + 136;
  constexpr uint64_t kReqAddr = 0x30000;
  constexpr uint64_t kCallbackOpsAddr = 0x60000;
  constexpr uint64_t kInvokeAddr = 0x70000;

  auto func = CreateMockDispatcherFunction("driver_runtime::Dispatcher::DispatchCallback",
                                           kDispatcherAddr, disp_coll_);
  Location frame_loc(0x1000, FileLine(), 0, SymbolContext::ForRelativeAddresses(), func);

  auto lambda_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType);
  auto tparam = fxl::MakeRefCounted<TemplateParameter>("Callable", lambda_coll, false);
  auto invoke_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  invoke_func->set_assigned_name(
      "fit::internal::target<(lambda at ../../src/channel_driver.cc:99:12), true, false, "
      "void>::invoke");
  invoke_func->set_template_params({LazySymbol(tparam)});
  Location invoke_loc(kInvokeAddr, FileLine("internal/function.h", 114), 0,
                      SymbolContext::ForRelativeAddresses(), invoke_func);
  mock_mod_->AddSymbolLocations(kInvokeAddr, {invoke_loc});

  MockFrame frame(&session(), thread_, frame_loc, 0x8000);
  auto* mem_provider = frame.GetMockSymbolDataProvider();
  SetupMockDispatcher(mem_provider, kDispatcherAddr, "direct_cb_driver", 16,
                      /*use_odd_sentinel=*/true, /*callback_queue_head=*/std::nullopt,
                      /*delayed_tasks_head=*/std::nullopt,
                      /*registered_callbacks_head=*/kReqAddr);

  // CallbackRequest with no async_operation_ and callback_.ops_ pointing to kCallbackOpsAddr.
  std::vector<uint8_t> req_bytes(96, 0);
  uint64_t sentinel_next = kRegQueueAddr | 1ULL;
  std::memcpy(req_bytes.data() + 16, &sentinel_next, sizeof(uint64_t));
  std::memcpy(req_bytes.data() + 72, &kCallbackOpsAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kReqAddr, req_bytes);

  std::vector<uint8_t> ops_bytes(24, 0);
  std::memcpy(ops_bytes.data() + 16, &kInvokeAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kCallbackOpsAddr, ops_bytes);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(&frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());
  auto children = retrieved_tasks[0]->GetChildren();
  ASSERT_EQ(1u, children.size());
  auto task_children = children[0].get().GetChildren();
  ASSERT_EQ(1u, task_children.size());

  const auto& func_node = task_children[0].get();
  EXPECT_EQ("Waiting", func_node.GetState());
  EXPECT_EQ("{closure}", func_node.GetIdentifier().GetFullNameNoQual());
  EXPECT_EQ("../../src/channel_driver.cc", func_node.GetLocation().file_line().file());
  EXPECT_EQ(99, func_node.GetLocation().file_line().line());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksNonDispatcherThisFallsBackToLoop) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kRuntimeAddr = 0x14000;
  constexpr uint64_t kLoopAddr = 0x10000;
  constexpr uint64_t kWaiterAddr = 0x18000;
  constexpr uint64_t kDispatcherAddr = 0x20000;

  // Frame 0: fdf_testing::DriverRuntime::Run where "this" points to DriverRuntime, not Dispatcher.
  auto runtime_coll = fxl::MakeRefCounted<Collection>(DwarfTag::kClassType, "DriverRuntime");
  runtime_coll->set_byte_size(264);
  auto runtime_func =
      CreateMockDispatcherFunction("fdf_testing::DriverRuntime::Run", kRuntimeAddr, runtime_coll);
  Location frame0_loc(0x1100, FileLine("driver_runtime.cc", 30), 0,
                      SymbolContext::ForRelativeAddresses(), runtime_func);

  // Frame 1: async_loop_run with a valid loop pointer.
  auto uint64_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 8, "uint64_t");
  auto loop_ptr_type = fxl::MakeRefCounted<ModifiedType>(DwarfTag::kPointerType, uint64_type);
  auto loop_var = MakeVariableForTest("loop", loop_ptr_type,
                                      VariableLocation(MakeConstantAddressExpr(kLoopAddr)));
  auto loop_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  loop_func->set_assigned_name("async_loop_run");
  loop_func->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));
  loop_func->set_parameters({LazySymbol(loop_var)});
  Location frame1_loc(0x1200, FileLine("loop.c", 50), 0, SymbolContext::ForRelativeAddresses(),
                      loop_func);

  std::vector<std::unique_ptr<Frame>> stack_frames;
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame0_loc, 0x8000));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame1_loc, 0x8100));

  Frame* top_frame = stack_frames[0].get();
  auto* mem_provider0 = static_cast<MockFrame*>(stack_frames[0].get())->GetMockSymbolDataProvider();
  auto* mem_provider1 = static_cast<MockFrame*>(stack_frames[1].get())->GetMockSymbolDataProvider();

  // Populate mapped memory at kRuntimeAddr with a bogus name to ensure it is not mistaken for a
  // Dispatcher.
  SetupMockDispatcher(mem_provider0, kRuntimeAddr, "wrong_runtime_object", 20,
                      /*use_odd_sentinel=*/true);

  std::vector<uint8_t> loop_bytes(96, 0);
  std::memcpy(loop_bytes.data() + 72, &kWaiterAddr, sizeof(uint64_t));
  mem_provider1->AddMemory(kLoopAddr, loop_bytes);

  std::vector<uint8_t> waiter_bytes(32, 0);
  std::memcpy(waiter_bytes.data() + 24, &kDispatcherAddr, sizeof(uint64_t));
  mem_provider1->AddMemory(kWaiterAddr, waiter_bytes);

  SetupMockDispatcher(mem_provider1, kDispatcherAddr, "real_dispatcher", 15,
                      /*use_odd_sentinel=*/true);

  thread_->GetStack().SetFramesForTest(std::move(stack_frames), true);

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(top_frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());
  EXPECT_EQ("real_dispatcher", retrieved_tasks[0]->GetState());
  EXPECT_EQ(kDispatcherAddr, retrieved_tasks[0]->GetId());
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksAsyncLoopEmptyWaitList) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kLoopAddr = 0x10000;
  constexpr uint64_t kWaitListAddr = kLoopAddr + 72;
  constexpr uint64_t kFakeDispatcherAddr = 0x20000;

  auto uint64_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 8, "uint64_t");
  auto loop_ptr_type = fxl::MakeRefCounted<ModifiedType>(DwarfTag::kPointerType, uint64_type);
  auto loop_var = MakeVariableForTest("loop", loop_ptr_type,
                                      VariableLocation(MakeConstantAddressExpr(kLoopAddr)));

  auto loop_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  loop_func->set_assigned_name("async_loop_run");
  loop_func->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));
  loop_func->set_parameters({LazySymbol(loop_var)});
  Location frame0_loc(0x1100, FileLine("loop.c", 50), 0, SymbolContext::ForRelativeAddresses(),
                      loop_func);

  MockFrame loop_frame(&session(), thread_, frame0_loc, 0x8000);
  auto* mem_provider = loop_frame.GetMockSymbolDataProvider();

  // Mock an empty circular list_node_t where wait_list.prev and wait_list.next both point back to
  // kWaitListAddr (&loop->wait_list), and place kFakeDispatcherAddr at offset +24 from
  // kWaitListAddr so dereferencing kWaitListAddr as an EventWaiter would falsely succeed.
  std::vector<uint8_t> loop_bytes(104, 0);
  std::memcpy(loop_bytes.data() + 72, &kWaitListAddr, sizeof(uint64_t));
  std::memcpy(loop_bytes.data() + 80, &kWaitListAddr, sizeof(uint64_t));
  std::memcpy(loop_bytes.data() + 96, &kFakeDispatcherAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kLoopAddr, loop_bytes);

  SetupMockDispatcher(mem_provider, kFakeDispatcherAddr, "bogus_dispatcher", 16,
                      /*use_odd_sentinel=*/true);

  bool called = false;
  provider.GetTasks(&loop_frame,
                    [&](const Err& err, const std::vector<std::unique_ptr<AsyncTask>>& tasks) {
                      called = true;
                      EXPECT_TRUE(err.has_error());
                    });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
}

TEST_F(FdfDispatcherTaskProviderTest, GetTasksAsyncLoopEventWaiterFallback) {
  EnsureTestTargetInitialized();
  FdfDispatcherTaskProvider provider;

  constexpr uint64_t kLoopAddr = 0x10000;
  constexpr uint64_t kWaiterAddr = 0x18000;
  constexpr uint64_t kDispatcherAddr = 0x20000;

  auto uint64_type = fxl::MakeRefCounted<BaseType>(BaseType::kBaseTypeUnsigned, 8, "uint64_t");
  auto loop_ptr_type = fxl::MakeRefCounted<ModifiedType>(DwarfTag::kPointerType, uint64_type);
  auto loop_var = MakeVariableForTest("loop", loop_ptr_type,
                                      VariableLocation(MakeConstantAddressExpr(kLoopAddr)));

  auto loop_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  loop_func->set_assigned_name("async_loop_run");
  loop_func->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));
  loop_func->set_parameters({LazySymbol(loop_var)});
  Location frame0_loc(0x1100, FileLine("loop.c", 50), 0, SymbolContext::ForRelativeAddresses(),
                      loop_func);

  auto caller_func = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  caller_func->set_assigned_name("driver_runtime::ThreadPool::WorkerThread");
  Location frame1_loc(0x1500, FileLine("thread_pool.cc", 80), 0,
                      SymbolContext::ForRelativeAddresses(), caller_func);

  std::vector<std::unique_ptr<Frame>> stack_frames;
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame0_loc, 0x8000));
  stack_frames.push_back(std::make_unique<MockFrame>(&session(), thread_, frame1_loc, 0x8100));

  Frame* loop_frame = stack_frames[0].get();
  auto* mem_provider = static_cast<MockFrame*>(loop_frame)->GetMockSymbolDataProvider();

  // Mock async_loop at kLoopAddr with wait_list.prev pointing to kWaiterAddr.
  std::vector<uint8_t> loop_bytes(96, 0);
  std::memcpy(loop_bytes.data() + 72, &kWaiterAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kLoopAddr, loop_bytes);

  // Mock driver_runtime::EventWaiter at kWaiterAddr with dispatcher_ref_.ptr_
  // pointing to kDispatcherAddr.
  std::vector<uint8_t> waiter_bytes(32, 0);
  std::memcpy(waiter_bytes.data() + 24, &kDispatcherAddr, sizeof(uint64_t));
  mem_provider->AddMemory(kWaiterAddr, waiter_bytes);

  SetupMockDispatcher(mem_provider, kDispatcherAddr, "loop_fallback_driver", 20,
                      /*use_odd_sentinel=*/true);

  thread_->GetStack().SetFramesForTest(std::move(stack_frames), true);
  EXPECT_TRUE(provider.CanHandle(loop_frame));

  bool called = false;
  std::vector<std::unique_ptr<AsyncTask>> retrieved_tasks;
  provider.GetTasks(loop_frame, [&](const Err& err, std::vector<std::unique_ptr<AsyncTask>> tasks) {
    called = true;
    EXPECT_FALSE(err.has_error()) << err.msg();
    retrieved_tasks = std::move(tasks);
  });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(called);
  ASSERT_EQ(1u, retrieved_tasks.size());
  EXPECT_EQ("loop_fallback_driver", retrieved_tasks[0]->GetState());
  EXPECT_EQ(kDispatcherAddr, retrieved_tasks[0]->GetId());
}

}  // namespace

}  // namespace zxdb
