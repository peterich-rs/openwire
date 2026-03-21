use http::Request;
use openwire::{Client, Jar, RequestBody, Url};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let jar = Jar::default();
    let url = "https://example.com/".parse::<Url>()?;
    jar.add_cookie_str("session=demo; Path=/", &url);

    let client = Client::builder().cookie_jar(jar).build()?;
    let request = Request::builder()
        .uri("https://example.com/")
        .body(RequestBody::empty())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
