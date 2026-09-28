use serde::Serialize;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, watch};

/// Errors while reading lines.
#[derive(Debug, thiserror::Error)]
pub enum LineError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    #[error("line exceeds {max} bytes")]
    TooLong { max: usize },
}

/// One line read from the stream.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadLine {
    /// A line that parsed as JSON.
    Json(Value),
    /// A non-empty line that is not valid JSON (kept verbatim, lossily decoded as UTF-8).
    NotJson(String),
}

/// Reads LF-delimited JSON values. A trailing `\r` is stripped; empty lines are skipped.
pub struct JsonLinesReader<R> {
    inner: BufReader<R>,
    /// The part of the current line read so far. It lives here (not in the `next` future) so
    /// that dropping a pending `next` loses nothing.
    buf: Vec<u8>,
    /// The rest of an oversized line is being discarded.
    skipping: bool,
    max_line_bytes: usize,
}

impl<R: AsyncRead + Unpin> JsonLinesReader<R> {
    /// `max_line_bytes` bounds memory use when a child writes an unterminated stream: a line
    /// longer than that is reported as [`LineError::TooLong`] and its remainder is discarded
    /// chunk by chunk, without being buffered.
    pub fn new(reader: R, max_line_bytes: usize) -> Self {
        Self {
            inner: BufReader::with_capacity(64 * 1024, reader),
            buf: Vec::new(),
            skipping: false,
            max_line_bytes,
        }
    }

    /// Returns the next line, or `Ok(None)` at end of stream.
    ///
    /// Cancel safe: every byte taken from the stream is recorded in `self` before the next
    /// await point, so the future can be dropped (e.g. as the losing branch of a `select!`)
    /// and a later call continues the same line.
    pub async fn next(&mut self) -> Result<Option<ReadLine>, LineError> {
        loop {
            // `fill_buf` consumes nothing, so being cancelled here loses nothing.
            let available = self.inner.fill_buf().await?;
            if available.is_empty() {
                // End of stream: a final unterminated line still counts.
                if std::mem::take(&mut self.skipping) || self.buf.is_empty() {
                    return Ok(None);
                }
                let line = std::mem::take(&mut self.buf);
                return Ok(parse_line(&line));
            }
            let newline = available.iter().position(|&b| b == b'\n');
            let content = newline.unwrap_or(available.len());
            let consumed = newline.map_or(available.len(), |pos| pos + 1);
            if self.skipping {
                self.inner.consume(consumed);
                if newline.is_some() {
                    self.skipping = false;
                }
                continue;
            }
            if self.buf.len() + content > self.max_line_bytes {
                self.inner.consume(consumed);
                // Release the memory of the partial line and discard the rest of it.
                self.buf = Vec::new();
                self.skipping = newline.is_none();
                return Err(LineError::TooLong {
                    max: self.max_line_bytes,
                });
            }
            self.buf.extend_from_slice(&available[..content]);
            self.inner.consume(consumed);
            if newline.is_none() {
                continue;
            }
            // Taking the buffer also releases the capacity of an unusually long line.
            let line = std::mem::take(&mut self.buf);
            if let Some(parsed) = parse_line(&line) {
                return Ok(Some(parsed));
            }
        }
    }
}

/// Parses one line (without its LF); `None` for a blank line.
fn parse_line(raw: &[u8]) -> Option<ReadLine> {
    let mut line = raw;
    while let Some((&last, rest)) = line.split_last() {
        if last == b'\n' || last == b'\r' {
            line = rest;
        } else {
            break;
        }
    }
    if line.iter().all(u8::is_ascii_whitespace) {
        return None;
    }
    Some(match serde_json::from_slice::<Value>(line) {
        Ok(v) => ReadLine::Json(v),
        Err(_) => ReadLine::NotJson(String::from_utf8_lossy(line).into_owned()),
    })
}

/// Writes values as single-line JSON terminated by LF, flushing after each value.
pub struct JsonLinesWriter<W> {
    inner: W,
}

impl<W: AsyncWrite + Unpin> JsonLinesWriter<W> {
    pub fn new(writer: W) -> Self {
        Self { inner: writer }
    }

    pub async fn send<T: Serialize + ?Sized>(&mut self, value: &T) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(value).map_err(std::io::Error::other)?;
        bytes.push(b'\n');
        self.inner.write_all(&bytes).await?;
        self.inner.flush().await
    }

    /// Closes the underlying stream (EOF for the child).
    pub async fn close(mut self) -> std::io::Result<()> {
        self.inner.shutdown().await
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Failure of [`SharedJsonLinesWriter::send`].
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The writer was closed (by [`SharedJsonLinesWriter::close`]) or the child's end of the
    /// pipe is gone.
    #[error("the stream is closed")]
    Closed,
    #[error("i/o error: {0}")]
    Io(std::io::Error),
}

type BoxWriter = Box<dyn AsyncWrite + Send + Unpin>;

/// A [`JsonLinesWriter`] shared by several tasks (one message at a time).
///
/// [`close`](Self::close) never waits behind a write that cannot finish: a child that stops
/// reading its stdin leaves a write blocked on the full pipe forever, and the staged stop
/// (close stdin → grace → terminate the tree) must still go ahead. Closing therefore abandons
/// a pending write (the writer is dropped and that `send` returns [`WriteError::Closed`]).
pub struct SharedJsonLinesWriter {
    writer: Mutex<Option<JsonLinesWriter<BoxWriter>>>,
    closing: watch::Sender<bool>,
}

impl SharedJsonLinesWriter {
    pub fn new<W: AsyncWrite + Send + Unpin + 'static>(writer: W) -> Self {
        Self {
            writer: Mutex::new(Some(JsonLinesWriter::new(Box::new(writer)))),
            closing: watch::channel(false).0,
        }
    }

    /// Writes one value (and flushes it).
    pub async fn send<T: Serialize + ?Sized>(&self, value: &T) -> Result<(), WriteError> {
        let mut closing = self.closing.subscribe();
        if *closing.borrow_and_update() {
            return Err(WriteError::Closed);
        }
        let mut guard = tokio::select! {
            guard = self.writer.lock() => guard,
            _ = closing.wait_for(|c| *c) => return Err(WriteError::Closed),
        };
        let Some(writer) = guard.as_mut() else {
            return Err(WriteError::Closed);
        };
        let written = tokio::select! {
            result = writer.send(value) => Some(result),
            _ = closing.wait_for(|c| *c) => None,
        };
        match written {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(WriteError::Closed),
            Some(Err(e)) => Err(WriteError::Io(e)),
            None => {
                // Closed while this write was pending: the stream may hold a partial line, so
                // it is dropped rather than reused.
                *guard = None;
                Err(WriteError::Closed)
            }
        }
    }

    /// Closes the stream (EOF for the child). Returns promptly even when another task's
    /// write is stuck on a full pipe; later sends fail with [`WriteError::Closed`].
    pub async fn close(&self) {
        self.closing.send_replace(true);
        let writer = self.writer.lock().await.take();
        if let Some(writer) = writer {
            let _ = writer.close().await;
        }
    }

    /// Whether [`close`](Self::close) was called.
    pub fn is_closed(&self) -> bool {
        *self.closing.borrow()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    async fn read_all(input: &[u8], max: usize) -> Vec<Result<ReadLine, String>> {
        let mut reader = JsonLinesReader::new(input, max);
        let mut out = Vec::new();
        loop {
            match reader.next().await {
                Ok(Some(line)) => out.push(Ok(line)),
                Ok(None) => break,
                Err(e) => out.push(Err(e.to_string())),
            }
        }
        out
    }

    #[tokio::test]
    async fn parses_lf_crlf_and_final_line() {
        let out = read_all(b"{\"a\":1}\r\n\n  \n{\"b\":2}\nnot json\n{\"c\":3}", 1024).await;
        assert_eq!(
            out,
            vec![
                Ok(ReadLine::Json(serde_json::json!({"a":1}))),
                Ok(ReadLine::Json(serde_json::json!({"b":2}))),
                Ok(ReadLine::NotJson("not json".into())),
                Ok(ReadLine::Json(serde_json::json!({"c":3}))),
            ]
        );
    }

    #[tokio::test]
    async fn oversized_line_is_skipped_and_stream_continues() {
        let mut input = Vec::new();
        input.extend_from_slice(b"{\"x\":\"");
        input.extend(std::iter::repeat_n(b'a', 200));
        input.extend_from_slice(b"\"}\n{\"ok\":true}\n");
        let out = read_all(&input, 64).await;
        assert_eq!(out.len(), 2);
        assert!(out[0].as_ref().unwrap_err().contains("exceeds"));
        assert_eq!(out[1], Ok(ReadLine::Json(serde_json::json!({"ok":true}))));
    }

    #[tokio::test]
    async fn oversized_line_split_across_reads_is_discarded_without_buffering_it() {
        let (mut tx, rx) = tokio::io::duplex(64);
        let mut reader = JsonLinesReader::new(rx, 100);
        let writer = tokio::spawn(async move {
            tx.write_all(b"{\"big\":\"").await.unwrap();
            for _ in 0..200 {
                tx.write_all(&[b'x'; 50]).await.unwrap();
            }
            tx.write_all(b"\"}\n{\"after\":1}\n").await.unwrap();
        });
        assert!(matches!(
            reader.next().await,
            Err(LineError::TooLong { max: 100 })
        ));
        assert!(
            reader.buf.capacity() <= 100,
            "the discarded line is not kept in memory"
        );
        assert_eq!(
            reader.next().await.unwrap(),
            Some(ReadLine::Json(serde_json::json!({"after": 1})))
        );
        assert!(reader.buf.capacity() <= 100);
        writer.await.unwrap();
    }

    #[tokio::test]
    async fn a_cancelled_next_keeps_the_partial_line() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let mut reader = JsonLinesReader::new(rx, 1024);
        tx.write_all(b"{\"type\":\"agent_end\",\"pad\":\"")
            .await
            .unwrap();
        // The line is incomplete: `next` waits for more data and is dropped (as the losing
        // branch of a `select!` would be).
        assert!(
            tokio::time::timeout(Duration::from_millis(50), reader.next())
                .await
                .is_err()
        );
        tx.write_all(b"xyz\"}\n").await.unwrap();
        assert_eq!(
            reader.next().await.unwrap(),
            Some(ReadLine::Json(
                serde_json::json!({"type": "agent_end", "pad": "xyz"})
            ))
        );
    }

    #[tokio::test]
    async fn a_cancelled_next_while_skipping_keeps_skipping() {
        let (mut tx, rx) = tokio::io::duplex(1024);
        let mut reader = JsonLinesReader::new(rx, 16);
        tx.write_all(&[b'y'; 40]).await.unwrap();
        assert!(matches!(
            reader.next().await,
            Err(LineError::TooLong { max: 16 })
        ));
        tx.write_all(&[b'y'; 40]).await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), reader.next())
                .await
                .is_err()
        );
        tx.write_all(b"yy\n{\"n\":2}\n").await.unwrap();
        assert_eq!(
            reader.next().await.unwrap(),
            Some(ReadLine::Json(serde_json::json!({"n": 2})))
        );
    }

    #[tokio::test]
    async fn writer_emits_one_line_per_value() {
        let mut buf = Vec::new();
        {
            let mut w = JsonLinesWriter::new(&mut buf);
            w.send(&serde_json::json!({"a": "x\ny"})).await.unwrap();
            w.send(&serde_json::json!([1, 2])).await.unwrap();
        }
        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "{\"a\":\"x\\ny\"}\n[1,2]\n"
        );
    }

    #[tokio::test]
    async fn shared_writer_writes_and_closes() {
        let (tx, mut rx) = tokio::io::duplex(1024);
        let writer = SharedJsonLinesWriter::new(tx);
        writer.send(&serde_json::json!({"a": 1})).await.unwrap();
        writer.close().await;
        assert!(writer.is_closed());
        assert!(matches!(
            writer.send(&serde_json::json!({"b": 2})).await,
            Err(WriteError::Closed)
        ));
        let mut out = String::new();
        rx.read_to_string(&mut out).await.unwrap();
        assert_eq!(
            out, "{\"a\":1}\n",
            "the reader sees EOF after the first value"
        );
    }

    #[tokio::test]
    async fn close_does_not_wait_for_a_write_stuck_on_a_full_pipe() {
        // Nobody reads the other end: a write larger than the pipe buffer never completes.
        let (tx, _rx) = tokio::io::duplex(64);
        let writer = std::sync::Arc::new(SharedJsonLinesWriter::new(tx));
        let stuck = {
            let writer = writer.clone();
            tokio::spawn(async move { writer.send(&"z".repeat(70 * 1024)).await })
        };
        // Let the write start and block.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!stuck.is_finished());
        tokio::time::timeout(Duration::from_secs(5), writer.close())
            .await
            .expect("close returned promptly");
        let result = tokio::time::timeout(Duration::from_secs(5), stuck)
            .await
            .expect("the stuck write was abandoned")
            .unwrap();
        assert!(matches!(result, Err(WriteError::Closed)));
    }
}
