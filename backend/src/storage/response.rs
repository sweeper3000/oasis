use super::{Materialized, Workspace};
use crate::service::range::ByteRange;
use rocket::{
    http::{ContentType, Status},
    response::{self, Responder},
    Request, Response,
};
use std::{
    io::SeekFrom,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    fs::File,
    io::{AsyncRead, AsyncReadExt, ReadBuf, Take},
};

pub struct StorageFile {
    materialized: Materialized,
    filename: String,
    range: Option<ByteRange>,
}

// The response body owns the temporary directory, including on client disconnect.
// Field order closes the file before removing its directory (also on Windows).
struct OwnedReader {
    file: Take<File>,
    _workspace: Option<Workspace>,
}
impl AsyncRead for OwnedReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_read(cx, buffer)
    }
}

impl StorageFile {
    pub fn new(materialized: Materialized, filename: String, range: Option<ByteRange>) -> Self {
        Self {
            materialized,
            filename,
            range,
        }
    }
}

impl<'r> Responder<'r, 'static> for StorageFile {
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'static> {
        // Rocket responders are synchronous: prepare a std file and seek before
        // handing ownership to Tokio's asynchronous response reader.
        let mut file = std::fs::File::open(&self.materialized.path)
            .map_err(|_| Status::InternalServerError)?;
        let size = file
            .metadata()
            .map_err(|_| Status::InternalServerError)?
            .len();
        let content_type = std::path::Path::new(&self.filename)
            .extension()
            .and_then(|e| e.to_str())
            .and_then(ContentType::from_extension)
            .unwrap_or(ContentType::Binary);
        let mut response = Response::build();
        response
            .header(content_type)
            .raw_header("Accept-Ranges", "bytes");
        let length = if let Some(range) = self.range {
            let (start, end) = match range.resolve(size) {
                Some(value) => value,
                None => {
                    return Response::build()
                        .status(Status::RangeNotSatisfiable)
                        .raw_header("Content-Range", format!("bytes */{size}"))
                        .ok()
                }
            };
            std::io::Seek::seek(&mut file, SeekFrom::Start(start))
                .map_err(|_| Status::InternalServerError)?;
            response
                .status(Status::PartialContent)
                .raw_header("Content-Range", format!("bytes {start}-{end}/{size}"));
            end - start + 1
        } else {
            size
        };
        let reader = OwnedReader {
            file: File::from_std(file).take(length),
            _workspace: self.materialized.workspace,
        };
        response
            .raw_header("Content-Length", length.to_string())
            .streamed_body(reader)
            .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::range::Range;
    use rocket::{local::asynchronous::Client, State};
    use std::{path::PathBuf, sync::Mutex};

    struct Observed(Mutex<Option<PathBuf>>);
    #[get("/data")]
    async fn data(observed: &State<Observed>, range: Range) -> StorageFile {
        let workspace = Workspace::new().unwrap();
        let path = workspace.path().join("plain");
        tokio::fs::write(&path, b"plaintext response")
            .await
            .unwrap();
        *observed.0.lock().unwrap() = Some(workspace.path().to_owned());
        StorageFile::new(
            Materialized {
                path,
                workspace: Some(workspace),
            },
            "report.txt".into(),
            range.range,
        )
    }

    #[rocket::async_test]
    async fn response_body_owns_staging_and_cleans_up_on_drop() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_lazy("sqlite::memory:")
            .unwrap();
        let state = crate::service::app_state::AppState::new(None, pool);
        state
            .set_storage_config(crate::storage::config::StorageConfig {
                enabled: true,
                ..Default::default()
            })
            .unwrap();
        let client = Client::tracked(
            rocket::build()
                .manage(state)
                .attach(crate::service::fairings::StaticFileCache)
                .manage(Observed(Mutex::new(None)))
                .mount("/api/file", routes![data]),
        )
        .await
        .unwrap();
        let response = client.get("/api/file/data").dispatch().await;
        assert_eq!(
            response.headers().get_one("Cache-Control"),
            Some("private, no-store")
        );
        let directory = client
            .rocket()
            .state::<Observed>()
            .unwrap()
            .0
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert!(directory.exists());
        drop(response); // Simulate a client leaving without consuming the body.
        assert!(!directory.exists());
        let response = client
            .get("/api/file/data")
            .header(rocket::http::Header::new("Range", "bytes=0-0"))
            .dispatch()
            .await;
        assert_eq!(response.status(), Status::PartialContent);
        assert_eq!(response.into_bytes().await.unwrap(), b"p");
        let directory = client
            .rocket()
            .state::<Observed>()
            .unwrap()
            .0
            .lock()
            .unwrap()
            .clone()
            .unwrap();
        assert!(!directory.exists());
    }
}
