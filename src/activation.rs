use std::{
    error::Error,
    fmt,
    fs::File,
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};

use dhttp::{
    certificate::DhttpSubjectKeyIdentifier,
    home::{DhttpHome, HomeScope, identity::IdentityProfile},
    name::DhttpName,
};
use rankey::EncodePem;
use reqwest::Client;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    sync::Mutex,
    time::{MissedTickBehavior, interval},
};
use uuid::Uuid;
use x509_parser::{
    extensions::GeneralName,
    prelude::{FromDer, X509Certificate},
};

use crate::config::CertServerConfig;
use crate::runtime::{RuntimeBridge, RuntimeIdentity, RuntimeStatus};

mod storage;

use storage::{
    atomic_write, create_private_dir, read_json, read_string, sync_directory, write_new_file,
};

const ACTIVE_FILE: &str = "active.json";
const OPERATION_LOCK_FILE: &str = "operation.lock";
const RUNTIME_GENERATION_FILE: &str = "runtime-generation.json";
const STATE_DIR_NAME: &str = ".kundy";
const PENDING_DIR: &str = "pending";
const PENDING_METADATA_FILE: &str = "state.json";
const PENDING_KEY_FILE: &str = "privkey.pem";
const PENDING_CSR_FILE: &str = "request.csr.pem";
const STATE_VERSION: u8 = 1;
const GENMETA: &str = "genmeta";
const RUNTIME_CONTROLLER_INTERVAL: Duration = Duration::from_secs(10);

type BoxError = Box<dyn Error + Send + Sync>;

#[derive(Clone)]
pub struct ActivationService {
    inner: Arc<ActivationServiceInner>,
}

struct ActivationServiceInner {
    state_dir: PathBuf,
    dhttp_home: DhttpHome,
    certserver_http_url: Arc<str>,
    http: Client,
    operation_lock: Mutex<()>,
    runtime_bridge: RuntimeBridge,
}

#[derive(Debug)]
pub enum ActivationError {
    Remote {
        status: u16,
        error: Value,
    },
    CertServerUnavailable,
    NameUnavailable,
    PendingNameConflict {
        domain_name: String,
    },
    PendingCodeConflict,
    NotActive,
    RecoveryCredentialUnavailable,
    DeactivationInProgress,
    DeactivationCredentialInvalid,
    Internal {
        context: &'static str,
        source: BoxError,
    },
}

impl fmt::Display for ActivationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote { status, .. } => write!(formatter, "CertServer returned HTTP {status}"),
            Self::CertServerUnavailable => formatter.write_str("CertServer is unavailable"),
            Self::NameUnavailable => {
                formatter.write_str("the requested activation name is unavailable")
            }
            Self::PendingNameConflict { domain_name } => {
                write!(formatter, "activation is already pending for {domain_name}")
            }
            Self::PendingCodeConflict => {
                formatter.write_str("activation is already pending with another activation code")
            }
            Self::NotActive => formatter.write_str("the device is not activated"),
            Self::RecoveryCredentialUnavailable => {
                formatter.write_str("the activation recovery credential is unavailable")
            }
            Self::DeactivationInProgress => {
                formatter.write_str("device deactivation is already in progress")
            }
            Self::DeactivationCredentialInvalid => {
                formatter.write_str("the activation code does not match this device")
            }
            Self::Internal { context, .. } => formatter.write_str(context),
        }
    }
}

impl Error for ActivationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Internal { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ActivationCodeInspection {
    pub parent_domain: String,
    pub bound_subname: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NameAvailability {
    pub status: String,
    pub subname: String,
    pub domain_name: String,
    pub display_name: String,
    pub available: bool,
    pub name_locked: bool,
    pub unavailable_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaimResponse {
    operation_id: String,
    status: String,
    subname: String,
    device_name: String,
    domain_name: String,
    display_name: String,
    certificate: CertificateResponse,
}

#[derive(Debug, Deserialize)]
struct CertificateResponse {
    domain: String,
    sequence: u32,
    kind: String,
    ski: Option<String>,
    status: String,
    cert_pem: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingMetadata {
    version: u8,
    operation: PendingOperation,
    subname: String,
    domain_name: String,
    idempotency_key: String,
    activation_code_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PendingOperation {
    Claim,
    Recover,
}

struct PendingActivation {
    metadata: PendingMetadata,
    key_pem: String,
    csr_pem: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActiveState {
    version: u8,
    operation_id: String,
    status: String,
    subname: String,
    device_name: String,
    domain_name: String,
    display_name: String,
    activation_code_sha256: String,
    #[serde(default)]
    activation_code: Option<String>,
    #[serde(default)]
    runtime_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    deactivation_generation: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct RuntimeGenerationState {
    version: u8,
    generation: u64,
}

#[derive(Debug, Serialize)]
pub struct ActivationResult {
    pub state: &'static str,
    pub domain_name: String,
    pub display_name: String,
}

#[derive(Debug, Serialize)]
pub struct CertificateRenewalResult {
    pub state: &'static str,
}

#[derive(Debug, Serialize)]
pub struct DeactivationResult {
    pub state: &'static str,
    pub runtime_state: &'static str,
}

#[derive(Debug, Serialize)]
pub struct DeviceStatus {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct CertificateInfo {
    pub subject: String,
    pub issuer: String,
    pub serial_number: String,
    pub valid_from: i64,
    pub valid_until: i64,
    pub fingerprint_sha256: String,
}

pub struct CertificateDownload {
    pub filename: String,
    pub pem: Vec<u8>,
}

impl From<ActiveState> for ActivationResult {
    fn from(active: ActiveState) -> Self {
        Self {
            state: "active",
            domain_name: active.domain_name,
            display_name: active.display_name,
        }
    }
}

impl ActivationService {
    pub fn new(certserver: &CertServerConfig) -> Result<Self, ActivationError> {
        let dhttp_home = DhttpHome::load(HomeScope::User)
            .map_err(|source| internal("failed to locate the user DHTTP home", source))?;
        let user_home = dirs::home_dir()
            .ok_or_else(|| internal_without_source("failed to locate the user home"))?;
        let state_dir = state_dir_for(&user_home);
        tracing::info!(state_dir = %state_dir.display(), "activation state directory resolved");
        Ok(Self::with_home(state_dir, certserver, dhttp_home))
    }

    fn with_home(state_dir: PathBuf, certserver: &CertServerConfig, dhttp_home: DhttpHome) -> Self {
        Self {
            inner: Arc::new(ActivationServiceInner {
                state_dir,
                dhttp_home,
                certserver_http_url: Arc::from(
                    certserver.base_url.trim_end_matches('/').to_string(),
                ),
                http: Client::new(),
                operation_lock: Mutex::new(()),
                runtime_bridge: RuntimeBridge::new(),
            }),
        }
    }

    pub async fn status(&self) -> Result<DeviceStatus, ActivationError> {
        if let Some(active) = self.load_active().await? {
            return Ok(
                if active.deactivation_generation.is_some()
                    || self.identity_is_installed(&active).await?
                {
                    DeviceStatus {
                        state: "active",
                        display_name: Some(active.display_name),
                    }
                } else {
                    DeviceStatus {
                        state: "claiming",
                        display_name: None,
                    }
                },
            );
        }
        Ok(DeviceStatus {
            state: if self.load_pending().await?.is_some() {
                "claiming"
            } else {
                "factory_ready"
            },
            display_name: None,
        })
    }

    pub async fn certificate_info(&self) -> Result<Option<CertificateInfo>, ActivationError> {
        let Some((_, profile)) = self.active_identity_profile().await? else {
            return Ok(None);
        };
        let certs = profile
            .load_certs()
            .await
            .map_err(|source| internal("failed to load the active certificate", source))?;
        let leaf = certs.first().ok_or_else(|| {
            internal_without_source("the active certificate chain does not contain a leaf")
        })?;

        parse_certificate_info(leaf.as_ref()).map(Some)
    }

    pub async fn active_domain_name(&self) -> Result<Option<String>, ActivationError> {
        let Some(active) = self.load_active().await? else {
            return Ok(None);
        };
        if active.deactivation_generation.is_some() {
            return Ok(None);
        }
        if !self.identity_is_installed(&active).await? {
            return Ok(None);
        }
        Ok(Some(active.domain_name))
    }

    // 只有服务端控制器可在显式激活操作结束后续作宿主机运行态；页面和状态接口保持只读。
    //
    // Only the server-side controller may continue host runtime work after an explicit activation
    // operation; pages and status endpoints remain read-only.
    pub(crate) async fn run_host_runtime_controller(self) {
        let mut timer = interval(RUNTIME_CONTROLLER_INTERVAL);
        timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            timer.tick().await;
            if let Err(error) = self.continue_pending_runtime_operation().await {
                tracing::error!(
                    error = %error,
                    source = ?std::error::Error::source(&error),
                    "failed to continue the persisted host runtime operation"
                );
            }
        }
    }

    async fn continue_pending_runtime_operation(&self) -> Result<(), ActivationError> {
        let _guard = self.inner.operation_lock.lock().await;
        let _file_guard = self.acquire_operation_file_lock().await?;
        let Some(mut active) = self.load_active().await? else {
            return Ok(());
        };
        if active.deactivation_generation.is_some() {
            self.apply_deactivation_runtime_locked(&mut active).await?;
            return Ok(());
        }
        if !self.identity_is_installed(&active).await? {
            return Ok(());
        }
        let status = self
            .inner
            .runtime_bridge
            .status(active.runtime_generation.max(1))
            .await
            .map_err(|source| internal("failed to inspect host runtime status", source))?;
        if status.state != "ready" {
            self.apply_activation_runtime_locked(&mut active).await?;
        }
        Ok(())
    }

    pub async fn runtime_status(&self) -> Result<RuntimeStatus, ActivationError> {
        let active = self
            .load_active()
            .await?
            .ok_or(ActivationError::NotActive)?;
        if let Some(generation) = active.deactivation_generation {
            return self
                .inner
                .runtime_bridge
                .deactivation_status(generation)
                .await
                .map_err(|source| {
                    internal("failed to inspect host runtime deactivation status", source)
                });
        }
        if !self.identity_is_installed(&active).await? {
            return Err(ActivationError::NotActive);
        }
        self.inner
            .runtime_bridge
            .status(active.runtime_generation.max(1))
            .await
            .map_err(|source| internal("failed to inspect host runtime status", source))
    }

    pub async fn certificate_download(
        &self,
    ) -> Result<Option<CertificateDownload>, ActivationError> {
        let Some((active, profile)) = self.active_identity_profile().await? else {
            return Ok(None);
        };
        let pem = fs::read(
            profile
                .ssl_dir()
                .join(dhttp::home::identity::ssl::CERT_FILE_NAME),
        )
        .await
        .map_err(|source| internal("failed to read the active certificate chain", source))?;

        Ok(Some(CertificateDownload {
            filename: format!("{}.crt.pem", active.device_name),
            pem,
        }))
    }

    pub async fn claim(
        &self,
        activation_code: &str,
        subname: &str,
    ) -> Result<ActivationResult, ActivationError> {
        let _guard = self.inner.operation_lock.lock().await;
        let _file_guard = self.acquire_operation_file_lock().await?;

        let active = self.load_active().await?;
        if let Some(existing) = active.as_ref()
            && existing.deactivation_generation.is_some()
        {
            let mut active = existing.clone();
            let _ = self.apply_deactivation_runtime_locked(&mut active).await?;
            return Err(ActivationError::DeactivationInProgress);
        }
        if let Some(existing) = active.as_ref()
            && self.identity_is_installed(existing).await?
        {
            let mut active = existing.clone();
            let _ = self.apply_activation_runtime_locked(&mut active).await?;
            return Ok(active.into());
        }

        let availability = self.check_name(activation_code, subname).await?;
        if !availability.available {
            return Err(ActivationError::NameUnavailable);
        }
        let activation_code_sha256 = activation_code_digest(activation_code);
        if let Some(active) = &active {
            if active.activation_code_sha256 != activation_code_sha256 {
                return Err(ActivationError::PendingCodeConflict);
            }
            if active.domain_name != availability.domain_name {
                return Err(ActivationError::PendingNameConflict {
                    domain_name: active.domain_name.clone(),
                });
            }
        }

        let identity_name = DhttpName::try_from(availability.domain_name.as_str())
            .map_err(|source| internal("CertServer returned an invalid DHTTP name", source))?;
        let operation = if availability.status == "active" && availability.name_locked {
            PendingOperation::Recover
        } else {
            PendingOperation::Claim
        };
        let pending = self
            .load_or_create_pending(&availability, activation_code, operation)
            .await?;

        let (path, payload) = match pending.metadata.operation {
            PendingOperation::Recover => (
                "/v2/activation/recover",
                json!({
                    "activation_code": activation_code,
                    "csr": &pending.csr_pem,
                }),
            ),
            PendingOperation::Claim => (
                "/v2/activation/claim",
                json!({
                    "activation_code": activation_code,
                    "subname": &pending.metadata.subname,
                    "csr": &pending.csr_pem,
                }),
            ),
        };

        let response: ClaimResponse = self
            .certserver_post(path, payload, Some(&pending.metadata.idempotency_key))
            .await?;
        validate_claim_response(&pending.metadata, &response)?;

        self.install_identity(identity_name, &response.certificate, &pending.key_pem)
            .await?;

        let runtime_generation = self.advance_runtime_generation(0).await?;
        let mut active = ActiveState {
            version: STATE_VERSION,
            operation_id: response.operation_id,
            status: response.status,
            subname: response.subname,
            device_name: response.device_name,
            domain_name: response.domain_name,
            display_name: response.display_name,
            activation_code_sha256,
            activation_code: Some(normalize_activation_code(activation_code)),
            runtime_generation,
            deactivation_generation: None,
        };
        self.save_active(&active).await?;
        self.clear_pending().await;

        // 即使可选的宿主机集成尚未安装，证书和身份安装也已完成；服务端运行态控制器会
        // 根据持久化状态继续处理。
        //
        // Certificate and identity installation remains complete without the optional host
        // integration; the server-side runtime controller continues from persisted state.
        let _ = self.apply_activation_runtime_locked(&mut active).await?;

        Ok(active.into())
    }

    pub async fn renew_certificate(&self) -> Result<CertificateRenewalResult, ActivationError> {
        let _guard = self.inner.operation_lock.lock().await;
        let _file_guard = self.acquire_operation_file_lock().await?;
        let mut active = self
            .load_active()
            .await?
            .ok_or(ActivationError::NotActive)?;
        if active.deactivation_generation.is_some() {
            return Err(ActivationError::DeactivationInProgress);
        }

        self.recover_identity(&mut active).await?;
        active.runtime_generation = self
            .advance_runtime_generation(active.runtime_generation)
            .await?;

        self.save_active(&active).await?;
        self.clear_pending().await;
        let _ = self.apply_activation_runtime_locked(&mut active).await?;
        Ok(CertificateRenewalResult { state: "active" })
    }

    /// Remove the local identity and ask the host helpers to disable the remote runtime.
    /// CertServer activation records are deliberately left untouched so the same code can be
    /// used to activate this appliance again without changing the server-side binding.
    pub async fn deactivate(&self) -> Result<DeactivationResult, ActivationError> {
        self.deactivate_inner(None).await
    }

    pub async fn deactivate_with_code(
        &self,
        activation_code: &str,
    ) -> Result<DeactivationResult, ActivationError> {
        self.deactivate_inner(Some(activation_code)).await
    }

    async fn deactivate_inner(
        &self,
        confirmation_code: Option<&str>,
    ) -> Result<DeactivationResult, ActivationError> {
        let _guard = self.inner.operation_lock.lock().await;
        let _file_guard = self.acquire_operation_file_lock().await?;

        let Some(mut active) = self.load_active().await? else {
            self.clear_pending().await;
            return Ok(DeactivationResult {
                state: "factory_ready",
                runtime_state: "ready",
            });
        };

        if let Some(confirmation_code) = confirmation_code {
            let normalized = normalize_activation_code(confirmation_code);
            let credential_matches = active
                .activation_code
                .as_deref()
                .is_some_and(|saved| normalize_activation_code(saved) == normalized)
                || activation_code_digest(&normalized) == active.activation_code_sha256;
            if !credential_matches {
                return Err(ActivationError::DeactivationCredentialInvalid);
            }
        }

        let helper_available = self
            .inner
            .runtime_bridge
            .host_available()
            .await
            .map_err(|source| internal("failed to inspect host runtime bridge", source))?;
        if !helper_available {
            self.finish_deactivation(&active).await?;
            return Ok(DeactivationResult {
                state: "factory_ready",
                runtime_state: "pending",
            });
        }

        if active.deactivation_generation.is_none() {
            let generation = self
                .advance_runtime_generation(active.runtime_generation)
                .await?;
            active.runtime_generation = generation;
            active.deactivation_generation = Some(generation);
            // 在第一个 helper 请求前保存停用意图，使其他 Kundy 进程只能继续同一停用，
            // 不能再把仍保留的本地身份解释为待激活状态。
            //
            // Persist the intent before the first helper request so other Kundy processes can
            // only continue this deactivation instead of reactivating the retained identity.
            self.save_active(&active).await?;
        }

        let status = self.apply_deactivation_runtime_locked(&mut active).await?;
        Ok(DeactivationResult {
            state: if status.state == "ready" {
                "factory_ready"
            } else {
                "active"
            },
            runtime_state: status.state,
        })
    }

    async fn apply_deactivation_runtime_locked(
        &self,
        active: &mut ActiveState,
    ) -> Result<RuntimeStatus, ActivationError> {
        let generation = active
            .deactivation_generation
            .ok_or_else(|| internal_without_source("deactivation generation is missing"))?;
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        let profile = self.inner.dhttp_home.identity_profile(name);
        let mut identity = RuntimeIdentity {
            device_name: active.device_name.clone(),
            generation,
            profile_dir: profile.path().to_path_buf(),
        };
        let mut config_status = self
            .inner
            .runtime_bridge
            .deactivate_config(&identity)
            .await
            .map_err(|source| internal("failed to deactivate host runtime config", source))?;
        if config_status.code.as_deref() == Some("stale_generation")
            && let Some(host_generation) = config_status.current_generation
        {
            let generation = self.advance_runtime_generation(host_generation).await?;
            active.runtime_generation = generation;
            active.deactivation_generation = Some(generation);
            self.save_active(active).await?;
            identity.generation = generation;
            config_status = self
                .inner
                .runtime_bridge
                .deactivate_config(&identity)
                .await
                .map_err(|source| {
                    internal("failed to migrate host runtime deactivation", source)
                })?;
        }
        if config_status.state != "ready" {
            return Ok(config_status);
        }

        self.inner
            .runtime_bridge
            .remove_identity_config(&identity)
            .await
            .map_err(|source| internal("failed to remove the Pishoo identity config", source))?;
        let pishoo_status = self
            .inner
            .runtime_bridge
            .deactivate_pishoo(&identity)
            .await
            .map_err(|source| internal("failed to reload Pishoo after deactivation", source))?;
        if pishoo_status.state != "ready" {
            tracing::warn!(
                state = pishoo_status.state,
                code = ?pishoo_status.code,
                generation = identity.generation,
                "host Pishoo deactivation is pending; local identity is retained"
            );
            return Ok(pishoo_status);
        }

        self.finish_deactivation(active).await?;
        Ok(pishoo_status)
    }

    async fn finish_deactivation(&self, active: &ActiveState) -> Result<(), ActivationError> {
        self.remove_active_identity(active).await?;
        self.clear_pending().await;
        self.remove_active_state().await
    }

    pub async fn inspect_activation_code(
        &self,
        activation_code: &str,
    ) -> Result<ActivationCodeInspection, ActivationError> {
        self.certserver_post(
            "/v2/activation/inspect",
            json!({"activation_code": activation_code}),
            None,
        )
        .await
    }

    pub async fn check_name(
        &self,
        activation_code: &str,
        subname: &str,
    ) -> Result<NameAvailability, ActivationError> {
        self.certserver_post(
            "/v2/activation/names/check",
            json!({
                "activation_code": activation_code,
                "subname": subname,
            }),
            None,
        )
        .await
    }

    async fn apply_activation_runtime_locked(
        &self,
        active: &mut ActiveState,
    ) -> Result<RuntimeStatus, ActivationError> {
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        let profile = self.inner.dhttp_home.identity_profile(name.clone());
        let access_initialized = ensure_default_access(&profile).await?;
        active.runtime_generation = self
            .synchronize_runtime_generation(active.runtime_generation)
            .await?;
        if access_initialized {
            active.runtime_generation =
                reconciled_generation(active.runtime_generation, access_initialized);
            self.save_runtime_generation(active.runtime_generation)
                .await?;
            // 调用辅助程序前先持久化，避免部分失败丢失要求 Pishoo 加载新访问库的代次。
            //
            // Persist before invoking helpers so a partial failure cannot lose the generation
            // that forces Pishoo to load the new access store.
            self.save_active(active).await?;
        }
        let mut identity = RuntimeIdentity {
            device_name: active.device_name.clone(),
            generation: active.runtime_generation,
            profile_dir: profile.path().to_path_buf(),
        };
        let mut status = self
            .inner
            .runtime_bridge
            .activate(&identity)
            .await
            .map_err(|source| internal("failed to reconcile host runtime", source))?;
        if status.code.as_deref() == Some("stale_generation")
            && let Some(host_generation) = status.current_generation
        {
            // 旧版本会在取消激活时删除唯一的本地代次。宿主机返回已应用水位后，选择更大的
            // 新代次重试，既完成迁移，也保留 helper 的防回滚边界。
            //
            // Older releases removed their only local generation during deactivation. When the
            // host reports its applied watermark, retry above it while retaining rollback checks.
            active.runtime_generation = self.advance_runtime_generation(host_generation).await?;
            self.save_active(active).await?;
            identity.generation = active.runtime_generation;
            status = self
                .inner
                .runtime_bridge
                .activate(&identity)
                .await
                .map_err(|source| internal("failed to reconcile migrated host runtime", source))?;
        }
        // 激活状态始终是真实数据源；运行态辅助结果文件是临时数据，可在启动时重新生成。
        //
        // Active state remains the source of truth; runtime helper result files are ephemeral
        // and can be regenerated on boot.
        if status.state == "error" {
            tracing::error!(
                generation = status.generation,
                code = ?status.code,
                "host runtime integration rejected the active identity"
            );
        }
        self.save_active(active).await?;
        Ok(status)
    }

    async fn recover_identity(&self, active: &mut ActiveState) -> Result<(), ActivationError> {
        let activation_code = active
            .activation_code
            .as_deref()
            .ok_or(ActivationError::RecoveryCredentialUnavailable)?;
        let pending = self.load_or_create_recovery_pending(active).await?;
        let response: ClaimResponse = self
            .certserver_post(
                "/v2/activation/recover",
                json!({
                    "activation_code": activation_code,
                    "csr": &pending.csr_pem,
                }),
                Some(&pending.metadata.idempotency_key),
            )
            .await?;
        validate_recovery_response(active, &response)?;
        self.install_active_identity(active, &response.certificate, &pending.key_pem)
            .await?;

        active.operation_id = response.operation_id;
        active.status = response.status;
        active.subname = response.subname;
        active.device_name = response.device_name;
        active.domain_name = response.domain_name;
        active.display_name = response.display_name;
        Ok(())
    }

    async fn load_or_create_recovery_pending(
        &self,
        active: &ActiveState,
    ) -> Result<PendingActivation, ActivationError> {
        if let Some(pending) = self.load_pending().await? {
            if pending.metadata.operation == PendingOperation::Recover
                && pending.metadata.domain_name == active.domain_name
                && pending.metadata.subname == active.subname
                && pending.metadata.activation_code_sha256 == active.activation_code_sha256
            {
                return Ok(pending);
            }
            self.clear_pending().await;
        }

        let (key_pem, csr_pem) = generate_identity_request(&active.domain_name)?;
        let pending = PendingActivation {
            metadata: PendingMetadata {
                version: STATE_VERSION,
                operation: PendingOperation::Recover,
                subname: active.subname.clone(),
                domain_name: active.domain_name.clone(),
                idempotency_key: Uuid::new_v4().to_string(),
                activation_code_sha256: active.activation_code_sha256.clone(),
            },
            key_pem,
            csr_pem,
        };
        self.save_pending(&pending).await?;
        Ok(pending)
    }

    async fn install_active_identity(
        &self,
        active: &ActiveState,
        certificate: &CertificateResponse,
        key_pem: &str,
    ) -> Result<(), ActivationError> {
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        self.install_identity(name, certificate, key_pem).await
    }

    async fn certserver_post<T: DeserializeOwned>(
        &self,
        path: &str,
        payload: Value,
        idempotency_key: Option<&str>,
    ) -> Result<T, ActivationError> {
        let url = format!("{}{}", self.inner.certserver_http_url, path);
        let mut request = self
            .inner
            .http
            .post(url)
            .timeout(Duration::from_secs(30))
            .json(&payload);
        if let Some(value) = idempotency_key {
            request = request.header("Idempotency-Key", value);
        }
        let response = request
            .send()
            .await
            .map_err(|_| ActivationError::CertServerUnavailable)?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| ActivationError::CertServerUnavailable)?;
        if !status.is_success() {
            let payload = serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| {
                json!({"error": {"code": "certserver_error", "message": "远程激活服务请求失败。"}})
            });
            return Err(ActivationError::Remote {
                status: status.as_u16(),
                error: payload.get("error").cloned().unwrap_or(payload),
            });
        }

        serde_json::from_slice(&body).map_err(|source| {
            internal("failed to parse the CertServer activation response", source)
        })
    }

    async fn load_or_create_pending(
        &self,
        availability: &NameAvailability,
        activation_code: &str,
        operation: PendingOperation,
    ) -> Result<PendingActivation, ActivationError> {
        let activation_code_sha256 = activation_code_digest(activation_code);
        if let Some(pending) = self.load_pending().await? {
            if pending.metadata.activation_code_sha256 != activation_code_sha256 {
                return Err(ActivationError::PendingCodeConflict);
            }
            if pending.metadata.domain_name != availability.domain_name {
                return Err(ActivationError::PendingNameConflict {
                    domain_name: pending.metadata.domain_name,
                });
            }
            return Ok(pending);
        }

        let (key_pem, csr_pem) = generate_identity_request(&availability.domain_name)?;
        let pending = PendingActivation {
            metadata: PendingMetadata {
                version: STATE_VERSION,
                operation,
                subname: availability.subname.clone(),
                domain_name: availability.domain_name.clone(),
                idempotency_key: Uuid::new_v4().to_string(),
                activation_code_sha256,
            },
            key_pem,
            csr_pem,
        };
        self.save_pending(&pending).await?;
        Ok(pending)
    }

    async fn load_pending(&self) -> Result<Option<PendingActivation>, ActivationError> {
        let path = self.pending_path();
        if !fs::try_exists(&path)
            .await
            .map_err(|source| internal("failed to inspect activation recovery state", source))?
        {
            return Ok(None);
        }
        let metadata: PendingMetadata = read_json(path.join(PENDING_METADATA_FILE)).await?;
        if metadata.version != STATE_VERSION {
            return Err(internal_without_source(
                "the activation recovery state version is unsupported",
            ));
        }
        let key_pem = read_string(path.join(PENDING_KEY_FILE)).await?;
        let csr_pem = read_string(path.join(PENDING_CSR_FILE)).await?;
        Ok(Some(PendingActivation {
            metadata,
            key_pem,
            csr_pem,
        }))
    }

    async fn save_pending(&self, pending: &PendingActivation) -> Result<(), ActivationError> {
        create_private_dir(&self.inner.state_dir).await?;
        let stage_path = self
            .inner
            .state_dir
            .join(format!(".pending-stage-{}", Uuid::new_v4()));
        create_private_dir(&stage_path).await?;

        let result = async {
            let metadata = serde_json::to_vec_pretty(&pending.metadata).map_err(|source| {
                internal("failed to serialize activation recovery state", source)
            })?;
            write_new_file(stage_path.join(PENDING_METADATA_FILE), &metadata, 0o600).await?;
            write_new_file(
                stage_path.join(PENDING_KEY_FILE),
                pending.key_pem.as_bytes(),
                0o400,
            )
            .await?;
            write_new_file(
                stage_path.join(PENDING_CSR_FILE),
                pending.csr_pem.as_bytes(),
                0o600,
            )
            .await?;
            sync_directory(stage_path.clone()).await?;
            fs::rename(&stage_path, self.pending_path())
                .await
                .map_err(|source| internal("failed to commit activation recovery state", source))?;
            sync_directory(self.inner.state_dir.clone()).await
        }
        .await;

        if result.is_err() {
            let _ = fs::remove_dir_all(&stage_path).await;
        }
        result
    }

    async fn load_active(&self) -> Result<Option<ActiveState>, ActivationError> {
        let path = self.active_path();
        if !fs::try_exists(&path)
            .await
            .map_err(|source| internal("failed to inspect active device state", source))?
        {
            return Ok(None);
        }
        let active: ActiveState = read_json(path).await?;
        if active.version != STATE_VERSION {
            return Err(internal_without_source(
                "the active device state version is unsupported",
            ));
        }
        Ok(Some(active))
    }

    async fn save_active(&self, active: &ActiveState) -> Result<(), ActivationError> {
        create_private_dir(&self.inner.state_dir).await?;
        let payload = serde_json::to_vec_pretty(active)
            .map_err(|source| internal("failed to serialize active device state", source))?;
        atomic_write(
            &self.inner.state_dir,
            self.active_path(),
            ".active",
            &payload,
            0o600,
        )
        .await
    }

    async fn load_runtime_generation(&self) -> Result<u64, ActivationError> {
        let path = self.runtime_generation_path();
        let payload = match fs::read(&path).await {
            Ok(payload) => payload,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(source) => {
                return Err(internal(
                    "failed to read the runtime generation watermark",
                    source,
                ));
            }
        };
        let state: RuntimeGenerationState = serde_json::from_slice(&payload).map_err(|source| {
            internal("failed to parse the runtime generation watermark", source)
        })?;
        if state.version != STATE_VERSION {
            return Err(internal_without_source(
                "the runtime generation watermark version is unsupported",
            ));
        }
        Ok(state.generation)
    }

    async fn synchronize_runtime_generation(
        &self,
        generation: u64,
    ) -> Result<u64, ActivationError> {
        let persisted = self.load_runtime_generation().await?;
        let generation = generation.max(persisted).max(1);
        if persisted != generation {
            self.save_runtime_generation(generation).await?;
        }
        Ok(generation)
    }

    async fn advance_runtime_generation(&self, generation: u64) -> Result<u64, ActivationError> {
        let persisted = self.load_runtime_generation().await?;
        let generation = next_runtime_generation(generation, persisted);
        self.save_runtime_generation(generation).await?;
        Ok(generation)
    }

    async fn save_runtime_generation(&self, generation: u64) -> Result<(), ActivationError> {
        create_private_dir(&self.inner.state_dir).await?;
        let payload = serde_json::to_vec_pretty(&RuntimeGenerationState {
            version: STATE_VERSION,
            generation,
        })
        .map_err(|source| {
            internal(
                "failed to serialize the runtime generation watermark",
                source,
            )
        })?;
        atomic_write(
            &self.inner.state_dir,
            self.runtime_generation_path(),
            ".runtime-generation",
            &payload,
            0o600,
        )
        .await
    }

    async fn identity_is_installed(&self, active: &ActiveState) -> Result<bool, ActivationError> {
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        let profile = self.inner.dhttp_home.identity_profile(name);
        if !fs::try_exists(profile.path())
            .await
            .map_err(|source| internal("failed to inspect the active DHTTP identity", source))?
        {
            return Ok(false);
        }
        profile
            .load_identity()
            .await
            .map(|_| true)
            .map_err(|source| internal("the active DHTTP identity is invalid", source))
    }

    async fn active_identity_profile(
        &self,
    ) -> Result<Option<(ActiveState, IdentityProfile)>, ActivationError> {
        let Some(active) = self.load_active().await? else {
            return Ok(None);
        };
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        let profile = self.inner.dhttp_home.identity_profile(name);
        if !fs::try_exists(profile.path())
            .await
            .map_err(|source| internal("failed to inspect the active DHTTP identity", source))?
        {
            return Ok(None);
        }
        Ok(Some((active, profile)))
    }

    async fn install_identity(
        &self,
        name: DhttpName<'_>,
        certificate: &CertificateResponse,
        key_pem: &str,
    ) -> Result<(), ActivationError> {
        validate_identity(&name, certificate, key_pem)?;

        let home = &self.inner.dhttp_home;
        create_private_dir(home.as_path()).await?;
        let profile = home.identity_profile(name.clone());
        create_private_dir(profile.path()).await?;
        profile
            .save_identity(certificate.cert_pem.as_bytes(), key_pem.as_bytes())
            .await
            .map_err(|source| internal("failed to install the activated DHTTP identity", source))?;
        profile
            .load_identity()
            .await
            .map_err(|source| internal("failed to verify the installed DHTTP identity", source))?;
        ensure_default_access(&profile).await?;

        let settings_path = home.settings_path();
        let mut settings = if fs::try_exists(&settings_path)
            .await
            .map_err(|source| internal("failed to inspect DHTTP settings", source))?
        {
            home.load_settings()
                .await
                .map_err(|source| internal("failed to load DHTTP settings", source))?
                .settings()
                .clone()
        } else {
            dhttp::home::identity::settings::DhttpSettings::default()
        };
        settings.set_default_identity_name(name.into_owned());
        let payload = toml::to_string_pretty(&settings)
            .map_err(|source| internal("failed to serialize DHTTP settings", source))?;
        atomic_write(
            home.as_path(),
            settings_path,
            ".settings",
            payload.as_bytes(),
            0o600,
        )
        .await
    }

    async fn remove_active_identity(&self, active: &ActiveState) -> Result<(), ActivationError> {
        let name = DhttpName::try_from(active.domain_name.as_str()).map_err(|source| {
            internal("the active device state contains an invalid name", source)
        })?;
        let profile = self.inner.dhttp_home.identity_profile(name);
        let profile_path = profile.path();
        match fs::symlink_metadata(profile_path).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(internal_without_source(
                    "the active DHTTP identity path is a symlink",
                ));
            }
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(profile_path).await.map_err(|source| {
                    internal("failed to remove the active DHTTP identity", source)
                })?;
            }
            Ok(_) => {
                return Err(internal_without_source(
                    "the active DHTTP identity path is not a directory",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(internal(
                    "failed to inspect the active DHTTP identity",
                    source,
                ));
            }
        }

        let settings_path = self.inner.dhttp_home.settings_path();
        let settings_metadata = match fs::symlink_metadata(&settings_path).await {
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(source) => return Err(internal("failed to inspect DHTTP settings", source)),
        };
        let Some(metadata) = settings_metadata else {
            return Ok(());
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(internal_without_source(
                "the DHTTP settings path is invalid",
            ));
        }

        let payload = fs::read_to_string(&settings_path)
            .await
            .map_err(|source| internal("failed to read DHTTP settings", source))?;
        let mut document = toml::from_str::<toml::Value>(&payload)
            .map_err(|source| internal("failed to parse DHTTP settings", source))?;
        let should_remove = document
            .get("default")
            .and_then(toml::Value::as_table)
            .and_then(|default| default.get("name"))
            .and_then(toml::Value::as_str)
            == Some(active.domain_name.as_str());
        if !should_remove {
            return Ok(());
        }

        let empty = if let Some(default) = document
            .get_mut("default")
            .and_then(toml::Value::as_table_mut)
        {
            default.remove("name");
            default.is_empty()
        } else {
            false
        };
        if empty {
            if let Some(table) = document.as_table_mut() {
                table.remove("default");
            }
        }
        if document.as_table().is_some_and(|table| table.is_empty()) {
            fs::remove_file(settings_path)
                .await
                .map_err(|source| internal("failed to remove empty DHTTP settings", source))?;
        } else {
            let rendered = toml::to_string_pretty(&document)
                .map_err(|source| internal("failed to serialize DHTTP settings", source))?;
            atomic_write(
                self.inner.dhttp_home.as_path(),
                settings_path,
                ".settings",
                rendered.as_bytes(),
                0o600,
            )
            .await?;
        }
        Ok(())
    }

    async fn remove_active_state(&self) -> Result<(), ActivationError> {
        match fs::remove_file(self.active_path()).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(internal("failed to remove active device state", source)),
        }
    }

    fn active_path(&self) -> PathBuf {
        self.inner.state_dir.join(ACTIVE_FILE)
    }

    fn runtime_generation_path(&self) -> PathBuf {
        self.inner.state_dir.join(RUNTIME_GENERATION_FILE)
    }

    fn pending_path(&self) -> PathBuf {
        self.inner.state_dir.join(PENDING_DIR)
    }

    async fn clear_pending(&self) {
        if let Err(error) = fs::remove_dir_all(self.pending_path()).await
            && error.kind() != io::ErrorKind::NotFound
        {
            tracing::warn!(error = %error, "failed to remove committed activation recovery state");
        }
    }

    async fn acquire_operation_file_lock(&self) -> Result<File, ActivationError> {
        create_private_dir(&self.inner.state_dir).await?;
        let path = self.inner.state_dir.join(OPERATION_LOCK_FILE);
        tokio::task::spawn_blocking(move || -> io::Result<File> {
            let mut options = std::fs::OpenOptions::new();
            options.create(true).read(true).write(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let file = options.open(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
            file.lock()?;
            Ok(file)
        })
        .await
        .map_err(|source| internal("failed to join the activation lock task", source))?
        .map_err(|source| internal("failed to lock activation state", source))
    }
}

fn state_dir_for(user_home: &Path) -> PathBuf {
    user_home.join(STATE_DIR_NAME)
}

fn generate_identity_request(domain_name: &str) -> Result<(String, String), ActivationError> {
    let key_pem = rankey::generate_secp384r1_key()
        .map_err(|source| internal("failed to generate the activation private key", source))?
        .to_string();
    let csr_pem = rankey::generate_csr(&key_pem, "CN", domain_name, &[domain_name])
        .map_err(|source| {
            internal(
                "failed to generate the activation certificate request",
                source,
            )
        })?
        .to_pem(rankey::LineEnding::LF)
        .map_err(|source| {
            internal(
                "failed to encode the activation certificate request",
                source,
            )
        })?;
    Ok((key_pem, csr_pem))
}

fn validate_claim_response(
    pending: &PendingMetadata,
    response: &ClaimResponse,
) -> Result<(), ActivationError> {
    let expected_device_name = pending
        .domain_name
        .strip_suffix(DhttpName::SUFFIX)
        .unwrap_or(&pending.domain_name);
    if response.status != "active"
        || response.certificate.status != "active"
        || response.certificate.kind != "primary"
        || response.certificate.sequence != 0
        || response.domain_name != pending.domain_name
        || response.certificate.domain != pending.domain_name
        || response.subname != pending.subname
        || response.device_name != expected_device_name
        || response.display_name != format!("{expected_device_name}~")
        || response
            .certificate
            .ski
            .as_deref()
            .is_none_or(str::is_empty)
        || response.certificate.cert_pem.trim().is_empty()
    {
        return Err(internal_without_source(
            "CertServer returned an inconsistent activation result",
        ));
    }
    Ok(())
}

fn validate_recovery_response(
    active: &ActiveState,
    response: &ClaimResponse,
) -> Result<(), ActivationError> {
    if response.status != "active"
        || response.subname != active.subname
        || response.device_name != active.device_name
        || response.domain_name != active.domain_name
        || response.display_name != active.display_name
    {
        return Err(internal_without_source(
            "CertServer returned an inconsistent activation recovery result",
        ));
    }
    validate_certificate_response(active, &response.certificate)
}

fn validate_certificate_response(
    active: &ActiveState,
    certificate: &CertificateResponse,
) -> Result<(), ActivationError> {
    if certificate.status != "active"
        || certificate.kind != "primary"
        || certificate.sequence != 0
        || certificate.domain != active.domain_name
        || certificate.ski.as_deref().is_none_or(str::is_empty)
        || certificate.cert_pem.trim().is_empty()
    {
        return Err(internal_without_source(
            "CertServer returned an inconsistent certificate renewal result",
        ));
    }
    Ok(())
}

fn validate_identity(
    name: &DhttpName<'_>,
    certificate: &CertificateResponse,
    key_pem: &str,
) -> Result<(), ActivationError> {
    let certs = CertificateDer::pem_slice_iter(certificate.cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| internal("failed to parse the activated certificate chain", source))?;
    if certs.is_empty() {
        return Err(internal_without_source(
            "the activated certificate chain does not contain a leaf",
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
        .map_err(|source| internal("failed to parse the generated private key", source))?;
    let identity = dhttp::identity::Identity::new(name.clone().into_name(), certs, key);
    let leaf = identity.certs().first().ok_or_else(|| {
        internal_without_source("the activated certificate chain does not contain a leaf")
    })?;

    let proof = b"kundy identity validation";
    let signature = identity
        .sign(proof)
        .map_err(|source| internal("the generated DHTTP private key is unusable", source))?;
    let key_matches_certificate = identity
        .verify(proof, &signature)
        .map_err(|source| internal("the activated DHTTP certificate is unusable", source))?;
    if !key_matches_certificate {
        return Err(internal_without_source(
            "the activated DHTTP certificate does not match its private key",
        ));
    }

    let (_, leaf_certificate) = X509Certificate::from_der(leaf.as_ref())
        .map_err(|source| internal("failed to parse the activated leaf certificate", source))?;
    let subject_alternative_name =
        leaf_certificate
            .subject_alternative_name()
            .map_err(|source| {
                internal(
                    "failed to parse the activated certificate subject names",
                    source,
                )
            })?;
    let has_expected_name = subject_alternative_name.as_ref().is_some_and(|extension| {
        extension.value.general_names.iter().any(|entry| {
            matches!(entry, GeneralName::DNSName(candidate) if candidate.eq_ignore_ascii_case(name.as_full()))
        })
    });
    if !has_expected_name {
        return Err(internal_without_source(
            "the activated certificate does not contain the permanent DHTTP name",
        ));
    }

    let response_ski = certificate.ski.as_deref().ok_or_else(|| {
        internal_without_source("the activation response is missing DHTTP certificate metadata")
    })?;
    let response_ski =
        DhttpSubjectKeyIdentifier::try_from_subject_key_identifier_bytes(response_ski.as_bytes())
            .map_err(|source| {
            internal(
                "the activation response contains invalid DHTTP certificate metadata",
                source,
            )
        })?;
    let leaf_ski = dhttp::identity::extract_dhttp_subject_key_identifier(identity.certs())
        .map_err(|source| {
            internal(
                "the activated certificate has invalid DHTTP certificate metadata",
                source,
            )
        })?;
    if leaf_ski != response_ski
        || leaf_ski.chain().usage().kind_flag() != "0"
        || leaf_ski.chain().sequence().get() != 0
    {
        return Err(internal_without_source(
            "the activated certificate metadata does not match the activation response",
        ));
    }

    Ok(())
}

fn activation_code_digest(value: &str) -> String {
    use std::fmt::Write as _;

    Sha256::digest(normalize_activation_code(value))
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn normalize_activation_code(value: &str) -> String {
    value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_uppercase() as char)
        .collect()
}

fn reconciled_generation(current: u64, access_initialized: bool) -> u64 {
    let current = current.max(1);
    if access_initialized {
        current.saturating_add(1)
    } else {
        current
    }
}

fn next_runtime_generation(current: u64, persisted: u64) -> u64 {
    current.max(persisted).saturating_add(1).max(1)
}

async fn ensure_default_access(profile: &IdentityProfile) -> Result<bool, ActivationError> {
    let database = profile.access_db_path();
    if fs::try_exists(&database)
        .await
        .map_err(|source| internal("failed to inspect the DHTTP access store", source))?
    {
        return Ok(false);
    }

    let identity_name = profile.name().as_partial().to_owned();
    let output =
        tokio::task::spawn_blocking(move || default_access_command(&identity_name).output())
            .await
            .map_err(|source| internal("failed to join the gmutils access task", source))?
            .map_err(|source| internal("failed to start gmutils access configuration", source))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if detail.is_empty() {
            format!("genmeta access exited with {}", output.status)
        } else {
            format!("genmeta access exited with {}: {detail}", output.status)
        };
        return Err(internal(
            "failed to configure the default DHTTP access rule",
            io::Error::other(message),
        ));
    }
    if !fs::try_exists(&database)
        .await
        .map_err(|source| internal("failed to verify the DHTTP access store", source))?
    {
        return Err(internal_without_source(
            "gmutils did not create the DHTTP access store",
        ));
    }
    Ok(true)
}

fn default_access_command(identity_name: &str) -> Command {
    let mut command = Command::new(GENMETA);
    command
        .args(["access", "--identity", identity_name, "/", "allow", "*?"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

fn parse_certificate_info(der: &[u8]) -> Result<CertificateInfo, ActivationError> {
    let (_, certificate) = X509Certificate::from_der(der)
        .map_err(|source| internal("failed to parse the active certificate", source))?;
    let common_name = |name: &x509_parser::x509::X509Name<'_>| {
        name.iter_common_name()
            .next()
            .and_then(|attribute| attribute.as_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| name.to_string())
    };
    let fingerprint = Sha256::digest(der)
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<Vec<_>>()
        .join(":");

    Ok(CertificateInfo {
        subject: common_name(certificate.subject()),
        issuer: common_name(certificate.issuer()),
        serial_number: certificate.raw_serial_as_string().to_uppercase(),
        valid_from: certificate.validity().not_before.timestamp(),
        valid_until: certificate.validity().not_after.timestamp(),
        fingerprint_sha256: fingerprint,
    })
}

fn internal(context: &'static str, source: impl Error + Send + Sync + 'static) -> ActivationError {
    ActivationError::Internal {
        context,
        source: Box::new(source),
    }
}

fn internal_without_source(context: &'static str) -> ActivationError {
    internal(context, io::Error::other(context))
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{
        ActivationService, ActiveState, CertificateResponse, ClaimResponse, NameAvailability,
        PendingMetadata, STATE_VERSION, default_access_command, next_runtime_generation,
        normalize_activation_code, reconciled_generation, state_dir_for, validate_claim_response,
    };
    use crate::config::CertServerConfig;

    #[test]
    fn state_directory_is_inside_the_user_home() {
        let state_dir = state_dir_for(Path::new("/home/appliance"));

        assert_eq!(state_dir, Path::new("/home/appliance/.kundy"));
    }

    #[test]
    fn default_access_uses_gmutils_for_the_activated_identity() {
        let command = default_access_command("tsinghua.kundy");
        let args = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(command.get_program(), "genmeta");
        assert_eq!(
            args,
            ["access", "--identity", "tsinghua.kundy", "/", "allow", "*?",]
        );
    }

    #[test]
    fn new_access_store_advances_runtime_generation() {
        assert_eq!(reconciled_generation(1, true), 2);
        assert_eq!(reconciled_generation(2, false), 2);
        assert_eq!(reconciled_generation(u64::MAX, true), u64::MAX);
    }

    #[test]
    fn runtime_generation_advances_above_the_durable_watermark() {
        assert_eq!(next_runtime_generation(0, 0), 1);
        assert_eq!(next_runtime_generation(1, 2), 3);
        assert_eq!(next_runtime_generation(4, 2), 5);
        assert_eq!(next_runtime_generation(u64::MAX, 2), u64::MAX);
    }

    #[tokio::test]
    async fn runtime_generation_survives_active_state_removal() {
        let root =
            std::env::temp_dir().join(format!("kundy-runtime-generation-{}", uuid::Uuid::new_v4()));
        let service = ActivationService::with_home(
            root.join("state"),
            &CertServerConfig::default(),
            dhttp::home::DhttpHome::new(root.join("dhttp")),
        );

        assert_eq!(service.advance_runtime_generation(0).await.unwrap(), 1);
        assert_eq!(service.advance_runtime_generation(1).await.unwrap(), 2);
        service.remove_active_state().await.unwrap();

        let reloaded = ActivationService::with_home(
            root.join("state"),
            &CertServerConfig::default(),
            dhttp::home::DhttpHome::new(root.join("dhttp")),
        );
        assert_eq!(reloaded.advance_runtime_generation(0).await.unwrap(), 3);

        std::fs::remove_dir_all(root).expect("test state should be removable");
    }

    #[test]
    fn pending_metadata_never_contains_the_activation_code() {
        let state = PendingMetadata {
            version: STATE_VERSION,
            operation: super::PendingOperation::Claim,
            subname: "tsinghua".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            idempotency_key: "local-idempotency-key".to_string(),
            activation_code_sha256: "digest".to_string(),
        };
        let value = serde_json::to_value(state).unwrap();

        assert!(value.get("activation_code").is_none());
        assert!(value.get("activation_code_sha256").is_some());
        assert_eq!(value["domain_name"], "tsinghua.kundy.dhttp.net");
    }

    #[test]
    fn claim_result_must_match_the_generated_identity() {
        let pending = PendingMetadata {
            version: STATE_VERSION,
            operation: super::PendingOperation::Claim,
            subname: "tsinghua".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            idempotency_key: "local-idempotency-key".to_string(),
            activation_code_sha256: "digest".to_string(),
        };
        let response = ClaimResponse {
            operation_id: "operation".to_string(),
            status: "active".to_string(),
            subname: "tsinghua".to_string(),
            device_name: "tsinghua.kundy".to_string(),
            domain_name: "other.kundy.dhttp.net".to_string(),
            display_name: "tsinghua.kundy~".to_string(),
            certificate: CertificateResponse {
                domain: "other.kundy.dhttp.net".to_string(),
                sequence: 0,
                kind: "primary".to_string(),
                ski: Some("metadata".to_string()),
                status: "active".to_string(),
                cert_pem: "certificate".to_string(),
            },
        };

        assert!(validate_claim_response(&pending, &response).is_err());
    }

    #[test]
    fn active_state_exposes_only_the_public_identity() {
        let state = ActiveState {
            version: STATE_VERSION,
            operation_id: "operation".to_string(),
            status: "active".to_string(),
            subname: "tsinghua".to_string(),
            device_name: "tsinghua.kundy".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            display_name: "tsinghua.kundy~".to_string(),
            activation_code_sha256: "digest".to_string(),
            activation_code: Some("AAAAABBBBBCCCCCDDDDDEEEEE".to_string()),
            runtime_generation: 1,
            deactivation_generation: None,
        };
        let result = super::ActivationResult::from(state);

        assert_eq!(result.state, "active");
        assert_eq!(result.display_name, "tsinghua.kundy~");
    }

    #[test]
    fn persisted_activation_code_uses_the_canonical_form() {
        assert_eq!(
            normalize_activation_code("aaaaa-bbbbb-ccccc-ddddd-eeeee"),
            "AAAAABBBBBCCCCCDDDDDEEEEE"
        );
    }

    #[test]
    fn active_state_persists_the_recovery_code_without_exposing_it_in_status() {
        let state = ActiveState {
            version: STATE_VERSION,
            operation_id: "operation".to_string(),
            status: "active".to_string(),
            subname: "tsinghua".to_string(),
            device_name: "tsinghua.kundy".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            display_name: "tsinghua.kundy~".to_string(),
            activation_code_sha256: "digest".to_string(),
            activation_code: Some("AAAAABBBBBCCCCCDDDDDEEEEE".to_string()),
            runtime_generation: 1,
            deactivation_generation: None,
        };

        let persisted = serde_json::to_value(&state).unwrap();
        let public = serde_json::to_value(super::ActivationResult::from(state)).unwrap();
        assert_eq!(persisted["activation_code"], "AAAAABBBBBCCCCCDDDDDEEEEE");
        assert!(public.get("activation_code").is_none());
        assert!(public.get("activation_code_sha256").is_none());
    }

    #[test]
    fn legacy_active_state_has_no_deactivation_in_progress() {
        let state: ActiveState = serde_json::from_value(serde_json::json!({
            "version": STATE_VERSION,
            "operation_id": "operation",
            "status": "active",
            "subname": "tsinghua",
            "device_name": "tsinghua.kundy",
            "domain_name": "tsinghua.kundy.dhttp.net",
            "display_name": "tsinghua.kundy~",
            "activation_code_sha256": "digest",
            "runtime_generation": 7
        }))
        .unwrap();

        assert_eq!(state.deactivation_generation, None);
    }

    #[tokio::test]
    async fn pending_identity_is_reused_after_reload() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = std::env::temp_dir().join(format!("kundy-pending-{}", uuid::Uuid::new_v4()));
        let service = ActivationService::with_home(
            root.join("state"),
            &CertServerConfig::default(),
            dhttp::home::DhttpHome::new(root.join("dhttp")),
        );
        let availability = NameAvailability {
            status: "issued".to_string(),
            subname: "tsinghua".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            display_name: "tsinghua.kundy~".to_string(),
            available: true,
            name_locked: false,
            unavailable_reason: None,
        };

        let first = service
            .load_or_create_pending(
                &availability,
                "AAAAA-BBBBB-CCCCC-DDDDD-EEEEE",
                super::PendingOperation::Claim,
            )
            .await
            .expect("first pending identity should be created");
        let reloaded = service
            .load_or_create_pending(
                &availability,
                "aaaaabbbbbcccccdddddeeeee",
                super::PendingOperation::Claim,
            )
            .await
            .expect("pending identity should reload");

        assert_eq!(
            reloaded.metadata.idempotency_key,
            first.metadata.idempotency_key
        );
        assert_eq!(reloaded.key_pem, first.key_pem);
        assert_eq!(reloaded.csr_pem, first.csr_pem);
        assert_eq!(service.status().await.unwrap().state, "claiming");

        let conflict = match service
            .load_or_create_pending(
                &availability,
                "FFFFF-GGGGG-HHHHH-JJJJJ-KKKKK",
                super::PendingOperation::Claim,
            )
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("a different activation code must not reuse pending key material"),
        };
        assert!(matches!(
            conflict,
            super::ActivationError::PendingCodeConflict
        ));

        std::fs::remove_dir_all(PathBuf::from(root)).expect("test state should be removable");
    }

    #[tokio::test]
    async fn automatic_recovery_reuses_its_key_and_idempotency_key() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let root = std::env::temp_dir().join(format!("kundy-recovery-{}", uuid::Uuid::new_v4()));
        let service = ActivationService::with_home(
            root.join("state"),
            &CertServerConfig::default(),
            dhttp::home::DhttpHome::new(root.join("dhttp")),
        );
        let active = ActiveState {
            version: STATE_VERSION,
            operation_id: "operation".to_string(),
            status: "active".to_string(),
            subname: "tsinghua".to_string(),
            device_name: "tsinghua.kundy".to_string(),
            domain_name: "tsinghua.kundy.dhttp.net".to_string(),
            display_name: "tsinghua.kundy~".to_string(),
            activation_code_sha256: super::activation_code_digest("AAAAA-BBBBB-CCCCC-DDDDD-EEEEE"),
            activation_code: Some("AAAAABBBBBCCCCCDDDDDEEEEE".to_string()),
            runtime_generation: 1,
            deactivation_generation: None,
        };

        let first = service
            .load_or_create_recovery_pending(&active)
            .await
            .expect("first recovery operation should be created");
        let reloaded = service
            .load_or_create_recovery_pending(&active)
            .await
            .expect("recovery operation should reload");

        assert_eq!(
            reloaded.metadata.idempotency_key,
            first.metadata.idempotency_key
        );
        assert_eq!(reloaded.key_pem, first.key_pem);
        assert_eq!(reloaded.csr_pem, first.csr_pem);

        std::fs::remove_dir_all(PathBuf::from(root)).expect("test state should be removable");
    }
}
