//! The previews in the viewer dialog. web/app.js opens the dialog, hands its content to [`show`]
//! through `window.terminalPreview.show(content, path, kind, version)`, and later calls what that
//! returns, to release the preview: its views, its workers' documents and its listeners. The
//! manual renders through `window.terminalPreview.markdown(text, path)`.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gloo_net::http::{Method, RequestBuilder};
use leptos::{ev, html, mount::mount_to, prelude::*, task::spawn_local};
use terminal_worker_preview::language;
use wasm_bindgen::{JsCast, JsValue, closure::Closure, prelude::wasm_bindgen};
use wasm_bindgen_futures::{JsFuture, future_to_promise};
use web_sys::{
    Blob, BlobPropertyBag, CanvasRenderingContext2d, Element, HtmlCanvasElement, HtmlElement,
    ResizeObserver, ResizeObserverEntry, Url,
};

use crate::preview::{self, ARCHIVE_MAX, DiffLine, Kind, TEXT_MAX, file_path, file_url};
use crate::search::Searchable;
use crate::workers;

#[wasm_bindgen]
extern "C" {
    /// Open `path` in the viewer, from a link in the file it shows, which Back returns to.
    #[wasm_bindgen(js_namespace = terminal, js_name = follow)]
    fn follow_link(path: &str, kind: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_namespace = terminal, js_name = place)]
    fn saved_place(path: &str) -> JsValue;
    #[wasm_bindgen(js_namespace = terminal, js_name = setPlace)]
    fn save_place(path: &str, value: JsValue);
}

/// Where the preview of `path` was left, as web/app.js keeps it: `{"slide": [column, row]}`,
/// `{"page": n, "zoom": z}`, and so on; null if nowhere.
fn place(path: &str) -> serde_json::Value {
    js_sys::JSON::stringify(&saved_place(path))
        .ok()
        .and_then(|text| text.as_string())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}
/// Keep `value`, merged with what is kept, as where the preview of `path` is.
fn set_place(path: &str, value: serde_json::Value) {
    if let Ok(value) = js_sys::JSON::parse(&value.to_string()) {
        save_place(path, value);
    }
}

/// The most bytes of a PDF opened, as many as an upload has.
const PDF_MAX: usize = 100_000_000;
/// The size slides are laid out at, as reveal.js laid them out before; they scale to the space
/// they have, in the dialog or full screen.
const SLIDE_SIZE: (f64, f64) = (1280.0, 720.0);

/// A JavaScript error as text.
pub fn js_error(error: JsValue) -> String {
    match error.dyn_ref::<js_sys::Error>() {
        Some(error) => String::from(error.message()),
        None => error.as_string().unwrap_or_else(|| format!("{error:?}")),
    }
}

/// Make [`show`] and [`preview::markdown`] web/app.js's `window.terminalPreview`.
pub fn register() {
    let show = Closure::<dyn Fn(HtmlElement, String, String, String) -> js_sys::Promise>::new(
        |content, path, kind, version| {
            future_to_promise(async move {
                show(content, path, kind, version)
                    .await
                    .map_err(|error| js_sys::Error::new(&error).into())
            })
        },
    );
    let markdown =
        Closure::<dyn Fn(String, String) -> String>::new(|text: String, path: String| {
            preview::markdown(&text, &path)
        });
    let api = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&api, &"show".into(), show.as_ref());
    let _ = js_sys::Reflect::set(&api, &"markdown".into(), markdown.as_ref());
    show.forget();
    markdown.forget();
    let _ = js_sys::Reflect::set(&window(), &"terminalPreview".into(), &api);
}

/// What a preview shows once its file is read.
enum Preview {
    /// An image, video or audio of the kind the terminal lists, which the browser streams.
    Media(String, String),
    Slides(String),
    File(Kind, Vec<u8>),
}

/// Show the file at `path`, which the terminal lists as `listed`, in `content`, unless the viewer
/// went on to another file while it was read, as `content`'s `data-version` then is no longer
/// `version`. Returns what releases it.
async fn show(
    content: HtmlElement,
    path: String,
    listed: String,
    version: String,
) -> Result<JsValue, String> {
    let url = file_url(&path);
    let size = head(&url).await?;
    let preview = match listed.as_str() {
        "image" | "video" | "audio" => Preview::Media(listed, url),
        _ => {
            let archive = [".zip", ".tar", ".tar.gz", ".tgz"]
                .iter()
                .any(|extension| path.to_lowercase().ends_with(extension));
            let limit = match listed.as_str() {
                "pdf" => PDF_MAX,
                _ if archive => ARCHIVE_MAX,
                _ => TEXT_MAX,
            };
            if size > limit {
                return Err(if limit == TEXT_MAX {
                    "Text exceeds 2 MB; use the download button in the top right.".into()
                } else {
                    "The file is too large to preview; use the download button in the top right."
                        .into()
                });
            }
            let data = get(&url).await?;
            match listed.as_str() {
                "slides" => Preview::Slides(String::from_utf8_lossy(&data).into_owned()),
                "pdf" => Preview::File(Kind::Pdf, data),
                _ => Preview::File(preview::kind(&path, &data), data),
            }
        }
    };
    if content.get_attribute("data-version").as_deref() != Some(version.as_str()) {
        return Ok(Closure::once_into_js(|| {}));
    }
    content.set_text_content(None);
    let handle = mount_to(content, move || render(preview, path));
    Ok(Closure::once_into_js(move || drop(handle)))
}

/// The size of the file at `url`, which the viewer may read.
async fn head(url: &str) -> Result<usize, String> {
    let response = respond(RequestBuilder::new(url).method(Method::HEAD)).await?;
    Ok(response
        .headers()
        .get("Content-Length")
        .and_then(|length| length.parse().ok())
        .unwrap_or(0))
}

/// The bytes of the file at `url`.
async fn get(url: &str) -> Result<Vec<u8>, String> {
    respond(RequestBuilder::new(url))
        .await?
        .binary()
        .await
        .map_err(|e| e.to_string())
}

/// The response to `request`, or the error the terminal gave.
async fn respond(request: RequestBuilder) -> Result<gloo_net::http::Response, String> {
    let response = request.send().await.map_err(|e| e.to_string())?;
    if response.ok() {
        return Ok(response);
    }
    let error = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|value| value["error"].as_str().map(String::from));
    Err(error.unwrap_or_else(|| format!("Request failed ({})", response.status())))
}

fn render(preview: Preview, path: String) -> AnyView {
    match preview {
        Preview::Media(kind, url) => media(&kind, url, &path),
        Preview::Slides(source) => view! { <Slides source path /> }.into_any(),
        Preview::File(kind, data) => match kind {
            Kind::Image(mime) => match image_url(&data, mime) {
                Some(url) => media("image", url, &path),
                None => no_preview(),
            },
            Kind::Video(mime) => match blob_url(&data, mime) {
                Some(url) => media("video", url, &path),
                None => no_preview(),
            },
            Kind::Audio(mime) => match blob_url(&data, mime) {
                Some(url) => media("audio", url, &path),
                None => no_preview(),
            },
            Kind::Pdf => view! { <PdfView data=Arc::new(data) path /> }.into_any(),
            Kind::Markdown { html, source } => {
                view! { <MarkdownView html source path /> }.into_any()
            }
            Kind::Code { text, language } => view! {
                <Searchable>
                    <Enhanced>
                        <pre class="text-preview"><code data-lang=language>{text}</code></pre>
                    </Enhanced>
                </Searchable>
            }
            .into_any(),
            Kind::Table(table) => table_view(table),
            Kind::Archive => view! { <ArchiveView data /> }.into_any(),
            Kind::Cards(cards) => cards_view(cards),
            Kind::Diff(lines) => {
                let diff = diff_view(lines);
                view! { <Searchable>{diff}</Searchable> }.into_any()
            }
            Kind::None => no_preview(),
        },
    }
}

fn no_preview() -> AnyView {
    view! {
        <div class="media-frame">
            <p class="no-preview">"No preview for this kind of file. Download it to open it."</p>
        </div>
    }
    .into_any()
}

/// An image, video or audio from `url`, the file at `path`.
fn media(kind: &str, url: String, path: &str) -> AnyView {
    let failed = RwSignal::new(false);
    let element = match kind {
        "image" => view! { <img src=url alt=path.to_string() on:error=move |_| failed.set(true) /> }
            .into_any(),
        "video" => view! {
            <video src=url controls preload="metadata" playsinline on:error=move |_| failed.set(true)></video>
        }
        .into_any(),
        _ => view! {
            <audio src=url controls preload="metadata" on:error=move |_| failed.set(true)></audio>
        }
        .into_any(),
    };
    view! {
        <div class="media-frame">
            {element}
            {move || {
                failed.get().then(|| view! { <p>"This browser cannot decode the media. Download it to open locally."</p> })
            }}
        </div>
    }
    .into_any()
}

/// A URL that shows the image `data`, of the MIME type `mime`: a blob URL of this origin, but for
/// SVG, which would open from one as a page of this origin, running its scripts; a `data:` URL
/// opens as no page, and as an image, it runs none.
fn image_url(data: &[u8], mime: &str) -> Option<String> {
    if mime == "image/svg+xml" {
        let text = String::from_utf8_lossy(data);
        Some(format!(
            "data:{mime};charset=utf-8,{}",
            js_sys::encode_uri_component(&text)
        ))
    } else {
        blob_url(data, mime)
    }
}

/// A blob URL of this origin for `data`, typed `mime`, revoked when the preview goes.
fn blob_url(data: &[u8], mime: &str) -> Option<String> {
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(data));
    let options = BlobPropertyBag::new();
    options.set_type(mime);
    let url = Blob::new_with_u8_array_sequence_and_options(&parts, &options)
        .and_then(|blob| Url::create_object_url_with_blob(&blob))
        .ok()?;
    let revoked = url.clone();
    on_cleanup(move || {
        let _ = Url::revoke_object_url(&revoked);
    });
    Some(url)
}

/// What it shows, once shown, with its code blocks highlighted and its diagrams drawn; its links
/// to files open them in the viewer.
#[component]
fn Enhanced(children: Children) -> impl IntoView {
    let node = NodeRef::<html::Div>::new();
    Effect::new(move || {
        if let Some(element) = node.get() {
            enhance(&element);
        }
    });
    view! {
        <div class="enhanced" node_ref=node on:click=follow>
            {children()}
        </div>
    }
}

/// Open the file a clicked link names in the viewer, instead of leaving for it.
fn follow(event: ev::MouseEvent) {
    let Some(link) = event
        .target()
        .and_then(|target| target.dyn_into::<Element>().ok())
        .and_then(|target| target.closest("a[href]").ok().flatten())
    else {
        return;
    };
    let Some(path) = link.get_attribute("href").and_then(|href| file_path(&href)) else {
        return;
    };
    event.prevent_default();
    let kind = terminal_worker_core::kind(&path);
    spawn_local(async move {
        let _ = JsFuture::from(follow_link(&path, kind)).await;
    });
}

/// The elements in `element` that `selector` selects.
fn select(element: &Element, selector: &str) -> Vec<Element> {
    let Ok(nodes) = element.query_selector_all(selector) else {
        return Vec::new();
    };
    (0..nodes.length())
        .filter_map(|i| nodes.item(i)?.dyn_into().ok())
        .collect()
}

/// Make the web links in `element` open in a tab of their own; highlight its code blocks,
/// `pre > code[data-lang]`, and draw its Mermaid ones, as images. Blocks that fail stay as they
/// are, saying why in `data-error`.
fn enhance(element: &Element) {
    for link in select(element, "a[href^='http']") {
        let _ = link.set_attribute("target", "_blank");
    }
    for code in select(element, "pre > code[data-lang]") {
        spawn_local(async move {
            let info = code.get_attribute("data-lang").unwrap_or_default();
            let text = code.text_content().unwrap_or_default();
            let shown = if info.eq_ignore_ascii_case("mermaid") {
                workers::diagram(&text).await.and_then(|svg| {
                    // A data: URL, not a blob: one of this origin, which would open as a page.
                    let image = document().create_element("img").map_err(js_error)?;
                    image.set_class_name("diagram");
                    let src = format!(
                        "data:image/svg+xml;charset=utf-8,{}",
                        js_sys::encode_uri_component(&svg)
                    );
                    let _ = image.set_attribute("alt", "diagram");
                    let _ = image.set_attribute("src", &src);
                    if let Some(block) = code.parent_element() {
                        block.replace_with_with_node_1(&image).map_err(js_error)?;
                    }
                    Ok(())
                })
            } else if let Some(language) = language::of_block(&info) {
                workers::highlight(language, &text)
                    .await
                    .map(|html| code.set_inner_html(&html))
            } else {
                Ok(())
            };
            if let (Err(e), Some(block)) = (shown, code.parent_element()) {
                let _ = block.set_attribute("data-error", &e);
            }
        });
    }
}

/// A line icon, as the terminal's file icons are drawn.
fn icon(path: &'static str) -> impl IntoView {
    view! {
        <svg class="preview-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d=path />
        </svg>
    }
}

const PREVIOUS: &str = "M15 6l-6 6 6 6";
const NEXT: &str = "M9 6l6 6-6 6";
const UP: &str = "M6 15l6-6 6 6";
const DOWN: &str = "M6 9l6 6 6-6";
const AS_SLIDES: &str = "M3 4h18M4 4v11a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1V4M12 16v4M8 20h8";
const AS_DOCUMENT: &str =
    "M14 3H7a2 2 0 0 0-2 2v14a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8zM14 3v5h5M9 13h6M9 17h6";

/// A Markdown file, as a document, or as slides.
#[component]
fn MarkdownView(html: String, source: String, path: String) -> impl IntoView {
    let as_slides = RwSignal::new(place(&path)["slides"] == true);
    let kept = path.clone();
    Effect::new(move || set_place(&kept, serde_json::json!({ "slides": as_slides.get() })));
    let label = move || {
        if as_slides.get() {
            "View as a document"
        } else {
            "View as slides"
        }
    };
    view! {
        <div class="preview-bar">
            <button
                class="preview-button"
                title=label
                aria-label=label
                on:click=move |_| as_slides.update(|slides| *slides = !*slides)
            >
                {move || icon(if as_slides.get() { AS_DOCUMENT } else { AS_SLIDES })}
            </button>
        </div>
        {move || {
            if as_slides.get() {
                view! { <Slides source=source.clone() path=path.clone() /> }.into_any()
            } else {
                let html = html.clone();
                view! {
                    <Searchable>
                        <Enhanced>
                            <article class="markdown" inner_html=html></article>
                        </Enhanced>
                    </Searchable>
                }
                .into_any()
            }
        }}
    }
}

/// The looks slides can take, for every deck: each a value of `.slides[data-theme]`, and its name.
const SLIDE_THEMES: [(&str, &str); 4] = [
    ("light", "Light"),
    ("dark", "Dark"),
    ("sepia", "Sepia"),
    ("night", "Night"),
];
const SLIDE_THEME: &str = "terminal-slide-theme";
/// The slide theme chosen last in this browser, or Light.
fn slide_theme() -> String {
    window()
        .local_storage()
        .ok()
        .flatten()
        .and_then(|storage| storage.get_item(SLIDE_THEME).ok().flatten())
        .filter(|theme| SLIDE_THEMES.iter().any(|(value, _)| value == theme))
        .unwrap_or_else(|| "light".into())
}
fn set_slide_theme(theme: &str) {
    if let Some(storage) = window().local_storage().ok().flatten() {
        let _ = storage.set_item(SLIDE_THEME, theme);
    }
}

/// Whether keys are the slides', rather than another dialog's or a text field's.
fn keys_for_slides() -> bool {
    let document = document();
    let typing = document.active_element().is_some_and(|element| {
        matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")
            || element.has_attribute("contenteditable")
    });
    let elsewhere = document
        .query_selector("dialog#auth[open], dialog#editor[open]")
        .ok()
        .flatten()
        .is_some();
    !typing && !elsewhere
}

/// Slides, as reveal.js lays them out: the arrow keys go to the next column, left and right, and
/// to the next slide in it, up and down; F shows the viewer full screen. They scale to the space
/// they have, as reveal.js scales them.
#[component]
fn Slides(source: String, path: String) -> impl IntoView {
    let deck = StoredValue::new(preview::slides(&source, &path));
    // The slide it was left at, if the deck still has it.
    let left = place(&path)["slide"]
        .as_array()
        .and_then(|at| {
            Some((
                at.first()?.as_u64()? as usize,
                at.get(1)?.as_u64()? as usize,
            ))
        })
        .filter(|&(column, row)| {
            deck.with_value(|deck| deck.get(column).is_some_and(|rows| row < rows.len()))
        });
    let position = RwSignal::new(left.unwrap_or((0, 0)));
    Effect::new(move || {
        let (column, row) = position.get();
        set_place(&path, serde_json::json!({ "slide": [column, row] }));
    });
    let theme = RwSignal::new(slide_theme());
    Effect::new(move || set_slide_theme(&theme.get()));
    let node = NodeRef::<html::Div>::new();
    let go = move |right: isize, down: isize| {
        position.update(|(column, row)| {
            deck.with_value(|deck| {
                let last = |len: usize| len.saturating_sub(1) as isize;
                if right != 0 {
                    let next = (*column as isize + right).clamp(0, last(deck.len())) as usize;
                    if next != *column {
                        (*column, *row) = (next, 0);
                    }
                } else {
                    *row = (*row as isize + down).clamp(0, last(deck[*column].len())) as usize;
                }
            })
        })
    };
    let keys = window_event_listener(ev::keydown, move |event| {
        if !keys_for_slides() || event.ctrl_key() || event.meta_key() || event.alt_key() {
            return;
        }
        let (right, down) = match event.key().as_str() {
            "ArrowRight" | "PageDown" => (1, 0),
            "ArrowLeft" | "PageUp" => (-1, 0),
            "ArrowDown" => (0, 1),
            "ArrowUp" => (0, -1),
            "f" | "F" => {
                if let Some(button) = document().get_element_by_id("fullscreen") {
                    event.prevent_default();
                    button.unchecked_into::<HtmlElement>().click();
                }
                return;
            }
            _ => return,
        };
        event.prevent_default();
        go(right, down);
    });
    // Scaled to fit the space the slides have, as it changes.
    let fit = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
        for entry in entries.iter() {
            let entry: ResizeObserverEntry = entry.unchecked_into();
            let size = entry.content_rect();
            let target = entry.target();
            // The room the bar takes, measured: none when it floats over the slides, as it does
            // full screen.
            let bar = target
                .query_selector(".slides-bar")
                .ok()
                .flatten()
                .filter(|bar| {
                    window()
                        .get_computed_style(bar)
                        .ok()
                        .flatten()
                        .and_then(|style| style.get_property_value("position").ok())
                        .is_none_or(|position| position != "absolute")
                })
                .map_or(0.0, |bar| {
                    f64::from(bar.unchecked_into::<HtmlElement>().offset_height())
                });
            let scale = (size.width() / SLIDE_SIZE.0).min((size.height() - bar) / SLIDE_SIZE.1);
            let style = target.unchecked_into::<HtmlElement>().style();
            let _ = style.set_property("--scale", &scale.max(0.1).to_string());
        }
    });
    let observer = StoredValue::new_local(ResizeObserver::new(fit.as_ref().unchecked_ref()).ok());
    let fit = StoredValue::new_local(Some(fit));
    Effect::new(move || {
        if let (Some(slides), Some(observer)) = (node.get(), observer.get_value()) {
            observer.observe(&slides);
        }
    });
    on_cleanup(move || {
        keys.remove();
        if let Some(Some(observer)) = observer.try_get_value() {
            observer.disconnect();
        }
        let _ = fit.try_update_value(Option::take);
    });
    let can = move |right: isize, down: isize| {
        let (column, row) = position.get();
        deck.with_value(|deck| match (right, down) {
            (-1, _) => column > 0,
            (1, _) => column + 1 < deck.len(),
            (_, -1) => row > 0,
            _ => row + 1 < deck[column].len(),
        })
    };
    let slide = move || {
        let (column, row) = position.get();
        deck.with_value(|deck| deck[column][row].clone())
    };
    let counter = move || {
        let (column, row) = position.get();
        deck.with_value(|deck| match deck[column].len() {
            1 => format!("{} / {}", column + 1, deck.len()),
            rows => format!("{} / {} · {} / {rows}", column + 1, deck.len(), row + 1),
        })
    };
    let button = move |label: &'static str, path: &'static str, right: isize, down: isize| {
        view! {
            <button
                class="preview-button"
                title=label
                aria-label=label
                disabled=move || !can(right, down)
                on:click=move |_| go(right, down)
            >
                {icon(path)}
            </button>
        }
    };
    view! {
        <div class="slides" data-theme=move || theme.get() node_ref=node>
            {move || {
                // Read here, where it is tracked: a component's children are built untracked.
                let html = slide();
                let class = if preview::image_slide(&html) {
                    "slide markdown slide-image"
                } else {
                    "slide markdown"
                };
                view! {
                    <Enhanced>
                        <div class=class inner_html=html></div>
                    </Enhanced>
                }
            }}
            <div class="slides-bar">
                {button("Previous slide", PREVIOUS, -1, 0)}
                {button("Slide above", UP, 0, -1)}
                <span role="status">{counter}</span>
                {button("Slide below", DOWN, 0, 1)}
                {button("Next slide", NEXT, 1, 0)}
                <select class="slides-theme" aria-label="Slide theme" prop:value=move || theme.get() on:change=move |e| theme.set(event_target_value(&e))>
                    {SLIDE_THEMES.iter().map(|(value, label)| view! { <option value=*value>{*label}</option> }).collect_view()}
                </select>
            </div>
        </div>
    }
}

/// The most pixels wide a PDF page is drawn: past that, the memory pages take outgrows what is
/// seen of them.
const PDF_PIXELS: f64 = 2000.0;
/// How far the view zooms out and in, from the stage's width.
const PDF_ZOOM: (f64, f64) = (0.5, 3.0);

/// A PDF as one document that scrolls. Every page has its place, at its size, from the start;
/// a worker draws the pages near the view, one at a time, and those far from it are let go,
/// so that a long document takes the memory of a few pages. The toolbar says which page is in
/// view, goes to the one before or after it, and zooms.
#[component]
fn PdfView(data: Arc<Vec<u8>>, path: String) -> impl IntoView {
    let pages = RwSignal::new(Vec::<(f64, f64)>::new());
    let failure = RwSignal::new(None::<String>);
    // Where it was left: its page and zoom.
    let left = place(&path);
    let current = RwSignal::new(0usize);
    let left_at = left["page"].as_u64().unwrap_or(0) as usize;
    let zoom = RwSignal::new(
        left["zoom"]
            .as_f64()
            .unwrap_or(1.0)
            .clamp(PDF_ZOOM.0, PDF_ZOOM.1),
    );
    let document = StoredValue::new(None::<workers::Pdf>);
    let stage = NodeRef::<html::Div>::new();
    // The pages near the view, and the width in pixels each one is drawn at.
    let near = StoredValue::new(BTreeSet::<usize>::new());
    let drawn = StoredValue::new(BTreeMap::<usize, u32>::new());
    let drawing = StoredValue::new(false);
    spawn_local(async move {
        match workers::open_pdf(&data).await {
            Ok((pdf, sizes)) => {
                if document.try_set_value(Some(pdf)).is_some() {
                    workers::close_pdf(pdf);
                    return;
                }
                pages.try_set(sizes);
            }
            Err(e) => {
                failure.try_set(Some(format!("Could not open PDF: {e}")));
            }
        }
    });
    let frame = move |index: usize| -> Option<HtmlElement> {
        stage
            .get_untracked()?
            .query_selector(&format!(".pdf-frame[data-index=\"{index}\"]"))
            .ok()
            .flatten()
            .map(JsCast::unchecked_into)
    };
    let canvas = move |index: usize| -> Option<HtmlCanvasElement> {
        frame(index)?
            .query_selector("canvas")
            .ok()
            .flatten()
            .map(JsCast::unchecked_into)
    };
    let release = move |index: usize| {
        if let Some(canvas) = canvas(index) {
            canvas.set_width(0);
            canvas.set_height(0);
        }
        drawn.update_value(|drawn| {
            drawn.remove(&index);
        });
    };
    // Draw what is near and not drawn at its width, nearest the page in view first, until
    // nothing is left; one drawing at a time, as the pages come and go meanwhile.
    let draw = move || {
        if drawing.get_value() {
            return;
        }
        drawing.set_value(true);
        spawn_local(async move {
            let ratio = window().device_pixel_ratio().min(2.0);
            while let Some(pdf) = document.try_get_value().flatten() {
                let wanted = near.with_value(|near| {
                    near.iter()
                        .filter_map(|&index| {
                            let width = frame(index)?.client_width();
                            let pixels = (f64::from(width) * ratio).min(PDF_PIXELS).round() as u32;
                            (pixels > 0 && drawn.with_value(|d| d.get(&index) != Some(&pixels)))
                                .then_some((index, pixels))
                        })
                        .min_by_key(|(index, _)| index.abs_diff(current.get_untracked()))
                });
                let Some((index, pixels)) = wanted else {
                    break;
                };
                let image = workers::render_pdf(pdf, index, pixels).await;
                if !near
                    .try_with_value(|near| near.contains(&index))
                    .unwrap_or(false)
                {
                    continue;
                }
                match image.and_then(|image| paint(canvas(index), &image)) {
                    Ok(()) => drawn.update_value(|drawn| {
                        drawn.insert(index, pixels);
                    }),
                    Err(e) => {
                        failure.set(Some(format!("PDF rendering failed: {e}")));
                        break;
                    }
                }
            }
            let _ = drawing.try_set_value(false);
        });
    };
    // Which pages are near the view, within a screen of it.
    let seen = Closure::<dyn FnMut(js_sys::Array)>::new(move |entries: js_sys::Array| {
        for entry in entries.iter() {
            let entry: web_sys::IntersectionObserverEntry = entry.unchecked_into();
            let Some(index) = entry
                .target()
                .get_attribute("data-index")
                .and_then(|index| index.parse::<usize>().ok())
            else {
                continue;
            };
            if entry.is_intersecting() {
                near.update_value(|near| {
                    near.insert(index);
                });
            } else {
                near.update_value(|near| {
                    near.remove(&index);
                });
                release(index);
            }
        }
        draw();
    });
    let seen = StoredValue::new_local(Some(seen));
    let observer = StoredValue::new_local(None::<web_sys::IntersectionObserver>);
    Effect::new(move || {
        let count = pages.with(Vec::len);
        let Some(stage) = stage.get() else {
            return;
        };
        if count == 0 || observer.with_value(Option::is_some) {
            return;
        }
        let options = web_sys::IntersectionObserverInit::new();
        options.set_root(Some(&stage));
        options.set_root_margin("100% 0px");
        let Some(Ok(watching)) = seen.with_value(|seen| {
            seen.as_ref().map(|seen| {
                web_sys::IntersectionObserver::new_with_options(
                    seen.as_ref().unchecked_ref(),
                    &options,
                )
            })
        }) else {
            return;
        };
        if let Ok(frames) = stage.query_selector_all(".pdf-frame") {
            for index in 0..frames.length() {
                if let Some(frame) = frames.item(index) {
                    watching.observe(frame.unchecked_ref());
                }
            }
        }
        observer.set_value(Some(watching));
        let _ = stage.focus();
        // Back to the page it was left at, once the pages are laid out.
        let page = left_at.min(count - 1);
        if page > 0 {
            request_animation_frame(move || {
                if let Some(frame) = frame(page) {
                    frame.scroll_into_view();
                    current.set(page);
                }
            });
        }
    });
    Effect::new(move || {
        let (page, level) = (current.get(), zoom.get());
        if pages.with(|pages| !pages.is_empty()) {
            set_place(&path, serde_json::json!({ "page": page, "zoom": level }));
        }
    });
    // A new zoom draws the pages in view at their new width, and keeps the page in view.
    Effect::new(move |previous: Option<f64>| {
        let level = zoom.get();
        if previous.is_some_and(|previous| previous != level) {
            if let Some(frame) = frame(current.get_untracked()) {
                frame.scroll_into_view();
            }
            draw();
        }
        level
    });
    on_cleanup(move || {
        if let Some(Some(observer)) = observer.try_get_value() {
            observer.disconnect();
        }
        let _ = seen.try_update_value(Option::take);
        if let Some(Some(pdf)) = document.try_get_value() {
            workers::close_pdf(pdf);
        }
    });
    // The page in view: the last one whose top is above a third of the stage.
    let scrolled = move |_| {
        let Some(stage) = stage.get_untracked() else {
            return;
        };
        let line = stage.scroll_top() + stage.client_height() / 3;
        let count = pages.with_untracked(Vec::len);
        let page = (0..count)
            .take_while(|&index| frame(index).is_some_and(|frame| frame.offset_top() <= line))
            .last()
            .unwrap_or(0);
        if current.get_untracked() != page {
            current.set(page);
        }
    };
    let go = move |page: usize| {
        if let Some(frame) = frame(page) {
            frame.scroll_into_view();
            current.set(page);
        }
    };
    let count = move || pages.with(Vec::len);
    let zoom_by = move |factor: f64| {
        zoom.update(|zoom| *zoom = (*zoom * factor).clamp(PDF_ZOOM.0, PDF_ZOOM.1));
    };
    view! {
        <div class="pdf-view">
            <div class="pdf-toolbar">
                <button
                    aria-label="Previous page"
                    disabled=move || current.get() == 0
                    on:click=move |_| go(current.get_untracked().saturating_sub(1))
                >
                    "↑"
                </button>
                <span role="status">
                    {move || {
                        failure
                            .get()
                            .unwrap_or_else(|| match count() {
                                0 => "Loading PDF...".into(),
                                count => format!("{} / {count}", current.get() + 1),
                            })
                    }}
                </span>
                <button
                    aria-label="Next page"
                    disabled=move || { current.get() + 1 >= count() }
                    on:click=move |_| go(current.get_untracked() + 1)
                >
                    "↓"
                </button>
                <span class="pdf-zoom">
                    <button aria-label="Zoom out" disabled=move || { zoom.get() <= PDF_ZOOM.0 } on:click=move |_| zoom_by(1.0 / 1.25)>"−"</button>
                    <button aria-label="Fit width" on:click=move |_| zoom.set(1.0)>{move || format!("{:.0}%", zoom.get() * 100.0)}</button>
                    <button aria-label="Zoom in" disabled=move || { zoom.get() >= PDF_ZOOM.1 } on:click=move |_| zoom_by(1.25)>"+"</button>
                </span>
            </div>
            <div
                class="pdf-stage"
                tabindex="0"
                node_ref=stage
                style=move || format!("--pdf-zoom:{}", zoom.get())
                on:scroll=scrolled
            >
                {move || {
                    pages
                        .get()
                        .into_iter()
                        .enumerate()
                        .map(|(index, (width, height))| {
                            view! {
                                <div class="pdf-frame" data-index=index style=format!("aspect-ratio:{width}/{height}")>
                                    <canvas class="pdf-page" aria-label=format!("PDF page {}", index + 1)></canvas>
                                </div>
                            }
                        })
                        .collect_view()
                }}
            </div>
        </div>
    }
}

/// Paint `image`, a page as the worker drew it, into `canvas`, which the page's frame sizes.
fn paint(canvas: Option<HtmlCanvasElement>, image: &web_sys::ImageData) -> Result<(), String> {
    let canvas = canvas.ok_or("the page is gone")?;
    canvas.set_width(image.width());
    canvas.set_height(image.height());
    let context: CanvasRenderingContext2d = canvas
        .get_context("2d")
        .map_err(js_error)?
        .ok_or("no 2D context")?
        .unchecked_into();
    context.put_image_data(image, 0.0, 0.0).map_err(js_error)
}

/// `bytes`, in the largest unit they make at least one of.
fn size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.1} KB", bytes as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

fn table_view(table: preview::Table) -> AnyView {
    let mut rows = table.rows.into_iter();
    let header = rows.next().unwrap_or_default();
    view! {
        <div class="table-preview">
            <table>
                <thead>
                    <tr>{header.into_iter().map(|cell| view! { <th>{cell}</th> }).collect_view()}</tr>
                </thead>
                <tbody>
                    {rows
                        .map(|row| {
                            view! {
                                <tr>{row.into_iter().map(|cell| view! { <td>{cell}</td> }).collect_view()}</tr>
                            }
                        })
                        .collect_view()}
                </tbody>
            </table>
            {table
                .more
                .then(|| {
                    view! {
                        <p class="preview-note">
                            {format!("Only the first {} rows show.", preview::ROWS_MAX)}
                        </p>
                    }
                })}
        </div>
    }
    .into_any()
}

/// The members of the archive `data`, as its worker lists them.
#[component]
fn ArchiveView(data: Vec<u8>) -> impl IntoView {
    let listed = RwSignal::new(None::<Result<(Vec<preview::Entry>, bool), String>>);
    spawn_local(async move {
        let result = workers::list_archive(&data).await;
        listed.try_set(Some(result));
    });
    move || match listed.get() {
        None => view! { <p class="preview-note">"Listing the archive..."</p> }.into_any(),
        Some(Ok((entries, more))) => archive_view(entries, more),
        Some(Err(e)) => view! {
            <div class="media-frame">
                <p class="no-preview">{format!("Could not list the archive: {e}")}</p>
            </div>
        }
        .into_any(),
    }
}

/// `entries`, and whether the archive has more, which do not show.
fn archive_view(entries: Vec<preview::Entry>, more: bool) -> AnyView {
    let files = entries.iter().filter(|entry| !entry.directory).count();
    let total: u64 = entries.iter().map(|entry| entry.size).sum();
    let first = if more { "The first " } else { "" };
    view! {
        <div class="archive">
            <p class="preview-note">{format!("{first}{files} files, {} unpacked", size(total))}</p>
            <ul>
                {entries
                    .into_iter()
                    .map(|entry| {
                        let length = if entry.directory { String::new() } else { size(entry.size) };
                        view! {
                            <li class:directory=entry.directory>
                                <span>{entry.path}</span>
                                <span>{length}</span>
                            </li>
                        }
                    })
                    .collect_view()}
            </ul>
        </div>
    }
    .into_any()
}

fn cards_view(cards: Vec<preview::Card>) -> AnyView {
    if cards.is_empty() {
        return no_preview();
    }
    view! {
        <div class="cards">
            {cards
                .into_iter()
                .map(|card| {
                    let title = if card.title.is_empty() { "Untitled".to_string() } else { card.title };
                    view! {
                        <section class="card">
                            <h3>{title}</h3>
                            <dl>
                                {card
                                    .fields
                                    .into_iter()
                                    .map(|(label, value)| view! { <dt>{label}</dt><dd>{value}</dd> })
                                    .collect_view()}
                            </dl>
                        </section>
                    }
                })
                .collect_view()}
        </div>
    }
    .into_any()
}

fn diff_view(lines: Vec<(DiffLine, String)>) -> AnyView {
    view! {
        <pre class="text-preview diff">
            {lines
                .into_iter()
                .map(|(kind, line)| {
                    let class = match kind {
                        DiffLine::File => "diff-file",
                        DiffLine::Hunk => "diff-hunk",
                        DiffLine::Added => "diff-added",
                        DiffLine::Removed => "diff-removed",
                        DiffLine::Context => "diff-context",
                    };
                    view! { <span class=class>{line}</span> }
                })
                .collect_view()}
        </pre>
    }
    .into_any()
}
