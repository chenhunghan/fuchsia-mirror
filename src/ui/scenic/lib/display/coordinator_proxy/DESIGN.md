# Display coordinator proxy design

This document is the design contract for `CoordinatorProxy`, Scenic's client of
`fuchsia.hardware.display/Coordinator`, and the `internal/` types behind it.
`coordinator_proxy.h` owns method signatures and per-method contracts; this
file states the rules that span methods, frames, and threads, and records why
the proxy avoids `CheckConfig()` round trips.

## Changing this code {#changing}

1. A change that contradicts a rule here amends the rule in the same CL, and
   the amended text names the fact that changed (a measurement, a bug, or an
   API change). The commit message says what the old rule was.
2. A change this document is silent about needs no amendment. If it adds a
   rule others must keep (a call order, a thread restriction, or a state), add
   the rule here.
3. Code cites this document by stable anchor (`DESIGN.md#cache`). Removing or
   renaming an anchor means updating every citation in the same CL.

## Requirements {#requirements}

- A frame that can go direct to display learns whether its layout is valid
  before it waits for any display image import, so a rejected layout falls
  back to GPU composition without paying for imports.
- Steady-state frames make no synchronous coordinator round trip on the main
  thread. In particular, a layout whose equivalence class has already passed
  `CheckConfig()` is not checked again ([#cache](#cache)).
- Only state that changed since the last commit is sent to the coordinator.
- The proxy's notion of applied state always matches the coordinator's
  committed state.

## Draft and applied state {#state}

Setters that correspond to coordinator config methods, including
`SetLayerImage()`, only update draft state in the proxy. Each layer and the
display-wide state keep a draft copy and an applied copy.

- Diffs between draft and applied state are sent, and later accepted, only for
  layers in the draft display list. `CommitConfig()` does not apply draft
  changes to undisplayed layers, and `DiscardConfig()` drops all draft changes
  (`coordinator.fidl`). A sent diff for an undisplayed layer would survive or
  vanish depending on later discards, so the proxy keeps that draft unsent until
  the layer is displayed.
- On commit, draft becomes applied. On rejection or discard, draft reverts to
  applied, so an abandoned layout is never sent later by accident.
- `SetLayerPrimaryConfig()` resets position and alpha and clears the bound
  image in the coordinator (`coordinator.fidl`), so a config diff also resends
  position and alpha, and the image is rebound before commit.

## Frame protocol {#frame}

A frame is validated and committed in two steps, so callers can stop between
them (for example, to import display images only after the layout passes):

1. `ValidateDraftLayout()` decides whether the draft layout is valid, using the
   mechanisms in [#cache](#cache), without requiring or sending layer images;
   only layout and display-list diffs are sent. A rejected layout leaves no diff
   pending in the coordinator and resets draft state (`#state`), so it is never
   committed by accident.
2. `CommitValidatedConfig()` sends `SetLayerImage2()` for every displayed image
   layer and then `CommitConfig()`, without checking again.
   `DiscardValidatedLayout()` ends the frame instead, sending `DiscardConfig()`
   and resetting draft state.

`ApplyConfig()` runs both steps back to back.

While a layout is validated (`has_validated_layout()`):

- Layout and display-list setters are fatal (`FX_CHECK`), because their diffs
  were already sent and checked.
- Calls that send no layout or display-list diff are allowed: `SetLayerImage()`,
  importing and releasing resources, and `DestroyLayer()` on an undisplayed
  layer. They leave the checked layout unchanged, so the validation still holds.
- `ReleaseImage()` of an image bound to a displayed layer must be followed by
  a replacement `SetLayerImage()` before `CommitValidatedConfig()`. Committing a
  displayed image layer with no image is fatal (`FX_CHECK`) in all builds,
  because the coordinator would otherwise close Scenic's channel
  asynchronously.

Until [fxb/449807074](https://fxbug.dev/449807074) is fixed, `SetLayerImage2()`
is resent for every displayed image layer on every commit, even when unchanged,
because otherwise vsync config stamps can be wrong (see
`Layer::SendImageDiffToCoordinator()`). `SetLayerImage2()` does not require
revalidation ([#coordinator-validation](#coordinator-validation)).

## Coordinator validation {#coordinator-validation}

What `coordinator.fidl` guarantees, which the proxy relies on:

- Most draft setters require revalidating the draft with `CheckConfig()`;
  `SetLayerImage2()` does not (the `CheckConfig()` documentation).
- `CommitConfig()` of a draft that cannot be committed fails silently, so a
  client must know its draft is valid before committing.

How the current coordinator implements this
(`src/graphics/display/drivers/coordinator/client.cc`). This is not part of the
contract and may change, for example with
[fxb/397427767](https://fxbug.dev/397427767):

- Each client has a validated flag for its draft. The display and layer
  setters that change layout clear it; these are exactly the layout diffs that
  `ValidateDraftLayout()` sends ([#frame](#frame)). `SetLayerImage2()`, resource
  imports and releases, and `DestroyLayer()` leave it unchanged.
- `CheckConfig()` sets the flag to its result. `DiscardConfig()` sets it, since
  the draft reverts to the committed configuration.
- `CommitConfig()` with the flag cleared re-runs the check itself and drops the
  commit if it fails.

Consequences for the proxy:

- A `ValidateDraftLayout()` that sends no layout diffs leaves the coordinator's
  draft validated by the last `CheckConfig()`, commit, or `DiscardConfig()`.
- A cached success that sent layout diffs ([#cache](#cache)) commits a draft the
  coordinator has not validated, so the proxy relies on the coordinator's
  re-check at commit, which the contract does not promise ([#cache](#cache)
  records the plan before strict mode).

## Avoiding CheckConfig() {#cache}

`CheckConfig()` is a synchronous round trip on the main thread, and a direct
to display frame cannot start waiting on display imports until its layout is
known to be valid, so avoiding it is central to frame latency. Three
mechanisms apply, in order:

1. **Heuristics** (opt-in via `CheckConfigHeuristics`): reject layouts known
   to fail on particular hardware without asking the coordinator. They are
   hardware-specific, so they are off unless a product enables them; products
   whose clients often use layers that are not full-screen, such as Android
   phones, leave them off because today's heuristic would force those frames
   to GPU composition. Heuristic rejections are not cached, so small
   variations of a failing layout do not thrash the cache.
2. **Equivalence classes** (`internal::DisplayEquivalence`): the cache key is
   the subset of display state that can change the result of `CheckConfig()`:
   the ordered layer list (each layer's `LayerEquivalence`), the display mode,
   and color conversion. Image layers key on destination and source
   rectangles, source transformation, image dimensions and tiling, blend mode,
   and `AlphaRange` (zero, strictly between zero and one, or one) rather than
   the exact alpha value. Color layers key on color and destination.
3. **Result cache** (`internal::CheckConfigCache`, a bounded LRU): stores
   both successes and failures by equivalence class. A cached failure rejects
   the layout without sending any diff.

A cached success is authoritative: `ValidateDraftLayout()` skips
`CheckConfig()` even when it has just sent layout diffs, for example an alpha
change within one `AlphaRange` or a return to a recently used layout. The
display team has agreed that configurations in the same equivalence class
pass or fail `CheckConfig()` together. The coordinator then re-checks the
unvalidated draft itself at `CommitConfig()`, without a round trip from Scenic
([#coordinator-validation](#coordinator-validation)).
[fxb/397427767](https://fxbug.dev/397427767) proposes making that commit an
error instead ("strict mode"). Before it lands, a cache hit that sent layout
diffs should send `CheckConfig()` asynchronously, without waiting for the reply:
the coordinator handles messages in order, so the draft is validated before
`CommitConfig()` arrives (`SetLayerImage2()` does not require revalidation). A
failed reply means the equivalence classes are wrong. This also moves the
coordinator's re-check out of its commit path; a trace of task switching
measured that re-check at about 70 µs per commit, with at least 1.8 ms between
commit and vsync, so it is not done yet for latency alone.

Rectangles are compared exactly, so animated or scrolling layouts miss the cache
on every frame. In a trace of task switching, 90 of 94 `CheckConfig()` round
trips (about 200 µs each, up to 1.3 ms) followed a position change, mostly
during animations.
Coarser position classes, like `AlphaRange`, would remove most of these, but
need the display team to agree on which parameters, like position, a config's
success does not depend on ([fxb/438306195](https://fxbug.dev/438306195),
blocking [fxb/446042966](https://fxbug.dev/446042966)).

## Calls and replies {#threads}

- Frame-path calls are one-way or asynchronous, because the frame path runs on
  Scenic's main thread and must not wait on the coordinator. The tolerated
  exception is `CheckConfig()` on a cache miss, which `#cache` exists to make
  rare. Until [fxb/580156981](https://fxbug.dev/580156981) moves Scenic's
  client-image imports to the asynchronous `ImportImage()`, those imports also
  block the frame path, before the layout is validated (`#requirements`). Any
  other blocking call is made only off the frame path, and a new one needs the
  same justification.
- Calls are made from Scenic's main thread. Until
  [fxb/580156981](https://fxbug.dev/580156981) moves client-image imports into
  `ImageImporter`, `DisplayCompositor` also calls the proxy from Flatland
  session threads; it serializes those calls under its own lock, because the
  proxy does not lock against concurrent calls.
- Replies run on the dispatcher the FIDL client was bound to at construction,
  which need not be the calling thread, and a canceled reply can run after the
  proxy is destroyed. So the proxy keeps the few fields its reply handlers
  update in a separately owned, mutex-protected object. Completion callbacks run
  on that same dispatcher; a caller that needs to act on its own thread posts
  the work there itself.
- A displayed layer may use an asynchronously imported image only after its
  import succeeds. The proxy forgets failed imports itself, so callers never
  release them.
- An owner that pumps the reply dispatcher itself may wait on `channel_handle()`
  only on the thread that pumps it, and cancels the wait before each pump: the
  handle is borrowed from the FIDL binding, which closes it while tearing down
  on that dispatcher, before the `SetOnFidlErrorCallback()` callback runs or, on
  `AsyncTeardown()`, with no callback at all. This is the current
  `AsyncClientBinding::FinishTeardown()`
  (`sdk/lib/fidl/cpp/wire/async_binding.cc`), not a `fidl::WireSharedClient`
  guarantee.

## Limitations {#limitations}

Only one display is supported (`CheckDisplayId()`). Nothing known rules out
more, but the equivalence key, draft and applied state, and validation all
assume one display, and the ramifications (for example, one `CheckConfig()`
spanning displays, or per-display cache keys) have not been explored.

## Tracing and Inspect {#metrics}

- Inspect holds what is needed to diagnose a report without a trace: paired
  counters whose ratio shows whether the proxy is avoiding work (API calls
  received versus sent, `CheckConfig()` calls sent versus skipped), and live
  resource counts that reveal leaks (for example, `Imported Images (current)`).
- Trace marks each frame-path decision and its reason: every frame-path
  coordinator round trip is a slice (`display::CoordinatorProxy::FidlCheckConfig`),
  and decisions carry an arg or instant (`cache_hit` on `ValidateDraftLayout()`,
  `scenic_d2d_failed: ...` with the rejection reason).
- New functionality that adds a frame-path decision or cost adds a trace
  signal; new resource lifetimes add an Inspect count.
