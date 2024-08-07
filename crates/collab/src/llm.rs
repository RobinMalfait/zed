mod authorization;
pub mod db;
mod token;

use crate::api::CloudflareIpCountryHeader;
use crate::llm::authorization::authorize_access_to_language_model;
use crate::llm::db::LlmDatabase;
use crate::{executor::Executor, Config, Error, Result};
use anyhow::{anyhow, Context as _};
use axum::TypedHeader;
use axum::{
    body::Body,
    http::{self, HeaderName, HeaderValue, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Extension, Json, Router,
};
use chrono::Utc;
use futures::StreamExt as _;
use http_client::IsahcHttpClient;
use rpc::{LanguageModelProvider, PerformCompletionParams, EXPIRED_LLM_TOKEN_HEADER_NAME};
use std::sync::Arc;
use util::ResultExt;

pub use token::*;

pub struct LlmState {
    pub config: Config,
    pub executor: Executor,
    pub db: Option<Arc<LlmDatabase>>,
    pub http_client: IsahcHttpClient,
}

impl LlmState {
    pub async fn new(config: Config, executor: Executor) -> Result<Arc<Self>> {
        // TODO: This is temporary until we have the LLM database stood up.
        let db = if config.is_development() {
            let database_url = config
                .llm_database_url
                .as_ref()
                .ok_or_else(|| anyhow!("missing LLM_DATABASE_URL"))?;
            let max_connections = config
                .llm_database_max_connections
                .ok_or_else(|| anyhow!("missing LLM_DATABASE_MAX_CONNECTIONS"))?;

            let mut db_options = db::ConnectOptions::new(database_url);
            db_options.max_connections(max_connections);
            let mut db = LlmDatabase::new(db_options, executor.clone()).await?;
            db.initialize().await?;

            Some(Arc::new(db))
        } else {
            None
        };

        let user_agent = format!("Zed Server/{}", env!("CARGO_PKG_VERSION"));
        let http_client = IsahcHttpClient::builder()
            .default_header("User-Agent", user_agent)
            .build()
            .context("failed to construct http client")?;

        let this = Self {
            config,
            executor,
            db,
            http_client,
        };

        Ok(Arc::new(this))
    }
}

pub fn routes() -> Router<(), Body> {
    Router::new()
        .route("/completion", post(perform_completion))
        .layer(middleware::from_fn(validate_api_token))
}

async fn validate_api_token<B>(mut req: Request<B>, next: Next<B>) -> impl IntoResponse {
    let token = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| {
            Error::http(
                StatusCode::BAD_REQUEST,
                "missing authorization header".to_string(),
            )
        })?
        .strip_prefix("Bearer ")
        .ok_or_else(|| {
            Error::http(
                StatusCode::BAD_REQUEST,
                "invalid authorization header".to_string(),
            )
        })?;

    let state = req.extensions().get::<Arc<LlmState>>().unwrap();
    match LlmTokenClaims::validate(&token, &state.config) {
        Ok(claims) => {
            req.extensions_mut().insert(claims);
            Ok::<_, Error>(next.run(req).await.into_response())
        }
        Err(ValidateLlmTokenError::Expired) => Err(Error::Http(
            StatusCode::UNAUTHORIZED,
            "unauthorized".to_string(),
            [(
                HeaderName::from_static(EXPIRED_LLM_TOKEN_HEADER_NAME),
                HeaderValue::from_static("true"),
            )]
            .into_iter()
            .collect(),
        )),
        Err(_err) => Err(Error::http(
            StatusCode::UNAUTHORIZED,
            "unauthorized".to_string(),
        )),
    }
}

async fn perform_completion(
    Extension(state): Extension<Arc<LlmState>>,
    Extension(claims): Extension<LlmTokenClaims>,
    country_code_header: Option<TypedHeader<CloudflareIpCountryHeader>>,
    Json(params): Json<PerformCompletionParams>,
) -> Result<impl IntoResponse> {
    let model = normalize_model_name(params.provider, params.model);

    authorize_access_to_language_model(
        &state.config,
        &claims,
        country_code_header.map(|header| header.to_string()),
        params.provider,
        &model,
    )?;

    let user_id = claims.user_id as i32;

    let db = state.db.clone();
    if let Some(db) = &db {
        check_usage_limit(&db, params.provider, &model, &claims).await?;
    }

    match params.provider {
        LanguageModelProvider::Anthropic => {
            let api_key = state
                .config
                .anthropic_api_key
                .as_ref()
                .context("no Anthropic AI API key configured on the server")?;
            let chunks = anthropic::stream_completion(
                &state.http_client,
                anthropic::ANTHROPIC_API_URL,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
                None,
            )
            .await?;

            let mut recorder = db.map(|db| UsageRecorder {
                db,
                executor: state.executor.clone(),
                user_id,
                provider: params.provider,
                model,
                token_count: 0,
            });

            let stream = chunks.map(move |event| {
                let mut buffer = Vec::new();
                event.map(|chunk| {
                    match &chunk {
                        anthropic::Event::MessageStart {
                            message: anthropic::Response { usage, .. },
                        }
                        | anthropic::Event::MessageDelta { usage, .. } => {
                            if let Some(recorder) = &mut recorder {
                                recorder.token_count += usage.input_tokens.unwrap_or(0) as usize;
                                recorder.token_count += usage.output_tokens.unwrap_or(0) as usize;
                            }
                        }
                        _ => {}
                    }

                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &chunk).unwrap();
                    buffer.push(b'\n');
                    buffer
                })
            });

            Ok(Response::new(Body::wrap_stream(stream)))
        }
        LanguageModelProvider::OpenAi => {
            let api_key = state
                .config
                .openai_api_key
                .as_ref()
                .context("no OpenAI API key configured on the server")?;
            let chunks = open_ai::stream_completion(
                &state.http_client,
                open_ai::OPEN_AI_API_URL,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
                None,
            )
            .await?;

            let stream = chunks.map(|event| {
                let mut buffer = Vec::new();
                event.map(|chunk| {
                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &chunk).unwrap();
                    buffer.push(b'\n');
                    buffer
                })
            });

            Ok(Response::new(Body::wrap_stream(stream)))
        }
        LanguageModelProvider::Google => {
            let api_key = state
                .config
                .google_ai_api_key
                .as_ref()
                .context("no Google AI API key configured on the server")?;
            let chunks = google_ai::stream_generate_content(
                &state.http_client,
                google_ai::API_URL,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
            )
            .await?;

            let stream = chunks.map(|event| {
                let mut buffer = Vec::new();
                event.map(|chunk| {
                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &chunk).unwrap();
                    buffer.push(b'\n');
                    buffer
                })
            });

            Ok(Response::new(Body::wrap_stream(stream)))
        }
        LanguageModelProvider::Zed => {
            let api_key = state
                .config
                .qwen2_7b_api_key
                .as_ref()
                .context("no Qwen2-7B API key configured on the server")?;
            let api_url = state
                .config
                .qwen2_7b_api_url
                .as_ref()
                .context("no Qwen2-7B URL configured on the server")?;
            let chunks = open_ai::stream_completion(
                &state.http_client,
                &api_url,
                api_key,
                serde_json::from_str(&params.provider_request.get())?,
                None,
            )
            .await?;

            let stream = chunks.map(|event| {
                let mut buffer = Vec::new();
                event.map(|chunk| {
                    buffer.clear();
                    serde_json::to_writer(&mut buffer, &chunk).unwrap();
                    buffer.push(b'\n');
                    buffer
                })
            });

            Ok(Response::new(Body::wrap_stream(stream)))
        }
    }
}

fn normalize_model_name(provider: LanguageModelProvider, name: String) -> String {
    match provider {
        LanguageModelProvider::Anthropic => {
            for prefix in &[
                "claude-3-5-sonnet",
                "claude-3-haiku",
                "claude-3-opus",
                "claude-3-sonnet",
            ] {
                if name.starts_with(prefix) {
                    return prefix.to_string();
                }
            }
        }
        LanguageModelProvider::OpenAi => {}
        LanguageModelProvider::Google => {}
        LanguageModelProvider::Zed => {}
    }

    name
}

async fn check_usage_limit(
    db: &Arc<LlmDatabase>,
    provider: LanguageModelProvider,
    model: &str,
    claims: &LlmTokenClaims,
) -> Result<()> {
    let usage = db
        .get_usage(claims.user_id as i32, provider, model, Utc::now())
        .await?;

    let model = db.model(provider, model)?;

    if usage.requests_this_minute > model.max_requests_per_minute as usize {
        return Err(Error::http(
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded. Maximum requests per minute reached.".to_string(),
        ));
    }
    if usage.tokens_this_minute > model.max_tokens_per_minute as usize {
        return Err(Error::http(
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded. Maximum tokens per minute reached.".to_string(),
        ));
    }
    if usage.tokens_this_day > model.max_tokens_per_day as usize {
        return Err(Error::http(
            StatusCode::TOO_MANY_REQUESTS,
            "Rate limit exceeded. Maximum tokens per day reached.".to_string(),
        ));
    }

    Ok(())
}

struct UsageRecorder {
    db: Arc<LlmDatabase>,
    executor: Executor,
    user_id: i32,
    provider: LanguageModelProvider,
    model: String,
    token_count: usize,
}

impl Drop for UsageRecorder {
    fn drop(&mut self) {
        let db = self.db.clone();
        let user_id = self.user_id;
        let provider = self.provider;
        let model = std::mem::take(&mut self.model);
        let token_count = self.token_count;
        self.executor.spawn_detached(async move {
            db.record_usage(user_id, provider, &model, token_count, Utc::now())
                .await
                .log_err();
        })
    }
}
