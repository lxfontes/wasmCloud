use core::str::FromStr as _;
use core::time::Duration;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use anyhow::Context as _;
use axum::extract;
use axum::handler::Handler as _;
use axum_server::tls_rustls::RustlsConfig;
use futures::StreamExt as _;
use tokio::{spawn, time};
use tower_http::cors::{self, CorsLayer};
use tracing::{debug, error, info, instrument, trace};
use wasmcloud_provider_sdk::{
    get_connection, initialize_observability, load_host_data, run_provider, HostData, LinkConfig,
    Provider,
};
use wrpc_interface_http::InvokeIncomingHandler as _;

mod hashmap_ci;
pub(crate) use hashmap_ci::make_case_insensitive;

mod settings;
pub use settings::{
    load_link, load_settings, ServiceSettings, CONTENT_LEN_LIMIT, DEFAULT_MAX_CONTENT_LEN,
};

use crate::settings::Tls;

pub async fn run() -> anyhow::Result<()> {
    initialize_observability!(
        "http-shared-server-provider",
        std::env::var_os("PROVIDER_HTTP_SERVER_FLAMEGRAPH_PATH")
    );
    let host_data = load_host_data().context("failed to load host data")?;

    let server = HttpServerProvider::new(&host_data).await?;

    let shutdown = run_provider(server, "http-shared-server-provider")
        .await
        .context("failed to run provider")?;
    shutdown.await;
    Ok(())
}

#[instrument(level = "debug", skip(router))]
async fn handle_request(
    extract::State(RequestContext {
        router,
        settings,
        scheme,
    }): extract::State<RequestContext>,
    extract::Host(authority): extract::Host,
    request: extract::Request,
) -> axum::response::Result<axum::response::Response> {
    let timeout = settings.timeout_ms.map(Duration::from_millis);
    let method = request.method();
    if let Some(readonly_mode) = settings.readonly_mode {
        if readonly_mode
            && method != http::method::Method::GET
            && method != http::method::Method::HEAD
        {
            debug!("only GET and HEAD allowed in read-only mode");
            Err((
                http::StatusCode::METHOD_NOT_ALLOWED,
                "only GET and HEAD allowed in read-only mode",
            ))?;
        }
    }
    let (
        http::request::Parts {
            method,
            uri,
            headers,
            ..
        },
        body,
    ) = request.into_parts();
    let http::uri::Parts { path_and_query, .. } = uri.into_parts();
    let mut uri = http::Uri::builder().scheme(scheme);
    if authority.is_empty() {
        debug!("request missing hostname");
        Err((http::StatusCode::BAD_GATEWAY, "missing hostname"))?;
    }

    uri = uri.authority(authority.clone());

    if let Some(path_and_query) = path_and_query {
        uri = uri.path_and_query(path_and_query);
    }
    let uri = uri
        .build()
        .map_err(|err| (http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let mut req = http::Request::builder();
    *req.headers_mut().ok_or((
        http::StatusCode::INTERNAL_SERVER_ERROR,
        "invalid request generated",
    ))? = headers;
    let req = req
        .uri(uri)
        .method(method)
        .body(body)
        .map_err(|err| (http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;

    // Create a new wRPC client with all headers from the current span injected
    let mut cx = async_nats::HeaderMap::new();
    for (k, v) in
        wasmcloud_provider_sdk::wasmcloud_tracing::context::TraceContextInjector::default_with_span(
        )
        .iter()
    {
        cx.insert(k.as_str(), v.as_str())
    }

    let target = router.find_link(authority).map_err(|err| {
        error!(?err, "failed to find component for hostname");
        (http::StatusCode::BAD_GATEWAY, format!("{err:#}"))
    })?;

    let wrpc = get_connection().get_wrpc_client_custom(target.as_str(), None);
    trace!(?req, "httpserver calling component");
    let fut = wrpc.invoke_handle_http(Some(cx), req);
    let res = if let Some(timeout) = timeout {
        let Ok(res) = time::timeout(timeout, fut).await else {
            Err(http::StatusCode::REQUEST_TIMEOUT)?
        };
        res
    } else {
        fut.await
    };
    let (res, mut errs, io) =
        res.map_err(|err| (http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    if let Some(io) = io {
        spawn(async move {
            if let Err(err) = io.await {
                error!(?err, "failed to complete async I/O");
            }
        });
    }
    spawn(async move {
        while let Some(err) = errs.next().await {
            error!(?err, "body error encountered");
        }
    });
    // TODO: Convert this to http status code
    let mut res =
        res.map_err(|err| (http::StatusCode::INTERNAL_SERVER_ERROR, format!("{err:?}")))?;
    if let Some(cache_control) = settings.cache_control.as_ref() {
        let cache_control = http::HeaderValue::from_str(cache_control)
            .map_err(|err| (http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
        res.headers_mut().append("Cache-Control", cache_control);
    };
    Ok(res.map(axum::body::Body::new))
}

#[derive(Clone, Debug)]
struct RequestContext {
    settings: ServiceSettings,
    router: Arc<HttpRouter>,
    scheme: http::uri::Scheme,
}

#[derive(Clone, Debug, Default)]
pub struct HttpRouter {
    pub hostnames_to_links: Arc<dashmap::DashMap<String, String>>,
    pub links_to_hostnames: Arc<dashmap::DashMap<String, String>>,
}

impl HttpRouter {
    fn find_link(&self, hostname: String) -> anyhow::Result<String> {
        if let Some(component) = self.hostnames_to_links.get(&hostname) {
            return Ok(component.value().clone());
        }

        Err(anyhow::anyhow!("hostname not found: {}", hostname))
    }
}

/// `wrpc:http/incoming-handler` provider implementation.
#[derive(Debug)]
pub struct HttpServerProvider {
    handle: axum_server::Handle,
    task: tokio::task::JoinHandle<()>,
    router: Arc<HttpRouter>,
}

impl HttpServerProvider {
    async fn new(host_data: &HostData) -> anyhow::Result<Self> {
        let settings = match load_settings(&host_data.config)
            .context("httpserver failed to load provider settings")
        {
            Ok(settings) => settings,
            Err(e) => {
                error!(
                    config = ?host_data.config,
                    "httpserver failed to provider settings: {}", e.to_string()
                );
                return Err(e);
            }
        };

        let addr = settings
            .address
            .unwrap_or_else(|| (Ipv4Addr::UNSPECIFIED, 8000).into());
        info!(
            %addr,
            "httpserver starting listener",
        );

        let allow_origin = settings.cors.allowed_origins.as_ref();
        let allow_origin: Vec<_> = allow_origin
            .map(|origins| {
                origins
                    .iter()
                    .map(AsRef::as_ref)
                    .map(http::HeaderValue::from_str)
                    .collect::<Result<_, _>>()
                    .context("failed to parse allowed origins")
            })
            .transpose()?
            .unwrap_or_default();
        let allow_origin = if allow_origin.is_empty() {
            cors::AllowOrigin::any()
        } else {
            cors::AllowOrigin::list(allow_origin)
        };
        let allow_headers = settings.cors.allowed_headers.as_ref();
        let allow_headers: Vec<_> = allow_headers
            .map(|headers| {
                headers
                    .iter()
                    .map(AsRef::as_ref)
                    .map(http::HeaderName::from_str)
                    .collect::<Result<_, _>>()
                    .context("failed to parse allowed header names")
            })
            .transpose()?
            .unwrap_or_default();
        let allow_headers = if allow_headers.is_empty() {
            cors::AllowHeaders::any()
        } else {
            cors::AllowHeaders::list(allow_headers)
        };
        let allow_methods = settings.cors.allowed_methods.as_ref();
        let allow_methods: Vec<_> = allow_methods
            .map(|methods| {
                methods
                    .iter()
                    .map(AsRef::as_ref)
                    .map(http::Method::from_str)
                    .collect::<Result<_, _>>()
                    .context("failed to parse allowed methods")
            })
            .transpose()?
            .unwrap_or_default();
        let allow_methods = if allow_methods.is_empty() {
            cors::AllowMethods::any()
        } else {
            cors::AllowMethods::list(allow_methods)
        };
        let expose_headers = settings.cors.exposed_headers.as_ref();
        let expose_headers: Vec<_> = expose_headers
            .map(|headers| {
                headers
                    .iter()
                    .map(AsRef::as_ref)
                    .map(http::HeaderName::from_str)
                    .collect::<Result<_, _>>()
                    .context("failed to parse exposeed header names")
            })
            .transpose()?
            .unwrap_or_default();
        let expose_headers = if expose_headers.is_empty() {
            cors::ExposeHeaders::any()
        } else {
            cors::ExposeHeaders::list(expose_headers)
        };
        let mut cors = CorsLayer::new()
            .allow_origin(allow_origin)
            .allow_headers(allow_headers)
            .allow_methods(allow_methods)
            .expose_headers(expose_headers);
        if let Some(max_age) = settings.cors.max_age_secs {
            cors = cors.max_age(Duration::from_secs(max_age));
        }
        let service = handle_request.layer(cors);

        let socket = match &addr {
            SocketAddr::V4(_) => tokio::net::TcpSocket::new_v4(),
            SocketAddr::V6(_) => tokio::net::TcpSocket::new_v6(),
        }
        .context("Unable to open socket")?;
        // Copied this option from
        // https://github.com/bytecodealliance/wasmtime/blob/05095c18680927ce0cf6c7b468f9569ec4d11bd7/src/commands/serve.rs#L319.
        // This does increase throughput by 10-15% which is why we're creating the socket. We're
        // using the tokio one because it exposes the `reuseaddr` option.
        socket
            .set_reuseaddr(!cfg!(windows))
            .context("Error when setting socket to reuseaddr")?;
        socket.bind(addr).context("Unable to bind to address")?;
        let listener = socket.listen(1024).context("unable to listen on socket")?;
        let listener = listener.into_std().context("Unable to get listener")?;

        let handle = axum_server::Handle::new();
        let task_handle = handle.clone();

        let router = Arc::new(HttpRouter::default());
        let task_router = Arc::clone(&router);
        let task = if let Tls {
            cert_file: Some(crt),
            priv_key_file: Some(key),
        } = &settings.tls
        {
            debug!(?addr, "bind HTTPS listener");
            let tls = RustlsConfig::from_pem_file(crt, key)
                .await
                .context("failed to construct TLS config")?;

            tokio::spawn(async move {
                if let Err(e) = axum_server::from_tcp_rustls(listener, tls)
                    .handle(task_handle)
                    .serve(
                        service
                            .with_state(RequestContext {
                                settings,
                                router: task_router,
                                scheme: http::uri::Scheme::HTTPS,
                            })
                            .into_make_service(),
                    )
                    .await
                {
                    error!(error = %e,  "failed to serve HTTPS for component");
                }
            })
        } else {
            debug!(?addr, "bind HTTP listener");

            tokio::spawn(async move {
                if let Err(e) = axum_server::from_tcp(listener)
                    .handle(task_handle)
                    .serve(
                        service
                            .with_state(RequestContext {
                                settings,
                                router: task_router,
                                scheme: http::uri::Scheme::HTTP,
                            })
                            .into_make_service(),
                    )
                    .await
                {
                    error!(error = %e, "failed to serve HTTP for component");
                }
            })
        };

        Ok(Self {
            task,
            router,
            handle,
        })
    }
}

impl Provider for HttpServerProvider {
    /// Provider should perform any operations needed for a new link,
    /// including setting up per-component resources, and checking authorization.
    async fn receive_link_config_as_source(
        &self,
        link_config: LinkConfig<'_>,
    ) -> anyhow::Result<()> {
        let settings = match load_link(link_config.target_id, link_config.config)
            .context("httpserver failed to load settings for component")
        {
            Ok(settings) => settings,
            Err(e) => {
                error!(
                    config = ?link_config.config,
                    "httpserver failed to load settings for component: {}", e.to_string()
                );
                return Err(e);
            }
        };

        if self
            .router
            .hostnames_to_links
            .contains_key(&settings.hostname)
        {
            return Err(anyhow::anyhow!(
                "hostname already in use: {}",
                settings.hostname
            ));
        }

        info!(
            "new link received: {:?} -> {:?}",
            link_config.target_id, settings
        );

        self.router
            .hostnames_to_links
            .insert(settings.hostname.clone(), settings.component.clone());
        self.router
            .links_to_hostnames
            .insert(settings.component.clone(), settings.hostname.clone());

        Ok(())
    }

    async fn delete_link(&self, actor_id: &str) -> anyhow::Result<()> {
        if let Some((_, link_entry)) = self.router.links_to_hostnames.remove(actor_id) {
            self.router.hostnames_to_links.remove(&link_entry);
            info!(%actor_id, "Removing component")
        }
        Ok(())
    }

    async fn shutdown(&self) -> anyhow::Result<()> {
        // empty the component link data and stop all servers
        self.handle.shutdown();
        self.task.abort();
        Ok(())
    }
}

/// Errors generated by this HTTP server
#[derive(Debug, thiserror::Error)]
pub enum HttpServerError {
    #[error("invalid parameter: {0}")]
    InvalidParameter(String),

    #[error("problem reading settings: {0}")]
    Settings(String),

    #[error("provider startup: {0}")]
    Init(String),

    #[error("axum error: {0}")]
    Axum(axum::Error),

    #[error("deserializing settings: {0}")]
    SettingsToml(toml::de::Error),
}
