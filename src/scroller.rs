use std::{
    cell::RefCell,
    collections::HashMap,
    hash::Hash,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    App, DispatchPhase, Global, HitboxBehavior, ParentElement, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, StatefulInteractiveElement, Styled, Window, canvas, point, px,
};
use gpui_component::scroll::ScrollbarHandle;

const EASE: f32 = 0.12;
const HERTZ: f32 = 180.0;
const MAX_FRAME_TIME: Duration = Duration::from_millis(64);
const REST: Pixels = px(0.5);

type Motion = Rc<RefCell<ScrollMotion>>;

/// A pane's scroll state is shared by every instance of it unless `scope`
/// tells them apart -- so a tab's own offset doesn't bleed into the next tab
/// that reuses the same pane name. Panes with exactly one instance on screen
/// (the tab strip, a settings list) use the empty scope.
type Key = (&'static str, String);

#[derive(Default)]
struct Registry {
    handles: HashMap<Key, ScrollHandle>,
    motions: HashMap<Key, (Motion, Motion)>,
}

impl Global for Registry {}

#[derive(Clone)]
pub struct Smooth {
    scroll: Rc<dyn ScrollbarHandle>,
    across: Rc<dyn ScrollbarHandle>,
    track: Option<ScrollHandle>,
    motion: Motion,
    across_motion: Motion,
    /// Whether the element's own scrolling has to be taken over in the capture
    /// phase because something beneath it consumes the wheel first.
    intercept: bool,
}

pub fn smooth(id: &'static str, cx: &mut App) -> Smooth {
    smooth_scoped(id, "", cx)
}

/// Like [`smooth`], keyed to one instance of the pane -- a tab, a connection
/// being edited, a review dialog -- so switching to another instance starts
/// from its own remembered offset instead of the last instance's.
pub fn smooth_scoped(id: &'static str, scope: impl Into<String>, cx: &mut App) -> Smooth {
    let key: Key = (id, scope.into());
    let registry = cx.default_global::<Registry>();
    let handle = registry.handles.entry(key.clone()).or_default().clone();
    let (motion, across_motion) = registry.motions.entry(key).or_default().clone();
    Smooth {
        scroll: Rc::new(handle.clone()),
        across: Rc::new(handle.clone()),
        track: Some(handle),
        motion,
        across_motion,
        intercept: false,
    }
}

/// `smooth_scroll`'s own wheel-capturing canvas is the tracked element's
/// first child, ahead of whatever it is scrolling: `scroll_to_item` counts
/// direct children, so a target index meant for the caller's own content has
/// to shift past it to land on the right one.
const OVERLAY_CHILDREN: usize = 1;

/// Brings an item of the element `smooth` tracks into view, at the next
/// prepaint -- the id has to be one `smooth` was already called with once,
/// which every render of the scrolled element does regardless. `ix` counts
/// the caller's own children, not `smooth_scroll`'s canvas ahead of them.
pub fn scroll_to(id: &'static str, ix: usize, cx: &mut App) {
    cx.default_global::<Registry>()
        .handles
        .entry((id, String::new()))
        .or_default()
        .scroll_to_item(ix + OVERLAY_CHILDREN);
}

pub fn smooth_for(
    id: &'static str,
    scope: impl Into<String>,
    scroll: impl ScrollbarHandle,
    across: impl ScrollbarHandle,
    cx: &mut App,
) -> Smooth {
    let key: Key = (id, scope.into());
    let (motion, across_motion) = cx
        .default_global::<Registry>()
        .motions
        .entry(key)
        .or_default()
        .clone();
    Smooth {
        scroll: Rc::new(scroll),
        across: Rc::new(across),
        track: None,
        motion,
        across_motion,
        intercept: true,
    }
}

impl Smooth {
    /// Glides the cross axis to `x` on the wheel's curve, for a scroll the
    /// code asks for rather than the user. A wheel turn, a click or another
    /// jump mid-glide stops it the way they stop a wheel's.
    pub fn glide_across(&self, x: Pixels, window: &mut Window) {
        let offset = self.across.offset();
        self.across_motion.borrow_mut().glide_to(
            offset,
            point(x, offset.y),
            max_offset(&*self.across),
        );
        schedule_frame(&self.across_motion, self.across.clone(), window);
    }
}

#[derive(Default)]
struct ScrollMotion {
    shown: Point<Pixels>,
    target: Point<Pixels>,
    active: bool,
    /// The motion is a `glide_across` rather than a wheel's, so a click lands
    /// it instead of stopping it short of where it was sent.
    glide: bool,
    frame_pending: bool,
    last_frame: Option<Instant>,
}

impl ScrollMotion {
    #[cfg(test)]
    fn new(offset: Point<Pixels>) -> Self {
        Self {
            shown: offset,
            target: offset,
            active: false,
            glide: false,
            frame_pending: false,
            last_frame: None,
        }
    }

    fn stop(&mut self, offset: Point<Pixels>) {
        self.shown = offset;
        self.target = offset;
        self.active = false;
        self.glide = false;
        self.last_frame = None;
    }

    /// Already moving, only the target changes: a glide asked for again every
    /// frame of a resize would otherwise restart its clock each time and trail
    /// the layout.
    fn glide_to(&mut self, offset: Point<Pixels>, target: Point<Pixels>, maximum: Point<Pixels>) {
        if self.active {
            self.target = clamp_offset(target, maximum);
        } else {
            self.stop(offset);
            self.nudge(target, maximum);
        }
        self.glide = self.active;
    }

    /// Where a click leaves the motion: a glide lands on its target, and a
    /// wheel's stops where it is.
    fn interrupt(&mut self, offset: Point<Pixels>) -> Point<Pixels> {
        let at = if self.active && self.glide {
            self.target
        } else {
            offset
        };
        self.stop(at);
        at
    }

    fn sync(&mut self, offset: Point<Pixels>, maximum: Point<Pixels>) {
        if !self.active || offset != clamp_offset(self.shown, maximum) {
            self.stop(offset);
        }
    }

    fn nudge(&mut self, offset: Point<Pixels>, maximum: Point<Pixels>) {
        self.glide = false;
        let delta = offset - self.shown;
        let from = if self.active { self.target } else { self.shown };
        self.target = clamp_offset(from + delta, maximum);
        self.shown = clamp_offset(self.shown, maximum);
        self.active = self.target != self.shown;
        if !self.active {
            self.last_frame = None;
        }
    }

    fn advance(&mut self, elapsed: Duration, maximum: Point<Pixels>) -> Point<Pixels> {
        self.target = clamp_offset(self.target, maximum);
        let distance = self.target - self.shown;
        if distance.x.abs() < REST && distance.y.abs() < REST {
            self.stop(self.target);
        } else {
            let ease = 1.0 - (1.0 - EASE).powf(elapsed.min(MAX_FRAME_TIME).as_secs_f32() * HERTZ);
            self.shown = clamp_offset(self.shown + distance * ease, maximum);
        }
        self.shown
    }
}

fn max_offset(scroll: &dyn ScrollbarHandle) -> Point<Pixels> {
    let extent = scroll.content_size() - scroll.viewport_bounds().size;
    point(
        extent.width.max(Pixels::ZERO),
        extent.height.max(Pixels::ZERO),
    )
}

fn clamp_offset(offset: Point<Pixels>, maximum: Point<Pixels>) -> Point<Pixels> {
    point(
        offset.x.clamp(-maximum.x.max(Pixels::ZERO), Pixels::ZERO),
        offset.y.clamp(-maximum.y.max(Pixels::ZERO), Pixels::ZERO),
    )
}

/// Things that glide to where they are told to be, on the same curve and clock
/// as the scrolling: each frame closes the same share of the distance left, per
/// 1/180 s of real time, so it looks the same at any refresh rate.
pub struct Shift<K> {
    inner: Rc<RefCell<ShiftState<K>>>,
}

struct ShiftState<K> {
    items: HashMap<K, (f32, f32)>,
    active: bool,
    frame_pending: bool,
    last_frame: Option<Instant>,
}

impl<K> Clone for Shift<K> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<K> Default for Shift<K> {
    fn default() -> Self {
        Self {
            inner: Rc::new(RefCell::new(ShiftState {
                items: HashMap::new(),
                active: false,
                frame_pending: false,
                last_frame: None,
            })),
        }
    }
}

impl<K: Hash + Eq + Clone + 'static> Shift<K> {
    /// Where `key` is drawn now, on its way to `target`.
    pub fn glide(&self, key: &K, target: f32) -> f32 {
        let mut state = self.inner.borrow_mut();
        let entry = state.items.entry(key.clone()).or_insert((target, target));
        entry.1 = target;
        let shown = entry.0;
        if (shown - target).abs() >= f32::from(REST) {
            state.active = true;
        }
        shown
    }

    /// `key` is exactly at `value`, with nothing to glide from.
    pub fn place(&self, key: &K, value: f32) {
        self.inner
            .borrow_mut()
            .items
            .insert(key.clone(), (value, value));
    }

    /// The layout has moved `key` by `moved` under it: keep it drawn where it
    /// was, and glide the rest of the way in.
    pub fn settle_into(&self, key: &K, moved: f32) {
        let mut state = self.inner.borrow_mut();
        if let Some(entry) = state.items.get_mut(key) {
            entry.0 -= moved;
            entry.1 = 0.;
            state.active = true;
        }
    }

    /// Ask for frames for as long as anything is still gliding.
    pub fn drive(&self, window: &mut Window) {
        {
            let mut state = self.inner.borrow_mut();
            if !state.active || state.frame_pending {
                return;
            }
            state.frame_pending = true;
        }
        let shift = self.clone();
        window.on_next_frame(move |window, _| {
            {
                let mut state = shift.inner.borrow_mut();
                state.frame_pending = false;
                let now = Instant::now();
                let elapsed = state
                    .last_frame
                    .replace(now)
                    .map(|last| now.duration_since(last))
                    .unwrap_or(Duration::from_secs_f32(1.0 / HERTZ));
                let ease =
                    1.0 - (1.0 - EASE).powf(elapsed.min(MAX_FRAME_TIME).as_secs_f32() * HERTZ);
                let rest = f32::from(REST);
                let mut moving = false;
                for (shown, target) in state.items.values_mut() {
                    let distance = *target - *shown;
                    if distance.abs() < rest {
                        *shown = *target;
                    } else {
                        *shown += distance * ease;
                        moving = true;
                    }
                }
                state.active = moving;
                if !moving {
                    state.last_frame = None;
                }
            }
            window.refresh();
            shift.drive(window);
        });
    }
}

fn schedule_frame(motion: &Motion, scroll: Rc<dyn ScrollbarHandle>, window: &mut Window) {
    {
        let mut state = motion.borrow_mut();
        if !state.active || state.frame_pending {
            return;
        }
        state.frame_pending = true;
    }

    let motion = motion.clone();
    window.on_next_frame(move |window, _| {
        {
            let mut state = motion.borrow_mut();
            state.frame_pending = false;
            let maximum = max_offset(&*scroll);
            state.sync(scroll.offset(), maximum);
            if !state.active {
                return;
            }
            let now = Instant::now();
            let elapsed = state
                .last_frame
                .replace(now)
                .map(|last| now.duration_since(last))
                .unwrap_or(Duration::from_secs_f32(1.0 / HERTZ));
            scroll.set_offset(state.advance(elapsed, maximum));
        }
        window.refresh();
        schedule_frame(&motion, scroll.clone(), window);
    });
}

pub trait SmoothScrollable: StatefulInteractiveElement + ParentElement + Sized {
    fn smooth_scroll(self, smooth: &Smooth) -> Self {
        let Smooth {
            scroll,
            across,
            track,
            motion,
            across_motion,
            intercept,
        } = smooth.clone();
        let this = match &track {
            Some(handle) => self.track_scroll(handle),
            None => self,
        };
        this.capture_any_mouse_down({
            let motion = motion.clone();
            let scroll = scroll.clone();
            let across_motion = across_motion.clone();
            let across = across.clone();
            move |_, window, _| {
                let at = motion.borrow_mut().interrupt(scroll.offset());
                scroll.set_offset(at);
                let at = across_motion.borrow_mut().interrupt(across.offset());
                across.set_offset(at);
                window.refresh();
            }
        })
        .child({
            let (scroll, across) = (scroll.clone(), across.clone());
            let (motion, across_motion) = (motion.clone(), across_motion.clone());
            canvas(
                // A hitbox, so a modal drawn over this is known to be in the
                // way: a bounds check alone would keep scrolling the table
                // behind it.
                |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
                move |_, hitbox, window, _| {
                    let (scroll, across) = (scroll.clone(), across.clone());
                    let (motion, across_motion) = (motion.clone(), across_motion.clone());
                    window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                            return;
                        }
                        let horizontal = event.modifiers.secondary();
                        if !horizontal && !intercept {
                            return;
                        }
                        let (handle, state) = match horizontal {
                            true => (&across, &across_motion),
                            false => (&scroll, &motion),
                        };
                        let maximum = max_offset(&**handle);
                        let room = match horizontal {
                            true => maximum.x,
                            false => maximum.y,
                        };
                        let delta = event.delta.pixel_delta(window.line_height());
                        if room <= REST || (!horizontal && delta.x.abs() > delta.y.abs()) {
                            return;
                        }
                        let offset = handle.offset();
                        let moved = match horizontal {
                            true => point(offset.x + delta.x + delta.y, offset.y),
                            false => point(offset.x, offset.y + delta.y),
                        };
                        if event.delta.precise() && !horizontal {
                            state.borrow_mut().stop(offset);
                            return;
                        }
                        cx.stop_propagation();
                        handle.set_offset(clamp_offset(moved, maximum));
                        {
                            let mut state = state.borrow_mut();
                            if event.delta.precise() {
                                state.stop(handle.offset());
                            } else {
                                state.nudge(handle.offset(), maximum);
                                handle.set_offset(state.shown);
                            }
                        }
                        schedule_frame(state, handle.clone(), window);
                        window.refresh();
                    });
                },
            )
            .absolute()
            .size_full()
        })
        .on_scroll_wheel(move |event: &ScrollWheelEvent, window, _| {
            {
                let mut state = motion.borrow_mut();
                if event.delta.precise() {
                    state.stop(scroll.offset());
                } else {
                    state.nudge(scroll.offset(), max_offset(&*scroll));
                    scroll.set_offset(state.shown);
                }
            }
            schedule_frame(&motion, scroll.clone(), window);
        })
    }
}

impl<E: StatefulInteractiveElement + ParentElement> SmoothScrollable for E {}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(y: f32) -> Point<Pixels> {
        point(px(0.0), px(y))
    }

    #[test]
    fn repeated_wheel_events_accumulate_and_reverse() {
        let mut motion = ScrollMotion::new(offset(0.0));
        motion.nudge(offset(-60.0), offset(1000.0));
        motion.advance(Duration::from_millis(16), offset(1000.0));
        assert!(motion.shown.y < px(0.0) && motion.shown.y > px(-60.0));
        motion.nudge(motion.shown + offset(-60.0), offset(1000.0));
        assert_eq!(motion.target, offset(-120.0));
        motion.nudge(motion.shown + offset(90.0), offset(1000.0));
        assert_eq!(motion.target, offset(-30.0));
    }

    #[test]
    fn easing_is_independent_of_refresh_rate() {
        let run = |frames, seconds| {
            let mut motion = ScrollMotion::new(offset(0.0));
            motion.nudge(offset(-1000.0), offset(2000.0));
            for _ in 0..frames {
                motion.advance(Duration::from_secs_f32(seconds), offset(2000.0));
            }
            motion.shown.y
        };
        assert!((run(6, 1.0 / 60.0) - run(18, 1.0 / 180.0)).abs() < px(0.01));
    }

    #[test]
    fn scroll_bounds_clamp_targets_and_shrinking_content() {
        let mut motion = ScrollMotion::new(offset(-100.0));
        motion.nudge(offset(-500.0), offset(200.0));
        assert_eq!(motion.target, offset(-200.0));
        let shown = motion.advance(Duration::from_millis(16), offset(40.0));
        assert_eq!(shown, offset(-40.0));
        motion.advance(Duration::from_millis(16), offset(40.0));
        assert!(!motion.active);
        motion.nudge(offset(500.0), offset(40.0));
        assert_eq!(motion.target, offset(0.0));
    }

    #[test]
    fn direct_input_and_programmatic_jumps_cancel_motion() {
        let mut motion = ScrollMotion::new(offset(0.0));
        motion.nudge(offset(-100.0), offset(1000.0));
        motion.stop(offset(-25.0));
        assert!(!motion.active);
        motion.nudge(offset(-50.0), offset(1000.0));
        motion.sync(offset(-800.0), offset(1000.0));
        assert!(!motion.active);
        assert_eq!(motion.shown, offset(-800.0));
    }

    #[test]
    fn a_glide_starts_from_where_the_offset_is_and_lands_on_its_target() {
        let mut motion = ScrollMotion::new(offset(-300.0));
        motion.stop(offset(0.0));
        motion.nudge(offset(-200.0), offset(1000.0));
        assert_eq!((motion.shown, motion.target), (offset(0.0), offset(-200.0)));
        for _ in 0..120 {
            motion.advance(Duration::from_millis(16), offset(1000.0));
        }
        assert_eq!(motion.shown, offset(-200.0));
    }

    #[test]
    fn a_glide_asked_for_again_mid_flight_keeps_its_clock_and_a_click_lands_it() {
        let mut motion = ScrollMotion::new(offset(0.0));
        motion.glide_to(offset(0.0), offset(-200.0), offset(1000.0));
        motion.advance(Duration::from_millis(16), offset(1000.0));
        motion.last_frame = Some(Instant::now());
        let shown = motion.shown;
        motion.glide_to(offset(-999.0), offset(-300.0), offset(1000.0));
        assert_eq!((motion.shown, motion.target), (shown, offset(-300.0)));
        assert!(motion.last_frame.is_some());
        assert_eq!(motion.interrupt(shown), offset(-300.0));
        assert!(!motion.active);

        motion.nudge(offset(-400.0), offset(1000.0));
        assert_eq!(motion.interrupt(offset(-310.0)), offset(-310.0));
    }

    #[test]
    fn motion_settles_exactly_and_empty_regions_stay_idle() {
        let mut motion = ScrollMotion::new(offset(0.0));
        motion.nudge(offset(-80.0), offset(0.0));
        assert!(!motion.active);
        motion.nudge(offset(-80.0), offset(1000.0));
        for _ in 0..120 {
            if motion.active {
                motion.advance(Duration::from_millis(16), offset(1000.0));
            }
        }
        assert!(!motion.active);
        assert_eq!(motion.shown, offset(-80.0));
    }
}
