use std::sync::Arc;

use axum::Router;
use dhttp::{
    ddns::resolvers::DnsScheme, dquic::binds::BindPattern, endpoint::Endpoint,
    h3x::hyper::TowerService, name::Name, network::DhttpNetwork,
};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::WebPkiClientVerifier,
};
use snafu::{ResultExt, Snafu};
use tracing::info;

use crate::config::DhttpConfig;

const BOOTSTRAP_CERTIFICATE_PEM: &[u8] = include_bytes!("../tls/kundy.dhttp.net.crt.pem");
const BOOTSTRAP_PRIVATE_KEY_PEM: &[u8] = include_bytes!("../tls/kundy.dhttp.net.key.pem");

#[derive(Debug, Snafu)]
pub enum DhttpServerError {
    #[snafu(display("dhttp.name is invalid"))]
    InvalidName { source: dhttp::name::InvalidName },
    #[snafu(display("dhttp bind pattern is invalid: {bind}"))]
    InvalidBind {
        bind: String,
        source: <BindPattern as std::str::FromStr>::Err,
    },
    #[snafu(display("failed to parse the embedded Kundy certificate"))]
    ParseCertificate {
        source: rustls::pki_types::pem::Error,
    },
    #[snafu(display("the embedded Kundy certificate chain is empty"))]
    EmptyCertificateChain,
    #[snafu(display("failed to parse the embedded Kundy private key"))]
    ParsePrivateKey {
        source: rustls::pki_types::pem::Error,
    },
    #[snafu(display("failed to build mDNS-only dhttp network"))]
    BuildNetwork {
        source: dhttp::ddns::BuildDhttpNetworkWithDnsError,
    },
    #[snafu(display("failed to build mDNS-only dhttp endpoint"))]
    BuildEndpoint {
        source: dhttp::endpoint::BuildEndpointError,
    },
    #[snafu(display("dhttp listener stopped unexpectedly"))]
    Listen {
        source: dhttp::h3x::dquic::AcceptError,
    },
}

pub async fn build(config: &DhttpConfig) -> Result<Arc<Endpoint>, DhttpServerError> {
    info!(
        name = %config.name,
        binds = ?config.bind,
        dns = "mdns-only",
        "kundy local endpoint listening"
    );
    build_endpoint(config).await.map(Arc::new)
}

pub async fn serve(endpoint: Arc<Endpoint>, app: Router) -> Result<(), DhttpServerError> {
    endpoint
        .listen(TowerService(app.into_service()))
        .await
        .context(ListenSnafu)
}

async fn build_endpoint(config: &DhttpConfig) -> Result<Endpoint, DhttpServerError> {
    let name = Name::try_from(config.name.as_str())
        .context(InvalidNameSnafu)?
        .into_owned();
    let binds = config
        .bind
        .iter()
        .map(|bind| {
            bind.parse::<BindPattern>()
                .context(InvalidBindSnafu { bind: bind.clone() })
        })
        .collect::<Result<Vec<_>, _>>()?;

    let cert_chain = load_certificate_chain()?;
    let key = load_private_key()?;
    let identity = Arc::new(dhttp::identity::Identity::new(name, cert_chain, key));
    let mut server = dhttp::trust::default_server_quic_config();
    server.client_cert_verifier = WebPkiClientVerifier::no_client_auth();
    let network = DhttpNetwork::builder()
        .dns(DnsScheme::Mdns)
        .stun_server(None)
        .bind(Arc::new(binds.clone()))
        .build()
        .await
        .context(BuildNetworkSnafu)?;

    Endpoint::builder()
        .identity(identity)
        .dns(DnsScheme::Mdns)
        .network(network)
        .bind(Arc::new(binds))
        .server(server)
        .build()
        .await
        .context(BuildEndpointSnafu)
}

fn load_certificate_chain() -> Result<Vec<CertificateDer<'static>>, DhttpServerError> {
    let certs = CertificateDer::pem_slice_iter(BOOTSTRAP_CERTIFICATE_PEM)
        .collect::<Result<Vec<_>, _>>()
        .context(ParseCertificateSnafu)?;
    snafu::ensure!(!certs.is_empty(), EmptyCertificateChainSnafu);
    Ok(certs)
}

fn load_private_key() -> Result<PrivateKeyDer<'static>, DhttpServerError> {
    PrivateKeyDer::from_pem_slice(BOOTSTRAP_PRIVATE_KEY_PEM).context(ParsePrivateKeySnafu)
}
