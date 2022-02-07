use crate::configuration::{Configuration, Cors};
use crate::http_server_factory::{HttpServerFactory, HttpServerHandle};
use crate::FederatedServerError;
use apollo_router_core::prelude::*;
use futures::{channel::oneshot, prelude::*};
use http::Request;
use hyper::server::conn::Http;
use once_cell::sync::Lazy;
use opentelemetry::propagation::Extractor;
use std::pin::Pin;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tower::{BoxError, ServiceBuilder, ServiceExt};
use tower_http::trace::{DefaultMakeSpan, TraceLayer};
use tower_service::Service;
use tracing::instrument::WithSubscriber;
use tracing::{Instrument, Level, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use urlencoding::decode_binary;
use warp::host::Authority;
use warp::{
    http::{header::HeaderMap, StatusCode, Uri},
    hyper::{Body, Response},
    Filter,
};
use warp::{Rejection, Reply};

/// A basic http server using warp.
/// Uses streaming as primary method of response.
/// Redirects to studio for GET requests.
#[derive(Debug)]
pub(crate) struct WarpHttpServerFactory;

impl WarpHttpServerFactory {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl HttpServerFactory for WarpHttpServerFactory {
    type Future =
        Pin<Box<dyn Future<Output = Result<HttpServerHandle, FederatedServerError>> + Send>>;

    fn create<RS>(
        &self,
        service: RS,
        configuration: Arc<Configuration>,
        listener: Option<TcpListener>,
    ) -> Self::Future
    where
        RS: Service<
                Request<graphql::Request>,
                Response = Response<graphql::Response>,
                Error = BoxError,
            > + Send
            + Sync
            + Clone
            + 'static,

        <RS as Service<http::Request<apollo_router_core::Request>>>::Future: std::marker::Send,
    {
        Box::pin(async move {
            let (shutdown_sender, shutdown_receiver) = oneshot::channel::<()>();
            let listen_address = configuration.server.listen;

            let cors = configuration
                .server
                .cors
                .as_ref()
                .map(|cors_configuration| cors_configuration.into_warp_middleware())
                .unwrap_or_else(|| Cors::builder().build().into_warp_middleware());

            let dispatcher = configuration
                .subscriber
                .clone()
                .map(tracing::Dispatch::new)
                .unwrap_or_default();

            let routes = get_health_request()
                .or(get_graphql_request_or_redirect(service.clone()))
                .or(post_graphql_request(service.clone()))
                .with(cors);

            // generate a hyper service from warp routes
            let svc = warp::service(routes);

            let svc = ServiceBuilder::new()
                // generate a tracing span that covers request parsing and response serializing
                .layer(
                    TraceLayer::new_for_http()
                        .make_span_with(DefaultMakeSpan::new().level(Level::INFO)),
                )
                .service(svc);

            // if we received a TCP listener, reuse it, otherwise create a new one
            let tcp_listener = if let Some(listener) = listener {
                listener
            } else {
                TcpListener::bind(listen_address)
                    .await
                    .map_err(FederatedServerError::ServerCreationError)?
            };
            let actual_listen_address = tcp_listener
                .local_addr()
                .map_err(FederatedServerError::ServerCreationError)?;

            // this server reproduces most of hyper::server::Server's behaviour
            // we select over the stop_listen_receiver channel and the listener's
            // accept future. If the channel received something or the sender
            // was dropped, we stop using the listener and send it back through
            // listener_receiver
            let server = async move {
                tokio::pin!(shutdown_receiver);

                let connection_shutdown = Arc::new(Notify::new());

                loop {
                    tokio::select! {
                        _ = &mut shutdown_receiver => {
                            break;
                        }
                        res = tcp_listener.accept() => {
                            let svc = svc.clone();
                            let connection_shutdown = connection_shutdown.clone();

                            tokio::task::spawn(async move {
                                // we unwrap the result of accept() here to avoid stopping
                                // the entire server on an issue with that socket
                                // Unfortunately, the error here could also be linked
                                // to the listen socket (no RAM for kernel buffers, no
                                // more file descriptors, network interface is down...)
                                // ideally we'd want to handle the errors in the server task
                                // with varying behaviours
                                let (tcp_stream, _) = res.unwrap();
                                tcp_stream.set_nodelay(true).expect("this should not fail unless the socket is invalid");

                                let connection = Http::new()
                                    .http1_keep_alive(true)
                                    .serve_connection(tcp_stream, svc);

                                tokio::pin!(connection);
                                tokio::select! {
                                    // the connection finished first
                                    _res = &mut connection => {
                                        /*if let Err(http_err) = res {
                                            tracing::error!(
                                                "Error while serving HTTP connection: {}",
                                                http_err
                                            );
                                        }*/
                                    }
                                    // the shutdown receiver was triggered first,
                                    // so we tell the connection to do a graceful shutdown
                                    // on the next request, then we wait for it to finish
                                    _ = connection_shutdown.notified() => {
                                        let c = connection.as_mut();
                                        c.graceful_shutdown();

                                        if let Err(_http_err) = connection.await {
                                            /*tracing::error!(
                                                "Error while serving HTTP connection: {}",
                                                http_err
                                            );*/
                                        }
                                    }
                                }
                            }.with_subscriber(dispatcher.clone()));
                        }
                    }
                }

                // the shutdown receiver was triggered so we break out of
                // the server loop, tell the currently active connections to stop
                // then return the TCP listen socket
                connection_shutdown.notify_waiters();
                tcp_listener
            };

            // Spawn the server into a runtime
            let server_future = tokio::task::spawn(server)
                .map_err(|_| FederatedServerError::HttpServerLifecycleError)
                .boxed();

            Ok(HttpServerHandle::new(
                shutdown_sender,
                server_future,
                actual_listen_address,
            ))
        })
    }
}

fn get_graphql_request_or_redirect<RS>(
    service: RS,
) -> impl Filter<Extract = (Box<dyn Reply>,), Error = Rejection> + Clone
where
    RS: Service<Request<graphql::Request>, Response = Response<graphql::Response>, Error = BoxError>
        + Send
        + Clone
        + 'static,
    <RS as Service<http::Request<apollo_router_core::Request>>>::Future: std::marker::Send,
{
    warp::get()
        .and(warp::path::end().or(warp::path("graphql")).unify())
        .and(warp::header::optional::<String>("accept"))
        .and(warp::host::optional())
        .and(warp::query::raw())
        .and(warp::header::headers_cloned())
        .and_then(
            move |accept: Option<String>,
                  host: Option<Authority>,
                  query: String,
                  header_map: HeaderMap| {
                let service = service.clone();
                async move {
                    let reply: Box<dyn Reply> = if accept.map(prefers_html).unwrap_or_default() {
                        redirect_to_studio(host)
                    } else {
                        // decode percent encoded string
                        // from the docs `Unencoded `+` is preserved literally, and _not_ changed to a space.`,
                        // so let's do it I guess
                        let query = query.replace('+', " ");
                        let decoded_query = decode_binary(query.as_bytes());
                        if let Ok(request) = serde_json::from_slice(&decoded_query) {
                            run_graphql_request(service, http::Method::GET, request, header_map)
                                .await
                        } else {
                            Box::new(warp::reply::with_status(
                                "Invalid url encoded GraphQL request",
                                StatusCode::BAD_REQUEST,
                            ))
                        }
                    };

                    Ok::<_, warp::reject::Rejection>(reply)
                }
            },
        )
}

fn redirect_to_studio(host: Option<Authority>) -> Box<dyn Reply> {
    // Try to redirect to Studio
    if host.is_some() {
        if let Ok(uri) = format!(
            "https://studio.apollographql.com/sandbox?endpoint=http://{}",
            // we made sure host.is_some() above
            host.unwrap()
        )
        .parse::<Uri>()
        {
            Box::new(warp::redirect::temporary(uri))
        } else {
            Box::new(warp::reply::with_status(
                "Invalid host to redirect to",
                StatusCode::BAD_REQUEST,
            ))
        }
    } else {
        Box::new(warp::reply::with_status(
            "Invalid host to redirect to",
            StatusCode::BAD_REQUEST,
        ))
    }
}

fn get_health_request() -> impl Filter<Extract = (Box<dyn Reply>,), Error = Rejection> + Clone {
    warp::get()
        .and(warp::path(".well-known"))
        .and(warp::path("apollo"))
        .and(warp::path("server-health"))
        .and_then(move || async {
            static RESULT: Lazy<serde_json::Value> =
                Lazy::new(|| serde_json::json!({"status": "pass"}));

            let reply = Box::new(warp::reply::json(&*RESULT)) as Box<dyn Reply>;
            Ok::<_, Rejection>(reply)
        })
}

fn post_graphql_request<RS>(
    service: RS,
) -> impl Filter<Extract = (Box<dyn Reply>,), Error = Rejection> + Clone
where
    RS: Service<Request<graphql::Request>, Response = Response<graphql::Response>, Error = BoxError>
        + Send
        + Clone
        + 'static,
    <RS as Service<http::Request<apollo_router_core::Request>>>::Future: std::marker::Send,
{
    warp::post()
        .and(warp::path::end().or(warp::path("graphql")).unify())
        .and(warp::body::json())
        .and(warp::header::headers_cloned())
        .and_then(move |request: graphql::Request, header_map: HeaderMap| {
            let service = service.clone();
            async move {
                let reply =
                    run_graphql_request(service, http::Method::POST, request, header_map).await;
                Ok::<_, warp::reject::Rejection>(reply)
            }
        })
}

fn run_graphql_request<RS>(
    service: RS,
    method: http::Method,
    request: graphql::Request,
    header_map: HeaderMap,
) -> impl Future<Output = Box<dyn Reply>> + Send
where
    RS: Service<Request<graphql::Request>, Response = Response<graphql::Response>, Error = BoxError>
        + Send
        + Clone
        + 'static,
    <RS as Service<http::Request<apollo_router_core::Request>>>::Future: std::marker::Send,
{
    // retrieve and reuse the potential trace id from the caller
    opentelemetry::global::get_text_map_propagator(|injector| {
        injector.extract_with_context(&Span::current().context(), &HeaderMapCarrier(&header_map));
    });

    async move {
        match service.ready_oneshot().await {
            Ok(service) => {
                let service = service.clone();
                let mut http_request = http::Request::builder()
                    .method(method)
                    .body(request)
                    .unwrap();
                *http_request.headers_mut() = header_map;

                let response = stream_request(service, http_request)
                    .instrument(tracing::info_span!("graphql_request"))
                    .await;

                Box::new(Response::new(Body::from(response))) as Box<dyn Reply>
            }
            Err(_) => Box::new(warp::reply::with_status(
                "Invalid host to redirect to",
                StatusCode::BAD_REQUEST,
            )),
        }
    }
}

async fn stream_request<RS>(service: RS, request: Request<graphql::Request>) -> String
where
    RS: Service<Request<graphql::Request>, Response = Response<graphql::Response>, Error = BoxError>
        + Send
        + Clone
        + 'static,
{
    match service.oneshot(request).await {
        Err(_) => String::new(),
        Ok(response) => {
            let span = Span::current();
            // TODO headers
            tracing::debug_span!(parent: &span, "serialize_response").in_scope(|| {
                serde_json::to_string(response.body())
                    .expect("serde_json::Value serialization will not fail")
            })
        }
    }
}

fn prefers_html(accept_header: String) -> bool {
    accept_header
        .split(',')
        .map(|a| a.trim())
        .any(|a| a == "text/html")
}

struct HeaderMapCarrier<'a>(&'a HeaderMap);

impl<'a> Extractor for HeaderMapCarrier<'a> {
    fn get(&self, key: &str) -> Option<&str> {
        if let Some(value) = self.0.get(key).and_then(|x| x.to_str().ok()) {
            tracing::trace!(
                "found OpenTelemetry key in user's request: {}={}",
                key,
                value
            );
            Some(value)
        } else {
            None
        }
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|x| x.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::configuration::Cors;
    use mockall::mock;
    use reqwest::header::{
        ACCEPT, ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
        ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_REQUEST_HEADERS, ACCESS_CONTROL_REQUEST_METHOD,
        LOCATION, ORIGIN,
    };
    use reqwest::redirect::Policy;
    use reqwest::{Client, Method, StatusCode};
    use serde_json::json;
    use std::net::SocketAddr;
    use std::str::FromStr;
    use test_log::test;

    macro_rules! assert_header {
        ($response:expr, $header:expr, $expected:expr $(, $msg:expr)?) => {
            assert_eq!(
                $response
                    .headers()
                    .get_all($header)
                    .iter()
                    .map(|v|v.to_str().unwrap().to_string())
                    .collect::<Vec<_>>(),
                $expected
                $(, $msg)*
            );
        };
    }

    /// `assert_header_contains` works like `assert_headers`,
    /// except it doesn't care for the order of the items
    macro_rules! assert_header_contains {
        ($response:expr, $header:expr, $expected:expr $(, $msg:expr)?) => {
            let header_values = $response
            .headers()
            .get_all($header)
            .iter()
            .map(|v|v.to_str().unwrap().to_string())
            .collect::<Vec<_>>();

            for e in $expected {
                assert!(
                    header_values
                        .iter()
                        .find(|header_value| header_value.contains(&e.to_string()))
                        .is_some(),
                    $($msg)*
                );
            }

        };
    }

    mock! {
        #[derive(Debug)]
        RouterService {
            fn service_call(&mut self, req: http::Request<graphql::Request>) -> Result<Response<graphql::Response>, BoxError>;
        }
    }

    async fn init(mut mock: MockRouterService) -> (HttpServerHandle, Client) {
        let server_factory = WarpHttpServerFactory::new();
        let (service, mut handle) = tower_test::mock::spawn();

        tokio::spawn(async move {
            loop {
                while let Some((request, responder)) = handle.next_request().await {
                    match mock.service_call(request) {
                        Ok(response) => responder.send_response(response),
                        Err(err) => responder.send_error(err),
                    }
                }
            }
        });
        let server = server_factory
            .create(
                service.into_inner(),
                Arc::new(
                    Configuration::builder()
                        .server(
                            crate::configuration::Server::builder()
                                .listen(SocketAddr::from_str("127.0.0.1:0").unwrap())
                                .cors(Some(
                                    Cors::builder()
                                        .origins(vec!["http://studio".to_string()])
                                        .build(),
                                ))
                                .build(),
                        )
                        .subgraphs(Default::default())
                        .build(),
                ),
                None,
            )
            .await
            .expect("Failed to create server factory");
        let client = reqwest::Client::builder()
            .redirect(Policy::none())
            .build()
            .unwrap();
        (server, client)
    }

    #[test(tokio::test)]
    async fn redirect_to_studio() -> Result<(), FederatedServerError> {
        let expectations = MockRouterService::new();
        let (server, client) = init(expectations).await;

        for url in vec![
            format!("http://{}/", server.listen_address()),
            format!("http://{}/graphql", server.listen_address()),
        ] {
            // Regular studio redirect
            let response = client
                .get(url.as_str())
                .header(ACCEPT, "text/html")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::TEMPORARY_REDIRECT,
                "{}",
                response.text().await.unwrap()
            );
            assert_header!(
                &response,
                LOCATION,
                vec![format!(
                    "https://studio.apollographql.com/sandbox?endpoint=http://{}",
                    server.listen_address()
                )],
                "Incorrect redirect url"
            );

            // application/json, but the query body is empty
            let response = client
                .get(url.as_str())
                .header(ACCEPT, "application/json")
                .send()
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "{}",
                response.text().await.unwrap(),
            );
        }

        server.shutdown().await
    }

    #[test(tokio::test)]
    async fn malformed_request() -> Result<(), FederatedServerError> {
        let expectations = MockRouterService::new();
        let (server, client) = init(expectations).await;
        let response = client
            .post(format!("http://{}/graphql", server.listen_address()))
            .body("Garbage")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        server.shutdown().await
    }

    #[test(tokio::test)]
    async fn response() -> Result<(), FederatedServerError> {
        let expected_response = graphql::Response::builder()
            .data(json!({"response": "yay"}))
            .build();
        let example_response = expected_response.clone();
        let mut expectations = MockRouterService::new();
        expectations
            .expect_service_call()
            .times(2)
            .returning(move |_| {
                let example_response = example_response.clone();
                Ok(http::Response::builder()
                    .status(200)
                    .body(example_response)
                    .unwrap())
            });
        let (server, client) = init(expectations).await;
        let url = format!("http://{}/graphql", server.listen_address());
        // Post query
        let response = client
            .post(url.as_str())
            .body(json!({ "query": "query" }).to_string())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        assert_eq!(
            response.json::<graphql::Response>().await.unwrap(),
            expected_response,
        );

        // Get query
        let response = client
            .get(url.as_str())
            .body(json!({ "query": "query" }).to_string())
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();

        assert_eq!(
            response.json::<graphql::Response>().await.unwrap(),
            expected_response,
        );

        server.shutdown().await
    }

    #[test(tokio::test)]
    async fn response_failure() -> Result<(), FederatedServerError> {
        let mut expectations = MockRouterService::new();
        expectations
            .expect_service_call()
            .times(1)
            .returning(move |_| {
                let example_response = graphql::FetchError::SubrequestHttpError {
                    service: "Mock service".to_string(),
                    reason: "Mock error".to_string(),
                }
                .to_response(true);
                Ok(http::Response::builder()
                    .status(200)
                    .body(example_response)
                    .unwrap())
            });
        let (server, client) = init(expectations).await;

        let response = client
            .post(format!("http://{}/graphql", server.listen_address()))
            .body(
                json!(
                {
                  "query": "query",
                })
                .to_string(),
            )
            .send()
            .await
            .unwrap()
            .json::<graphql::Response>()
            .await
            .unwrap();

        assert_eq!(
            response,
            graphql::FetchError::SubrequestHttpError {
                service: "Mock service".to_string(),
                reason: "Mock error".to_string(),
            }
            .to_response(true)
        );
        server.shutdown().await
    }

    #[test(tokio::test)]
    async fn cors_preflight() -> Result<(), FederatedServerError> {
        let expectations = MockRouterService::new();
        let (server, client) = init(expectations).await;

        for url in vec![
            format!("http://{}/", server.listen_address()),
            format!("http://{}/graphql", server.listen_address()),
        ] {
            let response = client
                .request(Method::OPTIONS, &url)
                .header(ACCEPT, "text/html")
                .header(ORIGIN, "http://studio")
                .header(ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(ACCESS_CONTROL_REQUEST_HEADERS, "Content-type")
                .send()
                .await
                .unwrap();

            assert_header!(
                &response,
                ACCESS_CONTROL_ALLOW_ORIGIN,
                vec!["http://studio"],
                "Incorrect access control allow origin header"
            );
            assert_header_contains!(
                &response,
                ACCESS_CONTROL_ALLOW_HEADERS,
                &["content-type"],
                "Incorrect access control allow header header"
            );
            assert_header_contains!(
                &response,
                ACCESS_CONTROL_ALLOW_METHODS,
                &["GET", "POST", "OPTIONS"],
                "Incorrect access control allow methods header"
            );

            assert_eq!(response.status(), StatusCode::OK);
        }

        server.shutdown().await
    }

    #[test(tokio::test)]
    async fn test_health_check() {
        let filter = get_health_request();

        let res = warp::test::request()
            .path("/.well-known/apollo/server-health")
            .reply(&filter)
            .await;

        insta::assert_debug_snapshot!(res);
    }
}
