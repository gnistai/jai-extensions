//! The HTTP part of an install: a resumable download of one file into its part
//! file, hashing as it goes, with retries and backoff.

use std::future::Future;
use std::path::Path;

use anyhow::Context as _;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::StatusCode;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWriteExt as _, BufWriter};
use tracing::{info, warn};

use super::report::Reporter;
use super::{files, Ctx, DlError, DlState, IoAt, Tuning};
use crate::catalog::Component;
use crate::fsutil;

/// The only identifying header Studio sends: its name and version.
pub(super) const USER_AGENT: &str = concat!("jai-studio/", env!("CARGO_PKG_VERSION"));

const WRITE_BUFFER: usize = 1 << 20;

/// The client for one install run: a fixed User-Agent, no Referer, no cookies.
pub(super) fn client(tuning: &Tuning) -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .connect_timeout(tuning.connect_timeout)
        .referer(false)
        .build()
        .context("cannot set up the HTTP client")
}

/// Downloads `c.url` into `part` until the part holds `c.size` bytes, and
/// returns the hex SHA-256 of the whole part. An existing part is hashed first
/// and resumed. A part that is already complete needs no request at all.
pub(super) async fn download(
    ctx: &Ctx<'_>,
    c: &Component,
    part: &Path,
    rep: &Reporter,
) -> Result<String, DlError> {
    let mut hasher = Sha256::new();
    let mut have = 0u64;
    match std::fs::metadata(part) {
        Ok(meta) if meta.len() > c.size => {
            warn!(path = %part.display(), "the part file is larger than the component; starting over");
            files::remove_file(part)?;
        }
        Ok(meta) if meta.len() > 0 => {
            rep.set(DlState::Verifying, 0);
            (hasher, have) =
                files::hash(part.to_path_buf(), rep.clone(), ctx.cancel.clone()).await?;
        }
        _ => {}
    }

    let mut failures = 0u32;
    while have < c.size {
        if ctx.cancel.is_cancelled() {
            return Err(DlError::Canceled);
        }
        rep.set(DlState::Downloading, have);
        let mut received = 0u64;
        let error = match attempt(ctx, c, part, &mut hasher, &mut have, &mut received, rep).await {
            Ok(()) if have == c.size => break,
            Ok(()) => DlError::Incomplete {
                got: have,
                expected: c.size,
            },
            Err(e) => e,
        };
        if matches!(error, DlError::WrongSize { .. } | DlError::TooLong { .. }) {
            // The server has a different file; what we have is not part of it.
            files::remove_file(part)?;
        }
        if !error.retryable() {
            return Err(error);
        }
        failures = if received > 0 { 1 } else { failures + 1 };
        if failures > ctx.tuning.retries {
            return Err(error);
        }
        let delay = ctx
            .tuning
            .backoff
            .saturating_mul(1 << (failures - 1).min(10));
        warn!(id = %c.id, %error, retry = failures, delay_ms = delay.as_millis() as u64, "download interrupted; retrying");
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = ctx.cancel.cancelled() => return Err(DlError::Canceled),
        }
    }
    files::sync(part)?;
    Ok(fsutil::hex(&hasher.finalize()))
}

/// One request. `have` and `hasher` always describe what the part file holds;
/// `received` counts the bytes this attempt added.
async fn attempt(
    ctx: &Ctx<'_>,
    c: &Component,
    part: &Path,
    hasher: &mut Sha256,
    have: &mut u64,
    received: &mut u64,
    rep: &Reporter,
) -> Result<(), DlError> {
    let mut request = ctx.client.get(&c.url);
    if *have > 0 {
        request = request.header(RANGE, format!("bytes={have}-"));
    }
    let mut response = guarded(ctx, request.send()).await?;
    let append = match response.status() {
        StatusCode::PARTIAL_CONTENT => {
            let header = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_owned();
            match parse_content_range(&header) {
                Some((start, total)) if start == *have => {
                    if let Some(total) = total.filter(|t| *t != c.size) {
                        return Err(DlError::WrongSize {
                            got: total,
                            expected: c.size,
                        });
                    }
                    true
                }
                _ => {
                    let error = DlError::BadRange {
                        expected: *have,
                        header,
                    };
                    (*hasher, *have) = (Sha256::new(), 0);
                    return Err(error);
                }
            }
        }
        StatusCode::OK => {
            if let Some(len) = response.content_length().filter(|l| *l != c.size) {
                return Err(DlError::WrongSize {
                    got: len,
                    expected: c.size,
                });
            }
            if *have > 0 {
                info!(id = %c.id, "the server does not resume downloads; starting over");
            }
            (*hasher, *have) = (Sha256::new(), 0);
            false
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            (*hasher, *have) = (Sha256::new(), 0);
            return Err(DlError::RangeRejected);
        }
        other => return Err(DlError::Status(other)),
    };

    let file = files::open_part(part, append && *have > 0)?;
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER, tokio::fs::File::from_std(file));
    let streamed = async {
        while let Some(chunk) = guarded(ctx, response.chunk()).await? {
            let len = chunk.len() as u64;
            if *have + len > c.size {
                return Err(DlError::TooLong { expected: c.size });
            }
            writer.write_all(&chunk).await.at(part)?;
            hasher.update(&chunk);
            *have += len;
            *received += len;
            rep.progress(*have);
        }
        Ok(())
    }
    .await;
    // Whatever happened, the part file must hold exactly what was hashed.
    writer.flush().await.at(part)?;
    streamed
}

/// Bounds a request step by the read timeout and the cancellation token.
async fn guarded<T>(
    ctx: &Ctx<'_>,
    step: impl Future<Output = reqwest::Result<T>>,
) -> Result<T, DlError> {
    tokio::select! {
        result = tokio::time::timeout(ctx.tuning.read_timeout, step) => match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(e)) => Err(DlError::Network(e)),
            Err(_) => Err(DlError::Stalled(ctx.tuning.read_timeout)),
        },
        () = ctx.cancel.cancelled() => Err(DlError::Canceled),
    }
}

/// `bytes <start>-<end>/<total or *>` as the start and the total, if known.
fn parse_content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((start, total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(
            parse_content_range("bytes 100-199/200"),
            Some((100, Some(200)))
        );
        assert_eq!(parse_content_range("bytes 0-0/*"), Some((0, None)));
        assert_eq!(parse_content_range(" bytes  5-9/10 "), Some((5, Some(10))));
        assert_eq!(parse_content_range("bytes */200"), None);
        assert_eq!(parse_content_range("bytes 9-5/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range(""), None);
    }

    #[test]
    fn user_agent_names_only_the_app() {
        assert_eq!(
            USER_AGENT,
            format!("jai-studio/{}", env!("CARGO_PKG_VERSION"))
        );
    }
}
