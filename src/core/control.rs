//! Control socket: newline-delimited JSON requests and responses between the CLI and the
//! running applet. The socket is the one bound by `InstanceLock`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::capture::Target;
use crate::core::instance_lock::default_socket_path;
use crate::core::status::Status;

const SUBSCRIBE_INTERVAL: Duration = Duration::from_millis(250);
/// Subscribers get a line at least this often, even without changes, so dead peers are noticed.
const SUBSCRIBE_HEARTBEAT: Duration = Duration::from_secs(5);
const ACTION_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Status,
    /// Stream a status line whenever it changes.
    Subscribe,
    Action(Action),
}

/// Requests that change state; handled on the UI thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    /// `skip_dialog` uses the last-used (or configured) options without asking.
    Start {
        target: Target,
        skip_dialog: bool,
    },
    Stop,
    /// Stop if recording or counting down, otherwise start.
    Toggle {
        target: Target,
        skip_dialog: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
}

impl Response {
    fn status(status: Status) -> Self {
        Self {
            ok: true,
            error: None,
            status: Some(status),
        }
    }

    fn from_result(result: Result<()>) -> Self {
        Self {
            ok: result.is_ok(),
            error: result.err().map(|err| format!("{err:#}")),
            status: None,
        }
    }
}

/// An action plus the channel its result is reported on.
pub type ActionRequest = (Action, Sender<Result<()>>);

/// Serve the control socket on background threads. `status` is kept current by the UI;
/// actions are forwarded to `actions`.
pub fn serve(listener: UnixListener, status: Arc<Mutex<Status>>, actions: Sender<ActionRequest>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let status = status.clone();
            let actions = actions.clone();
            thread::spawn(move || {
                if let Err(err) = handle_connection(stream, &status, &actions) {
                    eprintln!("control connection error: {err:#}");
                }
            });
        }
    });
}

fn handle_connection(
    stream: UnixStream,
    status: &Mutex<Status>,
    actions: &Sender<ActionRequest>,
) -> Result<()> {
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    if line.trim().is_empty() {
        // Plain connect (e.g. the instance lock probing for a live daemon).
        return Ok(());
    }
    let request: Request = serde_json::from_str(&line).context("invalid control request")?;

    let mut stream = stream;
    match request {
        Request::Status => write_line(
            &mut stream,
            &Response::status(status.lock().unwrap().clone()),
        ),
        Request::Subscribe => {
            let mut last: Option<Status> = None;
            let mut last_sent = Instant::now();
            loop {
                let current = status.lock().unwrap().clone();
                if last.as_ref() != Some(&current) || last_sent.elapsed() >= SUBSCRIBE_HEARTBEAT {
                    // A write error means the subscriber went away.
                    if write_line(&mut stream, &current).is_err() {
                        return Ok(());
                    }
                    last = Some(current);
                    last_sent = Instant::now();
                }
                thread::sleep(SUBSCRIBE_INTERVAL);
            }
        }
        Request::Action(action) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            actions
                .send((action, reply_tx))
                .context("applet is shutting down")?;
            let result = reply_rx
                .recv_timeout(ACTION_TIMEOUT)
                .unwrap_or_else(|_| Err(anyhow::anyhow!("applet did not respond")));
            write_line(&mut stream, &Response::from_result(result))
        }
    }
}

fn write_line<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    Ok(())
}

fn connect() -> Result<UnixStream> {
    let path = default_socket_path();
    UnixStream::connect(&path).with_context(|| {
        format!(
            "slycapture is not running (no control socket at {})",
            path.display()
        )
    })
}

/// Send one request to the running applet.
pub fn request(request: &Request) -> Result<Response> {
    let mut stream = connect()?;
    write_line(&mut stream, request)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    let response: Response = serde_json::from_str(&line).context("invalid control response")?;
    if let Some(error) = &response.error {
        bail!("{error}");
    }
    Ok(response)
}

/// Call `on_status` with every status update until the connection drops.
pub fn subscribe(mut on_status: impl FnMut(&Status)) -> Result<()> {
    let mut stream = connect()?;
    write_line(&mut stream, &Request::Subscribe)?;
    for line in BufReader::new(stream).lines() {
        let status: Status = serde_json::from_str(&line?).context("invalid status update")?;
        on_status(&status);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Action, Request};
    use crate::capture::Target;

    #[test]
    fn request_wire_format() {
        let request = Request::Action(Action::Start {
            target: Target::Window,
            skip_dialog: false,
        });
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            json,
            r#"{"cmd":"action","action":"start","target":"window","skip_dialog":false}"#
        );
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), request);
        assert_eq!(
            serde_json::to_string(&Request::Status).unwrap(),
            r#"{"cmd":"status"}"#
        );
    }
}
