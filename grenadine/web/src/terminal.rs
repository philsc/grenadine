//! The terminal panel under a session's transcript: tabs of login shells in
//! the session's worktree, shown with xterm.js. The server keeps the shells
//! running, so they outlive the page; a tab that connects gets their recent
//! output first.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::time::Duration;

use grenadine_core::api::TerminalSize;
use leptos::ev;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use web_sys::{BinaryType, MessageEvent, WebSocket};

use crate::inboxes::{load_flag, save_flag, storage};
use crate::{Updates, api};

#[wasm_bindgen]
extern "C" {
    /// xterm.js's terminal, from /xterm.js.
    #[wasm_bindgen(js_name = Terminal)]
    type XTerm;
    #[wasm_bindgen(constructor, js_class = "Terminal")]
    fn new(options: &JsValue) -> XTerm;
    #[wasm_bindgen(method)]
    fn open(this: &XTerm, parent: &web_sys::HtmlElement);
    #[wasm_bindgen(method)]
    fn write(this: &XTerm, data: &js_sys::Uint8Array);
    #[wasm_bindgen(method)]
    fn reset(this: &XTerm);
    #[wasm_bindgen(method)]
    fn focus(this: &XTerm);
    #[wasm_bindgen(method)]
    fn dispose(this: &XTerm);
    #[wasm_bindgen(method, js_name = loadAddon)]
    fn load_addon(this: &XTerm, addon: &Fit);
    #[wasm_bindgen(method, js_name = onData)]
    fn on_data(this: &XTerm, f: &Closure<dyn FnMut(String)>);
    #[wasm_bindgen(method, js_name = onBinary)]
    fn on_binary(this: &XTerm, f: &Closure<dyn FnMut(String)>);
    #[wasm_bindgen(method, js_name = onResize)]
    fn on_resize(this: &XTerm, f: &Closure<dyn FnMut()>);
    #[wasm_bindgen(method, getter)]
    fn cols(this: &XTerm) -> u16;
    #[wasm_bindgen(method, getter)]
    fn rows(this: &XTerm) -> u16;
    #[wasm_bindgen(method, getter)]
    fn options(this: &XTerm) -> JsValue;

    /// xterm.js's addon that fits a terminal to its element, from
    /// /addon-fit.js.
    #[wasm_bindgen(js_namespace = FitAddon, js_name = FitAddon)]
    type Fit;
    #[wasm_bindgen(constructor, js_namespace = FitAddon, js_class = "FitAddon")]
    fn new() -> Fit;
    #[wasm_bindgen(method)]
    fn fit(this: &Fit);

    /// The Keyboard Lock API's `navigator.keyboard`, in Chromium only.
    type Keyboard;
    #[wasm_bindgen(method)]
    fn lock(this: &Keyboard, codes: &js_sys::Array) -> js_sys::Promise;
    #[wasm_bindgen(method)]
    fn unlock(this: &Keyboard);
}

/// The keys the browser keeps for itself (closing the tab, a new window, ...)
/// that a terminal wants: ^W, ^N, ^T, ^Q and the like.
const LOCKED_KEYS: &[&str] = &[
    "KeyW",
    "KeyN",
    "KeyT",
    "KeyQ",
    "Tab",
    "PageUp",
    "PageDown",
    "BracketLeft",
    "BracketRight",
];

/// The browser's keyboard lock, which hands the keys it keeps for itself to
/// the page, but only while the page is full screen. Firefox has none;
/// there the site's "Override keyboard shortcuts" permission does the same.
fn keyboard() -> Option<Keyboard> {
    let keyboard = js_sys::Reflect::get(&window(), &"navigator".into())
        .and_then(|n| js_sys::Reflect::get(&n, &"keyboard".into()))
        .ok()?;
    js_sys::Reflect::get(&keyboard, &"lock".into())
        .ok()?
        .is_function()
        .then(|| keyboard.unchecked_into())
}

/// Locks the keys while a terminal has focus, so that elsewhere on the page,
/// such as in the prompt, they keep doing what the browser does.
fn lock_keys(locked: bool) {
    let Some(keyboard) = keyboard() else { return };
    if locked {
        let codes: js_sys::Array = LOCKED_KEYS.iter().map(|&k| JsValue::from(k)).collect();
        let promise = keyboard.lock(&codes);
        // It's refused outside a top-level, active page; nothing to do then.
        spawn_local(async move {
            let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
        });
    } else {
        keyboard.unlock();
    }
}

/// Puts the page full screen, where the keyboard lock holds, or takes it
/// out again.
fn toggle_full_screen() {
    let document = document();
    if document.fullscreen_element().is_some() {
        document.exit_fullscreen();
        return;
    }
    if let Some(root) = document.document_element() {
        let _ = root.request_fullscreen();
    }
    // The click moved the focus to the button; give it back to the shell.
    if let Ok(Some(input)) =
        document.query_selector(".terminal-host:not([hidden]) .xterm-helper-textarea")
        && let Ok(input) = input.dyn_into::<web_sys::HtmlElement>()
    {
        let _ = input.focus();
    }
}

/// Where whether session `id`'s panel is open is kept. Each session has its
/// own, so that a terminal open in one doesn't open the panel in the others.
fn open_key(id: &str) -> String {
    format!("grenadine.terminal-open.{id}")
}

/// Where the tab last selected in session `id` is kept, so that switching
/// to another session and back shows the same terminal.
fn selected_key(id: &str) -> String {
    format!("grenadine.terminal-selected.{id}")
}

/// Drops what's kept for session `id`'s panel, once the session is deleted.
pub fn forget(id: &str) {
    if let Some(s) = storage() {
        let _ = s.remove_item(&open_key(id));
        let _ = s.remove_item(&selected_key(id));
    }
}

/// The panel's height in pixels, for all sessions.
const HEIGHT_KEY: &str = "grenadine.terminal-height";
const DEFAULT_HEIGHT: f64 = 320.0;
const MIN_HEIGHT: f64 = 120.0;

/// How long a tab waits before it reconnects to a shell that still runs.
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// The panel's height when dragged to `height`, given the window's.
fn clamp_height(height: f64, window_height: f64) -> f64 {
    height.min(window_height * 0.85).max(MIN_HEIGHT)
}

/// The GitHub terminal colors, which go with the page's.
fn theme() -> JsValue {
    let dark = document()
        .document_element()
        .and_then(|e| e.get_attribute("data-theme"))
        .is_some_and(|t| t == "dark");
    let theme = if dark {
        serde_json::json!({
            "background": "#0d1117", "foreground": "#e6edf3", "cursor": "#e6edf3",
            "selectionBackground": "rgba(68, 147, 248, 0.4)",
            "black": "#484f58", "red": "#ff7b72", "green": "#3fb950", "yellow": "#d29922",
            "blue": "#58a6ff", "magenta": "#bc8cff", "cyan": "#39c5cf", "white": "#b1bac4",
            "brightBlack": "#6e7681", "brightRed": "#ffa198", "brightGreen": "#56d364",
            "brightYellow": "#e3b341", "brightBlue": "#79c0ff", "brightMagenta": "#d2a8ff",
            "brightCyan": "#56d4dd", "brightWhite": "#ffffff",
        })
    } else {
        serde_json::json!({
            "background": "#ffffff", "foreground": "#1f2328", "cursor": "#1f2328",
            "selectionBackground": "rgba(9, 105, 218, 0.25)",
            "black": "#24292f", "red": "#cf222e", "green": "#116329", "yellow": "#4d2d00",
            "blue": "#0969da", "magenta": "#8250df", "cyan": "#1b7c83", "white": "#6e7781",
            "brightBlack": "#57606a", "brightRed": "#a40e26", "brightGreen": "#1a7f37",
            "brightYellow": "#633c01", "brightBlue": "#218bff", "brightMagenta": "#a475f9",
            "brightCyan": "#3192aa", "brightWhite": "#8c959f",
        })
    };
    js_sys::JSON::parse(&theme.to_string()).unwrap_or(JsValue::NULL)
}

fn socket_url(session: &str, terminal: u64) -> String {
    let location = window().location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss:",
        _ => "ws:",
    };
    let host = location.host().unwrap_or_default();
    format!("{scheme}//{host}/api/agents/{session}/terminals/{terminal}/ws")
}

/// One WebSocket to the shell and its handlers; dropping it closes it.
struct Connection {
    socket: WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

/// A tab's emulator and its connection to the shell.
struct Live {
    session: String,
    terminal: u64,
    xterm: XTerm,
    fit: Fit,
    connection: RefCell<Option<Connection>>,
    /// What was typed while the connection was still opening, sent once
    /// it opens.
    pending: RefCell<Vec<u8>>,
    stopped: Cell<bool>,
    observer: RefCell<Option<web_sys::ResizeObserver>>,
    _on_data: Closure<dyn FnMut(String)>,
    _on_binary: Closure<dyn FnMut(String)>,
    _on_resize: Closure<dyn FnMut()>,
    _on_observe: RefCell<Option<Closure<dyn FnMut()>>>,
    on_theme: RefCell<Option<WindowListenerHandle>>,
}

impl Live {
    fn start(session: String, terminal: u64, host: &web_sys::HtmlElement) -> Rc<Live> {
        let options = js_sys::Object::new();
        let set = |k: &str, v: JsValue| {
            let _ = js_sys::Reflect::set(&options, &k.into(), &v);
        };
        set("cursorBlink", true.into());
        set("fontSize", 13.into());
        set(
            "fontFamily",
            "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace"
                .into(),
        );
        set("scrollback", 10_000.into());
        set("theme", theme());
        let xterm = XTerm::new(&options);
        let fit = Fit::new();
        xterm.load_addon(&fit);

        Rc::new_cyclic(|weak: &Weak<Live>| {
            let w = weak.clone();
            let on_data = Closure::<dyn FnMut(String)>::new(move |data: String| {
                if let Some(live) = w.upgrade() {
                    live.send(data.as_bytes());
                }
            });
            xterm.on_data(&on_data);
            // Mouse reports in some modes: a byte per character.
            let w = weak.clone();
            let on_binary = Closure::<dyn FnMut(String)>::new(move |data: String| {
                if let Some(live) = w.upgrade() {
                    let bytes: Vec<u8> = data.chars().map(|c| c as u32 as u8).collect();
                    live.send(&bytes);
                }
            });
            xterm.on_binary(&on_binary);
            let w = weak.clone();
            let on_resize = Closure::<dyn FnMut()>::new(move || {
                if let Some(live) = w.upgrade() {
                    live.send_size();
                }
            });
            xterm.on_resize(&on_resize);
            Live {
                session,
                terminal,
                xterm,
                fit,
                connection: RefCell::new(None),
                pending: RefCell::new(Vec::new()),
                stopped: Cell::new(false),
                observer: RefCell::new(None),
                _on_data: on_data,
                _on_binary: on_binary,
                _on_resize: on_resize,
                _on_observe: RefCell::new(None),
                on_theme: RefCell::new(None),
            }
        })
        .attached(host)
    }

    /// Opens the emulator in `host`, keeps it fitted to it and connects.
    fn attached(self: Rc<Self>, host: &web_sys::HtmlElement) -> Rc<Self> {
        self.xterm.open(host);
        let w = Rc::downgrade(&self);
        // Also fires when a hidden tab is shown again.
        let on_observe = Closure::<dyn FnMut()>::new(move || {
            if let Some(live) = w.upgrade() {
                live.fit.fit();
            }
        });
        if let Ok(observer) = web_sys::ResizeObserver::new(on_observe.as_ref().unchecked_ref()) {
            observer.observe(host);
            *self.observer.borrow_mut() = Some(observer);
        }
        *self._on_observe.borrow_mut() = Some(on_observe);
        let w = Rc::downgrade(&self);
        *self.on_theme.borrow_mut() = Some(window_event_listener_untyped(
            crate::theme::CHANGED_EVENT,
            move |_| {
                if let Some(live) = w.upgrade() {
                    let _ = js_sys::Reflect::set(&live.xterm.options(), &"theme".into(), &theme());
                }
            },
        ));
        self.connect();
        self
    }

    fn connect(self: &Rc<Self>) {
        let Ok(socket) = WebSocket::new(&socket_url(&self.session, self.terminal)) else {
            return;
        };
        socket.set_binary_type(BinaryType::Arraybuffer);
        let w = Rc::downgrade(self);
        let on_open = Closure::<dyn FnMut()>::new(move || {
            if let Some(live) = w.upgrade() {
                // The server starts over with the recent output.
                live.xterm.reset();
                live.send_size();
                let pending = std::mem::take(&mut *live.pending.borrow_mut());
                if !pending.is_empty() {
                    live.send(&pending);
                }
            }
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        let w = Rc::downgrade(self);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let (Some(live), Ok(buf)) = (w.upgrade(), e.data().dyn_into::<js_sys::ArrayBuffer>())
            {
                live.xterm.write(&js_sys::Uint8Array::new(&buf));
            }
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let w = Rc::downgrade(self);
        let on_close = Closure::<dyn FnMut()>::new(move || {
            if let Some(live) = w.upgrade() {
                live.reconnect();
            }
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        *self.connection.borrow_mut() = Some(Connection {
            socket,
            _on_open: on_open,
            _on_message: on_message,
            _on_close: on_close,
        });
    }

    /// Reconnects after the connection closed, unless the shell exited, in
    /// which case its tab goes away with the panel's next refetch.
    fn reconnect(self: &Rc<Self>) {
        let w = Rc::downgrade(self);
        set_timeout(
            move || {
                spawn_local(async move {
                    let Some(live) = w.upgrade() else { return };
                    if live.stopped.get() {
                        return;
                    }
                    let open = api::terminals(&live.session)
                        .await
                        .map(|ts| ts.iter().any(|t| t.id == live.terminal));
                    // Unless the server is unreachable, it knows.
                    if open != Ok(false) && !live.stopped.get() {
                        live.connect();
                    }
                });
            },
            RECONNECT_DELAY,
        );
    }

    fn send(&self, data: &[u8]) {
        let Some(c) = &*self.connection.borrow() else {
            return;
        };
        match c.socket.ready_state() {
            WebSocket::OPEN => {
                let _ = c.socket.send_with_u8_array(data);
            }
            // Keys typed right after a tab opens would otherwise be lost.
            WebSocket::CONNECTING => self.pending.borrow_mut().extend_from_slice(data),
            _ => {}
        }
    }

    fn send_size(&self) {
        let size = TerminalSize {
            cols: self.xterm.cols(),
            rows: self.xterm.rows(),
        };
        if let Some(c) = &*self.connection.borrow()
            && c.socket.ready_state() == WebSocket::OPEN
            && let Ok(json) = serde_json::to_string(&size)
        {
            let _ = c.socket.send_with_str(&json);
        }
    }

    fn show(&self) {
        self.fit.fit();
        self.xterm.focus();
    }

    fn stop(&self) {
        self.stopped.set(true);
        *self.connection.borrow_mut() = None;
        if let Some(o) = self.observer.borrow_mut().take() {
            o.disconnect();
        }
        if let Some(h) = self.on_theme.borrow_mut().take() {
            h.remove();
        }
        self.xterm.dispose();
    }
}

/// One shell's tab, which stays connected while another tab is shown.
#[component]
fn TerminalView(session: String, terminal: u64, active: Signal<bool>) -> impl IntoView {
    let host = NodeRef::<leptos::html::Div>::new();
    let live = StoredValue::new_local(None::<Rc<Live>>);
    host.on_load(move |el| {
        let l = Live::start(session, terminal, &el);
        if active.get_untracked() {
            l.show();
        }
        live.set_value(Some(l));
    });
    Effect::new(move |_| {
        if active.get() {
            live.with_value(|l| {
                if let Some(l) = l {
                    l.show();
                }
            });
        }
    });
    on_cleanup(move || {
        live.try_update_value(|l| {
            if let Some(l) = l.take() {
                l.stop();
            }
        });
    });
    view! {
        <div
            class="terminal-host"
            node_ref=host
            hidden=move || !active.get()
            data-terminal=terminal
            on:focusin=move |_| lock_keys(true)
            on:focusout=move |_| lock_keys(false)
        ></div>
    }
}

/// The session's terminals as tabs in a panel that can be collapsed to its
/// tab bar and dragged taller.
#[component]
pub fn TerminalPanel(id: String) -> impl IntoView {
    let updates = expect_context::<Updates>();
    let open_key = open_key(&id);
    let open = RwSignal::new(load_flag(&open_key, false));
    Effect::new(move |_| save_flag(&open_key, open.get()));
    let height = RwSignal::new(
        storage()
            .and_then(|s| s.get_item(HEIGHT_KEY).ok().flatten())
            .and_then(|h| h.parse().ok())
            .unwrap_or(DEFAULT_HEIGHT),
    );
    Effect::new(move |_| {
        if let Some(s) = storage() {
            let _ = s.set_item(HEIGHT_KEY, &height.get().to_string());
        }
    });
    let listed = {
        let id = id.clone();
        LocalResource::new(move || {
            updates.terminals.track();
            let id = id.clone();
            async move { api::terminals(&id).await }
        })
    };
    let terminals = RwSignal::new(Vec::<u64>::new());
    let selected = RwSignal::new(None::<u64>);
    let selected_key = selected_key(&id);
    let stored = storage()
        .and_then(|s| s.get_item(&selected_key).ok().flatten())
        .and_then(|t| t.parse::<u64>().ok());
    Effect::new(move |_| {
        if let (Some(t), Some(s)) = (selected.get(), storage()) {
            let _ = s.set_item(&selected_key, &t.to_string());
        }
    });
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| match listed.get() {
        Some(Ok(list)) => {
            let ids: Vec<u64> = list.iter().map(|t| t.id).collect();
            if !selected.get_untracked().is_some_and(|s| ids.contains(&s)) {
                let kept = stored.filter(|s| ids.contains(s));
                selected.set(kept.or_else(|| ids.last().copied()));
            }
            // Without terminals there's nothing to show, so the panel
            // doesn't come back open on an empty body.
            if ids.is_empty() {
                open.set(false);
            }
            terminals.set(ids);
        }
        Some(Err(e)) => error.set(Some(e)),
        None => {}
    });

    let create = {
        let id = id.clone();
        move || {
            let id = id.clone();
            error.set(None);
            spawn_local(async move {
                match api::open_terminal(&id).await {
                    Ok(t) => {
                        terminals.update(|ts| {
                            if !ts.contains(&t.id) {
                                ts.push(t.id);
                            }
                        });
                        selected.set(Some(t.id));
                        open.set(true);
                    }
                    Err(e) => error.set(Some(e)),
                }
            });
        }
    };
    let toggle = {
        let create = create.clone();
        move |_| {
            open.update(|o| *o = !*o);
            if open.get_untracked() && terminals.with_untracked(Vec::is_empty) {
                create();
            }
        }
    };
    let close = {
        let id = id.clone();
        move |terminal: u64| {
            let id = id.clone();
            spawn_local(async move {
                if let Err(e) = api::close_terminal(&id, terminal).await {
                    error.set(Some(e));
                }
            });
        }
    };

    let drag = StoredValue::new_local(None::<(WindowListenerHandle, WindowListenerHandle)>);
    let start_drag = move |e: web_sys::MouseEvent| {
        e.prevent_default();
        let (y, h) = (e.client_y() as f64, height.get_untracked());
        let on_move = window_event_listener(ev::mousemove, move |e| {
            let window_height = window()
                .inner_height()
                .ok()
                .and_then(|h| h.as_f64())
                .unwrap_or(1000.0);
            height.set(clamp_height(h + y - e.client_y() as f64, window_height));
        });
        let on_up = window_event_listener(ev::mouseup, move |_| {
            drag.update_value(|d| {
                if let Some((a, b)) = d.take() {
                    a.remove();
                    b.remove();
                }
            });
        });
        drag.set_value(Some((on_move, on_up)));
    };

    let full_screen = RwSignal::new(document().fullscreen_element().is_some());
    let on_full_screen = window_event_listener_untyped("fullscreenchange", move |_| {
        full_screen.set(document().fullscreen_element().is_some());
    });
    on_cleanup(move || on_full_screen.remove());

    let session = id;
    view! {
        <section class="terminal-panel" class:open=open>
            {move || open.get().then(|| view! {
                <div class="terminal-resize" title="Drag to resize" on:mousedown=start_drag></div>
            })}
            <div class="terminal-bar">
                <button
                    class="terminal-toggle"
                    aria-expanded=move || open.get().to_string()
                    title="Shells in the session's worktree"
                    on:click=toggle
                >
                    <span class="terminal-chevron" aria-hidden="true">{move || if open.get() { "▾" } else { "▸" }}</span>
                    "Terminal"
                </button>
                <div class="terminal-tabs" role="tablist">
                    <For
                        each=move || terminals.get()
                        key=|t| *t
                        children=move |t| {
                            let close = close.clone();
                            view! {
                                <span
                                    class="terminal-tab"
                                    class:selected=move || selected.get() == Some(t)
                                >
                                    <button
                                        class="terminal-tab-name"
                                        role="tab"
                                        aria-selected=move || (selected.get() == Some(t)).to_string()
                                        on:click=move |_| {
                                            selected.set(Some(t));
                                            open.set(true);
                                        }
                                    >{format!("Terminal {t}")}</button>
                                    <button
                                        class="terminal-tab-close"
                                        aria-label=format!("Close terminal {t}")
                                        title="Close"
                                        on:click=move |_| close(t)
                                    >"×"</button>
                                </span>
                            }
                        }
                    />
                </div>
                <button
                    class="terminal-new"
                    aria-label="New terminal"
                    title="New terminal"
                    on:click=move |_| create()
                >"+"</button>
                {move || error.get().map(|e| view! { <span class="error">{e}</span> })}
                {keyboard().is_some().then(|| view! {
                    <button
                        class="terminal-full-screen"
                        aria-pressed=move || full_screen.get().to_string()
                        title=move || if full_screen.get() {
                            "Leave full screen"
                        } else {
                            "Full screen, where Ctrl+W, Ctrl+N, Ctrl+T and the like reach the terminal (hold Esc to leave)"
                        }
                        on:click=move |_| toggle_full_screen()
                    >{move || if full_screen.get() { "Exit full screen" } else { "Full screen" }}</button>
                })}
            </div>
            <div
                class="terminal-body"
                hidden=move || !open.get()
                style:height=move || format!("{}px", height.get())
            >
                <For
                    each=move || terminals.get()
                    key=|t| *t
                    children=move |t| {
                        let active = Signal::derive(move || open.get() && selected.get() == Some(t));
                        view! { <TerminalView session=session.clone() terminal=t active=active /> }
                    }
                />
            </div>
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn height_stays_usable() {
        assert_eq!(clamp_height(300.0, 1000.0), 300.0);
        assert_eq!(clamp_height(10.0, 1000.0), MIN_HEIGHT);
        assert_eq!(clamp_height(2000.0, 1000.0), 850.0);
        // A tiny window still leaves the terminal its minimum.
        assert_eq!(clamp_height(300.0, 100.0), MIN_HEIGHT);
    }
}
