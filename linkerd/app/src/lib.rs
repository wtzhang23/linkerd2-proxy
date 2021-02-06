//! Configures and executes the proxy

#![deny(warnings, rust_2018_idioms)]

pub mod admin;
pub mod dst;
pub mod env;
pub mod identity;
pub mod oc_collector;
pub mod tap;

pub use self::metrics::Metrics;
use futures::{future, FutureExt, TryFutureExt};
pub use linkerd_app_core::{self as core, metrics, trace};
use linkerd_app_core::{
    control::ControlAddr, dns, drain, proxy::http, serve, svc, Error, ProxyRuntime,
};
use linkerd_app_gateway as gateway;
use linkerd_app_inbound::{self as inbound, Inbound};
use linkerd_app_outbound::{self as outbound, Outbound};
use linkerd_channel::into_stream::IntoStream;
use std::{net::SocketAddr, pin::Pin};
use tokio::{sync::mpsc, time::Duration};

use tracing::instrument::Instrument;
use tracing::{debug, error, info, info_span};

/// Spawns a sidecar proxy.
///
/// The proxy binds two listeners:
///
/// - a private socket (TCP or UNIX) for outbound requests to other instances;
/// - and a public socket (TCP and optionally TLS) for inbound requests from other
///   instances.
///
/// The public listener forwards requests to a local socket (TCP or UNIX).
///
/// The private listener routes requests to service-discovery-aware load-balancer.
///
#[derive(Clone, Debug)]
pub struct Config {
    pub outbound: outbound::Config,
    pub inbound: inbound::Config,
    pub gateway: gateway::Config,

    // In "ingress mode", we assume we are always routing HTTP requests and do
    // not perform per-target-address discovery. Non-HTTP connections are
    // forwarded without discovery/routing/mTLS.
    pub ingress_mode: bool,

    pub dns: dns::Config,
    pub identity: identity::Config,
    pub dst: dst::Config,
    pub admin: admin::Config,
    pub tap: tap::Config,
    pub oc_collector: oc_collector::Config,
}

pub struct App {
    admin: admin::Admin,
    drain: drain::Signal,
    dst: ControlAddr,
    identity: identity::Identity,
    inbound_addr: SocketAddr,
    oc_collector: oc_collector::OcCollector,
    outbound_addr: SocketAddr,
    start_proxy: Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    tap: tap::Tap,
}

impl Config {
    pub fn try_from_env() -> Result<Self, env::EnvError> {
        env::Env.try_config()
    }

    /// Build an application.
    ///
    /// It is currently required that this be run on a Tokio runtime, since some
    /// services are created eagerly and must spawn tasks to do so.
    pub async fn build(
        self,
        shutdown_tx: mpsc::UnboundedSender<()>,
        log_level: trace::Handle,
    ) -> Result<App, Error> {
        use metrics::FmtMetrics;

        let Config {
            admin,
            dns,
            dst,
            identity,
            inbound,
            oc_collector,
            outbound,
            gateway,
            tap,
            ingress_mode,
        } = self;
        debug!("building app");
        let (metrics, report) = Metrics::new(admin.metrics_retain_idle);

        let dns = dns.build();

        let identity = info_span!("identity")
            .in_scope(|| identity.build(dns.resolver.clone(), metrics.control.clone()))?;
        let report = identity.metrics().and_then(report);

        let (drain_tx, drain_rx) = drain::channel();

        let tap = info_span!("tap").in_scope(|| tap.build(identity.local(), drain_rx.clone()))?;
        let dst = {
            let metrics = metrics.control.clone();
            let dns = dns.resolver.clone();
            info_span!("dst").in_scope(|| dst.build(dns, metrics, identity.local()))
        }?;

        let oc_collector = {
            let identity = identity.local();
            let dns = dns.resolver;
            let client_metrics = metrics.control;
            let metrics = metrics.opencensus;
            info_span!("opencensus")
                .in_scope(|| oc_collector.build(identity, dns, metrics, client_metrics))
        }?;

        let admin = {
            let identity = identity.local();
            let drain = drain_rx.clone();
            let metrics = metrics.inbound.clone();
            info_span!("admin").in_scope(move || {
                admin.build(identity, report, metrics, log_level, drain, shutdown_tx)
            })?
        };

        let dst_addr = dst.addr.clone();

        let (inbound_addr, inbound_listen) = inbound.proxy.server.bind.bind()?;
        let inbound_metrics = metrics.inbound;

        let (outbound_addr, outbound_listen) = outbound.proxy.server.bind.bind()?;
        let outbound_metrics = metrics.outbound;

        let local_identity = identity.local();
        let tap_registry = tap.registry();
        let oc_span_sink = oc_collector.span_sink();

        let start_proxy = Box::pin(async move {
            let span = info_span!("outbound");
            let _outbound = span.enter();

            info!(listen.addr = %outbound_addr, ingress_mode);

            let outbound_rt = ProxyRuntime {
                identity: local_identity.clone(),
                metrics: outbound_metrics,
                tap: tap_registry.clone(),
                span_sink: oc_span_sink.clone(),
                drain: drain_rx.clone(),
            };

            if ingress_mode {
                let tcp = Outbound::new_tcp_connect(outbound.clone(), outbound_rt.clone())
                    .push_tcp_endpoint()
                    .push_tcp_forward()
                    .into_inner();
                let http = Outbound::new_tcp_connect(outbound.clone(), outbound_rt.clone())
                    .push_http_endpoint()
                    .push_http_logical(dst.resolve.clone())
                    .into_inner();
                let server = outbound::ingress::new_service(
                    outbound.clone(),
                    outbound_rt.clone(),
                    dst.profiles.clone(),
                    tcp,
                    http,
                );
                tokio::spawn(
                    serve::serve(outbound_listen, server, drain_rx.clone().signaled())
                        .map_err(|e| panic!("outbound failed: {}", e))
                        .instrument(span.clone()),
                );
            } else {
                let server = Outbound::new_tcp_connect(outbound.clone(), outbound_rt.clone())
                    .into_server(dst.resolve.clone(), dst.profiles.clone());
                tokio::spawn(
                    serve::serve(outbound_listen, server, drain_rx.clone().signaled())
                        .map_err(|e| panic!("outbound failed: {}", e))
                        .instrument(span.clone()),
                );
            }
            drop(_outbound);

            let span = info_span!("inbound");
            let _inbound = span.enter();
            info!(listen.addr = %inbound_addr);

            let inbound = Inbound::new_without_stack(
                inbound,
                ProxyRuntime {
                    identity: local_identity,
                    metrics: inbound_metrics,
                    tap: tap_registry,
                    span_sink: oc_span_sink,
                    drain: drain_rx.clone(),
                },
            );

            let gateway_stack = gateway::stack(
                gateway,
                inbound.clone(),
                Outbound::new_tcp_connect(outbound, outbound_rt)
                    .push_tcp_endpoint()
                    .push_http_endpoint()
                    .push_http_logical(dst.resolve.clone())
                    .into_inner(),
                dst.profiles.clone(),
            );

            tokio::spawn(
                serve::serve(
                    inbound_listen,
                    inbound.tcp_connect().into_server(
                        inbound_addr.port(),
                        dst.profiles,
                        gateway_stack,
                    ),
                    drain_rx.signaled(),
                )
                .map_err(|e| panic!("inbound failed: {}", e))
                .instrument(span.clone()),
            );
            drop(_inbound);
        });

        Ok(App {
            admin,
            dst: dst_addr,
            drain: drain_tx,
            identity,
            inbound_addr,
            oc_collector,
            outbound_addr,
            start_proxy,
            tap,
        })
    }
}

impl App {
    pub fn admin_addr(&self) -> SocketAddr {
        self.admin.listen_addr
    }

    pub fn inbound_addr(&self) -> SocketAddr {
        self.inbound_addr
    }

    pub fn outbound_addr(&self) -> SocketAddr {
        self.outbound_addr
    }

    pub fn tap_addr(&self) -> Option<SocketAddr> {
        match self.tap {
            tap::Tap::Disabled { .. } => None,
            tap::Tap::Enabled { listen_addr, .. } => Some(listen_addr),
        }
    }

    pub fn dst_addr(&self) -> &ControlAddr {
        &self.dst
    }

    pub fn local_identity(&self) -> Option<&identity::LocalCrtKey> {
        match self.identity {
            identity::Identity::Disabled => None,
            identity::Identity::Enabled { ref local, .. } => Some(local),
        }
    }

    pub fn identity_addr(&self) -> Option<&ControlAddr> {
        match self.identity {
            identity::Identity::Disabled => None,
            identity::Identity::Enabled { ref addr, .. } => Some(addr),
        }
    }

    pub fn opencensus_addr(&self) -> Option<&ControlAddr> {
        match self.oc_collector {
            oc_collector::OcCollector::Disabled { .. } => None,
            oc_collector::OcCollector::Enabled(ref oc) => Some(&oc.addr),
        }
    }

    pub fn spawn(self) -> drain::Signal {
        let App {
            admin,
            drain,
            identity,
            oc_collector,
            start_proxy,
            tap,
            ..
        } = self;

        // Run a daemon thread for all administrative tasks.
        //
        // The main reactor holds `admin_shutdown_tx` until the reactor drops
        // the task. This causes the daemon reactor to stop.
        let (admin_shutdown_tx, admin_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        debug!("spawning daemon thread");
        tokio::spawn(future::pending().map(|()| drop(admin_shutdown_tx)));
        std::thread::Builder::new()
            .name("admin".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("building admin runtime must succeed");
                rt.block_on(
                    async move {
                        debug!("running admin thread");

                        // Start the admin server to serve the readiness endpoint.
                        tokio::spawn(
                            admin
                                .serve
                                .map_err(|e| panic!("admin server died: {}", e))
                                .instrument(info_span!("admin", listen.addr = %admin.listen_addr)),
                        );

                        // Kick off the identity so that the process can become ready.
                        if let identity::Identity::Enabled { local, task, .. } = identity {
                            tokio::spawn(task.instrument(info_span!("identity")));

                            let latch = admin.latch;
                            tokio::spawn(
                                local
                                    .await_crt()
                                    .map_ok(move |id| {
                                        latch.release();
                                        info!("Certified identity: {}", id.name().as_ref());
                                    })
                                    .map_err(|_| {
                                        // The daemon task was lost?!
                                        panic!("Failed to certify identity!");
                                    })
                                    .instrument(info_span!("identity")),
                            );
                        } else {
                            admin.latch.release()
                        }

                        if let tap::Tap::Enabled {
                            registry, serve, ..
                        } = tap
                        {
                            tokio::spawn(
                                registry
                                    .clean(
                                        tokio::time::interval(Duration::from_secs(60))
                                            .into_stream(),
                                    )
                                    .instrument(info_span!("tap_clean")),
                            );
                            tokio::spawn(
                                serve
                                    .map_err(|error| error!(%error, "server died"))
                                    .instrument(info_span!("tap")),
                            );
                        }

                        if let oc_collector::OcCollector::Enabled(oc) = oc_collector {
                            tokio::spawn(oc.task.instrument(info_span!("opencensus")));
                        }

                        // we don't care if the admin shutdown channel is
                        // dropped or actually triggered.
                        let _ = admin_shutdown_rx.await;
                    }
                    .instrument(info_span!("daemon")),
                )
            })
            .expect("admin");

        tokio::spawn(start_proxy);

        drain
    }
}
