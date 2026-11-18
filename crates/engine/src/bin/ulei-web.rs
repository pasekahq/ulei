//! `ulei-web`: one Exa call of the web tools per process (ADR-0015). The request comes as one
//! JSON object on stdin; the tool result goes to stdout, exit 0 on success and 1 on failure.

#![forbid(unsafe_code)]

use std::io::{Read as _, Write as _};
use std::process::ExitCode;
use std::time::Duration;

use serde_json::Value;
use ulei_engine::web::{Call, ExaError, serve};

/// Largest Exa reply read; a longer one fails the call.
const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);

fn main() -> ExitCode {
    let mut input = Vec::new();
    if std::io::stdin().lock().read_to_end(&mut input).is_err() {
        return ExitCode::FAILURE;
    }
    let reply = serve(&input, &mut post);
    let written = std::io::stdout()
        .lock()
        .write_all(reply.text.as_bytes())
        .and_then(|()| std::io::stdout().lock().flush());
    match (written, reply.failed) {
        (Ok(()), false) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}

/// One POST without redirects, proxy or compression, bounded in time and reply size.
fn post(call: &Call<'_>) -> Result<Value, ExaError> {
    let agent = ureq::Agent::config_builder()
        .https_only(!cfg!(feature = "web-loopback-http"))
        .max_redirects(0)
        .http_status_as_error(false)
        .proxy(None)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_global(Some(call.timeout))
        .user_agent(concat!("ulei-web/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let other = |e: ureq::Error| ExaError::Other(e.to_string());
    let mut response = agent
        .post(&call.url)
        .header("x-api-key", call.key)
        .header("Content-Type", "application/json")
        .send(call.body.to_string().as_bytes())
        .map_err(other)?;
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        return Err(ExaError::Other(format!(
            "redirect not followed: status code {status}"
        )));
    }
    // Reserving the announced length keeps a growing buffer from doubling past the cap.
    let announced = response
        .body()
        .content_length()
        .filter(|&n| n <= MAX_REPLY_BYTES)
        .and_then(|n| usize::try_from(n).ok());
    let mut body = Vec::with_capacity(announced.unwrap_or(0));
    response
        .body_mut()
        .with_config()
        .limit(MAX_REPLY_BYTES)
        .reader()
        .read_to_end(&mut body)
        .map_err(|e| ExaError::Other(e.to_string()))?;
    if status >= 400 {
        return Err(ExaError::Value(format!(
            "Request failed with status code {status}: {}",
            String::from_utf8_lossy(&body)
        )));
    }
    serde_json::from_slice(&body).map_err(|e| ExaError::Value(e.to_string()))
}
