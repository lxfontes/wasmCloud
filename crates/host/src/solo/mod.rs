#![allow(clippy::type_complexity)]

use core::sync::atomic::Ordering;

use std::collections::hash_map::Entry;
use std::collections::{hash_map, BTreeMap, HashMap};
use std::env::consts::{ARCH, FAMILY, OS};
use std::future::Future;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context as _};
use async_nats::jetstream::kv::Store;
use bytes::{BufMut, Bytes, BytesMut};
use claims::{Claims, StoredClaims};
use cloudevents::{EventBuilder, EventBuilderV10};
use futures::future::Either;
use futures::stream::{AbortHandle, Abortable, SelectAll};
use futures::{join, stream, try_join, FutureExt, Stream, StreamExt, TryFutureExt, TryStreamExt};
use hyper_util::rt::{TokioExecutor, TokioIo};
use nkeys::{KeyPair, KeyPairType, XKey};
use providers::Provider;
use secrecy::SecretBox;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sysinfo::System;
use tokio::io::AsyncWrite;
use tokio::net::TcpListener;
use tokio::spawn;
use tokio::sync::{mpsc, watch, RwLock, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval_at, Instant};
use tokio_stream::wrappers::IntervalStream;
use tracing::{debug, debug_span, error, info, instrument, trace, warn, Instrument as _};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use wascap::jwt;
use wasmcloud_control_interface::{
    ComponentAuctionAck, ComponentAuctionRequest, ComponentDescription, CtlResponse,
    DeleteInterfaceLinkDefinitionRequest, HostInventory, HostLabel, HostLabelIdentifier, Link,
    ProviderAuctionAck, ProviderAuctionRequest, ProviderDescription, RegistryCredential,
    ScaleComponentCommand, StartProviderCommand, StopHostCommand, StopProviderCommand,
    UpdateComponentCommand,
};
use wasmcloud_core::{ComponentId, CTL_API_VERSION_1};
use wasmcloud_runtime::capability::secrets::store::SecretValue;
use wasmcloud_runtime::component::WrpcServeEvent;
use wasmcloud_runtime::Runtime;
use wasmcloud_secrets_types::SECRET_PREFIX;
use wasmcloud_tracing::context::TraceContextInjector;
use wasmcloud_tracing::{global, InstrumentationScope, KeyValue};

use crate::registry::RegistryCredentialExt;
use crate::solo::jetstream::create_bucket;
use crate::workload_identity::WorkloadIdentityConfig;
use crate::{
    fetch_component, HostMetrics, OciConfig, PolicyHostInfo, PolicyManager, PolicyResponse,
    RegistryAuth, RegistryConfig, RegistryType, ResourceRef, SecretsManager,
};

pub mod api;
mod claims;
mod event;
mod experimental;
mod handler;
mod jetstream;
mod providers;

pub mod config;
/// wasmCloud host configuration
pub mod host_config;

pub use self::experimental::Features;
pub use self::host_config::Host as HostConfig;
pub use jetstream::ComponentSpecification;

use self::config::{BundleGenerator, ConfigBundle};
use self::handler::Handler;

const MAX_INVOCATION_CHANNEL_SIZE: usize = 5000;
const MIN_INVOCATION_CHANNEL_SIZE: usize = 256;

#[derive(Debug)]
struct ControlMapper {
    all_streams: SelectAll<async_nats::Subscriber>,
}

impl Stream for ControlMapper {
    type Item = async_nats::Message;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.all_streams.poll_next_unpin(cx)
    }
}

#[derive(Clone, Default)]
struct AsyncBytesMut(Arc<std::sync::Mutex<BytesMut>>);

impl AsyncWrite for AsyncBytesMut {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        Poll::Ready({
            self.0
                .lock()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))?
                .put_slice(buf);
            Ok(buf.len())
        })
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl TryFrom<AsyncBytesMut> for Vec<u8> {
    type Error = anyhow::Error;

    fn try_from(buf: AsyncBytesMut) -> Result<Self, Self::Error> {
        buf.0
            .lock()
            .map(|buf| buf.clone().into())
            .map_err(|e| anyhow!(e.to_string()).context("failed to lock"))
    }
}

impl ControlMapper {
    #[instrument]
    async fn new(
        nats: &async_nats::Client,
        topic_prefix: &str,
        lattice: &str,
        host_key: &KeyPair,
        component_auction: bool,
        provider_auction: bool,
    ) -> anyhow::Result<Self> {
        let host_id = host_key.public_key();
        let subs = vec![nats.subscribe(format!("{topic_prefix}.{host_id}.>"))];
        let streams = futures::future::join_all(subs)
            .await
            .into_iter()
            .collect::<Result<Vec<_>, async_nats::SubscribeError>>()
            .context("failed to subscribe to queues")?;
        Ok(Self {
            all_streams: futures::stream::select_all(streams),
        })
    }
}

type Annotations = BTreeMap<String, String>;

#[derive(Debug)]
struct Component {
    component: wasmcloud_runtime::Component<Handler>,
    wrpc_id: Arc<String>,
    handler: Handler,
    exports: JoinHandle<()>,
    annotations: Annotations,
    /// Maximum number of instances of this component that can be running at once
    max_instances: NonZeroUsize,
    image: Arc<String>,
    events: mpsc::Sender<WrpcServeEvent<<WrpcServer as wrpc_transport::Serve>::Context>>,
    permits: Arc<Semaphore>,
}

impl Deref for Component {
    type Target = wasmcloud_runtime::Component<Handler>;

    fn deref(&self) -> &Self::Target {
        &self.component
    }
}

#[derive(Clone)]
struct WrpcServer {
    nats: wrpc_transport_nats::Client,
    claims: Option<Arc<jwt::Claims<jwt::Component>>>,
    id: Arc<String>,
    image_reference: Arc<String>,
    annotations: Arc<Annotations>,
    metrics: Arc<HostMetrics>,
}

struct InvocationContext {
    start_at: Instant,
    attributes: Vec<KeyValue>,
    span: tracing::Span,
}

impl Deref for InvocationContext {
    type Target = tracing::Span;

    fn deref(&self) -> &Self::Target {
        &self.span
    }
}

impl wrpc_transport::Serve for WrpcServer {
    type Context = InvocationContext;
    type Outgoing = <wrpc_transport_nats::Client as wrpc_transport::Serve>::Outgoing;
    type Incoming = <wrpc_transport_nats::Client as wrpc_transport::Serve>::Incoming;

    #[instrument(
        level = "info",
        skip(self, paths),
        fields(
            component_id = ?self.id,
            component_ref = ?self.image_reference)
    )]
    async fn serve(
        &self,
        instance: &str,
        func: &str,
        paths: impl Into<Arc<[Box<[Option<usize>]>]>> + Send,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(Self::Context, Self::Outgoing, Self::Incoming)>>
            + Send
            + 'static,
    > {
        debug!("serving invocations");
        let invocations = self.nats.serve(instance, func, paths).await?;

        let func: Arc<str> = Arc::from(func);
        let instance: Arc<str> = Arc::from(instance);
        let annotations = Arc::clone(&self.annotations);
        let id = Arc::clone(&self.id);
        let image_reference = Arc::clone(&self.image_reference).to_string();
        let metrics = Arc::clone(&self.metrics);
        let claims = self.claims.clone();
        Ok(invocations.and_then(move |(cx, tx, rx)| {
            let image_reference = image_reference.clone();
            let annotations = Arc::clone(&annotations);
            let claims = claims.clone();
            let func = Arc::clone(&func);
            let id = Arc::clone(&id);
            let instance = Arc::clone(&instance);
            let metrics = Arc::clone(&metrics);
            let span = tracing::info_span!("component_invocation", func = %func, id = %id, instance = %instance);
            async move {
                if let Some(ref cx) = cx {
                    // Coerce the HashMap<String, Vec<String>> into a Vec<(String, String)> by
                    // flattening the values
                    let trace_context = cx
                        .iter()
                        .flat_map(|(key, value)| {
                            value
                                .iter()
                                .map(|v| (key.to_string(), v.to_string()))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<(String, String)>>();
                    span.set_parent(wasmcloud_tracing::context::get_span_context(&trace_context));
                }

                Ok((
                    InvocationContext{
                        start_at: Instant::now(),
                        // TODO(metrics): insert information about the source once we have concrete context data
                        attributes: vec![
                            KeyValue::new("component.ref", image_reference),
                            KeyValue::new("lattice", metrics.lattice_id.clone()),
                            KeyValue::new("host", metrics.host_id.clone()),
                            KeyValue::new("operation", format!("{instance}/{func}")),
                        ],
                        span,
                    },
                    tx,
                    rx,
                ))
            }
        }))
    }
}

#[derive(Debug, Clone)]

enum TrackerEntry {
    Component(Arc<Component>),
    Provider(Arc<Provider>),
}
type Tracker = HashMap<ComponentId, TrackerEntry>;

/// wasmCloud Host
pub struct Host {
    components: Arc<RwLock<HashMap<ComponentId, Arc<Component>>>>,
    tracker: Arc<RwLock<Tracker>>,
    event_builder: EventBuilderV10,
    friendly_name: String,
    heartbeat: AbortHandle,
    host_config: HostConfig,
    host_key: Arc<KeyPair>,
    host_token: Arc<jwt::Token<jwt::Host>>,
    labels: Arc<RwLock<BTreeMap<String, String>>>,
    ctl_topic_prefix: String,
    /// NATS client to use for control interface subscriptions and jetstream queries
    ctl_nats: async_nats::Client,
    /// NATS client to use for RPC calls
    rpc_nats: Arc<async_nats::Client>,
    data: Store,
    /// Task to watch for changes in the LATTICEDATA store
    data_watch: AbortHandle,
    /// The provider map is a map of provider component ID to provider
    providers: RwLock<HashMap<String, Provider>>,
    registry_config: RwLock<HashMap<String, RegistryConfig>>,
    runtime: Runtime,
    start_at: Instant,
    stop_tx: watch::Sender<Option<Instant>>,
    stop_rx: watch::Receiver<Option<Instant>>,
    queue: AbortHandle,
    // Component ID -> All Links
    links: RwLock<HashMap<String, Vec<Link>>>,
    component_claims: Arc<RwLock<HashMap<ComponentId, jwt::Claims<jwt::Component>>>>, // TODO: use a single map once Claims is an enum
    provider_claims: Arc<RwLock<HashMap<String, jwt::Claims<jwt::CapabilityProvider>>>>,
    metrics: Arc<HostMetrics>,
    max_execution_time: Duration,
    messaging_links:
        Arc<RwLock<HashMap<Arc<str>, Arc<RwLock<HashMap<Box<str>, async_nats::Client>>>>>>,
    /// Experimental features to enable in the host that gate functionality
    experimental_features: Features,
    ready: Arc<AtomicBool>,
    /// A set of host tasks
    #[allow(unused)]
    tasks: JoinSet<()>,
}

/// Given the NATS address, authentication jwt, seed, tls requirement and optional request timeout,
/// attempt to establish connection.
///
/// This function should be used to create a NATS client for Host communication, for non-host NATS
/// clients we recommend using async-nats directly.
///
/// # Errors
///
/// Returns an error if:
/// - Only one of JWT or seed is specified, as we cannot authenticate with only one of them
/// - Connection fails
pub async fn connect_nats(
    addr: impl async_nats::ToServerAddrs,
    jwt: Option<&String>,
    key: Option<Arc<KeyPair>>,
    require_tls: bool,
    request_timeout: Option<Duration>,
    workload_identity_config: Option<WorkloadIdentityConfig>,
) -> anyhow::Result<async_nats::Client> {
    let opts = match (jwt, key, workload_identity_config) {
        (Some(jwt), Some(key), None) => {
            async_nats::ConnectOptions::with_jwt(jwt.to_string(), move |nonce| {
                let key = key.clone();
                async move { key.sign(&nonce).map_err(async_nats::AuthError::new) }
            })
        }
        (Some(_), None, _) | (None, Some(_), _) => {
            bail!("cannot authenticate if only one of jwt or seed is specified")
        }
        (jwt, key, Some(wid_cfg)) => {
            setup_workload_identity_nats_connect_options(jwt, key, wid_cfg).await?
        }
        _ => async_nats::ConnectOptions::new(),
    };
    let opts = if let Some(timeout) = request_timeout {
        opts.request_timeout(Some(timeout))
    } else {
        opts
    };
    let opts = opts.require_tls(require_tls);
    opts.connect(addr)
        .await
        .context("failed to connect to NATS")
}

#[cfg(unix)]
async fn setup_workload_identity_nats_connect_options(
    jwt: Option<&String>,
    key: Option<Arc<KeyPair>>,
    wid_cfg: WorkloadIdentityConfig,
) -> anyhow::Result<async_nats::ConnectOptions> {
    let wid_cfg = Arc::new(wid_cfg);
    let jwt = jwt.map(String::to_string).map(Arc::new);
    let key = key.clone();

    // Return an auth callback that'll get called any time the
    // NATS connection needs to be (re-)established. This is
    // necessary to ensure that we always provide a recently
    // issued JWT-SVID.
    Ok(async_nats::ConnectOptions::with_auth_callback(
        move |nonce| {
            let key = key.clone();
            let jwt = jwt.clone();
            let wid_cfg = wid_cfg.clone();

            let fetch_svid_handle = tokio::spawn(async move {
                let mut client = spiffe::WorkloadApiClient::default()
                    .await
                    .map_err(async_nats::AuthError::new)?;
                client
                    .fetch_jwt_svid(&[wid_cfg.auth_service_audience.as_str()], None)
                    .await
                    .map_err(async_nats::AuthError::new)
            });

            async move {
                let svid = fetch_svid_handle
                    .await
                    .map_err(async_nats::AuthError::new)?
                    .map_err(async_nats::AuthError::new)?;

                let mut auth = async_nats::Auth::new();
                if let Some(key) = key {
                    let signature = key.sign(&nonce).map_err(async_nats::AuthError::new)?;
                    auth.signature = Some(signature);
                }
                if let Some(jwt) = jwt {
                    auth.jwt = Some(jwt.to_string());
                }
                auth.token = Some(svid.token().into());
                Ok(auth)
            }
        },
    ))
}

#[cfg(target_family = "windows")]
async fn setup_workload_identity_nats_connect_options(
    jwt: Option<&String>,
    key: Option<Arc<KeyPair>>,
    wid_cfg: WorkloadIdentityConfig,
) -> anyhow::Result<async_nats::ConnectOptions> {
    bail!("workload identity is not supported on Windows")
}

#[derive(Debug, Default)]
struct SupplementalConfig {
    registry_config: Option<HashMap<String, RegistryConfig>>,
}

#[instrument(level = "debug", skip_all)]
async fn load_supplemental_config(
    ctl_nats: &async_nats::Client,
    lattice: &str,
    labels: &BTreeMap<String, String>,
) -> anyhow::Result<SupplementalConfig> {
    #[derive(Deserialize, Default)]
    struct SerializedSupplementalConfig {
        #[serde(default, rename = "registryCredentials")]
        registry_credentials: Option<HashMap<String, RegistryCredential>>,
    }

    let cfg_topic = format!("wasmbus.cfg.{lattice}.req");
    let cfg_payload = serde_json::to_vec(&json!({
        "labels": labels,
    }))
    .context("failed to serialize config payload")?;

    debug!("requesting supplemental config");
    match ctl_nats.request(cfg_topic, cfg_payload.into()).await {
        Ok(resp) => {
            match serde_json::from_slice::<SerializedSupplementalConfig>(resp.payload.as_ref()) {
                Ok(ser_cfg) => Ok(SupplementalConfig {
                    registry_config: ser_cfg.registry_credentials.and_then(|creds| {
                        creds
                            .into_iter()
                            .map(|(k, v)| {
                                debug!(registry_url = %k, "set registry config");
                                v.into_registry_config().map(|v| (k, v))
                            })
                            .collect::<anyhow::Result<_>>()
                            .ok()
                    }),
                }),
                Err(e) => {
                    error!(
                        ?e,
                        "failed to deserialize supplemental config. Defaulting to empty config"
                    );
                    Ok(SupplementalConfig::default())
                }
            }
        }
        Err(e) => {
            error!(
                ?e,
                "failed to request supplemental config. Defaulting to empty config"
            );
            Ok(SupplementalConfig::default())
        }
    }
}

#[instrument(level = "debug", skip_all)]
async fn merge_registry_config(
    registry_config: &RwLock<HashMap<String, RegistryConfig>>,
    oci_opts: OciConfig,
) -> () {
    let mut registry_config = registry_config.write().await;
    let allow_latest = oci_opts.allow_latest;
    let additional_ca_paths = oci_opts.additional_ca_paths;

    // update auth for specific registry, if provided
    if let Some(reg) = oci_opts.oci_registry {
        match registry_config.entry(reg.clone()) {
            Entry::Occupied(_entry) => {
                // note we don't update config here, since the config service should take priority
                warn!(oci_registry_url = %reg, "ignoring OCI registry config, overridden by config service");
            }
            Entry::Vacant(entry) => {
                debug!(oci_registry_url = %reg, "set registry config");
                entry.insert(
                    RegistryConfig::builder()
                        .reg_type(RegistryType::Oci)
                        .auth(RegistryAuth::from((
                            oci_opts.oci_user,
                            oci_opts.oci_password,
                        )))
                        .build()
                        .expect("failed to build registry config"),
                );
            }
        }
    }

    // update or create entry for all registries in allowed_insecure
    oci_opts.allowed_insecure.into_iter().for_each(|reg| {
        match registry_config.entry(reg.clone()) {
            Entry::Occupied(mut entry) => {
                debug!(oci_registry_url = %reg, "set allowed_insecure");
                entry.get_mut().set_allow_insecure(true);
            }
            Entry::Vacant(entry) => {
                debug!(oci_registry_url = %reg, "set allowed_insecure");
                entry.insert(
                    RegistryConfig::builder()
                        .reg_type(RegistryType::Oci)
                        .allow_insecure(true)
                        .build()
                        .expect("failed to build registry config"),
                );
            }
        }
    });

    // update allow_latest for all registries
    registry_config.iter_mut().for_each(|(url, config)| {
        if !additional_ca_paths.is_empty() {
            config.set_additional_ca_paths(additional_ca_paths.clone());
        }
        if allow_latest {
            debug!(oci_registry_url = %url, "set allow_latest");
        }
        config.set_allow_latest(allow_latest);
    });
}

impl Host {
    const DEFAULT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

    const NAME_ADJECTIVES: &'static str = "
    autumn hidden bitter misty silent empty dry dark summer
    icy delicate quiet white cool spring winter patient
    twilight dawn crimson wispy weathered blue billowing
    broken cold damp falling frosty green long late lingering
    bold little morning muddy old red rough still small
    sparkling bouncing shy wandering withered wild black
    young holy solitary fragrant aged snowy proud floral
    restless divine polished ancient purple lively nameless
    gray orange mauve
    ";

    const NAME_NOUNS: &'static str = "
    waterfall river breeze moon rain wind sea morning
    snow lake sunset pine shadow leaf dawn glitter forest
    hill cloud meadow sun glade bird brook butterfly
    bush dew dust field fire flower firefly ladybug feather grass
    haze mountain night pond darkness snowflake silence
    sound sky shape stapler surf thunder violet water wildflower
    wave water resonance sun timber dream cherry tree fog autocorrect
    frost voice paper frog smoke star hamster ocean emoji robot
    ";

    /// Generate a friendly name for the host based on a random number.
    /// Names we pulled from a list of friendly or neutral adjectives and nouns suitable for use in
    /// public and on hosts/domain names
    fn generate_friendly_name() -> Option<String> {
        let adjectives: Vec<_> = Self::NAME_ADJECTIVES.split_whitespace().collect();
        let nouns: Vec<_> = Self::NAME_NOUNS.split_whitespace().collect();
        names::Generator::new(&adjectives, &nouns, names::Name::Numbered).next()
    }

    /// Construct a new [Host] returning a tuple of its [Arc] and an async shutdown function.
    #[instrument(level = "debug", skip_all)]
    pub async fn new(
        config: HostConfig,
    ) -> anyhow::Result<(Arc<Self>, impl Future<Output = anyhow::Result<()>>)> {
        let host_key = if let Some(host_key) = &config.host_key {
            ensure!(host_key.key_pair_type() == KeyPairType::Server);
            Arc::clone(host_key)
        } else {
            Arc::new(KeyPair::new(KeyPairType::Server))
        };

        let mut labels = BTreeMap::from([
            ("hostcore.arch".into(), ARCH.into()),
            ("hostcore.os".into(), OS.into()),
            ("hostcore.osfamily".into(), FAMILY.into()),
        ]);
        labels.extend(config.labels.clone().into_iter());
        let friendly_name =
            Self::generate_friendly_name().context("failed to generate friendly name")?;

        let host_issuer = Arc::new(KeyPair::new_account());
        let claims = jwt::Claims::<jwt::Host>::new(
            friendly_name.clone(),
            host_issuer.public_key(),
            host_key.public_key().clone(),
            Some(HashMap::from_iter([(
                "self_signed".to_string(),
                "true".to_string(),
            )])),
        );
        let jwt = claims
            .encode(&host_issuer)
            .context("failed to encode host claims")?;
        let host_token = Arc::new(jwt::Token { jwt, claims });

        let start_evt = json!({
            "friendly_name": friendly_name,
            "labels": labels,
            "uptime_seconds": 0,
            "version": config.version,
        });

        let workload_identity_config = if config.experimental_features.workload_identity_auth {
            Some(WorkloadIdentityConfig::from_env()?)
        } else {
            None
        };

        let ((ctl_nats, mapper), rpc_nats) = try_join!(
            async {
                debug!(
                    ctl_nats_url = config.ctl_nats_url.as_str(),
                    "connecting to NATS control server"
                );
                let ctl_nats = connect_nats(
                    config.ctl_nats_url.as_str(),
                    config.ctl_jwt.as_ref(),
                    config.ctl_key.clone(),
                    config.ctl_tls,
                    None,
                    workload_identity_config.clone(),
                )
                .await
                .context("failed to establish NATS control server connection")?;
                let mapper = ControlMapper::new(
                    &ctl_nats,
                    &config.ctl_topic_prefix,
                    &config.lattice,
                    &host_key,
                    config.enable_component_auction,
                    config.enable_provider_auction,
                )
                .await
                .context("failed to initialize queue")?;
                ctl_nats.flush().await.context("failed to flush")?;
                Ok((ctl_nats, mapper))
            },
            async {
                debug!(
                    rpc_nats_url = config.rpc_nats_url.as_str(),
                    "connecting to NATS RPC server"
                );
                connect_nats(
                    config.rpc_nats_url.as_str(),
                    config.rpc_jwt.as_ref(),
                    config.rpc_key.clone(),
                    config.rpc_tls,
                    Some(config.rpc_timeout),
                    workload_identity_config.clone(),
                )
                .await
                .context("failed to establish NATS RPC server connection")
            }
        )?;

        let start_at = Instant::now();

        let heartbeat_interval = config
            .heartbeat_interval
            .unwrap_or(Self::DEFAULT_HEARTBEAT_INTERVAL);
        let heartbeat_start_at = start_at
            .checked_add(heartbeat_interval)
            .context("failed to compute heartbeat start time")?;
        let heartbeat = IntervalStream::new(interval_at(heartbeat_start_at, heartbeat_interval));

        let (stop_tx, stop_rx) = watch::channel(None);

        let (runtime, _epoch) = Runtime::builder()
            .max_execution_time(config.max_execution_time)
            .max_linear_memory(config.max_linear_memory)
            .max_components(config.max_components)
            .max_core_instances_per_component(config.max_core_instances_per_component)
            .max_component_size(config.max_component_size)
            .experimental_features(config.experimental_features.into())
            .build()
            .context("failed to build runtime")?;
        let event_builder = EventBuilderV10::new().source(host_key.public_key());

        let ctl_jetstream = if let Some(domain) = config.js_domain.as_ref() {
            async_nats::jetstream::with_domain(ctl_nats.clone(), domain)
        } else {
            async_nats::jetstream::new(ctl_nats.clone())
        };
        let bucket = format!("LATTICEDATA_{}", config.lattice);
        let data = create_bucket(&ctl_jetstream, &bucket).await?;

        let (queue_abort, queue_abort_reg) = AbortHandle::new_pair();
        let (heartbeat_abort, heartbeat_abort_reg) = AbortHandle::new_pair();
        let (data_watch_abort, data_watch_abort_reg) = AbortHandle::new_pair();

        let supplemental_config = if config.config_service_enabled {
            load_supplemental_config(&ctl_nats, &config.lattice, &labels).await?
        } else {
            SupplementalConfig::default()
        };

        let registry_config = RwLock::new(supplemental_config.registry_config.unwrap_or_default());
        merge_registry_config(&registry_config, config.oci_opts.clone()).await;

        // If provided, secrets topic must be non-empty
        // TODO(#2411): Validate secrets topic prefix as a valid NATS subject
        ensure!(
            config.secrets_topic_prefix.is_none()
                || config
                    .secrets_topic_prefix
                    .as_ref()
                    .is_some_and(|topic| !topic.is_empty()),
            "secrets topic prefix must be non-empty"
        );

        let scope = InstrumentationScope::builder("wasmcloud-host")
            .with_version(config.version.clone())
            .with_attributes(vec![
                KeyValue::new("host.id", host_key.public_key()),
                KeyValue::new("host.version", config.version.clone()),
                KeyValue::new("host.arch", ARCH),
                KeyValue::new("host.os", OS),
                KeyValue::new("host.osfamily", FAMILY),
                KeyValue::new("host.friendly_name", friendly_name.clone()),
                KeyValue::new("host.hostname", System::host_name().unwrap_or_default()),
                KeyValue::new(
                    "host.kernel_version",
                    System::kernel_version().unwrap_or_default(),
                ),
                KeyValue::new("host.os_version", System::os_version().unwrap_or_default()),
            ])
            .build();
        let meter = global::meter_with_scope(scope);
        let metrics = HostMetrics::new(
            &meter,
            host_key.public_key(),
            config.lattice.to_string(),
            None,
        )
        .context("failed to create HostMetrics instance")?;

        let max_execution_time_ms = config.max_execution_time;

        debug!("Feature flags: {:?}", config.experimental_features);

        let mut tasks = JoinSet::new();
        let ready = Arc::new(AtomicBool::new(true));
        if let Some(addr) = config.http_admin {
            let socket = TcpListener::bind(addr)
                .await
                .context("failed to bind on HTTP administration endpoint")?;
            let ready = Arc::clone(&ready);
            let svc = hyper::service::service_fn(move |req| {
                const OK: &str = r#"{"status":"ok"}"#;
                const FAIL: &str = r#"{"status":"failure"}"#;
                let ready = Arc::clone(&ready);
                async move {
                    let (http::request::Parts { method, uri, .. }, _) = req.into_parts();
                    match (method.as_str(), uri.path()) {
                        ("HEAD", "/livez") => Ok(http::Response::default()),
                        ("GET", "/livez") => Ok(http::Response::new(http_body_util::Full::new(
                            Bytes::from(OK),
                        ))),
                        (method, "/livez") => http::Response::builder()
                            .status(http::StatusCode::METHOD_NOT_ALLOWED)
                            .body(http_body_util::Full::new(Bytes::from(format!(
                                "method `{method}` not supported for path `/livez`"
                            )))),
                        ("HEAD", "/readyz") => {
                            if ready.load(Ordering::Relaxed) {
                                Ok(http::Response::default())
                            } else {
                                http::Response::builder()
                                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                                    .body(http_body_util::Full::default())
                            }
                        }
                        ("GET", "/readyz") => {
                            if ready.load(Ordering::Relaxed) {
                                Ok(http::Response::new(http_body_util::Full::new(Bytes::from(
                                    OK,
                                ))))
                            } else {
                                http::Response::builder()
                                    .status(http::StatusCode::INTERNAL_SERVER_ERROR)
                                    .body(http_body_util::Full::new(Bytes::from(FAIL)))
                            }
                        }
                        (method, "/readyz") => http::Response::builder()
                            .status(http::StatusCode::METHOD_NOT_ALLOWED)
                            .body(http_body_util::Full::new(Bytes::from(format!(
                                "method `{method}` not supported for path `/readyz`"
                            )))),
                        (.., path) => http::Response::builder()
                            .status(http::StatusCode::NOT_FOUND)
                            .body(http_body_util::Full::new(Bytes::from(format!(
                                "unknown endpoint `{path}`"
                            )))),
                    }
                }
            });
            let srv = hyper_util::server::conn::auto::Builder::new(TokioExecutor::new());
            tasks.spawn(async move {
                loop {
                    let stream = match socket.accept().await {
                        Ok((stream, _)) => stream,
                        Err(err) => {
                            error!(?err, "failed to accept HTTP administration connection");
                            continue;
                        }
                    };
                    let svc = svc.clone();
                    if let Err(err) = srv.serve_connection(TokioIo::new(stream), svc).await {
                        error!(?err, "failed to serve HTTP administration connection");
                    }
                }
            });
        }

        let host = Host {
            components: Arc::default(),
            tracker: Arc::default(),
            event_builder,
            friendly_name,
            heartbeat: heartbeat_abort.clone(),
            ctl_topic_prefix: config.ctl_topic_prefix.clone(),
            host_key,
            host_token,
            labels: Arc::new(RwLock::new(labels)),
            ctl_nats,
            rpc_nats: Arc::new(rpc_nats),
            experimental_features: config.experimental_features,
            host_config: config,
            data: data.clone(),
            data_watch: data_watch_abort.clone(),
            providers: RwLock::default(),
            registry_config,
            runtime,
            start_at,
            stop_rx,
            stop_tx,
            queue: queue_abort.clone(),
            links: RwLock::default(),
            component_claims: Arc::default(),
            provider_claims: Arc::default(),
            metrics: Arc::new(metrics),
            max_execution_time: max_execution_time_ms,
            messaging_links: Arc::default(),
            ready: Arc::clone(&ready),
            tasks,
        };

        let host = Arc::new(host);
        let control_handling = spawn({
            let host = Arc::clone(&host);
            async move {
                let mut queue = Abortable::new(mapper, queue_abort_reg);
                queue
                    .by_ref()
                    .for_each({
                        let host = Arc::clone(&host);
                        move |msg| {
                            let host = Arc::clone(&host);
                            async move { host.handle_ctl_message(msg).await }
                        }
                    })
                    .await;
                let deadline = { *host.stop_rx.borrow() };
                host.stop_tx.send_replace(deadline);
                if queue.is_aborted() {
                    info!("control interface queue task gracefully stopped");
                } else {
                    error!("control interface queue task unexpectedly stopped");
                }
            }
        });

        let data_watch: JoinHandle<anyhow::Result<_>> = spawn({
            let data = data.clone();
            let host = Arc::clone(&host);
            async move {
                // TODO(lxf): refactor
                // Shouldn't be watching all keys ( or any keys at all )
                let data_watch = data
                    .watch_all()
                    .await
                    .context("failed to watch lattice data bucket")?;
                let mut data_watch = Abortable::new(data_watch, data_watch_abort_reg);
                data_watch
                    .by_ref()
                    .for_each({
                        let host = Arc::clone(&host);
                        move |entry| {
                            let host = Arc::clone(&host);
                            async move {
                                match entry {
                                    Err(error) => {
                                        error!("failed to watch lattice data bucket: {error}");
                                    }
                                    Ok(entry) => host.process_entry(entry).await,
                                }
                            }
                        }
                    })
                    .await;

                let deadline = { *host.stop_rx.borrow() };
                host.stop_tx.send_replace(deadline);
                if data_watch.is_aborted() {
                    info!("data watch task gracefully stopped");
                } else {
                    error!("data watch task unexpectedly stopped");
                }
                Ok(())
            }
        });

        let heartbeat = spawn({
            let host = Arc::clone(&host);
            async move {
                let mut heartbeat = Abortable::new(heartbeat, heartbeat_abort_reg);
                heartbeat
                    .by_ref()
                    .for_each({
                        let host = Arc::clone(&host);
                        move |_| {
                            let host = Arc::clone(&host);
                            async move {
                                let heartbeat = match host.heartbeat().await {
                                    Ok(heartbeat) => heartbeat,
                                    Err(e) => {
                                        error!("failed to generate heartbeat: {e}");
                                        return;
                                    }
                                };

                                if let Err(e) =
                                    host.publish_event("host_heartbeat", heartbeat).await
                                {
                                    error!("failed to publish heartbeat: {e}");
                                }
                            }
                        }
                    })
                    .await;
                let deadline = { *host.stop_rx.borrow() };
                host.stop_tx.send_replace(deadline);
                if heartbeat.is_aborted() {
                    info!("heartbeat task gracefully stopped");
                } else {
                    error!("heartbeat task unexpectedly stopped");
                }
            }
        });

        // Process existing data without emitting events
        // TODO(lxf): refactor
        // Not sure what happens here
        // data.keys()
        //     .await
        //     .context("failed to read keys of lattice data bucket")?
        //     .map_err(|e| anyhow!(e).context("failed to read lattice data stream"))
        //     .try_filter_map(|key| async {
        //         data.entry(key)
        //             .await
        //             .context("failed to get entry in lattice data bucket")
        //     })
        //     .for_each(|entry| async {
        //         match entry {
        //             Ok(entry) => host.process_entry(entry).await,
        //             Err(err) => error!(%err, "failed to read entry from lattice data bucket"),
        //         }
        //     })
        //     .await;

        host.publish_event("host_started", start_evt)
            .await
            .context("failed to publish start event")?;
        info!(
            host_id = host.host_key.public_key(),
            "wasmCloud host started"
        );

        Ok((Arc::clone(&host), async move {
            ready.store(false, Ordering::Relaxed);
            heartbeat_abort.abort();
            queue_abort.abort();
            data_watch_abort.abort();
            let _ = try_join!(control_handling, data_watch, heartbeat)
                .context("failed to await tasks")?;
            host.publish_event(
                "host_stopped",
                json!({
                    "labels": *host.labels.read().await,
                }),
            )
            .await
            .context("failed to publish stop event")?;
            // Before we exit, make sure to flush all messages or we may lose some that we've
            // thought were sent (like the host_stopped event)
            try_join!(host.ctl_nats.flush(), host.rpc_nats.flush())
                .context("failed to flush NATS clients")?;
            Ok(())
        }))
    }

    /// Waits for host to be stopped via lattice commands and returns the shutdown deadline on
    /// success
    ///
    /// # Errors
    ///
    /// Returns an error if internal stop channel is closed prematurely
    #[instrument(level = "debug", skip_all)]
    pub async fn stopped(&self) -> anyhow::Result<Option<Instant>> {
        self.stop_rx
            .clone()
            .changed()
            .await
            .context("failed to wait for stop")?;
        Ok(*self.stop_rx.borrow())
    }

    #[instrument(level = "debug", skip_all)]
    async fn component_list(&self) -> HostInventory {
        trace!("generating host inventory");
        let components: Vec<_> = {
            let components = self.components.read().await;
            stream::iter(components.iter())
                .filter_map(|(id, component)| async move {
                    let mut description = ComponentDescription::builder()
                        .id(id.into())
                        .image_ref(component.image.to_string())
                        .annotations(component.annotations.clone().into_iter().collect())
                        .max_instances(component.max_instances.get().try_into().unwrap_or(u32::MAX))
                        .revision(
                            component
                                .claims()
                                .and_then(|claims| claims.metadata.as_ref())
                                .and_then(|jwt::Component { rev, .. }| *rev)
                                .unwrap_or_default(),
                        );
                    // Add name if present
                    if let Some(name) = component
                        .claims()
                        .and_then(|claims| claims.metadata.as_ref())
                        .and_then(|metadata| metadata.name.as_ref())
                        .cloned()
                    {
                        description = description.name(name);
                    };

                    Some(
                        description
                            .build()
                            .expect("failed to build component description: {e}"),
                    )
                })
                .collect()
                .await
        };

        let providers: Vec<_> = self
            .providers
            .read()
            .await
            .iter()
            .map(
                |(
                    provider_id,
                    Provider {
                        annotations,
                        claims_token,
                        image_ref,
                        ..
                    },
                )| {
                    let mut provider_description = ProviderDescription::builder()
                        .id(provider_id)
                        .image_ref(image_ref);
                    if let Some(name) = claims_token
                        .as_ref()
                        .and_then(|claims| claims.claims.metadata.as_ref())
                        .and_then(|metadata| metadata.name.as_ref())
                    {
                        provider_description = provider_description.name(name);
                    }
                    provider_description
                        .annotations(
                            annotations
                                .clone()
                                .into_iter()
                                .collect::<BTreeMap<String, String>>(),
                        )
                        .revision(
                            claims_token
                                .as_ref()
                                .and_then(|claims| claims.claims.metadata.as_ref())
                                .and_then(|jwt::CapabilityProvider { rev, .. }| *rev)
                                .unwrap_or_default(),
                        )
                        .build()
                        .expect("failed to build provider description")
                },
            )
            .collect();

        let uptime = self.start_at.elapsed();
        HostInventory::builder()
            .components(components)
            .providers(providers)
            .friendly_name(self.friendly_name.clone())
            .labels(self.labels.read().await.clone())
            .uptime_human(human_friendly_uptime(uptime))
            .uptime_seconds(uptime.as_secs())
            .version(self.host_config.version.clone())
            .host_id(self.host_key.public_key())
            .build()
            .expect("failed to build host inventory")
    }

    #[instrument(level = "debug", skip_all)]
    async fn heartbeat(&self) -> anyhow::Result<serde_json::Value> {
        trace!("generating heartbeat");
        Ok(serde_json::to_value(self.component_list().await)?)
    }

    #[instrument(level = "debug", skip(self))]
    async fn publish_event(&self, name: &str, data: serde_json::Value) -> anyhow::Result<()> {
        event::publish(
            &self.event_builder,
            &self.ctl_nats,
            &self.host_config.lattice,
            name,
            data,
        )
        .await
    }

    /// Instantiate a component
    #[allow(clippy::too_many_arguments)] // TODO: refactor into a config struct
    #[instrument(level = "debug", skip_all)]
    async fn instantiate_component(
        &self,
        annotations: &Annotations,
        image: Arc<String>,
        max_instances: NonZeroUsize,
        mut component: wasmcloud_runtime::Component<Handler>,
        handler: Handler,
    ) -> anyhow::Result<Arc<Component>> {
        trace!(
            component_ref = ?image,
            max_instances,
            "instantiating component"
        );

        let wrpc_id = Arc::clone(&handler.component_id);

        let max_execution_time = self.max_execution_time;
        component.set_max_execution_time(max_execution_time);

        let (events_tx, mut events_rx) = mpsc::channel(
            max_instances
                .get()
                .clamp(MIN_INVOCATION_CHANNEL_SIZE, MAX_INVOCATION_CHANNEL_SIZE),
        );
        let prefix = Arc::from(format!("{}.{wrpc_id}", &self.host_config.lattice));
        let nats = wrpc_transport_nats::Client::new(
            Arc::clone(&self.rpc_nats),
            Arc::clone(&prefix),
            Some(prefix),
        )
        .await?;
        let exports = component
            .serve_wrpc(
                &WrpcServer {
                    nats,
                    claims: component.claims().cloned().map(Arc::new),
                    id: Arc::clone(&wrpc_id),
                    image_reference: Arc::clone(&image),
                    annotations: Arc::new(annotations.clone()),
                    metrics: Arc::clone(&self.metrics),
                },
                handler.clone(),
                events_tx.clone(),
            )
            .await?;
        let permits = Arc::new(Semaphore::new(
            usize::from(max_instances).min(Semaphore::MAX_PERMITS),
        ));
        let metrics = Arc::clone(&self.metrics);
        Ok(Arc::new(Component {
            component,
            wrpc_id,
            handler,
            events: events_tx,
            permits: Arc::clone(&permits),
            exports: spawn(async move {
                join!(
                    async move {
                        let mut exports = stream::select_all(exports);
                        loop {
                            let permits = Arc::clone(&permits);
                            if let Some(fut) = exports.next().await {
                                match fut {
                                    Ok(fut) => {
                                        debug!("accepted invocation, acquiring permit");
                                        let permit = permits.acquire_owned().await;
                                        spawn(async move {
                                            let _permit = permit;
                                            debug!("handling invocation");
                                            match fut.await {
                                                Ok(()) => {
                                                    debug!("successfully handled invocation");
                                                    Ok(())
                                                }
                                                Err(err) => {
                                                    warn!(?err, "failed to handle invocation");
                                                    Err(err)
                                                }
                                            }
                                        });
                                    }
                                    Err(err) => {
                                        warn!(?err, "failed to accept invocation")
                                    }
                                }
                            }
                        }
                    },
                    async move {
                        while let Some(evt) = events_rx.recv().await {
                            match evt {
                                WrpcServeEvent::HttpIncomingHandlerHandleReturned {
                                    context:
                                        InvocationContext {
                                            start_at,
                                            ref attributes,
                                            ..
                                        },
                                    success,
                                }
                                | WrpcServeEvent::MessagingHandlerHandleMessageReturned {
                                    context:
                                        InvocationContext {
                                            start_at,
                                            ref attributes,
                                            ..
                                        },
                                    success,
                                }
                                | WrpcServeEvent::DynamicExportReturned {
                                    context:
                                        InvocationContext {
                                            start_at,
                                            ref attributes,
                                            ..
                                        },
                                    success,
                                } => metrics.record_component_invocation(
                                    u64::try_from(start_at.elapsed().as_nanos())
                                        .unwrap_or_default(),
                                    attributes,
                                    !success,
                                ),
                            }
                        }
                        debug!("serving event stream is done");
                    },
                );
                debug!("export serving task done");
            }),
            annotations: annotations.clone(),
            max_instances,
            image,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(level = "debug", skip_all)]
    async fn start_component<'a>(
        &self,
        wasm: &[u8],
        claims: Option<jwt::Claims<jwt::Component>>,
        image: Arc<String>,
        wrpc_id: Arc<String>,
        max_instances: NonZeroUsize,
        annotations: &Annotations,
        config: BTreeMap<String, String>,
        links: Vec<Link>,
    ) -> anyhow::Result<Arc<Component>> {
        debug!(?image, ?max_instances, "starting new component");

        if let Some(ref claims) = claims {
            self.store_claims(Claims::Component(claims.clone()))
                .await
                .context("failed to store claims")?;
        }

        let lattice = Arc::new(self.host_config.lattice.to_string());

        // Map the imports to pull out the result types of the functions for lookup when invoking them
        let handler = Handler {
            nats: Arc::clone(&self.rpc_nats),
            wasi_config: Arc::new(RwLock::new(config)),
            lattice: Arc::clone(&lattice),
            component_id: Arc::clone(&wrpc_id),
            link_targets: Arc::default(),
            link_instances: Arc::new(RwLock::new(component_import_links(&links))),
            messaging_links: Arc::default(),
            invocation_timeout: Duration::from_secs(10), // TODO: Make this configurable
            experimental_features: self.experimental_features,
            host_labels: Arc::clone(&self.labels),
        };
        let component = wasmcloud_runtime::Component::new(&self.runtime, wasm)?;
        let component = self
            .instantiate_component(
                annotations,
                Arc::clone(&image),
                max_instances,
                component,
                handler,
            )
            .await
            .context("failed to instantiate component")?;

        info!(?image, "component started");

        Ok(component)
    }

    #[instrument(level = "debug", skip_all)]
    async fn stop_component(&self, component: Arc<Component>) -> anyhow::Result<()> {
        trace!(component_id = %component.wrpc_id, "stopping component");

        component.exports.abort();

        Ok(())
    }

    #[instrument(level = "trace", skip_all)]
    async fn fetch_component(&self, component_ref: &str) -> anyhow::Result<Vec<u8>> {
        let registry_config = self.registry_config.read().await;
        fetch_component(
            component_ref,
            self.host_config.allow_file_load,
            &self.host_config.oci_opts.additional_ca_paths,
            &registry_config,
        )
        .await
        .context("failed to fetch component")
    }

    #[instrument(level = "trace", skip_all)]
    async fn store_component_claims(
        &self,
        claims: jwt::Claims<jwt::Component>,
    ) -> anyhow::Result<()> {
        let mut component_claims = self.component_claims.write().await;
        component_claims.insert(claims.subject.clone(), claims);
        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_stop_host(&self, _payload: impl AsRef<[u8]>) -> anyhow::Result<Vec<u8>> {
        let timeout: Option<u64> = Some(3000);

        info!(?timeout, "handling stop host");

        self.ready.store(false, Ordering::Relaxed);

        self.heartbeat.abort();
        self.data_watch.abort();
        self.queue.abort();
        let deadline =
            timeout.and_then(|timeout| Instant::now().checked_add(Duration::from_millis(timeout)));
        self.stop_tx.send_replace(deadline);

        api::Response::<()>::success(()).serialize()
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_stop_component(
        self: Arc<Self>,
        payload: impl AsRef<[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        let request: api::Request<api::StopComponentRequest> = payload.as_ref().try_into()?;

        let component_id = request.payload.id;
        let mut tracker = self.tracker.write().await;
        match tracker.remove(&component_id) {
            Some(TrackerEntry::Component(item)) => {
                self.stop_component(item.clone()).await?;
                api::Response::success_with_message(
                    "stopped".to_string(),
                    api::StopComponentResponse {},
                )
                .serialize()
            }
            _ => api::Response::success_with_message(
                "not found".to_string(),
                api::StopComponentResponse {},
            )
            .serialize(),
        }
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_start_component(
        self: Arc<Self>,
        payload: impl AsRef<[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        let request: api::Request<api::StartComponentRequest> = payload.as_ref().try_into()?;

        let component_id = uuid::Uuid::new_v4();
        let component_ref = request.payload.image;
        let max_instances = match NonZeroUsize::new(request.payload.concurrency as usize) {
            Some(max_instances) => max_instances,
            None => return Err(anyhow::anyhow!("concurrency must be greater than 0")),
        };
        let wasi_config = request.payload.wasi_config.unwrap_or_default();
        let annotations: Annotations = request
            .payload
            .annotations
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();

        // Fetch the component from the reference
        let component_bytes = self.fetch_component(&component_ref).await?;
        // Pull the claims token from the component, this returns an error only if claims are embedded
        // and they are invalid (expired, tampered with, etc)
        let claims_token = wasmcloud_runtime::component::claims_token(&component_bytes)?;

        let claims = claims_token.map(|c| c.claims.clone());
        let links = Vec::<Link>::default();

        let component = self
            .start_component(
                component_bytes.as_ref(),
                claims.clone(),
                Arc::new(component_ref.to_string()),
                Arc::new(component_id.to_string()),
                max_instances,
                &annotations,
                wasi_config,
                links,
            )
            .await?;

        let mut tracker = self.tracker.write().await;
        tracker.insert(
            component_id.to_string(),
            TrackerEntry::Component(Arc::clone(&component)),
        );

        let response = api::Response::success_with_message(
            "successfully started component".to_string(),
            api::StartComponentResponse {
                id: component_id.to_string(),
            },
        );

        response.serialize()
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_start_provider(
        self: Arc<Self>,
        payload: impl AsRef<[u8]>,
    ) -> anyhow::Result<Vec<u8>> {
        let request: api::Request<api::StartProviderRequest> = payload.as_ref().try_into()?;

        // Avoid responding to start providers for builtin providers if they're not enabled
        if let Ok(ResourceRef::Builtin(name)) =
            ResourceRef::try_from(request.payload.image.as_str())
        {
            if !self.experimental_features.builtin_http_server && name == "http-server" {
                return api::Response::error_with_message(
                    "builtin http-server provider is not enabled".to_string(),
                    (),
                )
                .serialize();
            }
            if !self.experimental_features.builtin_messaging_nats && name == "messaging-nats" {
                return api::Response::error_with_message(
                    "builtin messaging-nats provider is not enabled".to_string(),
                    (),
                )
                .serialize();
            }
        }

        // NOTE: We log at info since starting providers can take a while
        info!(
            provider_ref = request.payload.image,
            provider_id = request.payload.name,
            "handling start provider"
        );

        let host_key = self.host_key.public_key();
        let host_id = host_key.to_string();
        spawn(async move {
            // let config = request.config();
            // let provider_id = request.provider_id();
            // let provider_ref = request.provider_ref();
            // let annotations = request.annotations();

            // if let Err(err) = Arc::clone(&self)
            //     .handle_start_provider_task(
            //         config,
            //         provider_id,
            //         provider_ref,
            //         annotations.cloned().unwrap_or_default(),
            //     )
            //     .await
            // {
            //     error!(provider_ref, provider_id, ?err, "failed to start provider");
            //     if let Err(err) = self
            //         .publish_event(
            //             "provider_start_failed",
            //             event::provider_start_failed(provider_ref, provider_id, host_id, &err),
            //         )
            //         .await
            //     {
            //         error!(?err, "failed to publish provider_start_failed event");
            //     }
            // }
        });

        api::Response::success_with_message(
            "successfully started provider".to_string(),
            api::StartProviderResponse { id: "".to_string() },
        )
        .serialize()
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_start_provider_task(
        self: Arc<Self>,
        config_names: &[String],
        provider_id: &str,
        image: &str,
        annotations: BTreeMap<String, String>,
    ) -> anyhow::Result<()> {
        trace!(image, provider_id, "start provider task");

        // NOTE(lxf): refactor
        // This host id is only needed for caching
        let host_id = self.host_key.public_key().to_string();

        let registry_config = self.registry_config.read().await;
        let provider_ref =
            ResourceRef::try_from(image).context("failed to parse provider reference")?;
        let (path, claims_token) = match &provider_ref {
            ResourceRef::Builtin(..) => (None, None),
            _ => {
                let (path, claims_token) = crate::fetch_provider(
                    &provider_ref,
                    host_id,
                    self.host_config.allow_file_load,
                    &self.host_config.oci_opts.additional_ca_paths,
                    &registry_config,
                )
                .await
                .context("failed to fetch provider")?;
                (Some(path), claims_token)
            }
        };
        let claims = claims_token.as_ref().map(|t| t.claims.clone());

        if let Some(claims) = claims.clone() {
            self.store_claims(Claims::Provider(claims))
                .await
                .context("failed to store claims")?;
        }

        let annotations: Annotations = annotations.into_iter().collect();

        let mut providers = self.providers.write().await;
        if let hash_map::Entry::Vacant(entry) = providers.entry(provider_id.into()) {
            let provider_xkey = XKey::new();
            // We only need to store the public key of the provider xkey, as the private key is only needed by the provider
            let xkey = XKey::from_public_key(&provider_xkey.public_key())
                .context("failed to create XKey from provider public key xkey")?;
            // Generate the HostData and ConfigBundle for the provider
            let (host_data, config_bundle) = self
                .prepare_provider_config(
                    config_names,
                    claims_token.as_ref(),
                    provider_id,
                    &provider_xkey,
                    &annotations,
                )
                .await?;
            let config_bundle = Arc::new(RwLock::new(config_bundle));
            // Used by provider child tasks (health check, config watch, process restarter) to
            // know when to shutdown.
            let shutdown = Arc::new(AtomicBool::new(false));
            let tasks = match (path, &provider_ref) {
                (Some(path), ..) => {
                    Arc::clone(&self)
                        .start_binary_provider(
                            path,
                            host_data,
                            Arc::clone(&config_bundle),
                            provider_xkey,
                            provider_id,
                            // Arguments to allow regenerating configuration later
                            config_names.to_vec(),
                            claims_token.clone(),
                            annotations.clone(),
                            shutdown.clone(),
                        )
                        .await?
                }
                (None, ResourceRef::Builtin(name)) => match *name {
                    "http-server" if self.experimental_features.builtin_http_server => {
                        self.start_http_server_provider(host_data, provider_xkey, provider_id)
                            .await?
                    }
                    "http-server" => {
                        bail!("feature `builtin-http-server` is not enabled, denying start")
                    }
                    "messaging-nats" if self.experimental_features.builtin_messaging_nats => {
                        self.start_messaging_nats_provider(host_data, provider_xkey, provider_id)
                            .await?
                    }
                    "messaging-nats" => {
                        bail!("feature `builtin-messaging-nats` is not enabled, denying start")
                    }
                    _ => bail!("unknown builtin name: {name}"),
                },
                _ => bail!("invalid provider reference"),
            };

            info!(
                provider_ref = provider_ref.as_ref(),
                provider_id, "provider started"
            );

            // Add the provider
            entry.insert(Provider {
                tasks,
                annotations,
                claims_token,
                image_ref: provider_ref.as_ref().to_string(),
                xkey,
                shutdown,
            });
        } else {
            bail!("provider is already running with that ID")
        }

        Ok(())
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_stop_provider(
        &self,
        payload: impl AsRef<[u8]>,
    ) -> anyhow::Result<CtlResponse<()>> {
        let request = serde_json::from_slice::<StopProviderCommand>(payload.as_ref())
            .context("failed to deserialize provider stop command")?;

        let provider_id = request.provider_id();
        let host_id = request.host_id();

        debug!(provider_id, "handling stop provider");

        let mut providers = self.providers.write().await;
        let hash_map::Entry::Occupied(entry) = providers.entry(provider_id.into()) else {
            warn!(
                provider_id,
                "received request to stop provider that is not running"
            );
            return Ok(CtlResponse::error("provider with that ID is not running"));
        };
        let Provider {
            ref annotations,
            mut tasks,
            shutdown,
            ..
        } = entry.remove();

        // Set the shutdown flag to true to stop health checks and config updates. Also
        // prevents restarting the provider but does not stop the provider process.
        shutdown.store(true, Ordering::Relaxed);

        // Send a request to the provider, requesting a graceful shutdown
        let req = serde_json::to_vec(&json!({ "host_id": host_id }))
            .context("failed to encode provider stop request")?;
        let req = async_nats::Request::new()
            .payload(req.into())
            .timeout(self.host_config.provider_shutdown_delay)
            .headers(injector_to_headers(
                &TraceContextInjector::default_with_span(),
            ));
        if let Err(e) = self
            .rpc_nats
            .send_request(
                format!(
                    "wasmbus.rpc.{}.{provider_id}.default.shutdown",
                    self.host_config.lattice
                ),
                req,
            )
            .await
        {
            warn!(
                ?e,
                provider_id,
                "provider did not gracefully shut down in time, shutting down forcefully"
            );
            // NOTE: The provider child process is spawned with [tokio::process::Command::kill_on_drop],
            // so dropping the task will send a SIGKILL to the provider process.
        }

        // Stop the provider and health check / config changes tasks
        tasks.abort_all();

        info!(provider_id, "provider stopped");
        self.publish_event(
            "provider_stopped",
            event::provider_stopped(annotations, host_id, provider_id, "stop"),
        )
        .await?;
        Ok(CtlResponse::<()>::success(
            "successfully stopped provider".into(),
        ))
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_component_list(&self) -> anyhow::Result<Vec<u8>> {
        let inventory = self.component_list().await;
        api::Response::<HostInventory>::success(inventory).serialize()
    }

    #[instrument(level = "debug", skip_all)]
    async fn handle_ping(&self) -> anyhow::Result<Vec<u8>> {
        trace!("replying to ping");
        let uptime = self.start_at.elapsed();

        let mut host = wasmcloud_control_interface::Host::builder()
            .id(self.host_key.public_key())
            .labels(self.labels.read().await.clone())
            .friendly_name(self.friendly_name.clone())
            .uptime_seconds(uptime.as_secs())
            .uptime_human(human_friendly_uptime(uptime))
            .version(self.host_config.version.clone())
            .ctl_host(self.host_config.ctl_nats_url.to_string())
            .rpc_host(self.host_config.rpc_nats_url.to_string())
            .lattice(self.host_config.lattice.to_string());

        if let Some(ref js_domain) = self.host_config.js_domain {
            host = host.js_domain(js_domain.clone());
        }

        let host = host
            .build()
            .map_err(|e| anyhow!("failed to build host message: {e}"))?;

        api::Response::<()>::success(()).serialize()
    }

    #[instrument(level = "trace", skip_all, fields(subject = %message.subject))]
    async fn handle_ctl_message(self: Arc<Self>, message: async_nats::Message) {
        // NOTE: if log level is not `trace`, this won't have an effect, since the current span is
        // disabled. In most cases that's fine, since we aren't aware of any control interface
        // requests including a trace context
        opentelemetry_nats::attach_span_context(&message);
        let reply_to = match message.reply {
            Some(reply_to) => reply_to,
            None => return,
        };
        // Skip the topic prefix, the version, and the lattice
        // e.g. `wasmbus.ctl.v1.{prefix}`
        let subject = message.subject;
        let host_key = self.host_key.public_key();
        let mut parts = subject
            .trim()
            .trim_start_matches(&self.ctl_topic_prefix)
            .trim_start_matches('.')
            .trim_start_matches(host_key.as_str())
            .split('.')
            .skip(1);
        trace!(%subject, "handling control interface request");
        println!("parts: {:?}", parts.clone().collect::<Vec<_>>());

        // This response is a wrapped Result<Option<Result<Vec<u8>>>> for a good reason.
        // The outer Result is for reporting protocol errors in handling the request, e.g. failing to
        //    deserialize the request payload.
        // The Option is for the case where the request is handled successfully, but the handler
        //    doesn't want to send a response back to the client, like with an auction.
        // The inner Result is purely for the success or failure of serializing the [CtlResponse], which
        //    should never fail but it's a result we must handle.
        // And finally, the Vec<u8> is the serialized [CtlResponse] that we'll send back to the client
        let ctl_response = match (parts.next(), parts.next(), parts.next()) {
            // Component commands
            (Some("start"), Some("component"), None) => {
                Arc::clone(&self)
                    .handle_start_component(message.payload)
                    .await
            }
            (Some("stop"), Some("component"), None) => {
                Arc::clone(&self)
                    .handle_stop_component(message.payload)
                    .await
            }
            // Provider commands
            (Some("start"), Some("provider"), None) => {
                Arc::clone(&self)
                    .handle_start_provider(message.payload)
                    .await
            }
            // (Some("provider"), Some("stop"), None) => self
            //     .handle_stop_provider(message.payload)
            //     .await
            //     .map(Some)
            //     .map(serialize_ctl_response),
            // // Host commands
            (Some("list"), Some("component"), None) => self.handle_component_list().await,
            (Some("ping"), None, None) => self.handle_ping().await,
            (Some("shutdown"), None, None) => self.handle_stop_host(message.payload).await,
            _ => {
                warn!(%subject, "received control interface request on unsupported subject");
                api::Response::<()>::error_with_message(
                    "unsupported control interface request".to_string(),
                    (),
                )
                .serialize()
            }
        };

        match ctl_response {
            Ok(payload) => {
                let headers = injector_to_headers(&TraceContextInjector::default_with_span());
                if let Err(err) = self
                    .ctl_nats
                    .publish_with_headers(reply_to, headers, payload.into())
                    .await
                {
                    error!(%subject, ?err, "failed to send control interface response");
                }
            }
            Err(err) => {
                error!(%subject, ?err, "failed to handle control interface request");
            }
        }
    }

    // TODO: Remove this before wasmCloud 1.2 is released. This is a backwards-compatible
    // provider link definition put that is published to the provider's id, which is what
    // providers built for wasmCloud 1.0 expected.
    //
    // Thankfully, in a lattice where there are no "older" providers running, these publishes
    // will return immediately as there will be no subscribers on those topics.
    async fn put_backwards_compat_provider_link(&self, link: &Link) -> anyhow::Result<()> {
        // Only attempt to publish the backwards-compatible provider link definition if the link
        // does not contain any secret values.
        let source_config_contains_secret = link
            .source_config()
            .iter()
            .any(|c| c.starts_with(SECRET_PREFIX));
        let target_config_contains_secret = link
            .target_config()
            .iter()
            .any(|c| c.starts_with(SECRET_PREFIX));
        if source_config_contains_secret || target_config_contains_secret {
            debug!("link contains secrets and is not backwards compatible, skipping");
            return Ok(());
        }
        let provider_link = self
            .resolve_link_config(link.clone(), None, None, &XKey::new())
            .await
            .context("failed to resolve link config")?;
        let lattice = &self.host_config.lattice;
        let payload: Bytes = serde_json::to_vec(&provider_link)
            .context("failed to serialize provider link definition")?
            .into();

        if let Err(e) = self
            .rpc_nats
            .publish_with_headers(
                format!("wasmbus.rpc.{lattice}.{}.linkdefs.put", link.source_id()),
                injector_to_headers(&TraceContextInjector::default_with_span()),
                payload.clone(),
            )
            .await
        {
            warn!(
                ?e,
                "failed to publish backwards-compatible provider link to source"
            );
        }

        if let Err(e) = self
            .rpc_nats
            .publish_with_headers(
                format!("wasmbus.rpc.{lattice}.{}.linkdefs.put", link.target()),
                injector_to_headers(&TraceContextInjector::default_with_span()),
                payload,
            )
            .await
        {
            warn!(
                ?e,
                "failed to publish backwards-compatible provider link to target"
            );
        }

        Ok(())
    }

    /// Publishes a link to a provider running on this host to handle.
    #[instrument(level = "debug", skip_all)]
    async fn put_provider_link(&self, provider: &Provider, link: &Link) -> anyhow::Result<()> {
        let provider_link = self
            .resolve_link_config(
                link.clone(),
                provider.claims_token.as_ref().map(|t| &t.jwt),
                provider.annotations.get("wasmcloud.dev/appspec"),
                &provider.xkey,
            )
            .await
            .context("failed to resolve link config and secrets")?;
        let lattice = &self.host_config.lattice;
        let payload: Bytes = serde_json::to_vec(&provider_link)
            .context("failed to serialize provider link definition")?
            .into();

        self.rpc_nats
            .publish_with_headers(
                format!(
                    "wasmbus.rpc.{lattice}.{}.linkdefs.put",
                    provider.xkey.public_key()
                ),
                injector_to_headers(&TraceContextInjector::default_with_span()),
                payload.clone(),
            )
            .await
            .context("failed to publish provider link definition put")
    }

    /// Publishes a delete link to the lattice for all instances of a provider to handle
    /// Right now this is publishing _both_ to the source and the target in order to
    /// ensure that the provider is aware of the link delete. This would cause problems if a provider
    /// is linked to a provider (which it should never be.)
    #[instrument(level = "debug", skip(self))]
    async fn del_provider_link(&self, link: &Link) -> anyhow::Result<()> {
        let lattice = &self.host_config.lattice;
        // The provider expects the [`wasmcloud_core::InterfaceLinkDefinition`]
        let link = wasmcloud_core::InterfaceLinkDefinition {
            source_id: link.source_id().to_string(),
            target: link.target().to_string(),
            wit_namespace: link.wit_namespace().to_string(),
            wit_package: link.wit_package().to_string(),
            name: link.name().to_string(),
            interfaces: link.interfaces().clone(),
            // Configuration isn't needed for deletion
            ..Default::default()
        };
        let source_id = &link.source_id;
        let target = &link.target;
        let payload: Bytes = serde_json::to_vec(&link)
            .context("failed to serialize provider link definition for deletion")?
            .into();

        let (source_result, target_result) = futures::future::join(
            self.rpc_nats.publish_with_headers(
                format!("wasmbus.rpc.{lattice}.{source_id}.linkdefs.del"),
                injector_to_headers(&TraceContextInjector::default_with_span()),
                payload.clone(),
            ),
            self.rpc_nats.publish_with_headers(
                format!("wasmbus.rpc.{lattice}.{target}.linkdefs.del"),
                injector_to_headers(&TraceContextInjector::default_with_span()),
                payload,
            ),
        )
        .await;

        source_result
            .and(target_result)
            .context("failed to publish provider link definition delete")
    }

    async fn fetch_config_and_secrets(
        &self,
        config_names: &[String],
        entity_jwt: Option<&String>,
        application: Option<&String>,
    ) -> anyhow::Result<(ConfigBundle, HashMap<String, SecretBox<SecretValue>>)> {
        // let (secret_names, config_names) = config_names
        //     .iter()
        //     .map(|s| s.to_string())
        //     .partition(|name| name.starts_with(SECRET_PREFIX));

        todo!()
    }

    /// Validates that the provided configuration names exist in the store and are valid.
    ///
    /// For any configuration that starts with `SECRET_`, the configuration is expected to be a secret reference.
    /// For any other configuration, the configuration is expected to be a [`HashMap<String, String>`].
    async fn validate_config<I>(&self, config_names: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item: AsRef<str>>,
    {
        Ok(())
    }

    /// Transform a [`wasmcloud_control_interface::Link`] into a [`wasmcloud_core::InterfaceLinkDefinition`]
    /// by fetching the source and target configurations and secrets, and encrypting the secrets.
    async fn resolve_link_config(
        &self,
        link: Link,
        provider_jwt: Option<&String>,
        application: Option<&String>,
        provider_xkey: &XKey,
    ) -> anyhow::Result<wasmcloud_core::InterfaceLinkDefinition> {
        let (source_bundle, raw_source_secrets) = self
            .fetch_config_and_secrets(link.source_config().as_slice(), provider_jwt, application)
            .await?;
        let (target_bundle, raw_target_secrets) = self
            .fetch_config_and_secrets(link.target_config().as_slice(), provider_jwt, application)
            .await?;

        let source_config = source_bundle.get_config().await;
        let target_config = target_bundle.get_config().await;
        // NOTE(brooksmtownsend): This trait import is used here to ensure we're only exposing secret
        // values when we need them.
        use secrecy::ExposeSecret;
        let source_secrets_map: HashMap<String, wasmcloud_core::secrets::SecretValue> =
            raw_source_secrets
                .iter()
                .map(|(k, v)| match v.expose_secret() {
                    SecretValue::String(s) => (
                        k.clone(),
                        wasmcloud_core::secrets::SecretValue::String(s.to_owned()),
                    ),
                    SecretValue::Bytes(b) => (
                        k.clone(),
                        wasmcloud_core::secrets::SecretValue::Bytes(b.to_owned()),
                    ),
                })
                .collect();
        let target_secrets_map: HashMap<String, wasmcloud_core::secrets::SecretValue> =
            raw_target_secrets
                .iter()
                .map(|(k, v)| match v.expose_secret() {
                    SecretValue::String(s) => (
                        k.clone(),
                        wasmcloud_core::secrets::SecretValue::String(s.to_owned()),
                    ),
                    SecretValue::Bytes(b) => (
                        k.clone(),
                        wasmcloud_core::secrets::SecretValue::Bytes(b.to_owned()),
                    ),
                })
                .collect();
        // Serializing & sealing an empty map results in a non-empty Vec, which is difficult to tell the
        // difference between an empty map and an encrypted empty map. To avoid this, we explicitly handle
        // the case where the map is empty.
        // NOTE(lxf): refactor
        let source_secrets = None;
        let target_secrets = None;

        Ok(wasmcloud_core::InterfaceLinkDefinition {
            source_id: link.source_id().to_string(),
            target: link.target().to_string(),
            name: link.name().to_string(),
            wit_namespace: link.wit_namespace().to_string(),
            wit_package: link.wit_package().to_string(),
            interfaces: link.interfaces().clone(),
            source_config: source_config.clone(),
            target_config: target_config.clone(),
            source_secrets,
            target_secrets,
        })
    }
}

/// Helper function to transform a Vec of [`Link`]s into the structure components expect to be able
/// to quickly look up the desired target for a given interface
///
/// # Arguments
/// - links: A Vec of [`Link`]s
///
/// # Returns
/// - A `HashMap` in the form of `link_name` -> `instance` -> target
fn component_import_links(links: &[Link]) -> HashMap<Box<str>, HashMap<Box<str>, Box<str>>> {
    let mut m = HashMap::new();
    for link in links {
        let instances: &mut HashMap<Box<str>, Box<str>> = m
            .entry(link.name().to_string().into_boxed_str())
            .or_default();
        for interface in link.interfaces() {
            instances.insert(
                format!(
                    "{}:{}/{interface}",
                    link.wit_namespace(),
                    link.wit_package(),
                )
                .into_boxed_str(),
                link.target().to_string().into_boxed_str(),
            );
        }
    }
    m
}

/// Helper function to serialize `CtlResponse`<T> into a Vec<u8> if the response is Some
fn serialize_ctl_response<T: Serialize>(
    ctl_response: Option<CtlResponse<T>>,
) -> Option<anyhow::Result<Vec<u8>>> {
    ctl_response.map(|resp| serde_json::to_vec(&resp).map_err(anyhow::Error::from))
}

fn human_friendly_uptime(uptime: Duration) -> String {
    // strip sub-seconds, then convert to human-friendly format
    humantime::format_duration(
        uptime.saturating_sub(Duration::from_nanos(uptime.subsec_nanos().into())),
    )
    .to_string()
}

fn injector_to_headers(injector: &TraceContextInjector) -> async_nats::header::HeaderMap {
    injector
        .iter()
        .filter_map(|(k, v)| {
            // There's not really anything we can do about headers that don't parse
            let name = async_nats::header::HeaderName::from_str(k.as_str()).ok()?;
            let value = async_nats::header::HeaderValue::from_str(v.as_str()).ok()?;
            Some((name, value))
        })
        .collect()
}

#[cfg(test)]
mod test {
    // Ensure that the helper function to translate a list of links into a map of imports works as expected
    #[test]
    fn can_compute_component_links() {
        use std::collections::HashMap;
        use wasmcloud_control_interface::Link;

        let links = vec![
            Link::builder()
                .source_id("source_component")
                .target("kv-redis")
                .wit_namespace("wasi")
                .wit_package("keyvalue")
                .interfaces(vec!["atomics".into(), "store".into()])
                .name("default")
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("source_component")
                .target("kv-vault")
                .wit_namespace("wasi")
                .wit_package("keyvalue")
                .interfaces(vec!["atomics".into(), "store".into()])
                .name("secret")
                .source_config(vec![])
                .target_config(vec!["my-secret".into()])
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("source_component")
                .target("kv-vault-offsite")
                .wit_namespace("wasi")
                .wit_package("keyvalue")
                .interfaces(vec!["atomics".into()])
                .name("secret")
                .source_config(vec![])
                .target_config(vec!["my-secret".into()])
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("http")
                .target("source_component")
                .wit_namespace("wasi")
                .wit_package("http")
                .interfaces(vec!["incoming-handler".into()])
                .name("default")
                .source_config(vec!["some-port".into()])
                .target_config(vec![])
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("source_component")
                .target("httpclient")
                .wit_namespace("wasi")
                .wit_package("http")
                .interfaces(vec!["outgoing-handler".into()])
                .name("default")
                .source_config(vec![])
                .target_config(vec!["some-port".into()])
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("source_component")
                .target("other_component")
                .wit_namespace("custom")
                .wit_package("foo")
                .interfaces(vec!["bar".into(), "baz".into()])
                .name("default")
                .source_config(vec![])
                .target_config(vec![])
                .build()
                .expect("failed to build link"),
            Link::builder()
                .source_id("other_component")
                .target("target")
                .wit_namespace("wit")
                .wit_package("package")
                .interfaces(vec!["interface3".into()])
                .name("link2")
                .source_config(vec![])
                .target_config(vec![])
                .build()
                .expect("failed to build link"),
        ];

        let links_map = super::component_import_links(&links);

        // Expected structure:
        // {
        //     "default": {
        //         "wasi:keyvalue": {
        //             "atomics": "kv-redis",
        //             "store": "kv-redis"
        //         },
        //         "wasi:http": {
        //             "incoming-handler": "source_component"
        //         },
        //         "custom:foo": {
        //             "bar": "other_component",
        //             "baz": "other_component"
        //         }
        //     },
        //     "secret": {
        //         "wasi:keyvalue": {
        //             "atomics": "kv-vault-offsite",
        //             "store": "kv-vault"
        //         }
        //     },
        //     "link2": {
        //         "wit:package": {
        //             "interface3": "target"
        //         }
        //     }
        // }
        let expected_result = HashMap::from_iter([
            (
                "default".into(),
                HashMap::from([
                    ("wasi:keyvalue/atomics".into(), "kv-redis".into()),
                    ("wasi:keyvalue/store".into(), "kv-redis".into()),
                    (
                        "wasi:http/incoming-handler".into(),
                        "source_component".into(),
                    ),
                    ("wasi:http/outgoing-handler".into(), "httpclient".into()),
                    ("custom:foo/bar".into(), "other_component".into()),
                    ("custom:foo/baz".into(), "other_component".into()),
                ]),
            ),
            (
                "secret".into(),
                HashMap::from([
                    ("wasi:keyvalue/atomics".into(), "kv-vault-offsite".into()),
                    ("wasi:keyvalue/store".into(), "kv-vault".into()),
                ]),
            ),
            (
                "link2".into(),
                HashMap::from([("wit:package/interface3".into(), "target".into())]),
            ),
        ]);

        assert_eq!(links_map, expected_result);
    }
}
