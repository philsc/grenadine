//! Images that go with a prompt: pasted into its text box, dropped on its
//! form or picked from a file, shown as thumbnails until the prompt is
//! sent. They're kept with the prompt's draft in IndexedDB (see
//! `image_drafts`), so they survive leaving the form and reloads. Files
//! that aren't images Claude takes go to `files` instead.

use grenadine_core::api::PromptImage;

use crate::files::Files;
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

/// The longest edge Claude looks at; bigger images are scaled down to it
/// before they're sent, which keeps them small.
const MAX_EDGE: f64 = 1568.0;

/// How big an image's base64 may get as a PNG before it goes as a JPEG
/// instead. The Claude API takes images up to 5 MB.
const MAX_PNG_BASE64: usize = 4 * 1024 * 1024;

/// Reads `file` into an image Claude takes: scaled down to `MAX_EDGE` and
/// encoded as a PNG, or as a JPEG when it's a photo or too big as a PNG.
async fn read(file: web_sys::File) -> Result<PromptImage, String> {
    let fail = |e: JsValue| format!("can't read {}: {e:?}", file.name());
    let window = web_sys::window().ok_or("no window")?;
    let bitmap: web_sys::ImageBitmap =
        JsFuture::from(window.create_image_bitmap_with_blob(&file).map_err(fail)?)
            .await
            .map_err(|_| format!("{} isn't an image the browser can read", file.name()))?
            .unchecked_into();
    let (width, height) = (f64::from(bitmap.width()), f64::from(bitmap.height()));
    let scale = (MAX_EDGE / width.max(height)).min(1.0);
    let (width, height) = (
        (width * scale).round().max(1.0),
        (height * scale).round().max(1.0),
    );
    let canvas: web_sys::HtmlCanvasElement = document()
        .create_element("canvas")
        .map_err(fail)?
        .unchecked_into();
    canvas.set_width(width as u32);
    canvas.set_height(height as u32);
    let context: web_sys::CanvasRenderingContext2d = canvas
        .get_context("2d")
        .map_err(fail)?
        .ok_or("no 2D canvas")?
        .unchecked_into();
    context
        .draw_image_with_image_bitmap_and_dw_and_dh(&bitmap, 0.0, 0.0, width, height)
        .map_err(fail)?;
    bitmap.close();
    let mut url = String::new();
    if file.type_() != "image/jpeg" {
        url = canvas.to_data_url_with_type("image/png").map_err(fail)?;
    }
    if url.is_empty() || url.len() > MAX_PNG_BASE64 {
        url = canvas
            .to_data_url_with_type_and_encoder_options("image/jpeg", &0.9.into())
            .map_err(fail)?;
    }
    let (media_type, data) = url
        .strip_prefix("data:")
        .and_then(|u| u.split_once(";base64,"))
        .ok_or("the canvas made no data URL")?;
    Ok(PromptImage {
        media_type: media_type.to_owned(),
        data: data.to_owned(),
    })
}

/// The images Claude takes in a prompt. Other files, including images of
/// other kinds such as SVGs, go as files the agent reads.
const INLINE_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Whether `file` goes in the prompt as an image.
fn inline(file: &web_sys::File) -> bool {
    INLINE_TYPES.contains(&file.type_().as_str())
}

/// The files in `files`.
fn list(files: Option<web_sys::FileList>) -> Vec<web_sys::File> {
    let Some(files) = files else {
        return Vec::new();
    };
    (0..files.length()).filter_map(|i| files.get(i)).collect()
}

/// What goes with a prompt: its images and its other files.
#[derive(Clone, Copy)]
pub struct Attached {
    pub images: RwSignal<Vec<PromptImage>>,
    pub files: Files,
}

impl Attached {
    pub fn is_empty(self) -> bool {
        self.images.with(Vec::is_empty) && self.files.is_empty()
    }

    /// Adds `files`: images are read into `images`, or attached as files
    /// when the browser can't read them; the rest are attached as files.
    fn add(self, files: Vec<web_sys::File>) {
        let (images, others): (Vec<_>, Vec<_>) = files.into_iter().partition(inline);
        self.files.add(others);
        for file in images {
            spawn_local(async move {
                match read(file.clone()).await {
                    Ok(image) => self.images.update(|i| i.push(image)),
                    Err(_) => self.files.add(vec![file]),
                }
            });
        }
    }
}

/// Takes the files pasted into a text box. Copying from a spreadsheet or
/// a document can put a picture of the text on the clipboard too; then
/// the text is what's pasted.
pub fn paste(ev: web_sys::ClipboardEvent, attached: Attached) {
    let Some(data) = ev.clipboard_data() else {
        return;
    };
    let files = list(data.files());
    let text = data.get_data("text/plain").is_ok_and(|t| !t.is_empty());
    if files.is_empty() || (text && files.iter().all(inline)) {
        return;
    }
    ev.prevent_default();
    attached.add(files);
}

/// Lets files be dropped on a form.
pub fn drag_over(ev: web_sys::DragEvent) {
    let files = ev
        .data_transfer()
        .is_some_and(|d| d.types().includes(&"Files".into(), 0));
    if files {
        ev.prevent_default();
    }
}

/// Takes the files dropped on a form.
pub fn drop(ev: web_sys::DragEvent, attached: Attached) {
    let files = list(ev.data_transfer().and_then(|d| d.files()));
    if !files.is_empty() {
        ev.prevent_default();
        attached.add(files);
    }
}

/// The images that go with a prompt, each with a button that takes it off.
/// Nothing while there are none.
#[component]
pub fn Attachments(images: RwSignal<Vec<PromptImage>>) -> impl IntoView {
    view! {
        <div class="prompt-images" hidden=move || images.with(Vec::is_empty)>
            {move || {
                images
                    .get()
                    .into_iter()
                    .enumerate()
                    .map(|(i, image)| {
                        let src = format!("data:{};base64,{}", image.media_type, image.data);
                        view! {
                            <div class="prompt-image">
                                <img src=src alt="Image for the prompt" />
                                <button
                                    type="button"
                                    title="Remove the image"
                                    aria-label="Remove the image"
                                    on:click=move |_| images.update(|list| { list.remove(i); })
                                >
                                    "×"
                                </button>
                            </div>
                        }
                    })
                    .collect_view()
            }}
        </div>
    }
}

/// A button that attaches files picked from the disk.
#[component]
pub fn AttachButton(attached: Attached) -> impl IntoView {
    let input = NodeRef::<leptos::html::Input>::new();
    let picked = move |_| {
        let Some(input) = input.get() else { return };
        attached.add(list(input.files()));
        // Picking the same file again is a change too.
        input.set_value("");
    };
    view! {
        <label class="attach-image" title="Attach images or other files; you can also paste or drop them">
            "Attach files"
            <input node_ref=input type="file" multiple on:change=picked />
        </label>
    }
}
