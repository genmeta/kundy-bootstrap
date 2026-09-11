use std::{sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use dhttp::endpoint::Endpoint;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    activation::{ActivationError, ActivationService},
    config::{CertServerConfig, UiConfig},
};

const INDEX_TEMPLATE: &str = include_str!("../static/index.html");
const UI_BUILD_CONFIG_PLACEHOLDER: &str = "__APPLIANCE_UI_BUILD_CONFIG__";
const SERVICE_PROBE_TIMEOUT: Duration = Duration::from_secs(8);

fn render_index_page(product_name: &str, activation_domain: &str, service_path: &str) -> String {
    let config = json!({
        "product_name": product_name,
        "activation_domain": activation_domain,
        "service_path": service_path,
    });
    let config = serde_json::to_string(&config)
        .expect("the compile-time UI configuration should serialize")
        .replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029");

    INDEX_TEMPLATE.replace(UI_BUILD_CONFIG_PLACEHOLDER, &config)
}

#[derive(Clone)]
struct ApiState {
    certserver: CertServerClient,
    activation: ActivationService,
    endpoint: Arc<Endpoint>,
    index_page: Arc<str>,
    service_path: Arc<str>,
}

#[derive(Clone)]
struct CertServerClient {
    base_url: Arc<str>,
    http: Client,
}

impl ApiState {
    fn new(
        certserver: CertServerConfig,
        ui: UiConfig,
        endpoint: Arc<Endpoint>,
        activation: ActivationService,
    ) -> Self {
        let service_path = ui.service_path.trim();
        Self {
            certserver: CertServerClient {
                base_url: Arc::from(certserver.base_url.trim_end_matches('/').to_string()),
                http: Client::new(),
            },
            activation,
            endpoint,
            index_page: Arc::from(render_index_page(
                ui.product_name.trim(),
                ui.activation_domain.trim(),
                service_path,
            )),
            service_path: Arc::from(service_path),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InspectActivationRequest {
    activation_code: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckNameRequest {
    activation_code: String,
    subname: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimActivationRequest {
    activation_code: String,
    subname: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeactivateRequest {
    confirmation: String,
    activation_code: String,
}

#[derive(Debug, Serialize)]
struct ServiceStatusResponse {
    state: &'static str,
}

pub fn router(
    certserver: CertServerConfig,
    ui: UiConfig,
    endpoint: Arc<Endpoint>,
    activation: ActivationService,
) -> Router {
    let state = ApiState::new(certserver, ui, endpoint, activation);
    Router::new()
        .route("/", get(index))
        .route("/healthz", get(health))
        .route("/api/status", get(status))
        .route("/api/service/status", get(service_status))
        .route("/api/runtime/status", get(runtime_status))
        .route("/api/certificate", get(certificate_info))
        .route("/api/certificate/renew", post(renew_certificate))
        .route("/api/certificate/download", get(download_certificate))
        .route("/api/activation/inspect", post(inspect_activation))
        .route("/api/activation/names/check", post(check_name))
        .route("/api/activation/claim", post(claim_activation))
        .route("/api/activation/deactivate", post(deactivate_activation))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .with_state(state)
}

async fn index(State(state): State<ApiState>) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        Html(state.index_page.to_string()),
    )
}

async fn status(State(state): State<ApiState>) -> Response {
    match state.activation.status().await {
        Ok(device_state) => {
            ([(header::CACHE_CONTROL, "no-store")], Json(device_state)).into_response()
        }
        Err(error) => activation_error_response(error),
    }
}

async fn health() -> impl IntoResponse {
    ([(header::CACHE_CONTROL, "no-store")], "ok")
}

async fn service_status(State(state): State<ApiState>) -> Response {
    let domain_name = match state.activation.active_domain_name().await {
        Ok(Some(domain_name)) => domain_name,
        Ok(None) => return service_status_response(false),
        Err(error) => return activation_error_response(error),
    };
    let uri = format!("https://{domain_name}{}", state.service_path);
    let available = matches!(
        tokio::time::timeout(
            SERVICE_PROBE_TIMEOUT,
            state.endpoint.get(uri).into_response(),
        )
        .await,
        Ok(Ok(response)) if service_response_is_healthy(response.status().as_u16())
    );

    service_status_response(available)
}

async fn runtime_status(State(state): State<ApiState>) -> Response {
    match state.activation.runtime_status().await {
        Ok(status) => ([(header::CACHE_CONTROL, "no-store")], Json(status)).into_response(),
        Err(ActivationError::NotActive) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({"state": "pending", "generation": 0})),
        )
            .into_response(),
        Err(error) => activation_error_response(error),
    }
}

async fn certificate_info(State(state): State<ApiState>) -> Response {
    match state.activation.certificate_info().await {
        Ok(Some(certificate)) => {
            ([(header::CACHE_CONTROL, "no-store")], Json(certificate)).into_response()
        }
        Ok(None) => certificate_unavailable(),
        Err(error) => activation_error_response(error),
    }
}

async fn renew_certificate(State(state): State<ApiState>) -> Response {
    match state.activation.renew_certificate().await {
        Ok(result) => ([(header::CACHE_CONTROL, "no-store")], Json(result)).into_response(),
        Err(error) => activation_error_response(error),
    }
}

async fn download_certificate(State(state): State<ApiState>) -> Response {
    match state.activation.certificate_download().await {
        Ok(Some(certificate)) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::CONTENT_TYPE, "application/pem-certificate-chain")
            .header(
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{}\"", certificate.filename),
            )
            .body(Body::from(certificate.pem))
            .unwrap_or_else(|_| certificate_unavailable()),
        Ok(None) => certificate_unavailable(),
        Err(error) => activation_error_response(error),
    }
}

async fn inspect_activation(
    State(state): State<ApiState>,
    Json(request): Json<InspectActivationRequest>,
) -> Response {
    state
        .certserver
        .post(
            "/v2/activation/inspect",
            json!({"activation_code": request.activation_code}),
        )
        .await
}

async fn check_name(
    State(state): State<ApiState>,
    Json(request): Json<CheckNameRequest>,
) -> Response {
    state
        .certserver
        .post(
            "/v2/activation/names/check",
            json!({
                "activation_code": request.activation_code,
                "subname": request.subname,
            }),
        )
        .await
}

async fn claim_activation(
    State(state): State<ApiState>,
    Json(request): Json<ClaimActivationRequest>,
) -> Response {
    match state
        .activation
        .claim(&request.activation_code, &request.subname)
        .await
    {
        Ok(result) => (
            StatusCode::CREATED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(result),
        )
            .into_response(),
        Err(error) => activation_error_response(error),
    }
}

async fn deactivate_activation(
    State(state): State<ApiState>,
    Json(request): Json<DeactivateRequest>,
) -> Response {
    if request.confirmation != "DEACTIVATE" {
        return activation_error(
            StatusCode::BAD_REQUEST,
            "deactivation_confirmation_required",
            "请输入 DEACTIVATE 以确认取消激活。",
        );
    }
    match state
        .activation
        .deactivate_with_code(&request.activation_code)
        .await
    {
        Ok(result) => ([(header::CACHE_CONTROL, "no-store")], Json(result)).into_response(),
        Err(error) => activation_error_response(error),
    }
}

impl CertServerClient {
    async fn post(&self, path: &str, payload: Value) -> Response {
        let url = format!("{}{}", self.base_url, path);
        let response = match self
            .http
            .post(url)
            .timeout(Duration::from_secs(15))
            .json(&payload)
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return certserver_unavailable(),
        };
        let status =
            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/json")
            .to_string();
        let body = match response.bytes().await {
            Ok(body) => body,
            Err(_) => return certserver_unavailable(),
        };

        Response::builder()
            .status(status)
            .header(header::CACHE_CONTROL, "no-store")
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body))
            .unwrap_or_else(|_| certserver_unavailable())
    }
}

fn certserver_unavailable() -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "error": {
                "code": "certserver_unavailable",
                "message": "远程激活服务暂时不可用，请稍后重试。"
            }
        })),
    )
        .into_response()
}

fn certificate_unavailable() -> Response {
    activation_error(
        StatusCode::NOT_FOUND,
        "certificate_unavailable",
        "本机证书尚不可用。",
    )
}

fn service_status_response(available: bool) -> Response {
    let state = if available {
        "available"
    } else {
        "unavailable"
    };
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(ServiceStatusResponse { state }),
    )
        .into_response()
}

fn service_response_is_healthy(status: u16) -> bool {
    (200..400).contains(&status)
}

fn activation_error_response(error: ActivationError) -> Response {
    match error {
        ActivationError::Remote { status, error } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            (
                status,
                [(header::CACHE_CONTROL, "no-store")],
                Json(json!({"error": error})),
            )
                .into_response()
        }
        ActivationError::CertServerUnavailable => certserver_unavailable(),
        ActivationError::NameUnavailable => activation_error(
            StatusCode::CONFLICT,
            "activation_name_unavailable",
            "该子名字当前不可用，请更换后重试。",
        ),
        ActivationError::PendingNameConflict { domain_name } => activation_error(
            StatusCode::CONFLICT,
            "activation_name_locked",
            format!("本机已经开始认领 {domain_name}，只能继续完成该名字的激活。"),
        ),
        ActivationError::PendingCodeConflict => activation_error(
            StatusCode::CONFLICT,
            "activation_code_locked",
            "本机已有未完成的激活，请使用最初提交的激活码继续。",
        ),
        ActivationError::NotActive => activation_error(
            StatusCode::CONFLICT,
            "certificate_renewal_unavailable",
            "本机尚未完成激活，无法更新证书。",
        ),
        ActivationError::RecoveryCredentialUnavailable => activation_error(
            StatusCode::CONFLICT,
            "activation_recovery_credential_unavailable",
            "本机没有保存恢复凭据，请使用原激活码重新完成恢复。",
        ),
        ActivationError::DeactivationInProgress => activation_error(
            StatusCode::CONFLICT,
            "deactivation_in_progress",
            "本机正在取消激活，请等待宿主机运行态同步完成。",
        ),
        ActivationError::DeactivationCredentialInvalid => activation_error(
            StatusCode::FORBIDDEN,
            "deactivation_credential_invalid",
            "激活码不匹配，未执行取消激活。",
        ),
        error @ ActivationError::Internal { .. } => {
            tracing::error!(
                error = %error,
                source = ?std::error::Error::source(&error),
                "local activation failed"
            );
            activation_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "activation_local_error",
                "本机激活配置失败，请稍后重试。",
            )
        }
    }
}

fn activation_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "error": {
                "code": code,
                "message": message.into()
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::{
        INDEX_TEMPLATE, UI_BUILD_CONFIG_PLACEHOLDER, render_index_page, service_response_is_healthy,
    };
    use crate::activation::DeviceStatus;
    use serde_json::json;

    #[test]
    fn factory_status_payload_is_minimal() {
        let status = DeviceStatus {
            state: "factory_ready",
            display_name: None,
        };

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"state": "factory_ready"})
        );
    }

    #[test]
    fn active_status_includes_the_public_device_name() {
        let status = DeviceStatus {
            state: "active",
            display_name: Some("test.kundy~".to_string()),
        };

        assert_eq!(
            serde_json::to_value(status).unwrap(),
            json!({"state": "active", "display_name": "test.kundy~"})
        );
    }

    #[test]
    fn index_page_embeds_the_compile_time_ui_configuration() {
        let page = render_index_page("SciFlow", "sciflow", "/sciflow-console/");

        assert!(!page.contains(UI_BUILD_CONFIG_PLACEHOLDER));
        assert!(page.contains(r#""product_name":"SciFlow""#));
        assert!(page.contains(r#""activation_domain":"sciflow""#));
        assert!(page.contains(r#""service_path":"/sciflow-console/""#));
    }

    #[test]
    fn index_page_escapes_script_breakout_characters() {
        let page = render_index_page("</script>", "kundy", "/sciflow-console/");

        assert!(!page.contains(r#""product_name":"</script>""#));
        assert!(page.contains(r#""product_name":"\u003c/script>""#));
    }

    #[test]
    fn information_page_does_not_submit_runtime_writes() {
        assert!(!INDEX_TEMPLATE.contains("/api/runtime/reconcile"));
    }

    #[test]
    fn service_probe_accepts_success_and_redirect_responses() {
        assert!(service_response_is_healthy(200));
        assert!(service_response_is_healthy(302));
        assert!(!service_response_is_healthy(404));
        assert!(!service_response_is_healthy(500));
    }
}
