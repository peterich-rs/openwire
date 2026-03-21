use http::header::AUTHORIZATION;
use http::Request;
use openwire::{AuthContext, Authenticator, BoxFuture, Client, RequestBody, WireError};

#[derive(Clone)]
struct BasicAuthenticator;

impl Authenticator for BasicAuthenticator {
    fn authenticate(
        &self,
        ctx: AuthContext,
    ) -> BoxFuture<Result<Option<Request<RequestBody>>, WireError>> {
        Box::pin(async move {
            let Some(mut request) = ctx.try_clone_request() else {
                return Ok(None);
            };
            request.headers_mut().insert(
                AUTHORIZATION,
                http::HeaderValue::from_static("Basic YWxpY2U6cGFzc3dvcmQ="),
            );
            Ok(Some(request))
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::builder()
        .authenticator(BasicAuthenticator)
        .build()?;

    let request = Request::builder()
        .uri("https://httpbin.org/basic-auth/alice/password")
        .body(RequestBody::empty())?;

    let response = client.execute(request).await?;
    println!("status = {}", response.status());
    Ok(())
}
