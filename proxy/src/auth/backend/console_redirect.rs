use crate::{
    auth,
    cache::Cached,
    compute,
    config::AuthenticationConfig,
    context::RequestMonitoring,
    control_plane::{self, provider::NodeInfo, CachedNodeInfo},
    error::{ReportableError, UserFacingError},
    proxy::connect_compute::ComputeConnectBackend,
    stream::PqStream,
    waiters,
};
use async_trait::async_trait;
use pq_proto::BeMessage as Be;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_postgres::config::SslMode;
use tracing::{info, info_span};

use super::ComputeCredentialKeys;

#[derive(Debug, Error)]
pub(crate) enum WebAuthError {
    #[error(transparent)]
    WaiterRegister(#[from] waiters::RegisterError),

    #[error(transparent)]
    WaiterWait(#[from] waiters::WaitError),

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug)]
pub struct ConsoleRedirectBackend {
    console_uri: reqwest::Url,
}

impl UserFacingError for WebAuthError {
    fn to_string_client(&self) -> String {
        "Internal error".to_string()
    }
}

impl ReportableError for WebAuthError {
    fn get_error_kind(&self) -> crate::error::ErrorKind {
        match self {
            Self::WaiterRegister(_) => crate::error::ErrorKind::Service,
            Self::WaiterWait(_) => crate::error::ErrorKind::Service,
            Self::Io(_) => crate::error::ErrorKind::ClientDisconnect,
        }
    }
}

fn hello_message(redirect_uri: &reqwest::Url, session_id: &str) -> String {
    format!(
        concat![
            "Welcome to Neon!\n",
            "Authenticate by visiting:\n",
            "    {redirect_uri}{session_id}\n\n",
        ],
        redirect_uri = redirect_uri,
        session_id = session_id,
    )
}

pub(crate) fn new_psql_session_id() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

impl ConsoleRedirectBackend {
    pub fn new(console_uri: reqwest::Url) -> Self {
        Self { console_uri }
    }

    pub(crate) async fn authenticate(
        &self,
        ctx: &RequestMonitoring,
        auth_config: &'static AuthenticationConfig,
        client: &mut PqStream<impl AsyncRead + AsyncWrite + Unpin>,
    ) -> auth::Result<ConsoleRedirectNodeInfo> {
        authenticate(ctx, auth_config, &self.console_uri, client)
            .await
            .map(ConsoleRedirectNodeInfo)
    }
}

pub struct ConsoleRedirectNodeInfo(pub(super) NodeInfo);

#[async_trait]
impl ComputeConnectBackend for ConsoleRedirectNodeInfo {
    async fn wake_compute(
        &self,
        _ctx: &RequestMonitoring,
    ) -> Result<CachedNodeInfo, control_plane::errors::WakeComputeError> {
        Ok(Cached::new_uncached(self.0.clone()))
    }

    fn get_keys(&self) -> &ComputeCredentialKeys {
        &ComputeCredentialKeys::None
    }
}

async fn authenticate(
    ctx: &RequestMonitoring,
    auth_config: &'static AuthenticationConfig,
    link_uri: &reqwest::Url,
    client: &mut PqStream<impl AsyncRead + AsyncWrite + Unpin>,
) -> auth::Result<NodeInfo> {
    ctx.set_auth_method(crate::context::AuthMethod::Web);

    // registering waiter can fail if we get unlucky with rng.
    // just try again.
    let (psql_session_id, waiter) = loop {
        let psql_session_id = new_psql_session_id();

        match control_plane::mgmt::get_waiter(&psql_session_id) {
            Ok(waiter) => break (psql_session_id, waiter),
            Err(_e) => continue,
        }
    };

    let span = info_span!("web", psql_session_id = &psql_session_id);
    let greeting = hello_message(link_uri, &psql_session_id);

    // Give user a URL to spawn a new database.
    info!(parent: &span, "sending the auth URL to the user");
    client
        .write_message_noflush(&Be::AuthenticationOk)?
        .write_message_noflush(&Be::CLIENT_ENCODING)?
        .write_message(&Be::NoticeResponse(&greeting))
        .await?;

    // Wait for web console response (see `mgmt`).
    info!(parent: &span, "waiting for console's reply...");
    let db_info = tokio::time::timeout(auth_config.webauth_confirmation_timeout, waiter)
        .await
        .map_err(|_elapsed| {
            auth::AuthError::confirmation_timeout(auth_config.webauth_confirmation_timeout.into())
        })?
        .map_err(WebAuthError::from)?;

    if auth_config.ip_allowlist_check_enabled {
        if let Some(allowed_ips) = &db_info.allowed_ips {
            if !auth::check_peer_addr_is_in_list(&ctx.peer_addr(), allowed_ips) {
                return Err(auth::AuthError::ip_address_not_allowed(ctx.peer_addr()));
            }
        }
    }

    client.write_message_noflush(&Be::NoticeResponse("Connecting to database."))?;

    // This config should be self-contained, because we won't
    // take username or dbname from client's startup message.
    let mut config = compute::ConnCfg::new();
    config
        .host(&db_info.host)
        .port(db_info.port)
        .dbname(&db_info.dbname)
        .user(&db_info.user);

    ctx.set_dbname(db_info.dbname.into());
    ctx.set_user(db_info.user.into());
    ctx.set_project(db_info.aux.clone());
    info!("woken up a compute node");

    // Backwards compatibility. pg_sni_proxy uses "--" in domain names
    // while direct connections do not. Once we migrate to pg_sni_proxy
    // everywhere, we can remove this.
    if db_info.host.contains("--") {
        // we need TLS connection with SNI info to properly route it
        config.ssl_mode(SslMode::Require);
    } else {
        config.ssl_mode(SslMode::Disable);
    }

    if let Some(password) = db_info.password {
        config.password(password.as_ref());
    }

    Ok(NodeInfo {
        config,
        aux: db_info.aux,
        allow_self_signed_compute: false, // caller may override
    })
}
