use std::{
    env,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::{Bytes, to_bytes},
    extract::{FromRequest, Multipart, Request, State},
    http::{
        HeaderName, HeaderValue, Method, StatusCode,
        header::{CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use luadicrous_core::{CustomOpcodeTable, Engine, looks_like_luau_source};
use serde::Serialize;
use tokio::{sync::Semaphore, task::JoinError, time::timeout};
use tower_http::{
    cors::{AllowOrigin, Any, CorsLayer},
    limit::RequestBodyLimitLayer,
    trace::TraceLayer,
};

const ENGINE_HEADER: &str = "x-luadicrous-engine";
const TIME_HEADER: &str = "x-luadicrous-time";
const OPCODE_TABLE_HEADER: &str = "x-luadicrous-opcode-table";
const MULTIPART_ALLOWANCE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub bind: SocketAddr,
    pub max_upload_bytes: usize,
    pub max_output_bytes: usize,
    pub max_opcode_table_bytes: usize,
    pub engine_timeout: Duration,
    pub queue_timeout: Duration,
    pub max_concurrent: usize,
    pub cors_origins: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::from([0, 0, 0, 0]), 8000),
            max_upload_bytes: 64 * 1024 * 1024,
            max_output_bytes: 64 * 1024 * 1024,
            max_opcode_table_bytes: 1024 * 1024,
            engine_timeout: Duration::from_secs(30),
            queue_timeout: Duration::from_secs(15),
            max_concurrent: 2,
            cors_origins: vec!["*".to_owned()],
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let defaults = Self::default();
        let host = env::var("LUADICROUS_HOST")
            .or_else(|_| env::var("HOST"))
            .unwrap_or_else(|_| defaults.bind.ip().to_string());
        let port_value = env_value("LUADICROUS_PORT").or_else(|| env_value("PORT"));
        let port = port_value
            .as_deref()
            .map_or(Ok(defaults.bind.port()), parse_env)?;
        let bind = SocketAddr::new(
            host.parse()
                .with_context(|| format!("invalid LUADICROUS_HOST/HOST value: {host}"))?,
            port,
        );
        let cors_origins = env::var("LUADICROUS_CORS_ORIGINS")
            .unwrap_or_else(|_| "*".to_owned())
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();

        Ok(Self {
            bind,
            max_upload_bytes: parse_env_or(
                "LUADICROUS_MAX_UPLOAD_BYTES",
                defaults.max_upload_bytes,
            )?,
            max_output_bytes: parse_env_or(
                "LUADICROUS_MAX_OUTPUT_BYTES",
                defaults.max_output_bytes,
            )?,
            max_opcode_table_bytes: parse_env_or(
                "LUADICROUS_MAX_OPCODE_TABLE_BYTES",
                defaults.max_opcode_table_bytes,
            )?,
            engine_timeout: Duration::from_secs_f64(parse_env_or(
                "LUADICROUS_ENGINE_TIMEOUT",
                defaults.engine_timeout.as_secs_f64(),
            )?),
            queue_timeout: Duration::from_secs_f64(parse_env_or(
                "LUADICROUS_QUEUE_TIMEOUT",
                defaults.queue_timeout.as_secs_f64(),
            )?),
            max_concurrent: parse_env_or("LUADICROUS_MAX_CONCURRENT", defaults.max_concurrent)?
                .max(1),
            cors_origins: if cors_origins.is_empty() {
                defaults.cors_origins
            } else {
                cors_origins
            },
        })
    }

    fn max_request_bytes(&self) -> usize {
        self.max_upload_bytes
            .saturating_add(self.max_opcode_table_bytes)
            .saturating_add(MULTIPART_ALLOWANCE)
    }
}

fn env_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

fn parse_env<T>(value: &str) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    value
        .parse()
        .map_err(|error| anyhow::anyhow!("invalid environment value {value:?}: {error}"))
}

fn parse_env_or<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    match env_value(name) {
        Some(value) => parse_env(&value),
        None => Ok(default),
    }
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    semaphore: Arc<Semaphore>,
}

impl AppState {
    fn new(config: Config) -> Self {
        let max_concurrent = config.max_concurrent;
        Self {
            config: Arc::new(config),
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Mode {
    Decompile,
    Disassemble,
}

impl Mode {
    const fn command_name(self) -> &'static str {
        match self {
            Self::Decompile => "decompile",
            Self::Disassemble => "disasm",
        }
    }
}

#[derive(Debug)]
struct Payload {
    bytecode: Bytes,
    custom_opcode_table: Option<CustomOpcodeTable>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, self.message).into_response()
    }
}

#[derive(Debug, Serialize)]
struct Health<'a> {
    status: &'a str,
    engine: &'a str,
    version: &'a str,
    limits: HealthLimits,
}

#[derive(Debug, Serialize)]
struct HealthLimits {
    upload_bytes: usize,
    output_bytes: usize,
    timeout_seconds: f64,
    max_concurrent: usize,
}

pub fn build_router(config: Config) -> Result<Router> {
    let max_request_bytes = config.max_request_bytes();
    let cors = cors_layer(&config)?;
    let state = AppState::new(config);
    Ok(Router::new()
        .route("/health", get(health))
        .route("/decompile", post(decompile))
        .route("/disasm", post(disassemble))
        .route("/luadicrous/health", get(health))
        .route("/luadicrous/decompile", post(decompile))
        .route("/luadicrous/disasm", post(disassemble))
        .layer(RequestBodyLimitLayer::new(max_request_bytes))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state))
}

fn cors_layer(config: &Config) -> Result<CorsLayer> {
    let custom_opcode_header = HeaderName::from_static(OPCODE_TABLE_HEADER);
    let layer = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([CONTENT_TYPE, CONTENT_LENGTH, custom_opcode_header]);
    if config.cors_origins.iter().any(|origin| origin == "*") {
        return Ok(layer.allow_origin(Any));
    }
    let origins = config
        .cors_origins
        .iter()
        .map(|origin| {
            HeaderValue::from_str(origin).with_context(|| format!("invalid CORS origin {origin:?}"))
        })
        .collect::<Result<Vec<_>>>()?;
    if origins.is_empty() {
        bail!("at least one CORS origin is required");
    }
    Ok(layer.allow_origin(AllowOrigin::list(origins)))
}

async fn health(State(state): State<AppState>) -> impl IntoResponse {
    axum::Json(Health {
        status: "ok",
        engine: "rust-native",
        version: env!("CARGO_PKG_VERSION"),
        limits: HealthLimits {
            upload_bytes: state.config.max_upload_bytes,
            output_bytes: state.config.max_output_bytes,
            timeout_seconds: state.config.engine_timeout.as_secs_f64(),
            max_concurrent: state.config.max_concurrent,
        },
    })
}

async fn decompile(State(state): State<AppState>, request: Request) -> Response {
    handle_request(state, request, Mode::Decompile).await
}

async fn disassemble(State(state): State<AppState>, request: Request) -> Response {
    handle_request(state, request, Mode::Disassemble).await
}

async fn handle_request(state: AppState, request: Request, mode: Mode) -> Response {
    let payload = match read_payload(&state, request).await {
        Ok(payload) => payload,
        Err(error) => return error.into_response(),
    };
    if looks_like_luau_source(&payload.bytecode) {
        return warning_response(
            mode,
            "input appears to be plain Lua/Luau source, not serialized bytecode",
        );
    }

    let permit = match timeout(
        state.config.queue_timeout,
        state.semaphore.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            return ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "decompiler is shutting down",
            )
            .into_response();
        }
        Err(_) => {
            return ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "decompiler queue is full")
                .into_response();
        }
    };

    let Payload {
        bytecode,
        custom_opcode_table,
    } = payload;
    let max_output_bytes = state.config.max_output_bytes;
    let started = Instant::now();
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let engine = custom_opcode_table.map_or_else(Engine::default, |table| {
            Engine::default().with_opcode_table(table)
        });
        match mode {
            Mode::Decompile => engine.decompile(&bytecode).map(|result| result.source),
            Mode::Disassemble => engine.disassemble(&bytecode),
        }
    });

    let output = match timeout(state.config.engine_timeout, worker).await {
        Ok(Ok(Ok(output))) => output,
        Ok(Ok(Err(error))) => return warning_response(mode, &error.to_string()),
        Ok(Err(error)) => return worker_error_response(mode, &error),
        Err(_) => {
            return ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                format!(
                    "-- LUADICROUSWARNING: {} exceeded the {:.1}s timeout",
                    mode.command_name(),
                    state.config.engine_timeout.as_secs_f64()
                ),
            )
            .into_response();
        }
    };
    let elapsed = started.elapsed();
    let output = match mode {
        Mode::Decompile => add_timed_watermark(&output, elapsed),
        Mode::Disassemble => output,
    };
    if output.len() > max_output_bytes {
        return ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("-- LUADICROUSWARNING: output exceeds the {max_output_bytes}-byte limit"),
        )
        .into_response();
    }
    success_response(output, elapsed)
}

fn worker_error_response(mode: Mode, error: &JoinError) -> Response {
    warning_response(mode, &format!("worker failed: {error}"))
}

fn warning_response(mode: Mode, message: &str) -> Response {
    (
        StatusCode::OK,
        format!(
            "-- LUADICROUSWARNING: {} failed: {message}",
            mode.command_name()
        ),
    )
        .into_response()
}

fn success_response(output: String, elapsed: Duration) -> Response {
    let mut response = (StatusCode::OK, output).into_response();
    response.headers_mut().insert(
        HeaderName::from_static(ENGINE_HEADER),
        HeaderValue::from_static("rust-native"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("{:.6}", elapsed.as_secs_f64())) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(TIME_HEADER), value);
    }
    response
}

fn add_timed_watermark(source: &str, elapsed: Duration) -> String {
    let source = source
        .strip_prefix("-- Decompiled by Luadicrous V2\n")
        .or_else(|| source.strip_prefix("-- Decompiled by Luadicrous RS\n"))
        .unwrap_or(source);
    let source = source
        .lines()
        .next()
        .filter(|line| line.starts_with("-- Decompiled in "))
        .map_or(source, |_| source.split_once('\n').map_or("", |(_, rest)| rest));
    format!(
        "-- Decompiled by Luadicrous V2 (took {:.3}s)\n-- It's good! I tried it!  - Jonathan Sigmund\n-- https://fluxer.gg/wqi9tana\n{source}",
        elapsed.as_secs_f64()
    )
}

async fn read_payload(state: &AppState, request: Request) -> Result<Payload, ApiError> {
    let header_opcode_table = request
        .headers()
        .get(OPCODE_TABLE_HEADER)
        .map(|value| parse_opcode_table(value.as_bytes(), state.config.max_opcode_table_bytes))
        .transpose()?;
    let content_type = request
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let multipart = content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("multipart/form-data"));
    let has_multipart_boundary = content_type.split(';').skip(1).any(|parameter| {
        parameter.split_once('=').is_some_and(|(name, value)| {
            name.trim().eq_ignore_ascii_case("boundary")
                && !value.trim().trim_matches('"').is_empty()
        })
    });
    if !multipart || !has_multipart_boundary {
        let bytecode = to_bytes(request.into_body(), state.config.max_upload_bytes + 1)
            .await
            .map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to read request: {error}"),
                )
            })?;
        validate_bytecode_size(&bytecode, state.config.max_upload_bytes)?;
        return Ok(Payload {
            bytecode,
            custom_opcode_table: header_opcode_table,
        });
    }

    let mut multipart = Multipart::from_request(request, state)
        .await
        .map_err(|error| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("invalid multipart request: {error}"),
            )
        })?;
    let mut bytecode = None;
    let mut custom_opcode_table = header_opcode_table;
    while let Some(field) = multipart.next_field().await.map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid multipart field: {error}"),
        )
    })? {
        let name = field.name().unwrap_or_default().to_owned();
        if name == "file" {
            if bytecode.is_some() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "multipart request contains multiple file fields",
                ));
            }
            let bytes = field.bytes().await.map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to read bytecode field: {error}"),
                )
            })?;
            validate_bytecode_size(&bytes, state.config.max_upload_bytes)?;
            bytecode = Some(bytes);
        } else if name == "opcode_table" || name == "opcode_table_json" {
            let bytes = field.bytes().await.map_err(|error| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    format!("failed to read opcode table: {error}"),
                )
            })?;
            if bytes.len() > state.config.max_opcode_table_bytes {
                return Err(ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "opcode table is too large",
                ));
            }
            custom_opcode_table = Some(parse_opcode_table(
                &bytes,
                state.config.max_opcode_table_bytes,
            )?);
        }
    }
    let bytecode = bytecode.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "multipart request requires a file field",
        )
    })?;
    Ok(Payload {
        bytecode,
        custom_opcode_table,
    })
}

fn parse_opcode_table(
    bytes: &[u8],
    max_opcode_table_bytes: usize,
) -> Result<CustomOpcodeTable, ApiError> {
    if bytes.len() > max_opcode_table_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "opcode table is too large",
        ));
    }
    CustomOpcodeTable::from_json_slice(bytes).map_err(|error| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("invalid opcode table: {error}"),
        )
    })
}

fn validate_bytecode_size(bytecode: &Bytes, max_upload_bytes: usize) -> Result<(), ApiError> {
    if bytecode.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "empty bytecode"));
    }
    if bytecode.len() > max_upload_bytes {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("bytecode exceeds the {max_upload_bytes}-byte upload limit"),
        ));
    }
    Ok(())
}

pub async fn serve(config: Config) -> Result<()> {
    let bind = config.bind;
    let app = build_router(config)?;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("failed to bind {bind}"))?;
    eprintln!("Luadicrous RS server listening on http://{bind}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server failed")
}

async fn shutdown_signal() {
    let control_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = control_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use tower::ServiceExt;

    use super::{Config, build_router, looks_like_luau_source};

    fn return_42_chunk() -> Vec<u8> {
        vec![
            3, // bytecode version
            0, // strings
            1, // prototypes
            1, 0, 0, 0, // stack, params, upvalues, vararg
            2, // instructions
            4, 0, 42, 0, // LOADN R0 42
            22, 0, 2, 0, // RETURN R0 1
            0, // constants
            0, // children
            0, // line defined
            0, // debug name
            0, // line info
            0, // debug info
            0, // main
        ]
    }

    fn lua51_return_42_chunk() -> Vec<u8> {
        let mut bytes = b"\x1bLua\x51\0\x01\x04\x08\x04\x08\0".to_vec();
        bytes.extend(0_u64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend([0, 0, 2, 2]);
        bytes.extend(2_u32.to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.extend((30_u32 | (2_u32 << 23)).to_le_bytes());
        bytes.extend(1_u32.to_le_bytes());
        bytes.push(3);
        bytes.extend(42_f64.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes.extend(0_u32.to_le_bytes());
        bytes
    }

    #[test]
    fn detects_source_without_treating_binary_as_text() {
        assert!(looks_like_luau_source(
            b"local value = 42\nfunction read() return value end\nreturn read"
        ));
        assert!(!looks_like_luau_source(&return_42_chunk()));
    }

    #[tokio::test]
    async fn health_reports_native_engine() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/luadicrous/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("rust-native"));
    }

    #[tokio::test]
    async fn decompiles_raw_bytecode_with_timing_headers() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/luadicrous/decompile")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(return_42_chunk()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-luadicrous-engine"], "rust-native");
        assert!(response.headers().contains_key("x-luadicrous-time"));
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let source = String::from_utf8_lossy(&body);
        assert!(source.contains("Decompiled by Luadicrous V2 (took"));
        assert!(source.contains("return 42"));
        assert!(!source.to_lowercase().contains("recovery strategy"));
        assert!(!source.to_lowercase().contains("recovery method"));
    }

    #[tokio::test]
    async fn decompiles_lua51_bytecode_over_http() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/luadicrous/decompile")
                    .header("content-type", "application/octet-stream")
                    .body(Body::from(lua51_return_42_chunk()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let source = String::from_utf8_lossy(&body);
        assert!(source.contains("PUC Lua 5.1"));
        assert!(source.contains("return 42"));
    }

    #[tokio::test]
    async fn accepts_multipart_and_legacy_disassembly_routes() {
        let boundary = "luadicrous-test-boundary";
        let mut multipart = format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"fixture.luac\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )
        .into_bytes();
        multipart.extend(return_42_chunk());
        multipart.extend(format!("\r\n--{boundary}--\r\n").bytes());

        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/disasm")
                    .header(
                        "content-type",
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(multipart))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-luadicrous-engine"], "rust-native");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let assembly = String::from_utf8_lossy(&body);
        assert!(assembly.contains("LOADN"));
        assert!(assembly.contains("RETURN"));
    }

    #[tokio::test]
    async fn accepts_raw_bytecode_mislabeled_as_multipart() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/decompile")
                    .header("content-type", "multipart/form-data")
                    .body(Body::from(return_42_chunk()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-luadicrous-engine"], "rust-native");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("return 42"));
    }

    #[tokio::test]
    async fn source_input_has_an_explicit_warning() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/decompile")
                    .body(Body::from(
                        "local value = 42\nfunction read() return value end\nreturn read",
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let warning = String::from_utf8_lossy(&body);
        assert!(warning.contains("plain Lua/Luau source"));
        assert!(!warning.contains("version 10"));
    }

    #[tokio::test]
    async fn custom_opcode_requests_use_native_decoder() {
        let mut chunk = return_42_chunk();
        chunk[8] = 200;
        chunk[12] = 201;
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/decompile")
                    .header(
                        "x-luadicrous-opcode-table",
                        r#"{"raw_to_opcode":{"200":"LOADN","201":"RETURN"}}"#,
                    )
                    .body(Body::from(chunk))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-luadicrous-engine"], "rust-native");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert!(String::from_utf8_lossy(&body).contains("return 42"));
    }

    #[tokio::test]
    async fn malformed_custom_opcode_table_is_rejected() {
        let app = build_router(Config::default()).unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/decompile")
                    .header("x-luadicrous-opcode-table", "{}")
                    .body(Body::from(return_42_chunk()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
