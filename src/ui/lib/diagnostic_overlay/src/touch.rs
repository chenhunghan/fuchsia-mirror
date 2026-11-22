// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;
use std::sync::Arc;

use fidl_fuchsia_math as fmath;
use fidl_fuchsia_ui_composition as ui_comp;
use fidl_fuchsia_ui_pointer as fptr;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::channel::mpsc::UnboundedSender;

use crate::OverlayEvent;
use crate::renderer::{
    BOTTOM_BAR_BUF_H, BOTTOM_BAR_BUF_Y, CLOSE_BUTTON_BUF_H, CLOSE_BUTTON_BUF_W, CLOSE_BUTTON_BUF_X,
    CLOSE_BUTTON_BUF_Y, CONFIRM_BTN_BUF_H, CONFIRM_BTN_BUF_W, CONFIRM_BTN_BUF_Y,
    CONFIRM_CANCEL_BTN_BUF_X, CONFIRM_CLOSE_BTN_BUF_X, DEFAULT_OVERLAY_OPACITY, HEADER_BUF_HEIGHT,
    MAX_OVERLAY_OPACITY, MIN_OVERLAY_OPACITY, OVERLAY_BUFFER_HEIGHT, OVERLAY_BUFFER_WIDTH,
    SLIDER_TRACK_BUF_H, SLIDER_TRACK_BUF_W, SLIDER_TRACK_BUF_X, SLIDER_TRACK_BUF_Y,
    compute_overlay_placement,
};

/// Extra touch-target padding to the left of the visual `[X]` close button.
const CLOSE_HIT_LEFT_PAD: i32 = 10;

/// Draggable top header bar hit-test rectangle inside the `480x276` overlay buffer
/// (covers the top bar to the left of the `[X]` close button).
pub const HEADER_DRAG_HIT_BUF_X: f32 = 0.0;
pub const HEADER_DRAG_HIT_BUF_Y: f32 = 0.0;
pub const HEADER_DRAG_HIT_BUF_W: f32 = (CLOSE_BUTTON_BUF_X - CLOSE_HIT_LEFT_PAD) as f32;
pub const HEADER_DRAG_HIT_BUF_H: f32 = HEADER_BUF_HEIGHT as f32;

/// Touch hit-test rectangle of the `[X]` close button inside the `480x276` overlay buffer
/// (includes padding around the visual button box).
pub const CLOSE_HIT_BUF_X: f32 = HEADER_DRAG_HIT_BUF_W;
pub const CLOSE_HIT_BUF_Y: f32 = 0.0;
pub const CLOSE_HIT_BUF_W: f32 = OVERLAY_BUFFER_WIDTH as f32 - CLOSE_HIT_BUF_X;
pub const CLOSE_HIT_BUF_H: f32 = HEADER_BUF_HEIGHT as f32;

/// Touch hit-test rectangle of the opacity slider inside the `480x276` overlay buffer.
pub const SLIDER_HIT_BUF_X: f32 = 0.0;
pub const SLIDER_HIT_BUF_Y: f32 = BOTTOM_BAR_BUF_Y as f32;
pub const SLIDER_HIT_BUF_W: f32 = OVERLAY_BUFFER_WIDTH as f32;
pub const SLIDER_HIT_BUF_H: f32 = BOTTOM_BAR_BUF_H as f32;

const _: () = {
    assert!(HEADER_DRAG_HIT_BUF_W as i32 == 424);
    assert!(HEADER_DRAG_HIT_BUF_H as i32 == 46);
    assert!(CLOSE_HIT_BUF_X as i32 == 424);
    assert!(CLOSE_HIT_BUF_W as i32 == 56);
    assert!(CLOSE_HIT_BUF_H as i32 == 46);
    assert!(SLIDER_HIT_BUF_Y as i32 == 232);
    assert!(SLIDER_HIT_BUF_W as i32 == 480);
    assert!(SLIDER_HIT_BUF_H as i32 == 44);
    assert!(HEADER_DRAG_HIT_BUF_W + CLOSE_HIT_BUF_W == OVERLAY_BUFFER_WIDTH as f32);
    assert!(CLOSE_HIT_BUF_X as i32 <= CLOSE_BUTTON_BUF_X);
    assert!(
        CLOSE_HIT_BUF_X as i32 + CLOSE_HIT_BUF_W as i32 >= CLOSE_BUTTON_BUF_X + CLOSE_BUTTON_BUF_W
    );
    assert!(
        CLOSE_BUTTON_BUF_Y >= 0 && CLOSE_BUTTON_BUF_Y + CLOSE_BUTTON_BUF_H <= HEADER_BUF_HEIGHT
    );
    assert!(
        BOTTOM_BAR_BUF_Y >= HEADER_BUF_HEIGHT
            && BOTTOM_BAR_BUF_Y + BOTTOM_BAR_BUF_H == OVERLAY_BUFFER_HEIGHT as i32
    );
    assert!(
        CONFIRM_CLOSE_BTN_BUF_X >= 0
            && CONFIRM_CLOSE_BTN_BUF_X + CONFIRM_BTN_BUF_W <= CONFIRM_CANCEL_BTN_BUF_X
            && CONFIRM_CANCEL_BTN_BUF_X + CONFIRM_BTN_BUF_W <= OVERLAY_BUFFER_WIDTH as i32
    );
    assert!(
        CONFIRM_BTN_BUF_Y >= HEADER_BUF_HEIGHT
            && CONFIRM_BTN_BUF_Y + CONFIRM_BTN_BUF_H <= BOTTOM_BAR_BUF_Y
    );
    assert!(
        SLIDER_TRACK_BUF_X >= 0
            && SLIDER_TRACK_BUF_X + SLIDER_TRACK_BUF_W <= OVERLAY_BUFFER_WIDTH as i32
            && SLIDER_TRACK_BUF_Y >= BOTTOM_BAR_BUF_Y
            && SLIDER_TRACK_BUF_Y + SLIDER_TRACK_BUF_H <= BOTTOM_BAR_BUF_Y + BOTTOM_BAR_BUF_H
    );
};

/// Hit-test bounds in logical overlay view coordinates for interactive controls on the HUD card.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverlayHitTargets {
    pub overlay_origin_x: f32,
    pub overlay_origin_y: f32,
    pub header_drag_rect: fmath::RectF,
    pub close_rect: fmath::RectF,
    pub slider_hit_rect: fmath::RectF,
    pub slider_track_rect: fmath::RectF,
    pub confirm_close_rect: Option<fmath::RectF>,
    pub confirm_cancel_rect: Option<fmath::RectF>,
}

/// Shared hit-test bounds of the overlay controls when visible, or `None` when hidden.
pub type SharedOverlayHitTargets = Arc<Mutex<Option<OverlayHitTargets>>>;

pub fn new_overlay_hit_targets() -> SharedOverlayHitTargets {
    Arc::new(Mutex::new(None))
}

/// Computed logical placement, Flatland [`ui_comp::HitRegion`]s, and view-space
/// [`OverlayHitTargets`] for the overlay HUD card.
#[derive(Debug, Clone)]
pub struct OverlayLayout {
    pub x: i32,
    pub y: i32,
    pub dest_w: u32,
    pub dest_h: u32,
    pub confirm_close: bool,
    pub local_hit_regions: Vec<ui_comp::HitRegion>,
    pub targets: OverlayHitTargets,
}

/// Computes the clamped overlay placement, `overlay_transform_id`-local Flatland `HitRegion`s, and
/// logical view-space `OverlayHitTargets`.
pub fn compute_overlay_layout(
    display_size: fmath::SizeU,
    custom_origin: Option<(i32, i32)>,
    confirm_close: bool,
) -> OverlayLayout {
    let (x, y, dest_w, dest_h) = compute_overlay_placement(display_size, custom_origin);

    let scale_x = dest_w as f32 / OVERLAY_BUFFER_WIDTH as f32;
    let scale_y = dest_h as f32 / OVERLAY_BUFFER_HEIGHT as f32;

    let to_local_rect = |bx: f32, by: f32, bw: f32, bh: f32| fmath::RectF {
        x: bx * scale_x,
        y: by * scale_y,
        width: bw * scale_x,
        height: bh * scale_y,
    };
    let to_view_rect = |local: &fmath::RectF| fmath::RectF {
        x: x as f32 + local.x,
        y: y as f32 + local.y,
        width: local.width,
        height: local.height,
    };
    let hit_region =
        |region| ui_comp::HitRegion { region, hit_test: ui_comp::HitTestInteraction::Default };

    let local_header_drag_rect = to_local_rect(
        HEADER_DRAG_HIT_BUF_X,
        HEADER_DRAG_HIT_BUF_Y,
        HEADER_DRAG_HIT_BUF_W,
        HEADER_DRAG_HIT_BUF_H,
    );
    let local_close_rect =
        to_local_rect(CLOSE_HIT_BUF_X, CLOSE_HIT_BUF_Y, CLOSE_HIT_BUF_W, CLOSE_HIT_BUF_H);
    let local_slider_hit_rect =
        to_local_rect(SLIDER_HIT_BUF_X, SLIDER_HIT_BUF_Y, SLIDER_HIT_BUF_W, SLIDER_HIT_BUF_H);
    let local_slider_track_rect = to_local_rect(
        SLIDER_TRACK_BUF_X as f32,
        SLIDER_TRACK_BUF_Y as f32,
        SLIDER_TRACK_BUF_W as f32,
        SLIDER_TRACK_BUF_H as f32,
    );

    let mut local_hit_regions = Vec::with_capacity(if confirm_close { 5 } else { 3 });
    local_hit_regions.push(hit_region(local_header_drag_rect));
    local_hit_regions.push(hit_region(local_close_rect));
    local_hit_regions.push(hit_region(local_slider_hit_rect));

    let (confirm_close_rect, confirm_cancel_rect) = if confirm_close {
        let local_confirm_close = to_local_rect(
            CONFIRM_CLOSE_BTN_BUF_X as f32,
            CONFIRM_BTN_BUF_Y as f32,
            CONFIRM_BTN_BUF_W as f32,
            CONFIRM_BTN_BUF_H as f32,
        );
        let local_confirm_cancel = to_local_rect(
            CONFIRM_CANCEL_BTN_BUF_X as f32,
            CONFIRM_BTN_BUF_Y as f32,
            CONFIRM_BTN_BUF_W as f32,
            CONFIRM_BTN_BUF_H as f32,
        );
        local_hit_regions.push(hit_region(local_confirm_close));
        local_hit_regions.push(hit_region(local_confirm_cancel));
        (Some(to_view_rect(&local_confirm_close)), Some(to_view_rect(&local_confirm_cancel)))
    } else {
        (None, None)
    };

    let targets = OverlayHitTargets {
        overlay_origin_x: x as f32,
        overlay_origin_y: y as f32,
        header_drag_rect: to_view_rect(&local_header_drag_rect),
        close_rect: to_view_rect(&local_close_rect),
        slider_hit_rect: to_view_rect(&local_slider_hit_rect),
        slider_track_rect: to_view_rect(&local_slider_track_rect),
        confirm_close_rect,
        confirm_cancel_rect,
    };

    OverlayLayout { x, y, dest_w, dest_h, confirm_close, local_hit_regions, targets }
}

/// Returns true if `(vx, vy)` lies within `rect`.
pub fn rect_contains_point(rect: &fmath::RectF, vx: f32, vy: f32) -> bool {
    (rect.x..=(rect.x + rect.width)).contains(&vx)
        && (rect.y..=(rect.y + rect.height)).contains(&vy)
}

/// Maps a view-space X coordinate `vx` along `slider_track_rect` to an opacity value in
/// `[MIN_OVERLAY_OPACITY, MAX_OVERLAY_OPACITY]`.
pub fn opacity_from_slider_x(vx: f32, slider_track_rect: &fmath::RectF) -> f32 {
    if !vx.is_finite() || slider_track_rect.width <= 0.0 {
        return DEFAULT_OVERLAY_OPACITY;
    }
    let t = ((vx - slider_track_rect.x) / slider_track_rect.width).clamp(0.0, 1.0);
    MIN_OVERLAY_OPACITY + t * (MAX_OVERLAY_OPACITY - MIN_OVERLAY_OPACITY)
}

/// Transforms `viewport_coords` into logical view coordinates using the column-major 3x3
/// homogeneous matrix `viewport_to_view_transform`.
pub fn viewport_to_view_coordinates(
    viewport_coords: [f32; 2],
    viewport_to_view_transform: &[f32; 9],
) -> (f32, f32) {
    let [x, y] = viewport_coords;
    let m = viewport_to_view_transform;
    let xp = m[0] * x + m[3] * y + m[6];
    let yp = m[1] * x + m[4] * y + m[7];
    let wp = m[2] * x + m[5] * y + m[8];
    if wp != 0.0 { (xp / wp, yp / wp) } else { (xp, yp) }
}

#[derive(Clone, Copy, Debug)]
enum ClaimedTouchInteraction {
    ButtonTap,
    HeaderDrag { grab_offset_x: f32, grab_offset_y: f32 },
    OpacitySlider,
}

fn classify_touch_add(
    targets: &OverlayHitTargets,
    vx: f32,
    vy: f32,
    sender: &UnboundedSender<OverlayEvent>,
    latest_opacity: &mut Option<f32>,
) -> Option<ClaimedTouchInteraction> {
    if rect_contains_point(&targets.close_rect, vx, vy) {
        let _ = sender.unbounded_send(OverlayEvent::CloseRequested);
        Some(ClaimedTouchInteraction::ButtonTap)
    } else if targets.confirm_close_rect.is_some_and(|r| rect_contains_point(&r, vx, vy)) {
        let _ = sender.unbounded_send(OverlayEvent::CloseConfirmed);
        Some(ClaimedTouchInteraction::ButtonTap)
    } else if targets.confirm_cancel_rect.is_some_and(|r| rect_contains_point(&r, vx, vy)) {
        let _ = sender.unbounded_send(OverlayEvent::CloseCancelled);
        Some(ClaimedTouchInteraction::ButtonTap)
    } else if rect_contains_point(&targets.slider_hit_rect, vx, vy) {
        *latest_opacity = Some(opacity_from_slider_x(vx, &targets.slider_track_rect));
        Some(ClaimedTouchInteraction::OpacitySlider)
    } else if rect_contains_point(&targets.header_drag_rect, vx, vy) {
        Some(ClaimedTouchInteraction::HeaderDrag {
            grab_offset_x: vx - targets.overlay_origin_x,
            grab_offset_y: vy - targets.overlay_origin_y,
        })
    } else {
        None
    }
}

/// Spawns a watcher on the host root view's `TouchSource` that claims touch interactions landing
/// inside the overlay's interactive controls (top header drag bar, `[X]` close button, close
/// confirmation buttons, and bottom opacity slider) and rejects all other touches with
/// `TouchResponseType::No` so underlying child views receive them unimpeded.
///
/// Multiple `EventPhase::Change` samples within a single `TouchSource::Watch` batch are coalesced
/// so at most one `MoveRequested` and one `SetOpacity` event are sent per batch.
pub fn spawn_touch_source_watcher(
    touch_source: fptr::TouchSourceProxy,
    hit_targets: SharedOverlayHitTargets,
    sender: UnboundedSender<OverlayEvent>,
) -> fasync::Task<()> {
    fasync::Task::local(async move {
        let mut pending_responses: Vec<fptr::TouchResponse> = Vec::new();
        let mut current_transform: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
        let mut claimed_interactions: HashMap<fptr::TouchInteractionId, ClaimedTouchInteraction> =
            HashMap::new();

        while let Ok(events) = touch_source.watch(&pending_responses).await {
            pending_responses.clear();
            pending_responses.reserve(events.len());
            let maybe_targets = *hit_targets.lock();
            let mut latest_move: Option<(i32, i32)> = None;
            let mut latest_opacity: Option<f32> = None;

            for event in &events {
                if let Some(view_params) = &event.view_parameters {
                    current_transform = view_params.viewport_to_view_transform;
                }
                if let Some(result) = &event.interaction_result
                    && result.status == fptr::TouchInteractionStatus::Denied
                {
                    claimed_interactions.remove(&result.interaction);
                }

                let mut response_type = fptr::TouchResponseType::No;
                if let Some(sample) = &event.pointer_sample
                    && let (Some(interaction), Some(phase), Some(pos_in_viewport)) =
                        (&sample.interaction, &sample.phase, &sample.position_in_viewport)
                {
                    match phase {
                        fptr::EventPhase::Add => {
                            if let Some(targets) = &maybe_targets {
                                let (vx, vy) = viewport_to_view_coordinates(
                                    *pos_in_viewport,
                                    &current_transform,
                                );
                                if vx.is_finite()
                                    && vy.is_finite()
                                    && let Some(claimed) = classify_touch_add(
                                        targets,
                                        vx,
                                        vy,
                                        &sender,
                                        &mut latest_opacity,
                                    )
                                {
                                    claimed_interactions.insert(*interaction, claimed);
                                    response_type = fptr::TouchResponseType::YesPrioritize;
                                }
                            }
                        }
                        fptr::EventPhase::Change => {
                            if let Some(&claimed) = claimed_interactions.get(interaction) {
                                match claimed {
                                    ClaimedTouchInteraction::ButtonTap => {}
                                    ClaimedTouchInteraction::HeaderDrag {
                                        grab_offset_x,
                                        grab_offset_y,
                                    } => {
                                        if maybe_targets.is_some() {
                                            let (vx, vy) = viewport_to_view_coordinates(
                                                *pos_in_viewport,
                                                &current_transform,
                                            );
                                            if vx.is_finite() && vy.is_finite() {
                                                latest_move = Some((
                                                    (vx - grab_offset_x).round() as i32,
                                                    (vy - grab_offset_y).round() as i32,
                                                ));
                                            }
                                        }
                                    }
                                    ClaimedTouchInteraction::OpacitySlider => {
                                        if let Some(targets) = &maybe_targets {
                                            let (vx, vy) = viewport_to_view_coordinates(
                                                *pos_in_viewport,
                                                &current_transform,
                                            );
                                            if vx.is_finite() && vy.is_finite() {
                                                latest_opacity = Some(opacity_from_slider_x(
                                                    vx,
                                                    &targets.slider_track_rect,
                                                ));
                                            }
                                        }
                                    }
                                }
                                response_type = fptr::TouchResponseType::YesPrioritize;
                            }
                        }
                        fptr::EventPhase::Remove | fptr::EventPhase::Cancel => {
                            if claimed_interactions.remove(interaction).is_some() {
                                response_type = fptr::TouchResponseType::YesPrioritize;
                            }
                        }
                    }
                }

                pending_responses.push(if event.pointer_sample.is_some() {
                    fptr::TouchResponse {
                        response_type: Some(response_type),
                        trace_flow_id: event.trace_flow_id,
                        ..Default::default()
                    }
                } else {
                    Default::default()
                });
            }

            if let Some((x, y)) = latest_move {
                let _ = sender.unbounded_send(OverlayEvent::MoveRequested { x, y });
            }
            if let Some(opacity) = latest_opacity {
                let _ = sender.unbounded_send(OverlayEvent::SetOpacity { opacity });
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy_and_stream;
    use futures::TryStreamExt;
    use futures::channel::mpsc::unbounded;

    #[fuchsia::test]
    fn test_touch_source_watcher_handles_close_confirm_header_drag_and_opacity_slider() {
        let mut exec = fasync::TestExecutor::new();
        let (touch_proxy, mut touch_stream) = create_proxy_and_stream::<fptr::TouchSourceMarker>();
        let hit_targets = new_overlay_hit_targets();
        let layout =
            compute_overlay_layout(fmath::SizeU { width: 480, height: 800 }, Some((0, 100)), true);
        assert_eq!(layout.local_hit_regions.len(), 5);
        let targets = layout.targets;
        *hit_targets.lock() = Some(targets);
        let (sender, mut receiver) = unbounded();

        let _watcher_task = spawn_touch_source_watcher(touch_proxy, hit_targets.clone(), sender);

        let mut next_watch = |exec: &mut fasync::TestExecutor| {
            let std::task::Poll::Ready(Ok(Some(fptr::TouchSourceRequest::Watch {
                responses,
                responder,
            }))) = exec.run_until_stalled(&mut touch_stream.try_next())
            else {
                panic!("Expected TouchSource::Watch request");
            };
            (responses, responder)
        };
        let touch_event =
            |interaction_id: u32, phase: fptr::EventPhase, pos: [f32; 2]| fptr::TouchEvent {
                pointer_sample: Some(fptr::TouchPointerSample {
                    interaction: Some(fptr::TouchInteractionId {
                        device_id: 1,
                        pointer_id: 1,
                        interaction_id,
                    }),
                    phase: Some(phase),
                    position_in_viewport: Some(pos),
                    ..Default::default()
                }),
                ..Default::default()
            };

        // 1. Initial Watch() call has empty responses; send a tap outside all controls (20, 280).
        let (responses, responder) = next_watch(&mut exec);
        assert!(responses.is_empty());
        let mut outside_event = touch_event(10, fptr::EventPhase::Add, [20.0, 280.0]);
        outside_event.view_parameters = Some(fptr::ViewParameters {
            view: fptr::Rectangle { min: [0.0, 0.0], max: [480.0, 800.0] },
            viewport: fptr::Rectangle { min: [0.0, 0.0], max: [480.0, 800.0] },
            viewport_to_view_transform: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        });
        responder.send(vec![outside_event]).unwrap();

        // 2. Next Watch() call must respond with TouchResponseType::No so underlying session views
        // receive the tap unimpeded.
        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::No));
        assert!(receiver.try_recv().is_err());

        // 3. Send a tap inside the [X] button.
        let close_pos = [
            targets.close_rect.x + targets.close_rect.width / 2.0,
            targets.close_rect.y + targets.close_rect.height / 2.0,
        ];
        responder.send(vec![touch_event(11, fptr::EventPhase::Add, close_pos)]).unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert!(matches!(
            receiver.try_recv().expect("Expected close request"),
            OverlayEvent::CloseRequested
        ));

        // 4. Send a tap on the confirmation [Close] button and another on [Cancel].
        let cr = targets.confirm_close_rect.unwrap();
        responder
            .send(vec![touch_event(
                12,
                fptr::EventPhase::Add,
                [cr.x + cr.width / 2.0, cr.y + cr.height / 2.0],
            )])
            .unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert!(matches!(
            receiver.try_recv().expect("Expected close confirmed"),
            OverlayEvent::CloseConfirmed
        ));

        let cancel_r = targets.confirm_cancel_rect.unwrap();
        responder
            .send(vec![touch_event(
                15,
                fptr::EventPhase::Add,
                [cancel_r.x + cancel_r.width / 2.0, cancel_r.y + cancel_r.height / 2.0],
            )])
            .unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert!(matches!(
            receiver.try_recv().expect("Expected close cancelled"),
            OverlayEvent::CloseCancelled
        ));

        // 5. Drag the top header bar and verify MoveRequested is emitted.
        let hdr_start = [targets.header_drag_rect.x + 50.0, targets.header_drag_rect.y + 15.0];
        responder
            .send(vec![
                touch_event(13, fptr::EventPhase::Add, hdr_start),
                touch_event(
                    13,
                    fptr::EventPhase::Change,
                    [hdr_start[0] + 30.0, hdr_start[1] + 40.0],
                ),
            ])
            .unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert_eq!(responses[1].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert!(matches!(
            receiver.try_recv().expect("Expected move message"),
            OverlayEvent::MoveRequested { x: 30, y: 140 }
        ));

        // 6. Tap the bottom opacity slider (Add) and drag across multiple Change samples in a
        // batch, verifying batch coalescing emits one SetOpacity event per Watch batch.
        let track = targets.slider_track_rect;
        let slider_y = targets.slider_hit_rect.y + 16.0;
        responder.send(vec![touch_event(14, fptr::EventPhase::Add, [track.x, slider_y])]).unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 1);
        let OverlayEvent::SetOpacity { opacity: op_min } =
            receiver.try_recv().expect("Expected min opacity")
        else {
            panic!("Expected OverlayEvent::SetOpacity");
        };
        assert!((op_min - MIN_OVERLAY_OPACITY).abs() < 1e-4);

        responder
            .send(vec![
                touch_event(14, fptr::EventPhase::Change, [track.x + track.width * 0.5, slider_y]),
                touch_event(14, fptr::EventPhase::Change, [track.x + track.width, slider_y]),
            ])
            .unwrap();

        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 2);
        let OverlayEvent::SetOpacity { opacity: op_max } =
            receiver.try_recv().expect("Expected coalesced max opacity")
        else {
            panic!("Expected OverlayEvent::SetOpacity");
        };
        assert!((op_max - MAX_OVERLAY_OPACITY).abs() < 1e-4);
        assert!(receiver.try_recv().is_err());

        // 7. Claim a header drag (16), deny it via `interaction_result`, and verify subsequent
        // `Change` events respond with `No` and emit no `MoveRequested`.
        responder.send(vec![touch_event(16, fptr::EventPhase::Add, hdr_start)]).unwrap();
        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));

        responder
            .send(vec![
                fptr::TouchEvent {
                    interaction_result: Some(fptr::TouchInteractionResult {
                        interaction: fptr::TouchInteractionId {
                            device_id: 1,
                            pointer_id: 1,
                            interaction_id: 16,
                        },
                        status: fptr::TouchInteractionStatus::Denied,
                    }),
                    ..Default::default()
                },
                touch_event(
                    16,
                    fptr::EventPhase::Change,
                    [hdr_start[0] + 50.0, hdr_start[1] + 50.0],
                ),
            ])
            .unwrap();
        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0].response_type, None);
        assert_eq!(responses[1].response_type, Some(fptr::TouchResponseType::No));
        assert!(receiver.try_recv().is_err());

        // 8. Claim a header drag (17), hide the overlay (`*hit_targets.lock() = None`), and verify
        // subsequent `Change` events stop emitting `MoveRequested` while keeping the claimed
        // interaction acknowledged until `Remove`, and any new `Add` while hidden responds `No`.
        responder.send(vec![touch_event(17, fptr::EventPhase::Add, hdr_start)]).unwrap();
        let (responses, responder) = next_watch(&mut exec);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));

        *hit_targets.lock() = None;
        responder
            .send(vec![
                touch_event(
                    17,
                    fptr::EventPhase::Change,
                    [hdr_start[0] + 60.0, hdr_start[1] + 60.0],
                ),
                touch_event(
                    17,
                    fptr::EventPhase::Remove,
                    [hdr_start[0] + 60.0, hdr_start[1] + 60.0],
                ),
                touch_event(18, fptr::EventPhase::Add, hdr_start),
            ])
            .unwrap();
        let (responses, _responder) = next_watch(&mut exec);
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert_eq!(responses[1].response_type, Some(fptr::TouchResponseType::YesPrioritize));
        assert_eq!(responses[2].response_type, Some(fptr::TouchResponseType::No));
        assert!(receiver.try_recv().is_err());
    }
}
