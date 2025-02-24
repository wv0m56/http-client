//! http-client implementation for reqwest

use super::{Error, HttpClient, Request, Response};
use http_types::headers::{HeaderName, HeaderValue};
use http_types::StatusCode;
use hyper::body::{Body, HttpBody};
use hyper::client::{Builder, Client, HttpConnector};
use hyper_tls::HttpsConnector;
use std::convert::TryFrom;
use std::str::FromStr;
use std::sync::Arc;

/// Hyper-based HTTP Client.
#[derive(Debug)]
pub struct HyperClient {
    client: Arc<Client<HttpsConnector<HttpConnector>, Body>>,
}

impl HyperClient {
    /// Create a new default client.
    pub fn new() -> Self {
        HyperClient::with_builder_connector(Client::builder(), HttpsConnector::new())
    }

    /// Create a new client with custom hyper configs.
    pub fn with_builder_connector(
        builder: Builder,
        connector: HttpsConnector<HttpConnector>,
    ) -> Self {
        HyperClient {
            client: Arc::new(builder.build(connector)),
        }
    }
}

impl HttpClient for HyperClient {
    fn send(&self, req: Request) -> futures::future::BoxFuture<'static, Result<Response, Error>> {
        let client = self.client.clone();
        Box::pin(async move {
            let req = HyperHttpRequest::try_from(req).await?.into_inner();
            let response = client.request(req).await?;
            let resp = HttpTypesResponse::try_from(response).await?.into_inner();
            Ok(resp)
        })
    }
}

struct HyperHttpRequest {
    inner: hyper::Request<hyper::Body>,
}

impl HyperHttpRequest {
    async fn try_from(mut value: Request) -> Result<Self, Error> {
        // UNWRAP: This unwrap is unjustified in `http-types`, need to check if it's actually safe.
        let uri = hyper::Uri::try_from(&format!("{}", value.url())).unwrap();

        // `HyperClient` depends on the scheme being either "http" or "https"
        match uri.scheme_str() {
            Some("http") | Some("https") => (),
            _ => return Err(Error::from_str(StatusCode::BadRequest, "invalid scheme")),
        };

        let mut request = hyper::Request::builder();

        // UNWRAP: Default builder is safe
        let req_headers = request.headers_mut().unwrap();
        for (name, values) in &value {
            // UNWRAP: http-types and http have equivalent validation rules
            let name = hyper::header::HeaderName::from_str(name.as_str()).unwrap();

            for value in values.iter() {
                // UNWRAP: http-types and http have equivalent validation rules
                let value =
                    hyper::header::HeaderValue::from_bytes(value.as_str().as_bytes()).unwrap();
                req_headers.append(&name, value);
            }
        }

        let body = value.body_bytes().await?;
        let body = hyper::Body::from(body);

        let request = request
            .method(value.method())
            .version(value.version().map(|v| v.into()).unwrap_or_default())
            .uri(uri)
            .body(body)?;

        Ok(HyperHttpRequest { inner: request })
    }

    fn into_inner(self) -> hyper::Request<hyper::Body> {
        self.inner
    }
}

struct HttpTypesResponse {
    inner: Response,
}

impl HttpTypesResponse {
    async fn try_from(value: hyper::Response<hyper::Body>) -> Result<Self, Error> {
        let (parts, mut body) = value.into_parts();

        let body = match body.data().await {
            None => None,
            Some(Ok(b)) => Some(b),
            Some(Err(_)) => {
                return Err(Error::from_str(
                    StatusCode::BadGateway,
                    "unable to read HTTP response body",
                ))
            }
        }
        .map(|b| http_types::Body::from_bytes(b.to_vec()))
        .unwrap_or(http_types::Body::empty());

        let mut res = Response::new(parts.status);
        res.set_version(Some(parts.version.into()));

        for (name, value) in parts.headers {
            let value = value.as_bytes().to_owned();
            let value = HeaderValue::from_bytes(value)?;

            if let Some(name) = name {
                let name = name.as_str();
                let name = HeaderName::from_str(name)?;
                res.insert_header(name, value);
            }
        }

        res.set_body(body);
        Ok(HttpTypesResponse { inner: res })
    }

    fn into_inner(self) -> Response {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use crate::{Error, HttpClient};
    use http_types::{Method, Request, Url};
    use hyper::service::{make_service_fn, service_fn};
    use std::time::Duration;
    use tokio::sync::oneshot::channel;

    use super::HyperClient;

    async fn echo(
        req: hyper::Request<hyper::Body>,
    ) -> Result<hyper::Response<hyper::Body>, hyper::Error> {
        Ok(hyper::Response::new(req.into_body()))
    }

    #[tokio::test]
    async fn basic_functionality() {
        let (send, recv) = channel::<()>();

        let recv = async move { recv.await.unwrap_or(()) };

        let addr = ([127, 0, 0, 1], portpicker::pick_unused_port().unwrap()).into();
        let service = make_service_fn(|_| async { Ok::<_, hyper::Error>(service_fn(echo)) });
        let server = hyper::Server::bind(&addr)
            .serve(service)
            .with_graceful_shutdown(recv);

        let client = HyperClient::new();
        let url = Url::parse(&format!("http://localhost:{}", addr.port())).unwrap();
        let mut req = Request::new(Method::Get, url);
        req.set_body("hello");

        let client = async move {
            tokio::time::delay_for(Duration::from_millis(100)).await;
            let mut resp = client.send(req).await?;
            send.send(()).unwrap();
            assert_eq!(resp.body_string().await?, "hello");

            Result::<(), Error>::Ok(())
        };

        let (client_res, server_res) = tokio::join!(client, server);
        assert!(client_res.is_ok());
        assert!(server_res.is_ok());
    }
}
