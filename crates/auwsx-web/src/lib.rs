//! Thin HTTP adapter over the auwsx daemon IPC socket.
//!
//! The web layer owns transport concerns only: HTTP headers, GitHub payload
//! shape, webhook signature validation, and daemon socket routing. All durable
//! mutation stays in `auwsx-core`.

use anyhow::{anyhow, Context};
use auwsx_core::db::remote::{ProjectRemoteConfig, RemoteProvider};
use auwsx_core::ipc::{self, Command, Response};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as HttpResponse};
use axum::{routing::post, Json, Router};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone)]
pub struct AppState {
    socket_path: PathBuf,
}

impl AppState {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }
}

pub fn app(socket_path: PathBuf) -> Router {
    Router::new()
        .route("/webhooks/github", post(github_webhook))
        .with_state(AppState::new(socket_path))
}

pub async fn serve(addr: SocketAddr, socket_path: PathBuf) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding auwsx-web to {addr}"))?;
    axum::serve(listener, app(socket_path))
        .await
        .context("serving auwsx-web")
}

async fn github_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, WebhookError> {
    let event_kind = header_str(&headers, "x-github-event")?.to_string();
    let delivery_id = header_str(&headers, "x-github-delivery")?.to_string();
    let envelope = parse_github_envelope(&body)?;
    let config = get_remote_config_by_repo(
        &state.socket_path,
        RemoteProvider::Github,
        &envelope.owner,
        &envelope.repo,
    )
    .await?;

    let Some(config) = config else {
        return Ok(Json(json!({
            "status": "ignored",
            "reason": "missing_config"
        })));
    };
    verify_configured_signature(&config, &headers, &body)?;

    if event_kind != "issue_comment" {
        return Ok(Json(json!({
            "status": "ignored",
            "reason": "unsupported_event",
            "event": event_kind
        })));
    }

    let command = github_issue_comment_command(&body, event_kind, delivery_id)?;
    let outcome = process_remote_run(&state.socket_path, command).await?;
    Ok(Json(json!({
        "status": "processed",
        "outcome": outcome
    })))
}

async fn get_remote_config_by_repo(
    socket_path: &Path,
    provider: RemoteProvider,
    owner: &str,
    repo: &str,
) -> Result<Option<ProjectRemoteConfig>, WebhookError> {
    match ipc::request(
        socket_path,
        &Command::GetProjectRemoteConfigByRepo {
            provider,
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
    )
    .await?
    {
        Response::ProjectRemoteConfig(config) => Ok(config),
        Response::Err { message } => Err(WebhookError::BadGateway(message)),
        other => Err(WebhookError::BadGateway(format!(
            "unexpected daemon response: {other:?}"
        ))),
    }
}

async fn process_remote_run(
    socket_path: &Path,
    command: Command,
) -> Result<auwsx_core::remote_inbound::RemoteInboundOutcome, WebhookError> {
    match ipc::request(socket_path, &command).await? {
        Response::RemoteInboundOutcome(outcome) => Ok(outcome),
        Response::Err { message } => Err(WebhookError::BadGateway(message)),
        other => Err(WebhookError::BadGateway(format!(
            "unexpected daemon response: {other:?}"
        ))),
    }
}

fn verify_configured_signature(
    config: &ProjectRemoteConfig,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), WebhookError> {
    let Some(secret_ref) = config.webhook_secret_ref.as_deref() else {
        return Ok(());
    };
    let secret = read_secret_ref(secret_ref)?;
    let signature = header_str(headers, "x-hub-signature-256")?;
    verify_github_signature(secret.as_bytes(), body, signature)?;
    Ok(())
}

fn read_secret_ref(secret_ref: &str) -> Result<String, WebhookError> {
    let env_name = secret_ref
        .trim()
        .strip_prefix("env:")
        .unwrap_or_else(|| secret_ref.trim());
    if env_name.is_empty() {
        return Err(WebhookError::Misconfigured(
            "webhook secret ref is blank".to_string(),
        ));
    }
    std::env::var(env_name).map_err(|_| {
        WebhookError::Misconfigured(format!("webhook secret env var `{env_name}` is not set"))
    })
}

pub fn github_issue_comment_command(
    body: &[u8],
    event_kind: String,
    delivery_id: String,
) -> Result<Command, WebhookError> {
    let payload: GithubIssueCommentPayload = serde_json::from_slice(body)?;
    let owner = payload.repository.owner.login.trim();
    let repo = payload.repository.name.trim();
    if owner.is_empty() || repo.is_empty() {
        return Err(WebhookError::BadRequest(
            "repository owner and name are required".to_string(),
        ));
    }
    Ok(Command::ProcessRemoteAuwsxRun {
        provider: RemoteProvider::Github,
        delivery_id,
        event_kind,
        action: payload.action,
        payload_hash: sha256_hex(body),
        owner: owner.to_string(),
        repo: repo.to_string(),
        remote_issue_number: payload.issue.number,
        remote_issue_node_id: payload.issue.node_id,
        remote_issue_title: payload.issue.title,
        remote_issue_url: payload.issue.html_url,
        comment_body: payload.comment.body,
    })
}

fn parse_github_envelope(body: &[u8]) -> Result<GithubEnvelope, WebhookError> {
    let payload: GithubEnvelopePayload = serde_json::from_slice(body)?;
    let owner = payload.repository.owner.login.trim();
    let repo = payload.repository.name.trim();
    if owner.is_empty() || repo.is_empty() {
        return Err(WebhookError::BadRequest(
            "repository owner and name are required".to_string(),
        ));
    }
    Ok(GithubEnvelope {
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

pub fn verify_github_signature(
    secret: &[u8],
    body: &[u8],
    signature_header: &str,
) -> Result<(), WebhookError> {
    let Some(hex_signature) = signature_header.trim().strip_prefix("sha256=") else {
        return Err(WebhookError::Unauthorized(
            "missing sha256 signature prefix".to_string(),
        ));
    };
    let expected = hex::decode(hex_signature)
        .map_err(|_| WebhookError::Unauthorized("invalid signature hex".to_string()))?;
    let mut mac = HmacSha256::new_from_slice(secret)
        .map_err(|_| WebhookError::Unauthorized("invalid webhook secret".to_string()))?;
    mac.update(body);
    mac.verify_slice(&expected)
        .map_err(|_| WebhookError::Unauthorized("signature mismatch".to_string()))
}

pub fn github_signature(secret: &[u8], body: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn sha256_hex(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, WebhookError> {
    headers
        .get(name)
        .ok_or_else(|| WebhookError::BadRequest(format!("missing `{name}` header")))?
        .to_str()
        .map_err(|_| WebhookError::BadRequest(format!("invalid `{name}` header")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GithubEnvelope {
    owner: String,
    repo: String,
}

#[derive(Debug, Deserialize)]
struct GithubEnvelopePayload {
    repository: GithubRepository,
}

#[derive(Debug, Deserialize)]
struct GithubIssueCommentPayload {
    action: Option<String>,
    repository: GithubRepository,
    issue: GithubIssue,
    comment: GithubComment,
}

#[derive(Debug, Deserialize)]
struct GithubRepository {
    name: String,
    owner: GithubOwner,
}

#[derive(Debug, Deserialize)]
struct GithubOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct GithubIssue {
    number: i64,
    node_id: Option<String>,
    title: String,
    html_url: String,
}

#[derive(Debug, Deserialize)]
struct GithubComment {
    body: String,
}

#[derive(Debug)]
pub enum WebhookError {
    BadRequest(String),
    Unauthorized(String),
    Misconfigured(String),
    BadGateway(String),
}

impl From<serde_json::Error> for WebhookError {
    fn from(value: serde_json::Error) -> Self {
        WebhookError::BadRequest(value.to_string())
    }
}

impl From<anyhow::Error> for WebhookError {
    fn from(value: anyhow::Error) -> Self {
        WebhookError::BadGateway(format!("{value:#}"))
    }
}

impl IntoResponse for WebhookError {
    fn into_response(self) -> HttpResponse {
        let (status, reason, detail) = match self {
            WebhookError::BadRequest(detail) => (StatusCode::BAD_REQUEST, "bad_request", detail),
            WebhookError::Unauthorized(detail) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", detail)
            }
            WebhookError::Misconfigured(detail) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "misconfigured", detail)
            }
            WebhookError::BadGateway(detail) => (StatusCode::BAD_GATEWAY, "bad_gateway", detail),
        };
        (status, Json(json!({ "error": reason, "detail": detail }))).into_response()
    }
}

pub fn default_addr() -> anyhow::Result<SocketAddr> {
    std::env::var("AUWSX_WEB_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:7789".to_string())
        .parse()
        .map_err(|e| anyhow!("invalid AUWSX_WEB_ADDR: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn issue_comment_payload() -> Vec<u8> {
        br#"{
          "action": "created",
          "repository": {
            "name": "app",
            "owner": { "login": "acme" }
          },
          "issue": {
            "number": 42,
            "node_id": "I_kwDO",
            "title": "Remote work",
            "html_url": "https://github.com/acme/app/issues/42"
          },
          "comment": {
            "body": "please\n/auwsx-run build the adapter"
          }
        }"#
        .to_vec()
    }

    #[test]
    fn given_github_issue_comment_when_normalized_then_remote_command_contains_issue_context() {
        let body = issue_comment_payload();

        let command = github_issue_comment_command(
            &body,
            "issue_comment".to_string(),
            "delivery-1".to_string(),
        )
        .unwrap();

        let Command::ProcessRemoteAuwsxRun {
            provider,
            delivery_id,
            event_kind,
            action,
            payload_hash,
            owner,
            repo,
            remote_issue_number,
            remote_issue_node_id,
            remote_issue_title,
            remote_issue_url,
            comment_body,
        } = command
        else {
            panic!("expected ProcessRemoteAuwsxRun command");
        };
        assert_eq!(provider, RemoteProvider::Github);
        assert_eq!(delivery_id, "delivery-1");
        assert_eq!(event_kind, "issue_comment");
        assert_eq!(action.as_deref(), Some("created"));
        assert_eq!(payload_hash, sha256_hex(&body));
        assert_eq!((owner.as_str(), repo.as_str()), ("acme", "app"));
        assert_eq!(remote_issue_number, 42);
        assert_eq!(remote_issue_node_id.as_deref(), Some("I_kwDO"));
        assert_eq!(remote_issue_title, "Remote work");
        assert_eq!(remote_issue_url, "https://github.com/acme/app/issues/42");
        assert!(comment_body.contains("/auwsx-run build the adapter"));
    }

    #[test]
    fn given_valid_github_signature_when_verified_then_ok() {
        let body = issue_comment_payload();
        let signature = github_signature(b"secret", &body);

        verify_github_signature(b"secret", &body, &signature).unwrap();
    }

    #[test]
    fn given_wrong_github_signature_when_verified_then_unauthorized() {
        let body = issue_comment_payload();
        let signature = github_signature(b"other", &body);

        let err = verify_github_signature(b"secret", &body, &signature).unwrap_err();

        assert!(matches!(err, WebhookError::Unauthorized(_)));
    }

    #[test]
    fn given_missing_signature_prefix_when_verified_then_unauthorized() {
        let body = issue_comment_payload();
        let signature = github_signature(b"secret", &body).replace("sha256=", "");

        let err = verify_github_signature(b"secret", &body, &signature).unwrap_err();

        assert!(matches!(err, WebhookError::Unauthorized(_)));
    }
}
