use crate::future::BoxFuture;
use crate::plugins::telemetry::config::MetricsCommon;
use crate::plugins::telemetry::{MetricsBuilder, MetricsConfigurator};
use apollo_router_core::{http_compat, ResponseBody};
use bytes::Bytes;
use http::StatusCode;
use prometheus::{Encoder, Registry, TextEncoder};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::task::{Context, Poll};
use tower::{BoxError, ServiceExt};
use tower_service::Service;

#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    enabled: bool,
}

impl MetricsConfigurator for Config {
    fn apply(
        &self,
        mut builder: MetricsBuilder,
        _metrics_config: &MetricsCommon,
    ) -> Result<MetricsBuilder, BoxError> {
        if self.enabled {
            let exporter = opentelemetry_prometheus::exporter().try_init()?;
            builder = builder.with_custom_endpoint(
                "prometheus",
                PrometheusService {
                    registry: exporter.registry().clone(),
                }
                .boxed(),
            );
            builder = builder.with_exporter(exporter);
        }
        Ok(builder)
    }
}

#[derive(Clone)]
pub struct PrometheusService {
    registry: Registry,
}

impl Service<http_compat::Request<Bytes>> for PrometheusService {
    type Response = http_compat::Response<ResponseBody>;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Ok(()).into()
    }

    fn call(&mut self, _req: http_compat::Request<Bytes>) -> Self::Future {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut result = Vec::new();
        encoder.encode(&metric_families, &mut result).unwrap();

        Box::pin(async move {
            Ok(http_compat::Response {
                inner: http::Response::builder()
                    .status(StatusCode::OK)
                    .body(ResponseBody::Text(
                        String::from_utf8_lossy(&result).into_owned(),
                    ))
                    .map_err(|err| BoxError::from(err.to_string()))?,
            })
        })
    }
}