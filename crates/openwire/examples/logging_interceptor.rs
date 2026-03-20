use std::sync::{Arc, Mutex};

use http::Request;
use openwire::{
    BoxFuture, Client, Exchange, Interceptor, Next, RequestBody, ResponseBody, WireError,
};

#[derive(Clone)]
struct LoggingInterceptor {
    shared: Arc<Mutex<Vec<String>>>,
}

impl Interceptor for LoggingInterceptor {
    fn intercept(
        &self,
        exchange: Exchange,
        next: Next,
    ) -> BoxFuture<Result<http::Response<ResponseBody>, WireError>> {
        let shared = self.shared.clone();
        let method = exchange.request().method().clone();
        let uri = exchange.request().uri().clone();
        Box::pin(async move {
            shared
                .lock()
                .expect("log lock")
                .push(format!("sending {method} {uri}"));
            let response = next.run(exchange).await?;
            shared
                .lock()
                .expect("log lock")
                .push(format!("received {}", response.status()));
            Ok(response)
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let logs = Arc::new(Mutex::new(Vec::new()));
    let client = Client::builder()
        .application_interceptor(LoggingInterceptor {
            shared: logs.clone(),
        })
        .build()?;

    let request = Request::builder()
        .uri("http://example.com/")
        .body(RequestBody::empty())?;

    let _ = client.execute(request).await?;
    for line in logs.lock().expect("log lock").iter() {
        println!("{line}");
    }
    Ok(())
}
