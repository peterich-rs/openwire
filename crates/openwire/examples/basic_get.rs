use http::Request;
use openwire::{Client, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let request = Request::builder()
        .uri("http://example.com/")
        .body(RequestBody::empty())?;
    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
