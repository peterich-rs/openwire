//! Multipart form upload.
//!
//! Requires `--features multipart`.
//!
//! For a large payload, replace the file part with a one-shot stream:
//!
//! ```ignore
//! Part::stream_with_length(stream, known_len)
//!     .file_name("hello.txt")
//!     .mime_str("text/plain; charset=utf-8")?
//! ```
//!
//! A known length does not make the stream replayable.

use bytes::Bytes;
use http::Request;
use openwire::{
    multipart::{Form, Part},
    Client,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let request = Form::new()
        .text("description", "upload from OpenWire")
        .part(
            "file",
            Part::bytes(Bytes::from_static(b"hello\n"))
                .file_name("hello.txt")
                .mime_str("text/plain; charset=utf-8")?,
        )
        .into_request(
            Request::builder()
                .method("POST")
                .uri("https://example.com/upload"),
        )?;
    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
