//! `jai chat`: one message, with optional images, through `POST
//! /v1/chat/completions`. The answer streams to stdout; the model's thinking
//! (with `--think`) streams to stderr. `--json` prints the whole response.
//!
//! An image can steer what the model writes, so on a terminal the text is
//! printed through `term::safe`; redirected, it is written exactly as the
//! model sent it.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{anyhow, Context as _};
use base64::Engine as _;
use futures::StreamExt as _;
use serde_json::{json, Value};

use super::{check, engine_progress, read_limited, usage};
use crate::term::{self, Live, Stream};
use crate::ChatArgs;

/// The server takes at most 8 images per request (DESIGN.md section 7).
pub const MAX_IMAGES: usize = 8;

/// The server's limit for one image, 40 MB (DESIGN.md section 7).
pub const MAX_IMAGE_BYTES: u64 = 40_000_000;

/// Whether the server takes an image of `n` bytes. It estimates the size from
/// the base64 text as `len / 4 * 3`, which counts the padding, so this is the
/// same sum rather than `n <= MAX_IMAGE_BYTES`.
pub fn within_image_limit(n: u64) -> bool {
    n.div_ceil(3) * 3 <= MAX_IMAGE_BYTES
}

/// The server's body limit for chat completions.
const MAX_BODY: usize = 64 << 20;

/// The image types the server accepts, from magic bytes.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

/// An image file as a data URL; PNG, JPEG, GIF or BMP only.
fn image_data_url(path: &Path) -> anyhow::Result<String> {
    let bytes = read_limited(path, MAX_IMAGE_BYTES)?;
    if !within_image_limit(bytes.len() as u64) {
        anyhow::bail!(
            "{} is too large (at most {} per image)",
            path.display(),
            crate::output::format_bytes(MAX_IMAGE_BYTES)
        );
    }
    let Some(mime) = image_mime(&bytes) else {
        let webp = bytes.len() > 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP";
        return Err(usage(if webp {
            format!(
                "{}: WebP is not accepted; convert it to PNG or JPEG",
                path.display()
            )
        } else {
            format!("{}: not a PNG, JPEG, GIF or BMP image", path.display())
        }));
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&bytes)
    ))
}

/// The request body. Images come before the text, one part each.
pub fn chat_body(
    prompt: &str,
    images: &[String],
    system: Option<&str>,
    think: bool,
    max_tokens: Option<u32>,
    stream: bool,
) -> Value {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(json!({"role": "system", "content": system}));
    }
    let content = if images.is_empty() {
        json!(prompt)
    } else {
        let mut parts: Vec<Value> = images
            .iter()
            .map(|url| json!({"type": "image_url", "image_url": {"url": url}}))
            .collect();
        parts.push(json!({"type": "text", "text": prompt}));
        Value::Array(parts)
    };
    messages.push(json!({"role": "user", "content": content}));
    let mut body = json!({
        "model": "qwen3.8-27b",
        "messages": messages,
        "stream": stream,
        "chat_template_kwargs": {"enable_thinking": think},
    });
    if let Some(n) = max_tokens {
        body["max_tokens"] = json!(n);
    }
    body
}

/// Splits a `text/event-stream` body into the data of its events.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data: Vec<String>,
}

impl SseParser {
    /// Feeds bytes; returns the data of every event they complete.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&raw);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                self.data
                    .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            }
            // Comments (":") and the event, id and retry fields are not used.
        }
        events
    }

    /// At the end of the stream: the data of an event it did not close.
    pub fn finish(&mut self) -> Vec<String> {
        let mut events = Vec::new();
        if !self.buf.is_empty() {
            self.buf.push(b'\n');
            events = self.push(&[]);
        }
        if !self.data.is_empty() {
            events.push(self.data.join("\n"));
            self.data.clear();
        }
        events
    }
}

/// What one stream chunk adds: thinking, answer text, and the finish reason.
#[derive(Debug, Default, PartialEq)]
pub struct Delta {
    pub reasoning: Option<String>,
    pub content: Option<String>,
    pub finish: Option<String>,
}

/// Reads one chunk of a streamed completion; an error object is an error.
pub fn parse_chunk(data: &str) -> anyhow::Result<Delta> {
    let v: Value =
        serde_json::from_str(data).with_context(|| format!("unreadable stream chunk: {data}"))?;
    if let Some(err) = v.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("the model failed");
        return Err(anyhow!("{message}"));
    }
    let choice = v.get("choices").and_then(|c| c.get(0));
    let delta = choice.and_then(|c| c.get("delta"));
    let text = |key: &str| {
        delta
            .and_then(|d| d.get(key))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Ok(Delta {
        reasoning: text("reasoning_content"),
        content: text("content"),
        finish: choice
            .and_then(|c| c.get("finish_reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

pub async fn run(args: ChatArgs) -> anyhow::Result<ExitCode> {
    if args.images.len() > MAX_IMAGES {
        return Err(usage(format!("at most {MAX_IMAGES} images per message")));
    }
    let images = args
        .images
        .iter()
        .map(|p| image_data_url(p))
        .collect::<anyhow::Result<Vec<_>>>()?;
    if images.iter().map(String::len).sum::<usize>() > MAX_BODY - (1 << 20) {
        return Err(usage(
            "the images are too large together; send fewer or smaller ones",
        ));
    }
    let body = chat_body(
        &args.prompt,
        &images,
        args.system.as_deref(),
        args.think,
        args.max_tokens,
        !args.json,
    );
    let paths = super::paths()?;
    let live = Live::new();
    let progress_live = live.clone();
    let json_out = args.json;
    let (out_tty, err_tty) = (Stream::Out.is_terminal(), Stream::Err.is_terminal());
    let finish = super::with_server(&paths, &live, false, move |client, _| async move {
        let progress = engine_progress(&progress_live, &client, "Thinking");
        let resp = client
            .post("/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .context("cannot reach the JAI server")?;
        let resp = check(resp).await?;
        if json_out {
            let v: Value = resp.json().await?;
            drop(progress);
            let json = serde_json::to_string_pretty(&v)?;
            term::write_raw(Stream::Out, &format!("{}\n", term::json_safe(&json)));
            return Ok(None);
        }
        let mut progress = Some(progress);
        let mut parser = SseParser::default();
        let mut stream = resp.bytes_stream();
        let mut finish = None;
        let mut thinking = false;
        'read: loop {
            let (events, end) = match stream.next().await {
                Some(chunk) => (
                    parser.push(&chunk.context("the answer stream broke off")?),
                    false,
                ),
                None => (parser.finish(), true),
            };
            for data in events {
                if data.trim() == "[DONE]" {
                    break 'read;
                }
                let delta = parse_chunk(&data)?;
                if let Some(r) = delta.reasoning {
                    progress.take();
                    thinking = true;
                    stream_text(Stream::Err, err_tty, &r);
                }
                if let Some(c) = delta.content {
                    progress.take();
                    if thinking {
                        term::write_raw(Stream::Err, "\n");
                        thinking = false;
                    }
                    stream_text(Stream::Out, out_tty, &c);
                }
                if delta.finish.is_some() {
                    finish = delta.finish;
                }
            }
            if end {
                break;
            }
        }
        drop(progress);
        term::write_raw(Stream::Out, "\n");
        Ok(finish)
    })
    .await?;
    if finish.as_deref() == Some("length") {
        errln!("(The answer stopped at the token limit; raise --max-tokens for more.)");
    }
    Ok(ExitCode::SUCCESS)
}

/// Streamed model text: made safe on a terminal, exact when redirected.
fn stream_text(to: Stream, tty: bool, text: &str) {
    if tty {
        term::write_raw(to, &term::safe(text));
    } else {
        term::write_raw(to, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mime_types() {
        assert_eq!(image_mime(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
        assert_eq!(image_mime(&[0xff, 0xd8, 0xff, 0xe0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(image_mime(b"BM......"), Some("image/bmp"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WEBPVP8 "), None);
        assert_eq!(image_mime(b""), None);
    }

    #[test]
    fn image_limit_matches_the_server() {
        // The server counts len / 4 * 3 of the base64 text.
        let server_takes = |n: u64| (4 * n.div_ceil(3)) / 4 * 3 <= 40_000_000;
        for n in [
            0, 1, 39_999_998, 39_999_999, 40_000_000, 40_000_001, 41_943_040,
        ] {
            assert_eq!(within_image_limit(n), server_takes(n), "{n}");
        }
        assert!(within_image_limit(39_999_999));
        assert!(!within_image_limit(40_000_000));
    }

    #[test]
    fn image_files() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\nxx").unwrap();
        assert!(image_data_url(&png)
            .unwrap()
            .starts_with("data:image/png;base64,iVBORw0KGg"));
        let webp = dir.path().join("a.webp");
        std::fs::write(&webp, b"RIFF\0\0\0\0WEBPVP8 xxxx").unwrap();
        let e = image_data_url(&webp).unwrap_err();
        assert!(e.to_string().contains("WebP"), "{e}");
        assert_eq!(super::super::exit_code(&e), 2);
    }

    #[test]
    fn bodies() {
        let b = chat_body("hi", &[], None, false, None, true);
        assert_eq!(
            b,
            json!({"model": "qwen3.8-27b", "messages": [{"role": "user", "content": "hi"}],
                   "stream": true, "chat_template_kwargs": {"enable_thinking": false}})
        );
        let b = chat_body(
            "what is this?",
            &["data:image/png;base64,AA==".into()],
            Some("Be brief."),
            true,
            Some(100),
            false,
        );
        assert_eq!(
            b["messages"][0],
            json!({"role": "system", "content": "Be brief."})
        );
        assert_eq!(
            b["messages"][1]["content"],
            json!([
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}},
                {"type": "text", "text": "what is this?"}
            ])
        );
        assert_eq!(b["max_tokens"], 100);
        assert_eq!(b["chat_template_kwargs"]["enable_thinking"], true);
        assert_eq!(b["stream"], false);
    }

    #[test]
    fn server_sent_events() {
        let mut p = SseParser::default();
        assert!(p.push(b"data: {\"a\"").is_empty());
        assert_eq!(p.push(b":1}\n\n: comment\n\ndata: x\r\n"), ["{\"a\":1}"]);
        assert_eq!(p.push(b"\r\ndata: [DONE]\n\n"), ["x", "[DONE]"]);
        // Multi-line data, and an event the stream does not close.
        assert_eq!(p.push(b"data: one\ndata: two\n\n"), ["one\ntwo"]);
        p.push(b"data: last");
        assert_eq!(p.finish(), ["last"]);
        assert!(p.finish().is_empty());
        // A multibyte character split across chunks.
        let mut p = SseParser::default();
        let bytes = "data: ø\n\n".as_bytes();
        assert!(p.push(&bytes[..7]).is_empty());
        assert_eq!(p.push(&bytes[7..]), ["ø"]);
    }

    #[test]
    fn chunks() {
        let d = parse_chunk(r#"{"choices":[{"delta":{"content":"Hel"},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(
            d,
            Delta {
                content: Some("Hel".into()),
                ..Delta::default()
            }
        );
        let d = parse_chunk(r#"{"choices":[{"delta":{"reasoning_content":"hmm"}}]}"#).unwrap();
        assert_eq!(d.reasoning.as_deref(), Some("hmm"));
        let d = parse_chunk(r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#).unwrap();
        assert_eq!(d.finish.as_deref(), Some("length"));
        let e = parse_chunk(r#"{"error":{"message":"engine crashed"}}"#).unwrap_err();
        assert_eq!(e.to_string(), "engine crashed");
        assert!(parse_chunk("not json").is_err());
    }
}
