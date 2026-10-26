//! File bytes, which go between the client and R2 in the Worker, outside the authority: it only
//! says whether a request may read or write them, and records what was written.
use crate::api::chunk_key;
use crate::streams::{concatenate, fixed_length, object_range};
use serde::Deserialize;
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;
use terminal_worker_core::{MAX_UPLOAD, byte_range, mime};
use wasm_bindgen::prelude::*;
use worker::wasm_bindgen_futures::future_to_promise;
use worker::*;

/// How long a reader waits for a chunk still uploading, without any new chunk arriving.
const STALL: Duration = Duration::from_secs(60);

pub fn error_response(message: &str, status: u16) -> Result<Response> {
    Ok(Response::from_json(&json!({"error":message}))?.with_status(status))
}

/// Ask the authority, as the client whose session cookie is `cookie`.
async fn authority_as(
    env: &Env,
    cookie: Option<&str>,
    method: Method,
    path_and_query: &str,
    body: Option<Value>,
) -> Result<Response> {
    let headers = Headers::new();
    if let Some(cookie) = cookie {
        headers.set("Cookie", cookie)?;
    }
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        headers.set("Content-Type", "application/json")?;
        init.with_body(Some(JsValue::from_str(&body.to_string())));
    }
    init.with_headers(headers);
    let url = format!("https://authority{path_and_query}");
    env.durable_object("AUTHORITY")?
        .id_from_name("filesystem")?
        .get_stub()?
        .fetch_with_request(Request::new_with_init(&url, &init)?)
        .await
}
/// Ask the authority, as the client of `req`.
async fn authority(
    env: &Env,
    req: &Request,
    method: Method,
    path_and_query: &str,
    body: Option<Value>,
) -> Result<Response> {
    let cookie = req.headers().get("Cookie")?;
    authority_as(env, cookie.as_deref(), method, path_and_query, body).await
}

fn param(req: &Request, name: &str) -> Result<String> {
    Ok(req
        .url()?
        .query_pairs()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default())
}
fn encode(value: &str) -> String {
    js_sys::encode_uri_component(value)
        .as_string()
        .unwrap_or_default()
}
fn content_length(req: &Request) -> Result<Option<u64>> {
    Ok(req
        .headers()
        .get("Content-Length")?
        .and_then(|s| s.parse::<u64>().ok()))
}

/// What the authority lets a reader read.
#[derive(Deserialize)]
struct Grant {
    file: String,
    size: u64,
    chunk_size: u64,
    state: String,
    etag: String,
    mime: String,
    /// Committed chunks, as (index, R2 key).
    chunks: Vec<(u64, String)>,
}

/// GET or HEAD a file: authorization precedes every HEAD, Range and conditional response,
/// including 304. A file still uploading is read as it arrives: the response waits for each
/// chunk not uploaded yet.
pub async fn read(req: Request, env: &Env) -> Result<Response> {
    let query = format!("path={}", encode(&param(&req, "path")?));
    let mut grant = authority(
        env,
        &req,
        Method::Get,
        &format!("/internal/read?{query}"),
        None,
    )
    .await?;
    if grant.status_code() >= 400 {
        return Ok(grant);
    }
    let grant: Grant = grant.json().await?;
    let headers = Headers::new();
    headers.set("Content-Type", &grant.mime)?;
    headers.set("Accept-Ranges", "bytes")?;
    headers.set("ETag", &grant.etag)?;
    headers.set("Content-Security-Policy", "default-src 'none'; sandbox")?;
    if grant.state != "complete" {
        headers.set("X-Upload-State", "uploading")?;
    }
    if req.headers().get("If-None-Match")?.as_deref() == Some(&grant.etag) {
        return Ok(Response::empty()?.with_status(304).with_headers(headers));
    }
    let size = grant.size;
    let range = req.headers().get("Range")?;
    let if_range = req.headers().get("If-Range")?;
    let range = range.filter(|_| if_range.as_ref().is_none_or(|v| v == &grant.etag));
    let (start, end, status) = if let Some(r) = range {
        let Some((a, b)) = byte_range(&r, size) else {
            headers.set("Content-Range", &format!("bytes */{size}"))?;
            return Ok(Response::empty()?.with_status(416).with_headers(headers));
        };
        headers.set("Content-Range", &format!("bytes {a}-{b}/{size}"))?;
        (a, b, 206)
    } else {
        (0, size.saturating_sub(1), 200)
    };
    let length = if size == 0 { 0 } else { end - start + 1 };
    headers.set("Content-Length", &length.to_string())?;
    if req.method() == Method::Head || length == 0 {
        return Ok(Response::empty()?.with_status(status).with_headers(headers));
    }
    let reader = Reader {
        bucket: env.bucket("FILES")?,
        env: env.clone(),
        cookie: req.headers().get("Cookie")?,
        query,
        complete: grant.state == "complete",
        chunks: grant.chunks.iter().cloned().collect(),
        file: grant.file.clone(),
    };
    let body = reader.stream(grant.chunk_size, start, end);
    Ok(Response::from_body(ResponseBody::Stream(body))?
        .with_status(status)
        .with_headers(headers))
}

/// The chunks of one file, for one reader, who may wait for chunks still uploading.
struct Reader {
    bucket: Bucket,
    env: Env,
    cookie: Option<String>,
    /// The file's path, as the authority's query.
    query: String,
    file: String,
    complete: bool,
    /// Committed chunks: index to R2 key.
    chunks: HashMap<u64, String>,
}
impl Reader {
    /// Bytes `start..=end`, read chunk by chunk as the client consumes them, as a stream of
    /// known length, so that responses keep their Content-Length.
    fn stream(self, chunk_size: u64, start: u64, end: u64) -> web_sys::ReadableStream {
        let mut parts = VecDeque::new();
        for index in start / chunk_size..=end / chunk_size {
            let chunk_start = index * chunk_size;
            let from = start.max(chunk_start) - chunk_start;
            let to = end.min(chunk_start + chunk_size - 1) - chunk_start;
            parts.push_back((index, from, to - from + 1));
        }
        let parts = Rc::new(RefCell::new(parts));
        let reader = Rc::new(RefCell::new(Some(self)));
        let next = Closure::<dyn FnMut() -> js_sys::Promise>::new(move || {
            let part = parts.borrow_mut().pop_front();
            let reader = reader.clone();
            future_to_promise(async move {
                let Some((index, offset, length)) = part else {
                    return Ok(JsValue::NULL);
                };
                // The streams are read one after another, so the reader is never shared.
                let mut this = reader
                    .borrow_mut()
                    .take()
                    .ok_or_else(|| JsValue::from_str("chunks were read out of order"))?;
                let key = this.key(index).await;
                let bucket = this.bucket.clone();
                *reader.borrow_mut() = Some(this);
                object_range(&bucket, &key?, offset, length)
                    .await
                    .map(Into::into)
            })
        });
        let stream = concatenate(&next.into_js_value().into());
        fixed_length(&stream, (end - start + 1) as f64)
    }
    /// The R2 key of chunk `index`, waiting for it while the file uploads.
    async fn key(&mut self, index: u64) -> std::result::Result<String, JsValue> {
        let mut pause = Duration::from_millis(500);
        let mut stalled = Duration::ZERO;
        loop {
            if let Some(key) = self.chunks.get(&index) {
                return Ok(key.clone());
            }
            if self.complete {
                return Err(JsValue::from_str("File content does not exist"));
            }
            if stalled >= STALL {
                return Err(JsValue::from_str("The upload stalled; try again later"));
            }
            Delay::from(pause).await;
            stalled += pause;
            let known = self.chunks.len();
            self.refresh()
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            if self.chunks.len() > known {
                stalled = Duration::ZERO;
                pause = Duration::from_millis(500);
            } else {
                pause = (pause * 2).min(Duration::from_secs(4));
            }
        }
    }
    /// Ask the authority again which chunks are committed.
    async fn refresh(&mut self) -> Result<()> {
        let mut grant = authority_as(
            &self.env,
            self.cookie.as_deref(),
            Method::Get,
            &format!("/internal/read?{}", self.query),
            None,
        )
        .await?;
        if grant.status_code() >= 400 {
            return Err(Error::RustError(
                "File was removed while it was read".into(),
            ));
        }
        let grant: Grant = grant.json().await?;
        if grant.file != self.file {
            return Err(Error::RustError(
                "File was replaced while it was read".into(),
            ));
        }
        self.complete = grant.state == "complete";
        self.chunks = grant.chunks.into_iter().collect();
        Ok(())
    }
}

/// Stream the request's body, of `length` bytes, to the R2 object `key`.
async fn store(
    env: &Env,
    req: &Request,
    key: &str,
    length: u64,
    content_type: &str,
    sha256: Option<Vec<u8>>,
) -> Result<()> {
    let body = req
        .inner()
        .body()
        .ok_or_else(|| Error::RustError("Upload has no body".into()))?;
    let bucket = env.bucket("FILES")?;
    let mut put = bucket
        .put(
            key,
            Data::ReadableStream(fixed_length(&body, length as f64)),
        )
        .http_metadata(HttpMetadata {
            content_type: Some(content_type.into()),
            ..Default::default()
        });
    if let Some(sha256) = sha256 {
        put = put.sha256(sha256);
    }
    put.execute().await.map(|_| ())
}

/// PUT a whole file in one request: the authority opens an upload, the body streams to R2 as
/// the file's only chunk, and the authority completes the upload.
pub async fn write(req: Request, env: &Env) -> Result<Response> {
    let path = param(&req, "path")?;
    let Some(length) = content_length(&req)? else {
        return error_response("Upload must include a content length", 411);
    };
    if length > MAX_UPLOAD {
        return error_response("Maximum file size is 100 MB", 413);
    }
    let overwrite = param(&req, "overwrite")? == "true";
    let mut begin = authority(
        env,
        &req,
        Method::Post,
        &format!("/internal/upload/begin?path={}", encode(&path)),
        Some(json!({
            "size": length, "chunk_size": length.max(1), "overwrite": overwrite,
            "if_match": req.headers().get("If-Match")?,
            "if_none_match": req.headers().get("If-None-Match")?,
        })),
    )
    .await?;
    if begin.status_code() >= 400 {
        return Ok(begin);
    }
    let id = begin.json::<Value>().await?["id"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let key = chunk_key(&id, 0);
    let index = if length == 0 {
        None
    } else {
        if let Err(error) = store(env, &req, &key, length, mime(&path), None).await {
            console_error!("upload failed: {}", error);
            authority(
                env,
                &req,
                Method::Post,
                "/internal/upload/abort",
                Some(json!({"id":id})),
            )
            .await?;
            return error_response("Upload failed or was interrupted", 400);
        }
        Some(0)
    };
    authority(
        env,
        &req,
        Method::Post,
        "/internal/upload/commit",
        Some(json!({"id":id,"index":index,"key":key,"complete":true})),
    )
    .await
}

/// PUT chunk `index` of upload `id`, with its SHA-256 in `X-Chunk-SHA256`, which R2 checks.
/// Sending a chunk again is harmless: the copy committed first stays, so an interrupted upload
/// resumes by sending what is missing.
pub async fn chunk(req: Request, env: &Env) -> Result<Response> {
    let id = param(&req, "id")?;
    let index = param(&req, "index")?;
    let Some(length) = content_length(&req)? else {
        return error_response("Upload must include a content length", 411);
    };
    let sha256 = req.headers().get("X-Chunk-SHA256")?;
    let digest = match &sha256 {
        Some(hex) => match decode_hex(hex) {
            Some(bytes) if bytes.len() == 32 => Some(bytes),
            _ => return error_response("X-Chunk-SHA256 must be 64 hexadecimal digits", 400),
        },
        None => None,
    };
    let mut grant = authority(
        env,
        &req,
        Method::Get,
        &format!(
            "/internal/upload/chunk?id={}&index={}&length={length}",
            encode(&id),
            encode(&index)
        ),
        None,
    )
    .await?;
    if grant.status_code() >= 400 {
        return Ok(grant);
    }
    let grant: Value = grant.json().await?;
    let key = grant["key"].as_str().unwrap_or_default();
    let content_type = grant["mime"].as_str().unwrap_or_default();
    if let Err(error) = store(env, &req, key, length, content_type, digest).await {
        console_error!("chunk upload failed: {}", error);
        return error_response(
            "Chunk upload failed, or the chunk does not match its SHA-256; send it again",
            400,
        );
    }
    authority(
        env,
        &req,
        Method::Post,
        "/internal/upload/commit",
        Some(json!({
            "id": id, "index": index.parse::<u64>().ok(), "key": key, "sha256": sha256,
            "complete": false,
        })),
    )
    .await
}

pub fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}
