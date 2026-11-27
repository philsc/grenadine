//! The server's event streams, over WebSockets that are replaced when they
//! close or go quiet. After the machine sleeps, the old connection is
//! often dead without the browser noticing.
//!
//! They aren't server-sent events because a browser keeps only six HTTP
//! connections to a server. Every tab has two streams, so a third tab
//! took the last ones and no further request got through.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use grenadine_core::api::PING;
use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use web_sys::{MessageEvent, WebSocket};

/// How often the connection is checked.
const CHECK_EVERY: Duration = Duration::from_secs(5);
/// The server pings every 15 seconds while there is nothing else to send,
/// so this much silence means a few pings went missing.
const SILENCE_MS: f64 = 40_000.0;
/// A check this late means the timers were paused, i.e. the machine slept.
/// A connection that was also silent for that long is then replaced
/// without waiting for the full `SILENCE_MS`.
const SLEPT_MS: f64 = 20_000.0;

/// Whether to replace the connection, given the times (in ms) of now, the
/// last thing received and the previous check.
fn should_reconnect(now: f64, last_seen: f64, last_check: f64) -> bool {
    let silent = now - last_seen;
    silent > SILENCE_MS || (now - last_check > SLEPT_MS && silent > SLEPT_MS)
}

/// The first wait before replacing a connection that closed, and the
/// longest one.
const RETRY_FIRST: Duration = Duration::from_secs(1);
const RETRY_LONGEST: Duration = Duration::from_secs(10);

/// How long to wait before replacing a connection that closed, after
/// `failures` earlier ones closed without opening: twice as long each
/// time, so that a server that is down isn't asked every second.
fn retry_delay(failures: u32) -> Duration {
    RETRY_FIRST
        .saturating_mul(2u32.saturating_pow(failures))
        .min(RETRY_LONGEST)
}

/// The WebSocket URL of `path` on the page's server.
pub fn socket_url(path: &str) -> String {
    let location = window().location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss:",
        _ => "ws:",
    };
    let host = location.host().unwrap_or_default();
    format!("{scheme}//{host}{path}")
}

fn now() -> f64 {
    js_sys::Date::now()
}

/// What every connection to the same URL shares.
struct Handlers {
    last_seen: Cell<f64>,
    /// How many connections in a row closed without opening.
    failures: Cell<u32>,
    on_message: RefCell<Box<dyn FnMut(String)>>,
    on_open: RefCell<Box<dyn FnMut()>>,
}

/// One `WebSocket` and its handlers; dropping it closes the connection.
struct Connection {
    socket: WebSocket,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_open: Closure<dyn FnMut()>,
    _on_close: Closure<dyn FnMut()>,
}

impl Connection {
    /// Connects to `path`. `on_close` runs when the connection closes
    /// other than by being dropped, also when it never opened. That
    /// includes a server that refuses the socket, e.g. for a session that
    /// was deleted: a page can't tell that from a server that is down, so
    /// a tab left on such a page keeps asking, at the longest delay.
    fn open(path: &str, handlers: &Rc<Handlers>, on_close: impl Fn() + 'static) -> Option<Self> {
        let socket = WebSocket::new(&socket_url(path)).ok()?;
        let h = handlers.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            h.last_seen.set(now());
            if let Some(data) = e.data().as_string().filter(|data| data != PING) {
                (h.on_message.borrow_mut())(data);
            }
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let h = handlers.clone();
        let on_open = Closure::<dyn FnMut()>::new(move || {
            h.last_seen.set(now());
            h.failures.set(0);
            (h.on_open.borrow_mut())();
        });
        socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        let on_close = Closure::<dyn FnMut()>::new(on_close);
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        Some(Self {
            socket,
            _on_message: on_message,
            _on_open: on_open,
            _on_close: on_close,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // The handlers go away with this; a close event after that would
        // call into freed closures.
        self.socket.set_onmessage(None);
        self.socket.set_onopen(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

struct State {
    url: String,
    handlers: Rc<Handlers>,
    connection: Option<Connection>,
    /// Counts the connections, so that a wait to replace one that closed
    /// can tell whether it was replaced meanwhile.
    generation: u64,
    last_check: f64,
}

type Stored = StoredValue<State, LocalStorage>;

/// Replaces the connection with a new one to the current URL.
fn connect(state: Stored) {
    state.try_update_value(|s| {
        // Close the old connection before opening its replacement.
        s.connection = None;
        s.generation += 1;
        let generation = s.generation;
        s.handlers.last_seen.set(now());
        let handlers = s.handlers.clone();
        s.connection = Connection::open(&s.url, &s.handlers, move || {
            let failures = handlers.failures.get();
            handlers.failures.set(failures.saturating_add(1));
            // Not right away: that would drop this handler while it runs.
            set_timeout(
                move || {
                    if state.try_with_value(|s| s.generation) == Some(generation) {
                        connect(state);
                    }
                },
                retry_delay(failures),
            );
        });
    });
}

/// Follows the event stream at `url`, a path on the server, for the life
/// of the current owner; a new `url` closes the connection and opens a
/// new one. `on_message` gets each message's data, and `on_open` runs on
/// every (re)connect, so that it can refetch whatever was missed.
pub fn subscribe(
    url: Signal<String>,
    on_message: impl FnMut(String) + 'static,
    on_open: impl FnMut() + 'static,
) {
    let handlers = Rc::new(Handlers {
        last_seen: Cell::new(now()),
        failures: Cell::new(0),
        on_message: RefCell::new(Box::new(on_message)),
        on_open: RefCell::new(Box::new(on_open)),
    });
    let state: Stored = StoredValue::new_local(State {
        url: url.get_untracked(),
        handlers,
        connection: None,
        generation: 0,
        last_check: now(),
    });
    connect(state);

    // A new URL replaces the connection; the new one's `on_open`
    // refetches, which covers what a missed message would have.
    Effect::new(move |_| {
        let url = url.get();
        let changed = state.try_update_value(|s| {
            let changed = s.url != url;
            s.url = url;
            changed
        });
        if changed == Some(true) {
            connect(state);
        }
    });

    let check = move || {
        let now = now();
        let silent = state.try_update_value(|s| {
            let silent = should_reconnect(now, s.handlers.last_seen.get(), s.last_check);
            s.last_check = now;
            silent
        });
        if silent == Some(true) {
            connect(state);
        }
    };
    let interval = set_interval_with_handle(check, CHECK_EVERY);
    // Coming back to the tab or the network is when a dead connection
    // matters, so check right away rather than on the next tick.
    let on_visible = window_event_listener_untyped("visibilitychange", move |_| {
        if !document().hidden() {
            check();
        }
    });
    let on_online = window_event_listener_untyped("online", move |_| check());
    on_cleanup(move || {
        if let Ok(h) = interval {
            h.clear();
        }
        on_visible.remove();
        on_online.remove();
        state.try_update_value(|s| s.connection = None);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnects_when_silent_or_after_sleeping() {
        // Pings arriving: keep the connection.
        assert!(!should_reconnect(100_000.0, 90_000.0, 95_000.0));
        // A quiet stream that still gets checked on time.
        assert!(!should_reconnect(100_000.0, 61_000.0, 95_000.0));
        assert!(should_reconnect(100_000.0, 59_000.0, 95_000.0));
        // Woken up after a sleep with nothing received meanwhile.
        assert!(should_reconnect(100_000.0, 75_000.0, 75_000.0));
        // A throttled background tab whose pings still arrive.
        assert!(!should_reconnect(100_000.0, 90_000.0, 40_000.0));
    }

    #[test]
    fn closed_connections_are_replaced_after_longer_and_longer() {
        let secs = |failures| retry_delay(failures).as_secs();
        assert_eq!([0, 1, 2, 3].map(secs), [1, 2, 4, 8]);
        assert_eq!([4, 5, 40, u32::MAX].map(secs), [10; 4]);
    }
}
