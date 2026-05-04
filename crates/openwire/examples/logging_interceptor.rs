use http::Request;
use openwire::{Client, LogLevel, LoggerInterceptor, RequestBody};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .application_interceptor(LoggerInterceptor::new(LogLevel::Body))
        .build()?;

    let request = Request::builder()
        .method("POST")
        .uri("https://api.example.com/users")
        .header("content-type", "application/json")
        .header("authorization", "Bearer secret")
        .body(RequestBody::from_static(br#"{"name":"Alice","age":18}"#))?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
