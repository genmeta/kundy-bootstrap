//! 供宿主机 systemd 单元调用的固定用途特权操作。
//! Kundy 公共服务不直接调用 Kubernetes 或 systemd，而是以设备用户身份写入受限请求；
//! root 所有的 oneshot 单元调用本模块，且不接受用户可控的命令行参数。
//!
//! Fixed-purpose privileged operations used by the host systemd units.
//! The public service writes constrained requests as the appliance user instead of calling
//! Kubernetes or systemd directly. Root-owned oneshot units invoke this module without
//! user-controlled command-line arguments.

use std::{
    error::Error,
    fmt, fs,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::runtime::{RUNTIME_DIR, RuntimeOperation, is_dns_hostname};

const PROTOCOL_VERSION: u8 = 1;
const MAX_REQUEST_BYTES: u64 = 4 * 1024;
const K3S: &str = "/usr/local/bin/k3s";
const K3S_KUBECONFIG: &str = "/etc/rancher/k3s/k3s.yaml";
const SYSTEMCTL: &str = "/usr/bin/systemctl";
const GETENT: &str = "/usr/bin/getent";
const PISHOO: &str = "/usr/bin/pishoo";

const RUNTIME_CONFIG_REQUEST: &str = "runtime-config.request";
const RUNTIME_CONFIG_RESULT: &str = "runtime-config.result";
const PISHOO_RELOAD_REQUEST: &str = "pishoo-reload.request";
const PISHOO_RELOAD_RESULT: &str = "pishoo-reload.result";

const IDENTITY_NAMESPACE: &str = "flux-system";
const IDENTITY_CONFIG_MAP: &str = "kundy-device-identity";
const COOKIE_NAMESPACE: &str = "authentik";
const COOKIE_SECRET: &str = "oauth2-proxy-browser-dhttp";
const COOKIE_SECRET_KEY: &str = "OAUTH2_PROXY_COOKIE_SECRET";
const CLUSTER_SETTINGS: &str = "cluster-settings";
const FLUX_RECONCILE_ANNOTATION: &str = "reconcile.fluxcd.io/requestedAt";
const FLUX_KUSTOMIZATIONS: [&str; 2] = ["authentik", "kundy-dhttp"];
const FLUX_RECONCILE_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const FLUX_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub struct HostError {
    code: &'static str,
    context: &'static str,
    source: Option<Box<dyn Error + Send + Sync>>,
    current_generation: Option<u64>,
}

impl HostError {
    fn message(code: &'static str, context: &'static str) -> Self {
        Self {
            code,
            context,
            source: None,
            current_generation: None,
        }
    }

    fn with_source(
        code: &'static str,
        context: &'static str,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            code,
            context,
            source: Some(Box::new(source)),
            current_generation: None,
        }
    }

    fn stale_generation(current_generation: u64) -> Self {
        Self {
            code: "stale_generation",
            context: "the runtime request would roll back the applied generation",
            source: None,
            current_generation: Some(current_generation),
        }
    }
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} ({})", self.context, self.code)
    }
}

impl Error for HostError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_deref()
            .map(|source| source as &(dyn Error + 'static))
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HostRequest {
    version: u8,
    generation: u64,
    device_name: String,
    #[serde(default)]
    operation: RuntimeOperation,
}

#[derive(Debug)]
struct RequestEnvelope {
    request: HostRequest,
    owner_uid: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct HelperResult {
    version: u8,
    generation: u64,
    state: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    upstream_host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_generation: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
enum ApplyState {
    Applied,
    AlreadyApplied,
}

#[derive(Debug)]
struct HostOutcome {
    state: ApplyState,
    upstream_host: Option<String>,
}

impl HostOutcome {
    const fn new(state: ApplyState) -> Self {
        Self {
            state,
            upstream_host: None,
        }
    }

    fn with_upstream_host(mut self, upstream_host: String) -> Self {
        self.upstream_host = Some(upstream_host);
        self
    }
}

impl ApplyState {
    const fn state(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::AlreadyApplied => "already_applied",
        }
    }
}

pub fn apply_runtime_config() -> Result<(), HostError> {
    ensure_root()?;
    process_request(
        RUNTIME_CONFIG_REQUEST,
        RUNTIME_CONFIG_RESULT,
        |envelope| match envelope.request.operation {
            RuntimeOperation::Activate => apply_runtime_config_request(envelope),
            RuntimeOperation::Deactivate => deactivate_runtime_config_request(envelope),
        },
    )
}

pub fn reload_pishoo() -> Result<(), HostError> {
    ensure_root()?;
    process_request(
        PISHOO_RELOAD_REQUEST,
        PISHOO_RELOAD_RESULT,
        |envelope| match envelope.request.operation {
            RuntimeOperation::Activate => reload_pishoo_request(envelope),
            RuntimeOperation::Deactivate => deactivate_pishoo_request(envelope),
        },
    )
}

fn process_request(
    request_name: &str,
    result_name: &str,
    operation: impl FnOnce(&RequestEnvelope) -> Result<HostOutcome, HostError>,
) -> Result<(), HostError> {
    let envelope = read_request(request_name)?;
    let outcome = operation(&envelope);
    let result = match &outcome {
        Ok(outcome) => HelperResult {
            version: PROTOCOL_VERSION,
            generation: envelope.request.generation,
            state: outcome.state.state().to_string(),
            code: outcome.state.state().to_string(),
            upstream_host: outcome.upstream_host.clone(),
            current_generation: None,
        },
        Err(error) => HelperResult {
            version: PROTOCOL_VERSION,
            generation: envelope.request.generation,
            state: "error".to_string(),
            code: error.code.to_string(),
            upstream_host: None,
            current_generation: error.current_generation,
        },
    };

    // 先持久化结果再消费请求，确保调用方不会看到“请求已消失但结果尚不存在”的状态。
    //
    // Persist the result before consuming the request so callers never observe a missing request
    // without its corresponding result.
    write_result(result_name, &result)?;
    fs::remove_file(runtime_path(request_name)).map_err(|source| {
        HostError::with_source(
            "request_cleanup_failed",
            "failed to consume the Kundy host request",
            source,
        )
    })?;
    outcome.map(|_| ())
}

fn apply_runtime_config_request(envelope: &RequestEnvelope) -> Result<HostOutcome, HostError> {
    let request = &envelope.request;
    let upstream_host = load_upstream_host()?;
    let expected = identity_config_map(request);
    let mut state = ApplyState::Applied;
    if let Some(current) = kubectl_get_json(
        IDENTITY_NAMESPACE,
        "configmap",
        IDENTITY_CONFIG_MAP,
        "configmap_read_failed",
    )? {
        let current_name = current
            .pointer("/data/DHTTP_PARTIAL_NAME")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let current_generation = parse_generation(&current)?;
        let current_enabled = current
            .pointer("/data/DHTTP_ENABLED")
            .and_then(Value::as_str)
            == Some("true");
        if (current_enabled || current_generation > 0)
            && !current_name.is_empty()
            && current_name != request.device_name
        {
            return Err(HostError::message(
                "device_name_conflict",
                "the runtime ConfigMap is already bound to another permanent name",
            ));
        }

        if current_generation > request.generation {
            return Err(HostError::stale_generation(current_generation));
        }
        if current_generation == request.generation && manifest_data_matches(&current, &expected) {
            state = ApplyState::AlreadyApplied;
        }
    }

    ensure_cookie_secret()?;
    if matches!(state, ApplyState::Applied) {
        kubectl_apply(&expected, "configmap_apply_failed")?;
    }
    request_flux_reconciliation(request.generation)?;
    Ok(HostOutcome::new(state).with_upstream_host(upstream_host))
}

fn deactivate_runtime_config_request(envelope: &RequestEnvelope) -> Result<HostOutcome, HostError> {
    let request = &envelope.request;
    let expected = inactive_identity_config_map(request);
    let state = match kubectl_get_json(
        IDENTITY_NAMESPACE,
        "configmap",
        IDENTITY_CONFIG_MAP,
        "configmap_read_failed",
    )? {
        Some(current) => {
            let current_generation = parse_generation(&current)?;
            if current_generation > request.generation {
                return Err(HostError::stale_generation(current_generation));
            }
            if current_generation == request.generation
                && manifest_data_matches(&current, &expected)
            {
                ApplyState::AlreadyApplied
            } else {
                kubectl_apply(&expected, "configmap_apply_failed")?;
                ApplyState::Applied
            }
        }
        _ => {
            kubectl_apply(&expected, "configmap_apply_failed")?;
            ApplyState::Applied
        }
    };
    request_flux_reconciliation(request.generation)?;
    Ok(HostOutcome::new(state))
}

fn reload_pishoo_request(envelope: &RequestEnvelope) -> Result<HostOutcome, HostError> {
    let current = kubectl_get_json(
        IDENTITY_NAMESPACE,
        "configmap",
        IDENTITY_CONFIG_MAP,
        "configmap_read_failed",
    )?
    .ok_or_else(|| {
        HostError::message(
            "runtime_config_not_applied",
            "the runtime ConfigMap is missing",
        )
    })?;
    if parse_generation(&current)? != envelope.request.generation
        || !manifest_data_matches(&current, &identity_config_map(&envelope.request))
    {
        return Err(HostError::message(
            "runtime_config_not_applied",
            "the runtime ConfigMap generation has not been applied",
        ));
    }

    let home = user_home(envelope.owner_uid)?;
    let identity = home.join(".dhttp").join(&envelope.request.device_name);
    let ssl = identity.join("ssl");
    let server_conf = identity.join("server.conf");
    if !identity.is_dir() || !ssl.is_dir() || !server_conf.is_file() {
        return Err(HostError::message(
            "identity_not_installed",
            "the activated DHTTP identity is not installed for the Kundy user",
        ));
    }

    wait_for_flux_reconciliation(envelope.request.generation)?;

    run_status_command(
        PISHOO,
        &["-t"],
        "pishoo_config_invalid",
        "Pishoo rejected its global configuration",
    )?;
    run_status_command(
        SYSTEMCTL,
        &["reload", "pishoo.service"],
        "pishoo_reload_failed",
        "failed to reload Pishoo",
    )?;
    run_status_command(
        SYSTEMCTL,
        &["is-active", "--quiet", "pishoo.service"],
        "pishoo_inactive",
        "Pishoo is not active after reload",
    )?;
    Ok(HostOutcome::new(ApplyState::Applied))
}

fn deactivate_pishoo_request(envelope: &RequestEnvelope) -> Result<HostOutcome, HostError> {
    let current = kubectl_get_json(
        IDENTITY_NAMESPACE,
        "configmap",
        IDENTITY_CONFIG_MAP,
        "configmap_read_failed",
    )?
    .ok_or_else(|| {
        HostError::message(
            "runtime_config_not_applied",
            "the inactive runtime ConfigMap is missing",
        )
    })?;
    if parse_generation(&current)? != envelope.request.generation
        || !manifest_data_matches(&current, &inactive_identity_config_map(&envelope.request))
    {
        return Err(HostError::message(
            "runtime_config_not_applied",
            "the inactive runtime ConfigMap generation has not been applied",
        ));
    }

    wait_for_flux_reconciliation(envelope.request.generation)?;
    run_status_command(
        PISHOO,
        &["-t"],
        "pishoo_config_invalid",
        "Pishoo rejected its global configuration",
    )?;
    run_status_command(
        SYSTEMCTL,
        &["reload", "pishoo.service"],
        "pishoo_reload_failed",
        "failed to reload Pishoo",
    )?;
    run_status_command(
        SYSTEMCTL,
        &["is-active", "--quiet", "pishoo.service"],
        "pishoo_inactive",
        "Pishoo is not active after reload",
    )?;
    Ok(HostOutcome::new(ApplyState::Applied))
}

fn ensure_cookie_secret() -> Result<(), HostError> {
    if let Some(secret) = kubectl_get_json(
        COOKIE_NAMESPACE,
        "secret",
        COOKIE_SECRET,
        "cookie_secret_read_failed",
    )? {
        let encoded = secret
            .pointer(&format!("/data/{COOKIE_SECRET_KEY}"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if is_supported_cookie_secret(encoded) {
            return Ok(());
        }
        return Err(HostError::message(
            "cookie_secret_invalid",
            "the existing DHTTP OAuth cookie Secret is invalid",
        ));
    }

    let secret = json!({
        "apiVersion": "v1",
        "kind": "Secret",
        "metadata": {
            "name": COOKIE_SECRET,
            "namespace": COOKIE_NAMESPACE,
        },
        "type": "Opaque",
        "stringData": {
            "OAUTH2_PROXY_COOKIE_SECRET": Uuid::new_v4().simple().to_string(),
        },
    });
    kubectl_apply(&secret, "cookie_secret_apply_failed")
}

fn identity_config_map(request: &HostRequest) -> Value {
    let host = format!("{}.dhttp.net", request.device_name);
    let origin = format!("https://{host}");
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": IDENTITY_CONFIG_MAP,
            "namespace": IDENTITY_NAMESPACE,
            "labels": {
                "reconcile.fluxcd.io/watch": "Enabled",
            },
        },
        "data": {
            "DHTTP_ENABLED": "true",
            "DHTTP_CONFIG_GENERATION": request.generation.to_string(),
            "DHTTP_PARTIAL_NAME": request.device_name,
            "DHTTP_HOST": host,
            "DHTTP_DISPLAY_HOST": format!("{}~", request.device_name),
            "DHTTP_ORIGIN": origin,
            "DHTTP_REDIRECT_URL": format!("{origin}/oauth2/callback"),
            "DHTTP_REPLICAS": "1",
        },
    })
}

fn inactive_identity_config_map(request: &HostRequest) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": IDENTITY_CONFIG_MAP,
            "namespace": IDENTITY_NAMESPACE,
            "labels": {
                "reconcile.fluxcd.io/watch": "Enabled",
            },
        },
        "data": {
            "DHTTP_ENABLED": "false",
            "DHTTP_CONFIG_GENERATION": request.generation.to_string(),
            "DHTTP_PARTIAL_NAME": "",
            "DHTTP_HOST": "",
            "DHTTP_DISPLAY_HOST": "",
            "DHTTP_ORIGIN": "",
            "DHTTP_REDIRECT_URL": "",
            "DHTTP_REPLICAS": "0",
        },
    })
}

fn manifest_data_matches(current: &Value, expected: &Value) -> bool {
    let Some(expected_data) = expected.get("data").and_then(Value::as_object) else {
        return false;
    };
    let data_matches = expected_data.iter().all(|(key, value)| {
        current
            .get("data")
            .and_then(|data| data.get(key.as_str()))
            .is_some_and(|current_value| current_value == value)
    });
    let watch_label_matches = current
        .pointer("/metadata/labels/reconcile.fluxcd.io~1watch")
        .and_then(Value::as_str)
        == Some("Enabled");
    data_matches && watch_label_matches
}

fn load_upstream_host() -> Result<String, HostError> {
    let settings = kubectl_get_json(
        IDENTITY_NAMESPACE,
        "configmap",
        CLUSTER_SETTINGS,
        "cluster_settings_read_failed",
    )?
    .ok_or_else(|| {
        HostError::message(
            "cluster_settings_missing",
            "the Flux cluster settings ConfigMap is missing",
        )
    })?;
    let host = settings
        .pointer("/data/HOST")
        .and_then(Value::as_str)
        .filter(|host| is_dns_hostname(host))
        .ok_or_else(|| {
            HostError::message(
                "cluster_host_invalid",
                "the Flux cluster settings HOST is invalid",
            )
        })?;
    Ok(host.to_string())
}

fn request_flux_reconciliation(generation: u64) -> Result<(), HostError> {
    let token = format!("kundy-{generation}-{}", Uuid::new_v4().simple());
    let annotation = format!("{FLUX_RECONCILE_ANNOTATION}={token}");
    for name in FLUX_KUSTOMIZATIONS {
        let output = kubectl_output(&[
            "annotate",
            "kustomization",
            name,
            "--namespace",
            IDENTITY_NAMESPACE,
            &annotation,
            "--overwrite=true",
        ])
        .map_err(|source| {
            HostError::with_source(
                "flux_reconcile_request_failed",
                "failed to request Flux reconciliation",
                source,
            )
        })?;
        if !output.status.success() {
            return Err(HostError::message(
                "flux_reconcile_request_failed",
                "Flux rejected a fixed reconciliation request",
            ));
        }
    }
    Ok(())
}

fn wait_for_flux_reconciliation(generation: u64) -> Result<(), HostError> {
    let deadline = Instant::now() + FLUX_RECONCILE_TIMEOUT;
    loop {
        let mut ready = true;
        for name in FLUX_KUSTOMIZATIONS {
            let resource = kubectl_get_json(
                IDENTITY_NAMESPACE,
                "kustomization",
                name,
                "flux_status_read_failed",
            )?
            .ok_or_else(|| {
                HostError::message(
                    "flux_kustomization_missing",
                    "a required Flux Kustomization is missing",
                )
            })?;
            if resource.pointer("/spec/suspend").and_then(Value::as_bool) == Some(true) {
                return Err(HostError::message(
                    "flux_kustomization_suspended",
                    "a required Flux Kustomization is suspended",
                ));
            }
            ready &= flux_reconciliation_ready(&resource, generation);
        }
        if ready {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(HostError::message(
                "flux_reconcile_timeout",
                "Flux did not finish the Kundy runtime reconciliation in time",
            ));
        }
        thread::sleep(FLUX_POLL_INTERVAL);
    }
}

fn flux_reconciliation_ready(resource: &Value, expected_generation: u64) -> bool {
    let handled = resource
        .pointer("/status/lastHandledReconcileAt")
        .and_then(Value::as_str);
    let resource_generation = resource
        .pointer("/metadata/generation")
        .and_then(Value::as_u64);
    let observed = resource
        .pointer("/status/observedGeneration")
        .and_then(Value::as_u64);
    let ready = resource
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .is_some_and(|conditions| {
            conditions.iter().any(|condition| {
                condition.get("type").and_then(Value::as_str) == Some("Ready")
                    && condition.get("status").and_then(Value::as_str) == Some("True")
            })
        });
    let handled_prefix = format!("kundy-{expected_generation}-");
    handled.is_some_and(|handled| handled.starts_with(&handled_prefix))
        && resource_generation == observed
        && ready
}

fn parse_generation(config_map: &Value) -> Result<u64, HostError> {
    let Some(raw) = config_map
        .pointer("/data/DHTTP_CONFIG_GENERATION")
        .and_then(Value::as_str)
    else {
        return Ok(0);
    };
    raw.parse().map_err(|source| {
        HostError::with_source(
            "configmap_invalid",
            "the runtime ConfigMap generation is invalid",
            source,
        )
    })
}

fn is_supported_cookie_secret(encoded: &str) -> bool {
    matches!(encoded.len(), 24 | 32 | 44)
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
}

fn kubectl_get_json(
    namespace: &str,
    kind: &str,
    name: &str,
    code: &'static str,
) -> Result<Option<Value>, HostError> {
    let output = kubectl_output(&[
        "get",
        kind,
        name,
        "--namespace",
        namespace,
        "--ignore-not-found=true",
        "--output=json",
    ])
    .map_err(|source| HostError::with_source(code, "failed to query k3s", source))?;
    if !output.status.success() {
        return Err(HostError::message(
            code,
            "k3s rejected a fixed object query",
        ));
    }
    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&output.stdout)
        .map(Some)
        .map_err(|source| HostError::with_source(code, "k3s returned invalid JSON", source))
}

fn kubectl_apply(manifest: &Value, code: &'static str) -> Result<(), HostError> {
    let payload = serde_json::to_vec(manifest).map_err(|source| {
        HostError::with_source(code, "failed to serialize a fixed k3s object", source)
    })?;
    let mut child = ProcessCommand::new(K3S)
        .arg("kubectl")
        .args([
            "apply",
            "--server-side=true",
            "--field-manager=kundy-host",
            "--force-conflicts=true",
            "--filename=-",
        ])
        .env("KUBECONFIG", K3S_KUBECONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| HostError::with_source(code, "failed to start k3s kubectl", source))?;
    child
        .stdin
        .take()
        .ok_or_else(|| HostError::message(code, "failed to open k3s kubectl input"))?
        .write_all(&payload)
        .map_err(|source| {
            HostError::with_source(code, "failed to send a fixed k3s object", source)
        })?;
    let status = child
        .wait()
        .map_err(|source| HostError::with_source(code, "failed to wait for k3s kubectl", source))?;
    if !status.success() {
        return Err(HostError::message(
            code,
            "k3s rejected a fixed object update",
        ));
    }
    Ok(())
}

fn kubectl_output(args: &[&str]) -> Result<Output, std::io::Error> {
    ProcessCommand::new(K3S)
        .arg("kubectl")
        .args(args)
        .env("KUBECONFIG", K3S_KUBECONFIG)
        .output()
}

fn read_request(name: &str) -> Result<RequestEnvelope, HostError> {
    let dir = fs::symlink_metadata(RUNTIME_DIR).map_err(|source| {
        HostError::with_source(
            "runtime_dir_invalid",
            "failed to inspect the Kundy runtime directory",
            source,
        )
    })?;
    if !dir.is_dir() || dir.file_type().is_symlink() || dir.mode() & 0o077 != 0 || dir.uid() == 0 {
        return Err(HostError::message(
            "runtime_dir_invalid",
            "the Kundy runtime directory ownership or mode is invalid",
        ));
    }

    let path = runtime_path(name);
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)
        .map_err(|source| {
            HostError::with_source(
                "request_invalid",
                "failed to open the Kundy host request",
                source,
            )
        })?;
    let metadata = file.metadata().map_err(|source| {
        HostError::with_source(
            "request_invalid",
            "failed to inspect the Kundy host request",
            source,
        )
    })?;
    if !metadata.is_file()
        || metadata.uid() != dir.uid()
        || metadata.mode() & 0o077 != 0
        || metadata.len() > MAX_REQUEST_BYTES
    {
        return Err(HostError::message(
            "request_invalid",
            "the Kundy host request ownership, mode, or size is invalid",
        ));
    }

    let mut payload = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut payload).map_err(|source| {
        HostError::with_source(
            "request_invalid",
            "failed to read the Kundy host request",
            source,
        )
    })?;
    let request: HostRequest = serde_json::from_slice(&payload).map_err(|source| {
        HostError::with_source(
            "request_invalid",
            "the Kundy host request is invalid JSON",
            source,
        )
    })?;
    validate_request(&request)?;
    Ok(RequestEnvelope {
        request,
        owner_uid: dir.uid(),
    })
}

fn validate_request(request: &HostRequest) -> Result<(), HostError> {
    if request.version != PROTOCOL_VERSION || request.generation == 0 {
        return Err(HostError::message(
            "request_invalid",
            "the Kundy host request protocol or generation is invalid",
        ));
    }
    validate_device_name(&request.device_name)
}

fn validate_device_name(name: &str) -> Result<(), HostError> {
    if !is_dns_hostname(name)
        || name.len() + ".dhttp.net".len() > 253
        || name.ends_with(".dhttp.net")
    {
        return Err(HostError::message(
            "device_name_invalid",
            "the permanent device name is invalid",
        ));
    }
    Ok(())
}

fn write_result(name: &str, result: &HelperResult) -> Result<(), HostError> {
    let payload = serde_json::to_vec(result).map_err(|source| {
        HostError::with_source(
            "result_write_failed",
            "failed to serialize a Kundy helper result",
            source,
        )
    })?;
    let parent = Path::new(RUNTIME_DIR);
    let stage = parent.join(format!(".kundy-helper-{}.tmp", Uuid::new_v4()));
    let target = runtime_path(name);
    let write = || -> Result<(), std::io::Error> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o644)
            .open(&stage)?;
        file.write_all(&payload)?;
        file.sync_all()?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o644))?;
        fs::rename(&stage, target)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    let outcome = write();
    if outcome.is_err() {
        let _ = fs::remove_file(&stage);
    }
    outcome.map_err(|source| {
        HostError::with_source(
            "result_write_failed",
            "failed to persist a Kundy helper result",
            source,
        )
    })
}

fn user_home(uid: u32) -> Result<PathBuf, HostError> {
    let output = ProcessCommand::new(GETENT)
        .args(["passwd", &uid.to_string()])
        .output()
        .map_err(|source| {
            HostError::with_source(
                "runtime_user_invalid",
                "failed to resolve the Kundy user",
                source,
            )
        })?;
    if !output.status.success() {
        return Err(HostError::message(
            "runtime_user_invalid",
            "the Kundy runtime user does not exist",
        ));
    }
    let record = std::str::from_utf8(&output.stdout).map_err(|source| {
        HostError::with_source(
            "runtime_user_invalid",
            "the Kundy user record is invalid",
            source,
        )
    })?;
    let home = record
        .trim_end()
        .split(':')
        .nth(5)
        .filter(|value| value.starts_with('/') && !value.contains('\n'))
        .ok_or_else(|| {
            HostError::message(
                "runtime_user_invalid",
                "the Kundy runtime user has no valid home directory",
            )
        })?;
    Ok(PathBuf::from(home))
}

fn run_status_command(
    program: &str,
    args: &[&str],
    code: &'static str,
    context: &'static str,
) -> Result<(), HostError> {
    let status = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| HostError::with_source(code, context, source))?;
    if status.success() {
        Ok(())
    } else {
        Err(HostError::message(code, context))
    }
}

fn ensure_root() -> Result<(), HostError> {
    // 安全性：geteuid 没有前置条件，也不会解引用指针。
    //
    // SAFETY: geteuid has no preconditions and does not dereference pointers.
    if unsafe { libc::geteuid() } != 0 {
        return Err(HostError::message(
            "root_required",
            "this fixed Kundy host helper must run as root",
        ));
    }
    Ok(())
}

fn runtime_path(name: &str) -> PathBuf {
    Path::new(RUNTIME_DIR).join(name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        HostRequest, flux_reconciliation_ready, identity_config_map, is_supported_cookie_secret,
        manifest_data_matches, validate_device_name,
    };
    use crate::runtime::RuntimeOperation;

    #[test]
    fn accepts_partial_permanent_names_only() {
        assert!(validate_device_name("tsinghua.kundy").is_ok());
        assert!(validate_device_name("lab-1.future-domain").is_ok());
        assert!(validate_device_name("kundy").is_err());
        assert!(validate_device_name("Tsinghua.kundy").is_err());
        assert!(validate_device_name("test.kundy.dhttp.net").is_err());
        assert!(validate_device_name("-test.kundy").is_err());
    }

    #[test]
    fn renders_the_flux_runtime_contract() {
        let request = HostRequest {
            version: 1,
            generation: 7,
            device_name: "tsinghua.kundy".to_string(),
            operation: RuntimeOperation::Activate,
        };
        let manifest = identity_config_map(&request);
        assert_eq!(manifest["metadata"]["namespace"], "flux-system");
        assert_eq!(manifest["data"]["DHTTP_CONFIG_GENERATION"], "7");
        assert_eq!(manifest["data"]["DHTTP_HOST"], "tsinghua.kundy.dhttp.net");
        assert_eq!(
            manifest["data"]["DHTTP_REDIRECT_URL"],
            "https://tsinghua.kundy.dhttp.net/oauth2/callback"
        );
        assert_eq!(manifest["data"]["DHTTP_REPLICAS"], "1");
    }

    #[test]
    fn compares_only_the_owned_configmap_fields() {
        let request = HostRequest {
            version: 1,
            generation: 1,
            device_name: "test.kundy".to_string(),
            operation: RuntimeOperation::Activate,
        };
        let expected = identity_config_map(&request);
        let mut current = expected.clone();
        current["data"]["UNRELATED"] = json!("preserved");
        assert!(manifest_data_matches(&current, &expected));
        current["data"]["DHTTP_REPLICAS"] = json!("0");
        assert!(!manifest_data_matches(&current, &expected));
        current = expected.clone();
        current["metadata"]["labels"]["reconcile.fluxcd.io/watch"] = json!("Disabled");
        assert!(!manifest_data_matches(&current, &expected));
    }

    #[test]
    fn recognizes_supported_cookie_secret_lengths() {
        assert!(is_supported_cookie_secret("YWJjZGVmZ2hpamtsbW5vcA=="));
        assert!(!is_supported_cookie_secret("short"));
    }

    #[test]
    fn recognizes_a_handled_reconciliation_after_flux_removes_the_request_annotation() {
        let ready = json!({
            "metadata": {
                "generation": 3,
            },
            "status": {
                "observedGeneration": 3,
                "lastHandledReconcileAt": "kundy-1-request",
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        });
        assert!(flux_reconciliation_ready(&ready, 1));

        let mut stale = ready;
        stale["status"]["lastHandledReconcileAt"] = json!("older-request");
        assert!(!flux_reconciliation_ready(&stale, 1));
    }

    #[test]
    fn requires_flux_to_handle_the_current_kundy_generation() {
        let ready = json!({
            "metadata": { "generation": 3 },
            "status": {
                "observedGeneration": 3,
                "lastHandledReconcileAt": "kundy-10-request",
                "conditions": [{"type": "Ready", "status": "True"}],
            },
        });
        assert!(!flux_reconciliation_ready(&ready, 1));
        assert!(flux_reconciliation_ready(&ready, 10));
    }
}
