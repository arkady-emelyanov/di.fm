//! A seekable HTTP reader built on range requests.
//!
//! Tracks are served as complete files from a CDN, so the decoder needs `Read + Seek`
//! (MP4 containers often keep their index at the end). Instead of downloading the whole
//! file up front, we stream it and reopen the connection at the new offset on seek.
//! The connection is also reopened transparently if it drops, which happens when a
//! track is queued for gapless playback well before it starts.

use std::io::{self, Read, Seek, SeekFrom};

use anyhow::{Context, Result, bail};
use reqwest::blocking::{Client, Response};
use reqwest::header::{CONTENT_RANGE, RANGE};

const MAX_RECONNECTS: u32 = 5;

pub struct HttpRangeReader {
    client: Client,
    url: String,
    len: u64,
    pos: u64,
    body: Option<Response>,
}

impl HttpRangeReader {
    pub fn open(client: Client, url: &str) -> Result<Self> {
        let resp = client
            .get(url)
            .header(RANGE, "bytes=0-")
            .send()
            .and_then(Response::error_for_status)
            .context("requesting track")?;
        let len = total_len(&resp).context("track response has no length")?;
        Ok(Self { client, url: url.to_owned(), len, pos: 0, body: Some(resp) })
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    fn reconnect(&mut self) -> io::Result<()> {
        self.body = None;
        if self.pos >= self.len {
            return Ok(());
        }
        let resp = self
            .client
            .get(&self.url)
            .header(RANGE, format!("bytes={}-", self.pos))
            .send()
            .and_then(Response::error_for_status)
            .map_err(io::Error::other)?;
        if resp.status() != reqwest::StatusCode::PARTIAL_CONTENT && self.pos > 0 {
            return Err(io::Error::other("server ignored range request"));
        }
        self.body = Some(resp);
        Ok(())
    }
}

fn total_len(resp: &Response) -> Result<u64> {
    if let Some(range) = resp.headers().get(CONTENT_RANGE) {
        // "bytes 0-1234/1235"
        let range = range.to_str()?;
        if let Some(total) = range.rsplit('/').next().and_then(|t| t.parse().ok()) {
            return Ok(total);
        }
    }
    match resp.content_length() {
        Some(len) => Ok(len),
        None => bail!("missing Content-Length"),
    }
}

impl Read for HttpRangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len || buf.is_empty() {
            return Ok(0);
        }
        let mut attempts = 0;
        loop {
            if self.body.is_none() {
                self.reconnect()?;
            }
            let result = self.body.as_mut().map_or(Ok(0), |b| b.read(buf));
            match result {
                Ok(n) if n > 0 => {
                    self.pos += n as u64;
                    return Ok(n);
                }
                // Premature EOF or a dropped connection: resume from where we are.
                Ok(_) | Err(_) if attempts < MAX_RECONNECTS => {
                    attempts += 1;
                    log::debug!("track connection dropped at {}/{}, reconnecting", self.pos, self.len);
                    self.body = None;
                }
                Ok(_) => return Ok(0),
                Err(e) => return Err(e),
            }
        }
    }
}

impl Seek for HttpRangeReader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::End(off) => self.len as i64 + off,
            SeekFrom::Current(off) => self.pos as i64 + off,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        let target = target as u64;
        if target != self.pos {
            // Short forward skips are cheaper to read through than to reconnect.
            if target > self.pos && target - self.pos <= 64 * 1024 && self.body.is_some() {
                let n = target - self.pos;
                let mut skip = (&mut *self).take(n);
                io::copy(&mut skip, &mut io::sink())?;
            } else {
                self.pos = target;
                self.body = None;
            }
        }
        Ok(self.pos)
    }
}
