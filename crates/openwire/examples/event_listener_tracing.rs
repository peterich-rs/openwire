use http::Request;
use openwire::{Client, RequestBody};
use openwire_test::RecordingEventListenerFactory;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("openwire=debug"))
        .try_init();

    let events = RecordingEventListenerFactory::default();
    let client = Client::builder()
        .event_listener_factory(events.clone())
        .build()?;

    let request = Request::builder()
        .uri("http://example.com/")
        .body(RequestBody::empty())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    for event in events.events() {
        println!("{event}");
    }
    Ok(())
}
