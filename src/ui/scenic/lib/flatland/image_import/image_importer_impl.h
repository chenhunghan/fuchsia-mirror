// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Design: DESIGN.md in this directory. A change that contradicts a rule
// there amends the rule in the same CL, with the reason (DESIGN.md#changing).

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_IMPL_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_IMPL_H_

#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/zx/event.h>
#include <lib/zx/time.h>

#include <atomic>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <functional>
#include <memory>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <thread>
#include <unordered_map>
#include <utility>
#include <vector>

#include "src/lib/fxl/synchronization/thread_annotations.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/display/coordinator_proxy/coordinator_proxy.h"
#include "src/ui/scenic/lib/flatland/image_import/engine_image_importer.h"
#include "src/ui/scenic/lib/flatland/image_import/image_importer.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_collection.h"
#include "src/ui/scenic/lib/flatland/image_import/imported_image.h"
#include "src/ui/scenic/lib/flatland/image_import/undisplayed_frame_deferral.h"
#include "src/ui/scenic/lib/flatland/renderer/renderer.h"
#include "src/ui/scenic/lib/scheduling/median_duration_predictor.h"

namespace flatland {

// Scheduling and policy choices on `ImageImporterImpl::Config`
// (DESIGN.md#config).
enum class ImportKinds : uint8_t { kPredictedKindFirst, kConcurrent };
enum class OffScreenOrder : uint8_t { kMostRecentFirst, kLeastRecentFirst };
enum class NeverShownImports : uint8_t { kWhenFree, kWhenIdle, kNever };

// Asynchronous, prioritized Flatland client image importer, implementing both
// the session interface (`ImageImporter`) and the frame interface
// (`EngineImageImporter`). Only `App` holds the concrete type
// (DESIGN.md#ownership).
class alignas(64) ImageImporterImpl final : public ImageImporter,
                                            public EngineImageImporter,
                                            public allocation::BufferCollectionImporter,
                                            public std::enable_shared_from_this<ImageImporterImpl> {
 public:
  // Defaults match the policy keys listed in
  // `src/ui/scenic/bin/meta/config.shard.cml` (DESIGN.md#config).
  // LINT.IfChange(config)
  struct Config {
    bool enable_direct_to_display = true;
    ImportKinds import_kinds = ImportKinds::kPredictedKindFirst;
    OffScreenOrder off_screen_order = OffScreenOrder::kMostRecentFirst;
    NeverShownImports never_shown_imports = NeverShownImports::kWhenIdle;
    zx::duration idle_after = zx::msec(100);  // Must be non-negative.
    uint32_t display_import_cap = 1;          // Must be at least 1.
    // Number of GPU worker threads. Workers run only GPU imports; display
    // `ImportImage()` calls are always made from main and completed on the
    // reply loop. `0` runs GPU imports on main in its idle window
    // (DESIGN.md#dispatcher).
    uint8_t worker_pool_size = 1;
    bool display_reply_thread = false;
  };
  // LINT.ThenChange(/src/ui/scenic/bin/meta/config.shard.cml:image_import_policy)

  // Factory method guaranteeing `std::shared_ptr` ownership.
  // Main thread only. The `image_import_policy` keys that set `config` fields,
  // with their values and defaults, are listed in
  // `src/ui/scenic/bin/meta/config.shard.cml`. Requires:
  // - `main_dispatcher` is non-null, `renderer` is non-null;
  // - `config.display_import_cap >= 1`, `config.idle_after >= zx::duration{0}`;
  // - `coordinator` is non-null when `config.enable_direct_to_display` is true, and whenever
  //   non-null is bound to `reply_loop.dispatcher()`;
  // - when `config.display_reply_thread` is false and `coordinator` is non-null, `coordinator`
  //   was built from a `fidl::ClientEnd` (not a pre-bound `WireSharedClient`);
  // - `main_dispatcher` and `reply_loop` outlive all frame calls and `StopOnMain()`, and
  //   `reply_loop` is passed with no thread started (`New()` starts its `"scenic.display-reply"`
  //   thread when `config.display_reply_thread` is true, and pumps it on `main_dispatcher` when
  //   `config.display_reply_thread` is false);
  // - `next_wakeup` is non-null, runs on main, and returns `std::nullopt` when idle;
  //   the importer reads it only from its own posted main-loop tasks;
  // - `raw_policy` is recorded verbatim to Inspect; `config` is authoritative.
  static std::shared_ptr<ImageImporterImpl> New(
      Config config, async_dispatcher_t* main_dispatcher,
      std::shared_ptr<display::CoordinatorProxy> coordinator, async::Loop& reply_loop,
      std::shared_ptr<Renderer> renderer, fit::function<std::optional<zx::time>()> next_wakeup,
      inspect::Node inspect_node, std::string_view raw_policy);

  // Main thread only. `FX_CHECK`s that `StopOnMain()` ran before destruction.
  ~ImageImporterImpl() override;

  ImageImporterImpl(const ImageImporterImpl&) = delete;
  ImageImporterImpl& operator=(const ImageImporterImpl&) = delete;

  // Main thread only (`FX_CHECK`s `utils::IsOnMainThread()`). Requires
  // `reply_loop` to still be running. Joins workers, drains in-flight display
  // imports, cleans up every live image, and tears down every collection so
  // nothing left alive touches the Vulkan device (DESIGN.md#cleanup-on-main).
  void StopOnMain();

  const Config& config() const { return config_; }

  // `ImageImporter`:
  std::shared_ptr<ImportedImage> ImportImage(
      const allocation::ImageMetadata& metadata, ImageReadinessPolicy policy,
      const std::shared_ptr<utils::DispatcherHolder>& reply_dispatcher,
      FailureCallback on_failure) override;
  void ReleaseImage(const std::shared_ptr<ImportedImage>& image) override;
  zx::event SignalWhenReady(std::span<ImportedImage* const> images) override;

  // `allocation::BufferCollectionImporter` (main thread only, `kClientImage`
  // collections). `ImageImporterImpl` is registered as a `BufferCollectionImporter` only
  // with `allocation::Allocator` (never in `Flatland`'s `BufferCollectionImporter` list,
  // which receives it as `ImageImporter`). Only collection registration and release go through
  // `BufferCollectionImporter`; `ImportBufferImage()` and
  // `ReleaseBufferImage()` abort because client images use `ImageImporter`.
  fpromise::promise<> ImportBufferCollection(
      allocation::GlobalBufferCollectionId collection_id,
      fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token,
      allocation::BufferCollectionUsage usage, std::optional<fuchsia_math::SizeU> size) override;
  void ReleaseBufferCollection(allocation::GlobalBufferCollectionId collection_id,
                               allocation::BufferCollectionUsage usage) override;
  fpromise::promise<> ImportBufferImage(const allocation::ImageMetadata& metadata,
                                        allocation::BufferCollectionUsage usage) override;
  void ReleaseBufferImage(allocation::GlobalImageId image_id) override;

  // `EngineImageImporter`:
  bool SupportsDirectToDisplay() const override { return config_.enable_direct_to_display; }
  void BeginFrame() override;
  FrameOutcome CheckDisplayStates(std::span<ImportedImage* const> images) const override;
  FrameOutcome PrepareForDirectToDisplay(std::span<ImportedImage* const> images) override;
  void PrepareForGpuComposition(std::span<ImportedImage* const> images) override;
  void OnFrameCommitted(const FrameReport& report) override;
  void OnVsync(display::WireConfigStamp stamp, zx::time_monotonic timestamp) override;

  // Main thread only, invoked after every rendered or skipped frame. Ends
  // `undisplayed_frame_deferral_` on a skipped frame and calls
  // `ScheduleDispatcher()` (DESIGN.md#dispatcher).
  void OnCpuWorkDone();

 private:
  friend class ImageImporterImplTestPeer;

  // Private so every instance is owned by a `std::shared_ptr` (via `New()`), which
  // `weak_from_this()` in posted tasks and reply callbacks requires.
  ImageImporterImpl(Config config, async_dispatcher_t* main_dispatcher,
                    std::shared_ptr<display::CoordinatorProxy> coordinator, async::Loop& reply_loop,
                    std::shared_ptr<Renderer> renderer,
                    fit::function<std::optional<zx::time>()> next_wakeup,
                    inspect::Node inspect_node, std::string_view raw_policy);

  enum class JobKind : uint8_t { kDisplay, kGpu };

  enum class JobClass : uint8_t {
    kNamedByPresent = 1,
    kUsed = 2,
    kNeverNamed = 3,
  };

  struct JobPriority {
    JobClass cls = JobClass::kNeverNamed;
    bool optimistic = false;
    uint64_t frame_num = 0;
    uint64_t image_id = 0;
  };

  // Argument to `EnqueueLocked()`; not stored. `image`'s collection has
  // finished allocating, so it always gets a GPU job.
  struct ImageToEnqueue {
    std::shared_ptr<ImportedImage> image;
    // Whether sysmem kept the display branch of `image`'s collection
    // (`kDisplayCapable`), so `image` also gets a display job. The display
    // import can still fail (DESIGN.md#gpu-readable).
    bool display_capable = false;
  };

  struct WaitItem {
    ImportedImage* image = nullptr;
    bool sent_by_frame = false;
  };

  struct PendingRegistration {
    std::unique_ptr<ImportedCollection::GpuCollection> gpu_collection;
    fidl::Client<fuchsia_sysmem2::BufferCollection> allocation_participant;
    fidl::Client<fuchsia_sysmem2::BufferCollection> display_participant;
    bool coordinator_import_in_flight = false;
    bool coordinator_imported_collection = false;
    bool released_by_allocator = false;
    fpromise::completer<> completer;
  };

  struct ChannelWatchState {
    std::atomic<bool> channel_closed{false};
    async::Wait readable_wait;
  };

  struct SysmemParticipants {
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> display_token;
    fidl::Client<fuchsia_sysmem2::BufferCollection> display_participant;
    fidl::Client<fuchsia_sysmem2::BufferCollection> allocation_participant;

    void Abandon();
  };

  std::optional<SysmemParticipants> SetupSysmemParticipants(
      allocation::GlobalBufferCollectionId collection_id,
      fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
      fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken>& parent_token);

  void FinishCoordinatorCollectionImportOnMain(allocation::GlobalBufferCollectionId collection_id,
                                               display::WireBufferCollectionId wire_collection_id,
                                               zx::result<> import_res);

  // Atomically updates `image.word_` with a compare-and-swap loop; safe from any
  // thread and takes no locks (DESIGN.md#one-word).
  //
  // `mutate` receives a copy of the current word and either edits it and returns
  // true to store it, or returns false to leave `word_` unchanged. It is re-run
  // on a fresh copy whenever another thread changes `word_` first, so it must
  // decide from its argument alone and have no side effects beyond values it
  // fully overwrites on every call.
  //
  // Returns `{old, new}` if the store happened, or `std::nullopt` if `mutate`
  // returned false. The thread that gets a value back owns any side effect of
  // the transition (DESIGN.md#owner-acts). The store has release ordering, so
  // fields written before it (such as `gpu_image_` or `gpu_error_`) are visible
  // to any thread that later reads `word_` with acquire ordering.
  static std::optional<std::pair<ImportWord, ImportWord>> Transition(
      ImportedImage& image, fit::function<bool(ImportWord&)> mutate);

  void AbandonRegistration(allocation::GlobalBufferCollectionId collection_id,
                           PendingRegistration& pending);
  void FinishCollectionRegistrationOnMain(allocation::GlobalBufferCollectionId collection_id,
                                          bool coordinator_succeeded);
  void StartCollectionAllocationWaits(const std::shared_ptr<ImportedCollection>& collection);
  void MaybeFinishCollectionAllocation(const std::shared_ptr<ImportedCollection>& collection);
  // Checks `image`'s `vmo_index` and size against `collection`, which has
  // finished allocating. Reads only collection fields fixed before the
  // collection leaves `kRegistered`, so it needs no lock.
  static std::optional<ImportImageError> ValidateImageForCollection(
      const ImportedImage& image, const ImportedCollection& collection);
  // Marks `image` as allocated once its collection has finished allocating, and
  // moves its import states from `kNotStarted` to `kQueued`: GPU always, and
  // display only if `display_capable` (otherwise display becomes
  // `kUnsupported`). The caller must still push the image onto the queues with
  // `EnqueueLocked()`. Returns false, changing nothing, if `image` is already
  // released (DESIGN.md#gpu-readable).
  static bool MarkImageAllocated(ImportedImage& image, bool display_capable);
  void FailImage(ImportedImage& image, ImportImageError code);
  // Pushes `images` and returns `RefreshGpuCanStartLocked()`.
  bool EnqueueLocked(std::span<ImageToEnqueue> images) FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Re-evaluates whether the best queued GPU job may start now, returns that
  // answer, and stores the negation in `gpu_jobs_deferred_`. Call after anything that can
  // change GPU job eligibility without enqueuing (a present stamping images,
  // display imports draining, or the importer going idle).
  bool RefreshGpuCanStartLocked() FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Starts whatever runs GPU jobs: notifies `gpu_workers_`, or schedules
  // `RunDispatcher()` on main when `worker_pool_size == 0`. Call without
  // `queue_mutex_` held, after `RefreshGpuCanStartLocked()` returns true.
  void WakeGpuRunner();
  bool SignalPendingEntriesLocked(ImportedImage& image, zx::time now)
      FXL_EXCLUSIVE_LOCKS_REQUIRED(pending_signal_mutex_);

  void SendDisplayImport(std::shared_ptr<ImportedImage> image);
  void OnDisplayImportComplete(std::shared_ptr<ImportedImage> image, zx::time sent_at,
                               zx::result<> result);
  void OnReplyChannelReadable(async_dispatcher_t* dispatcher, async::Wait* wait, zx_status_t status,
                              const zx_packet_signal_t* signal);

  JobKind PredictedNextKind() const;
  bool NeverShownEligibleLocked() const FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  JobPriority ComputeJobPriority(const ImportedImage& image, JobKind kind, uint64_t frames_composed,
                                 JobKind predicted_kind) const;
  bool IsJobBetter(const JobPriority& a, const JobPriority& b, uint64_t frames_composed) const;
  std::shared_ptr<ImportedImage> PopQueueAtLocked(JobKind kind, size_t index)
      FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Worker threads only. Pops the best eligible GPU job and counts it in
  // `gpu_jobs_running_`, or returns null if no GPU job is eligible.
  std::shared_ptr<ImportedImage> PopBestGpuJobForWorkerLocked()
      FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Whether `image` is still waiting in `kind`'s queue: not released, and not yet
  // claimed by another thread (still `kQueued` for `kind`). Queue readers skip
  // entries that fail this; `EnqueueLocked()` removes them before pushing, via
  // `PruneStaleQueueEntriesLocked()`.
  static bool IsQueuedJob(JobKind kind, const ImportedImage& image);
  void PruneStaleQueueEntriesLocked() FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  size_t QueuedJobCountLocked(JobKind kind) const FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Scans `kind`'s queue linearly for the best eligible job.
  std::optional<std::pair<size_t, JobPriority>> FindBestEligibleJobLocked(JobKind kind) const
      FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Checks whether `kind` has an eligible job allowed to start next under
  // `config_.import_kinds`. Callers intentionally re-run
  // `FindBestEligibleJobLocked()` when popping because realistic queues are small
  // (0-10 jobs) and `FindBestEligibleJobLocked()`'s `TRACE_DURATION` measures
  // whether caching the index is ever worth the coupling.
  bool CanStartKindNextLocked(JobKind kind) const FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  bool HasQueuedStampZeroJobLocked() const FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);
  // Updates `queued_jobs_` and emits the `ImageImporter::Queues` trace counter
  // (queued display and GPU job counts, and `gpu_jobs_running_`). Call it right
  // after changing any of those values, inside the helper that makes the change
  // (pushing, popping, clearing, or counting a GPU worker job), so the helper's
  // callers never need to.
  void EmitQueueTraceCountersLocked() FXL_EXCLUSIVE_LOCKS_REQUIRED(queue_mutex_);

  // Safe from any thread. Posts one `RunDispatcher()` task to main unless one is
  // already pending or `StopOnMain()` has begun; callers use this instead of
  // calling `RunDispatcher()` (DESIGN.md#dispatcher).
  void ScheduleDispatcher();

  // Main thread only, and runs only as the task `ScheduleDispatcher()` posts,
  // never directly: `next_wakeup_()` is accurate only between main-loop tasks,
  // and presents posted meanwhile must run first (DESIGN.md#dispatcher). Starts
  // eligible display imports that fit before the wakeup, wakes GPU workers, or
  // with `worker_pool_size == 0` runs at most one GPU import inline and posts
  // itself again if more work is eligible. A job deferred only because it does
  // not fit before the wakeup is retried when that wakeup passes.
  void RunDispatcher();
  // Main thread only. Destroys an unsampled `GpuImage` off main: queues it on
  // `reaper_queue_`, which `gpu_workers_` drain when no GPU job is eligible
  // (the reaper currently aliases the GPU workers). Destroys it inline when
  // there are no workers or `StopOnMain()` has begun (DESIGN.md#cleanup-on-main).
  void ReapGpuImage(std::unique_ptr<ImportedImage::GpuImage> gpu_image);
  // Body of each `gpu_workers_` thread: waits on `gpu_queue_nonempty_`, then
  // pops and runs GPU jobs from `gpu_queue_` until none is eligible, and
  // destroys `reaper_queue_` entries when no GPU job is eligible. Never touches
  // `display_queue_`.
  void GpuWorkerLoop(uint32_t worker_index);
  // `gpu_lock` must hold `image.gpu_run_mutex_` on entry and is unlocked before return.
  void ExecuteGpuImport(ImportedImage& image, std::unique_lock<std::mutex>& gpu_lock);
  void ResolvePendingEntries(ImportedImage& image);
  void PostCleanUp(allocation::GlobalImageId id);
  void CleanUpById(allocation::GlobalImageId id);
  static uint64_t SafeCollectionBufferBytes(const ImportedCollection* collection);
  void CleanUp(const std::shared_ptr<ImportedImage>& image);
  void TearDown(ImportedCollection& collection);
  void InitInspect(std::string_view raw_policy);

  struct MainDispatcherGate {
    std::mutex mutex;
    async_dispatcher_t* dispatcher FXL_GUARDED_BY(mutex);
  };

  // Group 0: immutable configuration and main-thread-only state
  // (DESIGN.md#lock-order).
  const Config config_;
  async_dispatcher_t* const main_dispatcher_;
  UndisplayedFrameDeferral undisplayed_frame_deferral_;
  std::shared_ptr<MainDispatcherGate> main_dispatcher_gate_;
  std::shared_ptr<display::CoordinatorProxy> coordinator_;
  const std::shared_ptr<Renderer> renderer_;
  const fit::function<std::optional<zx::time>()> next_wakeup_;
  async::Loop& reply_loop_;
  const std::shared_ptr<ChannelWatchState> watch_state_;
  std::optional<zx::time> pending_channel_readable_timestamp_;
  zx::time_monotonic last_frame_time_{0};
  bool frame_composed_since_last_cpu_done_ = false;
  bool stopped_ = false;
  zx::duration current_frame_display_sent_wait_{0};
  zx::duration current_frame_display_inflight_wait_{0};
  zx::duration current_frame_gpu_wait_{0};
  // Whether the last `ImageImporter::FrameBlockedUs` trace counter sample was nonzero, so
  // `OnVsync()` zeroes it (DESIGN.md#metrics).
  bool frame_blocked_counter_nonzero_ = false;
  std::unordered_map<allocation::GlobalBufferCollectionId, std::unique_ptr<PendingRegistration>>
      pending_registrations_;
  std::vector<WaitItem> waiters_scratch_;
  async::TaskClosure wakeup_retry_task_;
  uint64_t reply_channel_readable_calls_for_test_ = 0;
  std::function<void()> stop_drain_entered_hook_for_test_;

  // Group 1: cross-thread lock-free atomics (DESIGN.md#lock-order).
  std::atomic<uint32_t> queued_jobs_{0};
  // Incremented on `SendDisplayImport()` and decremented at entry of `OnDisplayImportComplete()`
  // for cap and kind-switching checks.
  alignas(64) std::atomic<uint32_t> display_imports_in_flight_{0};
  // Incremented on `SendDisplayImport()` and decremented at exit of `OnDisplayImportComplete()`
  // so `StopOnMain()` and `PrepareForDirectToDisplay()` drain the entire callback.
  std::atomic<uint32_t> display_imports_outstanding_{0};
  std::atomic<uint64_t> frames_composed_{0};
  std::atomic<FrameOutcome> last_outcome_{FrameOutcome::kDirect};
  // Set only once a present naming a GPU-only image has finished waiting for its imports.
  std::atomic<bool> present_named_gpu_only_{false};
  std::atomic<bool> dispatch_posted_{false};

  // Group 2: collection and live-image registries. Acquired before
  // `queue_mutex_` and `pending_signal_mutex_`, never after them; critical
  // sections never block on FIDL or events (DESIGN.md#lock-order,
  // DESIGN.md#no-blocking-under-locks).
  alignas(64) std::mutex collections_mutex_;
  std::unordered_map<allocation::GlobalBufferCollectionId, std::shared_ptr<ImportedCollection>>
      collections_ FXL_GUARDED_BY(collections_mutex_);
  // Holds `shared_ptr` until main-thread `CleanUp()` erases the entry so
  // `~ImportedImage()` never runs on a session thread before main clears
  // `gpu_image_` and `pending_` (DESIGN.md#cleanup-on-main).
  std::unordered_map<allocation::GlobalImageId, std::shared_ptr<ImportedImage>> live_images_by_id_
      FXL_GUARDED_BY(collections_mutex_);

  // Group 3: unordered job queues, worker state, and duration predictors.
  // Acquired after `collections_mutex_` and before `pending_signal_mutex_`,
  // never in reverse; critical sections never block and scan `O(queued jobs)`
  // on push and pop (DESIGN.md#lock-order, DESIGN.md#no-blocking-under-locks).
  alignas(64) std::mutex queue_mutex_;
  std::vector<std::shared_ptr<ImportedImage>> display_queue_ FXL_GUARDED_BY(queue_mutex_);
  std::vector<std::shared_ptr<ImportedImage>> gpu_queue_ FXL_GUARDED_BY(queue_mutex_);
  // Unsampled `GpuImage`s awaiting destruction by `ReapGpuImage()`.
  std::vector<std::unique_ptr<ImportedImage::GpuImage>> reaper_queue_ FXL_GUARDED_BY(queue_mutex_);
  // `collections_mutex_` fences `ImportImage()`'s registry insertion and `queue_mutex_` fences the
  // `gpu_queue_nonempty_.wait()` predicate before `notify_all()`.
  std::atomic<bool> stopping_{false};
  bool gpu_jobs_deferred_ FXL_GUARDED_BY(queue_mutex_) = false;
  // GPU jobs executing on `gpu_workers_`. Inline GPU jobs that `RunDispatcher()`
  // runs on main when `worker_pool_size == 0` are not counted.
  uint32_t gpu_jobs_running_ FXL_GUARDED_BY(queue_mutex_) = 0;
  // True from the first vsync at least `config_.idle_after` after the last
  // composed frame until the next `OnFrameCommitted()`. Maintained only for
  // `NeverShownImports::kWhenIdle`, where it lets never-named (Class 3) imports
  // run (DESIGN.md#priority). Tracks frames, not imports. Written only on main,
  // without a lock; other threads read it as a scheduling hint.
  std::atomic<bool> idle_{false};
  // Conservative cold-import seed duration before real samples arrive (DESIGN.md#dispatcher).
  static constexpr zx::duration kInitialImportDurationSeed = zx::msec(3);
  scheduling::MedianDurationPredictor display_import_duration_ FXL_GUARDED_BY(queue_mutex_){
      kInitialImportDurationSeed};
  scheduling::MedianDurationPredictor first_gpu_import_duration_ FXL_GUARDED_BY(queue_mutex_){
      kInitialImportDurationSeed};
  scheduling::MedianDurationPredictor later_gpu_import_duration_ FXL_GUARDED_BY(queue_mutex_){
      kInitialImportDurationSeed};
  std::condition_variable gpu_queue_nonempty_;
  std::vector<std::thread> gpu_workers_;

  // Group 4: `ImportedImage::pending_`, `PendingSignal::countdown`, and the
  // condition variable used when `display_reply_thread` is `true`. Leaf lock in
  // the hierarchy; never acquires `collections_mutex_` or `queue_mutex_`
  // (DESIGN.md#lock-order).
  alignas(64) std::mutex pending_signal_mutex_;
  std::condition_variable display_import_done_;
  std::function<void()> post_display_decrement_hook_for_test_ FXL_GUARDED_BY(pending_signal_mutex_);

  // Inspect nodes, counters, and histograms (DESIGN.md#metrics).
  inspect::Node inspect_node_;
  inspect::Node inspect_config_node_;
  inspect::LazyNode inspect_live_metrics_;
  inspect::UintProperty inspect_image_imports_rejected_;
  inspect::UintProperty inspect_display_imports_failed_;
  inspect::UintProperty inspect_gpu_imports_failed_;
  inspect::UintProperty inspect_collection_allocations_failed_;
  inspect::UintProperty inspect_prepare_for_gpu_ns_;
  inspect::UintProperty inspect_wasted_display_imports_;
  inspect::UintProperty inspect_wasted_gpu_imports_;
  inspect::UintProperty inspect_frames_direct_;
  inspect::UintProperty inspect_frames_gpu_forced_;
  inspect::UintProperty inspect_frames_fallback_too_many_layers_;
  inspect::UintProperty inspect_frames_fallback_config_rejected_;
  inspect::UintProperty inspect_frames_fallback_gpu_only_image_;
  inspect::UintProperty inspect_frames_fallback_display_import_failed_;
  inspect::UintProperty inspect_frames_skipped_;
  inspect::UintProperty inspect_predictions_correct_;
  inspect::UintProperty inspect_predictions_wrong_;
  inspect::UintProperty inspect_predictions_from_present_hint_;
  inspect::UintProperty inspect_predictions_from_present_hint_correct_;
  inspect::UintProperty inspect_worker_wakeups_;
  inspect::UintProperty inspect_worker_jobs_completed_;
  inspect::ExponentialUintHistogram inspect_frame_blocked_total_us_;
  inspect::ExponentialUintHistogram inspect_frame_blocked_display_sent_us_;
  inspect::ExponentialUintHistogram inspect_frame_blocked_display_in_flight_us_;
  inspect::ExponentialUintHistogram inspect_frame_blocked_gpu_us_;
  inspect::ExponentialUintHistogram inspect_image_wait_display_us_;
  inspect::ExponentialUintHistogram inspect_image_wait_gpu_us_;
  inspect::ExponentialUintHistogram inspect_display_reply_handling_delay_us_;
  inspect::ExponentialUintHistogram inspect_present_delay_wait_for_imports_us_;
  inspect::ExponentialUintHistogram inspect_present_delay_optimistic_us_;
  inspect::ExponentialUintHistogram inspect_worker_jobs_per_wakeup_;
};

// Parses comma-separated `key=value` overrides from `image_import_policy` into
// `config`, returning an error naming the problem on unknown or duplicate keys,
// leading zeros on multi-digit `uint32` values, or malformed values (validated
// at build time by assembly). Keys and values:
// `src/ui/scenic/bin/meta/config.shard.cml`.
fit::result<std::string> TryParseImageImportPolicy(std::string_view policy,
                                                   ImageImporterImpl::Config& config);

// Like `TryParseImageImportPolicy()`, `FX_CHECK`-failing with its error message
// when `policy` is invalid.
void ParseImageImportPolicy(std::string_view policy, ImageImporterImpl::Config& config);

// Formats the resolved policy fields of `config` in canonical `key=value` order
// for startup logging and diagnostics.
std::string FormatImageImportPolicy(const ImageImporterImpl::Config& config);

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_IMAGE_IMPORT_IMAGE_IMPORTER_IMPL_H_
