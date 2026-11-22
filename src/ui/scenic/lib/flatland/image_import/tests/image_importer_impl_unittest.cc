// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: ../DESIGN.md. A change that contradicts a rule there amends the rule
// in the same CL, with the reason (../DESIGN.md#changing).

#include "src/ui/scenic/lib/flatland/image_import/image_importer_impl.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.images2/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/time.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/clock.h>
#include <lib/zx/event.h>
#include <lib/zx/thread.h>
#include <lib/zx/time.h>
#include <zircon/syscalls.h>
#include <zircon/syscalls/object.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <functional>
#include <memory>
#include <memory_resource>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/fsl/handles/object_info.h"
#include "src/lib/fxl/synchronization/thread_annotations.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/display/coordinator_proxy/coordinator_proxy.h"
#include "src/ui/scenic/lib/display/tests/mock_display_coordinator.h"
#include "src/ui/scenic/lib/flatland/image_import/testing/image_import_test_peer.h"
#include "src/ui/scenic/lib/flatland/renderer/renderer.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/dispatcher_holder.h"
#include "src/ui/scenic/lib/utils/pmr_stack_arena.h"

namespace flatland {

class ImageImporterImplTestPeer {
 public:
  static void SetPostDisplayDecrementHook(ImageImporterImpl& importer, std::function<void()> hook) {
    std::scoped_lock lock(importer.pending_signal_mutex_);
    importer.post_display_decrement_hook_for_test_ = std::move(hook);
  }

  static void SetStopDrainEnteredHook(ImageImporterImpl& importer, std::function<void()> hook) {
    importer.stop_drain_entered_hook_for_test_ = std::move(hook);
  }

  static size_t LiveImagesCount(ImageImporterImpl& importer) {
    std::scoped_lock lock(importer.collections_mutex_);
    return importer.live_images_by_id_.size();
  }

  static bool GpuJobsDeferred(ImageImporterImpl& importer) {
    std::scoped_lock lock(importer.queue_mutex_);
    return importer.gpu_jobs_deferred_;
  }

  static zx::duration DisplayImportDurationPrediction(ImageImporterImpl& importer) {
    std::scoped_lock lock(importer.queue_mutex_);
    return importer.display_import_duration_.GetPrediction();
  }

  static zx::duration FirstGpuImportDurationPrediction(ImageImporterImpl& importer) {
    std::scoped_lock lock(importer.queue_mutex_);
    return importer.first_gpu_import_duration_.GetPrediction();
  }

  static zx::duration LaterGpuImportDurationPrediction(ImageImporterImpl& importer) {
    std::scoped_lock lock(importer.queue_mutex_);
    return importer.later_gpu_import_duration_.GetPrediction();
  }

  static bool PredictedNextKindIsGpu(const ImageImporterImpl& importer) {
    return importer.PredictedNextKind() == ImageImporterImpl::JobKind::kGpu;
  }

  static std::unique_lock<std::mutex> LockQueueMutex(ImageImporterImpl& importer)
      FXL_NO_THREAD_SAFETY_ANALYSIS {
    return std::unique_lock<std::mutex>(importer.queue_mutex_);
  }

  static uint64_t ReplyChannelReadableCalls(const ImageImporterImpl& importer) {
    return importer.reply_channel_readable_calls_for_test_;
  }
};

namespace {

enum class TokenBranch : uint8_t {
  kRoot,
  kGroupChild0,
  kGroupChild1,
};

struct CollectionSpec {
  bool auto_allocate = true;
  bool allocation_succeeds = true;
  bool display_branch_succeeds = true;
  uint32_t buffer_count = 4;
  uint64_t per_buffer_bytes = 8192;
  fuchsia_images2::PixelFormatModifier modifier = fuchsia_images2::PixelFormatModifier::kLinear;
  fuchsia_math::SizeU min_size{1, 1};
  fuchsia_math::SizeU max_size{4096, 4096};
};

// In-process `fuchsia.sysmem2` test server running on its own loop thread so
// synchronous `DuplicateSync()` and `CreateChildrenSync()` calls from main work
// without external sysmem dependencies.
class FakeSysmem final {
 public:
  explicit FakeSysmem(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {}

  fidl::WireClient<fuchsia_sysmem2::Allocator> ConnectAllocator(
      async_dispatcher_t* client_dispatcher) {
    auto [client_end, server_end] = fidl::Endpoints<fuchsia_sysmem2::Allocator>::Create();
    libsync::Completion bound;
    async::PostTask(dispatcher_, [this, server_end = std::move(server_end), &bound]() mutable {
      allocator_bindings_.AddBinding(dispatcher_, std::move(server_end), &allocator_server_,
                                     fidl::kIgnoreBindingClosure);
      bound.Signal();
    });
    bound.Wait();
    return fidl::WireClient<fuchsia_sysmem2::Allocator>(std::move(client_end), client_dispatcher);
  }

  std::pair<uint64_t, fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>> CreateRootToken(
      CollectionSpec spec = {}) {
    auto [client_end, server_end] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    const zx_koid_t server_koid = fsl::GetKoid(server_end.channel().get());

    uint64_t index = 0;
    {
      std::scoped_lock lock(mutex_);
      index = ++next_index_;
      auto state = std::make_shared<CollectionState>();
      state->spec = spec;
      collections_.emplace(index, std::move(state));
      token_routes_.emplace(server_koid, TokenRoute{.index = index, .branch = TokenBranch::kRoot});
    }

    libsync::Completion bound;
    async::PostTask(dispatcher_,
                    [this, index, server_end = std::move(server_end), &bound]() mutable {
                      BindTokenOnServerThread(index, TokenBranch::kRoot, std::move(server_end));
                      bound.Signal();
                    });
    bound.Wait();
    return {index, std::move(client_end)};
  }

  void CompleteAllocation(uint64_t index) {
    libsync::Completion done;
    async::PostTask(dispatcher_, [this, index, &done] {
      std::vector<PendingWait> waits;
      CollectionSpec spec;
      {
        std::scoped_lock lock(mutex_);
        auto it = collections_.find(index);
        if (it != collections_.end()) {
          it->second->spec.auto_allocate = true;
          spec = it->second->spec;
          waits = std::move(it->second->pending_waits);
          it->second->pending_waits.clear();
        }
      }
      for (auto& w : waits) {
        ReplyWaitOnServerThread(spec, w.branch, w.completer);
      }
      done.Signal();
    });
    done.Wait();
  }

  struct Stats {
    uint32_t token_groups_created = 0;
    uint32_t root_collections_bound = 0;
    uint32_t display_collections_bound = 0;
    uint32_t root_releases = 0;
    uint32_t display_releases = 0;
    uint32_t token_releases = 0;
  };

  void SetFailDuplicateSyncIndex(int index) {
    fail_duplicate_sync_index_ = index;
    duplicate_sync_count_ = 0;
  }
  void SetFailCreateChildrenSyncIndex(int index) {
    fail_create_children_sync_index_ = index;
    create_children_sync_count_ = 0;
  }
  void SetFailBindSharedCollectionIndex(int index) {
    fail_bind_shared_collection_index_ = index;
    bind_shared_collection_count_ = 0;
  }
  void ResetFailureInjections() {
    fail_duplicate_sync_index_ = -1;
    duplicate_sync_count_ = 0;
    fail_create_children_sync_index_ = -1;
    create_children_sync_count_ = 0;
    fail_bind_shared_collection_index_ = -1;
    bind_shared_collection_count_ = 0;
  }

  Stats GetStats(uint64_t index) const {
    std::scoped_lock lock(mutex_);
    auto it = collections_.find(index);
    if (it == collections_.end()) {
      return {};
    }
    return it->second->stats;
  }

 private:
  struct TokenRoute {
    uint64_t index = 0;
    TokenBranch branch = TokenBranch::kRoot;
  };

  struct PendingWait {
    TokenBranch branch = TokenBranch::kRoot;
    fidl::Server<fuchsia_sysmem2::BufferCollection>::WaitForAllBuffersAllocatedCompleter::Async
        completer;
  };

  struct CollectionState {
    CollectionSpec spec;
    Stats stats;
    std::vector<PendingWait> pending_waits;
  };

  static fuchsia_sysmem2::BufferCollectionInfo BuildCollectionInfo(const CollectionSpec& spec) {
    fuchsia_sysmem2::BufferCollectionInfo info;
    std::vector<fuchsia_sysmem2::VmoBuffer> buffers(spec.buffer_count);
    info.buffers(std::move(buffers));

    fuchsia_sysmem2::BufferMemorySettings mem_settings;
    mem_settings.size_bytes(spec.per_buffer_bytes);

    fuchsia_sysmem2::ImageFormatConstraints img_constraints;
    img_constraints.pixel_format(fuchsia_images2::PixelFormat::kR8G8B8A8);
    img_constraints.pixel_format_modifier(spec.modifier);
    img_constraints.min_size(spec.min_size);
    img_constraints.max_size(spec.max_size);

    fuchsia_sysmem2::SingleBufferSettings settings;
    settings.buffer_settings(std::move(mem_settings));
    settings.image_format_constraints(std::move(img_constraints));
    info.settings(std::move(settings));
    return info;
  }

  static void ReplyWaitOnServerThread(
      const CollectionSpec& spec, TokenBranch branch,
      fidl::Server<fuchsia_sysmem2::BufferCollection>::WaitForAllBuffersAllocatedCompleter::Async&
          completer) {
    const bool ok = spec.allocation_succeeds &&
                    (branch != TokenBranch::kGroupChild0 || spec.display_branch_succeeds);
    if (!ok) {
      completer.Reply(fit::error(fuchsia_sysmem2::Error::kNoMemory));
      return;
    }
    fuchsia_sysmem2::BufferCollectionWaitForAllBuffersAllocatedResponse resp;
    resp.buffer_collection_info(BuildCollectionInfo(spec));
    completer.Reply(fit::ok(std::move(resp)));
  }

  class TokenServer final : public fidl::testing::TestBase<fuchsia_sysmem2::BufferCollectionToken> {
   public:
    TokenServer(FakeSysmem& parent, uint64_t index, TokenBranch branch)
        : parent_(parent), index_(index), branch_(branch) {}

    void NotImplemented_(const std::string& /*name*/, fidl::CompleterBase& /*completer*/) override {
    }
    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_sysmem2::BufferCollectionToken> /*metadata*/,
        fidl::UnknownMethodCompleter::Sync& /*completer*/) override {}

    void Sync(SyncCompleter::Sync& completer) override { completer.Reply(); }

    void Release(ReleaseCompleter::Sync& completer) override {
      {
        std::scoped_lock lock(parent_.mutex_);
        if (auto it = parent_.collections_.find(index_); it != parent_.collections_.end()) {
          ++it->second->stats.token_releases;
        }
      }
      completer.Close(ZX_OK);
    }

    void DuplicateSync(DuplicateSyncRequest& request,
                       DuplicateSyncCompleter::Sync& completer) override {
      if (parent_.duplicate_sync_count_.fetch_add(1) == parent_.fail_duplicate_sync_index_.load()) {
        // Reply without `tokens` rather than closing the channel, so the importer's failure
        // path can still `Release()` this token and the test can observe it in `token_releases`.
        completer.Reply(fuchsia_sysmem2::BufferCollectionTokenDuplicateSyncResponse{});
        return;
      }
      const size_t count = request.rights_attenuation_masks().has_value()
                               ? request.rights_attenuation_masks()->size()
                               : 0;
      std::vector<fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>> tokens;
      tokens.reserve(count);
      for (size_t i = 0; i < count; ++i) {
        auto [client_end, server_end] =
            fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
        const zx_koid_t server_koid = fsl::GetKoid(server_end.channel().get());
        {
          std::scoped_lock lock(parent_.mutex_);
          parent_.token_routes_.emplace(server_koid,
                                        TokenRoute{.index = index_, .branch = branch_});
        }
        parent_.BindTokenOnServerThread(index_, branch_, std::move(server_end));
        tokens.push_back(std::move(client_end));
      }
      fuchsia_sysmem2::BufferCollectionTokenDuplicateSyncResponse resp;
      resp.tokens(std::move(tokens));
      completer.Reply(std::move(resp));
    }

    void CreateBufferCollectionTokenGroup(
        CreateBufferCollectionTokenGroupRequest& request,
        CreateBufferCollectionTokenGroupCompleter::Sync& /*completer*/) override {
      if (!request.group_request().has_value()) {
        return;
      }
      {
        std::scoped_lock lock(parent_.mutex_);
        if (auto it = parent_.collections_.find(index_); it != parent_.collections_.end()) {
          ++it->second->stats.token_groups_created;
        }
      }
      parent_.BindGroupOnServerThread(index_, std::move(request.group_request().value()));
    }

   private:
    FakeSysmem& parent_;
    const uint64_t index_;
    const TokenBranch branch_;
  };

  class GroupServer final
      : public fidl::testing::TestBase<fuchsia_sysmem2::BufferCollectionTokenGroup> {
   public:
    GroupServer(FakeSysmem& parent, uint64_t index) : parent_(parent), index_(index) {}

    void NotImplemented_(const std::string& /*name*/, fidl::CompleterBase& /*completer*/) override {
    }
    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_sysmem2::BufferCollectionTokenGroup> /*metadata*/,
        fidl::UnknownMethodCompleter::Sync& /*completer*/) override {}

    void Sync(SyncCompleter::Sync& completer) override { completer.Reply(); }
    void AllChildrenPresent(AllChildrenPresentCompleter::Sync& /*completer*/) override {}
    void Release(ReleaseCompleter::Sync& completer) override { completer.Close(ZX_OK); }

    void CreateChildrenSync(CreateChildrenSyncRequest& request,
                            CreateChildrenSyncCompleter::Sync& completer) override {
      if (parent_.create_children_sync_count_.fetch_add(1) ==
          parent_.fail_create_children_sync_index_.load()) {
        completer.Close(ZX_ERR_INTERNAL);
        return;
      }
      const size_t count = request.rights_attenuation_masks().has_value()
                               ? request.rights_attenuation_masks()->size()
                               : 0;
      std::vector<fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>> tokens;
      tokens.reserve(count);
      for (size_t i = 0; i < count; ++i) {
        const TokenBranch branch = (i == 0) ? TokenBranch::kGroupChild0 : TokenBranch::kGroupChild1;
        auto [client_end, server_end] =
            fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
        const zx_koid_t server_koid = fsl::GetKoid(server_end.channel().get());
        {
          std::scoped_lock lock(parent_.mutex_);
          parent_.token_routes_.emplace(server_koid, TokenRoute{.index = index_, .branch = branch});
        }
        parent_.BindTokenOnServerThread(index_, branch, std::move(server_end));
        tokens.push_back(std::move(client_end));
      }
      fuchsia_sysmem2::BufferCollectionTokenGroupCreateChildrenSyncResponse resp;
      resp.tokens(std::move(tokens));
      completer.Reply(std::move(resp));
    }

   private:
    FakeSysmem& parent_;
    const uint64_t index_;
  };

  class CollectionServer final : public fidl::testing::TestBase<fuchsia_sysmem2::BufferCollection> {
   public:
    CollectionServer(FakeSysmem& parent, uint64_t index, TokenBranch branch)
        : parent_(parent), index_(index), branch_(branch) {}

    void NotImplemented_(const std::string& /*name*/, fidl::CompleterBase& /*completer*/) override {
    }
    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_sysmem2::BufferCollection> /*metadata*/,
        fidl::UnknownMethodCompleter::Sync& /*completer*/) override {}

    void SetConstraints(SetConstraintsRequest& /*request*/,
                        SetConstraintsCompleter::Sync& /*completer*/) override {}

    void Release(ReleaseCompleter::Sync& completer) override {
      {
        std::scoped_lock lock(parent_.mutex_);
        if (auto it = parent_.collections_.find(index_); it != parent_.collections_.end()) {
          if (branch_ == TokenBranch::kGroupChild0) {
            ++it->second->stats.display_releases;
          } else {
            ++it->second->stats.root_releases;
          }
        }
      }
      completer.Close(ZX_OK);
    }

    void WaitForAllBuffersAllocated(WaitForAllBuffersAllocatedCompleter::Sync& completer) override {
      auto async_completer = completer.ToAsync();
      CollectionSpec spec;
      bool reply_now = false;
      {
        std::scoped_lock lock(parent_.mutex_);
        auto it = parent_.collections_.find(index_);
        if (it == parent_.collections_.end()) {
          async_completer.Reply(fit::error(fuchsia_sysmem2::Error::kProtocolDeviation));
          return;
        }
        spec = it->second->spec;
        if (spec.auto_allocate) {
          reply_now = true;
        } else {
          it->second->pending_waits.push_back(
              PendingWait{.branch = branch_, .completer = std::move(async_completer)});
        }
      }
      if (reply_now) {
        ReplyWaitOnServerThread(spec, branch_, async_completer);
      }
    }

   private:
    FakeSysmem& parent_;
    const uint64_t index_;
    const TokenBranch branch_;
  };

  class AllocatorServer final : public fidl::testing::TestBase<fuchsia_sysmem2::Allocator> {
   public:
    explicit AllocatorServer(FakeSysmem& parent) : parent_(parent) {}

    void NotImplemented_(const std::string& /*name*/, fidl::CompleterBase& /*completer*/) override {
    }
    void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_sysmem2::Allocator> /*metadata*/,
                               fidl::UnknownMethodCompleter::Sync& /*completer*/) override {}

    void BindSharedCollection(BindSharedCollectionRequest& request,
                              BindSharedCollectionCompleter::Sync& /*completer*/) override {
      if (!request.token().has_value() || !request.buffer_collection_request().has_value()) {
        return;
      }
      if (parent_.bind_shared_collection_count_.fetch_add(1) ==
          parent_.fail_bind_shared_collection_index_.load()) {
        return;
      }
      const zx_koid_t server_koid = fsl::GetRelatedKoid(request.token()->channel().get());
      TokenRoute route;
      {
        std::scoped_lock lock(parent_.mutex_);
        auto it = parent_.token_routes_.find(server_koid);
        if (it == parent_.token_routes_.end()) {
          return;
        }
        route = it->second;
        if (auto cit = parent_.collections_.find(route.index); cit != parent_.collections_.end()) {
          if (route.branch == TokenBranch::kGroupChild0) {
            ++cit->second->stats.display_collections_bound;
          } else {
            ++cit->second->stats.root_collections_bound;
          }
        }
      }
      parent_.BindCollectionOnServerThread(route.index, route.branch,
                                           std::move(request.buffer_collection_request().value()));
    }

   private:
    FakeSysmem& parent_;
  };

  void BindTokenOnServerThread(uint64_t index, TokenBranch branch,
                               fidl::ServerEnd<fuchsia_sysmem2::BufferCollectionToken> server_end) {
    auto impl = std::make_unique<TokenServer>(*this, index, branch);
    auto* raw = impl.get();
    token_servers_.push_back(std::move(impl));
    token_bindings_.AddBinding(dispatcher_, std::move(server_end), raw,
                               fidl::kIgnoreBindingClosure);
  }

  void BindGroupOnServerThread(
      uint64_t index, fidl::ServerEnd<fuchsia_sysmem2::BufferCollectionTokenGroup> server_end) {
    auto impl = std::make_unique<GroupServer>(*this, index);
    auto* raw = impl.get();
    group_servers_.push_back(std::move(impl));
    group_bindings_.AddBinding(dispatcher_, std::move(server_end), raw,
                               fidl::kIgnoreBindingClosure);
  }

  void BindCollectionOnServerThread(uint64_t index, TokenBranch branch,
                                    fidl::ServerEnd<fuchsia_sysmem2::BufferCollection> server_end) {
    auto impl = std::make_unique<CollectionServer>(*this, index, branch);
    auto* raw = impl.get();
    collection_servers_.push_back(std::move(impl));
    collection_bindings_.AddBinding(dispatcher_, std::move(server_end), raw,
                                    fidl::kIgnoreBindingClosure);
  }

  async_dispatcher_t* const dispatcher_;
  std::atomic<int> fail_duplicate_sync_index_{-1};
  std::atomic<int> duplicate_sync_count_{0};
  std::atomic<int> fail_create_children_sync_index_{-1};
  std::atomic<int> create_children_sync_count_{0};
  std::atomic<int> fail_bind_shared_collection_index_{-1};
  std::atomic<int> bind_shared_collection_count_{0};
  mutable std::mutex mutex_;
  uint64_t next_index_ FXL_GUARDED_BY(mutex_) = 0;
  std::unordered_map<uint64_t, std::shared_ptr<CollectionState>> collections_
      FXL_GUARDED_BY(mutex_);
  std::unordered_map<zx_koid_t, TokenRoute> token_routes_ FXL_GUARDED_BY(mutex_);

  AllocatorServer allocator_server_{*this};
  fidl::ServerBindingGroup<fuchsia_sysmem2::Allocator> allocator_bindings_;
  std::vector<std::unique_ptr<TokenServer>> token_servers_;
  fidl::ServerBindingGroup<fuchsia_sysmem2::BufferCollectionToken> token_bindings_;
  std::vector<std::unique_ptr<GroupServer>> group_servers_;
  fidl::ServerBindingGroup<fuchsia_sysmem2::BufferCollectionTokenGroup> group_bindings_;
  std::vector<std::unique_ptr<CollectionServer>> collection_servers_;
  fidl::ServerBindingGroup<fuchsia_sysmem2::BufferCollection> collection_bindings_;
};

// Controllable `Renderer` test double that verifies main-thread destruction of
// `GpuCollection` and `GpuImage` slots and supports gating or failing
// `CreateGpuImage()` on worker threads.
class ControllableRenderer final : public Renderer {
 public:
  class TrackedGpuCollection final : public GpuCollection {
   public:
    explicit TrackedGpuCollection(ControllableRenderer& owner) : owner_(owner) {
      owner_.live_gpu_collections_.fetch_add(1, std::memory_order_relaxed);
    }
    ~TrackedGpuCollection() override {
      if (!utils::IsOnMainThread()) {
        owner_.off_main_destructions_.fetch_add(1, std::memory_order_relaxed);
      }
      owner_.live_gpu_collections_.fetch_sub(1, std::memory_order_relaxed);
    }

   private:
    ControllableRenderer& owner_;
  };

  class TrackedGpuImage final : public GpuImage {
   public:
    TrackedGpuImage(ControllableRenderer& owner, allocation::GlobalImageId id, bool sampled)
        : owner_(owner), id_(id), sampled_(sampled) {
      owner_.live_gpu_images_.fetch_add(1, std::memory_order_relaxed);
    }
    ~TrackedGpuImage() override {
      // Only sampled slots must be destroyed on main (DESIGN.md#renderer-contract).
      if (!utils::IsOnMainThread()) {
        (sampled_ ? owner_.off_main_destructions_ : owner_.unsampled_off_main_destructions_)
            .fetch_add(1, std::memory_order_relaxed);
      }
      owner_.live_gpu_images_.fetch_sub(1, std::memory_order_relaxed);
    }

    bool WasSampled() const override { return sampled_; }
    allocation::GlobalImageId id() const { return id_; }

   private:
    ControllableRenderer& owner_;
    const allocation::GlobalImageId id_;
    const bool sampled_;
  };

  using CreateGpuImageHook = std::function<fit::result<ImportImageError, std::unique_ptr<GpuImage>>(
      ControllableRenderer&, const allocation::ImageMetadata&, uint64_t)>;

  void set_fail_create_collection(bool fail) { fail_create_collection_ = fail; }
  void set_default_sampled(bool sampled) { default_sampled_.store(sampled); }

  void SetCreateGpuImageHook(CreateGpuImageHook hook) {
    std::scoped_lock lock(mutex_);
    hook_ = std::move(hook);
  }

  std::vector<uint64_t> TakeCreatedImageOrder() {
    std::scoped_lock lock(mutex_);
    auto out = std::move(created_image_order_);
    created_image_order_.clear();
    return out;
  }

  uint32_t live_gpu_collections() const { return live_gpu_collections_.load(); }
  uint32_t live_gpu_images() const { return live_gpu_images_.load(); }
  // Collections or sampled images destroyed off main; must stay zero.
  uint32_t off_main_destructions() const { return off_main_destructions_.load(); }
  uint32_t unsampled_off_main_destructions() const {
    return unsampled_off_main_destructions_.load();
  }
  uint32_t create_gpu_image_calls() const { return create_gpu_image_calls_.load(); }

  fit::result<ImportImageError, std::unique_ptr<GpuCollection>> CreateGpuCollection(
      allocation::GlobalBufferCollectionId /*collection_id*/,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) override {
    EXPECT_TRUE(utils::IsOnMainThread());
    if (token.is_valid()) {
      auto st = fidl::WireCall(token)->Release();
      (void)st;
    }
    if (fail_create_collection_) {
      return fit::error(ImportImageError::kCollectionFailed);
    }
    return fit::ok(std::make_unique<TrackedGpuCollection>(*this));
  }

  fit::result<ImportImageError, std::unique_ptr<GpuImage>> CreateGpuImage(
      const allocation::ImageMetadata& metadata, GpuCollection& /*collection*/,
      uint64_t per_buffer_bytes) override {
    create_gpu_image_calls_.fetch_add(1, std::memory_order_relaxed);
    CreateGpuImageHook hook;
    {
      std::scoped_lock lock(mutex_);
      created_image_order_.push_back(metadata.identifier.value());
      hook = hook_;
    }
    if (hook) {
      return hook(*this, metadata, per_buffer_bytes);
    }
    return MakeTrackedImage(metadata.identifier);
  }

  fit::result<ImportImageError, std::unique_ptr<GpuImage>> MakeTrackedImage(
      allocation::GlobalImageId id) {
    return fit::ok(std::make_unique<TrackedGpuImage>(*this, id, default_sampled_.load()));
  }

  // Unused `Renderer` / `BufferCollectionImporter` methods:
  fpromise::promise<> ImportBufferCollection(
      allocation::GlobalBufferCollectionId, fidl::WireClient<fuchsia_sysmem2::Allocator>&,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>, allocation::BufferCollectionUsage,
      std::optional<fuchsia_math::SizeU>) override {
    return fpromise::make_ok_promise();
  }
  void ReleaseBufferCollection(allocation::GlobalBufferCollectionId,
                               allocation::BufferCollectionUsage) override {}
  fpromise::promise<> ImportBufferImage(const allocation::ImageMetadata&,
                                        allocation::BufferCollectionUsage) override {
    return fpromise::make_ok_promise();
  }
  void ReleaseBufferImage(allocation::GlobalImageId) override {}
  void Render(const allocation::ImageMetadata&, std::span<const ResolvedLayer>,
              const RenderArgs&) override {}
  void SetColorConversionValues(const fidl::Array<float, 9>&, const fidl::Array<float, 3>&,
                                const fidl::Array<float, 3>&) override {}
  fuchsia_images2::PixelFormat ChoosePreferredRenderTargetFormat(
      const std::vector<fuchsia_images2::PixelFormat>&) const override {
    return fuchsia_images2::PixelFormat::kR8G8B8A8;
  }
  bool SupportsRenderInProtected() const override { return false; }
  bool RequiresRenderInProtected(std::span<const ResolvedLayer>) const override { return false; }

 private:
  bool fail_create_collection_ = false;
  std::atomic<bool> default_sampled_{false};
  std::atomic<uint32_t> live_gpu_collections_{0};
  std::atomic<uint32_t> live_gpu_images_{0};
  std::atomic<uint32_t> off_main_destructions_{0};
  std::atomic<uint32_t> unsampled_off_main_destructions_{0};
  std::atomic<uint32_t> create_gpu_image_calls_{0};
  std::mutex mutex_;
  std::vector<uint64_t> created_image_order_ FXL_GUARDED_BY(mutex_);
  CreateGpuImageHook hook_ FXL_GUARDED_BY(mutex_);
};

class ImageImporterImplTest : public gtest::RealLoopFixture {
 protected:
  static constexpr std::array<display::WireDisplayMode, 1> kDisplayModes = {
      fuchsia_hardware_display_types::wire::Mode{
          .active_area = {.width = 1080, .height = 2400},
          .refresh_rate_millihertz = 60000,
          .flags = {},
      },
  };

  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    main_setter_.emplace(dispatcher(), dispatcher());
    reply_holder_ = std::make_shared<utils::UnownedDispatcherHolder>(dispatcher());

    sysmem_loop_.StartThread("fake-sysmem-loop");
    fake_sysmem_ = std::make_unique<FakeSysmem>(sysmem_loop_.dispatcher());
    sysmem_allocator_ = fake_sysmem_->ConnectAllocator(dispatcher());

    renderer_ = std::make_shared<ControllableRenderer>();
  }

  void TearDown() override {
    if (importer_) {
      importer_->StopOnMain();
      importer_.reset();
    }
    coordinator_.reset();

    if (reply_loop_) {
      reply_loop_->Quit();
      reply_loop_->JoinThreads();
      reply_loop_.reset();
    }

    // Shut down `coord_loop_` before destroying `mock_coordinator_` so `AsyncServerBinding`
    // teardown completes while the server object is still alive.
    if (coord_loop_) {
      coord_loop_->Shutdown();
      coord_loop_.reset();
    }
    mock_coordinator_.reset();

    sysmem_allocator_ = {};
    if (fake_sysmem_) {
      libsync::Completion destroyed;
      async::PostTask(sysmem_loop_.dispatcher(), [this, &destroyed] {
        fake_sysmem_.reset();
        destroyed.Signal();
      });
      destroyed.Wait();
    }
    sysmem_loop_.Quit();
    sysmem_loop_.JoinThreads();

    EXPECT_EQ(renderer_->live_gpu_collections(), 0u);
    EXPECT_EQ(renderer_->live_gpu_images(), 0u);
    EXPECT_EQ(renderer_->off_main_destructions(), 0u);
    main_setter_.reset();
    gtest::RealLoopFixture::TearDown();
  }

  void CreateImporter(ImageImporterImpl::Config config = {}, std::string raw_policy = "") {
    if (importer_) {
      importer_->StopOnMain();
      importer_.reset();
    }
    coordinator_.reset();
    if (reply_loop_) {
      reply_loop_->Quit();
      reply_loop_->JoinThreads();
      reply_loop_.reset();
    }
    if (coord_loop_) {
      coord_loop_->Shutdown();
      coord_loop_.reset();
    }
    mock_coordinator_.reset();

    reply_loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);

    if (config.enable_direct_to_display) {
      coord_loop_ = std::make_unique<async::Loop>(&kAsyncLoopConfigNoAttachToCurrentThread);
      coord_loop_->StartThread("mock-coordinator-loop");

      auto endpoints = fidl::CreateEndpoints<fuchsia_hardware_display::Coordinator>();
      ASSERT_TRUE(endpoints.is_ok());
      auto [client_end, server_end] = std::move(endpoints).value();

      fuchsia_hardware_display::wire::Info info = {
          .modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
              const_cast<display::WireDisplayMode*>(kDisplayModes.data()), kDisplayModes.size()),
          .max_layer_count = 4,
      };
      mock_coordinator_ = std::make_unique<display::test::MockDisplayCoordinator>(info);
      mock_coordinator_->set_import_buffer_collection_fn(
          [](display::WireBufferCollectionId,
             fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
            if (token.is_valid()) {
              auto st = fidl::WireCall(token)->Release();
              (void)st;
            }
          });

      libsync::Completion bound;
      async::PostTask(
          coord_loop_->dispatcher(), [this, server_end = std::move(server_end), &bound]() mutable {
            mock_coordinator_->Bind(std::move(server_end), {}, coord_loop_->dispatcher());
            bound.Signal();
          });
      bound.Wait();

      coordinator_ = std::make_shared<display::CoordinatorProxy>(std::move(client_end),
                                                                 reply_loop_->dispatcher());
    }

    next_wakeup_time_.reset();
    next_wakeup_from_now_.reset();
    inspector_ = inspect::Inspector();
    importer_ = ImageImporterImpl::New(
        config, dispatcher(), coordinator_, *reply_loop_, renderer_,
        [this]() -> std::optional<zx::time> {
          if (next_wakeup_from_now_.has_value()) {
            return zx::clock::get_monotonic() + *next_wakeup_from_now_;
          }
          return next_wakeup_time_;
        },
        inspector_.GetRoot().CreateChild("image_import"), std::move(raw_policy));
  }

  struct RegisteredCollection {
    allocation::GlobalBufferCollectionId id = 0;
    uint64_t sysmem_index = 0;
    bool registered_ok = false;
  };

  RegisteredCollection RegisterCollection(
      CollectionSpec spec = {},
      std::optional<allocation::GlobalBufferCollectionId> explicit_id = std::nullopt) {
    const auto collection_id = explicit_id.value_or(allocation::GenerateUniqueBufferCollectionId());
    auto [sysmem_index, root_token] = fake_sysmem_->CreateRootToken(spec);

    std::optional<bool> ok;
    async::Executor exec(dispatcher());
    auto promise = importer_->ImportBufferCollection(
        collection_id, sysmem_allocator_, std::move(root_token),
        allocation::BufferCollectionUsage::kClientImage, std::nullopt);
    exec.schedule_task(promise.then([&ok](fpromise::result<>& res) { ok = res.is_ok(); }));
    RunLoopUntil([&] { return ok.has_value(); });
    RunLoopUntilIdle();
    if (*ok && spec.auto_allocate) {
      const bool expect_display_release = importer_->config().enable_direct_to_display;
      RunLoopUntil([&] {
        const auto stats = fake_sysmem_->GetStats(sysmem_index);
        return stats.root_releases == 1u &&
               (!expect_display_release || stats.display_releases == 1u);
      });
    }
    return RegisteredCollection{
        .id = collection_id,
        .sysmem_index = sysmem_index,
        .registered_ok = *ok,
    };
  }

  allocation::ImageMetadata MakeMetadata(allocation::GlobalBufferCollectionId collection_id,
                                         uint32_t vmo_index = 0, uint32_t width = 64,
                                         uint32_t height = 64) {
    return allocation::ImageMetadata{
        .collection_id = collection_id,
        .identifier = allocation::kInvalidImageId,
        .vmo_index = vmo_index,
        .width = width,
        .height = height,
    };
  }

  inspect::Hierarchy ReadInspectRoot() {
    auto res = fpromise::run_single_threaded(inspect::ReadFromInspector(inspector_));
    FX_CHECK(res.is_ok());
    return res.take_value();
  }

  static uint64_t GetInspectUint(const inspect::Hierarchy& node, const std::string& name) {
    const auto* prop = node.node().get_property<inspect::UintPropertyValue>(name);
    FX_CHECK(prop) << "Missing uint property: " << name;
    return prop->value();
  }

  static std::string GetInspectString(const inspect::Hierarchy& node, const std::string& name) {
    const auto* prop = node.node().get_property<inspect::StringPropertyValue>(name);
    FX_CHECK(prop) << "Missing string property: " << name;
    return prop->value();
  }

  static bool GetInspectBool(const inspect::Hierarchy& node, const std::string& name) {
    const auto* prop = node.node().get_property<inspect::BoolPropertyValue>(name);
    FX_CHECK(prop) << "Missing bool property: " << name;
    return prop->value();
  }

  static uint64_t GetInspectHistogramCount(const inspect::Hierarchy& node,
                                           const std::string& name) {
    const auto* prop = node.node().get_property<inspect::UintArrayValue>(name);
    FX_CHECK(prop) << "Missing histogram property: " << name;
    uint64_t total = 0;
    for (const auto& bucket : prop->GetBuckets()) {
      total += bucket.count;
    }
    return total;
  }

  static uint64_t GetInspectHistogramNonZeroCount(const inspect::Hierarchy& node,
                                                  const std::string& name) {
    const auto* prop = node.node().get_property<inspect::UintArrayValue>(name);
    FX_CHECK(prop) << "Missing histogram property: " << name;
    uint64_t total = 0;
    for (const auto& bucket : prop->GetBuckets()) {
      if (bucket.floor > 0) {
        total += bucket.count;
      }
    }
    return total;
  }

  std::optional<utils::ScopedThreadDispatcherSetter> main_setter_;
  std::shared_ptr<utils::DispatcherHolder> reply_holder_;
  async::Loop sysmem_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  std::unique_ptr<FakeSysmem> fake_sysmem_;
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator_;

  std::unique_ptr<async::Loop> coord_loop_;
  std::unique_ptr<display::test::MockDisplayCoordinator> mock_coordinator_;
  std::unique_ptr<async::Loop> reply_loop_;
  std::shared_ptr<display::CoordinatorProxy> coordinator_;
  std::shared_ptr<ControllableRenderer> renderer_;
  std::optional<zx::time> next_wakeup_time_;
  std::optional<zx::duration> next_wakeup_from_now_;
  inspect::Inspector inspector_;
  std::shared_ptr<ImageImporterImpl> importer_;
};

// Each `policy` here must also appear in assembly's inline cases in `ui.rs`, which check that
// product assembly formats the matching `ImageImportConfig` to exactly that string.
// LINT.IfChange(policy_test_cases)
struct PolicyTestCase {
  const char* policy;
  ImportKinds kinds;
  OffScreenOrder off_screen;
  NeverShownImports never_shown;
  uint32_t idle_ms;
  uint32_t display_cap;
  bool reply_thread;
  // `FormatImageImportPolicy()` of the parsed `Config`.
  const char* formatted;
};

constexpr PolicyTestCase kPolicyTestCases[] = {
    {
        .policy = "",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kMostRecentFirst,
        .never_shown = NeverShownImports::kWhenIdle,
        .idle_ms = 100,
        .display_cap = 1,
        .reply_thread = false,
        .formatted =
            "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=when_idle,"
            "idle_ms=100,display_cap=1,reply_thread=false",
    },
    {
        .policy = "kinds=concurrent,off_screen=least_recent_first,never_shown=when_free,"
                  "idle_ms=250,display_cap=3,reply_thread=true",
        .kinds = ImportKinds::kConcurrent,
        .off_screen = OffScreenOrder::kLeastRecentFirst,
        .never_shown = NeverShownImports::kWhenFree,
        .idle_ms = 250,
        .display_cap = 3,
        .reply_thread = true,
        .formatted = "kinds=concurrent,off_screen=least_recent_first,never_shown=when_free,"
                     "idle_ms=250,display_cap=3,reply_thread=true",
    },
    {
        .policy = "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=never,"
                  "reply_thread=false",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kMostRecentFirst,
        .never_shown = NeverShownImports::kNever,
        .idle_ms = 100,
        .display_cap = 1,
        .reply_thread = false,
        .formatted = "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=never,"
                     "idle_ms=100,display_cap=1,reply_thread=false",
    },
    {
        .policy = "never_shown=when_idle",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kMostRecentFirst,
        .never_shown = NeverShownImports::kWhenIdle,
        .idle_ms = 100,
        .display_cap = 1,
        .reply_thread = false,
        .formatted =
            "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=when_idle,"
            "idle_ms=100,display_cap=1,reply_thread=false",
    },
    {
        .policy = "idle_ms=0",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kMostRecentFirst,
        .never_shown = NeverShownImports::kWhenIdle,
        .idle_ms = 0,
        .display_cap = 1,
        .reply_thread = false,
        .formatted =
            "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=when_idle,"
            "idle_ms=0,display_cap=1,reply_thread=false",
    },
    {
        .policy = "idle_ms=4294967295,display_cap=1",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kMostRecentFirst,
        .never_shown = NeverShownImports::kWhenIdle,
        .idle_ms = 4294967295,
        .display_cap = 1,
        .reply_thread = false,
        .formatted =
            "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=when_idle,"
            "idle_ms=4294967295,display_cap=1,reply_thread=false",
    },
    {
        .policy = "kinds=predicted_kind_first,off_screen=least_recent_first,never_shown=when_idle,"
                  "idle_ms=4294967295,display_cap=4294967295,reply_thread=false",
        .kinds = ImportKinds::kPredictedKindFirst,
        .off_screen = OffScreenOrder::kLeastRecentFirst,
        .never_shown = NeverShownImports::kWhenIdle,
        .idle_ms = 4294967295,
        .display_cap = 4294967295,
        .reply_thread = false,
        .formatted =
            "kinds=predicted_kind_first,off_screen=least_recent_first,never_shown=when_idle,"
            "idle_ms=4294967295,display_cap=4294967295,reply_thread=false",
    },
};
// LINT.ThenChange(/src/lib/assembly/platform_configuration/src/subsystems/ui.rs:policy_test_cases)

TEST(ParseImageImportPolicyTest, ParsesAllSupportedOverrides) {
  for (const PolicyTestCase& test_case : kPolicyTestCases) {
    SCOPED_TRACE(test_case.policy);

    ImageImporterImpl::Config cfg;
    ParseImageImportPolicy(test_case.policy, cfg);
    EXPECT_EQ(cfg.import_kinds, test_case.kinds);
    EXPECT_EQ(cfg.off_screen_order, test_case.off_screen);
    EXPECT_EQ(cfg.never_shown_imports, test_case.never_shown);
    EXPECT_EQ(cfg.idle_after, zx::msec(test_case.idle_ms));
    EXPECT_EQ(cfg.display_import_cap, test_case.display_cap);
    EXPECT_EQ(cfg.display_reply_thread, test_case.reply_thread);

    const std::string formatted = FormatImageImportPolicy(cfg);
    EXPECT_EQ(formatted, test_case.formatted);

    ImageImporterImpl::Config round_trip;
    ParseImageImportPolicy(formatted, round_trip);
    EXPECT_EQ(FormatImageImportPolicy(round_trip), formatted);
  }

  // Also check partial overrides onto a non-default `Config` and non-canonical key order.
  ImageImporterImpl::Config cfg;
  ParseImageImportPolicy(
      "kinds=concurrent,off_screen=least_recent_first,never_shown=when_free,"
      "idle_ms=250,display_cap=3,reply_thread=true",
      cfg);
  ParseImageImportPolicy(
      "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=never,"
      "idle_ms=0,reply_thread=false",
      cfg);
  EXPECT_EQ(FormatImageImportPolicy(cfg),
            "kinds=predicted_kind_first,off_screen=most_recent_first,never_shown=never,"
            "idle_ms=0,display_cap=3,reply_thread=false");

  ParseImageImportPolicy("display_cap=1,idle_ms=4294967295", cfg);
  EXPECT_EQ(cfg.display_import_cap, 1u);
  EXPECT_EQ(cfg.idle_after, zx::msec(4294967295));
}

TEST(ParseImageImportPolicyTest, RejectsMalformedOrUnknownOverrides) {
  struct InvalidCase {
    std::string_view policy;
    std::string_view error;
  };
  static constexpr InvalidCase kInvalidCases[] = {
      {.policy = "timing=eager", .error = "Unknown key"},
      {.policy = "kinds=x", .error = "Unknown kinds"},
      {.policy = "kinds=bogus", .error = "Unknown kinds"},
      {.policy = "off_screen=x", .error = "Unknown off_screen"},
      {.policy = "off_screen=bogus", .error = "Unknown off_screen"},
      {.policy = "never_shown=x", .error = "Unknown never_shown"},
      {.policy = "never_shown=bogus", .error = "Unknown never_shown"},
      {.policy = "predictor=last_frame_content", .error = "Unknown key"},
      {.policy = "reply_thread=x", .error = "Unknown reply_thread"},
      {.policy = "reply_thread=bogus", .error = "Unknown reply_thread"},
      {.policy = "display_cap=0", .error = "must be >= 1"},
      {.policy = "idle_ms=+5", .error = "Invalid uint32"},
      {.policy = "idle_ms=abc", .error = "Invalid uint32"},
      {.policy = "idle_ms=4294967296", .error = "Invalid uint32"},
      {.policy = "display_cap=4294967296", .error = "Invalid uint32"},
      {.policy = "kinds=concurrent,", .error = "Empty token in image_import_policy:"},
      {.policy = "kinds", .error = "Malformed"},
      {.policy = "=concurrent", .error = "Malformed"},
      {.policy = "kinds=", .error = "Malformed"},
      {.policy = "unknown_key=1", .error = "Unknown key"},
      {.policy = "kinds=concurrent,kinds=concurrent", .error = "Duplicate key"},
      {.policy = "idle_ms=01", .error = "Invalid uint32"},
      {.policy = "idle_ms=10ms", .error = "Invalid uint32"},
  };

  for (const auto& tc : kInvalidCases) {
    SCOPED_TRACE(tc.policy);
    ImageImporterImpl::Config cfg;
    auto r = TryParseImageImportPolicy(tc.policy, cfg);
    ASSERT_TRUE(r.is_error());
    EXPECT_THAT(r.error_value(), testing::HasSubstr(tc.error));
  }
}

TEST(ParseImageImportPolicyDeathTest, AbortsOnInvalidPolicy) {
  ImageImporterImpl::Config cfg;
  EXPECT_DEATH(ParseImageImportPolicy("kinds=bogus", cfg), "Unknown kinds");
}

TEST_F(ImageImporterImplTest, PerBufferBytesIsSysmemSizeRoundedUpToPage) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  // A 1x1 RGBA buffer's `size_bytes` (64) can be smaller than Vulkan's memory requirements for the
  // image, but the VMO is a whole page, so the renderer must be given the page-rounded size.
  std::atomic<uint64_t> renderer_per_buffer_bytes{0};
  renderer_->SetCreateGpuImageHook([&](ControllableRenderer& r,
                                       const allocation::ImageMetadata& meta,
                                       uint64_t per_buffer_bytes) {
    renderer_per_buffer_bytes.store(per_buffer_bytes);
    return r.MakeTrackedImage(meta.identifier);
  });

  auto coll = RegisterCollection(CollectionSpec{
      .display_branch_succeeds = false,
      .buffer_count = 1,
      .per_buffer_bytes = 64,
  });
  ASSERT_TRUE(coll.registered_ok);
  auto img = importer_->ImportImage(MakeMetadata(coll.id, /*vmo_index=*/0, 1, 1),
                                    ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ASSERT_NE(img, nullptr);
  RunLoopUntil([&] { return img->gpu_state() == ImportState::kReady; });

  const uint64_t page_size = zx_system_get_page_size();
  EXPECT_EQ(img->collection()->per_buffer_bytes(), page_size);
  EXPECT_EQ(renderer_per_buffer_bytes.load(), page_size);
  renderer_->SetCreateGpuImageHook(nullptr);

  EXPECT_EQ(ImportedCollection::VmoBytesPerBuffer(0), 0u);
  EXPECT_EQ(ImportedCollection::VmoBytesPerBuffer(1), page_size);
  EXPECT_EQ(ImportedCollection::VmoBytesPerBuffer(page_size), page_size);
  EXPECT_EQ(ImportedCollection::VmoBytesPerBuffer(page_size + 1), 2 * page_size);
}

TEST_F(ImageImporterImplTest, CollectionRegistrationAllocationAndSysmemReleaseMatrix) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);
  EXPECT_TRUE(importer_->SupportsDirectToDisplay());

  // 1. Display-capable collection with Y-tiled modifier -> tiling 2, both sysmem clients released.
  auto disp_coll = RegisterCollection(CollectionSpec{
      .buffer_count = 3,
      .per_buffer_bytes = 16384,
      .modifier = fuchsia_images2::PixelFormatModifier::kIntelI915YTiled,
      .min_size = {16, 16},
      .max_size = {512, 512},
  });
  ASSERT_TRUE(disp_coll.registered_ok);
  const auto disp_stats = fake_sysmem_->GetStats(disp_coll.sysmem_index);
  EXPECT_EQ(disp_stats.token_groups_created, 1u);
  EXPECT_EQ(disp_stats.root_collections_bound, 1u);
  EXPECT_EQ(disp_stats.display_collections_bound, 1u);

  auto img = importer_->ImportImage(MakeMetadata(disp_coll.id, /*vmo_index=*/1, 64, 64),
                                    ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ASSERT_NE(img, nullptr);
  ASSERT_NE(img->collection(), nullptr);
  EXPECT_EQ(img->collection()->state(), ImportedCollection::State::kDisplayCapable);
  EXPECT_EQ(img->collection()->tiling_type(), 2u);
  EXPECT_EQ(img->collection()->per_buffer_bytes(), 16384u);
  RunLoopUntil([&] {
    return img->display_state() == ImportState::kReady && img->gpu_state() == ImportState::kReady;
  });
  EXPECT_EQ(img->tiling_type(), 2u);

  // 2. Display branch pruned -> `kGpuOnly` (`ImportState::kUnsupported` on display).
  auto gpu_only_coll = RegisterCollection(CollectionSpec{
      .display_branch_succeeds = false,
      .buffer_count = 2,
      .per_buffer_bytes = 4096,
  });
  ASSERT_TRUE(gpu_only_coll.registered_ok);

  auto gpu_img =
      importer_->ImportImage(MakeMetadata(gpu_only_coll.id, 0, 32, 32),
                             ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] { return gpu_img->gpu_state() == ImportState::kReady; });
  EXPECT_EQ(gpu_img->display_state(), ImportState::kUnsupported);
  EXPECT_EQ(gpu_img->collection()->state(), ImportedCollection::State::kGpuOnly);

  // 3. Sysmem allocation failure -> `kFailed` and `on_failure(kCollectionFailed)`.
  auto failed_coll = RegisterCollection(CollectionSpec{
      .auto_allocate = false,
      .allocation_succeeds = false,
  });
  ASSERT_TRUE(failed_coll.registered_ok);
  std::optional<allocation::GlobalImageId> pre_alloc_got_id;
  std::optional<ImportImageError> pre_alloc_err;
  auto waiting_img = importer_->ImportImage(
      MakeMetadata(failed_coll.id, 0, 32, 32), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId got_id, ImportImageError e) {
        pre_alloc_got_id = got_id;
        pre_alloc_err = e;
      });
  ImportedImage* wait_arr[] = {waiting_img.get()};
  zx::event wait_ev = importer_->SignalWhenReady(wait_arr);
  ASSERT_TRUE(wait_ev.is_valid());

  fake_sysmem_->CompleteAllocation(failed_coll.sysmem_index);
  RunLoopUntil([&] { return pre_alloc_err.has_value(); });
  ASSERT_TRUE(pre_alloc_got_id.has_value());
  EXPECT_NE(*pre_alloc_got_id, allocation::kInvalidImageId);
  EXPECT_EQ(*pre_alloc_got_id, waiting_img->id());
  EXPECT_EQ(*pre_alloc_err, ImportImageError::kCollectionFailed);
  EXPECT_EQ(waiting_img->display_state(), ImportState::kFailed);
  EXPECT_EQ(waiting_img->gpu_state(), ImportState::kFailed);
  EXPECT_EQ(wait_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr), ZX_OK);

  // Importing onto a collection whose allocation already failed immediately fails with
  // `kCollectionFailed`.
  std::optional<allocation::GlobalImageId> post_fail_got_id;
  std::optional<ImportImageError> post_fail_coll_err;
  auto post_fail_img = importer_->ImportImage(
      MakeMetadata(failed_coll.id, 0, 32, 32), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId got_id, ImportImageError e) {
        post_fail_got_id = got_id;
        post_fail_coll_err = e;
      });
  RunLoopUntil([&] { return post_fail_coll_err.has_value(); });
  ASSERT_TRUE(post_fail_got_id.has_value());
  EXPECT_NE(*post_fail_got_id, allocation::kInvalidImageId);
  EXPECT_EQ(*post_fail_got_id, post_fail_img->id());
  EXPECT_EQ(*post_fail_coll_err, ImportImageError::kCollectionFailed);
  EXPECT_EQ(post_fail_img->display_state(), ImportState::kFailed);
  EXPECT_EQ(post_fail_img->gpu_state(), ImportState::kFailed);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*post_fail_img), ImportImageError::kCollectionFailed);

  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "collection_allocations_failed"), 1u);
    EXPECT_EQ(GetInspectUint(*node, "image_imports_rejected"), 2u);
  }

  // 4. Validation failures (`kUnknownCollection`, `kInvalidVmoIndex`, all 6 `kImageTooLarge`
  // dimension clauses, boundary acceptance at `min_size` / `max_size`, and pre-allocation bounds
  // failure).
  std::optional<ImportImageError> unknown_err;
  auto bad_coll_img = importer_->ImportImage(
      MakeMetadata(999999, 0, 32, 32), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId, ImportImageError e) { unknown_err = e; });
  RunLoopUntil([&] { return unknown_err.has_value(); });
  EXPECT_EQ(*unknown_err, ImportImageError::kUnknownCollection);
  (void)bad_coll_img;

  std::optional<ImportImageError> vmo_err;
  auto bad_vmo_img = importer_->ImportImage(
      MakeMetadata(disp_coll.id, /*vmo_index=*/3, 32, 32), ImageReadinessPolicy::kWaitForImports,
      reply_holder_, [&](allocation::GlobalImageId, ImportImageError e) { vmo_err = e; });
  RunLoopUntil([&] { return vmo_err.has_value(); });
  EXPECT_EQ(*vmo_err, ImportImageError::kInvalidVmoIndex);
  (void)bad_vmo_img;

  const std::pair<uint32_t, uint32_t> rejected_sizes[] = {
      {0, 64},     // zero width
      {64, 0},     // zero height
      {8, 64},     // width below `min_size` (16)
      {64, 8},     // height below `min_size` (16)
      {1024, 64},  // width above `max_size` (512)
      {64, 1024},  // height above `max_size` (512)
  };
  for (const auto& [w, h] : rejected_sizes) {
    SCOPED_TRACE(::testing::Message() << "size=" << w << "x" << h);
    std::optional<ImportImageError> bounds_err;
    auto bad_size_img = importer_->ImportImage(
        MakeMetadata(disp_coll.id, 0, w, h), ImageReadinessPolicy::kWaitForImports, reply_holder_,
        [&](allocation::GlobalImageId, ImportImageError e) { bounds_err = e; });
    RunLoopUntil([&] { return bounds_err.has_value(); });
    EXPECT_EQ(*bounds_err, ImportImageError::kImageTooLarge);
    EXPECT_EQ(ImageImportTestPeer::GpuError(*bad_size_img), ImportImageError::kImageTooLarge);
  }

  for (const auto& [w, h] : std::array<std::pair<uint32_t, uint32_t>, 2>{{{16, 16}, {512, 512}}}) {
    SCOPED_TRACE(::testing::Message() << "boundary_size=" << w << "x" << h);
    auto ok_img =
        importer_->ImportImage(MakeMetadata(disp_coll.id, 0, w, h),
                               ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    RunLoopUntil([&] {
      return ok_img->display_state() == ImportState::kReady &&
             ok_img->gpu_state() == ImportState::kReady;
    });
  }

  auto pre_bounds_coll = RegisterCollection(CollectionSpec{
      .auto_allocate = false,
      .min_size = {16, 16},
      .max_size = {512, 512},
  });
  ASSERT_TRUE(pre_bounds_coll.registered_ok);
  std::optional<ImportImageError> pre_bounds_err;
  auto pre_bounds_img = importer_->ImportImage(
      MakeMetadata(pre_bounds_coll.id, 0, /*width=*/1024, /*height=*/64),
      ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId, ImportImageError e) { pre_bounds_err = e; });
  fake_sysmem_->CompleteAllocation(pre_bounds_coll.sysmem_index);
  RunLoopUntil([&] { return pre_bounds_err.has_value(); });
  EXPECT_EQ(*pre_bounds_err, ImportImageError::kImageTooLarge);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*pre_bounds_img), ImportImageError::kImageTooLarge);

  // Dimensions exceeding `INT32_MAX` are rejected with `kImageTooLarge` even when sysmem
  // `max_size` allows `UINT32_MAX`.
  auto huge_max_coll = RegisterCollection(CollectionSpec{
      .min_size = {1, 1},
      .max_size = {UINT32_MAX, UINT32_MAX},
  });
  ASSERT_TRUE(huge_max_coll.registered_ok);
  std::optional<ImportImageError> int32_overflow_err;
  auto int32_overflow_img = importer_->ImportImage(
      MakeMetadata(huge_max_coll.id, 0, /*width=*/0x80000000u, /*height=*/64),
      ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId, ImportImageError e) { int32_overflow_err = e; });
  RunLoopUntil([&] { return int32_overflow_err.has_value(); });
  EXPECT_EQ(*int32_overflow_err, ImportImageError::kImageTooLarge);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*int32_overflow_img), ImportImageError::kImageTooLarge);
  EXPECT_EQ(
      GetInspectUint(*ReadInspectRoot().GetByPath({"image_import"}), "image_imports_rejected"),
      12u);

  // 5. Tearing down an unallocated (`kRegistered`) collection sends sysmem `Release()`.
  auto unallocated_coll = RegisterCollection(CollectionSpec{.auto_allocate = false});
  ASSERT_TRUE(unallocated_coll.registered_ok);
  importer_->ReleaseBufferCollection(unallocated_coll.id,
                                     allocation::BufferCollectionUsage::kClientImage);
  RunLoopUntil([&] {
    const auto s = fake_sysmem_->GetStats(unallocated_coll.sysmem_index);
    return s.root_releases == 1u && s.display_releases == 1u;
  });
}

TEST_F(ImageImporterImplTest, CollectionRegistrationErrorAndCancelPaths) {
  // 1. `CreateGpuCollection()` failure with `enable_direct_to_display == false`.
  {
    ImageImporterImpl::Config no_disp_cfg;
    no_disp_cfg.enable_direct_to_display = false;
    CreateImporter(no_disp_cfg);
    renderer_->set_fail_create_collection(true);

    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    EXPECT_EQ(renderer_->live_gpu_collections(), 0u);
    RunLoopUntil([&] { return fake_sysmem_->GetStats(res.sysmem_index).root_releases == 1u; });
    renderer_->set_fail_create_collection(false);
  }

  ImageImporterImpl::Config cfg;
  CreateImporter(cfg);

  // 2. `CreateGpuCollection()` failure with `enable_direct_to_display == true`: the promise stays
  // pending until the coordinator's `ImportBufferCollection()` replies, releases the coordinator
  // collection, and leaves the `collection_id` reusable.
  const auto reused_id = allocation::GenerateUniqueBufferCollectionId();
  {
    renderer_->set_fail_create_collection(true);
    libsync::Completion import_coll_entered;
    libsync::Completion allow_import_coll_reply;
    mock_coordinator_->set_import_buffer_collection_fn(
        [&](display::WireBufferCollectionId,
            fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
          if (token.is_valid()) {
            auto st = fidl::WireCall(token)->Release();
            (void)st;
          }
          import_coll_entered.Signal();
          allow_import_coll_reply.Wait();
        });

    auto [sysmem_idx, token] = fake_sysmem_->CreateRootToken();
    std::optional<bool> ok;
    async::Executor exec(dispatcher());
    exec.schedule_task(importer_
                           ->ImportBufferCollection(reused_id, sysmem_allocator_, std::move(token),
                                                    allocation::BufferCollectionUsage::kClientImage,
                                                    std::nullopt)
                           .then([&ok](fpromise::result<>& res) { ok = res.is_ok(); }));

    RunLoopUntil([&] { return import_coll_entered.signaled(); });
    RunLoopUntilIdle();
    EXPECT_FALSE(ok.has_value());

    allow_import_coll_reply.Signal();
    RunLoopUntil([&] { return ok.has_value(); });
    EXPECT_FALSE(*ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(sysmem_idx);
      return s.root_releases == 1u && s.display_releases == 1u &&
             mock_coordinator_->release_buffer_collection_count() == 1u;
    });
    EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);

    mock_coordinator_->set_import_buffer_collection_fn(
        [](display::WireBufferCollectionId,
           fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> tok) {
          if (tok.is_valid()) {
            auto st = fidl::WireCall(tok)->Release();
            (void)st;
          }
        });
    renderer_->set_fail_create_collection(false);

    auto re_reg = RegisterCollection(CollectionSpec{}, reused_id);
    EXPECT_TRUE(re_reg.registered_ok);
  }

  // 3. Coordinator `ImportBufferCollection()` error rejects without `ReleaseBufferCollection()`.
  {
    const uint32_t rel_before = mock_coordinator_->release_buffer_collection_count();
    mock_coordinator_->set_import_buffer_collection_result(ZX_ERR_NO_MEMORY);
    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(res.sysmem_index);
      return s.root_releases == 1u && s.display_releases == 1u;
    });
    EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
    EXPECT_EQ(mock_coordinator_->release_buffer_collection_count(), rel_before);
    mock_coordinator_->set_import_buffer_collection_result(ZX_OK);
  }

  // 4. Coordinator `SetBufferCollectionConstraints()` error releases the coordinator collection.
  {
    const uint32_t rel_before = mock_coordinator_->release_buffer_collection_count();
    mock_coordinator_->set_set_buffer_collection_constraints_result(ZX_ERR_NOT_SUPPORTED);
    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(res.sysmem_index);
      return s.root_releases == 1u && s.display_releases == 1u &&
             mock_coordinator_->release_buffer_collection_count() == rel_before + 1u;
    });
    EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
    mock_coordinator_->set_set_buffer_collection_constraints_result(ZX_OK);
  }

  // 5. Invalid token and duplicate `collection_id` are rejected immediately.
  {
    std::optional<bool> invalid_tok_ok;
    async::Executor exec(dispatcher());
    exec.schedule_task(importer_
                           ->ImportBufferCollection(
                               allocation::GenerateUniqueBufferCollectionId(), sysmem_allocator_,
                               fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>{},
                               allocation::BufferCollectionUsage::kClientImage, std::nullopt)
                           .then([&](fpromise::result<>& res) { invalid_tok_ok = res.is_ok(); }));
    RunLoopUntil([&] { return invalid_tok_ok.has_value(); });
    EXPECT_FALSE(*invalid_tok_ok);

    auto dup_res = RegisterCollection(CollectionSpec{}, reused_id);
    EXPECT_FALSE(dup_res.registered_ok);

    // Release `reused_id` (registered in subcase 2) so that subcase 7's `StopOnMain()` does not
    // also tear it down and send an extra coordinator `ReleaseBufferCollection()`.
    const uint32_t rel_before = mock_coordinator_->release_buffer_collection_count();
    importer_->ReleaseBufferCollection(reused_id, allocation::BufferCollectionUsage::kClientImage);
    RunLoopUntil(
        [&] { return mock_coordinator_->release_buffer_collection_count() == rel_before + 1u; });
  }

  // 6. `ReleaseBufferCollection()` while coordinator registration is pending cancels cleanly.
  {
    const uint32_t rel_before = mock_coordinator_->release_buffer_collection_count();
    libsync::Completion import_coll_entered;
    libsync::Completion allow_import_coll_reply;
    mock_coordinator_->set_import_buffer_collection_fn(
        [&](display::WireBufferCollectionId,
            fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
          if (token.is_valid()) {
            auto st = fidl::WireCall(token)->Release();
            (void)st;
          }
          import_coll_entered.Signal();
          allow_import_coll_reply.Wait();
        });

    const auto cancel_id = allocation::GenerateUniqueBufferCollectionId();
    auto [sysmem_idx, token] = fake_sysmem_->CreateRootToken();
    std::optional<bool> ok;
    async::Executor exec(dispatcher());
    exec.schedule_task(importer_
                           ->ImportBufferCollection(cancel_id, sysmem_allocator_, std::move(token),
                                                    allocation::BufferCollectionUsage::kClientImage,
                                                    std::nullopt)
                           .then([&ok](fpromise::result<>& res) { ok = res.is_ok(); }));

    RunLoopUntil([&] { return import_coll_entered.signaled(); });

    // A duplicate `collection_id` while the first registration is still pending in
    // `pending_registrations_` is rejected immediately, releases its token, and leaves the first
    // registration undisturbed.
    auto [dup_sysmem_idx, dup_token] = fake_sysmem_->CreateRootToken();
    std::optional<bool> dup_ok;
    exec.schedule_task(
        importer_
            ->ImportBufferCollection(cancel_id, sysmem_allocator_, std::move(dup_token),
                                     allocation::BufferCollectionUsage::kClientImage, std::nullopt)
            .then([&dup_ok](fpromise::result<>& res) { dup_ok = res.is_ok(); }));
    RunLoopUntil([&] { return dup_ok.has_value(); });
    EXPECT_FALSE(*dup_ok);
    EXPECT_FALSE(ok.has_value());
    RunLoopUntil([&] { return fake_sysmem_->GetStats(dup_sysmem_idx).token_releases == 1u; });

    importer_->ReleaseBufferCollection(cancel_id, allocation::BufferCollectionUsage::kClientImage);
    allow_import_coll_reply.Signal();

    RunLoopUntil([&] { return ok.has_value(); });
    EXPECT_FALSE(*ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(sysmem_idx);
      return s.root_releases == 1u && s.display_releases == 1u &&
             mock_coordinator_->release_buffer_collection_count() == rel_before + 1u;
    });
    EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
  }

  // 7. `StopOnMain()` while coordinator registration is pending resolves the promise as failed and
  // releases sysmem participants and the in-flight coordinator collection.
  {
    const uint32_t rel_before = mock_coordinator_->release_buffer_collection_count();
    libsync::Completion import_coll_entered;
    libsync::Completion allow_import_coll_reply;
    mock_coordinator_->set_import_buffer_collection_fn(
        [&](display::WireBufferCollectionId,
            fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
          if (token.is_valid()) {
            auto st = fidl::WireCall(token)->Release();
            (void)st;
          }
          import_coll_entered.Signal();
          allow_import_coll_reply.Wait();
        });

    const auto stop_id = allocation::GenerateUniqueBufferCollectionId();
    auto [sysmem_idx, token] = fake_sysmem_->CreateRootToken();
    std::optional<bool> ok;
    async::Executor exec(dispatcher());
    exec.schedule_task(importer_
                           ->ImportBufferCollection(stop_id, sysmem_allocator_, std::move(token),
                                                    allocation::BufferCollectionUsage::kClientImage,
                                                    std::nullopt)
                           .then([&ok](fpromise::result<>& res) { ok = res.is_ok(); }));

    RunLoopUntil([&] { return import_coll_entered.signaled(); });
    importer_->StopOnMain();
    allow_import_coll_reply.Signal();

    RunLoopUntil([&] { return ok.has_value(); });
    EXPECT_FALSE(*ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(sysmem_idx);
      return s.root_releases == 1u && s.display_releases == 1u &&
             mock_coordinator_->release_buffer_collection_count() == rel_before + 1u;
    });
    EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
    EXPECT_EQ(renderer_->live_gpu_collections(), 0u);
  }

  // Recreate a live importer after subcase 7's `StopOnMain()` so subcases 8-12 exercise
  // active registration failure paths.
  CreateImporter(cfg);

  // 8. Sysmem failure on first `DuplicateSync()` (GPU token) releases `parent_token` and rejects.
  {
    fake_sysmem_->SetFailDuplicateSyncIndex(0);
    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    RunLoopUntil([&] { return fake_sysmem_->GetStats(res.sysmem_index).token_releases == 1u; });
    fake_sysmem_->ResetFailureInjections();
  }

  // 9. Sysmem failure on `CreateChildrenSync()` releases `parent_token` and `gpu_token`.
  {
    fake_sysmem_->SetFailCreateChildrenSyncIndex(0);
    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    RunLoopUntil([&] { return fake_sysmem_->GetStats(res.sysmem_index).token_releases == 2u; });
    fake_sysmem_->ResetFailureInjections();
  }

  // 10. Sysmem failure on second `DuplicateSync()` (display check token) releases all 4 tokens
  // (`empty_child`, `child0`, `parent_token`, `gpu_token`).
  {
    fake_sysmem_->SetFailDuplicateSyncIndex(1);
    auto res = RegisterCollection();
    EXPECT_FALSE(res.registered_ok);
    RunLoopUntil([&] { return fake_sysmem_->GetStats(res.sysmem_index).token_releases == 4u; });
    fake_sysmem_->ResetFailureInjections();
  }

  // There is no subcase for a synchronous `BindEmptyConstraintsParticipant()` failure on the first
  // call (display participant): that call sends a token freshly duplicated by sysmem, and FIDL
  // clients report one-way writes to a closed or unbound peer as success, so a test cannot make it
  // fail synchronously.

  // 11. Synchronous `BindEmptyConstraintsParticipant()` failure on second call (allocation
  // participant, triggered by stripping `ZX_RIGHT_TRANSFER` from `parent_token` so token-method
  // calls succeed but transferring `parent_token` in `BindSharedCollection()` fails synchronously)
  // abandons the display participant (`display_releases == 1u`) and releases the 3 surviving
  // tokens (`empty_child`, `child0`, `gpu_token`).
  {
    const auto coll_id = allocation::GenerateUniqueBufferCollectionId();
    auto [sysmem_idx, root_token] = fake_sysmem_->CreateRootToken();
    zx::channel stripped_ch;
    ASSERT_EQ(root_token.TakeChannel().replace(ZX_DEFAULT_CHANNEL_RIGHTS & ~ZX_RIGHT_TRANSFER,
                                               &stripped_ch),
              ZX_OK);
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> no_transfer_token(
        std::move(stripped_ch));

    std::optional<bool> ok;
    async::Executor exec(dispatcher());
    exec.schedule_task(
        importer_
            ->ImportBufferCollection(coll_id, sysmem_allocator_, std::move(no_transfer_token),
                                     allocation::BufferCollectionUsage::kClientImage, std::nullopt)
            .then([&ok](fpromise::result<>& res) { ok = res.is_ok(); }));
    RunLoopUntil([&] { return ok.has_value(); });
    EXPECT_FALSE(*ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(sysmem_idx);
      return s.display_releases == 1u && s.token_releases == 3u;
    });
  }
}

TEST_F(ImageImporterImplTest, DeferredCollectionTeardownWaitsForLastLiveImage) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
  ASSERT_TRUE(coll.registered_ok);

  auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] {
    return img1->display_state() == ImportState::kReady &&
           img1->gpu_state() == ImportState::kReady &&
           img2->display_state() == ImportState::kReady && img2->gpu_state() == ImportState::kReady;
  });

  importer_->ReleaseBufferCollection(coll.id, allocation::BufferCollectionUsage::kClientImage);
  RunLoopUntilIdle();
  EXPECT_EQ(renderer_->live_gpu_collections(), 1u);
  EXPECT_EQ(mock_coordinator_->release_buffer_collection_count(), 0u);

  importer_->ReleaseImage(img1);
  RunLoopUntil([&] {
    return img1->display_state() == ImportState::kReleased &&
           img1->gpu_state() == ImportState::kReleased;
  });
  RunLoopUntilIdle();
  EXPECT_EQ(renderer_->live_gpu_collections(), 1u);
  EXPECT_EQ(mock_coordinator_->release_buffer_collection_count(), 0u);

  importer_->ReleaseImage(img2);
  RunLoopUntil([&] {
    return img2->display_state() == ImportState::kReleased &&
           img2->gpu_state() == ImportState::kReleased;
  });
  RunLoopUntil([&] { return mock_coordinator_->release_buffer_collection_count() == 1u; });
  EXPECT_EQ(renderer_->live_gpu_collections(), 0u);
}

TEST_F(ImageImporterImplTest, GpuImportFailureOnLiveImageFiresCallbackAndIncrementsInspect) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
  ASSERT_TRUE(coll.registered_ok);

  libsync::Completion hook_called;
  libsync::Completion allow_hook_finish;
  renderer_->SetCreateGpuImageHook(
      [&](ControllableRenderer&, const allocation::ImageMetadata&,
          uint64_t) -> fit::result<ImportImageError, std::unique_ptr<Renderer::GpuImage>> {
        hook_called.Signal();
        allow_hook_finish.Wait();
        return fit::error(ImportImageError::kGpuImportFailed);
      });

  std::optional<allocation::GlobalImageId> wait_got_id;
  std::optional<ImportImageError> wait_err;
  bool wait_cb_on_main = false;
  auto wait_img = importer_->ImportImage(
      MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId got_id, ImportImageError err) {
        wait_cb_on_main = utils::IsOnMainThread();
        wait_got_id = got_id;
        wait_err = err;
      });
  ImportedImage* wait_arr[] = {wait_img.get()};
  zx::event wait_ev = importer_->SignalWhenReady(wait_arr);
  ASSERT_TRUE(wait_ev.is_valid());

  allow_hook_finish.Signal();
  RunLoopUntil([&] { return wait_err.has_value(); });
  EXPECT_TRUE(wait_cb_on_main);
  ASSERT_TRUE(wait_got_id.has_value());
  EXPECT_NE(*wait_got_id, allocation::kInvalidImageId);
  EXPECT_EQ(*wait_got_id, wait_img->id());
  EXPECT_EQ(*wait_err, ImportImageError::kGpuImportFailed);
  EXPECT_EQ(wait_img->gpu_state(), ImportState::kFailed);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*wait_img), ImportImageError::kGpuImportFailed);
  EXPECT_EQ(wait_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr), ZX_OK);
  EXPECT_EQ(GetInspectUint(*ReadInspectRoot().GetByPath({"image_import"}), "gpu_imports_failed"),
            1u);

  std::optional<ImportImageError> opt_err;
  bool opt_cb_on_main = false;
  auto opt_img =
      importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                             reply_holder_, [&](allocation::GlobalImageId, ImportImageError err) {
                               opt_cb_on_main = utils::IsOnMainThread();
                               opt_err = err;
                             });
  RunLoopUntil([&] { return opt_err.has_value(); });
  EXPECT_TRUE(opt_cb_on_main);
  EXPECT_EQ(*opt_err, ImportImageError::kGpuImportFailed);
  EXPECT_EQ(opt_img->gpu_state(), ImportState::kFailed);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*opt_img), ImportImageError::kGpuImportFailed);
  EXPECT_EQ(GetInspectUint(*ReadInspectRoot().GetByPath({"image_import"}), "gpu_imports_failed"),
            2u);
}

TEST_F(ImageImporterImplTest, ImportImageAfterStopOnMainReturnsStoppedWithoutLeakingLiveImage) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection();
  ASSERT_TRUE(coll.registered_ok);

  importer_->StopOnMain();
  const auto root_before = ReadInspectRoot();
  const auto* node_before = root_before.GetByPath({"image_import"});
  ASSERT_NE(node_before, nullptr);
  const uint64_t failed_before = GetInspectUint(*node_before, "gpu_imports_failed");
  const uint64_t rejected_before = GetInspectUint(*node_before, "image_imports_rejected");

  bool callback_called = false;
  auto img = importer_->ImportImage(
      MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId, ImportImageError) { callback_called = true; });
  RunLoopUntilIdle();

  EXPECT_EQ(img->display_state(), ImportState::kFailed);
  EXPECT_EQ(img->gpu_state(), ImportState::kFailed);
  EXPECT_EQ(ImageImportTestPeer::GpuError(*img), ImportImageError::kStopped);
  EXPECT_FALSE(callback_called);
  const auto root_after = ReadInspectRoot();
  const auto* node_after = root_after.GetByPath({"image_import"});
  ASSERT_NE(node_after, nullptr);
  EXPECT_EQ(GetInspectUint(*node_after, "gpu_imports_failed"), failed_before);
  EXPECT_EQ(GetInspectUint(*node_after, "image_imports_rejected"), rejected_before);
  EXPECT_EQ(ImageImporterImplTestPeer::LiveImagesCount(*importer_), 0u);
}

TEST_F(ImageImporterImplTest, DirectToDisplayDisabledSkipsTokenGroupAndCoordinator) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);
  EXPECT_FALSE(importer_->SupportsDirectToDisplay());

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);
  const auto stats = fake_sysmem_->GetStats(coll.sysmem_index);
  EXPECT_EQ(stats.token_groups_created, 0u);
  EXPECT_EQ(stats.display_collections_bound, 0u);

  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0, 32, 32),
                                    ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] { return img->gpu_state() == ImportState::kReady; });
  EXPECT_EQ(img->display_state(), ImportState::kUnsupported);
  ImportedImage* imgs[] = {img.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(imgs), FrameOutcome::kGpuOnlyImage);
}

using ImageImporterImplDeathTest = ImageImporterImplTest;

TEST_F(ImageImporterImplDeathTest, LegacyBufferCollectionImporterImageMethodsAbort) {
  CreateImporter(ImageImporterImpl::Config{});
  EXPECT_DEATH(
      (void)importer_->ImportBufferImage(MakeMetadata(/*collection_id=*/1, /*vmo_index=*/0),
                                         allocation::BufferCollectionUsage::kClientImage),
      "");
  EXPECT_DEATH(importer_->ReleaseBufferImage(allocation::GlobalImageId(1)), "");
}

class ReplyThreadModeTest : public ImageImporterImplTest,
                            public ::testing::WithParamInterface<bool> {};

TEST_P(ReplyThreadModeTest, DisplayImportsFrameWaitFailureForgetAndPeerClosed) {
  ImageImporterImpl::Config cfg;
  cfg.display_reply_thread = GetParam();
  cfg.import_kinds = ImportKinds::kConcurrent;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 4, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);

  // 1. Idle-window display + GPU import completes and steady-state `SignalWhenReady()` returns an
  // invalid event.
  auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0, 64, 64),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] {
    return img1->display_state() == ImportState::kReady && img1->gpu_state() == ImportState::kReady;
  });
  ImportedImage* arr1[] = {img1.get()};
  EXPECT_FALSE(importer_->SignalWhenReady(arr1).is_valid());
  EXPECT_EQ(importer_->CheckDisplayStates(arr1), FrameOutcome::kDirect);
  EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr1), FrameOutcome::kDirect);
  EXPECT_EQ(coordinator_->current_image_count(), 1u);

  // 2. Frame wait inside `PrepareForDirectToDisplay()` while coordinator `ImportImage()` is gated.
  libsync::Completion import_entered;
  libsync::Completion allow_import_finish;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        import_entered.Signal();
        allow_import_finish.Wait();
      });

  // Defer idle-window display `ImportImage()` calls by setting a near wakeup so
  // `PrepareForDirectToDisplay()` makes the call for `img2` itself.
  next_wakeup_from_now_ = zx::usec(100);
  auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1, 64, 64),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(img2->display_state(), ImportState::kQueued);

  std::thread releaser([&] {
    import_entered.Wait();
    allow_import_finish.Signal();
  });
  ImportedImage* arr2[] = {img2.get()};
  EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr2), FrameOutcome::kDirect);
  releaser.join();
  EXPECT_EQ(img2->display_state(), ImportState::kReady);
  next_wakeup_from_now_.reset();
  mock_coordinator_->set_import_image_fn(nullptr);

  // 3. Failed display import unregisters the image from `CoordinatorProxy`.
  const uint64_t count_before_fail = coordinator_->current_image_count();
  mock_coordinator_->set_import_image_result(ZX_ERR_NOT_SUPPORTED);
  auto img3 = importer_->ImportImage(MakeMetadata(coll.id, 2, 64, 64),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] {
    return img3->display_state() == ImportState::kFailed &&
           img3->gpu_state() == ImportState::kReady;
  });
  RunLoopUntilIdle();
  ImportedImage* arr3[] = {img3.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(arr3), FrameOutcome::kDisplayImportFailed);
  EXPECT_EQ(coordinator_->current_image_count(), count_before_fail);
  mock_coordinator_->set_import_image_result(ZX_OK);

  // 4. `ZX_CHANNEL_PEER_CLOSED` on an idle screen with an in-flight display import unblocks
  // `SignalWhenReady()` without a re-arm storm on the main loop.
  coord_loop_->Quit();
  coord_loop_->JoinThreads();

  auto img4 = importer_->ImportImage(MakeMetadata(coll.id, 3, 64, 64),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ImportedImage* arr4[] = {img4.get()};
  zx::event ev4 = importer_->SignalWhenReady(arr4);
  ASSERT_TRUE(ev4.is_valid());
  RunLoopUntil([&] {
    return img4->display_state() == ImportState::kImporting &&
           img4->gpu_state() == ImportState::kReady;
  });
  EXPECT_EQ(ev4.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr), ZX_ERR_TIMED_OUT);

  coord_loop_->Shutdown();
  coord_loop_.reset();
  mock_coordinator_.reset();

  if (!GetParam()) {
    RunLoopUntil([&] {
      return ev4.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK;
    });
    // Running the loop further must not spin in an `async::Wait` re-arm storm.
    const uint64_t calls_after_close =
        ImageImporterImplTestPeer::ReplyChannelReadableCalls(*importer_);
    RunLoopUntilIdle();
    EXPECT_EQ(ImageImporterImplTestPeer::ReplyChannelReadableCalls(*importer_), calls_after_close);
  } else {
    ASSERT_EQ(ev4.wait_one(ZX_EVENT_SIGNALED, zx::deadline_after(zx::sec(5)), nullptr), ZX_OK);
  }
  EXPECT_EQ(img4->display_state(), ImportState::kFailed);

  // 5. Registering a collection after coordinator peer-close fails locally without hanging or
  // leaking a `GpuCollection` or sysmem tokens.
  const uint32_t live_gpu_colls_before = renderer_->live_gpu_collections();
  auto after_close = RegisterCollection();
  EXPECT_FALSE(after_close.registered_ok);
  RunLoopUntil([&] {
    const auto s = fake_sysmem_->GetStats(after_close.sysmem_index);
    return s.root_releases == 1u && s.display_releases == 1u &&
           s.token_releases == (GetParam() ? 2u : 3u);
  });
  EXPECT_EQ(renderer_->live_gpu_collections(), live_gpu_colls_before);

  // 6. Constructing `ImageImporterImpl` with `display_reply_thread == false` after the
  // `CoordinatorProxy` channel has already unbound (`channel_handle() == ZX_HANDLE_INVALID`)
  // succeeds without aborting and cleanly rejects subsequent registrations.
  if (!GetParam()) {
    importer_->StopOnMain();
    importer_.reset();
    reply_loop_->RunUntilIdle();
    EXPECT_EQ(coordinator_->channel_handle(), ZX_HANDLE_INVALID);

    importer_ =
        ImageImporterImpl::New(cfg, dispatcher(), coordinator_, *reply_loop_, renderer_,
                               [this]() -> std::optional<zx::time> { return next_wakeup_time_; },
                               inspector_.GetRoot().CreateChild("image_import_unbound"), {});
    auto post_unbind_reg = RegisterCollection();
    EXPECT_FALSE(post_unbind_reg.registered_ok);
    RunLoopUntil([&] {
      const auto s = fake_sysmem_->GetStats(post_unbind_reg.sysmem_index);
      return s.root_releases == 1u && s.display_releases == 1u && s.token_releases == 3u;
    });
    importer_->StopOnMain();
    importer_.reset();
  }
}

TEST_P(ReplyThreadModeTest, StopOnMainDrainsInFlightDisplayImportCleanly) {
  ImageImporterImpl::Config cfg;
  cfg.display_reply_thread = GetParam();
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection();
  ASSERT_TRUE(coll.registered_ok);

  libsync::Completion import_started;
  libsync::Completion release_import;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        import_started.Signal();
        release_import.Wait();
      });

  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0, 64, 64),
                                    ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntil([&] { return import_started.signaled(); });

  ImageImporterImplTestPeer::SetStopDrainEnteredHook(*importer_, [&] { release_import.Signal(); });
  importer_->StopOnMain();
  EXPECT_EQ(img->display_state(), ImportState::kReleased);
}

TEST_P(ReplyThreadModeTest, AsyncTeardownWhileReadableWaitArmedDoesNotAssertOrUaf) {
  if (GetParam()) {
    GTEST_SKIP();
  }

  // 1. `AsyncTeardown()` pumped during `PrepareForDirectToDisplay()` while `readable_wait` was
  // armed with no signal packet queued on the client channel.
  {
    ImageImporterImpl::Config cfg;
    cfg.display_reply_thread = false;
    cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cfg);

    auto coll = RegisterCollection();
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_from_now_ = zx::usec(100);
    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0, 64, 64),
                                      ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    RunLoopUntilIdle();
    ASSERT_EQ(img->display_state(), ImportState::kQueued);

    coordinator_->raw().AsyncTeardown();
    ImportedImage* arr[] = {img.get()};
    EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr), FrameOutcome::kDisplayImportFailed);
    EXPECT_EQ(coordinator_->channel_handle(), ZX_HANDLE_INVALID);

    importer_->StopOnMain();
    importer_.reset();
    RunLoopUntilIdle();
  }

  // 2. `AsyncTeardown()` pumped during `StopOnMain()` while an import is outstanding and
  // `readable_wait` was armed with no signal packet queued on the client channel.
  {
    ImageImporterImpl::Config cfg;
    cfg.display_reply_thread = false;
    cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cfg);

    auto coll = RegisterCollection();
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion import_started;
    libsync::Completion release_import;
    libsync::Completion import_returned;
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          import_started.Signal();
          release_import.Wait();
          import_returned.Signal();
        });

    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0, 64, 64),
                                      ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    RunLoopUntil([&] { return import_started.signaled(); });

    ImageImporterImplTestPeer::SetStopDrainEnteredHook(*importer_, [&] {
      coordinator_->raw().AsyncTeardown();
      release_import.Signal();
    });
    importer_->StopOnMain();
    EXPECT_EQ(coordinator_->channel_handle(), ZX_HANDLE_INVALID);
    // The teardown cancels the import, so `StopOnMain()` can return while the coordinator thread is
    // still returning from `release_import.Wait()`. Wait for it before the completions go out of
    // scope. Do not clear the handler here: the coordinator thread may still be returning through
    // it, and `TearDown()` shuts down `coord_loop_` before destroying `mock_coordinator_`.
    import_returned.Wait();
    img.reset();
    importer_.reset();
    RunLoopUntilIdle();
  }
}

TEST_F(ImageImporterImplTest, InFlightRegistrationReplyAfterStopAndMainLoopDestruction) {
  async::Loop local_coord_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  local_coord_loop.StartThread("local-mock-coordinator-loop");

  fuchsia_hardware_display::wire::Info info = {
      .modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
          const_cast<display::WireDisplayMode*>(kDisplayModes.data()), kDisplayModes.size()),
      .max_layer_count = 4,
  };
  display::test::MockDisplayCoordinator local_mock_coordinator(info);

  libsync::Completion import_coll_entered;
  libsync::Completion allow_import_coll_reply;
  local_mock_coordinator.set_import_buffer_collection_fn(
      [&](display::WireBufferCollectionId,
          fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token) {
        if (token.is_valid()) {
          auto st = fidl::WireCall(token)->Release();
          (void)st;
        }
        import_coll_entered.Signal();
        allow_import_coll_reply.Wait();
      });

  auto [coord_client, coord_server] =
      fidl::Endpoints<fuchsia_hardware_display::Coordinator>::Create();
  libsync::Completion bound;
  async::PostTask(
      local_coord_loop.dispatcher(), [&local_mock_coordinator, &local_coord_loop,
                                      server = std::move(coord_server), &bound]() mutable {
        local_mock_coordinator.Bind(std::move(server), {}, local_coord_loop.dispatcher());
        bound.Signal();
      });
  bound.Wait();

  async::Loop outliving_reply_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
  auto local_coordinator = std::make_shared<display::CoordinatorProxy>(
      std::move(coord_client), outliving_reply_loop.dispatcher());

  main_setter_.reset();
  std::thread isolated_main_thread([&] {
    async::Loop local_main_loop(&kAsyncLoopConfigAttachToCurrentThread);
    utils::ScopedThreadDispatcherSetter local_main_setter(local_main_loop.dispatcher(),
                                                          local_main_loop.dispatcher());
    fidl::WireClient<fuchsia_sysmem2::Allocator> local_sysmem =
        fake_sysmem_->ConnectAllocator(local_main_loop.dispatcher());

    ImageImporterImpl::Config cfg;
    cfg.display_reply_thread = true;
    auto local_importer = ImageImporterImpl::New(
        cfg, local_main_loop.dispatcher(), local_coordinator, outliving_reply_loop, renderer_,
        []() -> std::optional<zx::time> { return std::nullopt; }, inspect::Node(), {});

    const auto coll_id = allocation::GenerateUniqueBufferCollectionId();
    auto [sysmem_idx, token] = fake_sysmem_->CreateRootToken();
    (void)sysmem_idx;
    async::Executor exec(local_main_loop.dispatcher());
    exec.schedule_task(local_importer->ImportBufferCollection(
        coll_id, local_sysmem, std::move(token), allocation::BufferCollectionUsage::kClientImage,
        std::nullopt));

    while (!import_coll_entered.signaled()) {
      local_main_loop.RunUntilIdle();
      zx::nanosleep(zx::deadline_after(zx::usec(100)));
    }

    local_importer->StopOnMain();
    local_importer.reset();
    local_main_loop.Shutdown();
  });
  isolated_main_thread.join();
  main_setter_.emplace(dispatcher(), dispatcher());

  allow_import_coll_reply.Signal();
  libsync::Completion reply_drained;
  async::PostTask(outliving_reply_loop.dispatcher(), [&] { reply_drained.Signal(); });
  reply_drained.Wait();
  local_coordinator.reset();
  outliving_reply_loop.Quit();
  outliving_reply_loop.JoinThreads();
  outliving_reply_loop.Shutdown();
  local_coord_loop.Shutdown();
}

TEST_F(ImageImporterImplTest, ReplyThreadCallbackAllowsOwnerDestructionOnMainImmediatelyAfterStop) {
  ImageImporterImpl::Config cfg;
  cfg.display_reply_thread = true;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection();
  ASSERT_TRUE(coll.registered_ok);

  libsync::Completion import_started;
  libsync::Completion allow_reply;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        import_started.Signal();
        allow_reply.Wait();
      });

  libsync::Completion hook_entered;
  libsync::Completion release_hook;
  libsync::Completion hook_exited;
  ImageImporterImplTestPeer::SetPostDisplayDecrementHook(*importer_, [&] {
    hook_entered.Signal();
    release_hook.Wait();
    hook_exited.Signal();
  });
  ImageImporterImplTestPeer::SetStopDrainEnteredHook(*importer_, [&] { allow_reply.Signal(); });

  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0, 64, 64),
                                    ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntil([&] { return import_started.signaled(); });

  // `StopOnMain()` unblocks as soon as `OnDisplayImportComplete()` decrements
  // `display_imports_outstanding_` and unlocks `pending_signal_mutex_`. Hold the reply thread
  // right before its return while main drops the last `shared_ptr<ImageImporterImpl>`.
  importer_->StopOnMain();
  hook_entered.Wait();
  img.reset();
  importer_.reset();
  EXPECT_TRUE(utils::IsOnMainThread());
  release_hook.Signal();
  // The reply thread is still reading `release_hook` when `Wait()` wakes it, so wait for the hook
  // to return before these stack-allocated completions go out of scope.
  hook_exited.Wait();
}

TEST_F(ImageImporterImplTest, DisplayReplyHistogramAndFrameDurationMetrics) {
  // 1. In `display_reply_thread == true` mode, `display_reply_handling_delay_us` stays empty.
  {
    ImageImporterImpl::Config reply_cfg;
    reply_cfg.display_reply_thread = true;
    reply_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(reply_cfg);

    auto coll = RegisterCollection();
    ASSERT_TRUE(coll.registered_ok);
    auto img = importer_->ImportImage(
        MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    RunLoopUntil([&] { return img->display_state() == ImportState::kReady; });

    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectHistogramCount(*node, "display_reply_handling_delay_us"), 0u);
  }

  // 2. In `display_reply_thread == false` mode, a frame-driven `PrepareForDirectToDisplay()`
  // import updates `display_import_duration_` off its 3 ms seed even without a readable packet.
  {
    ImageImporterImpl::Config main_cfg;
    main_cfg.display_reply_thread = false;
    main_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(main_cfg);

    auto coll = RegisterCollection();
    ASSERT_TRUE(coll.registered_ok);
    EXPECT_EQ(ImageImporterImplTestPeer::DisplayImportDurationPrediction(*importer_), zx::msec(3));

    mock_coordinator_->set_import_image_fn(
        [](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          zx::nanosleep(zx::deadline_after(zx::msec(4)));
        });
    next_wakeup_from_now_ = zx::msec(1);
    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
    RunLoopUntilIdle();
    ASSERT_EQ(img->display_state(), ImportState::kQueued);

    ImportedImage* arr[] = {img.get()};
    EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr), FrameOutcome::kDirect);
    EXPECT_GT(ImageImporterImplTestPeer::DisplayImportDurationPrediction(*importer_), zx::msec(3));
  }

  // 3. In `display_reply_thread == false` mode, idle-window imports completing via channel readable
  // record `display_reply_handling_delay_us`.
  {
    ImageImporterImpl::Config idle_cfg;
    idle_cfg.display_reply_thread = false;
    idle_cfg.display_import_cap = 2;
    idle_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(idle_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);

    auto img1 = importer_->ImportImage(
        MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    auto img2 = importer_->ImportImage(
        MakeMetadata(coll.id, 1), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    RunLoopUntil([&] {
      return img1->display_state() == ImportState::kReady &&
             img2->display_state() == ImportState::kReady;
    });

    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_GE(GetInspectHistogramCount(*node, "display_reply_handling_delay_us"), 1u);
  }

  // 4. Stale `pending_channel_readable_timestamp_ < sent_at` (left by a reply drained
  // synchronously inside `PrepareForDirectToDisplay()` before `main_dispatcher_` ran the wait
  // packet) is ignored by a later `OnDisplayImportComplete()` rather than clamping
  // `display_import_duration_` toward 0.
  {
    ImageImporterImpl::Config stale_cfg;
    stale_cfg.display_reply_thread = false;
    stale_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    // With `kPredictedKindFirst`, a GPU job still running on the worker would keep `img1`'s display
    // import from being sent before `second_reply_sent.Wait()` blocks main.
    stale_cfg.import_kinds = ImportKinds::kConcurrent;
    CreateImporter(stale_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion second_reply_sent;
    std::atomic<uint32_t> call_count{0};
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          zx::nanosleep(zx::deadline_after(zx::msec(4)));
          if (call_count.fetch_add(1) == 1) {
            second_reply_sent.Signal();
          }
        });

    next_wakeup_from_now_ = zx::msec(1);
    auto img0 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    // Drain `img0` synchronously on `reply_loop_` without pumping `main_dispatcher_`, leaving an
    // older kernel `ZX_CHANNEL_READABLE` packet timestamp queued on `main_dispatcher_`.
    ImportedImage* arr0[] = {img0.get()};
    EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr0), FrameOutcome::kDirect);

    zx::nanosleep(zx::deadline_after(zx::msec(5)));
    next_wakeup_from_now_.reset();
    auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    second_reply_sent.Wait();
    ASSERT_EQ(zx_object_wait_one(coordinator_->channel_handle(), ZX_CHANNEL_READABLE,
                                 zx::time::infinite().get(), nullptr),
              ZX_OK);

    RunLoopUntil([&] { return img1->display_state() == ImportState::kReady; });
    EXPECT_GT(ImageImporterImplTestPeer::DisplayImportDurationPrediction(*importer_), zx::msec(3));
    mock_coordinator_->set_import_image_fn(nullptr);
  }
}

INSTANTIATE_TEST_SUITE_P(BothReplyModes, ReplyThreadModeTest, ::testing::Bool());

TEST_F(ImageImporterImplTest, PriorityClassesOptimisticPrecedenceAndOccludedExpiry) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.worker_pool_size = 0;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  cfg.off_screen_order = OffScreenOrder::kMostRecentFirst;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 12});
  ASSERT_TRUE(coll.registered_ok);

  // Defer inline execution using `next_wakeup_time_` while queuing candidates across all 3 classes.
  next_wakeup_time_ = async::Now(dispatcher());

  // Advance `frames_composed_` to 2 with placeholder composed frames so we can test off-screen
  // stamps.
  auto seed1 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
  auto seed2 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
  auto seed2_wait = importer_->ImportImage(
      MakeMetadata(coll.id, 2), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ImportedImage* s1_arr[] = {seed1.get()};
  ImportedImage* s2_arr[] = {seed2.get(), seed2_wait.get()};
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = s1_arr});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = s2_arr});
  // `seed1` now has Class 2 stamp 1 (`< frames_composed_`), `seed2` and `seed2_wait` have stamp 2.

  // Stamp `occluded_opt` at frame 3 (`2 + 1`), then compose frame 3 without `occluded_opt` so its
  // `kNamedByPresent` stamp passively expires to Class 2 (`3 <= frames_composed_`).
  auto occluded_opt = importer_->ImportImage(
      MakeMetadata(coll.id, 3), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* occ_arr[] = {occluded_opt.get()};
  (void)importer_->SignalWhenReady(occ_arr);

  // `waiting_old` stamped at frame 3 stays in Class 1 even after frame 3 composes because
  // `kWaitForImports` presents are blocked in `FenceQueue` until their imports finish.
  auto waiting_old = importer_->ImportImage(
      MakeMetadata(coll.id, 4), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ImportedImage* w_old_arr[] = {waiting_old.get()};
  (void)importer_->SignalWhenReady(w_old_arr);

  // Compose frame 3 with `seed2` and `seed2_wait` (`frames_composed_` becomes 3).
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = s2_arr});

  // Now at `frames_composed_ == 3`, stamp `waiting_new`, `active_opt`, and `active_opt2` for
  // frame 4.
  auto waiting_new = importer_->ImportImage(
      MakeMetadata(coll.id, 5), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ImportedImage* w_new_arr[] = {waiting_new.get()};
  (void)importer_->SignalWhenReady(w_new_arr);

  auto active_opt = importer_->ImportImage(
      MakeMetadata(coll.id, 6), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* act_arr[] = {active_opt.get()};
  (void)importer_->SignalWhenReady(act_arr);

  auto active_opt2 = importer_->ImportImage(
      MakeMetadata(coll.id, 7), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* act2_arr[] = {active_opt2.get()};
  (void)importer_->SignalWhenReady(act2_arr);

  // In Class 3: import `never_wait` before `never_named` (so its `image_id` is smaller).
  // `never_named` (kOptimistic) beats `never_wait` (kWaitForImports).
  auto never_wait = importer_->ImportImage(
      MakeMetadata(coll.id, 8), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  (void)never_wait;
  auto never_named = importer_->ImportImage(
      MakeMetadata(coll.id, 9), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  (void)never_named;

  renderer_->TakeCreatedImageOrder();
  next_wakeup_time_.reset();
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  RunLoopUntilIdle();

  const std::vector<uint64_t> expected_order = {
      // Class 1: un-aged `kOptimistic` in creation order (`active_opt`, `active_opt2`), then
      // `kWaitForImports` oldest-first.
      active_opt->id().value(),
      active_opt2->id().value(),
      waiting_old->id().value(),
      waiting_new->id().value(),
      // Class 2: current/future stamps (`>= frames_composed_`: `kOptimistic` ordered by
      // `image_id` (`seed2`, `occluded_opt`), then `kWaitForImports` (`seed2_wait`)), then
      // off-screen (`seed1`, frame 1).
      seed2->id().value(),
      occluded_opt->id().value(),
      seed2_wait->id().value(),
      seed1->id().value(),
      // Class 3: stamp 0 (`kOptimistic` before `kWaitForImports`: `never_named` beats
      // `never_wait`).
      never_named->id().value(),
      never_wait->id().value(),
  };
  EXPECT_EQ(renderer_->TakeCreatedImageOrder(), expected_order);
}

TEST_F(ImageImporterImplTest, OffScreenLeastRecentFirstOrdersOlderStampsFirst) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.worker_pool_size = 0;
  cfg.off_screen_order = OffScreenOrder::kLeastRecentFirst;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 5});
  ASSERT_TRUE(coll.registered_ok);

  next_wakeup_time_ = async::Now(dispatcher());
  auto f1 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                   reply_holder_, nullptr);
  auto f2 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                   reply_holder_, nullptr);
  auto f3 = importer_->ImportImage(MakeMetadata(coll.id, 2), ImageReadinessPolicy::kOptimistic,
                                   reply_holder_, nullptr);
  ImportedImage* a1[] = {f1.get()};
  ImportedImage* a2[] = {f2.get()};
  ImportedImage* a3[] = {f3.get()};
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = a1});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = a2});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = a3});

  next_wakeup_time_.reset();
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  RunLoopUntilIdle();

  // `f3` is current (`frame_num == frames_composed_ == 3`), then off-screen oldest first (`f1`,
  // `f2`).
  const std::vector<uint64_t> expected = {f3->id().value(), f1->id().value(), f2->id().value()};
  EXPECT_EQ(renderer_->TakeCreatedImageOrder(), expected);
}

TEST_F(ImageImporterImplTest, PredictedKindFirstDefersOtherKindAndHonorsPresentGpuOnlyHint) {
  ImageImporterImpl::Config cfg;
  cfg.import_kinds = ImportKinds::kPredictedKindFirst;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto disp_coll = RegisterCollection(CollectionSpec{.buffer_count = 6});
  auto gpu_coll =
      RegisterCollection(CollectionSpec{.display_branch_succeeds = false, .buffer_count = 4});
  ASSERT_TRUE(disp_coll.registered_ok);
  ASSERT_TRUE(gpu_coll.registered_ok);

  // Hold coordinator `ImportImage()` so we can verify the GPU worker stays deferred while display
  // is the predicted kind and a display job is in flight.
  libsync::Completion disp_started;
  libsync::Completion allow_disp;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        disp_started.Signal();
        allow_disp.Wait();
      });

  auto img1 = importer_->ImportImage(MakeMetadata(disp_coll.id, 0),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntil([&] { return disp_started.signaled(); });
  RunLoopUntilIdle();
  EXPECT_TRUE(ImageImporterImplTestPeer::GpuJobsDeferred(*importer_));
  EXPECT_EQ(img1->gpu_state(), ImportState::kQueued);
  EXPECT_EQ(renderer_->create_gpu_image_calls(), 0u);

  allow_disp.Signal();
  RunLoopUntil([&] {
    return img1->display_state() == ImportState::kReady && img1->gpu_state() == ImportState::kReady;
  });
  mock_coordinator_->set_import_image_fn(nullptr);

  // Import a GPU-only image and name it in `SignalWhenReady()` alongside a new `kOptimistic`
  // display-capable image while deferring main's idle-window display `ImportImage()` calls via
  // `next_wakeup_time_`. Because both images already satisfy readiness (`gpu_only_img` is `kReady`
  // and `img2` is `kOptimistic` on an allocated collection), `SignalWhenReady()` latches
  // `present_named_gpu_only_` immediately so the worker imports `img2`'s GPU side in Class 1.
  auto gpu_only_img = importer_->ImportImage(
      MakeMetadata(gpu_coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] { return gpu_only_img->gpu_state() == ImportState::kReady; });

  next_wakeup_time_ = async::Now(dispatcher());
  auto img2 = importer_->ImportImage(MakeMetadata(disp_coll.id, 1),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* present_imgs[] = {gpu_only_img.get(), img2.get()};
  (void)importer_->SignalWhenReady(present_imgs);

  RunLoopUntil([&] { return img2->gpu_state() == ImportState::kReady; });
  EXPECT_EQ(img2->display_state(), ImportState::kQueued);

  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kGpuOnlyImage, .images = present_imgs});
  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "predictions_from_present_hint"), 1u);
    EXPECT_EQ(GetInspectUint(*node, "predictions_from_present_hint_correct"), 1u);
  }

  // Reset `last_outcome_` to `kDirect`, then verify that releasing the display-capable image that
  // caused `gpu_jobs_deferred_ == true` unblocks a deferred GPU job without needing another frame
  // or `SignalWhenReady()`.
  next_wakeup_time_.reset();
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  RunLoopUntil([&] { return img2->display_state() == ImportState::kReady; });
  ImportedImage* direct_imgs[] = {img1.get()};
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kDirect, .images = direct_imgs});

  next_wakeup_time_ = async::Now(dispatcher());
  auto deferring_disp = importer_->ImportImage(
      MakeMetadata(disp_coll.id, 2), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_TRUE(ImageImporterImplTestPeer::GpuJobsDeferred(*importer_));

  auto deferred_gpu = importer_->ImportImage(
      MakeMetadata(gpu_coll.id, 1), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(deferred_gpu->gpu_state(), ImportState::kQueued);

  // `deferred_gpu` reaching `kReady` shows the release unblocked it. Do not check
  // `GpuJobsDeferred()` afterwards: with the GPU queue empty, `RefreshGpuCanStartLocked()` sets it
  // back to true (on the worker right after the import completes, and again in main-thread
  // cleanup), racing with any check here.
  importer_->ReleaseImage(deferring_disp);
  RunLoopUntil([&] { return deferred_gpu->gpu_state() == ImportState::kReady; });

  // Readiness latch on a waiting mixed present: `pre_gpu_only` (`kWaitForImports`) on an
  // unallocated GPU-only collection does NOT set `present_named_gpu_only_` while its import is
  // still unfinished, even after allocation completes; once its GPU import finishes and the
  // present becomes ready, the hint latches and wakes GPU work.
  auto unalloc_gpu_coll = RegisterCollection(
      CollectionSpec{.auto_allocate = false, .display_branch_succeeds = false, .buffer_count = 1});
  ASSERT_TRUE(unalloc_gpu_coll.registered_ok);

  auto pre_gpu_only =
      importer_->ImportImage(MakeMetadata(unalloc_gpu_coll.id, 0),
                             ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto img3 = importer_->ImportImage(MakeMetadata(disp_coll.id, 3),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* pre_present_imgs[] = {pre_gpu_only.get(), img3.get()};
  zx::event pre_ev = importer_->SignalWhenReady(pre_present_imgs);
  ASSERT_TRUE(pre_ev.is_valid());
  RunLoopUntilIdle();
  EXPECT_FALSE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));
  EXPECT_EQ(img3->gpu_state(), ImportState::kQueued);

  fake_sysmem_->CompleteAllocation(unalloc_gpu_coll.sysmem_index);
  RunLoopUntilIdle();
  // While `img3`'s Class 1 display job is deferred by `next_wakeup_time_`, `pre_gpu_only`'s GPU
  // import has not run yet, so the present is still waiting and has not flipped prediction.
  EXPECT_FALSE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));

  next_wakeup_time_.reset();
  importer_->OnCpuWorkDone();
  RunLoopUntil([&] {
    return pre_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK &&
           img3->gpu_state() == ImportState::kReady;
  });
  EXPECT_TRUE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));

  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kGpuOnlyImage, .images = pre_present_imgs});
  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "predictions_from_present_hint"), 2u);
    EXPECT_EQ(GetInspectUint(*node, "predictions_from_present_hint_correct"), 2u);
  }

  // Multi-session subcase (A18): a fence-blocked `kWaitForImports` Session B naming a `kGpuOnly`
  // image neither demotes Session A's `kOptimistic` display imports during Frame N nor loses its
  // GPU-only hint when Session A commits Frame N direct-to-display.
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kDirect, .images = direct_imgs});
  ASSERT_FALSE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));

  // Gate `blocker_disp` in flight so `session_b_gpu_img` stays `kQueued` (`gpu_jobs_running_ == 0`)
  // when Session B calls `SignalWhenReady()`.
  libsync::Completion blocker_started;
  libsync::Completion allow_blocker;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        if (!blocker_started.signaled()) {
          blocker_started.Signal();
          allow_blocker.Wait();
        }
      });
  auto blocker_disp = importer_->ImportImage(
      MakeMetadata(disp_coll.id, 4), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntil([&] { return blocker_started.signaled(); });

  libsync::Completion b_worker_entered;
  libsync::Completion allow_b_worker;
  renderer_->SetCreateGpuImageHook(
      [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
        if (meta.collection_id == gpu_coll.id && meta.vmo_index == 2) {
          b_worker_entered.Signal();
          allow_b_worker.Wait();
        }
        return r.MakeTrackedImage(meta.identifier);
      });

  auto session_b_gpu_img = importer_->ImportImage(
      MakeMetadata(gpu_coll.id, 2), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  ImportedImage* session_b_imgs[] = {session_b_gpu_img.get()};
  zx::event session_b_ev = importer_->SignalWhenReady(session_b_imgs);
  ASSERT_TRUE(session_b_ev.is_valid());
  EXPECT_FALSE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));

  // Session A (`kOptimistic`, `kDisplayCapable`) presents: its display import stays Class 1
  // because `PredictedNextKind()` is still `kDisplay`, so releasing `blocker_disp` dispatches
  // Session A's display import ahead of Session B's GPU import.
  auto session_a_disp_img = importer_->ImportImage(
      MakeMetadata(disp_coll.id, 5), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* session_a_imgs[] = {session_a_disp_img.get()};
  EXPECT_FALSE(importer_->SignalWhenReady(session_a_imgs).is_valid());
  EXPECT_FALSE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));

  allow_blocker.Signal();
  RunLoopUntil([&] { return session_a_disp_img->display_state() == ImportState::kReady; });
  mock_coordinator_->set_import_image_fn(nullptr);

  // Session A commits Frame N direct-to-display while Session B's GPU import is still blocked on
  // the worker; Frame N neither counts a mispredicted hint nor consumes Session B's hint.
  b_worker_entered.Wait();
  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kDirect, .images = session_a_imgs});
  EXPECT_EQ(GetInspectUint(*ReadInspectRoot().GetByPath({"image_import"}),
                           "predictions_from_present_hint"),
            2u);

  // Completing Session B's GPU import unblocks its present and latches `present_named_gpu_only_`
  // for Frame N+1.
  renderer_->SetCreateGpuImageHook(nullptr);
  allow_b_worker.Signal();
  RunLoopUntil([&] {
    return session_b_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK;
  });
  EXPECT_TRUE(ImageImporterImplTestPeer::PredictedNextKindIsGpu(*importer_));
}

TEST_F(ImageImporterImplTest, HigherPriorityOtherKindDrainsInFlightInsteadOfPoppingSameKind) {
  // 1. `worker_pool_size = 2`: two workers busy on Class 3 GPU jobs drain to 0 instead of popping a
  // third Class 3 GPU job once a Class 1 display job arrives.
  {
    ImageImporterImpl::Config cfg;
    cfg.worker_pool_size = 2;
    cfg.import_kinds = ImportKinds::kPredictedKindFirst;
    cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cfg);

    auto gpu_coll =
        RegisterCollection(CollectionSpec{.display_branch_succeeds = false, .buffer_count = 4});
    auto disp_coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(gpu_coll.registered_ok);
    ASSERT_TRUE(disp_coll.registered_ok);

    libsync::Completion g1_started;
    libsync::Completion g2_started;
    libsync::Completion release_g1;
    libsync::Completion release_g2;
    libsync::Completion disp_imported;
    renderer_->SetCreateGpuImageHook(
        [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          // Block by image, not by arrival order: either worker may reach this hook first.
          if (meta.collection_id == gpu_coll.id && meta.vmo_index == 0) {
            g1_started.Signal();
            release_g1.Wait();
          } else if (meta.collection_id == gpu_coll.id && meta.vmo_index == 1) {
            g2_started.Signal();
            release_g2.Wait();
          }
          return r.MakeTrackedImage(meta.identifier);
        });
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          disp_imported.Signal();
        });

    auto g1 = importer_->ImportImage(MakeMetadata(gpu_coll.id, 0),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    auto g2 = importer_->ImportImage(MakeMetadata(gpu_coll.id, 1),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    g1_started.Wait();
    g2_started.Wait();
    auto g3 = importer_->ImportImage(MakeMetadata(gpu_coll.id, 2),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    ASSERT_EQ(g3->gpu_state(), ImportState::kQueued);

    auto d1 = importer_->ImportImage(MakeMetadata(disp_coll.id, 0),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    ImportedImage* d1_arr[] = {d1.get()};
    (void)importer_->SignalWhenReady(d1_arr);
    RunLoopUntilIdle();
    EXPECT_EQ(d1->display_state(), ImportState::kQueued);

    // `g1` finishes: `g3` must remain queued while `g2` is still running.
    release_g1.Signal();
    RunLoopUntil([&] { return g1->gpu_state() == ImportState::kReady; });
    RunLoopUntilIdle();
    EXPECT_EQ(g3->gpu_state(), ImportState::kQueued);
    EXPECT_EQ(d1->display_state(), ImportState::kQueued);

    // Hold `g3` if it starts after `d1` so we can verify `d1` ran before `g3`.
    libsync::Completion allow_g3;
    renderer_->SetCreateGpuImageHook(
        [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          allow_g3.Wait();
          return r.MakeTrackedImage(meta.identifier);
        });

    release_g2.Signal();
    RunLoopUntil(
        [&] { return disp_imported.signaled() && d1->display_state() == ImportState::kReady; });
    EXPECT_NE(g3->gpu_state(), ImportState::kReady);

    allow_g3.Signal();
    RunLoopUntil([&] { return g3->gpu_state() == ImportState::kReady; });
    // Join the GPU workers before this block's completions go out of scope: the hook is not
    // limited to `g3`, so `d1`'s GPU import can still be returning from `allow_g3.Wait()`.
    importer_->StopOnMain();
    renderer_->SetCreateGpuImageHook(nullptr);
    mock_coordinator_->set_import_image_fn(nullptr);
  }

  // 2. Symmetric case (`display_import_cap = 2`): two Class 3 display imports in flight drain to 0
  // instead of starting a third Class 3 display import once a Class 1 GPU-only present arrives.
  {
    ImageImporterImpl::Config cfg;
    cfg.display_import_cap = 2;
    cfg.import_kinds = ImportKinds::kPredictedKindFirst;
    cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cfg);

    auto disp_coll = RegisterCollection(CollectionSpec{.buffer_count = 4});
    auto gpu_coll =
        RegisterCollection(CollectionSpec{.display_branch_succeeds = false, .buffer_count = 2});
    ASSERT_TRUE(disp_coll.registered_ok);
    ASSERT_TRUE(gpu_coll.registered_ok);

    std::atomic<uint32_t> disp_entered{0};
    libsync::Completion release_disp_1;
    libsync::Completion release_disp_2;
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          const uint32_t idx = disp_entered.fetch_add(1) + 1;
          if (idx == 1) {
            release_disp_1.Wait();
          } else if (idx == 2) {
            release_disp_2.Wait();
          }
        });

    auto d1 = importer_->ImportImage(MakeMetadata(disp_coll.id, 0),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    auto d2 = importer_->ImportImage(MakeMetadata(disp_coll.id, 1),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    RunLoopUntil([&] {
      return d1->display_state() == ImportState::kImporting &&
             d2->display_state() == ImportState::kImporting;
    });
    auto d3 = importer_->ImportImage(MakeMetadata(disp_coll.id, 2),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    RunLoopUntilIdle();
    ASSERT_EQ(d3->display_state(), ImportState::kQueued);

    // `kOptimistic` GPU-only image is already allocation-ready, so `SignalWhenReady()` latches the
    // hint and stamps `g1` into Class 1.
    libsync::Completion g1_started;
    libsync::Completion allow_g1;
    renderer_->SetCreateGpuImageHook(
        [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          if (meta.collection_id == gpu_coll.id) {
            g1_started.Signal();
            allow_g1.Wait();
          }
          return r.MakeTrackedImage(meta.identifier);
        });
    auto g1 = importer_->ImportImage(MakeMetadata(gpu_coll.id, 0),
                                     ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    ImportedImage* g1_arr[] = {g1.get()};
    EXPECT_FALSE(importer_->SignalWhenReady(g1_arr).is_valid());
    EXPECT_EQ(g1->gpu_state(), ImportState::kQueued);

    // First display import finishes (`in_flight` drops from 2 to 1): `d3` must stay queued.
    release_disp_1.Signal();
    RunLoopUntil([&] { return d1->display_state() == ImportState::kReady; });
    RunLoopUntilIdle();
    EXPECT_EQ(d3->display_state(), ImportState::kQueued);
    EXPECT_FALSE(g1_started.signaled());

    // Second display import finishes (`in_flight` drops to 0): `g1` starts before `d3`.
    release_disp_2.Signal();
    RunLoopUntil([&] { return g1_started.signaled(); });
    EXPECT_EQ(d3->display_state(), ImportState::kQueued);

    allow_g1.Signal();
    RunLoopUntil([&] {
      return g1->gpu_state() == ImportState::kReady && d3->display_state() == ImportState::kReady;
    });
    // Join the GPU workers before this block's locals go out of scope: GPU imports of `d1`..`d3`
    // can still be running the hook, which reads `gpu_coll` and the completions above.
    importer_->StopOnMain();
    renderer_->SetCreateGpuImageHook(nullptr);
    mock_coordinator_->set_import_image_fn(nullptr);
  }
}

TEST_F(ImageImporterImplTest, PostCommitDeferralDisplayedTimeoutAndSkippedFrame) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 5});
  ASSERT_TRUE(coll.registered_ok);

  // Commit a direct frame with stamp 10 under a long deadline; the post-commit deferral defers
  // idle-window display `ImportImage()` calls deterministically until `OnVsync()` reaches stamp 10.
  auto seed = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                     reply_holder_, nullptr);
  RunLoopUntil([&] { return seed->display_state() == ImportState::kReady; });
  ImportedImage* seed_arr[] = {seed.get()};

  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{10},
      .target_presentation_time = async::Now(dispatcher()) + zx::sec(10),
      .vsync_interval = zx::sec(10),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();

  const uint32_t imports_before = mock_coordinator_->import_image_count();
  auto deferred_until_vsync = importer_->ImportImage(
      MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(mock_coordinator_->import_image_count(), imports_before);
  EXPECT_EQ(deferred_until_vsync->display_state(), ImportState::kQueued);

  // Vsync with older stamp 9 keeps deferring; stamp 10 ends the deferral and starts the call.
  importer_->OnVsync(display::WireConfigStamp{9}, async::Now(dispatcher()));
  RunLoopUntilIdle();
  EXPECT_EQ(mock_coordinator_->import_image_count(), imports_before);

  importer_->OnVsync(display::WireConfigStamp{10}, async::Now(dispatcher()));
  RunLoopUntil([&] { return deferred_until_vsync->display_state() == ImportState::kReady; });

  // First defer `deferred_until_timeout` under a long deadline and verify `RunLoopUntilIdle()`
  // leaves it `kQueued`. Then arm a short deadline with `deferred_until_timeout` already queued and
  // no `OnVsync()` so only the deferral's timeout task can end it.
  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{11},
      .target_presentation_time = async::Now(dispatcher()) + zx::sec(10),
      .vsync_interval = zx::sec(10),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();

  auto deferred_until_timeout = importer_->ImportImage(
      MakeMetadata(coll.id, 2), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(deferred_until_timeout->display_state(), ImportState::kQueued);

  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{12},
      .target_presentation_time = async::Now(dispatcher()) + zx::msec(5),
      .vsync_interval = zx::msec(5),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  RunLoopUntil([&] { return deferred_until_timeout->display_state() == ImportState::kReady; });
  // `deferred_until_timeout`'s GPU job becomes eligible once its display import finishes. Wait for
  // the worker to finish it: with `kPredictedKindFirst`, a GPU job still running would hold back
  // the display import that the expired deadline below must start.
  RunLoopUntil([&] {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    return deferred_until_timeout->gpu_state() == ImportState::kReady && node != nullptr &&
           GetInspectUint(*node, "in_flight_gpu_jobs") == 0u;
  });

  // Lazy deadline expiry check inside `IsDeferred()` when `RunDispatcher()` runs: when
  // `OnFrameCommitted()` sets an already-expired deadline (`deadline <= async::Now()`), the posted
  // `RunDispatcher()` task starts the deferred display import immediately without waiting for the
  // deferral's timeout task.
  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{12},
      .target_presentation_time = async::Now(dispatcher()) + zx::sec(10),
      .vsync_interval = zx::sec(10),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();

  auto deferred_until_inline_expiry = importer_->ImportImage(
      MakeMetadata(coll.id, 4), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(deferred_until_inline_expiry->display_state(), ImportState::kQueued);

  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{13},
      .target_presentation_time = async::Now(dispatcher()) - zx::msec(10),
      .vsync_interval = zx::msec(5),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  EXPECT_NE(deferred_until_inline_expiry->display_state(), ImportState::kQueued);
  RunLoopUntil(
      [&] { return deferred_until_inline_expiry->display_state() == ImportState::kReady; });

  // Commit stamp 13, then run a skipped frame (`OnCpuWorkDone()` with no preceding
  // `OnFrameCommitted()`), verifying the deferral ends immediately.
  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = seed_arr,
      .config_stamp = display::WireConfigStamp{13},
      .target_presentation_time = async::Now(dispatcher()) + zx::sec(10),
      .vsync_interval = zx::sec(10),
  });
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();

  auto deferred_until_skip = importer_->ImportImage(
      MakeMetadata(coll.id, 3), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(deferred_until_skip->display_state(), ImportState::kQueued);

  // Skipped frame ends the deferral immediately without waiting 20s for the timeout task.
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  RunLoopUntil([&] { return deferred_until_skip->display_state() == ImportState::kReady; });

  const auto root = ReadInspectRoot();
  const auto* node = root.GetByPath({"image_import"});
  ASSERT_NE(node, nullptr);
  EXPECT_EQ(GetInspectUint(*node, "frames_skipped"), 1u);
}

TEST_F(ImageImporterImplTest, DispatcherFitMathAndDisplayImportCap) {
  // 1. `display_import_cap = 1` vs `display_import_cap = 2` with coordinator `ImportImage()` gated.
  for (const uint32_t cap : {1u, 2u}) {
    SCOPED_TRACE(::testing::Message() << "cap=" << cap);
    ImageImporterImpl::Config cap_cfg;
    cap_cfg.display_import_cap = cap;
    cap_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cap_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 3});
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion first_entered;
    libsync::Completion release_imports;
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          first_entered.Signal();
          release_imports.Wait();
        });

    next_wakeup_from_now_ = zx::msec(1);
    auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    RunLoopUntilIdle();
    ASSERT_EQ(img1->display_state(), ImportState::kQueued);
    ASSERT_EQ(img2->display_state(), ImportState::kQueued);

    next_wakeup_from_now_.reset();
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    RunLoopUntil([&] { return first_entered.signaled(); });
    RunLoopUntilIdle();

    const uint32_t in_flight_imgs = (img1->display_state() == ImportState::kImporting ? 1u : 0u) +
                                    (img2->display_state() == ImportState::kImporting ? 1u : 0u);
    EXPECT_EQ(in_flight_imgs, cap);

    release_imports.Signal();
    RunLoopUntil([&] {
      return img1->display_state() == ImportState::kReady &&
             img2->display_state() == ImportState::kReady;
    });
    mock_coordinator_->set_import_image_fn(nullptr);
  }

  // 2. Display fit math: `+ 3100 us` (inside 3 ms seed + 250 us safety margin = 3250 us) defers
  // the job; `+ 4 ms` dispatches it while a wakeup is armed.
  {
    ImageImporterImpl::Config disp_cfg;
    disp_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(disp_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_from_now_ = zx::usec(3100);
    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
    RunLoopUntilIdle();
    EXPECT_EQ(img->display_state(), ImportState::kQueued);

    next_wakeup_from_now_ = zx::sec(1);
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    RunLoopUntil([&] { return img->display_state() == ImportState::kReady; });
  }

  // 3. Inline GPU fit math (`worker_pool_size = 0`): `+ 3100 us` defers; `+ 4 ms` dispatches.
  {
    ImageImporterImpl::Config inline_cfg;
    inline_cfg.enable_direct_to_display = false;
    inline_cfg.worker_pool_size = 0;
    inline_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(inline_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_from_now_ = zx::usec(3100);
    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
    RunLoopUntilIdle();
    EXPECT_EQ(img->gpu_state(), ImportState::kQueued);

    next_wakeup_from_now_ = zx::sec(1);
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    RunLoopUntil([&] { return img->gpu_state() == ImportState::kReady; });
  }

  // 4. Multi-import fit math: with `display_import_cap = 2` and 1 in flight (`outstanding == 1`),
  // required budget is `(outstanding + 1) * 3 ms + 250 us = 6250 us`.
  // `next_wakeup_from_now_ = 5 ms` defers the second import; 7 ms dispatches it.
  {
    ImageImporterImpl::Config cap_cfg;
    cap_cfg.display_import_cap = 2;
    cap_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(cap_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 3});
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion first_entered;
    libsync::Completion release_first;
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          first_entered.Signal();
          release_first.Wait();
        });

    next_wakeup_from_now_.reset();
    auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    RunLoopUntil([&] { return first_entered.signaled(); });
    EXPECT_EQ(img1->display_state(), ImportState::kImporting);

    next_wakeup_from_now_ = zx::msec(5);
    auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                       reply_holder_, nullptr);
    RunLoopUntilIdle();
    EXPECT_EQ(img2->display_state(), ImportState::kQueued);

    next_wakeup_from_now_ = zx::sec(1);
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    RunLoopUntilIdle();
    EXPECT_EQ(img2->display_state(), ImportState::kImporting);

    release_first.Signal();
    RunLoopUntil([&] {
      return img1->display_state() == ImportState::kReady &&
             img2->display_state() == ImportState::kReady;
    });
    mock_coordinator_->set_import_image_fn(nullptr);
  }

  // 5. `OnCpuWorkDone()` posts `RunDispatcher()` instead of running it inline, so a task posted
  // before `OnCpuWorkDone()` runs first and moves `next_wakeup_` tight enough to defer the job.
  {
    ImageImporterImpl::Config post_cfg;
    post_cfg.enable_direct_to_display = false;
    post_cfg.worker_pool_size = 0;
    post_cfg.never_shown_imports = NeverShownImports::kNever;
    CreateImporter(post_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 1});
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_from_now_.reset();
    next_wakeup_time_.reset();
    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
    // Stamp `img` so it is Class 1, while deferring its initial `SignalWhenReady()` dispatch.
    next_wakeup_from_now_ = zx::usec(100);
    ImportedImage* arr[] = {img.get()};
    (void)importer_->SignalWhenReady(arr);
    RunLoopUntilIdle();
    ASSERT_EQ(img->gpu_state(), ImportState::kQueued);

    // Clear wakeup, post a task that tightens `next_wakeup_from_now_` before `OnCpuWorkDone()` is
    // called, then call `OnCpuWorkDone()`.
    next_wakeup_from_now_.reset();
    async::PostTask(dispatcher(), [this] { next_wakeup_from_now_ = zx::usec(500); });
    importer_->OnCpuWorkDone();
    RunLoopUntilIdle();
    EXPECT_EQ(img->gpu_state(), ImportState::kQueued);
    next_wakeup_from_now_.reset();
  }

  // 6. `wakeup_retry_task_`: a `kWaitForImports` display import or inline GPU import deferred only
  // because `fits == false` is retried when `*wakeup` passes even if `next_wakeup_` clears without
  // any `OnCpuWorkDone()` call.
  {
    ImageImporterImpl::Config retry_disp_cfg;
    retry_disp_cfg.never_shown_imports = NeverShownImports::kNever;
    CreateImporter(retry_disp_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 1});
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_time_ = async::Now(dispatcher()) + zx::msec(2);
    auto img = importer_->ImportImage(
        MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    ImportedImage* arr[] = {img.get()};
    zx::event ev = importer_->SignalWhenReady(arr);
    ASSERT_TRUE(ev.is_valid());
    RunLoopUntilIdle();
    ASSERT_EQ(img->display_state(), ImportState::kQueued);

    // Clear `next_wakeup_time_` with no `OnCpuWorkDone()` call; advancing past the original
    // `*wakeup` triggers `wakeup_retry_task_` and completes the import.
    next_wakeup_time_.reset();
    RunLoopUntil([&] {
      return ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK;
    });
    EXPECT_EQ(img->display_state(), ImportState::kReady);
  }
  {
    ImageImporterImpl::Config retry_gpu_cfg;
    retry_gpu_cfg.enable_direct_to_display = false;
    retry_gpu_cfg.worker_pool_size = 0;
    retry_gpu_cfg.never_shown_imports = NeverShownImports::kNever;
    CreateImporter(retry_gpu_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 1});
    ASSERT_TRUE(coll.registered_ok);

    next_wakeup_time_ = async::Now(dispatcher()) + zx::msec(2);
    auto img = importer_->ImportImage(
        MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
    ImportedImage* arr[] = {img.get()};
    zx::event ev = importer_->SignalWhenReady(arr);
    ASSERT_TRUE(ev.is_valid());
    RunLoopUntilIdle();
    ASSERT_EQ(img->gpu_state(), ImportState::kQueued);

    next_wakeup_time_.reset();
    RunLoopUntil([&] {
      return ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK;
    });
    EXPECT_EQ(img->gpu_state(), ImportState::kReady);
  }

  // 7. Cold (`first_gpu_import_duration_`) vs cached (`later_gpu_import_duration_`) predictors:
  // after a slow first GPU import and fast later GPU imports on collection A, an intermediate
  // wakeup budget dispatches a later image on collection A while deferring the first image on a
  // newly registered collection B.
  {
    ImageImporterImpl::Config split_cfg;
    split_cfg.enable_direct_to_display = false;
    split_cfg.worker_pool_size = 0;
    split_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(split_cfg);

    auto coll_a = RegisterCollection(CollectionSpec{.buffer_count = 6});
    ASSERT_TRUE(coll_a.registered_ok);

    renderer_->SetCreateGpuImageHook(
        [](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          if (meta.vmo_index == 0) {
            zx::nanosleep(zx::deadline_after(zx::msec(6)));
          }
          return r.MakeTrackedImage(meta.identifier);
        });

    next_wakeup_time_.reset();
    next_wakeup_from_now_.reset();
    for (uint32_t i = 0; i < 4; ++i) {
      auto sample = importer_->ImportImage(
          MakeMetadata(coll_a.id, i), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
      RunLoopUntil([&] { return sample->gpu_state() == ImportState::kReady; });
    }
    renderer_->SetCreateGpuImageHook(nullptr);

    EXPECT_GE(ImageImporterImplTestPeer::FirstGpuImportDurationPrediction(*importer_), zx::msec(5));
    EXPECT_LT(ImageImporterImplTestPeer::LaterGpuImportDurationPrediction(*importer_), zx::msec(1));

    auto coll_b = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll_b.registered_ok);

    // Budget of 2 ms fits `LaterGpuImportDurationPrediction() + 250 us` (< 1.25 ms) on `coll_a`,
    // but does not fit `FirstGpuImportDurationPrediction() + 250 us` (> 5.25 ms) on `coll_b`.
    next_wakeup_from_now_ = zx::msec(2);
    auto first_b = importer_->ImportImage(
        MakeMetadata(coll_b.id, 0), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    RunLoopUntilIdle();
    EXPECT_EQ(first_b->gpu_state(), ImportState::kQueued);

    auto later_a = importer_->ImportImage(
        MakeMetadata(coll_a.id, 4), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    // Stamp `later_a` ahead of `first_b` so `FindBestEligibleJobLocked()` picks `later_a`.
    ImportedImage* later_arr[] = {later_a.get()};
    (void)importer_->SignalWhenReady(later_arr);
    RunLoopUntilIdle();
    EXPECT_EQ(later_a->gpu_state(), ImportState::kReady);
    EXPECT_EQ(first_b->gpu_state(), ImportState::kQueued);
    next_wakeup_from_now_.reset();
  }
}

TEST_F(ImageImporterImplTest, FrameWaitContentionOnWorkerAndDisplayInFlightAndFailure) {
  // 1. `PrepareForGpuComposition()` contends on `gpu_run_mutex_` while a worker holds the import.
  {
    ImageImporterImpl::Config gpu_cfg;
    gpu_cfg.enable_direct_to_display = false;
    gpu_cfg.worker_pool_size = 1;
    gpu_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(gpu_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion worker_entered;
    libsync::Completion allow_worker_finish;
    renderer_->SetCreateGpuImageHook(
        [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          worker_entered.Signal();
          allow_worker_finish.Wait();
          return r.MakeTrackedImage(meta.identifier);
        });

    auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                      reply_holder_, nullptr);
    worker_entered.Wait();

    zx::thread main_thread;
    ASSERT_EQ(zx::thread::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &main_thread), ZX_OK);
    // Returns once `main_thread` has been blocked on a futex for two polls about 1 ms apart, so a
    // brief futex wait elsewhere (such as in thread creation) is not mistaken for main waiting on
    // `gpu_run_mutex_`.
    auto wait_until_main_blocked = [&main_thread] {
      const zx::time deadline = zx::deadline_after(zx::sec(2));
      int consecutive_blocked = 0;
      while (consecutive_blocked < 2 && zx::clock::get_monotonic() < deadline) {
        zx_info_thread_t info{};
        const bool blocked =
            main_thread.get_info(ZX_INFO_THREAD, &info, sizeof(info), nullptr, nullptr) == ZX_OK &&
            info.state == ZX_THREAD_STATE_BLOCKED_FUTEX;
        consecutive_blocked = blocked ? consecutive_blocked + 1 : 0;
        if (consecutive_blocked < 2) {
          zx::nanosleep(zx::deadline_after(blocked ? zx::msec(1) : zx::usec(50)));
        }
      }
    };
    std::thread releaser([&] {
      wait_until_main_blocked();
      zx::nanosleep(zx::deadline_after(zx::usec(100)));
      allow_worker_finish.Signal();
    });
    importer_->BeginFrame();
    ImportedImage* arr[] = {img.get()};
    importer_->PrepareForGpuComposition(arr);
    releaser.join();
    importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = arr});

    EXPECT_EQ(img->gpu_state(), ImportState::kReady);
    const auto gpu_root = ReadInspectRoot();
    const auto* gpu_node = gpu_root.GetByPath({"image_import"});
    ASSERT_NE(gpu_node, nullptr);
    EXPECT_GT(GetInspectUint(*gpu_node, "prepare_for_gpu_composition_ns"), 0u);
    EXPECT_GE(GetInspectHistogramCount(*gpu_node, "image_wait_gpu_us"), 1u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*gpu_node, "frame_blocked_gpu_us"), 1u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*gpu_node, "frame_blocked_display_sent_us"), 0u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*gpu_node, "frame_blocked_display_in_flight_us"), 0u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*gpu_node, "frame_blocked_total_us"), 1u);

    // Aborted-frame reset (`BeginFrame()`): accumulate GPU wait on a second image, then call
    // `BeginFrame()` before committing an unblocked frame, verifying `frame_blocked_total_us`
    // records 0 (no second non-zero sample).
    libsync::Completion worker2_entered;
    libsync::Completion allow_worker2_finish;
    renderer_->SetCreateGpuImageHook(
        [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
          worker2_entered.Signal();
          allow_worker2_finish.Wait();
          return r.MakeTrackedImage(meta.identifier);
        });
    auto img_aborted = importer_->ImportImage(
        MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    worker2_entered.Wait();
    std::thread releaser2([&] {
      wait_until_main_blocked();
      zx::nanosleep(zx::deadline_after(zx::usec(100)));
      allow_worker2_finish.Signal();
    });
    importer_->BeginFrame();
    ImportedImage* aborted_arr[] = {img_aborted.get()};
    importer_->PrepareForGpuComposition(aborted_arr);
    releaser2.join();
    // Abort that frame and start a fresh frame that does not block.
    importer_->BeginFrame();
    importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuForced, .images = arr});
    const auto reset_root = ReadInspectRoot();
    const auto* reset_node = reset_root.GetByPath({"image_import"});
    ASSERT_NE(reset_node, nullptr);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*reset_node, "frame_blocked_total_us"), 1u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*reset_node, "frame_blocked_gpu_us"), 1u);

    renderer_->SetCreateGpuImageHook(nullptr);
  }

  // 2. `PrepareForDirectToDisplay()` waits on an idle-dispatched `kImporting` image and succeeds,
  // and returns `kDisplayImportFailed` (forgetting the ID) when a waited display import fails.
  {
    ImageImporterImpl::Config disp_cfg;
    disp_cfg.never_shown_imports = NeverShownImports::kWhenFree;
    CreateImporter(disp_cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 3});
    ASSERT_TRUE(coll.registered_ok);

    libsync::Completion disp_entered;
    libsync::Completion allow_disp;
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          disp_entered.Signal();
          allow_disp.Wait();
        });

    auto inflight_img = importer_->ImportImage(
        MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    RunLoopUntil([&] { return disp_entered.signaled(); });
    ASSERT_EQ(inflight_img->display_state(), ImportState::kImporting);

    std::thread ok_releaser([&] {
      zx::nanosleep(zx::deadline_after(zx::usec(200)));
      allow_disp.Signal();
    });
    importer_->BeginFrame();
    ImportedImage* ok_arr[] = {inflight_img.get()};
    EXPECT_EQ(importer_->PrepareForDirectToDisplay(ok_arr), FrameOutcome::kDirect);
    ok_releaser.join();
    EXPECT_EQ(inflight_img->display_state(), ImportState::kReady);
    importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kDirect, .images = ok_arr});
    {
      const auto inflight_root = ReadInspectRoot();
      const auto* inflight_node = inflight_root.GetByPath({"image_import"});
      ASSERT_NE(inflight_node, nullptr);
      EXPECT_EQ(
          GetInspectHistogramNonZeroCount(*inflight_node, "frame_blocked_display_in_flight_us"),
          1u);
      EXPECT_EQ(GetInspectHistogramNonZeroCount(*inflight_node, "frame_blocked_display_sent_us"),
                0u);
      EXPECT_EQ(GetInspectHistogramNonZeroCount(*inflight_node, "frame_blocked_gpu_us"), 0u);
    }
    mock_coordinator_->set_import_image_fn(nullptr);

    // Fresh importer so `coordinator_->current_image_count()` and histograms start at 0 before the
    // frame-sent (`sent_by_frame == true`) wait.
    CreateImporter(disp_cfg);
    auto fail_coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(fail_coll.registered_ok);

    libsync::Completion fail_entered;
    libsync::Completion allow_fail;
    mock_coordinator_->set_import_image_result(ZX_ERR_NOT_SUPPORTED);
    mock_coordinator_->set_import_image_fn(
        [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
          fail_entered.Signal();
          allow_fail.Wait();
        });

    next_wakeup_from_now_ = zx::msec(1);
    auto fail_img = importer_->ImportImage(
        MakeMetadata(fail_coll.id, 0), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
    RunLoopUntilIdle();
    ASSERT_EQ(fail_img->display_state(), ImportState::kQueued);

    std::thread fail_releaser([&] {
      fail_entered.Wait();
      zx::nanosleep(zx::deadline_after(zx::usec(200)));
      allow_fail.Signal();
    });
    importer_->BeginFrame();
    ImportedImage* fail_arr[] = {fail_img.get()};
    EXPECT_EQ(importer_->PrepareForDirectToDisplay(fail_arr), FrameOutcome::kDisplayImportFailed);
    fail_releaser.join();
    EXPECT_EQ(fail_img->display_state(), ImportState::kFailed);
    EXPECT_EQ(coordinator_->current_image_count(), 0u);
    importer_->OnFrameCommitted(
        FrameReport{.outcome = FrameOutcome::kDisplayImportFailed, .images = fail_arr});

    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "display_imports_failed"), 1u);
    EXPECT_GE(GetInspectHistogramCount(*node, "image_wait_display_us"), 1u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*node, "frame_blocked_display_sent_us"), 1u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*node, "frame_blocked_display_in_flight_us"), 0u);
    EXPECT_EQ(GetInspectHistogramNonZeroCount(*node, "frame_blocked_gpu_us"), 0u);

    mock_coordinator_->set_import_image_fn(nullptr);
    mock_coordinator_->set_import_image_result(ZX_OK);
  }
}

TEST_F(ImageImporterImplTest, CheckDisplayStatesCoversAllFallbackReasons) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto disp_coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
  auto reg_coll = RegisterCollection(CollectionSpec{.auto_allocate = false, .buffer_count = 2});
  auto gpu_only_coll =
      RegisterCollection(CollectionSpec{.display_branch_succeeds = false, .buffer_count = 2});
  ASSERT_TRUE(disp_coll.registered_ok);
  ASSERT_TRUE(reg_coll.registered_ok);
  ASSERT_TRUE(gpu_only_coll.registered_ok);

  auto disp_ref = importer_->ImportImage(
      MakeMetadata(disp_coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto gpu_only_ref =
      importer_->ImportImage(MakeMetadata(gpu_only_coll.id, 0),
                             ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] {
    return disp_ref->display_state() == ImportState::kReady &&
           gpu_only_ref->gpu_state() == ImportState::kReady;
  });

  auto make_synthetic = [&](std::shared_ptr<ImportedCollection> collection, ImportWord word) {
    allocation::ImageMetadata meta = MakeMetadata(collection ? collection->id() : 0);
    meta.identifier = allocation::GenerateUniqueImageId();
    auto img =
        std::make_unique<ImportedImage>(meta, ImageReadinessPolicy::kOptimistic, nullptr, nullptr);
    ImageImportTestPeer::SetCollection(*img, std::move(collection));
    ImageImportTestPeer::SetWord(*img, word);
    return img;
  };

  // 1. Display `kReleased` returns `kDisplayImportFailed`.
  auto synth_released =
      make_synthetic(disp_ref->collection(), ImportWord{
                                                 .display = ImportState::kReleased,
                                                 .gpu = ImportState::kReady,
                                                 .released = false,
                                                 .allocated = true,
                                             });
  ImportedImage* rel_arr[] = {synth_released.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(rel_arr), FrameOutcome::kDisplayImportFailed);

  // 2. Null `collection()` returns `kDisplayImportFailed`.
  auto synth_no_coll = make_synthetic(nullptr, ImportWord{
                                                   .display = ImportState::kQueued,
                                                   .gpu = ImportState::kQueued,
                                                   .released = false,
                                                   .allocated = false,
                                               });
  ImportedImage* no_coll_arr[] = {synth_no_coll.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(no_coll_arr), FrameOutcome::kDisplayImportFailed);

  // 3. Collection still in `kRegistered` returns `kDisplayImportFailed`.
  auto reg_img = importer_->ImportImage(MakeMetadata(reg_coll.id, 0),
                                        ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ASSERT_EQ(reg_img->collection()->state(), ImportedCollection::State::kRegistered);
  ImportedImage* reg_arr[] = {reg_img.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(reg_arr), FrameOutcome::kDisplayImportFailed);

  // 4. `kGpuOnly` collection with display `kQueued` or `kNotStarted` returns `kGpuOnlyImage`.
  auto synth_gpu_queued =
      make_synthetic(gpu_only_ref->collection(), ImportWord{
                                                     .display = ImportState::kQueued,
                                                     .gpu = ImportState::kReady,
                                                     .released = false,
                                                     .allocated = true,
                                                 });
  ImportedImage* gpu_queued_arr[] = {synth_gpu_queued.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(gpu_queued_arr), FrameOutcome::kGpuOnlyImage);

  ImageImportTestPeer::SetWord(*synth_gpu_queued, ImportWord{
                                                      .display = ImportState::kNotStarted,
                                                      .gpu = ImportState::kReady,
                                                      .released = false,
                                                      .allocated = true,
                                                  });
  EXPECT_EQ(importer_->CheckDisplayStates(gpu_queued_arr), FrameOutcome::kGpuOnlyImage);

  // 5. Mixed list where an earlier image fails and a later image belongs to a `kGpuOnly` collection
  // returns `kGpuOnlyImage`.
  ImportedImage* mixed_arr[] = {reg_img.get(), synth_gpu_queued.get()};
  EXPECT_EQ(importer_->CheckDisplayStates(mixed_arr), FrameOutcome::kGpuOnlyImage);

  ImageImportTestPeer::ClearForDestruction(*synth_released);
  ImageImportTestPeer::ClearForDestruction(*synth_no_coll);
  ImageImportTestPeer::ClearForDestruction(*synth_gpu_queued);
}

TEST_F(ImageImporterImplTest, NeverShownImportsHonorsWhenIdleAndNever) {
  // 1. `kWhenIdle`: stamp-0 jobs stay queued until `OnVsync()` arrives after `idle_after`.
  ImageImporterImpl::Config idle_cfg;
  idle_cfg.enable_direct_to_display = false;
  idle_cfg.never_shown_imports = NeverShownImports::kWhenIdle;
  idle_cfg.idle_after = zx::sec(10);
  CreateImporter(idle_cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 3});
  ASSERT_TRUE(coll.registered_ok);

  const zx::time t0 = async::Now(dispatcher());
  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                    reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(img->gpu_state(), ImportState::kQueued);

  // Vsync before `idle_after` does not mark idle.
  importer_->OnVsync(display::WireConfigStamp{1}, t0 + zx::sec(5));
  RunLoopUntilIdle();
  EXPECT_EQ(img->gpu_state(), ImportState::kQueued);

  // Vsync at `>= idle_after` wakes the worker for Class 3 jobs.
  importer_->OnVsync(display::WireConfigStamp{2}, t0 + zx::sec(10));
  RunLoopUntil([&] { return img->gpu_state() == ImportState::kReady; });

  // Composing a frame resets `idle_` to false.
  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kGpuForced,
  });
  auto img_after_frame = importer_->ImportImage(
      MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(img_after_frame->gpu_state(), ImportState::kQueued);

  // 2. `kNever`: stamp-0 jobs never run until named by a present.
  ImageImporterImpl::Config never_cfg;
  never_cfg.enable_direct_to_display = false;
  never_cfg.never_shown_imports = NeverShownImports::kNever;
  CreateImporter(never_cfg);

  auto coll2 = RegisterCollection(CollectionSpec{.buffer_count = 2});
  ASSERT_TRUE(coll2.registered_ok);

  auto img2 = importer_->ImportImage(MakeMetadata(coll2.id, 0), ImageReadinessPolicy::kOptimistic,
                                     reply_holder_, nullptr);
  importer_->OnVsync(display::WireConfigStamp{1}, async::Now(dispatcher()) + zx::sec(5));
  RunLoopUntilIdle();
  EXPECT_EQ(img2->gpu_state(), ImportState::kQueued);

  ImportedImage* arr2[] = {img2.get()};
  (void)importer_->SignalWhenReady(arr2);
  RunLoopUntil([&] { return img2->gpu_state() == ImportState::kReady; });
}

TEST_F(ImageImporterImplTest, CleanUpReapsUnsampledGpuImagesOffMainAndSampledOnMain) {
  for (const uint8_t workers : {uint8_t{1}, uint8_t{0}}) {
    SCOPED_TRACE(workers);
    ImageImporterImpl::Config cfg;
    cfg.never_shown_imports = NeverShownImports::kWhenFree;
    cfg.worker_pool_size = workers;
    CreateImporter(cfg);

    auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2});
    ASSERT_TRUE(coll.registered_ok);
    const uint32_t unsampled_off_main_before = renderer_->unsampled_off_main_destructions();

    const auto import_ready = [&](uint32_t vmo_index) {
      auto img = importer_->ImportImage(MakeMetadata(coll.id, vmo_index),
                                        ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
      ImportedImage* arr[] = {img.get()};
      if (workers == 0) {
        importer_->PrepareForGpuComposition(arr);
      }
      RunLoopUntil([&] {
        return img->gpu_state() == ImportState::kReady &&
               img->display_state() == ImportState::kReady;
      });
      return img;
    };
    auto unsampled = import_ready(0);
    renderer_->set_default_sampled(true);
    auto sampled = import_ready(1);
    renderer_->set_default_sampled(false);
    ASSERT_EQ(renderer_->live_gpu_images(), 2u);

    // `CleanUp()` runs inline on main. The sampled image is destroyed there; the unsampled one is
    // reaped by a worker when there is one, and otherwise also destroyed on main.
    importer_->ReleaseImage(sampled);
    EXPECT_EQ(renderer_->live_gpu_images(), 1u);
    importer_->ReleaseImage(unsampled);
    RunLoopUntil([&] { return renderer_->live_gpu_images() == 0u; });
    EXPECT_EQ(renderer_->unsampled_off_main_destructions() - unsampled_off_main_before,
              workers > 0 ? 1u : 0u);
    EXPECT_EQ(renderer_->off_main_destructions(), 0u);
  }
}

TEST_F(ImageImporterImplTest, ZeroWorkersRunsOneGpuJobPerDispatcherTurn) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.worker_pool_size = 0;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 3});
  ASSERT_TRUE(coll.registered_ok);

  next_wakeup_time_ = async::Now(dispatcher());
  auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kOptimistic,
                                     reply_holder_, nullptr);
  auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1), ImageReadinessPolicy::kOptimistic,
                                     reply_holder_, nullptr);
  RunLoopUntilIdle();
  EXPECT_EQ(img1->gpu_state(), ImportState::kQueued);
  EXPECT_EQ(img2->gpu_state(), ImportState::kQueued);

  uint32_t ready_when_interleaved_task_ran = 99;
  renderer_->SetCreateGpuImageHook(
      [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
        if (meta.identifier == img1->id()) {
          // Post an interleaved task onto main while the first inline GPU job runs; because
          // `worker_pool_size == 0` runs at most one GPU job per `RunDispatcher()` task and
          // re-posts, this task runs before `img2` starts.
          async::PostTask(dispatcher(), [&] {
            ready_when_interleaved_task_ran = (img1->gpu_state() == ImportState::kReady ? 1u : 0u) +
                                              (img2->gpu_state() == ImportState::kReady ? 1u : 0u);
          });
        }
        return r.MakeTrackedImage(meta.identifier);
      });

  next_wakeup_time_.reset();
  async::PostTask(dispatcher(), [&] { importer_->OnCpuWorkDone(); });
  RunLoopUntil([&] { return img2->gpu_state() == ImportState::kReady; });
  EXPECT_EQ(ready_when_interleaved_task_ran, 1u);
}

TEST_F(ImageImporterImplTest,
       ReleaseWhileGpuAndDisplayInFlightStillReportsFailureAndCleansUpOnMain) {
  ImageImporterImpl::Config cfg;
  cfg.import_kinds = ImportKinds::kConcurrent;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 2, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);

  libsync::Completion gpu_entered;
  libsync::Completion allow_gpu;
  renderer_->SetCreateGpuImageHook(
      [&](ControllableRenderer&, const allocation::ImageMetadata&,
          uint64_t) -> fit::result<ImportImageError, std::unique_ptr<Renderer::GpuImage>> {
        gpu_entered.Signal();
        allow_gpu.Wait();
        return fit::error(ImportImageError::kGpuImportFailed);
      });

  libsync::Completion disp_entered;
  libsync::Completion allow_disp;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        disp_entered.Signal();
        allow_disp.Wait();
      });

  std::atomic<bool> failure_called{false};
  auto img = importer_->ImportImage(
      MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_,
      [&](allocation::GlobalImageId, ImportImageError) { failure_called.store(true); });
  ImportedImage* arr[] = {img.get()};
  zx::event ev = importer_->SignalWhenReady(arr);
  ASSERT_TRUE(ev.is_valid());

  RunLoopUntil([&] { return gpu_entered.signaled() && disp_entered.signaled(); });

  // Release from a separate session thread while BOTH display and GPU imports are `kImporting`,
  // and mark the collection released by `Allocator` so `CleanUp()` tears down the collection to
  // `kReleased` on main while `OnDisplayImportComplete()` runs on `reply_loop_`.
  std::thread session_releaser([&] { importer_->ReleaseImage(img); });
  session_releaser.join();
  EXPECT_TRUE(img->released());
  importer_->ReleaseBufferCollection(coll.id, allocation::BufferCollectionUsage::kClientImage);

  allow_gpu.Signal();
  allow_disp.Signal();

  RunLoopUntil([&] {
    return img->display_state() == ImportState::kReleased &&
           img->gpu_state() == ImportState::kReleased;
  });
  // The worker posts `on_failure` and the coordinator counts `ReleaseImage()` on their own threads,
  // possibly after `CleanUp()` on main has already marked both sides `kReleased`.
  RunLoopUntil([&] { return failure_called.load(); });
  RunLoopUntil([&] { return mock_coordinator_->release_image_count() == 1u; });
  RunLoopUntilIdle();

  EXPECT_EQ(ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr), ZX_OK);
  EXPECT_EQ(mock_coordinator_->release_image_count(), 1u);
}

TEST_F(ImageImporterImplTest, DeduplicationWastedCountersAndInspectMetrics) {
  ImageImporterImpl::Config cfg;
  cfg.import_kinds = ImportKinds::kConcurrent;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg, "kinds=concurrent,never_shown=when_free");

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 6, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);

  // `used_direct` is presented and composed direct-to-display after deduplicating a duplicate span;
  // its GPU import was never sampled (`wasted_gpu_imports == 1`, `wasted_display_imports == 0`).
  auto used_direct = importer_->ImportImage(
      MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  // `never_used` finishes both imports and is released unused (`wasted_display == 1`,
  // `wasted_gpu == 1`).
  auto never_used = importer_->ImportImage(
      MakeMetadata(coll.id, 1), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);

  // `sampled_img` has its GPU image marked sampled (`gpu_frames_ == 0`), proving `WasSampled()`
  // suppresses `wasted_gpu_imports`.
  // GPU images are created asynchronously on a worker, so wait for the neighboring GPU imports
  // to finish before toggling `set_default_sampled()` around `sampled_img`'s import.
  RunLoopUntil([&] {
    return used_direct->gpu_state() == ImportState::kReady &&
           never_used->gpu_state() == ImportState::kReady;
  });
  renderer_->set_default_sampled(true);
  auto sampled_img = importer_->ImportImage(
      MakeMetadata(coll.id, 2), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] { return sampled_img->gpu_state() == ImportState::kReady; });
  renderer_->set_default_sampled(false);

  // `gpu_composed` is composed in a non-direct frame (`gpu_frames_ == 1`), proving `gpu_frames_ >
  // 0` suppresses `wasted_gpu_imports`.
  auto gpu_composed = importer_->ImportImage(
      MakeMetadata(coll.id, 3), ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);

  RunLoopUntil([&] {
    return used_direct->display_state() == ImportState::kReady &&
           used_direct->gpu_state() == ImportState::kReady &&
           never_used->display_state() == ImportState::kReady &&
           never_used->gpu_state() == ImportState::kReady &&
           sampled_img->display_state() == ImportState::kReady &&
           sampled_img->gpu_state() == ImportState::kReady &&
           gpu_composed->display_state() == ImportState::kReady &&
           gpu_composed->gpu_state() == ImportState::kReady;
  });

  // Live Inspect gauges reflect 4 allocated images (`4 * 4096 == 16384` bytes).
  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    const auto* cfg_node = node->GetByPath({"config"});
    ASSERT_NE(cfg_node, nullptr);
    EXPECT_EQ(GetInspectString(*cfg_node, "image_import_policy"),
              "kinds=concurrent,never_shown=when_free");
    EXPECT_EQ(GetInspectString(*cfg_node, "import_kinds"), "concurrent");
    EXPECT_EQ(GetInspectString(*cfg_node, "never_shown_imports"), "when_free");
    EXPECT_TRUE(GetInspectBool(*cfg_node, "enable_direct_to_display"));

    // `CreateLazyValues()` splices live gauges directly onto `image_import`.
    EXPECT_EQ(GetInspectUint(*node, "live_images"), 4u);
    EXPECT_EQ(GetInspectUint(*node, "total_imported_bytes"), 16384u);
  }

  ImportedImage* dup_span[] = {used_direct.get(), nullptr, used_direct.get(), used_direct.get()};
  utils::PmrStackArena<256> arena;
  std::pmr::vector<ImportedImage*> unique(arena.resource());
  DeduplicateImages(dup_span, unique);
  ASSERT_EQ(unique.size(), 1u);
  EXPECT_EQ(unique[0], used_direct.get());
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kDirect, .images = unique});

  ImportedImage* gpu_comp_span[] = {gpu_composed.get()};
  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kGpuForced, .images = gpu_comp_span});

  // Compose reports for remaining outcomes to exercise Inspect counters.
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kTooManyLayers});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kConfigRejected});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kGpuOnlyImage});
  importer_->OnFrameCommitted(FrameReport{.outcome = FrameOutcome::kDisplayImportFailed});

  // The first `OnCpuWorkDone()` consumes the frames composed above; a second one with no frame
  // composed in between records a skipped frame.
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();
  importer_->OnVsync(display::WireConfigStamp{99}, async::Now(dispatcher()));

  // Drive async `SignalWhenReady()` waits on both `kWaitForImports` and `kOptimistic` images
  // on an unallocated collection so both `present_delay_*` histograms record samples.
  auto unalloc_coll = RegisterCollection(CollectionSpec{.auto_allocate = false, .buffer_count = 2});
  ASSERT_TRUE(unalloc_coll.registered_ok);
  auto delay_wait_img =
      importer_->ImportImage(MakeMetadata(unalloc_coll.id, 0),
                             ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto delay_opt_img = importer_->ImportImage(
      MakeMetadata(unalloc_coll.id, 1), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr);
  ImportedImage* delay_wait_arr[] = {delay_wait_img.get()};
  ImportedImage* delay_opt_arr[] = {delay_opt_img.get()};
  zx::event delay_wait_ev = importer_->SignalWhenReady(delay_wait_arr);
  zx::event delay_opt_ev = importer_->SignalWhenReady(delay_opt_arr);
  ASSERT_TRUE(delay_wait_ev.is_valid());
  ASSERT_TRUE(delay_opt_ev.is_valid());
  fake_sysmem_->CompleteAllocation(unalloc_coll.sysmem_index);
  RunLoopUntil([&] {
    return delay_wait_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK &&
           delay_opt_ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK &&
           delay_opt_img->display_state() == ImportState::kReady &&
           delay_opt_img->gpu_state() == ImportState::kReady;
  });
  // Mark `delay_wait_img` and `delay_opt_img` as direct- and GPU-used so releasing them does not
  // alter the wasted-import counts tested below.
  ImportedImage* delay_used_arr[] = {delay_wait_img.get(), delay_opt_img.get()};
  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kDirect, .images = delay_used_arr});
  importer_->OnFrameCommitted(
      FrameReport{.outcome = FrameOutcome::kGpuForced, .images = delay_used_arr});

  importer_->ReleaseImage(used_direct);
  importer_->ReleaseImage(never_used);
  importer_->ReleaseImage(sampled_img);
  importer_->ReleaseImage(gpu_composed);
  importer_->ReleaseImage(delay_wait_img);
  importer_->ReleaseImage(delay_opt_img);
  RunLoopUntilIdle();

  const auto root = ReadInspectRoot();
  const auto* node = root.GetByPath({"image_import"});
  ASSERT_NE(node, nullptr);
  EXPECT_EQ(GetInspectUint(*node, "frames_direct"), 2u);
  EXPECT_EQ(GetInspectUint(*node, "frames_gpu_forced"), 2u);
  EXPECT_EQ(GetInspectUint(*node, "frames_fallback_too_many_layers"), 1u);
  EXPECT_EQ(GetInspectUint(*node, "frames_fallback_config_rejected"), 1u);
  EXPECT_EQ(GetInspectUint(*node, "frames_fallback_gpu_only_image"), 1u);
  EXPECT_EQ(GetInspectUint(*node, "frames_fallback_display_import_failed"), 1u);
  EXPECT_EQ(GetInspectUint(*node, "frames_skipped"), 1u);
  EXPECT_GE(GetInspectUint(*node, "predictions_correct"), 1u);
  EXPECT_GE(GetInspectUint(*node, "predictions_wrong"), 1u);
  EXPECT_EQ(GetInspectUint(*node, "wasted_display_imports"), 3u);
  EXPECT_EQ(GetInspectUint(*node, "wasted_gpu_imports"), 2u);
  EXPECT_EQ(GetInspectUint(*node, "live_images"), 0u);
  EXPECT_EQ(GetInspectUint(*node, "total_imported_bytes"), 0u);
  EXPECT_GE(GetInspectHistogramCount(*node, "frame_blocked_total_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "frame_blocked_display_sent_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "frame_blocked_display_in_flight_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "frame_blocked_gpu_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "present_delay_wait_for_imports_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "present_delay_optimistic_us"), 1u);
  EXPECT_GE(GetInspectHistogramCount(*node, "worker_jobs_per_wakeup"), 1u);
}

TEST_F(ImageImporterImplTest, DeduplicateImagesPreservesOrderAcrossLinearAndHashPaths) {
  ImageImporterImpl::Config cfg;
  cfg.enable_direct_to_display = false;
  cfg.never_shown_imports = NeverShownImports::kNever;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 40});
  ASSERT_TRUE(coll.registered_ok);

  std::vector<std::shared_ptr<ImportedImage>> owned;
  owned.reserve(40);
  for (uint32_t i = 0; i < 40; ++i) {
    owned.push_back(importer_->ImportImage(
        MakeMetadata(coll.id, i), ImageReadinessPolicy::kOptimistic, reply_holder_, nullptr));
  }

  // `> 32` entries exercises the `std::pmr::unordered_set` branch in `DeduplicateImages()`.
  std::vector<ImportedImage*> with_dups;
  for (uint32_t i = 0; i < 40; ++i) {
    with_dups.push_back(owned[i].get());
    with_dups.push_back(owned[i / 2].get());
    if (i % 5 == 0) {
      with_dups.push_back(nullptr);
    }
  }

  utils::PmrStackArena<4096> arena;
  std::pmr::vector<ImportedImage*> unique(arena.resource());
  DeduplicateImages(with_dups, unique);
  std::vector<ImportedImage*> expected;
  expected.reserve(40);
  for (const auto& img : owned) {
    expected.push_back(img.get());
  }
  EXPECT_EQ(std::vector<ImportedImage*>(unique.begin(), unique.end()), expected);
}

TEST_F(ImageImporterImplTest, MultiThreadedConcurrentSessionsWorkersAndFramesStress) {
  ImageImporterImpl::Config cfg;
  cfg.display_reply_thread = true;
  cfg.worker_pool_size = 2;
  cfg.import_kinds = ImportKinds::kConcurrent;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 16, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);

  std::mutex shared_images_mutex;
  std::vector<std::shared_ptr<ImportedImage>> active_frame_images;
  std::atomic<int> sessions_finished{0};

  constexpr int kSessionThreads = 3;
  constexpr int kImagesPerSession = 25;
  std::vector<std::thread> session_threads;
  session_threads.reserve(kSessionThreads);

  for (int t = 0; t < kSessionThreads; ++t) {
    session_threads.emplace_back([&, t] {
      async::Loop session_loop(&kAsyncLoopConfigNoAttachToCurrentThread);
      auto holder = std::make_shared<utils::UnownedDispatcherHolder>(session_loop.dispatcher());
      for (int i = 0; i < kImagesPerSession; ++i) {
        const auto policy =
            (t == 0) ? ImageReadinessPolicy::kOptimistic : ImageReadinessPolicy::kWaitForImports;
        auto img = importer_->ImportImage(MakeMetadata(coll.id, static_cast<uint32_t>(i % 16)),
                                          policy, holder, nullptr);
        ImportedImage* arr[] = {img.get(), img.get()};
        zx::event ev = importer_->SignalWhenReady(arr);
        if (i % 3 == 0 && ev.is_valid()) {
          EXPECT_EQ(ev.wait_one(ZX_EVENT_SIGNALED, zx::deadline_after(zx::sec(10)), nullptr),
                    ZX_OK);
        }
        if (i % 2 == 0) {
          std::scoped_lock lock(shared_images_mutex);
          active_frame_images.push_back(img);
        } else {
          // Exercise concurrent off-main `ReleaseImage()` on images not in the published scene.
          importer_->ReleaseImage(img);
        }
      }
      session_loop.RunUntilIdle();
      sessions_finished.fetch_add(1, std::memory_order_release);
    });
  }

  // Pump frames and cleanup on main concurrently with session and worker threads.
  uint64_t stamp = 1;
  while (sessions_finished.load(std::memory_order_acquire) < kSessionThreads) {
    std::shared_ptr<ImportedImage> to_release_on_main;
    std::vector<std::shared_ptr<ImportedImage>> snapshot;
    {
      std::scoped_lock lock(shared_images_mutex);
      if (active_frame_images.size() > 8 && stamp % 3 == 0) {
        to_release_on_main = std::move(active_frame_images.back());
        active_frame_images.pop_back();
      }
      snapshot = active_frame_images;
    }
    if (to_release_on_main) {
      importer_->ReleaseImage(to_release_on_main);
    }

    std::vector<ImportedImage*> raw;
    raw.reserve(snapshot.size());
    for (const auto& sp : snapshot) {
      if (!sp->released()) {
        raw.push_back(sp.get());
      }
    }
    utils::PmrStackArena<1024> arena;
    std::pmr::vector<ImportedImage*> unique(arena.resource());
    DeduplicateImages(raw, unique);

    if (!unique.empty()) {
      if (stamp % 2 == 0 && importer_->CheckDisplayStates(unique) == FrameOutcome::kDirect) {
        const FrameOutcome outcome = importer_->PrepareForDirectToDisplay(unique);
        importer_->OnFrameCommitted(FrameReport{
            .outcome = outcome,
            .images = unique,
            .config_stamp = display::WireConfigStamp{stamp},
            .target_presentation_time = async::Now(dispatcher()) + zx::msec(8),
            .vsync_interval = zx::msec(8),
        });
      } else {
        importer_->PrepareForGpuComposition(unique);
        importer_->OnFrameCommitted(
            FrameReport{.outcome = FrameOutcome::kTooManyLayers, .images = unique});
      }
      importer_->OnCpuWorkDone();
      RunLoopUntilIdle();
      importer_->OnVsync(display::WireConfigStamp{stamp}, async::Now(dispatcher()) + zx::msec(8));
      ++stamp;
    }
    RunLoopUntilIdle();
  }

  for (auto& th : session_threads) {
    th.join();
  }
  for (const auto& sp : active_frame_images) {
    if (!sp->released()) {
      importer_->ReleaseImage(sp);
    }
  }

  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_GT(GetInspectUint(*node, "worker_wakeups"), 0u);
    EXPECT_GT(GetInspectUint(*node, "worker_jobs_completed"), 0u);
  }

  importer_->StopOnMain();
  active_frame_images.clear();

  EXPECT_EQ(renderer_->live_gpu_images(), 0u);
  EXPECT_EQ(renderer_->live_gpu_collections(), 0u);
  EXPECT_EQ(renderer_->off_main_destructions(), 0u);
}

TEST_F(ImageImporterImplTest, LiveQueueAndInFlightInspectGaugesAndReleasePruning) {
  ImageImporterImpl::Config cfg;
  cfg.import_kinds = ImportKinds::kConcurrent;
  cfg.display_import_cap = 1;
  cfg.worker_pool_size = 1;
  cfg.never_shown_imports = NeverShownImports::kWhenFree;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 4, .per_buffer_bytes = 4096});
  ASSERT_TRUE(coll.registered_ok);

  libsync::Completion disp_entered;
  libsync::Completion allow_disp;
  mock_coordinator_->set_import_image_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*) {
        disp_entered.Signal();
        allow_disp.Wait();
      });

  libsync::Completion gpu_entered;
  libsync::Completion allow_gpu;
  renderer_->SetCreateGpuImageHook(
      [&](ControllableRenderer& r, const allocation::ImageMetadata& meta, uint64_t) {
        gpu_entered.Signal();
        allow_gpu.Wait();
        return r.MakeTrackedImage(meta.identifier);
      });

  auto img1 = importer_->ImportImage(MakeMetadata(coll.id, 0),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto img2 = importer_->ImportImage(MakeMetadata(coll.id, 1),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  auto img3 = importer_->ImportImage(MakeMetadata(coll.id, 2),
                                     ImageReadinessPolicy::kWaitForImports, reply_holder_, nullptr);
  RunLoopUntil([&] { return disp_entered.signaled() && gpu_entered.signaled(); });
  RunLoopUntilIdle();

  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "in_flight_display_imports"), 1u);
    EXPECT_EQ(GetInspectUint(*node, "in_flight_gpu_jobs"), 1u);
    EXPECT_EQ(GetInspectUint(*node, "queued_display_jobs"), 2u);
    EXPECT_EQ(GetInspectUint(*node, "queued_gpu_jobs"), 2u);
  }

  // Releasing a still-queued image (`img3`) prunes its queue entries immediately in `CleanUp()`
  // without needing a later `ImportImage()`.
  std::weak_ptr<ImportedImage> weak_img3 = img3;
  importer_->ReleaseImage(img3);
  img3.reset();
  RunLoopUntilIdle();
  EXPECT_TRUE(weak_img3.expired());
  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "queued_display_jobs"), 1u);
    EXPECT_EQ(GetInspectUint(*node, "queued_gpu_jobs"), 1u);
  }

  // Leave the hooks installed until the imports finish: the coordinator and worker threads are
  // still inside them, and once signaled the completions no longer block `img2`.
  allow_disp.Signal();
  allow_gpu.Signal();
  RunLoopUntil([&] {
    return img1->display_state() == ImportState::kReady &&
           img1->gpu_state() == ImportState::kReady &&
           img2->display_state() == ImportState::kReady && img2->gpu_state() == ImportState::kReady;
  });
  // The worker decrements `in_flight_gpu_jobs` after publishing `kReady`.
  RunLoopUntil([&] {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    return node != nullptr && GetInspectUint(*node, "in_flight_gpu_jobs") == 0u;
  });
  mock_coordinator_->set_import_image_fn(nullptr);
  renderer_->SetCreateGpuImageHook(nullptr);

  {
    const auto root = ReadInspectRoot();
    const auto* node = root.GetByPath({"image_import"});
    ASSERT_NE(node, nullptr);
    EXPECT_EQ(GetInspectUint(*node, "in_flight_display_imports"), 0u);
    EXPECT_EQ(GetInspectUint(*node, "in_flight_gpu_jobs"), 0u);
    EXPECT_EQ(GetInspectUint(*node, "queued_display_jobs"), 0u);
    EXPECT_EQ(GetInspectUint(*node, "queued_gpu_jobs"), 0u);
  }
}

TEST_F(ImageImporterImplTest, ReleaseQueuedClass3ImagePrunesQueueWithoutLaterImport) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kNever;
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 1});
  ASSERT_TRUE(coll.registered_ok);

  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports,
                                    reply_holder_, nullptr);
  ASSERT_EQ(img->display_state(), ImportState::kQueued);
  ASSERT_EQ(img->gpu_state(), ImportState::kQueued);

  std::weak_ptr<ImportedImage> weak_img = img;
  importer_->ReleaseImage(img);
  img.reset();
  RunLoopUntilIdle();
  EXPECT_TRUE(weak_img.expired());
}

TEST_F(ImageImporterImplTest, SteadySceneTakesNoQueueMutexOnMain) {
  ImageImporterImpl::Config cfg;
  cfg.never_shown_imports = NeverShownImports::kWhenIdle;
  cfg.idle_after = zx::msec(50);
  CreateImporter(cfg);

  auto coll = RegisterCollection(CollectionSpec{.buffer_count = 1});
  ASSERT_TRUE(coll.registered_ok);

  auto img = importer_->ImportImage(MakeMetadata(coll.id, 0), ImageReadinessPolicy::kWaitForImports,
                                    reply_holder_, nullptr);
  ImportedImage* arr[] = {img.get()};
  zx::event ev = importer_->SignalWhenReady(arr);
  if (ev.is_valid()) {
    RunLoopUntil([&] {
      return ev.wait_one(ZX_EVENT_SIGNALED, zx::time::infinite_past(), nullptr) == ZX_OK;
    });
  }
  RunLoopUntilIdle();
  ASSERT_EQ(img->display_state(), ImportState::kReady);
  ASSERT_EQ(img->gpu_state(), ImportState::kReady);

  libsync::Completion helper_locked;
  libsync::Completion main_done;
  std::atomic<bool> timed_out{false};
  std::thread helper([&] {
    auto qlock = ImageImporterImplTestPeer::LockQueueMutex(*importer_);
    helper_locked.Signal();
    if (main_done.Wait(zx::sec(5)) != ZX_OK) {
      timed_out.store(true, std::memory_order_relaxed);
    }
  });
  helper_locked.Wait();

  EXPECT_FALSE(importer_->SignalWhenReady(arr).is_valid());
  importer_->BeginFrame();
  EXPECT_EQ(importer_->CheckDisplayStates(arr), FrameOutcome::kDirect);
  EXPECT_EQ(importer_->PrepareForDirectToDisplay(arr), FrameOutcome::kDirect);
  const zx::time frame_now = async::Now(dispatcher());
  importer_->OnFrameCommitted(FrameReport{
      .outcome = FrameOutcome::kDirect,
      .images = arr,
      .config_stamp = display::WireConfigStamp{10},
      .target_presentation_time = frame_now + zx::msec(8),
      .vsync_interval = zx::msec(8),
  });
  importer_->OnVsync(display::WireConfigStamp{10}, frame_now + zx::sec(1));
  importer_->OnCpuWorkDone();
  RunLoopUntilIdle();

  main_done.Signal();
  helper.join();
  EXPECT_FALSE(timed_out.load(std::memory_order_relaxed));
}

}  // namespace
}  // namespace flatland
