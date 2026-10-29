mod api;
mod auth;
mod commands;
mod db;
mod devices;
mod files;
mod jobs;
mod runner;
mod sound;
mod store;
mod streams;
mod sys;
use files::error_response;
pub use runner::JobRunner;
pub use store::TerminalAuthority;
use worker::*;

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let path = req.url()?.path().to_string();
    if !path.starts_with("/api/") {
        return env.assets("ASSETS")?.fetch_request(req).await;
    }
    // The page's line for changes: a WebSocket, which carries the session cookie, so only this
    // site may open one; its reply goes back as the authority made it.
    if path == "/api/events" {
        if req.headers().get("Origin")?.as_deref()
            != Some(req.url()?.origin().ascii_serialization().as_str())
        {
            return error_response("Change events must originate from this site", 403);
        }
        return env
            .durable_object("AUTHORITY")?
            .id_from_name("filesystem")?
            .get_stub()?
            .fetch_with_request(req)
            .await;
    }
    let result = async {
        if !matches!(req.method(), Method::Get | Method::Head)
            && req.headers().get("Origin")?.as_deref()
                != Some(req.url()?.origin().ascii_serialization().as_str())
        {
            return error_response("Write requests must originate from this site", 403);
        }
        // File bytes stream here, outside the authority, which only decides and records.
        match (req.method(), path.as_str()) {
            (Method::Get | Method::Head, "/api/file") => files::read(req, &env).await,
            (Method::Put, "/api/file") => files::write(req, &env).await,
            (Method::Put, "/api/uploads/chunk") => files::chunk(req, &env).await,
            _ => {
                env.durable_object("AUTHORITY")?
                    .id_from_name("filesystem")?
                    .get_stub()?
                    .fetch_with_request(req)
                    .await
            }
        }
    }
    .await;
    let mut response = match result {
        Ok(r) => r,
        Err(e) => {
            console_error!("request failed: {}", e);
            error_response("Service temporarily unavailable; please try again", 500)?
        }
    };
    let headers = Headers::new();
    for (name, value) in response.headers().entries() {
        headers.append(&name, &value)?;
    }
    response = response.with_headers(headers);
    response
        .headers_mut()
        .set("Cache-Control", "private, no-store")?;
    response.headers_mut().set("Vary", "Cookie")?;
    response
        .headers_mut()
        .set("X-Content-Type-Options", "nosniff")?;
    Ok(response)
}
