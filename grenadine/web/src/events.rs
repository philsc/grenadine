//! Server-sent event streams that replace their connection when it goes
//! quiet. After the machine sleeps, the old connection is often dead without
//! the browser noticing, and an `EventSource` would wait on it forever.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use web_sys::EventSource;

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

fn now() -> f64 {
    js_sys::Date::now()
}

/// What every connection to the same URL shares.
struct Handlers {
    last_seen: Cell<f64>,
    on_message: RefCell<Box<dyn FnMut(String)>>,
    on_open: RefCell<Box<dyn FnMut()>>,
}

/// One `EventSource` and its handlers; dropping it closes the connection.
struct Connection {
    source: EventSource,
    _on_message: Closure<dyn FnMut(web_sys::MessageEvent)>,
    _on_open: Closure<dyn FnMut()>,
    _on_ping: Closure<dyn FnMut()>,
}

impl Connection {
    fn open(url: &str, handlers: &Rc<Handlers>) -> Option<Self> {
        let source = EventSource::new(url).ok()?;
        let h = handlers.clone();
        let on_message =
            Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |e: web_sys::MessageEvent| {
                h.last_seen.set(now());
                if let Some(data) = e.data().as_string() {
                    (h.on_message.borrow_mut())(data);
                }
            });
        source.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let h = handlers.clone();
        let on_open = Closure::<dyn FnMut()>::new(move || {
            h.last_seen.set(now());
            (h.on_open.borrow_mut())();
        });
        source.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        let h = handlers.clone();
        let on_ping = Closure::<dyn FnMut()>::new(move || h.last_seen.set(now()));
        let _ = source.add_event_listener_with_callback("ping", on_ping.as_ref().unchecked_ref());
        Some(Self {
            source,
            _on_message: on_message,
            _on_open: on_open,
            _on_ping: on_ping,
        })
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.source.close();
    }
}

struct State {
    url: String,
    handlers: Rc<Handlers>,
    connection: Option<Connection>,
    last_check: f64,
}

/// Follows the event stream at `url` for the life of the current owner.
/// `on_message` gets each message's data, and `on_open` runs on every
/// (re)connect, so that it can refetch whatever was missed.
pub fn subscribe(
    url: String,
    on_message: impl FnMut(String) + 'static,
    on_open: impl FnMut() + 'static,
) {
    let handlers = Rc::new(Handlers {
        last_seen: Cell::new(now()),
        on_message: RefCell::new(Box::new(on_message)),
        on_open: RefCell::new(Box::new(on_open)),
    });
    let connection = Connection::open(&url, &handlers);
    let state = StoredValue::new_local(State {
        url,
        handlers,
        connection,
        last_check: now(),
    });

    let check = move || {
        state.try_update_value(|s| {
            let now = now();
            if should_reconnect(now, s.handlers.last_seen.get(), s.last_check) {
                // Close the old connection before opening its replacement.
                s.connection = None;
                s.handlers.last_seen.set(now);
                s.connection = Connection::open(&s.url, &s.handlers);
            }
            s.last_check = now;
        });
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
}
