# Flatland image import design

This document is the design contract for asynchronous, prioritized import of
Flatland client images across `//src/ui/scenic/lib/flatland/image_import/` and
the renderer, session, and compositor hooks that cite it. Headers own method
signatures; this file states the cross-object, cross-thread, and cross-frame
rules that no single header can express.

## Changing this code {#changing}

1. A change that contradicts a rule here amends the rule in the same CL, and
   the amended text names the fact that changed (a measurement, a bug, or an
   API change). The commit message says what the old rule was.
2. A change this document is silent about needs no amendment. If it adds a
   rule others must keep (a lock, a thread restriction, or a state), add the
   rule here.
3. Code cites this document by stable anchor (`DESIGN.md#states`). Removing or
   renaming an anchor means updating every citation in the same CL.

## Requirements {#requirements}

- Session threads handle `CreateImage()` and `CreateImage2()` without
  blocking: no Vulkan call and no display coordinator round trip runs on a
  session thread. Both calls stay one-way, and a client may present an image
  right after creating it.
- Pending imports run in priority order (`#priority`): images named by a
  present that no frame has used yet go first, then images that frames have
  used, and never-named images last, because a named-but-unused image is what
  the next frame or a waiting present needs soonest. This order is a starting
  point, not a measured optimum: it is one of the scheduling policies that a
  later requirement keeps configurable and measurable, and a better order
  backed by a trace replaces it (#changing).
- The main thread never waits for anything a client controls. A frame waits
  only for imports already under way (at most one GPU job per worker or
  display `ImportImage()` calls already made) and runs any other import it
  needs itself.
- Every lock main may wait on, in the importer or a renderer, is a `std::mutex`,
  not `std::call_once()` or a condition-variable wait, so main lends its
  deadline to the holder through futex priority inheritance (`#lock-order`).
  `#lock-order` lists them.
- A frame never shows a hole for an image that is still importing.
- A failed GPU import closes the session for both Flatland1 and Flatland2. A
  failed display import is not a client error: the image is composited on the
  GPU instead.
- `ReleaseImage()`, `ReleaseImage2()`, `ReleaseImageImmediately()`, and
  `ReleaseImageImmediately2()` are safe while either import is in flight.
- Aside from keeping `reply_loop` running until `StopOnMain()` returns so
  `StopOnMain()` can drain in-flight display import replies on it
  (`#cleanup-on-main`), Scenic's shutdown order does not matter to the
  importer: `StopOnMain()` leaves nothing alive that needs the Vulkan device,
  and any call arriving after it is safe.
- Trusted sessions may present before their images finish importing; untrusted
  sessions wait for both imports (`#readiness`), because with `kOptimistic` a
  frame can reach images whose imports have not finished, and must then start
  and wait for their display imports or run their GPU imports inline on main;
  only trusted sessions may add that work to main's frame budget.
- Importing does not add to system-wide CPU over-subscription: by default
  display and GPU imports do not run concurrently, and main runs a GPU import
  only for a frame that needs it or in its idle window (`#dispatcher`).
- Scheduling policies are configurable, and the metrics needed to tune their
  defaults from real workloads exist without a code change (`#config`,
  `#metrics`).
- While no import is pending (queued or in flight), main's frames and vsyncs
  take none of the importer's mutexes, so a steady scene adds no lock traffic
  to main (`#no-blocking-under-locks`).

## Every image is GPU-readable {#gpu-readable}

Scenic must be able to composite any client image on the GPU, because any
frame can fall back to GPU composition: too many layers, an image
the display cannot scan out, a layout the coordinator rejects, or a failed
display import. So `ImportBufferCollection()` binds the renderer's Vulkan
constraints to a plain duplicate of the client's token, and sysmem must
satisfy them or fail the collection. Display constraints are only a
preference: when direct-to-display is enabled, they go on a prunable branch of
a sysmem token group, so if they cannot be met together with Vulkan's, sysmem
prunes the display branch instead of failing the collection.

This one choice shapes the rest of the design:

- An allocated collection is either `kDisplayCapable` (the display branch was
  kept) or `kGpuOnly` (it was pruned). No collection is display-only.
- Every allocated image gets a GPU import (`gpu: kNotStarted -> kQueued`).
  Only images in `kDisplayCapable` collections also get a display import; in
  `kGpuOnly` collections, display becomes `kUnsupported`.
- A failed GPU import is a client error, but a failed or unsupported display
  import only sends the frame down the GPU path (`#requirements`).

## Ownership and objects {#ownership}

The `:image_import` target provides `ImportedImage`, `ImportedCollection`, and
the two client interfaces, so sessions, the compositor, and renderers depend on
them without pulling in Escher or the importer's implementation. `:impl`
provides `ImageImporterImpl`, which only `App` (and tests) name.

- **`ImportedImage`** (one per client image, shared): owned jointly by the
  importer's registry until main-thread cleanup, the session, every published
  `UberStruct` naming it, and any queued job or in-flight reply. Holders keep
  the pointer, so no per-frame or per-present path looks an image up by ID. It
  holds the session's reply dispatcher weakly, so unreleased images do not
  keep a destroyed session's dispatcher alive.
- **`ImportedCollection`** (one per client buffer collection, shared by the
  importer and each live image of it): the renderer's collection slot, the
  sysmem allocation result, and a count of live images.
- **Destructors own nothing.** Either type can drop its last reference on any
  thread; unsampled `GpuImage` slots may be destroyed off main before
  `~ImportedImage()` runs, while sampled `GpuImage` slots and all other Vulkan
  and sysmem resources must be released on main (`#cleanup-on-main`). So those
  resources are released before destruction, and the destructors `FX_CHECK` that
  they were. An image is cleaned up once released with no import in flight; a
  collection is torn down once the client has released it and no live image
  remains.
- **`ImageImporter`** (held by each session; thread-safe) and
  **`EngineImageImporter`** (held by `DisplayCompositor`; main thread only)
  split the importer by caller.
- **`ImageImporterImpl`** implements both interfaces and
  `allocation::BufferCollectionImporter` for client images. `App` owns it by
  concrete type for construction, registration, and shutdown, and hands it out
  only through the interfaces.

## Renderer contract {#renderer-contract}

`flatland::Renderer` creates the opaque GPU slots that `ImportedCollection`
and `ImportedImage` hold:

- **`CreateGpuCollection()`** (main thread only) binds the sysmem token and
  sets Vulkan constraints. The slot is destroyed on main.
- **`CreateGpuImage()`** (any thread, so GPU imports can run on workers) must
  not touch single-threaded Escher state (`EscherWeakPtr`,
  `ResourceRecycler`); an implementation copies what it needs from Escher on
  main at construction. It checks the image's Vulkan memory requirements
  against the size of the collection's buffer VMOs (sysmem's `size_bytes`
  rounded up to whole pages, because small images can need more than
  `size_bytes`) and returns an image with memory already bound, so everything
  that can fail happens here.
- **Lazy texture wrap** (main thread only): the first `Render()` that samples
  a `kReady` image wraps it in an Escher texture. Wrapping cannot fail, so GPU
  `kReady` is terminal.
- **Slot destruction:** a sampled slot is destroyed only on main, so Escher's
  recycler can defer destruction until command buffers using it retire. An
  unsampled slot may be destroyed on any thread, because no command buffer
  references it. Some drivers make `vkDestroyImage()` a synchronous GPU round
  trip (hundreds of microseconds), so unsampled slots are destroyed off main
  when possible.

## States and threads {#states}

Each `ImportedImage` tracks independent display and GPU state machines plus
`released` and `allocated` bits:

```
display:  kNotStarted -> kQueued -> kImporting -> kReady | kFailed
          kNotStarted -> kUnsupported                     (GPU-only collection)
gpu:      kNotStarted -> kQueued -> kImporting -> kReady | kFailed
both:     kNotStarted -> kFailed                          (collection failed, bad vmo_index or size)
          any state but kImporting -> kReleased           (cleanup on main)
          kImporting -> kReady | kFailed -> kReleased     (released during in-flight import)
```

A state is terminal once it reaches `kReady`, `kFailed`, or `kUnsupported`;
after that, only main-thread cleanup changes it to `kReleased`.

### Single atomic word {#one-word}

Both `ImportState`s and both bits (`released`, `allocated`) are packed into
one 32-bit atomic word, updated only by full-word compare-and-swap loops that
recompute the transition decision on every retry. No thread decides an action
from two separate atomic loads. A new state or bit must fit in the word; if it
cannot, redesign this rule rather than add a second atomic that decisions read
alongside it.

Results written once (the GPU slot or error, readiness latencies) are stored
before the release-ordered CAS that publishes the terminal state, and read
after an acquire load of the word. The frame stamp (`#priority`) is a separate
relaxed atomic: a stale stamp only reorders background queues and never
affects state. The state word and the frame stamp share their own cache line:
they are the only fields written from several threads, and presents, frames, and
the job picker read them together, so one line transfer serves both. Keeping
them apart from the cold fields and main's per-frame counters means writes to
the atomics do not evict what main reads every frame.

### Swapping thread acts {#owner-acts}

The thread whose compare-and-swap first makes a condition true owns the
resulting side effect:

| Condition first made true by CAS | Action owned by the swapping thread |
|---|---|
| Side enters `kImporting` | Execute that display or GPU import |
| Side becomes terminal, or `allocated` becomes true | Resolve any `PendingEntry` waiters satisfied by the new word |
| GPU enters `kReady` or `kFailed` | If `kFailed`, post `on_failure_` to the image's reply dispatcher, even if `released`; otherwise drop it |
| `released` is true and neither side is `kImporting` | Run or post main-thread cleanup (never inline inside a frame) |

| Transition | Permitted thread |
|---|---|
| `kNotStarted -> kQueued`, `kUnsupported`, or `kFailed`; `allocated` bit set | Creating thread in `ImportImage()` or main in the allocation handler |
| `kQueued -> kImporting` (display) | Main thread |
| `kImporting -> kReady` or `kFailed` (display) | Coordinator reply loop thread (main, or dedicated display reply thread) |
| `kQueued -> kImporting` (GPU) | GPU worker thread, or main thread |
| `kImporting -> kReady` or `kFailed` (GPU) | Thread that transitioned GPU to `kImporting` |
| `released` bit set | Any thread in `ReleaseImage()` |
| Any non-`kImporting` state `-> kReleased` | Main thread in cleanup or `StopOnMain()` |
| Frame stamp relaxed store | Session thread in `SignalWhenReady()` or main thread in `OnFrameCommitted()` |

Once a side reaches `kReady`, only main-thread cleanup (which never runs
inside a frame) can change it to `kReleased`. A main-thread frame reader that
observes `kReady` may therefore read `tiling_type()` or `gpu_image()` for the
remainder of the frame without further synchronization. For images a frame
references, `released` also remains stable throughout the frame: immediate
releases run on the main thread (and cannot interrupt an active frame), and all
other client releases are fence-gated until no frame references the image.

### Lock order and priority inheritance {#lock-order}

Every lock main may wait on is a `std::mutex` (`#requirements`). Locks are
acquired in this order, never in reverse:

1. `ImportedImage::gpu_run_mutex_` (per image): held across a whole GPU import
   job, so main waiting for that image in `PrepareForGpuComposition()` lends
   its deadline to the thread running it.
2. `VkGpuCollection::properties_mutex` (per collection): never acquires
   importer locks.
3. `ImageImporterImpl::collections_mutex_`
4. `ImageImporterImpl::queue_mutex_`
5. `ImageImporterImpl::pending_signal_mutex_`

A thread holding `gpu_run_mutex_` releases it right after the terminal CAS,
before taking `queue_mutex_` or `pending_signal_mutex_`. Main waits on a GPU
import only through `gpu_run_mutex_`, so priority inheritance lends main's
deadline to a worker that a frame catches mid-import. That helps only once a
frame is already waiting, so GPU workers also run under a bounded deadline role
of their own: background imports then make progress under fair-scheduled load,
finishing before frames need them and without starving presents that wait on
readiness events, while the cap keeps a burst of imports from starving main.
Main waits on the optional display reply thread through `display_import_done_`,
which priority inheritance does not cross, so that thread has a deadline role
too. Both roles' budgets are placeholders until traces of the threads' CPU time
size them (`core.profiles`).

### Short, non-blocking critical sections {#no-blocking-under-locks}

Nothing under `collections_mutex_`, `queue_mutex_`, or `pending_signal_mutex_`
blocks on a FIDL round trip, Vulkan call, or kernel event. Two
condition-variable waits use these locks, and both release their lock while
waiting: GPU workers idling on `gpu_queue_nonempty_` (`queue_mutex_`), and main
waiting for display replies on the reply thread (`pending_signal_mutex_`).

Critical sections are bounded by the images of one present, one frame, or one
collection's waiting list. Queue operations scan linearly (`O(queued jobs)`),
which is cheap only because the queues hold pending imports, not all live
images; a change that lets them grow with live images needs a different
structure.

Two exceptions walk every live image under `collections_mutex_` alone: the lazy
`live_state` Inspect node, whose callback runs only when something reads
Scenic's Inspect tree (rarely, and never on a frame or import path), and
`StopOnMain()`, after the workers are joined. Each image costs two loads.

Main learns whether any work is queued from an atomic count of queued jobs,
which the queue helpers update under `queue_mutex_`, and it reads and writes
`idle_` without a lock (main is its only writer). It takes `queue_mutex_` only
to act on queued jobs. Locks outside the importer that main takes per frame,
such as Inspect's, are outside this rule.

### Main-thread cleanup and teardown {#cleanup-on-main}

Vulkan and sysmem teardown for images and collections runs on main, and never
inside a frame, so a frame's `kReady` images stay valid (`#owner-acts`) and
Escher's single-threaded state is touched only on main.

- **Sysmem participants:** a collection registers zero-constraint sysmem
  participants to learn when allocation finishes. Each releases its channel as
  soon as allocation completes (or at teardown), so closing it never makes
  sysmem fail the client's collection.
- **Image cleanup:** the importer's registry keeps every image alive until
  main has cleaned it up, so the last reference never drops before cleanup
  (`#ownership`). Cleanup runs inline when the release happens on main with no
  import in flight; otherwise the releasing thread, worker, or reply handler
  posts it to main's dispatcher, even when main is pumping the reply loop
  inside a frame.
- **`StopOnMain()`:** requires the reply loop to still be running, because it
  waits for in-flight display imports. It stops new work, joins workers,
  drains display replies, and cleans up every image and collection, leaving
  nothing that needs the Vulkan device or calls back into the importer.
- **Off-main GPU image destruction:** cleanup hands unsampled GPU slots
  (`#renderer-contract`) to the GPU workers, which destroy them only when no
  GPU import is eligible, so reaping never delays an import that could start
  (though one that becomes eligible mid-destroy waits for it). With no
  workers, or once `StopOnMain()` has begun, main destroys them inline.
  Collection teardown does not wait for reaping: Vulkan images and memory may
  outlive the buffer collection they came from, as sampled slots already do
  through Escher's recycler.

## Priority {#priority}

Each image carries a frame stamp: zero until a present names it, then the
frame that present targets (flagged as named by a present), then the last
frame that used it while an import is unfinished. Jobs fall into three
classes, highest priority first:

1. **Named by an unapplied or current present.** A `kWaitForImports` image
   keeps both its jobs here until both finish. A `kOptimistic` image keeps
   only the job of the predicted next-frame kind here, and only until its
   target frame passes.
   - **Optimistic first, without aging:** within the class, `kOptimistic` jobs
     (newest frame first) strictly outrank `kWaitForImports` jobs (oldest
     first), so untrusted background presents never add latency to Composer
     HAL frames. The cost: a sustained trusted stream delays untrusted
     presents, which wait for their imports.
   - **Passive expiry:** an optimistic layer culled by clipping or occlusion
     never appears in a frame report, so nothing clears its flag. Comparing
     its target frame at pop time drops it to class 2 once that frame passes,
     with no per-frame work.
2. **Used by a frame, or optimistic and expired or not predicted.**
   Current-frame images first, then off-screen images in `off_screen_order`,
   then `kOptimistic` before `kWaitForImports`.
3. **Never named.** `kOptimistic` before `kWaitForImports`. Whether these run
   in the background at all is `never_shown_imports`.

Ties break by creation order.

## Readiness policy and failure {#readiness}

- **`kWaitForImports` (untrusted sessions):** a present waits until both
  imports are terminal, so an untrusted client cannot push its imports onto
  main during frames.
- **`kOptimistic` (trusted sessions):** a present waits only until the image's
  collection has allocated (or its GPU import has failed), so no frame meets
  an unallocated collection, while frames finish any remaining import on
  demand.
- **Failure:** terminal failures (`kFailed`, `kReleased`) satisfy the wait
  like success, so a failed import never stalls a session's `FenceQueue`.

## Dispatcher and configuration {#dispatcher}

- **Reply loop:** `CoordinatorProxy` calls are made on main, but its FIDL
  client is bound to `reply_loop`, which the owner keeps running through all
  frame methods and `StopOnMain()`. Main can then wait for display replies
  inside a frame without running unrelated main-loop tasks, which is why
  reply-triggered cleanup is posted to main. Replies run either on main, which
  pumps `reply_loop` whenever the coordinator channel is readable, or, with
  `display_reply_thread`, on a dedicated thread that notifies main when an
  import completes. The thread costs a deadline role but removes the delay of
  replies waiting for main (`#metrics`).
- **Interleaving (`import_kinds`):** by default display and GPU jobs do not
  run concurrently, to avoid CPU over-subscription. Main compares each queue's
  best eligible job, favoring the kind it predicts the next frame will use,
  and switches kinds only when no job of the active kind is in flight. The
  prediction comes from the last frame's outcome and from whether a present
  that has finished waiting for its imports names an image that cannot go
  direct to display. A present still waiting does not change it, so a
  fence-blocked present cannot demote another session's display imports or lose
  its hint to a frame it is not in. The cost: while it waits, that present's
  GPU imports get no boost from the hint, so they run after any display import
  in flight and any Class 1 display job queued for the predicted kind; such
  jobs come in bursts (new images), not every frame. Under
  `kPredictedKindFirst`, a GPU job does not start while a higher-ranked display
  job is queued, even if that display job is deferred (below): a display job
  starts only when no GPU job is running, so a multi-millisecond GPU job
  started during the deferral would delay that display `ImportImage()` call
  once the deferral ends. Other GPU jobs run during the deferral, since GPU
  imports do not contend with the display coordinator. The cost: a display job
  queued after such a GPU job starts waits for it to finish.
- **Pacing main's idle window:** idle-window display `ImportImage()` calls are
  capped (`display_import_cap`) and deferred after each frame until that frame
  is displayed (next bullet). While a wakeup is scheduled, main starts an
  idle-window job only if its predicted duration (a median of recent samples,
  seeded pessimistically until samples arrive) fits before the wakeup with
  margin, and retries a job it deferred once that wakeup passes. The dispatcher
  runs only as its own posted main-loop task, never inside a frame or another
  component's callback: the frame scheduler's wakeup is accurate only between
  tasks, and presents that arrived meanwhile must run first so they can move
  the wakeup earlier. For the same reason, with no GPU workers main runs at
  most one GPU job per task.
- **Deferring display calls until the frame is displayed:** the display
  coordinator serves every client, vsync, and render fence on one dispatcher
  and calls the engine's `ImportImage()` synchronously, so each display
  `ImportImage()` call stalls the whole coordinator while the engine imports
  (about 1 ms in traces). After each committed frame, main therefore defers
  idle-window `ImportImage()` calls until `OnVsync()` reports that frame's
  config stamp as displayed. Before then, an import can delay a GPU-composited
  frame's `SubmitConfig()`, which waits for the render fence, or the
  `OnVsync()` in which Scenic signals clients' release fences. The deferral
  also ends at the frame's target presentation time plus one vsync interval, so
  a dark display cannot defer calls forever, and at once when a frame is
  skipped. A frame's own `ImportImage()` calls are never deferred: it waits for
  their replies before committing.

### Configuration fields and policy string {#config}

Each knob controls a tradeoff that depends on the product's hardware and
workload. The `image_import_policy` keys, values, and defaults are documented
with the structured config in `src/ui/scenic/bin/meta/config.shard.cml`; every
other copy of the grammar is tied to it by a `LINT.IfChange` block, so adding or
changing a key updates all of them. A new knob adds a row here saying what it
trades.

| `Config` field | Trades |
|---|---|
| `enable_direct_to_display` | Display scanout of client images vs. always compositing on the GPU, with no display imports |
| `worker_pool_size` | GPU import throughput vs. threads competing for CPU (`0` runs GPU imports in main's idle window) |
| `import_kinds` | CPU contention vs. import latency |
| `off_screen_order` | Which off-screen images are ready first if shown again |
| `never_shown_imports` | Background CPU vs. latency when a never-presented image is first shown |
| `idle_after` | How soon after the last frame `when_idle` work may start |
| `display_import_cap` | Idle-window display import throughput vs. coordinator work in flight when a frame starts |
| `display_reply_thread` | Reply-handling delay on main vs. an extra deadline thread |

The policy string is parsed once at startup and any malformed string is fatal.
Product assembly exposes typed optional fields and formats whichever are set
into the `image_import_policy` string, so invalid product configurations fail at
schema deserialization instead of Scenic's boot. Scenic's parser tests
(`kPolicyTestCases` in `image_importer_impl_unittest.cc`) and assembly's
formatter tests (`ui.rs`) use the same policy strings, kept in sync with
`LINT.IfChange`; a new policy key or value adds cases to both.

## Tracing and Inspect {#metrics}

- Inspect shows, without a trace, whether import is healthy and how the
  policies perform: the resolved config, live counts (images, bytes, queued
  and in-flight jobs), failure and wasted-import counts, prediction accuracy,
  and histograms of time frames spent blocked on imports. These are what
  policy defaults are tuned against (`#config`).
- Traces show each image's life and every frame-path wait: one flow per image
  from `ImportImage()` through readiness and first use to cleanup (ending with
  a summary of its cost and use), and a slice for every wait in a frame
  method.
- Every Vulkan and FIDL call on the import, wrap, and release paths has its
  own `TRACE_DURATION`, so per-driver costs can be read directly from traces.
- A counter that marks a per-frame cost (`ImageImporter::FrameBlockedUs`) is
  zeroed at the next vsync, so each nonzero sample marks only the frame it
  delayed.
- New functionality that adds a frame-path wait or scheduling decision adds a
  trace signal; a new queue or resource lifetime adds an Inspect count; a new
  policy knob adds a metric that shows whether its setting helps.

## Not yet in the tree {#pending}

The following parts land with the session and engine cutover:
- Session, engine, and app cutover: `Flatland` session wiring (`kOptimistic`
  for trusted sessions, `FX_CHECK`ed to run on main, `kWaitForImports` for
  untrusted), `DisplayCompositor::RenderFrame()` direct-to-display and
  released-layer integration, passing the structured config parsed at `App`
  startup to `ImageImporterImpl` in `App::InitializeGraphics()`, and removal of
  `DisplayCompositor::lock_` and legacy client-image maps.

This section is deleted by the CL that lands the last of these parts
([fxb/580156981](https://fxbug.dev/580156981)).
