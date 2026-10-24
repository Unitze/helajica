//! Scrolling the way browsers do it, for the transcript and every scrolling
//! area in the app: a wheel notch glides to where it lands instead of jumping
//! there, and a middle click starts autoscroll, faster the further the pointer
//! moves from where it began. Touchpads already send smooth pixel deltas and
//! go straight to the element.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::assets::IconName as Lucide;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _};
use gpui_kit::{
    AnyElement, App, Context, CursorStyle, DispatchPhase, Div, EntityId, FollowMode,
    HitboxBehavior, IntoElement, ListState, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ParentElement as _, Pixels, Point, ScrollDelta, ScrollHandle, ScrollWheelEvent,
    Stateful, StatefulInteractiveElement as _, Styled as _, Window, anchored, canvas, deferred,
    div, point, px,
};

const LINE_HEIGHT: f32 = 36.;
/// Share of the way to the target covered in one 60 Hz frame.
const GLIDE_PER_FRAME: f32 = 0.25;
const FRAME: Duration = Duration::from_micros(16_667);
/// A stalled frame catches up at most this many frames' worth of glide.
const MAX_FRAMES: f32 = 4.;
const SETTLED: f32 = 0.5;
/// How far the pointer moves before autoscroll starts moving.
const AUTOSCROLL_DEAD_ZONE: f32 = 12.;
/// Pixels per second for every pixel past the dead zone.
const AUTOSCROLL_GAIN: f32 = 8.;
/// Held longer than this, letting go of the middle button ends autoscroll.
const AUTOSCROLL_HOLD: Duration = Duration::from_millis(300);
const AUTOSCROLL_MARK_SIZE: f32 = 28.;
const END_SLACK: f32 = 1.;

#[derive(Clone)]
pub struct Scroller {
    surface: Surface,
    state: Rc<RefCell<State>>,
}

#[derive(Clone)]
enum Surface {
    /// The transcript, which follows its tail.
    List(ListState),
    /// Any other element that scrolls, through [`SmoothScroll`].
    Area(ScrollHandle),
}

#[derive(Default)]
struct State {
    /// Where the wheel is heading, in pixels from the top.
    target: Option<f32>,
    autoscroll: Option<Autoscroll>,
    last_tick: Option<Instant>,
}

struct Autoscroll {
    origin: Point<Pixels>,
    started: Instant,
}

impl Scroller {
    pub fn new(list: ListState) -> Self {
        Self {
            surface: Surface::List(list),
            state: Rc::default(),
        }
    }

    pub fn area() -> Self {
        Self {
            surface: Surface::Area(ScrollHandle::new()),
            state: Rc::default(),
        }
    }

    /// Drops what the wheel and autoscroll were doing, for a new transcript
    /// or a jump to the end.
    pub fn stop(&self) {
        let mut state = self.state.borrow_mut();
        state.target = None;
        state.autoscroll = None;
    }

    /// Moves the list one frame along. Call at the start of each render.
    pub fn tick(&self, window: &mut Window) {
        let now = Instant::now();
        let mut state = self.state.borrow_mut();
        let elapsed = state.last_tick.map_or(FRAME, |last| now - last);
        let frames = elapsed.as_secs_f32() / FRAME.as_secs_f32();
        let mut moving = false;
        if let Some(autoscroll) = &state.autoscroll {
            let pull = f32::from(window.mouse_position().y - autoscroll.origin.y);
            let past = (pull.abs() - AUTOSCROLL_DEAD_ZONE).max(0.) * pull.signum();
            self.move_by(past * AUTOSCROLL_GAIN * elapsed.as_secs_f32());
            moving = true;
        }
        if let Some(target) = state.target {
            let target = target.clamp(0., self.max());
            let left = target - self.position();
            if left.abs() < SETTLED {
                self.move_by(left);
                state.target = None;
            } else {
                let share = 1. - (1. - GLIDE_PER_FRAME).powf(frames.min(MAX_FRAMES));
                self.move_by(left * share);
                state.target = Some(target);
                moving = true;
            }
        }
        state.last_tick = moving.then_some(now);
        if moving {
            window.request_animation_frame();
        }
    }

    /// Takes wheel and middle-button input before the element under it does.
    /// Over the transcript it fills the list's box; in an area it takes the
    /// box from the area's handle, since its own moves with the content.
    pub fn catcher(&self, view: EntityId) -> impl IntoElement {
        let this = self.clone();
        let ticker = self.clone();
        canvas(
            move |bounds, window, _| {
                let bounds = match &ticker.surface {
                    Surface::List(_) => bounds,
                    Surface::Area(handle) => {
                        ticker.tick(window);
                        handle.bounds()
                    }
                };
                window.insert_hitbox(bounds, HitboxBehavior::Normal)
            },
            move |_, hitbox, window, _| {
                if this.state.borrow().autoscroll.is_some() {
                    window.set_cursor_style(CursorStyle::ResizeUpDown, &hitbox);
                }
                let wheel = this.clone();
                let wheel_hitbox = hitbox.clone();
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture || !wheel_hitbox.should_handle_scroll(window)
                    {
                        return;
                    }
                    wheel.state.borrow_mut().autoscroll = None;
                    match event.delta {
                        ScrollDelta::Lines(lines) if lines.y.abs() > lines.x.abs() => {
                            wheel.glide(-lines.y * LINE_HEIGHT);
                            cx.stop_propagation();
                            cx.notify(view);
                        }
                        _ => wheel.state.borrow_mut().target = None,
                    }
                });
                let down = this.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let mut state = down.state.borrow_mut();
                    if state.autoscroll.take().is_some() {
                        cx.stop_propagation();
                    } else if event.button == MouseButton::Middle && hitbox.is_hovered(window) {
                        state.target = None;
                        state.autoscroll = Some(Autoscroll {
                            origin: event.position,
                            started: Instant::now(),
                        });
                        cx.stop_propagation();
                    } else {
                        return;
                    }
                    cx.notify(view);
                });
                let up = this.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    let mut state = up.state.borrow_mut();
                    let held = state.autoscroll.as_ref().is_some_and(|a| {
                        a.started.elapsed() > AUTOSCROLL_HOLD
                            && distance(a.origin, event.position) > AUTOSCROLL_DEAD_ZONE
                    });
                    if phase == DispatchPhase::Capture
                        && event.button == MouseButton::Middle
                        && held
                    {
                        state.autoscroll = None;
                        cx.notify(view);
                    }
                });
                let moved = this.clone();
                window.on_mouse_event(move |_: &MouseMoveEvent, phase, _, cx| {
                    if phase == DispatchPhase::Capture && moved.state.borrow().autoscroll.is_some()
                    {
                        cx.notify(view);
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// The mark autoscroll started from, drawn over everything.
    pub fn autoscroll_mark(&self, cx: &App) -> Option<AnyElement> {
        let origin = self.state.borrow().autoscroll.as_ref()?.origin;
        let half = px(AUTOSCROLL_MARK_SIZE / 2.);
        Some(
            deferred(
                anchored().position(origin - point(half, half)).child(
                    div()
                        .size(px(AUTOSCROLL_MARK_SIZE))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().background)
                        .shadow_sm()
                        .child(Icon::new(Lucide::ChevronsUpDown).small()),
                ),
            )
            .into_any_element(),
        )
    }

    fn glide(&self, distance: f32) {
        let mut state = self.state.borrow_mut();
        let from = state.target.unwrap_or_else(|| self.position());
        state.target = Some((from + distance).clamp(0., self.max()));
    }

    fn move_by(&self, distance: f32) {
        if distance == 0. {
            return;
        }
        match &self.surface {
            Surface::List(list) => {
                list.scroll_by(px(distance));
                if distance > 0. && self.position() >= self.max() - END_SLACK {
                    list.set_follow_mode(FollowMode::Tail);
                }
            }
            Surface::Area(handle) => {
                let y = (self.position() + distance).clamp(0., self.max());
                handle.set_offset(point(handle.offset().x, px(-y)));
            }
        }
    }

    /// Pixels scrolled from the top. While following the tail the list keeps
    /// its offset past the end, so that reads as the bottom.
    fn position(&self) -> f32 {
        match &self.surface {
            Surface::List(list) if list.is_following_tail() => self.max(),
            Surface::List(list) => -f32::from(list.scroll_px_offset_for_scrollbar().y),
            Surface::Area(handle) => -f32::from(handle.offset().y),
        }
    }

    fn max(&self) -> f32 {
        f32::from(match &self.surface {
            Surface::List(list) => list.max_offset_for_scrollbar().y,
            Surface::Area(handle) => handle.max_offset().y,
        })
    }
}

/// Vertical scrolling with a scrollbar, the glide and middle-click
/// autoscroll included. In place of `overflow_y_scrollbar`.
pub trait SmoothScroll {
    fn smooth_scroll<V: 'static>(self, scroller: &Scroller, cx: &Context<V>) -> Self;
}

impl SmoothScroll for Stateful<Div> {
    fn smooth_scroll<V: 'static>(self, scroller: &Scroller, cx: &Context<V>) -> Self {
        let Surface::Area(handle) = &scroller.surface else {
            return self;
        };
        self.track_scroll(handle)
            .overflow_y_scroll()
            .vertical_scrollbar(handle)
            .child(scroller.catcher(cx.entity_id()))
            .children(scroller.autoscroll_mark(cx))
    }
}

fn distance(a: Point<Pixels>, b: Point<Pixels>) -> f32 {
    f32::from(a.y - b.y).hypot(f32::from(a.x - b.x))
}
