use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use openwire::{BoxFuture, CallContext, Client, DnsResolver, WireError};

#[derive(Clone)]
struct LoopbackResolver;

impl DnsResolver for LoopbackResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        Box::pin(async move {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
            ctx.listener().dns_start(&ctx, &host, port);
            ctx.listener().dns_end(&ctx, &host, &[addr]);
            Ok(vec![addr])
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder().dns_resolver(LoopbackResolver).build()?;
    let response = client.get("http://openwire.local:8080/").send().await?;
    println!("status = {}", response.status());
    Ok(())
}
