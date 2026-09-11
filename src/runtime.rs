//! Kundy 与宿主机特权辅助程序之间的受限文件协议。
//! Kundy 以非特权用户运行。安装程序创建运行目录，两个 root 所有的辅助程序分别消费请求：
//! 一个应用固定的 k3s ConfigMap，另一个重载 Pishoo。此协议让守护进程无需依赖 Kubernetes
//! 客户端库或 kubeconfig，并明确隔离特权边界。
//!
//! The narrow file protocol between Kundy and the privileged host helpers.
//! Kundy runs as an unprivileged user. Two root-owned helpers consume requests from the
//! installer-created runtime directory: one applies the fixed k3s ConfigMap and the other reloads
//! Pishoo. This boundary keeps the daemon independent from Kubernetes clients and kubeconfig.

use std::{
    error::Error,
    fmt,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::{
    fs,
    time::{sleep, timeout},
};
use uuid::Uuid;

pub const RUNTIME_DIR: &str = "/run/kundy";
const PROTOCOL_VERSION: u8 = 1;
const RUNTIME_CONFIG_REQUEST: &str = "runtime-config.request";
const RUNTIME_CONFIG_RESULT: &str = "runtime-config.result";
const PISHOO_RELOAD_REQUEST: &str = "pishoo-reload.request";
const PISHOO_RELOAD_RESULT: &str = "pishoo-reload.result";
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeIdentity {
    pub device_name: String,
    pub generation: u64,
    pub profile_dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeState {
    Ready,
    Pending,
    Error,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOperation {
    Activate,
    Deactivate,
}

impl Default for RuntimeOperation {
    fn default() -> Self {
        Self::Activate
    }
}

impl RuntimeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Pending => "pending",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct RuntimeStatus {
    pub state: &'static str,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_generation: Option<u64>,
}

#[derive(Debug, Serialize)]
struct RuntimeConfigRequest<'a> {
    version: u8,
    generation: u64,
    device_name: &'a str,
    operation: RuntimeOperation,
}

#[derive(Debug, Serialize)]
struct PishooReloadRequest<'a> {
    version: u8,
    generation: u64,
    device_name: &'a str,
    operation: RuntimeOperation,
}

#[derive(Debug, Deserialize)]
struct HelperResult {
    version: u8,
    generation: u64,
    state: String,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    upstream_host: Option<String>,
    #[serde(default)]
    current_generation: Option<u64>,
}

#[derive(Debug)]
pub struct RuntimeBridgeError {
    context: &'static str,
    source: Box<dyn Error + Send + Sync>,
}

impl RuntimeBridgeError {
    fn new(context: &'static str, source: impl Error + Send + Sync + 'static) -> Self {
        Self {
            context,
            source: Box::new(source),
        }
    }
}

impl fmt::Display for RuntimeBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.context)
    }
}

impl Error for RuntimeBridgeError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeBridge {
    dir: PathBuf,
    timeout: Duration,
}

impl RuntimeBridge {
    pub fn new() -> Self {
        Self::with_dir(PathBuf::from(RUNTIME_DIR))
    }

    pub fn with_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(dir: PathBuf, timeout: Duration) -> Self {
        Self { dir, timeout }
    }

    pub async fn activate(
        &self,
        identity: &RuntimeIdentity,
    ) -> Result<RuntimeStatus, RuntimeBridgeError> {
        if !directory_exists(&self.dir).await.map_err(|source| {
            RuntimeBridgeError::new("failed to inspect the host runtime bridge", source)
        })? {
            return Ok(status(RuntimeState::Pending, identity.generation, None));
        }

        let config_result = self
            .ensure_applied(
                RUNTIME_CONFIG_REQUEST,
                RUNTIME_CONFIG_RESULT,
                &RuntimeConfigRequest {
                    version: PROTOCOL_VERSION,
                    generation: identity.generation,
                    device_name: &identity.device_name,
                    operation: RuntimeOperation::Activate,
                },
                true,
            )
            .await?;
        if !helper_succeeded(&config_result) {
            if config_result.state == "pending" {
                return Ok(status(
                    RuntimeState::Pending,
                    identity.generation,
                    result_code(&config_result),
                ));
            }
            return Ok(status_from_helper(
                RuntimeState::Error,
                identity.generation,
                &config_result,
            ));
        }

        let upstream_host = config_result
            .upstream_host
            .as_deref()
            .filter(|host| is_dns_hostname(host))
            .ok_or_else(|| {
                RuntimeBridgeError::new(
                    "the host helper returned an invalid upstream Host",
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid or missing upstream_host",
                    ),
                )
            })?;
        install_server_config(identity, upstream_host)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to install the Pishoo identity config", source)
            })?;

        let pishoo_result = self
            .ensure_applied(
                PISHOO_RELOAD_REQUEST,
                PISHOO_RELOAD_RESULT,
                &PishooReloadRequest {
                    version: PROTOCOL_VERSION,
                    generation: identity.generation,
                    device_name: &identity.device_name,
                    operation: RuntimeOperation::Activate,
                },
                false,
            )
            .await?;
        if !helper_succeeded(&pishoo_result) {
            if pishoo_result.state == "pending" {
                return Ok(status(
                    RuntimeState::Pending,
                    identity.generation,
                    result_code(&pishoo_result),
                ));
            }
            return Ok(status(
                RuntimeState::Error,
                identity.generation,
                result_code(&pishoo_result),
            ));
        }

        Ok(status(RuntimeState::Ready, identity.generation, None))
    }

    pub async fn host_available(&self) -> Result<bool, RuntimeBridgeError> {
        directory_exists(&self.dir).await.map_err(|source| {
            RuntimeBridgeError::new("failed to inspect the host runtime bridge", source)
        })
    }

    pub async fn remove_identity_config(
        &self,
        identity: &RuntimeIdentity,
    ) -> Result<(), RuntimeBridgeError> {
        remove_server_config(identity).await.map_err(|source| {
            RuntimeBridgeError::new("failed to remove the Pishoo identity config", source)
        })
    }

    pub async fn deactivate_config(
        &self,
        identity: &RuntimeIdentity,
    ) -> Result<RuntimeStatus, RuntimeBridgeError> {
        self.deactivate_step(
            RUNTIME_CONFIG_REQUEST,
            RUNTIME_CONFIG_RESULT,
            &RuntimeConfigRequest {
                version: PROTOCOL_VERSION,
                generation: identity.generation,
                device_name: &identity.device_name,
                operation: RuntimeOperation::Deactivate,
            },
        )
        .await
    }

    pub async fn deactivate_pishoo(
        &self,
        identity: &RuntimeIdentity,
    ) -> Result<RuntimeStatus, RuntimeBridgeError> {
        self.deactivate_step(
            PISHOO_RELOAD_REQUEST,
            PISHOO_RELOAD_RESULT,
            &PishooReloadRequest {
                version: PROTOCOL_VERSION,
                generation: identity.generation,
                device_name: &identity.device_name,
                operation: RuntimeOperation::Deactivate,
            },
        )
        .await
    }

    async fn deactivate_step<T: Serialize>(
        &self,
        request_name: &str,
        result_name: &str,
        request: &T,
    ) -> Result<RuntimeStatus, RuntimeBridgeError> {
        if !self.host_available().await? {
            return Ok(status(
                RuntimeState::Pending,
                request_generation(request),
                None,
            ));
        }
        let result = self
            .ensure_applied(request_name, result_name, request, false)
            .await?;
        let state = if helper_succeeded(&result) {
            RuntimeState::Ready
        } else if result.state == "pending" {
            RuntimeState::Pending
        } else {
            RuntimeState::Error
        };
        Ok(status_from_helper(
            state,
            request_generation(request),
            &result,
        ))
    }

    pub async fn status(&self, generation: u64) -> Result<RuntimeStatus, RuntimeBridgeError> {
        if !directory_exists(&self.dir).await.map_err(|source| {
            RuntimeBridgeError::new("failed to inspect the host runtime bridge", source)
        })? {
            return Ok(status(RuntimeState::Pending, generation, None));
        }

        let config = read_matching_result(self.result_path(RUNTIME_CONFIG_RESULT), generation)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to inspect the runtime config result", source)
            })?;
        let Some(config) = config else {
            return Ok(status(RuntimeState::Pending, generation, None));
        };
        if !helper_succeeded(&config) {
            return Ok(status_from_helper(RuntimeState::Error, generation, &config));
        }
        if !config.upstream_host.as_deref().is_some_and(is_dns_hostname) {
            return Ok(status(
                RuntimeState::Error,
                generation,
                Some("upstream_host_invalid".to_string()),
            ));
        }

        let pishoo = read_matching_result(self.result_path(PISHOO_RELOAD_RESULT), generation)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to inspect the Pishoo reload result", source)
            })?;
        let Some(pishoo) = pishoo else {
            return Ok(status(RuntimeState::Pending, generation, None));
        };
        if !helper_succeeded(&pishoo) {
            return Ok(status_from_helper(RuntimeState::Error, generation, &pishoo));
        }
        Ok(status(RuntimeState::Ready, generation, None))
    }

    pub async fn deactivation_status(
        &self,
        generation: u64,
    ) -> Result<RuntimeStatus, RuntimeBridgeError> {
        if !directory_exists(&self.dir).await.map_err(|source| {
            RuntimeBridgeError::new("failed to inspect the host runtime bridge", source)
        })? {
            return Ok(status(RuntimeState::Pending, generation, None));
        }

        let config = read_matching_result(self.result_path(RUNTIME_CONFIG_RESULT), generation)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to inspect the runtime config result", source)
            })?;
        let Some(config) = config else {
            return Ok(status(RuntimeState::Pending, generation, None));
        };
        if !helper_succeeded(&config) {
            return Ok(status_from_helper(RuntimeState::Error, generation, &config));
        }

        let pishoo = read_matching_result(self.result_path(PISHOO_RELOAD_RESULT), generation)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to inspect the Pishoo reload result", source)
            })?;
        let Some(pishoo) = pishoo else {
            return Ok(status(RuntimeState::Pending, generation, None));
        };
        if !helper_succeeded(&pishoo) {
            return Ok(status_from_helper(RuntimeState::Error, generation, &pishoo));
        }
        Ok(status(RuntimeState::Ready, generation, None))
    }

    async fn ensure_applied<T: Serialize>(
        &self,
        request_name: &str,
        result_name: &str,
        request: &T,
        require_upstream_host: bool,
    ) -> Result<HelperResult, RuntimeBridgeError> {
        let result_path = self.result_path(result_name);
        if let Some(result) = read_matching_result(result_path.clone(), request_generation(request))
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to inspect an existing helper result", source)
            })?
            && helper_succeeded(&result)
            && (!require_upstream_host
                || result.upstream_host.as_deref().is_some_and(is_dns_hostname))
        {
            return Ok(result);
        }

        match fs::remove_file(&result_path).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(RuntimeBridgeError::new(
                    "failed to clear a stale host helper result",
                    source,
                ));
            }
        }

        let payload = serde_json::to_vec(request).map_err(|source| {
            RuntimeBridgeError::new("failed to serialize a host runtime request", source)
        })?;
        atomic_write(&self.dir, self.dir.join(request_name), &payload)
            .await
            .map_err(|source| {
                RuntimeBridgeError::new("failed to write a host runtime request", source)
            })?;

        let generation = request_generation(request);
        match timeout(
            self.timeout,
            wait_for_matching_result(result_path, generation),
        )
        .await
        {
            Ok(Ok(Some(result))) => Ok(result),
            Ok(Ok(None)) | Err(_) => Ok(HelperResult {
                version: PROTOCOL_VERSION,
                generation,
                state: "pending".to_string(),
                code: Some("helper_timeout".to_string()),
                upstream_host: None,
                current_generation: None,
            }),
            Ok(Err(source)) => Err(RuntimeBridgeError::new(
                "failed while waiting for a host helper result",
                source,
            )),
        }
    }

    fn result_path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

async fn install_server_config(
    identity: &RuntimeIdentity,
    upstream_host: &str,
) -> Result<(), std::io::Error> {
    let public_host = format!("{}.dhttp.net", identity.device_name);
    if !is_dns_hostname(&public_host) || !is_dns_hostname(upstream_host) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid DHTTP proxy host",
        ));
    }
    let metadata = fs::symlink_metadata(&identity.profile_dir).await?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid DHTTP identity profile directory",
        ));
    }

    let payload = render_server_config(&public_host, upstream_host);
    let target = identity.profile_dir.join("server.conf");
    if fs::read(&target)
        .await
        .is_ok_and(|current| current == payload.as_bytes())
    {
        return Ok(());
    }
    atomic_write(&identity.profile_dir, target, payload.as_bytes()).await
}

async fn remove_server_config(identity: &RuntimeIdentity) -> Result<(), std::io::Error> {
    let profile = match fs::symlink_metadata(&identity.profile_dir).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !profile.is_dir() || profile.file_type().is_symlink() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid DHTTP identity profile directory",
        ));
    }

    let target = identity.profile_dir.join("server.conf");
    match fs::symlink_metadata(&target).await {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            fs::remove_file(target).await
        }
        Ok(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "invalid Pishoo identity config",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn render_server_config(public_host: &str, upstream_host: &str) -> String {
    format!(
        concat!(
            "server {{\n",
            "    listen all 0;\n",
            "\n",
            "    location / {{\n",
            "        proxy_pass http://127.0.0.1:80;\n",
            "        proxy_set_header Host {upstream_host};\n",
            "        proxy_set_header X-Forwarded-Proto https;\n",
            "        proxy_set_header X-Forwarded-Host {public_host};\n",
            "        proxy_set_header X-Forwarded-Port 443;\n",
            "        proxy_set_header X-DHTTP-Origin {public_host};\n",
            "    }}\n",
            "}}\n",
        ),
        upstream_host = upstream_host,
        public_host = public_host
    )
}

pub(crate) fn is_dns_hostname(host: &str) -> bool {
    if host.len() > 253 || host.split('.').count() < 2 {
        return false;
    }
    host.split('.').all(|label| {
        let bytes = label.as_bytes();
        !bytes.is_empty()
            && bytes.len() <= 63
            && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
            && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
            && bytes
                .iter()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
    })
}

fn request_generation<T: Serialize>(request: &T) -> u64 {
    // 两种请求共享 generation 字段；通过序列化读取可保持单一协议实现，无需向守护进程
    // 其他部分暴露线上的请求结构。
    //
    // Both request types share generation. Reading it through serialization preserves one protocol
    // implementation without exposing the wire structs to the rest of the daemon.
    serde_json::to_value(request)
        .ok()
        .and_then(|value| value.get("generation").and_then(serde_json::Value::as_u64))
        .unwrap_or_default()
}

fn helper_succeeded(result: &HelperResult) -> bool {
    result.version == PROTOCOL_VERSION
        && matches!(result.state.as_str(), "applied" | "already_applied")
}

fn result_code(result: &HelperResult) -> Option<String> {
    result.code.clone().or_else(|| Some(result.state.clone()))
}

fn status(state: RuntimeState, generation: u64, code: Option<String>) -> RuntimeStatus {
    RuntimeStatus {
        state: state.as_str(),
        generation,
        code,
        current_generation: None,
    }
}

fn status_from_helper(
    state: RuntimeState,
    generation: u64,
    result: &HelperResult,
) -> RuntimeStatus {
    RuntimeStatus {
        state: state.as_str(),
        generation,
        code: result_code(result),
        current_generation: result.current_generation,
    }
}

async fn directory_exists(path: &Path) -> Result<bool, std::io::Error> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

async fn read_matching_result(
    path: PathBuf,
    generation: u64,
) -> Result<Option<HelperResult>, std::io::Error> {
    let payload = match fs::read(path).await {
        Ok(payload) => payload,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let result = match serde_json::from_slice::<HelperResult>(&payload) {
        Ok(result) => result,
        Err(_) => return Ok(None),
    };
    Ok((result.version == PROTOCOL_VERSION && result.generation == generation).then_some(result))
}

async fn wait_for_matching_result(
    path: PathBuf,
    generation: u64,
) -> Result<Option<HelperResult>, std::io::Error> {
    loop {
        if let Some(result) = read_matching_result(path.clone(), generation).await? {
            return Ok(Some(result));
        }
        sleep(POLL_INTERVAL).await;
    }
}

async fn atomic_write(
    parent: &Path,
    target: PathBuf,
    payload: &[u8],
) -> Result<(), std::io::Error> {
    let stage = parent.join(format!(".kundy-runtime-{}.tmp", Uuid::new_v4()));
    let result = async {
        let mut options = fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut file = options.open(&stage).await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(payload).await?;
        file.sync_all().await?;
        fs::rename(&stage, target).await?;
        Ok::<(), std::io::Error>(())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&stage).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{RuntimeBridge, RuntimeIdentity, RuntimeState, is_dns_hostname};
    use std::{path::PathBuf, time::Duration};
    use tokio::{fs, time::sleep};

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("kundy-runtime-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn missing_host_helper_is_pending_without_creating_privileged_paths() {
        let dir = temp_dir();
        let bridge = RuntimeBridge::with_timeout(dir.clone(), Duration::from_millis(20));
        let result = bridge
            .activate(&RuntimeIdentity {
                device_name: "test.kundy".into(),
                generation: 4,
                profile_dir: dir.join("test.kundy"),
            })
            .await
            .unwrap();
        assert_eq!(result.state, RuntimeState::Pending.as_str());
        assert!(!dir.exists());
    }

    #[tokio::test]
    async fn matching_helper_results_complete_both_steps() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).await.unwrap();
        let bridge = RuntimeBridge::with_timeout(dir.clone(), Duration::from_secs(1));
        let profile_dir = dir.join("test.kundy");
        fs::create_dir_all(&profile_dir).await.unwrap();
        let identity = RuntimeIdentity {
            device_name: "test.kundy".into(),
            generation: 9,
            profile_dir: profile_dir.clone(),
        };
        let helper_dir = dir.clone();
        let helper = tokio::spawn(async move {
            loop {
                if dir.join("runtime-config.request").exists() {
                    fs::write(
                        dir.join("runtime-config.result"),
                        br#"{"version":1,"generation":9,"state":"applied","code":"applied","upstream_host":"192-168-3-113.nip.io"}"#,
                    )
                    .await
                    .unwrap();
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
            loop {
                if helper_dir.join("pishoo-reload.request").exists() {
                    fs::write(
                        helper_dir.join("pishoo-reload.result"),
                        br#"{"version":1,"generation":9,"state":"applied","code":"reloaded"}"#,
                    )
                    .await
                    .unwrap();
                    break;
                }
                sleep(Duration::from_millis(5)).await;
            }
        });
        let result = bridge.activate(&identity).await.unwrap();
        helper.await.unwrap();
        assert_eq!(result.state, RuntimeState::Ready.as_str());
        assert_eq!(result.generation, 9);
        let server_conf = fs::read_to_string(profile_dir.join("server.conf"))
            .await
            .unwrap();
        assert!(server_conf.contains("proxy_set_header Host 192-168-3-113.nip.io;"));
        assert!(server_conf.contains("proxy_set_header X-DHTTP-Origin test.kundy.dhttp.net;"));
    }

    #[tokio::test]
    async fn stale_results_do_not_satisfy_a_new_generation() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            dir.join("runtime-config.result"),
            br#"{"version":1,"generation":1,"state":"applied"}"#,
        )
        .await
        .unwrap();
        let bridge = RuntimeBridge::with_timeout(dir, Duration::from_millis(20));
        let result = bridge
            .status(2)
            .await
            .expect("status should accept stale result files");
        assert_eq!(result.state, RuntimeState::Pending.as_str());
    }

    #[tokio::test]
    async fn deactivation_status_does_not_require_an_upstream_host() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            dir.join("runtime-config.result"),
            br#"{"version":1,"generation":7,"state":"applied","code":"applied"}"#,
        )
        .await
        .unwrap();
        fs::write(
            dir.join("pishoo-reload.result"),
            br#"{"version":1,"generation":7,"state":"applied","code":"applied"}"#,
        )
        .await
        .unwrap();
        let bridge = RuntimeBridge::with_timeout(dir, Duration::from_millis(20));

        let result = bridge.deactivation_status(7).await.unwrap();

        assert_eq!(result.state, RuntimeState::Ready.as_str());
    }

    #[tokio::test]
    async fn identity_config_is_removed_before_pishoo_reload() {
        let dir = temp_dir();
        let profile_dir = dir.join("test.kundy");
        fs::create_dir_all(&profile_dir).await.unwrap();
        fs::write(profile_dir.join("server.conf"), b"server config")
            .await
            .unwrap();
        let bridge = RuntimeBridge::with_timeout(dir, Duration::from_millis(20));
        let identity = RuntimeIdentity {
            device_name: "test.kundy".into(),
            generation: 7,
            profile_dir: profile_dir.clone(),
        };

        bridge.remove_identity_config(&identity).await.unwrap();

        assert!(!profile_dir.join("server.conf").exists());
    }

    #[tokio::test]
    async fn stale_generation_result_reports_the_host_watermark() {
        let dir = temp_dir();
        fs::create_dir_all(&dir).await.unwrap();
        fs::write(
            dir.join("runtime-config.result"),
            br#"{"version":1,"generation":1,"state":"error","code":"stale_generation","current_generation":2}"#,
        )
        .await
        .unwrap();
        let bridge = RuntimeBridge::with_timeout(dir, Duration::from_millis(20));

        let result = bridge.status(1).await.unwrap();

        assert_eq!(result.state, RuntimeState::Error.as_str());
        assert_eq!(result.code.as_deref(), Some("stale_generation"));
        assert_eq!(result.current_generation, Some(2));
    }

    #[test]
    fn validates_hosts_before_rendering_pishoo_config() {
        assert!(is_dns_hostname("192-168-3-113.nip.io"));
        assert!(is_dns_hostname("sciflow161.yuanwuzhi.io"));
        assert!(!is_dns_hostname("https://example.com"));
        assert!(!is_dns_hostname("bad host.example"));
        assert!(!is_dns_hostname("host;reload.example"));
    }
}
