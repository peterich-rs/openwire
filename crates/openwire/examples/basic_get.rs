use openwire::Client;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().build()?;
    let response = client.get("http://example.com/").send().await?;
    println!("status = {}", response.status());
    Ok(())
}
