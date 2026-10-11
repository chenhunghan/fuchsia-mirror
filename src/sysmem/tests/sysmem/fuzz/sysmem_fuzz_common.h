// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_SYSMEM_TESTS_SYSMEM_FUZZ_SYSMEM_FUZZ_COMMON_H_
#define SRC_SYSMEM_TESTS_SYSMEM_FUZZ_SYSMEM_FUZZ_COMMON_H_

#include <fidl/fuchsia.sysmem/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/result.h>

#include <memory>

#include "src/sysmem/server/allocator.h"
#include "src/sysmem/server/sysmem.h"

class MockSysmem {
 public:
  static std::unique_ptr<MockSysmem> Create() { return std::make_unique<MockSysmem>(); }

  MockSysmem() : loop_(&kAsyncLoopConfigNeverAttachToThread) {
    ZX_ASSERT(loop_.StartThread("MockSysmem") == ZX_OK);
    libsync::Completion done;
    ZX_ASSERT(async::PostTask(loop_.dispatcher(), [this, &done] {
                sysmem_service::Sysmem::CreateArgs create_args;
                auto result = sysmem_service::Sysmem::Create(loop_.dispatcher(), create_args);
                ZX_ASSERT(result.is_ok());
                sysmem_ = std::move(result.value());
                done.Signal();
              }) == ZX_OK);
    done.Wait();
  }

  ~MockSysmem() {
    libsync::Completion done;
    ZX_ASSERT(async::PostTask(loop_.dispatcher(), [this, &done] {
                sysmem_.reset();
                done.Signal();
              }) == ZX_OK);
    done.Wait();
  }

  zx::result<fidl::ClientEnd<fuchsia_sysmem::Allocator>> ConnectAllocator() {
    auto [client, server] = fidl::Endpoints<fuchsia_sysmem::Allocator>::Create();
    sysmem_->SyncCall([this, server = std::move(server)] mutable {
      sysmem_service::Allocator::CreateOwnedV1(std::move(server), sysmem_.get(),
                                               sysmem_->v1_allocators());
    });
    return zx::ok(std::move(client));
  }

 private:
  async::Loop loop_;
  std::unique_ptr<sysmem_service::Sysmem> sysmem_;
};

#endif  // SRC_SYSMEM_TESTS_SYSMEM_FUZZ_SYSMEM_FUZZ_COMMON_H_
