// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_MOCK_ASYNC_TASK_H_
#define SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_MOCK_ASYNC_TASK_H_

#include <lib/fit/function.h>

#include <cstdint>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "src/developer/debug/zxdb/client/async_task.h"
#include "src/developer/debug/zxdb/client/async_task_provider.h"
#include "src/developer/debug/zxdb/client/frame.h"
#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/symbols/identifier.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/symbol_context.h"

namespace zxdb {

// Mock implementation of AsyncTask supporting both console and debug_adapter unit tests.
class MockAsyncTask : public AsyncTask {
 public:
  // Constructor specifying session, id, name, and optional location.
  MockAsyncTask(Session* session, uint64_t id, std::string name, Location loc = Location())
      : AsyncTask(session),
        id_(id),
        type_(Type::kFuture),
        identifier_(IdentifierComponent(std::move(name))),
        state_("Pending"),
        location_(std::move(loc)) {}

  // Convenience constructor without session.
  MockAsyncTask(uint64_t id, std::string name, Location loc = Location())
      : MockAsyncTask(nullptr, id, std::move(name), std::move(loc)) {}

  // Detailed constructor specifying id, type, identifier, state, optional location, and session.
  MockAsyncTask(uint64_t id, Type type, Identifier identifier, std::string state = "Pending",
                Location loc = Location(), Session* session = nullptr)
      : AsyncTask(session),
        id_(id),
        type_(type),
        identifier_(std::move(identifier)),
        state_(std::move(state)),
        location_(std::move(loc)) {}

  uint64_t GetId() const override { return id_; }
  Type GetType() const override { return type_; }
  const Location& GetLocation() const override { return location_; }
  const Identifier& GetIdentifier() const override { return identifier_; }
  std::string GetState() const override { return state_; }
  const std::vector<NamedValue>& GetValues() const override { return values_; }

  std::vector<Ref> GetChildren() const override {
    std::vector<Ref> refs = children_;
    for (const auto& child : owned_children_) {
      refs.push_back(*child);
    }
    return refs;
  }

  void set_id(uint64_t id) { id_ = id; }
  void set_type(Type type) { type_ = type; }
  void set_location(const Location& loc) { location_ = loc; }
  void set_identifier(Identifier identifier) { identifier_ = std::move(identifier); }
  void set_name(std::string name) {
    identifier_ = Identifier(IdentifierComponent(std::move(name)));
  }
  void set_state(std::string state) { state_ = std::move(state); }
  void set_values(std::vector<NamedValue> values) { values_ = std::move(values); }
  void set_children(std::vector<Ref> children) { children_ = std::move(children); }

  void AddChild(std::unique_ptr<MockAsyncTask> child) {
    owned_children_.push_back(std::move(child));
  }

 private:
  uint64_t id_ = 0;
  Type type_ = Type::kFuture;
  Identifier identifier_;
  std::string state_;
  Location location_;
  std::vector<NamedValue> values_;
  std::vector<Ref> children_;
  std::vector<std::unique_ptr<MockAsyncTask>> owned_children_;
};

// Mock implementation of AsyncTaskProvider that produces a mock hierarchy of async tasks.
class MockAsyncTaskProvider : public AsyncTaskProvider {
 public:
  explicit MockAsyncTaskProvider(std::string fake_file_path = "")
      : fake_file_path_(std::move(fake_file_path)) {}

  bool CanHandle(Frame* /*frame*/) const override { return true; }

  void GetTasks(
      Frame* frame,
      fit::callback<void(const Err&, std::vector<std::unique_ptr<AsyncTask>>)> cb) override {
    std::vector<std::unique_ptr<AsyncTask>> tasks;
    Location loc(0x1234, FileLine(fake_file_path_, 42), 0, SymbolContext::ForRelativeAddresses());
    auto root = std::make_unique<MockAsyncTask>(frame->session(), 1, "root", std::move(loc));
    auto child = std::make_unique<MockAsyncTask>(frame->session(), 2, "child");
    child->AddChild(std::make_unique<MockAsyncTask>(frame->session(), 3, "grandchild"));
    root->AddChild(std::move(child));
    tasks.push_back(std::move(root));
    auto zero_id_task = std::make_unique<MockAsyncTask>(frame->session(), 0, "zero_id_task");
    tasks.push_back(std::move(zero_id_task));
    cb(Err(), std::move(tasks));
  }

  void set_fake_file_path(std::string fake_file_path) {
    fake_file_path_ = std::move(fake_file_path);
  }
  const std::string& fake_file_path() const { return fake_file_path_; }

 private:
  std::string fake_file_path_;
};

}  // namespace zxdb

#endif  // SRC_DEVELOPER_DEBUG_ZXDB_CLIENT_MOCK_ASYNC_TASK_H_
