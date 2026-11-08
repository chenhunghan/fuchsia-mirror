// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/console/format_async_task.h"

#include <string>
#include <utility>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/async_task.h"
#include "src/developer/debug/zxdb/client/async_task_tree.h"
#include "src/developer/debug/zxdb/client/mock_async_task.h"
#include "src/developer/debug/zxdb/common/test_with_loop.h"
#include "src/developer/debug/zxdb/expr/mock_eval_context.h"
#include "src/developer/debug/zxdb/format/async_output_buffer_test_util.h"
#include "src/developer/debug/zxdb/symbols/identifier.h"
#include "src/developer/debug/zxdb/symbols/location.h"

namespace zxdb {

namespace {

const std::string kAwaiteeMarker = "└─ ";

class MockAsyncTaskTreeDelegate : public AsyncTaskTree::Delegate {
 public:
  void SyncAsyncTasks(fit::callback<void(const Err&, const Frame* const)> callback) override {
    // No-op for testing.
  }
};

class FormatAsyncTaskTest : public TestWithLoop {};

TEST_F(FormatAsyncTaskTest, Basic) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  // Test Function type
  MockAsyncTask task(1, AsyncTask::Type::kFunction, Identifier(std::string("my_func")), "Running");
  auto buffer = FormatAsyncTask(task, nullptr, options, eval_context, 0);
  EXPECT_EQ("my_func (Running)\n", LoopUntilAsyncOutputBufferComplete(buffer).AsString());

  // Test Scope type
  MockAsyncTask scope(2, AsyncTask::Type::kScope, Identifier(std::string("MyScope")), "Scope_A");
  buffer = FormatAsyncTask(scope, nullptr, options, eval_context, 0);
  EXPECT_EQ("MyScope(\"Scope_A\")\n", LoopUntilAsyncOutputBufferComplete(buffer).AsString());

  // Test with indent
  buffer = FormatAsyncTask(task, nullptr, options, eval_context, 3);
  EXPECT_EQ(kAwaiteeMarker + "my_func (Running)\n",
            LoopUntilAsyncOutputBufferComplete(buffer).AsString());
}

TEST_F(FormatAsyncTaskTest, Verbose) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;
  options.verbose = true;

  MockAsyncTask task(1, AsyncTask::Type::kFunction, Identifier(std::string("my_func")), "Running");

  std::vector<AsyncTask::NamedValue> values;
  values.push_back({.name = "var1", .value = ExprValue(123)});
  task.set_values(std::move(values));

  auto buffer = FormatAsyncTask(task, nullptr, options, eval_context, 0);
  // Indent for values is indent + 2.
  EXPECT_EQ("my_func (Running)\n  var1 = 123\n",
            LoopUntilAsyncOutputBufferComplete(buffer).AsString());
}

TEST_F(FormatAsyncTaskTest, EmptyTree) {
  MockAsyncTaskTreeDelegate delegate;
  AsyncTaskTree tree(&delegate);
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  auto buffer = FormatAsyncTaskTree(tree, nullptr, options, eval_context);
  EXPECT_EQ("No async tasks found.\n", LoopUntilAsyncOutputBufferComplete(buffer).AsString());
}

TEST_F(FormatAsyncTaskTest, NestedTree) {
  MockAsyncTaskTreeDelegate delegate;
  AsyncTaskTree tree(&delegate);
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  auto root_owned = std::make_unique<MockAsyncTask>(1, AsyncTask::Type::kFunction,
                                                    Identifier(std::string("root")), "Running");
  MockAsyncTask* root_raw = root_owned.get();

  auto child_owned = std::make_unique<MockAsyncTask>(2, AsyncTask::Type::kFunction,
                                                     Identifier(std::string("child")), "Pending");
  MockAsyncTask* child_raw = child_owned.get();

  root_raw->set_children({*child_raw});

  std::vector<std::unique_ptr<AsyncTask>> root_tasks;
  root_tasks.push_back(std::move(root_owned));
  // Note: child_owned must be kept alive, but SetTasks only takes root_tasks.
  // In this test, child_owned will stay alive because it's in this scope.

  tree.SetTasks(std::move(root_tasks));

  auto buffer = FormatAsyncTaskTree(tree, nullptr, options, eval_context);
  EXPECT_EQ("root (Running)\n" + kAwaiteeMarker + "child (Pending)\n",
            LoopUntilAsyncOutputBufferComplete(buffer).AsString());
}

TEST_F(FormatAsyncTaskTest, FdfDispatcherTaskTree) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  // Dispatcher root node (kScope).
  Identifier dispatcher_ident;
  dispatcher_ident.AppendComponent(IdentifierComponent("fdf"));
  dispatcher_ident.AppendComponent(IdentifierComponent("Dispatcher"));
  MockAsyncTask dispatcher_scope(0x1000, AsyncTask::Type::kScope, dispatcher_ident,
                                 "driver_dispatcher");

  auto scope_buf = FormatAsyncTask(dispatcher_scope, nullptr, options, eval_context, 0);
  EXPECT_EQ("fdf::Dispatcher(\"driver_dispatcher\")\n",
            LoopUntilAsyncOutputBufferComplete(scope_buf).AsString());

  // Active running task (kTask) + child function with C++ identifier and FileLine.
  MockAsyncTask task1(0x2000, AsyncTask::Type::kTask, Identifier(IdentifierComponent("Task")),
                      "id = 0x2000");

  Identifier work_ident;
  work_ident.AppendComponent(IdentifierComponent("my_driver"));
  work_ident.AppendComponent(IdentifierComponent("MyClass"));
  work_ident.AppendComponent(IdentifierComponent("OnWork"));
  Location work_loc(0x5000, FileLine("my_driver.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  MockAsyncTask work_fn(0, AsyncTask::Type::kFunction, work_ident, "Running", work_loc);

  auto task_buf = FormatAsyncTask(task1, nullptr, options, eval_context, 3);
  EXPECT_EQ(kAwaiteeMarker + "Task(id = 0x2000)\n",
            LoopUntilAsyncOutputBufferComplete(task_buf).AsString());

  auto fn_buf = FormatAsyncTask(work_fn, nullptr, options, eval_context, 6);
  EXPECT_EQ("   " + kAwaiteeMarker + "my_driver::MyClass::OnWork (Running) • my_driver.cc:42\n",
            LoopUntilAsyncOutputBufferComplete(fn_buf).AsString());

  // Delayed task with C++ lambda identifier, FileLine, and verbose NamedValue.
  MockAsyncTask task2(0x3000, AsyncTask::Type::kTask, Identifier(IdentifierComponent("Task")),
                      "id = 0x3000");

  Identifier lambda_ident;
  lambda_ident.AppendComponent(IdentifierComponent("my_driver"));
  lambda_ident.AppendComponent(IdentifierComponent("Start"));
  lambda_ident.AppendComponent(IdentifierComponent("{closure}"));
  Location lambda_loc(0x6000, FileLine("my_driver.cc", 88), 0,
                      SymbolContext::ForRelativeAddresses());
  MockAsyncTask lambda_fn(0, AsyncTask::Type::kFunction, lambda_ident, "Delayed", lambda_loc);

  std::vector<AsyncTask::NamedValue> captures;
  captures.push_back({.name = "deadline", .value = ExprValue(static_cast<uint64_t>(1000000))});
  captures.push_back({.name = "retry_count", .value = ExprValue(3)});
  lambda_fn.set_values(std::move(captures));

  FormatTaskOptions verbose_options = options;
  verbose_options.verbose = true;

  auto lambda_buf = FormatAsyncTask(lambda_fn, nullptr, verbose_options, eval_context, 6);
  EXPECT_EQ("   " + kAwaiteeMarker +
                "my_driver::Start::λ (Delayed) • my_driver.cc:88\n"
                "        deadline = 1000000\n"
                "        retry_count = 3\n",
            LoopUntilAsyncOutputBufferComplete(lambda_buf).AsString());

  // Construct complete AsyncTaskTree and format whole tree.
  MockAsyncTaskTreeDelegate delegate;
  AsyncTaskTree tree(&delegate);

  auto scope_owned = std::make_unique<MockAsyncTask>(0x1000, AsyncTask::Type::kScope,
                                                     dispatcher_ident, "driver_dispatcher");
  auto task1_owned = std::make_unique<MockAsyncTask>(
      0x2000, AsyncTask::Type::kTask, Identifier(IdentifierComponent("Task")), "id = 0x2000");
  auto work_fn_owned = std::make_unique<MockAsyncTask>(0, AsyncTask::Type::kFunction, work_ident,
                                                       "Running", work_loc);
  auto task2_owned = std::make_unique<MockAsyncTask>(
      0x3000, AsyncTask::Type::kTask, Identifier(IdentifierComponent("Task")), "id = 0x3000");
  auto lambda_fn_owned = std::make_unique<MockAsyncTask>(0, AsyncTask::Type::kFunction,
                                                         lambda_ident, "Delayed", lambda_loc);

  task1_owned->set_children({*work_fn_owned});
  task2_owned->set_children({*lambda_fn_owned});
  scope_owned->set_children({*task1_owned, *task2_owned});

  std::vector<std::unique_ptr<AsyncTask>> root_tasks;
  root_tasks.push_back(std::move(scope_owned));
  tree.SetTasks(std::move(root_tasks));

  auto tree_buf = FormatAsyncTaskTree(tree, nullptr, options, eval_context);
  std::string expected = "fdf::Dispatcher(\"driver_dispatcher\")\n" + kAwaiteeMarker +
                         "Task(id = 0x2000)\n" + "   " + kAwaiteeMarker +
                         "my_driver::MyClass::OnWork (Running) • my_driver.cc:42\n" +
                         kAwaiteeMarker + "Task(id = 0x3000)\n" + "   " + kAwaiteeMarker +
                         "my_driver::Start::λ (Delayed) • my_driver.cc:88\n";
  EXPECT_EQ(expected, LoopUntilAsyncOutputBufferComplete(tree_buf).AsString());
}

TEST_F(FormatAsyncTaskTest, EmptyScope) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  Identifier dispatcher_ident;
  dispatcher_ident.AppendComponent(IdentifierComponent("fdf"));
  dispatcher_ident.AppendComponent(IdentifierComponent("Dispatcher"));
  MockAsyncTask empty_scope(0x1000, AsyncTask::Type::kScope, dispatcher_ident, "");

  auto scope_buf = FormatAsyncTask(empty_scope, nullptr, options, eval_context, 0);
  EXPECT_EQ("fdf::Dispatcher\n", LoopUntilAsyncOutputBufferComplete(scope_buf).AsString());

  MockAsyncTaskTreeDelegate delegate;
  AsyncTaskTree tree(&delegate);
  auto empty_scope_owned =
      std::make_unique<MockAsyncTask>(0x1000, AsyncTask::Type::kScope, dispatcher_ident, "");
  std::vector<std::unique_ptr<AsyncTask>> root_tasks;
  root_tasks.push_back(std::move(empty_scope_owned));
  tree.SetTasks(std::move(root_tasks));

  auto tree_buf = FormatAsyncTaskTree(tree, nullptr, options, eval_context);
  EXPECT_EQ("fdf::Dispatcher\n", LoopUntilAsyncOutputBufferComplete(tree_buf).AsString());
}

TEST_F(FormatAsyncTaskTest, FunctionWithEmptyIdentifierAndMalformedValues) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  MockAsyncTask empty_fn(0, AsyncTask::Type::kFunction, Identifier(), "Pending", Location());

  std::vector<AsyncTask::NamedValue> malformed_values;
  malformed_values.push_back({.name = std::nullopt, .value = ExprValue()});
  malformed_values.push_back({.name = "uninitialized", .value = ExprValue()});
  malformed_values.push_back({.name = std::nullopt, .value = ExprValue(42)});
  empty_fn.set_values(std::move(malformed_values));

  options.verbose = false;
  auto non_verbose_buf = FormatAsyncTask(empty_fn, nullptr, options, eval_context, 0);
  std::string non_verbose_str = LoopUntilAsyncOutputBufferComplete(non_verbose_buf).AsString();
  EXPECT_FALSE(non_verbose_str.empty());
  EXPECT_NE(non_verbose_str.find("(Pending)"), std::string::npos);

  options.verbose = true;
  auto verbose_buf = FormatAsyncTask(empty_fn, nullptr, options, eval_context, 0);
  std::string verbose_str = LoopUntilAsyncOutputBufferComplete(verbose_buf).AsString();
  EXPECT_FALSE(verbose_str.empty());
  EXPECT_NE(verbose_str.find("(Pending)"), std::string::npos);
}

TEST_F(FormatAsyncTaskTest, DeeplyNestedFunctionCallChain) {
  auto eval_context = fxl::MakeRefCounted<MockEvalContext>();
  FormatTaskOptions options;

  MockAsyncTaskTreeDelegate delegate;
  AsyncTaskTree tree(&delegate);

  auto task_owned = std::make_unique<MockAsyncTask>(
      0x4000, AsyncTask::Type::kTask, Identifier(IdentifierComponent("Task")), "id = 0x4000");

  Identifier frame2_ident;
  frame2_ident.AppendComponent(IdentifierComponent("my_driver"));
  frame2_ident.AppendComponent(IdentifierComponent("DispatchCallback"));
  Location loc2(0x1000, FileLine("driver.cc", 100), 0, SymbolContext::ForRelativeAddresses());
  auto frame2_owned =
      std::make_unique<MockAsyncTask>(0, AsyncTask::Type::kFunction, frame2_ident, "Running", loc2);

  Identifier frame1_ident;
  frame1_ident.AppendComponent(IdentifierComponent("my_driver"));
  frame1_ident.AppendComponent(IdentifierComponent("ProcessMessage"));
  Location loc1(0x2000, FileLine("driver.cc", 120), 0, SymbolContext::ForRelativeAddresses());
  auto frame1_owned =
      std::make_unique<MockAsyncTask>(0, AsyncTask::Type::kFunction, frame1_ident, "Running", loc1);

  Identifier frame0_ident;
  frame0_ident.AppendComponent(IdentifierComponent("my_driver"));
  frame0_ident.AppendComponent(IdentifierComponent("HandlePacket"));
  Location loc0(0x3000, FileLine("driver.cc", 140), 0, SymbolContext::ForRelativeAddresses());
  auto frame0_owned =
      std::make_unique<MockAsyncTask>(0, AsyncTask::Type::kFunction, frame0_ident, "Running", loc0);

  frame1_owned->set_children({*frame0_owned});
  frame2_owned->set_children({*frame1_owned});
  task_owned->set_children({*frame2_owned});

  std::vector<std::unique_ptr<AsyncTask>> root_tasks;
  root_tasks.push_back(std::move(task_owned));
  tree.SetTasks(std::move(root_tasks));

  auto tree_buf = FormatAsyncTaskTree(tree, nullptr, options, eval_context);
  std::string expected = "Task(id = 0x4000)\n" + kAwaiteeMarker +
                         "my_driver::DispatchCallback (Running) • driver.cc:100\n" + "   " +
                         kAwaiteeMarker + "my_driver::ProcessMessage (Running) • driver.cc:120\n" +
                         "      " + kAwaiteeMarker +
                         "my_driver::HandlePacket (Running) • driver.cc:140\n";
  EXPECT_EQ(expected, LoopUntilAsyncOutputBufferComplete(tree_buf).AsString());
}

}  // namespace

}  // namespace zxdb
