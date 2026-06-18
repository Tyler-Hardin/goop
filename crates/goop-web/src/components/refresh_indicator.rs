//! Pull-to-refresh indicator.
//!
//! Attaches touch handlers to the message log element (`#log`).
//! When the user pulls down from the top (scrollTop == 0), a CSS
//! indicator slides in.  Crossing the threshold triggers a page reload.

use leptos::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::{Event, TouchEvent, window};

const REFRESH_THRESHOLD: f64 = 64.0;

/// Attach pull-to-refresh touch handlers to the message log.
///
/// Renders the `#refreshIndicator` div and wires up touch events.
#[component]
pub fn RefreshIndicator() -> impl IntoView {
    let node_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    Effect::new(move || {
        let Some(indicator) = node_ref.get() else {
            return;
        };
        let Some(document) = window().and_then(|w| w.document()) else {
            return;
        };
        let Some(log_el) = document.get_element_by_id("log") else {
            return;
        };

        use std::cell::Cell;
        use std::rc::Rc;

        let tracking: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let start_y: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));

        // touchstart
        {
            let tracking = Rc::clone(&tracking);
            let start_y = Rc::clone(&start_y);
            let log_el2 = log_el.clone();

            let on_touchstart = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
                if tracking.get() {
                    return;
                }
                if log_el2.scroll_top() > 0 {
                    return;
                }
                if e.touches().length() != 1 {
                    return;
                }
                let touch = e.touches().get(0).unwrap();
                // Don't interfere with sidebar swipe territory.
                if touch.client_x() <= 40 {
                    return;
                }
                tracking.set(true);
                start_y.set(touch.client_y() as f64);
                e.prevent_default();
            });
            log_el
                .add_event_listener_with_callback(
                    "touchstart",
                    on_touchstart.as_ref().unchecked_ref(),
                )
                .ok();
            on_touchstart.forget();
        }

        // touchmove
        {
            let tracking = Rc::clone(&tracking);
            let start_y = Rc::clone(&start_y);
            let indicator = indicator.unchecked_ref::<web_sys::HtmlElement>().clone();
            let log_el = log_el.clone();

            let on_touchmove = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
                if !tracking.get() {
                    return;
                }
                let touch = e.touches().get(0).unwrap();
                let dy = (touch.client_y() as f64 - start_y.get()).max(0.0);
                let progress = (dy / REFRESH_THRESHOLD).min(1.0);
                let offset = -48.0 + progress * 48.0;
                let style = indicator.style();
                style
                    .set_property("transform", &format!("translateY({offset}px)"))
                    .ok();

                let class_list = indicator.class_list();
                if dy >= REFRESH_THRESHOLD {
                    class_list.add_1("ready").ok();
                } else {
                    class_list.remove_1("ready").ok();
                }
            });
            log_el
                .add_event_listener_with_callback(
                    "touchmove",
                    on_touchmove.as_ref().unchecked_ref(),
                )
                .ok();
            on_touchmove.forget();
        }

        // touchend
        {
            let tracking = Rc::clone(&tracking);
            let start_y = Rc::clone(&start_y);
            let indicator = indicator.unchecked_ref::<web_sys::HtmlElement>().clone();
            let log_el = log_el.clone();

            let on_touchend = Closure::<dyn Fn(TouchEvent)>::new(move |e: TouchEvent| {
                if !tracking.get() {
                    return;
                }
                tracking.set(false);

                let Some(touch) = e.changed_touches().get(0) else {
                    tracking.set(false);
                    let style = indicator.style();
                    style.set_property("transform", "translateY(-100%)").ok();
                    indicator.class_list().remove_1("ready").ok();
                    return;
                };
                let dy = touch.client_y() as f64 - start_y.get();

                if dy >= REFRESH_THRESHOLD {
                    // Reload the page.
                    if let Some(w) = window() {
                        let _ = w.location().reload();
                    }
                } else {
                    // Spring back.
                    let style = indicator.style();
                    style
                        .set_property(
                            "transition",
                            "transform 0.2s cubic-bezier(0.0, 0.0, 0.2, 1)",
                        )
                        .ok();
                    style.set_property("transform", "translateY(-100%)").ok();
                    indicator.class_list().remove_1("ready").ok();

                    let indicator2 = indicator.clone();
                    let cleanup = Closure::<dyn Fn(Event)>::new(move |_| {
                        let style = indicator2.style();
                        style.set_property("transition", "").ok();
                        style.set_property("transform", "").ok();
                    });
                    indicator
                        .add_event_listener_with_callback(
                            "transitionend",
                            cleanup.as_ref().unchecked_ref(),
                        )
                        .ok();
                    cleanup.forget();
                }
            });
            log_el
                .add_event_listener_with_callback("touchend", on_touchend.as_ref().unchecked_ref())
                .ok();
            on_touchend.forget();
        }

        // touchcancel
        {
            let tracking = Rc::clone(&tracking);
            let indicator = indicator.unchecked_ref::<web_sys::HtmlElement>().clone();
            let log_el = log_el.clone();

            let on_touchcancel = Closure::<dyn Fn(Event)>::new(move |_| {
                if !tracking.get() {
                    return;
                }
                tracking.set(false);
                let style = indicator.style();
                style.set_property("transform", "translateY(-100%)").ok();
                indicator.class_list().remove_1("ready").ok();
            });
            log_el
                .add_event_listener_with_callback(
                    "touchcancel",
                    on_touchcancel.as_ref().unchecked_ref(),
                )
                .ok();
            on_touchcancel.forget();
        }
    });

    view! {
        <div id="refreshIndicator" node_ref=node_ref>
            <span class="refresh-label">"Pull to refresh"</span>
        </div>
    }
}
