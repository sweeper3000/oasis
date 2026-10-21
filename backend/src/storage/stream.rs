//! Range-aware video delivery, with no file staging or persistent buffers.
use super::{
    config::Provider,
    crypto::{self, FileDecoder, CHUNK, HEADER},
    remote::{Remote, RemoteReader},
};
use crate::service::range::ByteRange;
use anyhow::{ensure, Result};
use rocket::{
    http::Status,
    response::{self, Responder},
    Request, Response,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, ReadBuf},
    sync::mpsc,
    task::JoinHandle,
};

pub struct VideoResponse {
    size: u64,
    bounds: Option<(u64, u64)>,
    partial: bool,
    invalid: bool,
    filename: String,
    body: Option<VideoBody>,
}

struct VideoBody {
    rx: mpsc::Receiver<io::Result<Vec<u8>>>,
    buffer: Vec<u8>,
    position: usize,
    producer: JoinHandle<()>,
}
impl Drop for VideoBody {
    fn drop(&mut self) {
        self.producer.abort();
    }
}
impl AsyncRead for VideoBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            if self.position < self.buffer.len() {
                let n = output.remaining().min(self.buffer.len() - self.position);
                output.put_slice(&self.buffer[self.position..self.position + n]);
                self.position += n;
                return Poll::Ready(Ok(()));
            }
            match self.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Ready(Some(Err(error))) => return Poll::Ready(Err(error)),
                Poll::Ready(Some(Ok(bytes))) => {
                    self.buffer = bytes;
                    self.position = 0;
                }
            }
        }
    }
}

struct Segments {
    reader: RemoteReader,
    decoder: Option<FileDecoder>,
    index: u64,
    last: u64,
    skip: usize,
    remaining: u64,
}
impl Segments {
    async fn next(&mut self) -> Result<Vec<u8>> {
        let mut plain = if let Some(decoder) = &self.decoder {
            let mut encrypted = vec![0; decoder.chunk_size(self.index) + 16];
            self.reader.read_exact(&mut encrypted).await?;
            let plain = decoder.decrypt(self.index, &encrypted)?;
            self.index += 1;
            // Exact chunk multiples have an additional authenticated empty final
            // chunk. Check it before releasing the last requested plaintext.
            if self.index == self.last
                && self.last == decoder.last_index()
                && decoder.chunk_size(self.last) == 0
            {
                let mut terminal = [0; 16];
                self.reader.read_exact(&mut terminal).await?;
                decoder.decrypt(self.last, &terminal)?;
                self.index += 1;
            }
            plain
        } else {
            let mut bytes = vec![0; self.remaining.min(CHUNK as u64) as usize];
            self.reader.read_exact(&mut bytes).await?;
            bytes
        };
        let skip = self.skip.min(plain.len());
        self.skip = 0;
        plain.drain(..skip);
        plain.truncate(self.remaining.min(plain.len() as u64) as usize);
        self.remaining -= plain.len() as u64;
        if self.remaining == 0 {
            // Hold the final output until both EOF and process success are known.
            self.reader.finish().await?;
        }
        Ok(plain)
    }
}

impl VideoResponse {
    pub fn metadata(size: u64, filename: &str, range: Option<ByteRange>) -> Self {
        let bounds = range.map(|range| range.resolve(size)).unwrap_or_else(|| {
            if size == 0 {
                None
            } else {
                Some((0, size - 1))
            }
        });
        Self {
            size,
            bounds,
            partial: range.is_some(),
            invalid: range.is_some() && bounds.is_none(),
            filename: filename.into(),
            body: None,
        }
    }

    pub async fn open(
        provider: Provider,
        relative: &str,
        filename: &str,
        range: Option<ByteRange>,
        head: bool,
    ) -> Result<Self> {
        let key = provider.key()?;
        let remote = Remote::new(provider).await?;
        let metadata = remote.metadata(relative).await?;
        let size = if key.is_some() {
            crypto::plaintext_size(metadata.size)?
        } else {
            metadata.size
        };
        let mut response = Self::metadata(size, filename, range);
        if head || response.invalid || (size == 0 && key.is_none()) {
            return Ok(response);
        }
        let (start, end) = response.bounds.unwrap_or((0, 0));
        let length = if size == 0 { 0 } else { end - start + 1 };
        let (decoder, offset, count, index, last, skip) = if let Some(key) = key {
            let mut header_reader = remote.open_range(relative, 0, HEADER as u64).await?;
            let mut header = [0; HEADER];
            header_reader.read_exact(&mut header).await?;
            header_reader.finish().await?;
            let decoder = FileDecoder::new(&key, header, metadata.size)?;
            let index = start / CHUNK as u64;
            let last = if size == 0 || end == size - 1 {
                decoder.last_index()
            } else {
                end / CHUNK as u64
            };
            let offset = HEADER as u64 + index * (CHUNK as u64 + 16);
            let stop =
                HEADER as u64 + last * (CHUNK as u64 + 16) + decoder.chunk_size(last) as u64 + 16;
            (
                Some(decoder),
                offset,
                stop - offset,
                index,
                last,
                (start % CHUNK as u64) as usize,
            )
        } else {
            (None, start, length, 0, 0, 0)
        };
        let reader = remote.open_range(relative, offset, count).await?;
        let mut segments = Segments {
            reader,
            decoder,
            index,
            last,
            skip,
            remaining: length,
        };
        let first = segments.next().await?;
        ensure!(
            size == 0 || !first.is_empty(),
            "Remote video returned no data"
        );
        if size == 0 {
            return Ok(response);
        }
        let (tx, rx) = mpsc::channel(1);
        let producer = tokio::spawn(async move {
            if tx.send(Ok(first)).await.is_err() {
                return;
            }
            while segments.remaining > 0 {
                match segments.next().await {
                    Ok(bytes) => {
                        if tx.send(Ok(bytes)).await.is_err() {
                            return;
                        }
                    }
                    Err(_) => {
                        let _ = tx
                            .send(Err(io::Error::new(
                                io::ErrorKind::Other,
                                "Remote video stream failed authentication or transfer",
                            )))
                            .await;
                        return;
                    }
                }
            }
        });
        response.body = Some(VideoBody {
            rx,
            buffer: Vec::new(),
            position: 0,
            producer,
        });
        Ok(response)
    }
}

impl<'r> Responder<'r, 'static> for VideoResponse {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        let mut response = Response::build();
        response
            .header(super::response::content_type(&self.filename))
            .raw_header("Accept-Ranges", "bytes");
        if self.invalid {
            return response
                .status(Status::RangeNotSatisfiable)
                .raw_header("Content-Range", format!("bytes */{}", self.size))
                .ok();
        }
        let length = self.bounds.map(|(start, end)| end - start + 1).unwrap_or(0);
        if self.partial {
            let (start, end) = self.bounds.unwrap();
            response.status(Status::PartialContent).raw_header(
                "Content-Range",
                format!("bytes {start}-{end}/{}", self.size),
            );
        }
        response.raw_header("Content-Length", length.to_string());
        if let Some(body) = self.body {
            response.streamed_body(body);
        } else {
            // Rocket strips HEAD bodies while retaining their declared size.
            // A header alone with an empty unsized body is rejected by Hyper.
            response.sized_body(length as usize, std::io::Cursor::new(Vec::<u8>::new()));
        }
        response.ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocket::local::asynchronous::Client;

    #[head("/video")]
    fn head(range: crate::service::range::Range) -> VideoResponse {
        VideoResponse::metadata(123456, "clip.webm", range.range)
    }

    #[rocket::async_test]
    async fn head_retains_plaintext_size_without_a_body() {
        let client = Client::tracked(rocket::build().mount("/", routes![head]))
            .await
            .unwrap();
        let response = client.head("/video").dispatch().await;
        assert_eq!(response.status(), Status::Ok);
        assert_eq!(response.headers().get_one("Content-Length"), Some("123456"));
        assert_eq!(
            response.headers().get_one("Content-Type"),
            Some("video/webm")
        );
        assert!(response.into_bytes().await.unwrap_or_default().is_empty());
        let response = client
            .head("/video")
            .header(rocket::http::Header::new("Range", "bytes=-10"))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::PartialContent);
        assert_eq!(
            response.headers().get_one("Content-Range"),
            Some("bytes 123446-123455/123456")
        );
        assert_eq!(response.headers().get_one("Content-Length"), Some("10"));
    }

    #[tokio::test]
    async fn dropping_response_cancels_its_producer() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        struct Alive(Arc<AtomicBool>);
        impl Drop for Alive {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let alive = Arc::new(AtomicBool::new(true));
        let observer = Alive(alive.clone());
        let (tx, rx) = mpsc::channel(1);
        let producer = tokio::spawn(async move {
            let _alive = observer;
            loop {
                if tx.send(Ok(vec![0; CHUNK])).await.is_err() {
                    break;
                }
            }
        });
        let mut body = VideoBody {
            rx,
            buffer: vec![],
            position: 0,
            producer,
        };
        let mut byte = [0];
        tokio::io::AsyncReadExt::read_exact(&mut body, &mut byte)
            .await
            .unwrap();
        drop(body);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while alive.load(Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("Stream producer survived disconnect");
    }
}
