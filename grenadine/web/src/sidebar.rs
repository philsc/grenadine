//! The collapsible, resizable right sidebar the PR page and an agent
//! session's page share. On wide windows it's a column the toggle
//! collapses, which is remembered, and the divider resizes, also
//! remembered; on narrow ones it's an overlay the toggle opens, which
//! isn't.

use std::time::Duration;

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use crate::inboxes::{load_flag, save_flag, storage};

/// The sliver of the page the overlay's sidebar leaves visible at its
/// widest.
const OVERLAY_GAP: f64 = 48.0;

/// What a sidebar needs to decide when it fits and how wide it shows.
#[derive(Clone, Copy)]
pub struct SidebarConfig {
    /// Where the wide-mode collapse is kept, via `load_flag`.
    pub collapsed_key: &'static str,
    /// Where the dragged width is kept, for all pages of the kind.
    pub width_key: &'static str,
    /// The narrowest the sidebar is allowed to get, on column and
    /// overlay alike.
    pub sidebar_min: f64,
    /// The narrowest the content next to the column is allowed to get.
    pub content_min: f64,
    /// The content's width before a drag moves it; the sidebar's
    /// default is the rest of the page.
    pub content_default: f64,
}

/// The PR page's sidebar, with the values it has always used.
pub const PR_SIDEBAR: SidebarConfig = SidebarConfig {
    collapsed_key: "pr-sidebar-collapsed",
    width_key: "grenadine.pr-sidebar-width",
    sidebar_min: 320.0,
    content_min: 1180.0,
    content_default: 1600.0,
};

/// A session page's changes pane. Its row, `.agent-panes`, excludes
/// the session list. The transcript keeps at least 720px and gets
/// 960px by default, and the pane needs 480px for its diffs, so the
/// column fits from a 1500px window while the list is beside it.
pub const CHANGES_SIDEBAR: SidebarConfig = SidebarConfig {
    collapsed_key: "changes-pane-collapsed",
    width_key: "grenadine.changes-pane-width",
    sidebar_min: 480.0,
    content_min: 720.0,
    content_default: 960.0,
};

/// The observer and its callback keeping a sidebar's page width
/// current.
type ResizeWatch = (web_sys::ResizeObserver, Closure<dyn FnMut()>);

/// A page's sidebar state. `Copy`, so the view's closures can take it.
#[derive(Clone, Copy)]
pub struct Sidebar {
    cfg: &'static SidebarConfig,
    /// The page has room for the column; below that the sidebar is an
    /// overlay.
    pub wide: Memo<bool>,
    collapsed: RwSignal<bool>,
    open: RwSignal<bool>,
    width: Signal<f64>,
    resize_start: Callback<web_sys::MouseEvent>,
    resize_reset: Callback<web_sys::MouseEvent>,
    // Read only through the copies the listeners and callbacks hold;
    // kept here so the values live until the component unmounts.
    _page: RwSignal<f64>,
    _saved: RwSignal<Option<f64>>,
    _dragged: StoredValue<bool, LocalStorage>,
    _drag: StoredValue<Option<(WindowListenerHandle, WindowListenerHandle)>, LocalStorage>,
    _watching: StoredValue<Option<ResizeWatch>, LocalStorage>,
}

impl Sidebar {
    /// The page's sidebar, ready to render. `row` is the element whose
    /// width is the page's: the row the sidebar sits in. Until it
    /// mounts the window's width stands in; a ResizeObserver keeps the
    /// page current once it does, so a layout that reflows without a
    /// window resize — like `.agent-panes` when the session list stacks
    /// above it — still gets the right width. The listeners are removed
    /// and the observer disconnected when the component that calls this
    /// unmounts.
    pub fn new(cfg: &'static SidebarConfig, row: NodeRef<leptos::html::Div>) -> Self {
        let page = RwSignal::new(
            window()
                .inner_width()
                .ok()
                .and_then(|w| w.as_f64())
                .unwrap_or(0.0),
        );
        let watching = StoredValue::new_local(None::<ResizeWatch>);
        Effect::new(move |_| {
            let Some(row) = row.get() else {
                return;
            };
            if watching.with_value(|w| w.is_some()) {
                return;
            }
            let el = row.clone();
            let update = move || {
                if !el.is_connected() {
                    return;
                }
                let w = el.client_width() as f64;
                if page.get_untracked() != w {
                    page.set(w);
                }
            };
            // The observer's first callback is asynchronous; without
            // this the page stays the window's width until it fires.
            update();
            let on_resize = Closure::<dyn FnMut()>::new(update);
            if let Ok(observer) = web_sys::ResizeObserver::new(on_resize.as_ref().unchecked_ref()) {
                observer.observe(&row);
                watching.set_value(Some((observer, on_resize)));
            }
        });
        on_cleanup(move || {
            watching.try_with_value(|w| {
                if let Some((observer, _)) = w {
                    observer.disconnect();
                }
            });
        });
        let wide = Memo::new(move |_| page.get() >= cfg.content_min + cfg.sidebar_min);
        let collapsed = RwSignal::new(load_flag(cfg.collapsed_key, false));
        let open = RwSignal::new(false);
        Effect::new(move |_| {
            if wide.get() {
                open.set(false);
            }
        });
        // The width the divider last dragged to; `None` is the default
        // split. It's written when a drag ends, so a window too narrow
        // for it shows it clamped rather than losing it.
        let saved = RwSignal::new(saved_width(cfg));
        let width = Signal::derive(move || sidebar_width(cfg, saved.get(), page.get(), wide.get()));
        // The click the browser fires on the common ancestor when a drag
        // ends away from the divider would close the overlay, so the next
        // click after a drag is ignored here.
        let dragged = StoredValue::new_local(false);
        let drag = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
        let resize_start = Callback::new(move |e: web_sys::MouseEvent| {
            e.prevent_default();
            let (x, w) = (
                e.client_x() as f64,
                sidebar_width(
                    cfg,
                    saved.get_untracked(),
                    page.get_untracked(),
                    wide.get_untracked(),
                ),
            );
            if let Some(body) = document().body() {
                let _ = body.class_list().add_1("resizing-sidebar");
            }
            let on_move = window_event_listener(leptos::ev::mousemove, move |e| {
                saved.set(Some(sidebar_width(
                    cfg,
                    Some(w + x - e.client_x() as f64),
                    page.get_untracked(),
                    wide.get_untracked(),
                )));
            });
            let on_up = window_event_listener(leptos::ev::mouseup, move |_| {
                if let (Some(s), Some(w)) = (storage(), saved.get_untracked()) {
                    let _ = s.set_item(cfg.width_key, &w.to_string());
                }
                if let Some(body) = document().body() {
                    let _ = body.class_list().remove_1("resizing-sidebar");
                }
                drag.update_value(|d| {
                    if let Some((a, b)) = d.take() {
                        a.remove();
                        b.remove();
                    }
                });
                dragged.set_value(true);
                // The click that follows the mouseup is dispatched before
                // timers run; releasing the mouse outside the window fires
                // none, so the flag resets itself for the next click.
                set_timeout(move || dragged.set_value(false), Duration::ZERO);
            });
            drag.set_value(Some((on_move, on_up)));
        });
        let resize_reset = Callback::new(move |_: web_sys::MouseEvent| {
            saved.set(None);
            if let Some(s) = storage() {
                let _ = s.remove_item(cfg.width_key);
            }
        });
        // Escape and clicks outside the overlay's sidebar close it.
        let on_key = window_event_listener(leptos::ev::keydown, move |ev| {
            if ev.key() == "Escape" {
                open.set(false);
            }
        });
        let on_click = window_event_listener(leptos::ev::click, move |ev| {
            if dragged.get_value() {
                dragged.set_value(false);
                return;
            }
            if !open.get_untracked() || wide.get_untracked() {
                return;
            }
            let inside = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .and_then(|t| t.closest(".side-panel, .sidebar-toggle").ok().flatten())
                .is_some();
            if !inside {
                open.set(false);
            }
        });
        on_cleanup(move || {
            on_key.remove();
            on_click.remove();
        });
        Self {
            cfg,
            wide,
            collapsed,
            open,
            width,
            resize_start,
            resize_reset,
            _page: page,
            _saved: saved,
            _dragged: dragged,
            _drag: drag,
            _watching: watching,
        }
    }

    /// Whether the sidebar shows: expanded on a wide window, opened on
    /// a narrow one.
    pub fn shown(&self) -> bool {
        if self.wide.get() {
            !self.collapsed.get()
        } else {
            self.open.get()
        }
    }

    /// `shown` for the view's `then` and `derive`.
    pub fn shown_signal(&self) -> Signal<bool> {
        let this = *self;
        Signal::derive(move || this.shown())
    }

    /// The sidebar's width, clamped to what the page leaves it.
    pub fn width(&self) -> Signal<f64> {
        self.width
    }

    fn toggle(&self) {
        if self.wide.get_untracked() {
            self.collapsed.update(|c| *c = !*c);
            save_flag(self.cfg.collapsed_key, self.collapsed.get_untracked());
        } else {
            self.open.update(|o| *o = !*o);
        }
    }

    /// While the sidebar shows, its toggle sits in the sidebar's
    /// header; while hidden, it floats clear of the main column's
    /// scrollbar at the row's top-right.
    pub fn button(&self, floating: bool) -> impl IntoView + use<> {
        let this = *self;
        view! {
            <button type="button" class="sidebar-toggle" class:floating=floating
                aria-label=move || if this.shown() { "Hide sidebar" } else { "Show sidebar" }
                aria-expanded=move || this.shown().to_string()
                on:click=move |_| this.toggle()>
                <svg
                    viewBox="0 0 16 16"
                    width="14"
                    height="14"
                    fill="none"
                    stroke="currentColor"
                    stroke-width="1.5"
                    stroke-linecap="round"
                    stroke-linejoin="round"
                    aria-hidden="true"
                >
                    <path d=move || if this.shown() { "M6 4l4 4-4 4" } else { "M10 4l-4 4 4 4" } />
                </svg>
            </button>
        }
    }
}

/// The sidebar itself: a full-height column whose body scrolls on its
/// own, floated over the content on windows too narrow for a column.
/// The divider at its left edge resizes it. `class` is the page's own
/// class on top of `.side-panel`.
#[component]
pub fn SidePanel(
    sidebar: Sidebar,
    #[prop(into)] class: String,
    children: Children,
) -> impl IntoView {
    let class = format!("side-panel {class}");
    view! {
        <aside
            class=class
            class:overlay=move || !sidebar.wide.get()
            style:width=move || format!("{}px", sidebar.width().get())
        >
            <div
                class="sidebar-divider"
                role="separator"
                aria-orientation="vertical"
                title="Drag to resize"
                on:mousedown=move |e| sidebar.resize_start.run(e)
                on:dblclick=move |e| sidebar.resize_reset.run(e)
            ></div>
            {children()}
        </aside>
    }
}

/// The sidebar width kept from the last drag; `None` is the default
/// split.
fn saved_width(cfg: &SidebarConfig) -> Option<f64> {
    storage()
        .and_then(|s| s.get_item(cfg.width_key).ok().flatten())
        .and_then(|w| w.parse().ok())
}

/// The width the sidebar shows at. A `wide` page clamps it to what the
/// content's minimum leaves, an overlay to the page minus a sliver, and
/// the minimum wins where even that isn't room. `saved` too wide is
/// clamped rather than dropped, so widening the window brings it back.
fn sidebar_width(cfg: &SidebarConfig, saved: Option<f64>, page: f64, wide: bool) -> f64 {
    let max = if wide {
        page - cfg.content_min
    } else {
        page - OVERLAY_GAP
    };
    saved
        .unwrap_or(page - cfg.content_default)
        .min(max)
        .max(cfg.sidebar_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_defaults_to_the_split() {
        assert_eq!(sidebar_width(&PR_SIDEBAR, None, 2400.0, true), 800.0);
        assert_eq!(sidebar_width(&PR_SIDEBAR, None, 1280.0, false), 320.0);
        // The session's pane defaults to what's left of the
        // transcript's 960px, never narrower than its 480px.
        assert_eq!(sidebar_width(&CHANGES_SIDEBAR, None, 1200.0, true), 480.0);
        assert_eq!(sidebar_width(&CHANGES_SIDEBAR, None, 1500.0, true), 540.0);
    }

    #[test]
    fn sidebar_width_clamps_the_column() {
        assert_eq!(sidebar_width(&PR_SIDEBAR, Some(100.0), 2400.0, true), 320.0);
        assert_eq!(
            sidebar_width(&PR_SIDEBAR, Some(2000.0), 2400.0, true),
            1220.0
        );
        // A window too narrow for the saved width clamps it rather than
        // dropping it.
        assert_eq!(
            sidebar_width(&PR_SIDEBAR, Some(1000.0), 1600.0, true),
            420.0
        );
        // The session's transcript keeps its 720px.
        assert_eq!(
            sidebar_width(&CHANGES_SIDEBAR, Some(2000.0), 1500.0, true),
            780.0
        );
        assert_eq!(
            sidebar_width(&CHANGES_SIDEBAR, Some(100.0), 1500.0, true),
            480.0
        );
    }

    #[test]
    fn sidebar_width_clamps_the_overlay() {
        assert_eq!(
            sidebar_width(&PR_SIDEBAR, Some(5000.0), 1280.0, false),
            1232.0
        );
        // The minimum wins even on a page narrower than it.
        assert_eq!(sidebar_width(&PR_SIDEBAR, None, 300.0, false), 320.0);
        assert_eq!(
            sidebar_width(&CHANGES_SIDEBAR, Some(2000.0), 900.0, false),
            852.0
        );
        assert_eq!(sidebar_width(&CHANGES_SIDEBAR, None, 400.0, false), 480.0);
    }
}
