use bytes::Bytes;
use futures_util::stream;
use http::Request;
use openwire::{Client, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let body = RequestBody::from_stream(stream::iter([
        Ok::<Bytes, openwire::WireError>(Bytes::from_static(b"chunk-1 ")),
        Ok::<Bytes, openwire::WireError>(Bytes::from_static(b"chunk-2")),
    ]));

    let request = Request::builder()
        .method("POST")
        .uri("http://example.com/upload")
        .body(body)?;
    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
