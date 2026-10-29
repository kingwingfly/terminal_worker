//! Live sound through Cloudflare Realtime (owner's choice, 2026-10-28). A speaker's page sends
//! its microphone, as it records it (`stream FILE`), to the Realtime SFU over WebRTC, as the
//! track `voice` of a Realtime session of its own; /dev/snd/NAME keeps that session (devices.rs).
//! A listener's page gets a session of its own that pulls the track. Pages never see the app's
//! secret: the authority speaks to Realtime for them, once it has decided that they may, the
//! speaker as the account making the recording, a listener as one who may read it.
//!
//! The app is the site's: REALTIME_APP_ID and the secret REALTIME_APP_TOKEN. Without them, a
//! recording is only recorded.
use crate::api::{self, Reply};
use crate::{auth, devices};
use serde::Deserialize;
use serde_json::{Value, json};
use worker::*;

/// Realtime's API.
const REALTIME: &str = "https://rtc.live.cloudflare.com/v1";
/// The track a speaker sends.
const TRACK: &str = "voice";
/// The longest description of a session (SDP) taken, in bytes.
const SDP_MAX: u64 = 64 * 1024;
/// A listener's Realtime session, as `listen` made it: the deadline (kind) by which its page
/// answers it, once, and the seconds it has to.
const LISTENER: &str = "listener";
const ANSWER_TTL: u64 = 60;

/// The site's Realtime app: its id and secret.
fn app(env: &Env) -> std::result::Result<(String, String), String> {
    let text = |name: &str| {
        env.var(name)
            .map(|v| v.to_string())
            .or_else(|_| env.secret(name).map(|v| v.to_string()))
            .ok()
            .filter(|value| !value.is_empty())
    };
    match (text("REALTIME_APP_ID"), text("REALTIME_APP_TOKEN")) {
        (Some(id), Some(token)) => Ok((id, token)),
        _ => Err("Live sound needs the site's Cloudflare Realtime app (REALTIME_APP_ID and REALTIME_APP_TOKEN); this one has none".into()),
    }
}

/// Ask Realtime, at `path` of the app, with `body`; its answer, or why not.
async fn realtime(
    env: &Env,
    method: Method,
    path: &str,
    body: Option<Value>,
) -> std::result::Result<Value, String> {
    let (id, token) = app(env)?;
    let failed = |e: Error| format!("Realtime could not be reached: {e}");
    let headers = Headers::new();
    headers
        .set("Authorization", &format!("Bearer {token}"))
        .map_err(failed)?;
    let mut init = RequestInit::new();
    init.with_method(method);
    if let Some(body) = body {
        headers
            .set("Content-Type", "application/json")
            .map_err(failed)?;
        init.with_body(Some(body.to_string().into()));
    }
    init.with_headers(headers);
    let url = format!("{REALTIME}/apps/{id}/{path}");
    let request = Request::new_with_init(&url, &init).map_err(failed)?;
    let mut response = Fetch::Request(request).send().await.map_err(failed)?;
    let value: Value = response.json().await.unwrap_or_default();
    // A track refused is said in its entry, the request's other errors at the top.
    let refused = value["tracks"]
        .as_array()
        .and_then(|tracks| tracks.iter().find(|track| track["errorCode"].is_string()))
        .unwrap_or(&value);
    if response.status_code() >= 400 || refused["errorCode"].is_string() {
        let why = refused["errorDescription"]
            .as_str()
            .or(refused["errorCode"].as_str())
            .map_or_else(
                || format!("status {}", response.status_code()),
                str::to_string,
            );
        return Err(format!("Realtime refused: {why}"));
    }
    Ok(value)
}

/// A new Realtime session's id.
async fn session(env: &Env) -> std::result::Result<String, String> {
    let made = realtime(env, Method::Post, "sessions/new", None).await?;
    made["sessionId"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| "Realtime made no session".into())
}

/// A page's session description: the SDP of its offer or its answer, and the speaker's track.
#[derive(Deserialize)]
struct Described {
    sdp: String,
    #[serde(default)]
    mid: Option<String>,
}
async fn described(req: &mut Request) -> Option<Described> {
    let length = req
        .headers()
        .get("Content-Length")
        .ok()??
        .parse::<u64>()
        .ok()?;
    if length == 0 || length > SDP_MAX {
        return None;
    }
    req.json::<Described>().await.ok()
}

/// Send the speaker's microphone live as /dev/snd/`name`: as the recording at `path` is made,
/// or kept nowhere, given the ACL `entries` (stream -m). The page offers its track; Realtime
/// answers.
pub async fn speak(
    req: &mut Request,
    env: &Env,
    state: &State,
    key: Option<String>,
    path: Option<String>,
    name: Option<String>,
    entries: Option<String>,
) -> Result<Reply> {
    let Some(offer) = described(req).await else {
        return Ok(Reply::error("Invalid sound request", 400));
    };
    let Some(mid) = offer.mid.clone() else {
        return Ok(Reply::error("Invalid sound request: no track", 400));
    };
    if let Err(why) = app(env) {
        return Ok(Reply::error(&why, 503));
    }
    // Each makes a Realtime session, as a listener's does.
    if auth::limited(req, env, "speak").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    // Asked first, so that Realtime is not asked for one who may not.
    let asked = (key.clone(), path.clone());
    let device = api::run(state, move |db| {
        let (key, path) = asked;
        let who = auth::identity(db, key.as_deref());
        match devices::sound_name(db, &who, path.as_deref(), name.as_deref()) {
            Ok(device) => Reply::ok(json!({ "device": device })),
            Err(message) => api::denied(&message, &who),
        }
    })?;
    if device.status >= 400 {
        return Ok(device);
    }
    let device = device.body["device"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let sent = async {
        let session = session(env).await?;
        let tracks = json!({
            "sessionDescription": { "type": "offer", "sdp": offer.sdp },
            "tracks": [{ "location": "local", "mid": mid, "trackName": TRACK }],
        });
        let answered = realtime(
            env,
            Method::Post,
            &format!("sessions/{session}/tracks/new"),
            Some(tracks),
        )
        .await?;
        let sdp = answered["sessionDescription"]["sdp"]
            .as_str()
            .ok_or("Realtime gave no answer")?
            .to_string();
        Ok::<_, String>((session, sdp))
    }
    .await;
    let (session, sdp) = match sent {
        Ok(sent) => sent,
        Err(why) => return Ok(Reply::error(&why, 502)),
    };
    let media = json!({ "session": session, "track": TRACK }).to_string();
    api::run(state, move |db| {
        let who = auth::identity(db, key.as_deref());
        let made = devices::start_sound(
            db,
            &who,
            path.as_deref(),
            &device,
            &media,
            entries.as_deref(),
        );
        match made {
            Ok(made) => {
                Reply::ok(json!({ "device": device, "sdp": sdp, "version": made["version"] }))
            }
            Err(message) => api::denied(&message, &who),
        }
    })
}

/// Hear the live sound at `device` (/dev/snd/NAME): Realtime offers a session that pulls its
/// track, which the page answers (`answer`). Rate-limited, as each makes a Realtime session.
pub async fn listen(
    req: &mut Request,
    env: &Env,
    state: &State,
    key: Option<String>,
    device: String,
) -> Result<Reply> {
    if let Err(why) = app(env) {
        return Ok(Reply::error(&why, 503));
    }
    // A limit of its own: a room of listeners may come from one address, venue Wi-Fi's.
    if auth::limited_by(req, env, "LISTEN_LIMIT", "listen").await? {
        return Ok(Reply::error(
            "Too many attempts; please try again later",
            429,
        ));
    }
    let heard = api::run(state, move |db| {
        let who = auth::identity(db, key.as_deref());
        let media = devices::followed(db, &who, &device).and_then(|device| {
            let version = devices::version(&device);
            let media = device
                .media
                .ok_or_else(|| format!("{}: not a sound", device.name))?;
            Ok((media, version))
        });
        match media {
            Ok((media, version)) => {
                let mut heard: Value = serde_json::from_str(&media).unwrap_or_default();
                heard["version"] = json!(version);
                Reply::ok(heard)
            }
            Err(message) => api::denied(&message, &who),
        }
    })?;
    if heard.status >= 400 {
        return Ok(heard);
    }
    let speaker = heard.body["session"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let track = heard.body["track"].as_str().unwrap_or(TRACK).to_string();
    // What the device is (devices::version), which tells a page whose line was down whether it
    // was replaced meanwhile.
    let version = heard.body["version"].clone();
    let offered = async {
        let session = session(env).await?;
        let tracks = json!({
            "tracks": [{ "location": "remote", "sessionId": speaker, "trackName": track }],
        });
        let offered = realtime(
            env,
            Method::Post,
            &format!("sessions/{session}/tracks/new"),
            Some(tracks),
        )
        .await?;
        let sdp = offered["sessionDescription"]["sdp"]
            .as_str()
            .ok_or("Realtime offered nothing")?
            .to_string();
        Ok::<_, String>((session, sdp))
    }
    .await;
    match offered {
        // Only the page that asked answers it (`answer`).
        Ok((session, sdp)) => api::run(state, move |db| {
            db.set_expiry(LISTENER, &session, auth::now() + ANSWER_TTL);
            Reply::ok(json!({ "session": session, "sdp": sdp, "version": version }))
        }),
        Err(why) => Ok(Reply::error(&why, 502)),
    }
}

/// A listener's answer to what Realtime offered for `session`, a session `listen` made: once,
/// and soon after.
pub async fn answer(req: &mut Request, env: &Env, state: &State, session: String) -> Result<Reply> {
    if session.is_empty()
        || session.len() > 128
        || !session
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Ok(Reply::error("Invalid sound session", 400));
    }
    let Some(answer) = described(req).await else {
        return Ok(Reply::error("Invalid sound request", 400));
    };
    let offered = session.clone();
    let issued = api::run(state, move |db| {
        let fresh = db
            .expiry(LISTENER, &offered)
            .is_some_and(|due| auth::now() <= due);
        db.clear_expiry(LISTENER, &offered);
        if fresh {
            Reply::ok(json!({}))
        } else {
            Reply::error("No such sound session; listen again", 404)
        }
    })?;
    if issued.status >= 400 {
        return Ok(issued);
    }
    let renegotiated = realtime(
        env,
        Method::Put,
        &format!("sessions/{session}/renegotiate"),
        Some(json!({ "sessionDescription": { "type": "answer", "sdp": answer.sdp } })),
    )
    .await;
    Ok(match renegotiated {
        Ok(_) => Reply::ok(json!({ "session": session })),
        Err(why) => Reply::error(&why, 502),
    })
}
