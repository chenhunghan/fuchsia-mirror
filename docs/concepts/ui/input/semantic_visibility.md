# Semantic Visibility {#semantic-visibility}

## Background

When a user interacts with a part of the screen, Scenic runs a hit test from front to back to
determine which View should receive the interaction:

* **Regular hit testing** finds the View under touch or mouse input.
* **[Accessibility hit testing](/docs/concepts/accessibility/semantics.md#hit-testing)** (also
  called **semantic hit testing**) is used by accessibility features such as the Screen Reader to
  find the UI element at a point on screen.

Each [`HitRegion`](https://fuchsia.dev/reference/fidl/fuchsia.ui.composition#HitRegion) has a
[`HitTestInteraction`](https://fuchsia.dev/reference/fidl/fuchsia.ui.composition#HitTestInteraction)
setting that controls how these two hit tests treat it:

* **Semantically visible (`DEFAULT`):** Interacts with **both** regular and semantic hit testing.
* **Semantically invisible (`SEMANTICALLY_INVISIBLE`):** Interacts with **regular** hit testing,
  but is **skipped** by semantic hit testing. When a semantic hit test skips a semantically
  invisible region on a View, it keeps looking behind that region only within **that same View**
  and any **Views embedded inside it**. It never passes through to a parent or sibling View behind
  it.

## Smart Display Case Study

To better understand why we need semantic visibility as a property, it can help to look at a
motivating example. For instance, consider the Smart Display product, which is essentially a Flutter
app that can sometimes launch child apps, such as a web view.

The parent Flutter application can launch a child web view containing, say, a YouTube player. The
situation arises where some hits should go to YouTube (e.g. playing or pausing the video), while
others should be handled by the parent process instead, for instance a gesture to dismiss the player
altogether. Since both views are full screen, this is not so straightforward.

One solution is for the parent Flutter View to place a full-screen, transparent, semantically
invisible hit region in front of the embedded YouTube View. Let's call this region the **input
shield**. With the input shield, Flutter intercepts all regular input (even touches over the YouTube
player) so it can handle system gestures or pass touches along to YouTube. The input shield must
belong to the parent View that embeds YouTube (or an ancestor View), not a separate sibling View,
because semantic hit tests only look through a shield into that same View and Views embedded inside
it.

In order to better understand why this input shield needs to be semantically invisible, let's
consider what would happen if it weren't by looking at accessibility's explore mode. This mode is
when the user is dragging their finger across the screen, and the accessibility manager reads out
whatever content is under the user's finger. These accessibility hit tests want to ignore the input
shield, and instead read out what the "real" content is underneath it.

By making the input shield semantically invisible, that's exactly what happens. Accessibility hit
tests skip the parent View's input shield and reach the semantically _visible_ YouTube player
embedded underneath it.

## Examples

### Simple case

Suppose `Parent View` embeds a blue `Child View` (semantically visible) and places a green shield
hit region (`Shield owned by Parent View`, semantically invisible) in front of the left portion of
`Child View`:

* **Point A (covered by the shield owned by `Parent View`):** Regular hit tests hit the front-most
  hit region (`Shield owned by Parent View`, semantically invisible), while semantic hit tests skip
  that shield and hit `Child View` (semantically visible) underneath.
* **Point B (directly on the uncovered `Child View`):** Because semantically visible hit regions
  interact with *both* regular and semantic hit testing, both hit tests hit `Child View`.

Semantic hit tests at Point A reach `Child View` only because the green shield is owned by the
`Parent View` that embeds it. If the green region were instead a separate child or sibling View
(with nothing semantically visible inside it) placed over `Child View`, semantic hit tests at Point
A would stop at that green View and return no hit rather than exposing `Child View` behind it.

![Diagram showing Parent View embedding a blue semantically visible Child View and placing a green semantically invisible shield owned by Parent View over its left half: at Point A under the shield owned by Parent View, regular hit tests hit the shield owned by Parent View while semantic hit tests hit Child View; at Point B on the uncovered Child View, both regular and semantic hit tests hit Child View.](images/simple_case.png)

_Image Description: `Parent View` embeds a blue `Child View` (semantically visible) and places a
green `Shield owned by Parent View` (semantically invisible) in front of its left portion. At Point
A (shielded), regular hit tests hit `Shield owned by Parent View` (semantically invisible) while
semantic hit tests skip the shield and hit `Child View` (semantically visible) underneath. At Point
B (unshielded), both regular and semantic hit tests hit `Child View` (semantically visible)._

### Smart Display Case

From front to back, the Smart Display scene has two Views (`Parent View` for Flutter, and `Child
View` for YouTube embedded inside `Parent View`) arranged in three layers:

1. **`Shield owned by Parent View` (semantically invisible):** A full-screen transparent hit region
   at the front of `Parent View` (Flutter).
2. **`Child View` (YouTube, semantically visible):** The red child View in the middle, embedded
   inside `Parent View`.
3. **`Background owned by Parent View` (Flutter, semantically visible):** The blue hit region at
   the back of `Parent View`.

For a tap over `Child View` (YouTube), regular hit tests hit the front-most layer (`Shield owned by
Parent View`), while semantic hit tests skip that shield and hit `Child View` underneath (or the
background owned by `Parent View` if the tap is outside `Child View`).

![Diagram of the Smart Display scene from front to back: Parent View (Flutter) embeds a red Child View (YouTube, semantically visible) over the blue background owned by Parent View (semantically visible), with a transparent dashed shield owned by Parent View (semantically invisible) in front of both. For a tap over Child View, regular hit tests hit the shield owned by Parent View while semantic hit tests hit Child View.](images/smart_display_case.png)

_Image Description: `Parent View` (Flutter) has a blue background (semantically visible) at the
back, embeds the red `Child View` (YouTube, semantically visible) in the middle, and places a
transparent `Shield owned by Parent View` (semantically invisible) in front of both. For a tap over
`Child View`, regular hit tests hit `Shield owned by Parent View` while semantic hit tests skip the
shield and hit `Child View` underneath._
