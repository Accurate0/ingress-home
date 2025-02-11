use askama::Template;
use axum::{
    extract::{MatchedPath, State},
    http::{Request, StatusCode},
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use itertools::Itertools;
use k8s_openapi::api::networking::v1::Ingress;
use kube::{api::ListParams, Api, ResourceExt};
use tower_http::{
    services::ServeDir,
    trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer},
    LatencyUnit,
};
use tracing::Level;

struct Grouped {
    name: String,
    urls: Vec<String>,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    grouped_urls: Vec<Grouped>,
}

#[derive(Clone)]
struct AppState {
    client: kube::Client,
}

pub enum AppError {
    Error(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        match self {
            AppError::Error(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Something went wrong: {}", e),
            )
                .into_response(),
        }
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::Error(err.into())
    }
}

// basic handler that responds with a static string
async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let ingress: Api<Ingress> = Api::all(state.client);
    let params = ListParams::default();
    let all_ingress = ingress.list(&params).await?;

    let url_namespace_tuple = all_ingress.into_iter().map(|ingress| {
        let urls = ingress
            .spec
            .as_ref()
            .and_then(|ingress_spec| {
                ingress_spec.rules.as_ref().map(|ingress_rules| {
                    ingress_rules
                        .iter()
                        .map(|ingress_rule| ingress_rule.host.iter().cloned())
                        .collect_vec()
                })
            })
            .into_iter()
            .flatten()
            .flatten()
            .collect_vec();

        let namespace = ingress.namespace().unwrap_or("default".to_string());

        (namespace, urls)
    });

    tracing::info!("found {} ingress resources", url_namespace_tuple.len());

    let mut urls = url_namespace_tuple
        .into_group_map_by(|(namespace, _)| namespace.to_string())
        .iter()
        .map(|(k, v)| Grouped {
            name: k.to_string(),
            urls: v
                .iter()
                .flat_map(|u| u.1.iter().to_owned())
                .map(|s| s.to_string())
                .collect_vec(),
        })
        .collect_vec();

    urls.sort_by_key(|g| g.name.clone());

    let index_template = IndexTemplate { grouped_urls: urls };

    Ok(axum::response::Html(index_template.render()?))
}

async fn health() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    tracing::info!("started ingress-home");

    let trace_layer = TraceLayer::new_for_http()
        .make_span_with(|request: &Request<_>| {
            let matched_path = request
                .extensions()
                .get::<MatchedPath>()
                .map(MatchedPath::as_str)
                .unwrap_or_else(|| request.uri().path());

            tracing::info_span!("request", uri = matched_path)
        })
        .on_request(DefaultOnRequest::new().level(Level::INFO))
        .on_response(
            DefaultOnResponse::new()
                .level(Level::INFO)
                .latency_unit(LatencyUnit::Millis),
        );

    let client = kube::Client::try_default().await?;

    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .nest_service("/static/css", ServeDir::new("static/css"))
        .layer(trace_layer)
        .with_state(AppState { client });

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;

    Ok(())
}
