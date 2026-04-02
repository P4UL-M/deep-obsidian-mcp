use std::io::{self, Read, Write};

use deep_obsidian_types::{ResolvedServiceConfig, StdioMode};
use serde_json::Value;

use crate::mcp::{handle_request, AppState};
use crate::protocol::JsonRpcRequest;
use crate::runtime::RuntimeState;

fn parse_json_message(payload: &str) -> io::Result<JsonRpcRequest> {
    serde_json::from_str(payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn detect_input_mode(buffer: &[u8]) -> Option<StdioMode> {
    if buffer.is_empty() {
        return None;
    }
    let preview = String::from_utf8_lossy(&buffer[..buffer.len().min(64)]);
    let trimmed = preview.trim_start();
    if trimmed.starts_with("Content-Length:") {
        Some(StdioMode::Framed)
    } else if trimmed.starts_with('{') {
        Some(StdioMode::Newline)
    } else {
        None
    }
}

fn read_newline_message(buffer: &mut Vec<u8>) -> io::Result<Option<JsonRpcRequest>> {
    let Some(newline_index) = buffer.iter().position(|byte| *byte == b'\n') else {
        return Ok(None);
    };
    let line = String::from_utf8_lossy(&buffer[..newline_index]).trim_end_matches('\r').to_string();
    buffer.drain(..=newline_index);
    if line.trim().is_empty() {
        return Ok(None);
    }
    parse_json_message(&line).map(Some)
}

fn read_framed_message(buffer: &mut Vec<u8>) -> io::Result<Option<JsonRpcRequest>> {
    let bytes = buffer.as_slice();
    let crlf_header_end = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    let lf_header_end = bytes.windows(2).position(|window| window == b"\n\n");
    let Some(header_end) = crlf_header_end.or(lf_header_end) else {
        return Ok(None);
    };
    let separator_length = if crlf_header_end.is_some() { 4 } else { 2 };
    let header_text = String::from_utf8_lossy(&bytes[..header_end]);
    let mut content_length = None;
    for line in header_text.lines() {
        if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().ok();
                break;
            }
        }
    }
    let content_length = content_length
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header"))?;
    let body_start = header_end + separator_length;
    let body_end = body_start + content_length;
    if buffer.len() < body_end {
        return Ok(None);
    }
    let payload = String::from_utf8_lossy(&buffer[body_start..body_end]).to_string();
    buffer.drain(..body_end);
    parse_json_message(&payload).map(Some)
}

fn send_message(stdout: &mut impl Write, message: &Value, mode: StdioMode) -> io::Result<()> {
    let serialized = serde_json::to_string(message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    match mode {
        StdioMode::Framed => {
            write!(stdout, "Content-Length: {}\r\n\r\n{}", serialized.len(), serialized)?;
        }
        _ => {
            writeln!(stdout, "{serialized}")?;
        }
    }
    stdout.flush()
}

pub async fn run_stdio_service(config: ResolvedServiceConfig) -> io::Result<()> {
    let (runtime, _auto_reindex) = RuntimeState::bootstrap(config.clone())
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error))?;
    let state = AppState::new(config.clone(), runtime);
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut buffer = Vec::new();
    let mut read_buf = [0u8; 8192];
    let mut input_mode = match config.stdio_mode {
        StdioMode::Auto => None,
        mode => Some(mode),
    };
    let mut output_mode = match config.stdio_mode {
        StdioMode::Framed => StdioMode::Framed,
        _ => StdioMode::Newline,
    };

    loop {
        let bytes_read = stdin.read(&mut read_buf)?;
        if bytes_read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&read_buf[..bytes_read]);

        loop {
            if input_mode.is_none() {
                let Some(detected) = detect_input_mode(&buffer) else {
                    break;
                };
                input_mode = Some(detected);
                output_mode = detected;
            }

            let message = match input_mode.expect("input mode to be detected") {
                StdioMode::Framed => read_framed_message(&mut buffer)?,
                _ => read_newline_message(&mut buffer)?,
            };

            let Some(message) = message else {
                break;
            };

            let response = handle_request(state.clone(), message)
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, serde_json::to_string(&error).unwrap_or_else(|_| error.error.message)))?;
            if let Some(response) = response {
                send_message(&mut stdout, &response, output_mode)?;
            }
        }
    }
}
