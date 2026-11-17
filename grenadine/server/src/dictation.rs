//! Dictation: the page streams microphone audio over a WebSocket and gets
//! back text to insert into a text box. The audio is cut into phrases at
//! pauses. Each finished phrase is transcribed once, with the whole model,
//! and never changes after that. The phrase still being spoken is
//! transcribed again about every second, so the page can preview it.
//! Transcription runs locally with whisper.cpp; see `--whisper-model`.

use std::collections::VecDeque;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::ws::{Message, WebSocket};
use futures::{Sink, SinkExt, Stream, StreamExt};
use grenadine_core::api::{DictationCommand, DictationEvent};
use tokio::task::JoinHandle;

/// The sample rate the page sends and Whisper takes.
pub const SAMPLE_RATE: usize = 16_000;

/// The segmenter looks at the audio in frames of 20 ms.
const FRAME: usize = SAMPLE_RATE / 50;

/// A frame is speech when its RMS is above this, or above
/// `NOISE_FACTOR` times the background noise if that's higher.
const MIN_SPEECH_RMS: f32 = 0.006;
const NOISE_FACTOR: f32 = 3.0;

/// The silence that ends a phrase.
const PAUSE_FRAMES: usize = 25;

/// A phrase is cut after this long even without a pause, so the preview
/// stays quick to compute and Whisper's 30 s window isn't exceeded.
const MAX_PHRASE: usize = 20 * SAMPLE_RATE;

/// A phrase cut for being too long is cut at the quietest of its last
/// this many frames, so the cut falls between words where it can.
const CUT_FRAMES: usize = 50;

/// The audio kept from before speech starts, so the first sound of a
/// phrase isn't clipped.
const PREROLL_FRAMES: usize = 10;

/// The background noise is the quietest frame of this many recent ones.
/// Speech dips between syllables, so the quietest frame is the background
/// even while someone speaks, and a fan or hiss that starts is picked up
/// within this long.
const NOISE_FRAMES: usize = 100;

/// Phrases with less speech than this are clicks or bumps, not words.
const MIN_SPEECH_FRAMES: usize = 5;

/// How often the phrase being spoken is transcribed for the preview, and
/// how much it must have grown since the last preview.
const INTERIM_EVERY: Duration = Duration::from_millis(1000);
const INTERIM_GROWTH: usize = SAMPLE_RATE / 4;

/// Whisper gets audio of at least this long; shorter phrases are padded
/// with silence, since very short input makes it guess.
const MIN_AUDIO: usize = SAMPLE_RATE;

/// How much of the text before a phrase goes to Whisper as its prompt.
const PROMPT_CHARS: usize = 200;

/// Cuts a stream of samples into phrases at pauses.
#[derive(Default)]
pub struct Segmenter {
    /// Samples that don't make a whole frame yet.
    partial: Vec<f32>,
    /// The frames before speech started, at most `PREROLL_FRAMES`.
    preroll: VecDeque<Vec<f32>>,
    /// The phrase being spoken, if one started.
    phrase: Option<Phrase>,
    /// The RMS of the last `NOISE_FRAMES` frames, for the background
    /// noise.
    recent: VecDeque<f32>,
}

#[derive(Default)]
struct Phrase {
    audio: Vec<f32>,
    speech_frames: usize,
    silent_frames: usize,
    /// The RMS of its last `CUT_FRAMES` frames, and whether each was
    /// speech.
    tail: VecDeque<(f32, bool)>,
}

impl Segmenter {
    /// Adds `samples` and returns the phrases they finished.
    pub fn push(&mut self, samples: &[f32]) -> Vec<Vec<f32>> {
        let mut done = Vec::new();
        self.partial.extend_from_slice(samples);
        let whole = self.partial.len() / FRAME * FRAME;
        let frames: Vec<f32> = self.partial.drain(..whole).collect();
        for frame in frames.chunks(FRAME) {
            if let Some(p) = self.frame(frame) {
                done.push(p);
            }
        }
        done
    }

    fn frame(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        let rms = (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt();
        let noise = self.recent.iter().copied().reduce(f32::min).unwrap_or(0.0);
        let speech = rms > MIN_SPEECH_RMS.max(noise * NOISE_FACTOR);
        self.recent.push_back(rms);
        if self.recent.len() > NOISE_FRAMES {
            self.recent.pop_front();
        }
        let Some(p) = &mut self.phrase else {
            if speech {
                let mut audio: Vec<f32> = self.preroll.drain(..).flatten().collect();
                audio.extend_from_slice(frame);
                self.phrase = Some(Phrase {
                    audio,
                    speech_frames: 1,
                    tail: VecDeque::from([(rms, true)]),
                    ..Phrase::default()
                });
            } else {
                self.preroll.push_back(frame.to_vec());
                if self.preroll.len() > PREROLL_FRAMES {
                    self.preroll.pop_front();
                }
            }
            return None;
        };
        p.audio.extend_from_slice(frame);
        if speech {
            p.speech_frames += 1;
            p.silent_frames = 0;
        } else {
            p.silent_frames += 1;
        }
        p.tail.push_back((rms, speech));
        if p.tail.len() > CUT_FRAMES {
            p.tail.pop_front();
        }
        if p.silent_frames >= PAUSE_FRAMES {
            return self.phrase.take().and_then(Phrase::words);
        }
        if p.audio.len() >= MAX_PHRASE {
            // Speech goes on, so what follows the cut starts the next
            // phrase right away.
            let rest = p.cut();
            return std::mem::replace(p, rest).words();
        }
        None
    }

    /// The phrase being spoken, once it has enough speech to transcribe.
    pub fn pending(&self) -> Option<&[f32]> {
        self.phrase
            .as_ref()
            .filter(|p| p.speech_frames >= MIN_SPEECH_FRAMES)
            .map(|p| p.audio.as_slice())
    }

    /// Ends the stream: the phrase being spoken is finished.
    pub fn finish(&mut self) -> Option<Vec<f32>> {
        let mut p = self.phrase.take()?;
        p.audio.append(&mut self.partial);
        p.words()
    }
}

impl Phrase {
    /// Cuts the phrase after the quietest frame of its tail and returns
    /// the frames after that as the start of the next phrase.
    fn cut(&mut self) -> Phrase {
        // The latest of the quietest, so less goes to the next phrase.
        let quietest = self
            .tail
            .iter()
            .enumerate()
            .min_by(|(i, a), (j, b)| a.0.total_cmp(&b.0).then(j.cmp(i)))
            .map_or(0, |(i, _)| i);
        let tail = self.tail.split_off(quietest + 1);
        let audio = self.audio.split_off(self.audio.len() - tail.len() * FRAME);
        let speech_frames = tail.iter().filter(|f| f.1).count();
        self.speech_frames -= speech_frames;
        Phrase {
            audio,
            speech_frames,
            silent_frames: tail.iter().rev().take_while(|f| !f.1).count(),
            tail,
        }
    }

    fn words(self) -> Option<Vec<f32>> {
        (self.speech_frames >= MIN_SPEECH_FRAMES).then_some(self.audio)
    }
}

/// Which pass a transcription is: a preview of the phrase being spoken or
/// a finished phrase's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pass {
    Interim,
    Final,
}

/// Turns audio into text. `prompt` is the text that came before.
pub trait Transcriber: Send + Sync + 'static {
    fn transcribe(&self, audio: &[f32], prompt: &str, pass: Pass) -> Result<String>;
}

#[repr(C)]
struct RawWhisper {
    _private: [u8; 0],
}

unsafe extern "C" {
    fn gr_whisper_load(path: *const c_char) -> *mut RawWhisper;
    fn gr_whisper_transcribe(
        w: *mut RawWhisper,
        samples: *const f32,
        n: usize,
        prompt: *const c_char,
        threads: c_int,
        quick: c_int,
    ) -> *mut c_char;
    fn gr_whisper_free_text(text: *mut c_char);
    fn gr_whisper_free(w: *mut RawWhisper);
}

/// A whisper.cpp model. It transcribes one phrase at a time.
struct Whisper {
    raw: Mutex<*mut RawWhisper>,
    threads: c_int,
}

// The pointer is only used under the mutex.
unsafe impl Send for Whisper {}
unsafe impl Sync for Whisper {}

impl Whisper {
    fn load(path: &Path) -> Result<Whisper> {
        let c_path = CString::new(path.as_os_str().as_encoded_bytes())?;
        // SAFETY: c_path is a valid C string for the call.
        let raw = unsafe { gr_whisper_load(c_path.as_ptr()) };
        if raw.is_null() {
            bail!("can't load the Whisper model {}", path.display());
        }
        // Whisper gains little beyond 8 threads.
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        Ok(Whisper {
            raw: Mutex::new(raw),
            threads: threads as c_int,
        })
    }
}

impl Transcriber for Whisper {
    fn transcribe(&self, audio: &[f32], prompt: &str, pass: Pass) -> Result<String> {
        let prompt = CString::new(prompt.replace('\0', ""))?;
        let raw = self.raw.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the model is valid until drop, the mutex keeps calls
        // apart, and the buffers outlive the call.
        let text = unsafe {
            gr_whisper_transcribe(
                *raw,
                audio.as_ptr(),
                audio.len(),
                prompt.as_ptr(),
                self.threads,
                c_int::from(pass == Pass::Interim),
            )
        };
        if text.is_null() {
            bail!("Whisper failed to transcribe");
        }
        // SAFETY: the shim returned a NUL-terminated string it allocated.
        let out = unsafe { CStr::from_ptr(text) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: freed once, by the shim's allocator.
        unsafe { gr_whisper_free_text(text) };
        Ok(out)
    }
}

impl Drop for Whisper {
    fn drop(&mut self) {
        let raw = *self.raw.get_mut().unwrap_or_else(|e| e.into_inner());
        // SAFETY: loaded by gr_whisper_load and freed only here.
        unsafe { gr_whisper_free(raw) };
    }
}

/// The transcribers of the finished phrases and of the previews.
#[derive(Clone)]
struct Transcribers {
    finals: Arc<dyn Transcriber>,
    previews: Arc<dyn Transcriber>,
}

/// The models to load: one for the finished phrases and maybe a smaller,
/// faster one for the previews.
struct Models {
    finals: PathBuf,
    previews: Option<PathBuf>,
}

/// The server's dictation: off, or transcribers that the models are
/// loaded into on first use.
pub struct Dictation {
    models: Option<Models>,
    transcribers: tokio::sync::OnceCell<Transcribers>,
}

impl Dictation {
    /// No dictation; pages show no Record buttons.
    pub fn off() -> Dictation {
        Dictation {
            models: None,
            transcribers: tokio::sync::OnceCell::new(),
        }
    }

    /// Dictation with the ggml Whisper model at `finals`, and the one at
    /// `previews` for the previews, or else `finals` for them too. They're
    /// loaded when the first recording starts.
    pub fn from_models(finals: PathBuf, previews: Option<PathBuf>) -> Result<Dictation> {
        for path in std::iter::once(&finals).chain(&previews) {
            if !path.is_file() {
                bail!("the Whisper model {} doesn't exist", path.display());
            }
        }
        Ok(Dictation {
            models: Some(Models { finals, previews }),
            transcribers: tokio::sync::OnceCell::new(),
        })
    }

    /// Dictation through `transcriber`, e.g. a fake one in tests.
    pub fn with(transcriber: Arc<dyn Transcriber>) -> Dictation {
        Dictation {
            models: None,
            transcribers: tokio::sync::OnceCell::new_with(Some(Transcribers {
                finals: transcriber.clone(),
                previews: transcriber,
            })),
        }
    }

    pub fn available(&self) -> bool {
        self.models.is_some() || self.transcribers.initialized()
    }

    async fn transcribers(&self) -> Result<Transcribers> {
        let load = |path: PathBuf| async move {
            tracing::info!("loading the Whisper model {}", path.display());
            let whisper = tokio::task::spawn_blocking(move || Whisper::load(&path)).await??;
            Ok::<_, anyhow::Error>(Arc::new(whisper) as Arc<dyn Transcriber>)
        };
        self.transcribers
            .get_or_try_init(|| async {
                let models = self.models.as_ref().context("dictation is off")?;
                let finals = load(models.finals.clone()).await?;
                let previews = match &models.previews {
                    Some(p) => load(p.clone()).await?,
                    None => finals.clone(),
                };
                Ok::<_, anyhow::Error>(Transcribers { finals, previews })
            })
            .await
            .cloned()
    }
}

/// A transcriber for tests. Each finished phrase reads "phrase N." with N
/// counting from 1, and each preview "hearing N".
#[derive(Default)]
pub struct FakeTranscriber {
    finals: Mutex<usize>,
}

impl Transcriber for FakeTranscriber {
    fn transcribe(&self, _audio: &[f32], _prompt: &str, pass: Pass) -> Result<String> {
        let mut finals = self.finals.lock().unwrap_or_else(|e| e.into_inner());
        Ok(match pass {
            Pass::Interim => format!("hearing {}", *finals + 1),
            Pass::Final => {
                *finals += 1;
                format!("phrase {}.", *finals)
            }
        })
    }
}

/// Whisper's text for a phrase, or `None` when it heard no words, e.g.
/// "[BLANK_AUDIO]" or "(wind blowing)".
fn clean(text: &str) -> Option<String> {
    let text = text.trim();
    let bracketed = |open, close| text.starts_with(open) && text.ends_with(close);
    if text.is_empty() || bracketed('[', ']') || bracketed('(', ')') || bracketed('*', '*') {
        return None;
    }
    Some(text.to_owned())
}

/// The last `PROMPT_CHARS` characters of `context`.
fn prompt_tail(context: &str) -> String {
    let skip = context.chars().count().saturating_sub(PROMPT_CHARS);
    context.chars().skip(skip).collect()
}

/// Converts little-endian i16 samples to floats in [-1, 1).
fn samples(bytes: &[u8]) -> Vec<f32> {
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| f32::from(i16::from_le_bytes(b)) / 32768.0)
        .collect()
}

async fn send<O>(out: &mut O, event: &DictationEvent) -> Result<()>
where
    O: Sink<Message> + Unpin,
    O::Error: std::error::Error + Send + Sync + 'static,
{
    let json = serde_json::to_string(event)?;
    out.send(Message::Text(json.into())).await?;
    Ok(())
}

/// Serves one recording on `socket`, see `DictationCommand`.
pub async fn session(dictation: &Dictation, socket: WebSocket) {
    let (mut out, mut incoming) = socket.split();
    if let Err(e) = run(dictation, &mut incoming, &mut out).await {
        tracing::warn!("dictation: {e:#}");
        let message = format!("{e:#}");
        let _ = send(&mut out, &DictationEvent::Error { message }).await;
    }
    let _ = out.send(Message::Close(None)).await;
}

/// A transcription in flight: the phrase's number and the blocking task.
struct Job {
    phrase: usize,
    task: JoinHandle<Result<String>>,
}

impl Job {
    fn start(
        t: &Arc<dyn Transcriber>,
        pass: Pass,
        phrase: usize,
        audio: &[f32],
        context: &str,
    ) -> Job {
        let mut audio = audio.to_vec();
        if audio.len() < MIN_AUDIO {
            audio.resize(MIN_AUDIO, 0.0);
        }
        let (t, prompt) = (t.clone(), prompt_tail(context));
        Job {
            phrase,
            task: tokio::task::spawn_blocking(move || t.transcribe(&audio, &prompt, pass)),
        }
    }
}

/// Waits for `job`, which mustn't be `None`, and takes it.
async fn finish(job: &mut Option<Job>) -> Result<(usize, Option<String>)> {
    let done = (&mut job.as_mut().expect("guarded").task).await;
    let phrase = job.take().expect("guarded").phrase;
    let text = done.map_err(|e| anyhow!("transcription panicked: {e}"))??;
    Ok((phrase, clean(&text)))
}

async fn run<I, E, O>(dictation: &Dictation, incoming: &mut I, out: &mut O) -> Result<()>
where
    I: Stream<Item = Result<Message, E>> + Unpin,
    O: Sink<Message> + Unpin,
    O::Error: std::error::Error + Send + Sync + 'static,
{
    let transcribers = dictation.transcribers().await?;
    // Without a preview model the previews use the finals' model, which
    // transcribes one thing at a time: a preview would hold up the finals.
    let shared = std::ptr::addr_eq(
        Arc::as_ptr(&transcribers.finals),
        Arc::as_ptr(&transcribers.previews),
    );
    let mut segmenter = Segmenter::default();
    // Finished phrases waiting for their transcription, in order, with
    // their numbers.
    let mut queue: VecDeque<(usize, Vec<f32>)> = VecDeque::new();
    // The text so far, the prompt for the next phrase.
    let mut context = String::new();
    // How many phrases were cut, i.e. the number of the one being spoken.
    let mut cut = 0;
    // The pending phrase's length at its last preview.
    let mut previewed = 0;
    // The phrase whose preview the page shows, if any.
    let mut shown = None;
    let mut preview_due = false;
    let mut stopping = false;
    // A phrase's final transcription and a preview run side by side,
    // given two models.
    let mut final_job: Option<Job> = None;
    let mut preview_job: Option<Job> = None;
    let mut ticker = tokio::time::interval(INTERIM_EVERY);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        if final_job.is_none() {
            if let Some((phrase, audio)) = queue.pop_front() {
                final_job = Some(Job::start(
                    &transcribers.finals,
                    Pass::Final,
                    phrase,
                    &audio,
                    &context,
                ));
            } else if stopping {
                send(out, &DictationEvent::Done).await?;
                return Ok(());
            }
        }
        if preview_job.is_none()
            && !stopping
            && !(shared && final_job.is_some())
            && let Some(audio) = segmenter
                .pending()
                .filter(|a| preview_due && a.len() >= previewed + INTERIM_GROWTH)
        {
            previewed = audio.len();
            preview_due = false;
            preview_job = Some(Job::start(
                &transcribers.previews,
                Pass::Interim,
                cut,
                audio,
                &context,
            ));
        }
        tokio::select! {
            msg = incoming.next() => match msg {
                Some(Ok(Message::Binary(bytes))) if !stopping => {
                    for phrase in segmenter.push(&samples(&bytes)) {
                        queue.push_back((cut, phrase));
                        cut += 1;
                        previewed = 0;
                    }
                }
                Some(Ok(Message::Text(text))) => {
                    match serde_json::from_str(&text).context("bad dictation command")? {
                        DictationCommand::Start { prompt } => context = prompt,
                        DictationCommand::Stop => {
                            stopping = true;
                            if let Some(phrase) = segmenter.finish() {
                                queue.push_back((cut, phrase));
                                cut += 1;
                            }
                        }
                    }
                }
                Some(Ok(_)) => {}
                // The page went away; nothing to send anymore.
                Some(Err(_)) | None => return Ok(()),
            },
            done = finish(&mut final_job), if final_job.is_some() => {
                let (phrase, text) = done?;
                let text = text.unwrap_or_default();
                if !text.is_empty() {
                    if !context.is_empty() && !context.ends_with(char::is_whitespace) {
                        context.push(' ');
                    }
                    context.push_str(&text);
                }
                send(out, &DictationEvent::Final { text }).await?;
                // The phrase's preview gives way to its text.
                if shown.is_some_and(|s| s <= phrase) {
                    shown = None;
                    send(out, &DictationEvent::Interim { text: String::new() }).await?;
                }
            }
            done = finish(&mut preview_job), if preview_job.is_some() => {
                let (phrase, text) = done?;
                // A preview of a phrase that was cut meanwhile is stale.
                if phrase == cut && !stopping {
                    shown = Some(phrase);
                    let text = text.unwrap_or_default();
                    send(out, &DictationEvent::Interim { text }).await?;
                }
            }
            _ = ticker.tick() => preview_due = true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ms` of a 440 Hz tone at amplitude 0.3.
    fn tone(ms: usize) -> Vec<f32> {
        (0..ms * SAMPLE_RATE / 1000)
            .map(|i| 0.3 * (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin())
            .collect()
    }

    /// `ms` of speech: the tone, with the short dips between syllables.
    fn speech(ms: usize) -> Vec<f32> {
        let mut out = tone(ms);
        for syllable in out.chunks_mut(SAMPLE_RATE / 4) {
            let dip = syllable.len().min(SAMPLE_RATE / 25);
            syllable[..dip].fill(0.0);
        }
        out
    }

    /// `ms` of a steady 1 kHz hum at amplitude 0.02, louder than
    /// `MIN_SPEECH_RMS`, like a fan or a hissing microphone.
    fn hum(ms: usize) -> Vec<f32> {
        (0..ms * SAMPLE_RATE / 1000)
            .map(|i| 0.02 * (i as f32 * 1000.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin())
            .collect()
    }

    fn mix(a: &[f32], b: &[f32]) -> Vec<f32> {
        a.iter().zip(b).map(|(a, b)| a + b).collect()
    }

    fn silence(ms: usize) -> Vec<f32> {
        vec![0.0; ms * SAMPLE_RATE / 1000]
    }

    fn ms(samples: usize) -> usize {
        samples * 1000 / SAMPLE_RATE
    }

    #[test]
    fn cuts_phrases_at_pauses() {
        let mut s = Segmenter::default();
        assert!(s.push(&silence(1000)).is_empty());
        assert!(s.push(&tone(800)).is_empty());
        assert!(s.pending().is_some());
        // A short gap doesn't end the phrase.
        assert!(s.push(&silence(200)).is_empty());
        assert!(s.push(&tone(400)).is_empty());
        let done = s.push(&silence(600));
        assert_eq!(done.len(), 1);
        // The preroll, both tones, the gap and the pause up to the cut.
        assert_eq!(ms(done[0].len()), 200 + 800 + 200 + 400 + 500);
        assert!(s.pending().is_none());
        assert!(s.push(&tone(300)).is_empty());
        assert_eq!(s.push(&silence(500)).len(), 1);
    }

    #[test]
    fn ignores_clicks() {
        let mut s = Segmenter::default();
        s.push(&silence(500));
        s.push(&tone(60));
        assert!(s.pending().is_none());
        assert!(s.push(&silence(600)).is_empty());
        assert!(s.finish().is_none());
    }

    #[test]
    fn cuts_long_phrases() {
        let mut s = Segmenter::default();
        let done = s.push(&speech(25_000));
        assert_eq!(done.len(), 1);
        // At the last dip between syllables before 20 s.
        let at = done[0].len();
        assert!(
            at <= MAX_PHRASE && at > MAX_PHRASE - CUT_FRAMES * FRAME,
            "{at}"
        );
        assert!((at - FRAME) % (SAMPLE_RATE / 4) < SAMPLE_RATE / 25, "{at}");
        // The rest goes on as the next phrase.
        let rest = s.finish().unwrap();
        assert_eq!(at + rest.len(), 25 * SAMPLE_RATE);
    }

    #[test]
    fn finds_pauses_in_steady_noise() {
        let mut s = Segmenter::default();
        let mut phrases = 0;
        phrases += s.push(&hum(1000)).len();
        for _ in 0..3 {
            phrases += s.push(&mix(&hum(800), &speech(800))).len();
            phrases += s.push(&hum(600)).len();
        }
        assert_eq!(phrases, 3);
        assert!(s.pending().is_none());
    }

    #[test]
    fn finds_pauses_after_noise_starts() {
        let mut s = Segmenter::default();
        s.push(&silence(1000));
        // The hum starts while someone speaks.
        s.push(&speech(800));
        let mut phrases = 0;
        for _ in 0..4 {
            phrases += s.push(&mix(&hum(800), &speech(800))).len();
            phrases += s.push(&hum(2500)).len();
        }
        assert!(phrases >= 3, "{phrases} phrases");
    }

    #[test]
    fn finish_keeps_the_phrase_being_spoken() {
        let mut s = Segmenter::default();
        // Not a whole number of frames.
        s.push(&speech(1_005));
        let rest = s.finish().unwrap();
        assert_eq!(rest.len(), 1_005 * SAMPLE_RATE / 1000);
        assert!(s.finish().is_none());
    }

    #[test]
    fn cleans_non_speech() {
        assert_eq!(clean(" Hello there. "), Some("Hello there.".into()));
        assert_eq!(clean("[BLANK_AUDIO]"), None);
        assert_eq!(clean(" (wind blowing)"), None);
        assert_eq!(clean("*sigh*"), None);
        assert_eq!(clean("  "), None);
    }

    #[test]
    fn prompt_is_the_tail() {
        let long = "é".repeat(PROMPT_CHARS + 5);
        assert_eq!(prompt_tail(&long).chars().count(), PROMPT_CHARS);
        assert_eq!(prompt_tail("short"), "short");
    }

    #[test]
    fn decodes_little_endian_samples() {
        assert_eq!(
            samples(&[0x00, 0x80, 0xff, 0x7f, 0x00, 0x00]),
            vec![-1.0, 32767.0 / 32768.0, 0.0]
        );
    }

    fn pcm(samples: &[f32]) -> Message {
        let bytes: Vec<u8> = samples
            .iter()
            .flat_map(|s| ((s * 32767.0) as i16).to_le_bytes())
            .collect();
        Message::Binary(bytes.into())
    }

    fn command(c: &DictationCommand) -> Message {
        Message::Text(serde_json::to_string(c).unwrap().into())
    }

    /// The protocol, with the fake transcriber: a preview of the phrase
    /// being spoken, each phrase's text once it's cut, and the last
    /// phrase after Stop.
    #[tokio::test]
    async fn session_sends_previews_and_phrases() {
        let dictation = Dictation::with(Arc::new(FakeTranscriber::default()));
        let (to_server, from_page) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (to_page, mut from_server) = futures::channel::mpsc::unbounded::<Message>();
        let mut incoming = tokio_stream::wrappers::UnboundedReceiverStream::new(from_page)
            .map(Ok::<_, std::convert::Infallible>);
        let mut out = to_page;
        let server = async move { run(&dictation, &mut incoming, &mut out).await };
        let page = async move {
            let mut next = async || -> DictationEvent {
                match from_server.next().await.unwrap() {
                    Message::Text(t) => serde_json::from_str(&t).unwrap(),
                    m => panic!("unexpected {m:?}"),
                }
            };
            let start = DictationCommand::Start {
                prompt: "Before".into(),
            };
            to_server.send(command(&start)).unwrap();
            to_server.send(pcm(&silence(300))).unwrap();
            to_server.send(pcm(&tone(600))).unwrap();
            assert_eq!(
                next().await,
                DictationEvent::Interim {
                    text: "hearing 1".into()
                }
            );
            to_server.send(pcm(&silence(600))).unwrap();
            assert_eq!(
                next().await,
                DictationEvent::Final {
                    text: "phrase 1.".into()
                }
            );
            // The preview of the phrase gives way to its text.
            assert_eq!(
                next().await,
                DictationEvent::Interim {
                    text: String::new()
                }
            );
            to_server.send(pcm(&tone(300))).unwrap();
            to_server.send(command(&DictationCommand::Stop)).unwrap();
            let mut rest = Vec::new();
            loop {
                match next().await {
                    DictationEvent::Interim { .. } => {}
                    DictationEvent::Done => break,
                    e => rest.push(e),
                }
            }
            assert_eq!(
                rest,
                vec![DictationEvent::Final {
                    text: "phrase 2.".into()
                }]
            );
        };
        let (served, ()) = tokio::join!(server, page);
        served.unwrap();
    }

    #[tokio::test]
    async fn session_without_a_model_fails() {
        let dictation = Dictation::off();
        let mut incoming = futures::stream::empty::<Result<Message, std::convert::Infallible>>();
        let (mut out, _rx) = futures::channel::mpsc::unbounded::<Message>();
        assert!(run(&dictation, &mut incoming, &mut out).await.is_err());
    }

    /// Transcribes a 16 kHz mono WAV with a real model, phrase by phrase
    /// as a recording would be. Run by hand with
    /// `--test_env=WHISPER_MODEL=... --test_env=WHISPER_WAV=...
    /// --test_arg=--ignored --test_arg=real_model --test_output=all`.
    #[test]
    #[ignore]
    fn real_model() {
        let model = std::env::var("WHISPER_MODEL").unwrap();
        let wav = std::fs::read(std::env::var("WHISPER_WAV").unwrap()).unwrap();
        let start = std::time::Instant::now();
        let whisper = Whisper::load(Path::new(&model)).unwrap();
        println!("loaded in {:?}", start.elapsed());
        let mut s = Segmenter::default();
        let mut phrases = s.push(&samples(&wav[44..]));
        phrases.extend(s.finish());
        let mut context = String::new();
        for p in phrases {
            let start = std::time::Instant::now();
            let text = whisper
                .transcribe(&p, &prompt_tail(&context), Pass::Final)
                .unwrap();
            println!(
                "{} ms of audio in {:?}: {text}",
                ms(p.len()),
                start.elapsed()
            );
            context.push_str(&text);
        }
    }

    /// Streams a 16 kHz mono WAV through a session in real time with real
    /// models and prints when each message arrives. Run by hand with
    /// `--test_env=WHISPER_MODEL=... --test_env=WHISPER_PREVIEW_MODEL=...
    /// --test_env=WHISPER_WAV=... --test_arg=--ignored
    /// --test_arg=real_session --test_arg=--nocapture`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn real_session() {
        let var = |v| std::env::var(v).unwrap();
        let dictation = Dictation::from_models(
            var("WHISPER_MODEL").into(),
            std::env::var("WHISPER_PREVIEW_MODEL").ok().map(Into::into),
        )
        .unwrap();
        dictation.transcribers().await.unwrap();
        let wav = std::fs::read(var("WHISPER_WAV")).unwrap();
        let audio = samples(&wav[44..]);
        let (to_server, from_page) = tokio::sync::mpsc::unbounded_channel::<Message>();
        let (mut out, mut from_server) = futures::channel::mpsc::unbounded::<Message>();
        let mut incoming = tokio_stream::wrappers::UnboundedReceiverStream::new(from_page)
            .map(Ok::<_, std::convert::Infallible>);
        let start = std::time::Instant::now();
        let page = async move {
            let start_cmd = DictationCommand::Start {
                prompt: String::new(),
            };
            to_server.send(command(&start_cmd)).unwrap();
            for chunk in audio.chunks(SAMPLE_RATE / 20) {
                to_server.send(pcm(chunk)).unwrap();
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            println!("{:>6.2}s stop", start.elapsed().as_secs_f32());
            to_server.send(command(&DictationCommand::Stop)).unwrap();
            // Like a page, it stays connected until Done.
            to_server
        };
        let print = async move {
            while let Some(Message::Text(t)) = from_server.next().await {
                println!("{:>6.2}s {t}", start.elapsed().as_secs_f32());
            }
        };
        let server = async move {
            run(&dictation, &mut incoming, &mut out).await.unwrap();
        };
        let (_page, (), ()) = tokio::join!(page, print, server);
    }
}
