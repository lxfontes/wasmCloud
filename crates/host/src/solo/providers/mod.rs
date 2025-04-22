//! Provider module
//!
//! The root of this module includes functionality for running and managing provider binaries. The
//! submodules contain builtin implementations of wasmCloud capabilities providers.
use std::collections::{BTreeMap, HashMap};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _};
use async_nats::Client;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use bytes::Bytes;
use cloudevents::EventBuilderV10;
use futures::{stream, Future, StreamExt};
use nkeys::XKey;
use tokio::io::AsyncWriteExt;
use tokio::process;
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use tracing::{error, instrument, trace, warn};
use uuid::Uuid;
use wascap::jwt::{CapabilityProvider, Token};
use wasmcloud_core::{
    provider_config_update_subject, HealthCheckResponse, HostData, InterfaceLinkDefinition,
    OtelConfig,
};
use wasmcloud_runtime::capability::secrets::store::SecretValue;
use wasmcloud_tracing::context::TraceContextInjector;

use crate::jwt;
use crate::solo::{config::ConfigBundle, SimpleMap};
use crate::solo::{event, injector_to_headers};

use super::Host;

mod http_server;
mod messaging_nats;

/// An Provider instance
#[derive(Debug)]
pub(crate) struct Provider {
    pub(crate) image_ref: String,
    pub(crate) claims_token: Option<jwt::Token<jwt::CapabilityProvider>>,
    pub(crate) xkey: XKey,
    pub(crate) annotations: SimpleMap,
    /// Shutdown signal for the provider, set to `false` initially. When set to `true`, the
    /// tasks running the provider, health check, and config watcher will stop.
    pub(crate) shutdown: Arc<AtomicBool>,
    /// Tasks running the provider, health check, and config watcher
    pub(crate) tasks: JoinSet<()>,
}

impl Host {
    /// Fetch configuration and secrets for a capability provider, forming the host configuration
    /// with links, config and secrets to pass to that provider. Also returns the config bundle
    /// which is used to watch for changes to the configuration, or can be discarded if
    /// configuration updates aren't necessary.
    pub(crate) async fn prepare_provider_config(
        &self,
        config: &HashMap<String, String>,
        claims_token: Option<&Token<CapabilityProvider>>,
        provider_id: &str,
        provider_xkey: &XKey,
        imports: &Vec<InterfaceLinkDefinition>,
        annotations: &HashMap<String, String>,
    ) -> anyhow::Result<HostData> {
        // We only need to store the public key of the provider xkey, as the private key is only needed by the provider
        let xkey = XKey::from_public_key(&provider_xkey.public_key())
            .context("failed to create XKey from provider public key xkey")?;

        let lattice_rpc_user_seed = self
            .host_config
            .rpc_key
            .as_ref()
            .map(|key| key.seed())
            .transpose()
            .context("private key missing for provider RPC key")?;
        let default_rpc_timeout_ms = Some(
            self.host_config
                .rpc_timeout
                .as_millis()
                .try_into()
                .context("failed to convert rpc_timeout to u64")?,
        );
        let otel_config = OtelConfig {
            enable_observability: self.host_config.otel_config.enable_observability,
            enable_traces: self.host_config.otel_config.enable_traces,
            enable_metrics: self.host_config.otel_config.enable_metrics,
            enable_logs: self.host_config.otel_config.enable_logs,
            observability_endpoint: self.host_config.otel_config.observability_endpoint.clone(),
            traces_endpoint: self.host_config.otel_config.traces_endpoint.clone(),
            metrics_endpoint: self.host_config.otel_config.metrics_endpoint.clone(),
            logs_endpoint: self.host_config.otel_config.logs_endpoint.clone(),
            protocol: self.host_config.otel_config.protocol,
            additional_ca_paths: self.host_config.otel_config.additional_ca_paths.clone(),
            trace_level: self.host_config.otel_config.trace_level.clone(),
            ..Default::default()
        };

        // The provider itself needs to know its private key
        let provider_xkey_private_key = if let Ok(seed) = provider_xkey.seed() {
            seed
        } else if self.host_config.secrets_topic_prefix.is_none() {
            "".to_string()
        } else {
            // This should never happen since this returns an error when an Xkey is
            // created from a public key, but if we can't generate one for whatever
            // reason, we should bail.
            bail!("failed to generate seed for provider xkey")
        };
        let dummy = XKey::new();
        let host_data = HostData {
            host_id: self.host_key.public_key(),
            lattice_rpc_prefix: self.host_config.domain.to_string(),
            link_name: "default".to_string(),
            lattice_rpc_user_jwt: self.host_config.rpc_jwt.clone().unwrap_or_default(),
            lattice_rpc_user_seed: lattice_rpc_user_seed.unwrap_or_default(),
            lattice_rpc_url: self.host_config.rpc_nats_url.to_string(),
            env_values: vec![],
            instance_id: Uuid::new_v4().to_string(),
            provider_key: provider_id.to_string(),
            link_definitions: imports.clone(),
            config: config.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
            // TODO(lxf): refactor
            secrets: Default::default(),
            provider_xkey_private_key,
            // TODO(lxf): refactor
            host_xkey_public_key: dummy.public_key(),
            cluster_issuers: vec![],
            default_rpc_timeout_ms,
            log_level: Some(self.host_config.log_level.clone()),
            structured_logging: self.host_config.enable_structured_logging,
            otel_config,
        };
        Ok(host_data)
    }

    /// Start a binary provider
    #[allow(clippy::too_many_arguments)]
    #[instrument(level = "debug", skip_all)]
    pub(crate) async fn start_wrpc_nats_provider(
        self: Arc<Self>,
        path: PathBuf,
        host_data: HostData,
        provider_xkey: XKey,
        provider_id: &str,
        shutdown: Arc<AtomicBool>,
    ) -> anyhow::Result<JoinSet<()>> {
        let host_data =
            serde_json::to_vec(&host_data).context("failed to serialize provider data")?;

        trace!("spawn provider process");

        let mut child = provider_command(&path, host_data)
            .await
            .context("failed to configure binary provider command")?;

        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            match child.wait().await {
                Ok(status) => {
                    // NOTE(lxf): refactor
                    // maybe restart providers, but we need to report the error somewhere
                    trace!(
                        path = ?path.display(),
                        status = ?status,
                        "provider crashed but will not be restarted",
                    );
                }
                Err(e) => {
                    error!(
                        path = ?path.display(),
                        err = ?e,
                        "failed to wait for provider to execute",
                    );
                }
            }
        });

        Ok(tasks)
    }
}

/// Using the provided path as the provider binary, start the provider process and
/// pass the host data to it over stdin. Returns the child process handle which
/// has already been spawned.
async fn provider_command(path: &Path, host_data: Vec<u8>) -> anyhow::Result<process::Child> {
    let mut child_cmd = process::Command::new(path);
    // Prevent the provider from inheriting the host's environment, with the exception of
    // the following variables we manually add back
    child_cmd.env_clear();

    if cfg!(windows) {
        // Proxy SYSTEMROOT to providers. Without this, providers on Windows won't be able to start
        child_cmd.env(
            "SYSTEMROOT",
            env::var("SYSTEMROOT").context("SYSTEMROOT is not set. Providers cannot be started")?,
        );
    }

    // Proxy RUST_LOG to (Rust) providers, so they can use the same module-level directives
    if let Ok(rust_log) = env::var("RUST_LOG") {
        let _ = child_cmd.env("RUST_LOG", rust_log);
    }

    // Pass through any OTEL configuration options to the provider as well
    for (k, v) in env::vars() {
        if k.starts_with("OTEL_") {
            let _ = child_cmd.env(k, v);
        }
    }

    let mut child = child_cmd
        .stdin(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn provider process")?;
    let mut stdin = child.stdin.take().context("failed to take stdin")?;
    stdin
        .write_all(STANDARD.encode(host_data).as_bytes())
        .await
        .context("failed to write provider data")?;
    stdin
        .write_all(b"\r\n")
        .await
        .context("failed to write newline")?;
    stdin.shutdown().await.context("failed to close stdin")?;

    Ok(child)
}
