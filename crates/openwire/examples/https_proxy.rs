use http::Request;
use openwire::{Client, Proxy, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .proxy(Proxy::https("http://127.0.0.1:8080")?)
        .build()?;

    let request = Request::builder()
        .uri("https://example.com/")
        .body(RequestBody::empty())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
