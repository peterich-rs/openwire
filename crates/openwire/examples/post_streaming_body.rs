use bytes::Bytes;
use futures_util::stream;
use openwire::{Client, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let body = RequestBody::from_stream(stream::iter([
        Ok::<Bytes, openwire::WireError>(Bytes::from_static(b"chunk-1 ")),
        Ok::<Bytes, openwire::WireError>(Bytes::from_static(b"chunk-2")),
    ]));

    let response = client
        .post("http://example.com/upload")
        .body(body)
        .send()
        .await?;
    println!("status = {}", response.status());
    Ok(())
}
