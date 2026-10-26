//! Minimal client for the i3 IPC protocol (https://i3wm.org/docs/ipc.html).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::Value;

const MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LEN: usize = MAGIC.len() + 8;
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const GET_TREE: u32 = 4;

pub struct I3Connection {
    stream: UnixStream,
}

#[derive(Debug, Deserialize)]
struct CommandOutcome {
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

impl I3Connection {
    pub fn connect() -> Result<Self> {
        let path = socket_path()?;
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("failed to connect to i3 IPC socket: {}", path.display()))?;
        Ok(Self { stream })
    }

    /// Run an i3 command string (same syntax as `i3-msg`), failing if any part of it fails.
    pub fn run_command(&mut self, command: &str) -> Result<()> {
        let payload = self.request(RUN_COMMAND, command.as_bytes())?;
        let outcomes: Vec<CommandOutcome> =
            serde_json::from_slice(&payload).context("failed to parse i3 command reply")?;
        for outcome in outcomes {
            if !outcome.success {
                bail!(
                    "i3 command failed: {} ({command})",
                    outcome.error.unwrap_or_else(|| "unknown error".into())
                );
            }
        }
        Ok(())
    }

    fn focused_workspace(&mut self) -> Result<Option<String>> {
        let payload = self.request(GET_WORKSPACES, b"")?;
        let workspaces: Vec<Value> =
            serde_json::from_slice(&payload).context("failed to parse i3 workspaces")?;
        Ok(workspaces
            .iter()
            .find(|workspace| workspace["focused"].as_bool() == Some(true))
            .and_then(|workspace| workspace["name"].as_str())
            .map(str::to_string))
    }

    fn tree(&mut self) -> Result<Value> {
        let payload = self.request(GET_TREE, b"")?;
        serde_json::from_slice(&payload).context("failed to parse i3 tree")
    }

    fn request(&mut self, message_type: u32, payload: &[u8]) -> Result<Vec<u8>> {
        self.stream
            .write_all(&encode_message(message_type, payload))?;

        let mut header = [0u8; HEADER_LEN];
        self.stream.read_exact(&mut header)?;
        let (reply_type, len) = decode_header(&header)?;
        if reply_type != message_type {
            bail!("unexpected i3 reply type {reply_type} (expected {message_type})");
        }

        let mut reply = vec![0u8; len];
        self.stream.read_exact(&mut reply)?;
        Ok(reply)
    }
}

/// Where a window was before it was staged for recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowPlacement {
    pub window: u32,
    pub workspace: String,
    /// Workspace that had focus, switched back to on restore.
    pub focused_workspace: Option<String>,
    pub border: String,
    pub border_width: i64,
    /// Floating geometry `(x, y, width, height)`, or `None` if the window was tiled.
    pub floating_rect: Option<(i64, i64, i64, i64)>,
}

/// Move `window` to its own workspace, make it a borderless floating window of the given size,
/// and switch to that workspace. Returns the previous placement for [`restore_window`].
pub fn stage_window_for_recording(
    window: u32,
    width: u32,
    height: u32,
    workspace: &str,
) -> Result<Option<WindowPlacement>> {
    let mut conn = I3Connection::connect()?;
    let focused = conn.focused_workspace().unwrap_or(None);
    let placement = conn
        .tree()
        .ok()
        .and_then(|tree| find_placement(&tree, window, None, false))
        .map(|placement| WindowPlacement {
            focused_workspace: focused,
            ..placement
        });
    conn.run_command(&stage_window_command(window, width, height, workspace))?;
    Ok(placement)
}

/// Put a window back where [`stage_window_for_recording`] found it.
pub fn restore_window(placement: &WindowPlacement) -> Result<()> {
    I3Connection::connect()?.run_command(&restore_command(placement))
}

fn restore_command(placement: &WindowPlacement) -> String {
    let target = format!("[id=\"{}\"]", placement.window);
    let border = match placement.border.as_str() {
        "none" => "border none".to_string(),
        style => format!("border {style} {}", placement.border_width),
    };
    let layout = match placement.floating_rect {
        Some((x, y, width, height)) => format!(
            "floating enable, resize set {width} px {height} px, move position {x} px {y} px"
        ),
        None => "floating disable".to_string(),
    };
    let switch_to = placement
        .focused_workspace
        .as_deref()
        .unwrap_or(&placement.workspace);
    format!(
        "{target} move container to workspace {}; {target} {layout}, {border}; workspace {}",
        quote(&placement.workspace),
        quote(switch_to)
    )
}

/// Find `window` in an i3 tree, noting its workspace, border and floating geometry.
fn find_placement(
    node: &Value,
    window: u32,
    workspace: Option<&str>,
    in_floating: bool,
) -> Option<WindowPlacement> {
    let workspace = match node["type"].as_str() {
        Some("workspace") => node["name"].as_str(),
        _ => workspace,
    };
    if node["window"].as_u64() == Some(u64::from(window)) {
        let rect = &node["rect"];
        let floating =
            in_floating || matches!(node["floating"].as_str(), Some("user_on" | "auto_on"));
        return Some(WindowPlacement {
            window,
            workspace: workspace?.to_string(),
            focused_workspace: None,
            border: node["border"].as_str().unwrap_or("pixel").to_string(),
            border_width: node["current_border_width"].as_i64().unwrap_or(2),
            floating_rect: floating.then(|| {
                (
                    rect["x"].as_i64().unwrap_or(0),
                    rect["y"].as_i64().unwrap_or(0),
                    rect["width"].as_i64().unwrap_or(800),
                    rect["height"].as_i64().unwrap_or(600),
                )
            }),
        });
    }

    let floating_children = node["floating_nodes"].as_array().into_iter().flatten();
    let tiled_children = node["nodes"].as_array().into_iter().flatten();
    tiled_children
        .map(|child| (child, false))
        .chain(floating_children.map(|child| (child, true)))
        .find_map(|(child, floating)| {
            find_placement(child, window, workspace, in_floating || floating)
        })
}

fn stage_window_command(window: u32, width: u32, height: u32, workspace: &str) -> String {
    let workspace = quote(workspace);
    let target = format!("[id=\"{window}\"]");
    format!(
        "{target} move container to workspace {workspace}; workspace {workspace}; \
{target} floating enable, border none, resize set {width} px {height} px, move position center, focus"
    )
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn encode_message(message_type: u32, payload: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(HEADER_LEN + payload.len());
    message.extend_from_slice(MAGIC);
    message.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    message.extend_from_slice(&message_type.to_ne_bytes());
    message.extend_from_slice(payload);
    message
}

fn decode_header(header: &[u8; HEADER_LEN]) -> Result<(u32, usize)> {
    if &header[..MAGIC.len()] != MAGIC {
        bail!("invalid i3 IPC reply magic");
    }
    let len = u32::from_ne_bytes(header[6..10].try_into()?);
    let reply_type = u32::from_ne_bytes(header[10..14].try_into()?);
    Ok((reply_type, len as usize))
}

fn socket_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("I3SOCK").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(path));
    }

    let output = Command::new("i3")
        .arg("--get-socketpath")
        .output()
        .context("I3SOCK is not set and `i3 --get-socketpath` could not be run")?;
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    if !output.status.success() || path.is_empty() {
        bail!("unable to determine i3 IPC socket path; is i3 running?");
    }
    Ok(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::{
        decode_header, encode_message, find_placement, restore_command, stage_window_command,
        WindowPlacement, HEADER_LEN,
    };

    #[test]
    fn encodes_and_decodes_header() {
        let message = encode_message(0, b"nop");
        assert_eq!(&message[..6], b"i3-ipc");
        let header: [u8; HEADER_LEN] = message[..HEADER_LEN].try_into().unwrap();
        assert_eq!(decode_header(&header).unwrap(), (0, 3));
        assert_eq!(&message[HEADER_LEN..], b"nop");
    }

    #[test]
    fn rejects_bad_magic() {
        let header = [0u8; HEADER_LEN];
        assert!(decode_header(&header).is_err());
    }

    #[test]
    fn builds_stage_command_with_quoted_workspace() {
        let command = stage_window_command(42, 1920, 1080, "rec \"1\"");
        assert_eq!(
            command,
            "[id=\"42\"] move container to workspace \"rec \\\"1\\\"\"; workspace \"rec \\\"1\\\"\"; \
[id=\"42\"] floating enable, border none, resize set 1920 px 1080 px, move position center, focus"
        );
    }

    fn tree() -> serde_json::Value {
        serde_json::json!({
            "type": "root", "nodes": [{
                "type": "output", "nodes": [{
                    "type": "workspace", "name": "2",
                    "nodes": [{
                        "type": "con", "window": 7, "border": "pixel",
                        "current_border_width": 3, "floating": "auto_off",
                        "rect": {"x": 5, "y": 41, "width": 100, "height": 100}
                    }],
                    "floating_nodes": [{
                        "type": "floating_con", "floating": "user_on", "nodes": [{
                            "type": "con", "window": 9, "border": "normal",
                            "current_border_width": 2,
                            "rect": {"x": 10, "y": 20, "width": 300, "height": 200}
                        }]
                    }]
                }]
            }]
        })
    }

    #[test]
    fn finds_tiled_window() {
        let placement = find_placement(&tree(), 7, None, false).unwrap();
        assert_eq!(placement.workspace, "2");
        assert_eq!(placement.border, "pixel");
        assert_eq!(placement.border_width, 3);
        assert_eq!(placement.floating_rect, None);
    }

    #[test]
    fn finds_floating_window() {
        let placement = find_placement(&tree(), 9, None, false).unwrap();
        assert_eq!(placement.floating_rect, Some((10, 20, 300, 200)));
        assert!(find_placement(&tree(), 42, None, false).is_none());
    }

    #[test]
    fn builds_restore_command() {
        let placement = WindowPlacement {
            window: 7,
            workspace: "2".into(),
            focused_workspace: Some("1".into()),
            border: "pixel".into(),
            border_width: 3,
            floating_rect: None,
        };
        assert_eq!(
            restore_command(&placement),
            "[id=\"7\"] move container to workspace \"2\"; \
[id=\"7\"] floating disable, border pixel 3; workspace \"1\""
        );
    }
}
