//! Touch-gesture sidebar open/close for mobile.
//!
//! Opening swipe: only on the left-edge zone (40 px).  Closing swipe:
//! on the overlay or sidebar.  A 5 px dead zone ensures taps still
//! generate click events (session names, overlay close).
//!
//! This is a non-visual component — it attaches event listeners to the
//! existing DOM elements (`#sidebar`, `#edgeZone`, `.sidebar-overlay`).

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{Event, TouchEvent, window};

const EDGE_WIDTH: i32 = 40;
const SWIPE_THRESHOLD: f64 = 60.0;
const DEAD_ZONE: f64 = 5.0;

/// Attach swipe-to-open and swipe-to-close touch handlers.
///
/// Must be called inside a component that has `#sidebar`, `#edgeZone`,
/// and `.sidebar-overlay` in the DOM.  Drives `sidebar_open` directly.
#[component]
pub fn SwipeSidebar(sidebar_open: RwSignal<bool>) -> impl IntoView {
    Effect::new(move || {
        // Re-attach if sidebar DOM elements change (they shouldn't, but
        // this is a cheap guard).
        let _ = sidebar_open.get();

        if let Some(doc) = window().and_then(|w| w.document()) {
            setup_swipe_handlers(&doc, sidebar_open);
        }
    });

    // This component renders nothing.
    view! { <></> }
}

fn setup_swipe_handlers(document: &web_sys::Document, sidebar_open: RwSignal<bool>) {
    let Some(sidebar_el) = document.get_element_by_id("sidebar") else {
        return;
    };
    let Some(edge_zone) = document.get_element_by_id("edgeZone") else {
        return;
    };
    let Some(overlay_el) = document.query_selector(".sidebar-overlay").ok().flatten() else {
        return;
    };

    // ── shared gesture state ───────────────────────────────────────────
    use std::cell::Cell;
    use std::rc::Rc;

    let mode: Rc<Cell<Option<&'static str>>> = Rc::new(Cell::new(None));
    let start_x: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
    let committed: Rc<Cell<bool>> = Rc::new(Cell::new(false));

    // ── helper: end the swipe gesture ──────────────────────────────────
    fn end_swipe(
        sidebar_el: &web_sys::Element,
        overlay_el: &web_sys::Element,
        should_open: bool,
        mode: &Rc<Cell<Option<&'static str>>>,
        committed: &Rc<Cell<bool>>,
        sidebar_open: RwSignal<bool>,
    ) {
        let sidebar_width = sidebar_el.client_width().max(250) as f64;

        let sidebar_html = sidebar_el.unchecked_ref::<web_sys::HtmlElement>();
        let overlay_html = overlay_el.unchecked_ref::<web_sys::HtmlElement>();

        let target = if should_open {
            "translateX(0)".to_string()
        } else {
            format!("translateX(-{sidebar_width}px)")
        };

        let style = sidebar_html.style();
        style
            .set_property(
                "transition",
                "transform 0.2s cubic-bezier(0.0, 0.0, 0.2, 1)",
            )
            .ok();
        style.set_property("transform", &target).ok();

        let style_o = overlay_html.style();
        style_o
            .set_property("transition", "opacity 0.2s cubic-bezier(0.0, 0.0, 0.2, 1)")
            .ok();
        style_o
            .set_property("opacity", if should_open { "1" } else { "0" })
            .ok();

        sidebar_open.set(should_open);

        // Clean up after transition.
        let sidebar_clone = sidebar_html.clone();
        let overlay_clone = overlay_html.clone();

        let on_transition_end = Closure::<dyn Fn(Event)>::new(move |_| {
            let s = sidebar_clone.style();
            s.set_property("transition", "").ok();
            s.set_property("transform", "").ok();
            s.set_property("will-change", "").ok();
            let o = overlay_clone.style();
            o.set_property("transition", "").ok();
            o.set_property("display", "").ok();
            o.set_property("opacity", "").ok();
        });
        sidebar_html
            .add_event_listener_with_callback(
                "transitionend",
                on_transition_end.as_ref().unchecked_ref(),
            )
            .ok();
        on_transition_end.forget();

        mode.set(None);
        committed.set(false);
    }

    // ── opening swipe: only on left-edge zone ──────────────────────────
    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let start_x_c = Rc::clone(&start_x);
        let committed_c = Rc::clone(&committed);
        let sidebar_open_c = sidebar_open;

        let on_touch_start = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
            if mode_c.get().is_some() {
                return;
            }
            if e.touches().length() != 1 {
                return;
            }
            if sidebar_open_c.get_untracked() {
                return;
            }
            let touch = e.touches().get(0).unwrap();
            if touch.client_x() > EDGE_WIDTH {
                return;
            }
            // Don't preventDefault here — the shared touchmove handler
            // does it once the finger moves past the dead zone.  Calling
            // it on touchstart kills the click event, which prevents the
            // ☰ menu button (which overlaps the edgeZone at the top-left)
            // from working.
            mode_c.set(Some("open"));
            start_x_c.set(touch.client_x() as f64);
            // Set committed = true so the shared touchmove handler
            // immediately calls preventDefault on the first pixel of
            // movement (no dead-zone wait needed for edge swipes).
            committed_c.set(true);

            let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            s.set_property("transition", "none").ok();
            s.set_property("will-change", "transform").ok();
            let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            o.set_property("display", "block").ok();
            o.set_property("opacity", "0").ok();
        });
        edge_zone
            .add_event_listener_with_callback("touchstart", on_touch_start.as_ref().unchecked_ref())
            .ok();
        on_touch_start.forget();
    }

    // ── closing swipe: overlay + sidebar ──────────────────────────────
    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let start_x_c = Rc::clone(&start_x);
        let committed_c = Rc::clone(&committed);
        let sidebar_open_c = sidebar_open;

        let on_touch_start = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
            if mode_c.get().is_some() {
                return;
            }
            if e.touches().length() != 1 {
                return;
            }
            if !sidebar_open_c.get() {
                return;
            }
            // Do NOT preventDefault — defer to touchmove so taps still
            // generate click events (session names, overlay close).
            mode_c.set(Some("close"));
            start_x_c.set(e.touches().get(0).unwrap().client_x() as f64);
            committed_c.set(false);

            let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            s.set_property("transition", "none").ok();
            s.set_property("will-change", "transform").ok();
            let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            o.set_property("display", "block").ok();
            o.set_property("opacity", "1").ok();
        });

        overlay_el
            .add_event_listener_with_callback("touchstart", on_touch_start.as_ref().unchecked_ref())
            .ok();
        on_touch_start.forget();
    }

    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let start_x_c = Rc::clone(&start_x);
        let committed_c = Rc::clone(&committed);
        let sidebar_open_c = sidebar_open;

        let on_touch_start = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
            if mode_c.get().is_some() {
                return;
            }
            if e.touches().length() != 1 {
                return;
            }
            if !sidebar_open_c.get() {
                return;
            }
            mode_c.set(Some("close"));
            start_x_c.set(e.touches().get(0).unwrap().client_x() as f64);
            committed_c.set(false);

            let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            s.set_property("transition", "none").ok();
            s.set_property("will-change", "transform").ok();
            let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            o.set_property("display", "block").ok();
            o.set_property("opacity", "1").ok();
        });
        sidebar_el
            .add_event_listener_with_callback("touchstart", on_touch_start.as_ref().unchecked_ref())
            .ok();
        on_touch_start.forget();
    }

    // ── shared move / end / cancel on document ────────────────────────
    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let start_x_c = Rc::clone(&start_x);
        let committed_c = Rc::clone(&committed);

        let on_touch_move = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
            let Some(current_mode) = mode_c.get() else {
                return;
            };
            let touch = e.touches().get(0).unwrap();
            let delta_x = touch.client_x() as f64 - start_x_c.get();

            // Don't commit until finger moves past dead zone.
            if !committed_c.get() {
                if delta_x.abs() < DEAD_ZONE {
                    return;
                }
                committed_c.set(true);
            }
            e.prevent_default();

            let sidebar_width = sidebar_el_c.client_width().max(250) as f64;
            let px = if current_mode == "open" {
                (-sidebar_width + delta_x).clamp(-sidebar_width, 0.0)
            } else {
                delta_x.clamp(-sidebar_width, 0.0)
            };

            let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            s.set_property("transform", &format!("translateX({px}px)"))
                .ok();

            let progress = (sidebar_width + px) / sidebar_width;
            let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            o.set_property("display", if progress > 0.0 { "block" } else { "none" })
                .ok();
            o.set_property("opacity", &format!("{}", (progress * 0.45).clamp(0.0, 1.0)))
                .ok();
        });
        document
            .add_event_listener_with_callback("touchmove", on_touch_move.as_ref().unchecked_ref())
            .ok();
        on_touch_move.forget();
    }

    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let start_x_c = Rc::clone(&start_x);
        let committed_c = Rc::clone(&committed);
        let sidebar_open_c = sidebar_open;

        let on_touch_end = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
            let Some(current_mode) = mode_c.get() else {
                return;
            };

            if !committed_c.get() {
                // Finger never moved past dead zone — it was a tap.
                // Clean up without snapping.  The overlay may have been
                // set to `display: block` by the touchstart handler; we
                // must clear it here or it will persist as an inline style
                // and eat subsequent touches / block the UI.
                committed_c.set(false);
                mode_c.set(None);
                let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
                s.set_property("transition", "").ok();
                s.set_property("transform", "").ok();
                s.set_property("will-change", "").ok();
                let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
                o.set_property("display", "").ok();
                o.set_property("opacity", "").ok();
                o.set_property("transition", "").ok();
                return;
            }

            let Some(touch) = e.changed_touches().get(0) else {
                // No touches in the changed list — clean up without snapping.
                committed_c.set(false);
                mode_c.set(None);
                let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
                s.set_property("transition", "").ok();
                s.set_property("transform", "").ok();
                s.set_property("will-change", "").ok();
                let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
                o.set_property("display", "").ok();
                o.set_property("opacity", "").ok();
                o.set_property("transition", "").ok();
                return;
            };
            let delta_x = touch.client_x() as f64 - start_x_c.get();

            let should_open = if current_mode == "open" {
                delta_x > SWIPE_THRESHOLD
            } else {
                delta_x > -SWIPE_THRESHOLD
            };

            end_swipe(
                &sidebar_el_c,
                &overlay_el_c,
                should_open,
                &mode_c,
                &committed_c,
                sidebar_open_c,
            );
        });
        document
            .add_event_listener_with_callback("touchend", on_touch_end.as_ref().unchecked_ref())
            .ok();
        on_touch_end.forget();
    }

    {
        let sidebar_el_c = sidebar_el.clone();
        let overlay_el_c = overlay_el.clone();
        let mode_c = Rc::clone(&mode);
        let committed_c = Rc::clone(&committed);
        let sidebar_open_c = sidebar_open;

        let on_touch_cancel = Closure::<dyn Fn(Event)>::new(move |_| {
            let was_open = sidebar_open_c.get();
            committed_c.set(false);
            let s = sidebar_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            s.set_property("transition", "").ok();
            s.set_property("transform", "").ok();
            s.set_property("will-change", "").ok();
            let o = overlay_el_c.unchecked_ref::<web_sys::HtmlElement>().style();
            o.set_property("display", "").ok();
            o.set_property("opacity", "").ok();
            o.set_property("transition", "").ok();
            sidebar_open_c.set(was_open);
            mode_c.set(None);
        });
        document
            .add_event_listener_with_callback(
                "touchcancel",
                on_touch_cancel.as_ref().unchecked_ref(),
            )
            .ok();
        on_touch_cancel.forget();
    }
}
