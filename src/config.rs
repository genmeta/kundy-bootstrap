use serde::Deserialize;
use snafu::{ResultExt, Snafu};

const EMBEDDED_CONFIG: &str = include_str!("../config/kundy.toml");
const DEFAULT_NAME: &str = "kundy.dhttp.net";
const DEFAULT_BIND: &str = "*:3444";
const DEFAULT_CERTSERVER_HTTP_URL: &str = "http://127.0.0.1:3000";
const DEFAULT_PRODUCT_NAME: &str = "Kundy";
const DEFAULT_ACTIVATION_DOMAIN: &str = "kundy";
const DEFAULT_SERVICE_PATH: &str = "/sciflow-console/";

#[derive(Debug, Snafu)]
pub enum ConfigError {
    #[snafu(display("failed to parse the embedded Kundy configuration"))]
    Parse { source: toml::de::Error },
    #[snafu(display("dhttp.bind must contain at least one bind pattern"))]
    MissingBind,
    #[snafu(display("certserver.base_url must not be empty"))]
    MissingCertServerUrl,
    #[snafu(display("ui.product_name must not be empty"))]
    MissingProductName,
    #[snafu(display("ui.activation_domain must not be empty"))]
    MissingActivationDomain,
    #[snafu(display("ui.service_path must be an absolute path"))]
    InvalidServicePath,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub dhttp: DhttpConfig,
    #[serde(default)]
    pub certserver: CertServerConfig,
    #[serde(default)]
    pub ui: UiConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CertServerConfig {
    #[serde(default = "default_certserver_url")]
    pub base_url: String,
}

impl Default for CertServerConfig {
    fn default() -> Self {
        Self {
            base_url: default_certserver_url(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DhttpConfig {
    #[serde(default = "default_name")]
    pub name: String,
    #[serde(default = "default_bind")]
    pub bind: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiConfig {
    #[serde(default = "default_product_name")]
    pub product_name: String,
    #[serde(default = "default_activation_domain")]
    pub activation_domain: String,
    #[serde(default = "default_service_path")]
    pub service_path: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            product_name: default_product_name(),
            activation_domain: default_activation_domain(),
            service_path: default_service_path(),
        }
    }
}

impl AppConfig {
    pub fn embedded() -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(EMBEDDED_CONFIG).context(ParseSnafu)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.dhttp.bind.is_empty() {
            return MissingBindSnafu.fail();
        }
        if self.certserver.base_url.trim().is_empty() {
            return MissingCertServerUrlSnafu.fail();
        }
        if self.ui.product_name.trim().is_empty() {
            return MissingProductNameSnafu.fail();
        }
        if self.ui.activation_domain.trim().is_empty() {
            return MissingActivationDomainSnafu.fail();
        }
        let service_path = self.ui.service_path.trim();
        if !service_path.starts_with('/') || service_path.starts_with("//") {
            return InvalidServicePathSnafu.fail();
        }
        Ok(())
    }
}

fn default_name() -> String {
    DEFAULT_NAME.to_string()
}

fn default_bind() -> Vec<String> {
    vec![DEFAULT_BIND.to_string()]
}

fn default_certserver_url() -> String {
    DEFAULT_CERTSERVER_HTTP_URL.to_string()
}

fn default_product_name() -> String {
    DEFAULT_PRODUCT_NAME.to_string()
}

fn default_activation_domain() -> String {
    DEFAULT_ACTIVATION_DOMAIN.to_string()
}

fn default_service_path() -> String {
    DEFAULT_SERVICE_PATH.to_string()
}

#[cfg(test)]
mod tests {
    use super::AppConfig;

    #[test]
    fn parses_minimal_dhttp_config() {
        let config: AppConfig = toml::from_str(
            r#"
                [dhttp]
            "#,
        )
        .expect("minimal configuration should parse");

        assert_eq!(config.dhttp.name, "kundy.dhttp.net");
        assert_eq!(config.dhttp.bind, ["*:3444"]);
        assert_eq!(config.certserver.base_url, "http://127.0.0.1:3000");
        assert_eq!(config.ui.product_name, "Kundy");
        assert_eq!(config.ui.activation_domain, "kundy");
        assert_eq!(config.ui.service_path, "/sciflow-console/");
    }

    #[test]
    fn embedded_build_config_is_valid() {
        AppConfig::embedded().expect("embedded build configuration should be valid");
    }

    #[test]
    fn rejects_empty_bind_list() {
        let config: AppConfig = toml::from_str(
            r#"
                [dhttp]
                bind = []
            "#,
        )
        .expect("configuration should parse before validation");

        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_empty_certserver_url() {
        let config: AppConfig = toml::from_str(
            r#"
                [dhttp]

                [certserver]
                base_url = "  "
            "#,
        )
        .expect("configuration should parse before validation");

        assert!(config.validate().is_err());
    }
}
