//! Dictation into text boxes. Record streams the microphone to the server
//! (see the server's `dictation` module), which cuts the speech into
//! phrases at pauses. Each finished phrase is inserted where the cursor
//! was, and the phrase being spoken shows greyed out under the box. The
//! box is read-only while recording. One box records at a time.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::time::Duration;

use grenadine_core::api::{DictationCommand, DictationEvent};
use leptos::html::{Button, Textarea};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    AudioContext, AudioWorkletNode, BinaryType, Blob, BlobPropertyBag, MediaStream,
    MediaStreamConstraints, MediaStreamTrack, MessageEvent, Url, WebSocket,
};

use crate::api;

/// Downsamples the microphone to 16 kHz mono and posts it to the page as
/// little-endian i16 samples, 50 ms at a time.
const WORKLET: &str = r#"
class GrenadineDictation extends AudioWorkletProcessor {
  constructor() {
    super();
    this.ratio = sampleRate / 16000;
    this.pos = 0;
    this.sum = 0;
    this.count = 0;
    this.out = new Int16Array(800);
    this.n = 0;
  }
  process(inputs) {
    const input = inputs[0] && inputs[0][0];
    if (!input) return true;
    for (let i = 0; i < input.length; i++) {
      // Averaging the samples that make up each output sample also
      // filters out what 16 kHz can't represent.
      this.sum += input[i];
      this.count++;
      this.pos += 1;
      if (this.pos >= this.ratio) {
        this.pos -= this.ratio;
        const v = this.sum / this.count;
        this.sum = 0;
        this.count = 0;
        this.out[this.n++] = Math.max(-32768, Math.min(32767, Math.round(v * 32767)));
        if (this.n === this.out.length) {
          this.port.postMessage(this.out.buffer, [this.out.buffer]);
          this.out = new Int16Array(800);
          this.n = 0;
        }
      }
    }
    return true;
  }
}
registerProcessor("grenadine-dictation", GrenadineDictation);
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    /// Waiting for the microphone and the server.
    Starting,
    Recording,
    /// Stopped; the last phrases are still being transcribed.
    Finishing,
}

/// Dictation into one text box: its text, the textarea for the cursor,
/// and the recording's state.
#[derive(Clone, Copy)]
pub struct Dictation {
    id: u64,
    text: Signal<String>,
    set_text: SignalSetter<String>,
    textarea: NodeRef<Textarea>,
    /// The Record button.
    button: NodeRef<Button>,
    phase: RwSignal<Phase>,
    interim: RwSignal<String>,
    error: RwSignal<Option<String>>,
    seconds: RwSignal<u32>,
    /// Where the next phrase goes, as a byte index into `text`.
    cursor: StoredValue<usize>,
    /// Counts the presses of Record, so a start that was stopped, or
    /// started over, doesn't go on.
    attempt: StoredValue<u64>,
}

/// The browser objects of a recording; dropping them ends it.
struct Recorder {
    socket: WebSocket,
    audio: Option<Audio>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(JsValue)>,
}

struct Audio {
    context: AudioContext,
    stream: MediaStream,
    node: AudioWorkletNode,
    _on_samples: Closure<dyn FnMut(MessageEvent)>,
    timer: Option<IntervalHandle>,
}

impl Drop for Audio {
    fn drop(&mut self) {
        if let Some(t) = self.timer.take() {
            t.clear();
        }
        if let Ok(port) = self.node.port() {
            port.set_onmessage(None);
        }
        let _ = self.node.disconnect();
        for track in self.stream.get_tracks() {
            track.unchecked_into::<MediaStreamTrack>().stop();
        }
        let _ = self.context.close();
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

thread_local! {
    static NEXT_ID: Cell<u64> = const { Cell::new(0) };
    /// The recordings that are starting, running or finishing.
    static RECORDERS: RefCell<HashMap<u64, (Dictation, Recorder)>> = RefCell::default();
}

/// Whether Record can work: the server has a model, and the page may use
/// the microphone, which browsers allow on https and localhost only. The
/// app provides it as a context, see `provide`.
#[derive(Clone, Copy)]
struct Available(RwSignal<bool>);

/// Asks the server once whether it transcribes, for every box.
pub fn provide() {
    let available = RwSignal::new(false);
    let mic = window()
        .navigator()
        .media_devices()
        .is_ok_and(|m| !m.is_undefined());
    if mic {
        spawn_local(async move {
            if let Ok(info) = api::dictation_info().await {
                available.set(info.available);
            }
        });
    }
    provide_context(Available(available));
}

/// The byte index in `text` of the UTF-16 offset `utf16`, which is how
/// textareas count; past the end it's the end.
fn byte_index(text: &str, utf16: u32) -> usize {
    let mut units = 0;
    for (i, c) in text.char_indices() {
        if units >= utf16 as usize {
            return i;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// The UTF-16 offset of the byte index `at` in `text`.
fn utf16_offset(text: &str, at: usize) -> u32 {
    text[..at].chars().map(char::len_utf16).sum::<usize>() as u32
}

/// Inserts `phrase` into `text` at byte `at`, with a space before it
/// unless the text before ends in whitespace, and one after it unless the
/// text after starts with whitespace. Returns where the next phrase goes:
/// right after this one, before any space added after it.
fn insert_phrase(text: &mut String, at: usize, phrase: &str) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    let before = !text[..at].is_empty() && !text[..at].ends_with(char::is_whitespace);
    let after = !text[at..].is_empty() && !text[at..].starts_with(char::is_whitespace);
    let mut insert = String::new();
    if before {
        insert.push(' ');
    }
    insert.push_str(phrase);
    let end = at + insert.len();
    if after {
        insert.push(' ');
    }
    text.insert_str(at, &insert);
    end
}

fn socket_url() -> String {
    let location = window().location();
    let scheme = match location.protocol().as_deref() {
        Ok("https:") => "wss:",
        _ => "ws:",
    };
    let host = location.host().unwrap_or_default();
    format!("{scheme}//{host}/api/dictation/ws")
}

fn js_error(e: JsValue) -> String {
    e.dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}

impl Dictation {
    /// Dictation into the box whose text is `text`. The textarea gives
    /// the cursor; without it phrases go at the end.
    pub fn new(text: RwSignal<String>, textarea: NodeRef<Textarea>) -> Dictation {
        Dictation::with_text(text.into(), text.into(), textarea)
    }

    /// Dictation into a box whose text is read through `text` and
    /// written through `set_text`.
    pub fn with_text(
        text: Signal<String>,
        set_text: SignalSetter<String>,
        textarea: NodeRef<Textarea>,
    ) -> Dictation {
        let id = NEXT_ID.with(|n| {
            n.set(n.get() + 1);
            n.get()
        });
        let d = Dictation {
            id,
            text,
            set_text,
            textarea,
            button: NodeRef::new(),
            phase: RwSignal::new(Phase::Idle),
            interim: RwSignal::new(String::new()),
            error: RwSignal::new(None),
            seconds: RwSignal::new(0),
            cursor: StoredValue::new(0),
            attempt: StoredValue::new(0),
        };
        // Leaving the box stops its recording; phrases still being
        // transcribed are dropped with it.
        on_cleanup(move || {
            RECORDERS.with_borrow_mut(|r| r.remove(&id));
        });
        d
    }

    /// The box's textarea, which the box binds.
    pub fn textarea(self) -> NodeRef<Textarea> {
        self.textarea
    }

    /// Whether a recording is going on, from pressing Record until the
    /// last phrase is in: the box is read-only and can't be sent.
    pub fn busy(self) -> bool {
        self.phase.get() != Phase::Idle
    }

    fn toggle(self) {
        match self.phase.get_untracked() {
            Phase::Idle => self.start(),
            Phase::Starting | Phase::Recording => self.stop(),
            // The last phrases are taking a while: drop them, and leave
            // the box as a stop would.
            Phase::Finishing => self.finish(),
        }
    }

    fn start(self) {
        // Only one box records at a time.
        let others: Vec<Dictation> = RECORDERS.with_borrow(|r| {
            r.values()
                .map(|(d, _)| *d)
                .filter(|d| d.id != self.id)
                .collect()
        });
        for d in others {
            d.stop();
        }
        let text = self.text.get_untracked();
        let at = self
            .textarea
            .get_untracked()
            .and_then(|t| t.selection_end().ok().flatten())
            .map_or(text.len(), |c| byte_index(&text, c));
        self.cursor.set_value(at);
        self.interim.set(String::new());
        self.error.set(None);
        self.seconds.set(0);
        self.phase.set(Phase::Starting);
        let prompt = text[..at].to_owned();
        let attempt = self.attempt.get_value() + 1;
        self.attempt.set_value(attempt);
        // Made in the click, since browsers keep an audio context made
        // outside a user gesture suspended: it would record silence.
        let context = match AudioContext::new() {
            Ok(c) => c,
            Err(e) => return self.fail(js_error(e)),
        };
        let _ = context.resume();
        spawn_local(async move {
            if let Err(e) = self.open(prompt, attempt, context.clone()).await {
                let _ = context.close();
                // Stopping while starting makes the start fail; that's no
                // error.
                if self.current(attempt) {
                    self.fail(e);
                }
            }
        });
    }

    /// Whether `attempt` is the recording going on.
    fn current(self, attempt: u64) -> bool {
        self.attempt.try_get_value() == Some(attempt)
            && self
                .phase
                .try_get_untracked()
                .is_some_and(|p| p != Phase::Idle)
    }

    /// Connects to the server, then starts the microphone.
    async fn open(self, prompt: String, attempt: u64, context: AudioContext) -> Result<(), String> {
        let socket = WebSocket::new(&socket_url()).map_err(js_error)?;
        socket.set_binary_type(BinaryType::Arraybuffer);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            let Some(json) = ev.data().as_string() else {
                return;
            };
            match serde_json::from_str(&json) {
                Ok(event) => self.on_event(event),
                Err(e) => self.fail(format!("bad message from the server: {e}")),
            }
        });
        socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        let on_close = Closure::<dyn FnMut(JsValue)>::new(move |_| {
            if self.phase.get_untracked() != Phase::Idle {
                self.fail("the connection to the server closed".to_owned());
            }
        });
        socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        RECORDERS.with_borrow_mut(|r| {
            r.insert(
                self.id,
                (
                    self,
                    Recorder {
                        socket: socket.clone(),
                        audio: None,
                        _on_message: on_message,
                        _on_close: on_close,
                    },
                ),
            )
        });
        let opened = js_sys::Promise::new(&mut |resolve, reject| {
            socket.set_onopen(Some(&resolve));
            socket.set_onerror(Some(&reject));
        });
        JsFuture::from(opened)
            .await
            .map_err(|_| "can't connect to the server".to_owned())?;
        socket.set_onopen(None);
        socket.set_onerror(None);
        if !self.current(attempt) {
            return Err("stopped".to_owned());
        }
        let start = serde_json::to_string(&DictationCommand::Start { prompt }).unwrap_or_default();
        socket.send_with_str(&start).map_err(js_error)?;

        let constraints = MediaStreamConstraints::new();
        let audio = js_sys::Object::new();
        for (key, value) in [
            ("channelCount", JsValue::from(1)),
            ("echoCancellation", true.into()),
            ("noiseSuppression", true.into()),
            ("autoGainControl", true.into()),
        ] {
            let _ = js_sys::Reflect::set(&audio, &key.into(), &value);
        }
        constraints.set_audio(&audio);
        let devices = window().navigator().media_devices().map_err(js_error)?;
        let stream: MediaStream = JsFuture::from(
            devices
                .get_user_media_with_constraints(&constraints)
                .map_err(js_error)?,
        )
        .await
        .map_err(|e| format!("can't use the microphone: {}", js_error(e)))?
        .unchecked_into();
        let options = BlobPropertyBag::new();
        options.set_type("text/javascript");
        let parts = js_sys::Array::of1(&WORKLET.into());
        let blob = Blob::new_with_str_sequence_and_options(&parts, &options).map_err(js_error)?;
        let url = Url::create_object_url_with_blob(&blob).map_err(js_error)?;
        let added = context
            .audio_worklet()
            .and_then(|w| w.add_module(&url))
            .map_err(js_error);
        let added = match added {
            Ok(p) => JsFuture::from(p).await.map_err(js_error),
            Err(e) => Err(e),
        };
        let _ = Url::revoke_object_url(&url);
        added?;
        let source = context
            .create_media_stream_source(&stream)
            .map_err(js_error)?;
        let node = AudioWorkletNode::new(&context, "grenadine-dictation").map_err(js_error)?;
        let on_samples = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Ok(buffer) = ev.data().dyn_into::<js_sys::ArrayBuffer>() {
                RECORDERS.with_borrow(|r| {
                    if let Some((_, rec)) = r.get(&self.id) {
                        let _ = rec.socket.send_with_array_buffer(&buffer);
                    }
                });
            }
        });
        node.port()
            .map_err(js_error)?
            .set_onmessage(Some(on_samples.as_ref().unchecked_ref()));
        source.connect_with_audio_node(&node).map_err(js_error)?;
        // The node only runs when it leads to the speakers; it outputs
        // silence.
        node.connect_with_audio_node(&context.destination())
            .map_err(js_error)?;
        let started = js_sys::Date::now();
        let seconds = self.seconds;
        let timer = set_interval_with_handle(
            move || seconds.set(((js_sys::Date::now() - started) / 1000.0) as u32),
            Duration::from_millis(250),
        )
        .ok();
        let audio = Audio {
            context,
            stream,
            node,
            _on_samples: on_samples,
            timer,
        };
        // Stopped while starting: the audio goes again right away.
        let stopped = RECORDERS.with_borrow_mut(|r| match r.get_mut(&self.id) {
            Some((_, rec))
                if self.current(attempt) && self.phase.get_untracked() == Phase::Starting =>
            {
                rec.audio = Some(audio);
                false
            }
            _ => true,
        });
        if !stopped {
            self.phase.set(Phase::Recording);
        }
        Ok(())
    }

    /// Stops the microphone; the server sends the last phrases, then Done.
    fn stop(self) {
        let running = RECORDERS.with_borrow_mut(|r| {
            let Some((_, rec)) = r.get_mut(&self.id) else {
                return false;
            };
            rec.audio = None;
            let stop = serde_json::to_string(&DictationCommand::Stop).unwrap_or_default();
            rec.socket.ready_state() == WebSocket::OPEN && rec.socket.send_with_str(&stop).is_ok()
        });
        if running {
            self.phase.set(Phase::Finishing);
        } else {
            self.finish();
        }
    }

    /// Ends the recording when the box goes away and no box takes its
    /// place by the next task, as when the box is rebuilt. Call it in the
    /// scope that renders the box, when the `Dictation` outlives that.
    pub fn end_with_box(self) {
        on_cleanup(move || {
            set_timeout(
                move || {
                    let mounted = self
                        .textarea
                        .try_get_untracked()
                        .flatten()
                        .is_some_and(|t| t.is_connected());
                    if !mounted {
                        self.cancel();
                    }
                },
                Duration::ZERO,
            );
        });
    }

    /// Drops the recording, and the phrases still being transcribed, of a
    /// box that's gone.
    fn cancel(self) {
        RECORDERS.with_borrow_mut(|r| r.remove(&self.id));
        self.phase.try_set(Phase::Idle);
        self.interim.try_set(String::new());
    }

    fn on_event(self, event: DictationEvent) {
        match event {
            DictationEvent::Final { text } => {
                if text.is_empty() {
                    return;
                }
                // The box may be gone.
                let Some(mut all) = self.text.try_get_untracked() else {
                    return;
                };
                let next = insert_phrase(&mut all, self.cursor.get_value(), &text);
                self.set_text.set(all);
                self.cursor.set_value(next);
            }
            DictationEvent::Interim { text } => self.interim.set(text),
            DictationEvent::Done => self.finish(),
            DictationEvent::Error { message } => self.fail(message),
        }
    }

    fn fail(self, message: String) {
        self.finish();
        self.error.set(Some(message));
    }

    /// Ends the recording and puts the cursor after the inserted text. The
    /// box gets the focus unless the user has moved on to something else.
    fn finish(self) {
        RECORDERS.with_borrow_mut(|r| r.remove(&self.id));
        if self.phase.get_untracked() == Phase::Idle {
            return;
        }
        self.phase.set(Phase::Idle);
        self.interim.set(String::new());
        if let Some(t) = self.textarea.get_untracked() {
            let at = self.cursor.get_value();
            let pos = self
                .text
                .with_untracked(|text| utf16_offset(text, at.min(text.len())));
            // After the box is editable again.
            let button = self.button;
            request_animation_frame(move || {
                let moved_on = document().active_element().is_some_and(|focused| {
                    let button = button.try_get_untracked().flatten();
                    focused.tag_name() != "BODY"
                        && !t.is_same_node(Some(&focused))
                        && !button.is_some_and(|b| b.is_same_node(Some(&focused)))
                });
                if !moved_on {
                    let _ = t.focus();
                }
                let _ = t.set_selection_range(pos, pos);
            });
        }
    }
}

/// Record, or Stop and how long the recording runs; nothing when the
/// server can't transcribe.
#[component]
pub fn RecordButton(dictation: Dictation) -> impl IntoView {
    let available = use_context::<Available>().map(|a| a.0);
    let d = dictation;
    move || {
        available.is_some_and(|a| a.get()).then(|| {
            let label = move || match d.phase.get() {
                Phase::Idle => "🎤 Record".to_owned(),
                Phase::Starting => "Starting…".to_owned(),
                Phase::Recording => {
                    let s = d.seconds.get();
                    format!("■ Stop · {}:{:02}", s / 60, s % 60)
                }
                Phase::Finishing => "Finishing… · Cancel".to_owned(),
            };
            view! {
                <button
                    node_ref=d.button
                    type="button"
                    class="record"
                    class:recording=move || d.phase.get() == Phase::Recording
                    title=move || if d.phase.get() == Phase::Finishing { "Drop the phrases still being transcribed" } else { "Dictate at the cursor" }
                    on:click=move |_| d.toggle()
                >{label}</button>
            }
        })
    }
}

/// Under the box while recording: the phrase being spoken, greyed out;
/// and what went wrong, if something did.
#[component]
pub fn DictationLine(dictation: Dictation) -> impl IntoView {
    let d = dictation;
    view! {
        {move || d.busy().then(|| view! {
            <div class="dictation-line" aria-live="polite">
                <span class="dictation-dot"></span>
                <span class="dictation-interim">{move || d.interim.get()}</span>
            </div>
        })}
        {move || d.error.get().map(|e| view! { <p class="error">{e}</p> })}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inserts_with_spaces_as_needed() {
        let mut t = "Rename it and".to_owned();
        let at = insert_phrase(&mut t, 13, "update the callers.");
        assert_eq!(t, "Rename it and update the callers.");
        let at = insert_phrase(&mut t, at, "Then test it.");
        assert_eq!(t, "Rename it and update the callers. Then test it.");
        assert_eq!(at, t.len());
    }

    #[test]
    fn inserts_in_the_middle() {
        let mut t = "first last".to_owned();
        let at = insert_phrase(&mut t, 5, "middle");
        assert_eq!(t, "first middle last");
        let at = insert_phrase(&mut t, at, "more");
        assert_eq!(t, "first middle more last");
        assert_eq!(&t[at..], " last");
        // Right before a word, the space goes after the phrase.
        let mut t = "word".to_owned();
        assert_eq!(insert_phrase(&mut t, 0, "A"), 1);
        assert_eq!(t, "A word");
    }

    #[test]
    fn inserts_into_empty_text() {
        let mut t = String::new();
        assert_eq!(insert_phrase(&mut t, 0, "Hello."), 6);
        assert_eq!(t, "Hello.");
        // A cursor past the end or inside a character is clamped.
        let mut t = "é".to_owned();
        insert_phrase(&mut t, 1, "x");
        assert_eq!(t, "x é");
        let mut t = "a".to_owned();
        insert_phrase(&mut t, 9, "b");
        assert_eq!(t, "a b");
    }

    #[test]
    fn converts_utf16_offsets() {
        let text = "a😀b";
        assert_eq!(byte_index(text, 0), 0);
        assert_eq!(byte_index(text, 1), 1);
        assert_eq!(byte_index(text, 3), 5);
        assert_eq!(byte_index(text, 4), 6);
        assert_eq!(byte_index(text, 99), 6);
        assert_eq!(utf16_offset(text, 5), 3);
        assert_eq!(utf16_offset(text, 6), 4);
    }
}
