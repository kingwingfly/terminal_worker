//! System calls from the page (owner's design, 2026-10-29: the page is user space): the batch
//! endpoint (`POST /api/sys`), and file bytes, read as pread reads them (`GET /api/file`). The
//! programs of user space (crates/userland) run over this.
use abi::{Batch, Call, Errno, Error, Results, Return};
use serde_json::Value;

/// A process of this page: under sudo or not, with its umask.
#[derive(Clone, Copy, Debug)]
pub struct HttpSys {
    pub sudo: bool,
    pub umask: u16,
}
impl Default for HttpSys {
    fn default() -> Self {
        Self {
            sudo: false,
            umask: 0o022,
        }
    }
}

/// What failed to reach the kernel, as each call of the batch hears it.
fn unreached(why: String) -> Error {
    Error::because(Errno::EIO, why)
}

impl userland::Sys for HttpSys {
    async fn batch(&self, calls: Vec<Call>) -> Vec<Result<Value, Error>> {
        let count = calls.len();
        let batch = Batch {
            calls,
            umask: self.umask,
            sudo: self.sudo,
        };
        let sent = async {
            let response = gloo_net::http::Request::post("/api/sys")
                .json(&batch)
                .map_err(|e| e.to_string())?
                .send()
                .await
                .map_err(|e| e.to_string())?;
            if !response.ok() {
                let why = response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|value| value["error"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("Request failed ({})", response.status()));
                return Err(why);
            }
            response.json::<Results>().await.map_err(|e| e.to_string())
        }
        .await;
        match sent {
            Ok(results) => results
                .results
                .into_iter()
                .map(Return::into_result)
                .collect(),
            Err(why) => (0..count).map(|_| Err(unreached(why.clone()))).collect(),
        }
    }
    async fn read(&self, path: &str) -> Result<Vec<u8>, Error> {
        let url = format!("/api/file?path={}", js_sys::encode_uri_component(path));
        let response = gloo_net::http::Request::get(&url)
            .header("Accept-Encoding", "identity")
            .send()
            .await
            .map_err(|e| unreached(e.to_string()))?;
        if response.ok() {
            return response
                .binary()
                .await
                .map_err(|e| unreached(e.to_string()));
        }
        let errno = match response.status() {
            404 => Errno::ENOENT,
            401 | 403 => Errno::EACCES,
            400 => Errno::EISDIR,
            _ => Errno::EIO,
        };
        Err(errno.into())
    }
}
