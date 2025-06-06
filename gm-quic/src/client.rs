use std::{io, sync::Arc};

use dashmap::DashMap;
use handy::UdpSocketController;
use qbase::{
    net::address::{AddrKind, BindAddr, IpFamily},
    param::RememberedParameters,
};
use qconnection::builder::*;
use qevent::telemetry::{Log, handy::NoopLogger};
use rustls::{
    ClientConfig as TlsClientConfig, ConfigBuilder, WantsVerifier,
    client::{ResolvesClientCert, WantsClientCert},
};
use tokio::sync::mpsc;

use crate::*;

type TlsClientConfigBuilder<T> = ConfigBuilder<TlsClientConfig, T>;

/// A QUIC client for initiating connections to servers.
///
/// ## Creating Clients
///
/// Use [`QuicClient::builder`] to configure and create a client instance.
/// Configure interfaces, TLS settings, and connection behavior before building.
///
/// ## Interface Management
///
/// - **Automatic binding**: If no interfaces are bound, the client automatically binds to system-assigned addresses
/// - **Manual binding**: Use [`QuicClientBuilder::bind`] to bind specific interfaces
/// - **Interface reuse**: Enable with [`QuicClientBuilder::reuse_address`] to share interfaces between connections
///
/// ## Connection Handling
///
/// Call [`QuicClient::connect`] to establish connections. The client supports:
/// - **Connection reuse**: Enable with [`QuicClientBuilder::reuse_connection`] to reuse existing connections
/// - **Automatic interface selection**: Matches interface with server endpoint address
pub struct QuicClient {
    bind_interfaces: Option<DashMap<BindAddr, Arc<dyn QuicInterface>>>,
    defer_idle_timeout: HeartbeatConfig,
    // TODO: 好像得创建2个quic连接，一个用ipv4，一个用ipv6
    //       然后看谁先收到服务器的响应比较好
    _enable_happy_eyepballs: bool,
    parameters: ClientParameters,
    _prefer_versions: Vec<u32>,
    quic_iface_factory: Box<dyn ProductQuicInterface>,
    // TODO: 要改成一个加载上次连接的parameters的函数，根据server name
    _remembered: Option<RememberedParameters>,
    reuse_connection: bool,
    reuse_address: bool,
    stream_strategy_factory: Box<dyn ProductStreamsConcurrencyController>,
    logger: Arc<dyn Log + Send + Sync>,
    tls_config: Arc<TlsClientConfig>,
    token_sink: Option<Arc<dyn TokenSink>>,
}

impl QuicClient {
    fn reuseable_connections() -> &'static DashMap<String, Arc<Connection>> {
        static REUSEABLE_CONNECTIONS: OnceLock<DashMap<String, Arc<Connection>>> = OnceLock::new();
        REUSEABLE_CONNECTIONS.get_or_init(Default::default)
    }

    /// Create a new [`QuicClient`] builder.
    pub fn builder() -> QuicClientBuilder<TlsClientConfigBuilder<WantsVerifier>> {
        Self::builder_with_tls(TlsClientConfig::builder_with_protocol_versions(&[
            &rustls::version::TLS13,
        ]))
    }

    /// Create a [`QuicClient`] builder with custom crypto provider.
    pub fn builder_with_crypto_provieder(
        provider: Arc<rustls::crypto::CryptoProvider>,
    ) -> QuicClientBuilder<TlsClientConfigBuilder<WantsVerifier>> {
        Self::builder_with_tls(
            TlsClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap(),
        )
    }

    /// Start to build a QuicClient with the given TLS configuration.
    ///
    /// This is useful when you want to customize the TLS configuration, or integrate qm-quic with other crates.
    pub fn builder_with_tls<T>(tls_config: T) -> QuicClientBuilder<T> {
        QuicClientBuilder {
            bind_interfaces: DashMap::new(),
            reuse_address: false,
            reuse_connection: false,
            enable_happy_eyepballs: false,
            prefer_versions: vec![1],
            defer_idle_timeout: HeartbeatConfig::default(),
            quic_iface_factory: Box::new(UdpSocketController::bind),
            parameters: ClientParameters::default(),
            tls_config,
            stream_strategy_factory: Box::new(ConsistentConcurrency::new),
            logger: None,
            token_sink: None,
        }
    }

    fn new_connection(
        &self,
        server_name: String,
        server_ep: EndpointAddr,
    ) -> io::Result<Arc<Connection>> {
        let quic_iface = match &self.bind_interfaces {
            None => {
                let bind_addr = match server_ep.kind() {
                    AddrKind::Ip(IpFamily::V4) => "inet://0.0.0.0/alloc".into(),
                    AddrKind::Ip(IpFamily::V6) => "inet://::/alloc".into(),
                    _ => {
                        return Err(io::Error::other(
                            "Failed to bind an unspecified address for this kind of endpoint address",
                        ));
                    }
                };
                let quic_iface = self.quic_iface_factory.bind(bind_addr)?;
                crate::proto().add_interface(quic_iface.clone());
                quic_iface
            }
            Some(bind_interfaces) => bind_interfaces
                .iter()
                .map(|entry| entry.key().clone())
                .filter(|bind_addr| bind_addr.kind() == server_ep.kind())
                .find_map(|bind_addr| {
                    if self.reuse_address {
                        crate::proto().get_interface(bind_addr)
                    } else {
                        crate::proto()
                            .get_interface_if(bind_addr, |iface, _| Arc::strong_count(iface) == 1)
                    }
                })
                .ok_or(io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "No suitable address available",
                ))?,
        };

        let local_addr = quic_iface.read_addr()?;
        let link = Link::new(local_addr, *server_ep);
        //  TODO: 是否要outer addr，agent addr
        let pathway = Pathway::new(EndpointAddr::direct(local_addr), server_ep);

        let token_sink = self
            .token_sink
            .clone()
            .unwrap_or_else(|| Arc::new(NoopTokenRegistry));

        let (event_broker, mut events) = mpsc::unbounded_channel();

        let origin_dcid = ConnectionId::random_gen(8);
        let connection = Arc::new(
            Connection::with_token_sink(server_name.clone(), token_sink)
                .with_parameters(self.parameters.clone(), None)
                .with_tls_config(self.tls_config.clone())
                .with_streams_concurrency_strategy(self.stream_strategy_factory.as_ref())
                .with_proto(crate::proto().clone())
                .defer_idle_timeout(self.defer_idle_timeout)
                .with_cids(origin_dcid)
                .with_qlog(self.logger.as_ref())
                .run_with(event_broker),
        );

        tokio::spawn({
            let connection = connection.clone();
            async move {
                while let Some(event) = events.recv().await {
                    match event {
                        Event::Handshaked => {}
                        Event::ProbedNewPath(_, _) => {}
                        Event::PathInactivated(bind_addr, ..) => {
                            crate::proto().try_free_interface(bind_addr);
                        }
                        Event::ApplicationClose => {
                            Self::reuseable_connections().remove_if(&server_name, |_, exist| {
                                Arc::ptr_eq(&connection, exist)
                            });
                        }
                        Event::Failed(error) => {
                            Self::reuseable_connections().remove_if(&server_name, |_, exist| {
                                Arc::ptr_eq(&connection, exist)
                            });
                            connection.enter_closing(qbase::error::Error::from(error).into())
                        }
                        Event::Closed(ccf) => {
                            Self::reuseable_connections().remove_if(&server_name, |_, exist| {
                                Arc::ptr_eq(&connection, exist)
                            });
                            connection.enter_draining(ccf)
                        }
                        Event::StatelessReset => { /* TOOD: stateless reset */ }
                        Event::Terminated => return,
                    }
                }
            }
        });

        connection.add_path(quic_iface.bind_addr(), link, pathway)?;
        Ok(connection)
    }

    /// Returns the connection to the specified server.
    ///
    /// `server_name` is the name of the server, it will be included in the `ClientHello` message.
    ///
    /// `server_addr` is the address of the server, packets will be sent to this address.
    ///
    /// Note that the returned connection may not yet be connected to the server, but you can use it to do anything you
    /// want, such as sending data, receiving data... operations will be pending until the connection is connected or
    /// failed to connect.
    ///
    /// ### Select an interface
    ///
    /// First, the client will select an interface to communicate with the server.
    ///
    /// If the client has already bound a set of addresses, The client will select the interface whose IP family of the
    /// first address matches the server addr from the bound and not closed interfaces.
    ///
    /// If `reuse_address` is not enabled; the client will not select an interface that is in use.
    ///
    /// ### Connecte to server
    ///
    /// If connection reuse is enabled, the client will give priority to returning the existing connection to the
    /// `server_name` and `server_addr`.
    ///
    /// If the client does not bind any interface, the client will bind the interface on the address/port randomly assigned
    /// by the system (i.e. xxx) through `quic_iface_factory` *every time* it establishes a connection. When no interface is
    /// bound, the reuse interface option will have no effect.
    ///
    /// If `reuse connection` is not enabled or there is no connection that can be reused, the client will initiates
    /// a new connection to the server.
    pub fn connect(
        &self,
        server_name: impl Into<String>,
        server_ep: impl ToEndpointAddr,
    ) -> io::Result<Arc<Connection>> {
        let server_name = server_name.into();
        let server_ep = server_ep.to_endpoint_addr();
        if self.reuse_connection {
            Self::reuseable_connections()
                .entry(server_name.clone())
                .or_try_insert_with(|| self.new_connection(server_name, server_ep))
                .map(|entry| entry.clone())
        } else {
            self.new_connection(server_name, server_ep)
        }
    }
}

impl Drop for QuicClient {
    fn drop(&mut self) {
        if let Some(bind_interfaces) = self.bind_interfaces.take() {
            for (bind_addr, bind_iface) in bind_interfaces.into_read_only().iter() {
                crate::proto().del_interface_if(bind_addr.clone(), |iface, _| {
                    Arc::ptr_eq(bind_iface, iface) && Arc::strong_count(iface) == 2
                });
            }
        }
    }
}

/// A builder for [`QuicClient`].
pub struct QuicClientBuilder<T> {
    bind_interfaces: DashMap<BindAddr, Arc<dyn QuicInterface>>,
    reuse_address: bool,
    reuse_connection: bool,
    enable_happy_eyepballs: bool,
    prefer_versions: Vec<u32>,
    quic_iface_factory: Box<dyn ProductQuicInterface>,
    defer_idle_timeout: HeartbeatConfig,
    parameters: ClientParameters,
    tls_config: T,
    stream_strategy_factory: Box<dyn ProductStreamsConcurrencyController>,
    logger: Option<Arc<dyn Log + Send + Sync>>,
    token_sink: Option<Arc<dyn TokenSink>>,
}

impl<T> QuicClientBuilder<T> {
    /// Specify how client bind interfaces.
    ///
    /// The given factory will be used by [`Self::bind`],
    /// and/or [`QuicClient::connect`] if no interface bound when client built.
    ///
    /// The default quic interface is [`UdpSocketController`] that support GSO and GRO,
    /// and the factory is [`UdpSocketController::bind`].
    pub fn with_iface_factory(self, factory: impl ProductQuicInterface + 'static) -> Self {
        Self {
            quic_iface_factory: Box::new(factory),
            ..self
        }
    }

    /// Create quic interfaces bound on given address.
    ///
    /// If the bind failed, the error will be returned immediately.
    ///
    /// The default quic interface is [`UdpSocketController`] that support GSO and GRO.
    /// You can let the client bind custom interfaces by calling the [`Self::with_iface_factory`] method.
    ///
    /// If you dont bind any address, each time the client initiates a new connection,
    /// the client will use bind a new interface on address and port that dynamic assigned by the system.
    ///
    /// To know more about how the client selects the interface when initiates a new connection,
    /// read [`QuicClient::connect`].
    ///
    /// If you call this multiple times, only the last set of interface will be used,
    /// previous bound interface will be freed immediately.
    ///
    /// If the interface is closed for some reason after being created (meaning [`QuicInterface::poll_recv`]
    /// returns an error), only the log will be printed.
    ///
    /// If all interfaces are closed, clients will no longer be able to initiate new connections.
    pub fn bind(self, addrs: impl IntoIterator<Item = impl Into<BindAddr>>) -> io::Result<Self> {
        for entry in self.bind_interfaces.iter() {
            crate::proto().try_free_interface(entry.key().clone());
        }
        self.bind_interfaces.clear();

        for bind_addr in addrs.into_iter().map(Into::into) {
            if self.bind_interfaces.contains_key(&bind_addr) {
                continue;
            }
            let interface = self.quic_iface_factory.bind(bind_addr.clone())?;
            self.bind_interfaces.insert(bind_addr, interface);
        }

        Ok(self)
    }

    /// Enable efficiently reuse connections.
    ///
    /// If you enable this option the client will give priority to returning the existing connection to the `server_name`
    /// and `server_addr`, instead of creating a new connection every time.
    pub fn reuse_connection(mut self) -> Self {
        self.reuse_connection = true;
        self
    }

    /// Enable reuse interface.
    ///
    /// If you dont bind any address, this option will not take effect.
    ///
    /// By default, the client will not use the same interface with other connections, which means that the client must
    /// select a new interface every time it initiates a connection. If you enable this option, the client will share
    /// the same address between connections.
    pub fn reuse_address(mut self) -> Self {
        self.reuse_address = true;
        self
    }

    /// (WIP)Specify the quic versions that the client prefers.
    ///
    /// If you call this multiple times, only the last call will take effect.
    pub fn prefer_versions(mut self, versions: impl IntoIterator<Item = u32>) -> Self {
        self.prefer_versions.clear();
        self.prefer_versions.extend(versions);
        self
    }

    /// Provide an option to defer an idle timeout.
    ///
    /// This facility could be used when the application wishes to avoid losing
    /// state that has been associated with an open connection but does not expect
    /// to exchange application data for some time.
    ///
    /// See [Deferring Idle Timeout](https://datatracker.ietf.org/doc/html/rfc9000#name-deferring-idle-timeout)
    /// of [RFC 9000](https://datatracker.ietf.org/doc/html/rfc9000)
    /// for more information.
    pub fn defer_idle_timeout(mut self, config: HeartbeatConfig) -> Self {
        self.defer_idle_timeout = config;
        self
    }

    /// Specify the [transport parameters] for the client.
    ///
    /// If you call this multiple times, only the last `parameters` will be used.
    ///
    /// Usually, you don't need to call this method, because the client will use a set of default parameters.
    ///
    /// [transport parameters](https://www.rfc-editor.org/rfc/rfc9000.html#name-transport-parameter-definit)
    pub fn with_parameters(mut self, parameters: ClientParameters) -> Self {
        self.parameters = parameters;
        self
    }

    /// Specify the streams concurrency strategy controller for the client.
    ///
    /// The streams controller is used to control the concurrency of data streams. `controller` is a closure that accept
    /// (initial maximum number of bidirectional streams, initial maximum number of unidirectional streams) configured in
    /// [transport parameters] and return a `ControlConcurrency` object.
    ///
    /// If you call this multiple times, only the last `controller` will be used.
    ///
    /// [transport parameters](https://www.rfc-editor.org/rfc/rfc9000.html#name-transport-parameter-definit)
    pub fn with_streams_concurrency_strategy(
        mut self,
        strategy_factory: impl ProductStreamsConcurrencyController + 'static,
    ) -> Self {
        self.stream_strategy_factory = Box::new(strategy_factory);
        self
    }

    pub fn with_qlog(mut self, logger: Arc<dyn Log + Send + Sync>) -> Self {
        self.logger = Some(logger);
        self
    }

    /// Specify the token sink for the client.
    ///
    /// The token sink is used to storage the tokens that the client received from the server. The client will use the
    /// tokens to prove it self to the server when it reconnects to the server. read [address verification] in quic rfc
    /// for more information.
    ///
    /// [address verification](https://www.rfc-editor.org/rfc/rfc9000.html#name-address-validation)
    pub fn with_token_sink(mut self, sink: Arc<dyn TokenSink>) -> Self {
        self.token_sink = Some(sink);
        self
    }
}

impl QuicClientBuilder<TlsClientConfigBuilder<WantsVerifier>> {
    /// Choose how to verify server certificates.
    ///
    /// Read [TlsClientConfigBuilder::with_root_certificates] for more information.
    pub fn with_root_certificates(
        self,
        root_store: impl Into<Arc<rustls::RootCertStore>>,
    ) -> QuicClientBuilder<TlsClientConfigBuilder<WantsClientCert>> {
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            defer_idle_timeout: self.defer_idle_timeout,
            quic_iface_factory: self.quic_iface_factory,
            parameters: self.parameters,
            tls_config: self.tls_config.with_root_certificates(root_store),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }

    /// Choose how to verify server certificates using a webpki verifier.
    ///
    /// Read [TlsClientConfigBuilder::with_webpki_verifier] for more information.
    pub fn with_webpki_verifier(
        self,
        verifier: Arc<rustls::client::WebPkiServerVerifier>,
    ) -> QuicClientBuilder<TlsClientConfigBuilder<WantsClientCert>> {
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            defer_idle_timeout: self.defer_idle_timeout,
            quic_iface_factory: self.quic_iface_factory,
            parameters: self.parameters,
            tls_config: self.tls_config.with_webpki_verifier(verifier),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }

    /// Dangerously disable server certificate verification.
    pub fn without_verifier(self) -> QuicClientBuilder<TlsClientConfigBuilder<WantsClientCert>> {
        #[derive(Debug)]
        struct DangerousServerCertVerifier;

        impl rustls::client::danger::ServerCertVerifier for DangerousServerCertVerifier {
            fn verify_server_cert(
                &self,
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &[rustls::pki_types::CertificateDer<'_>],
                _: &rustls::pki_types::ServerName<'_>,
                _: &[u8],
                _: rustls::pki_types::UnixTime,
            ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }

            fn verify_tls13_signature(
                &self,
                _: &[u8],
                _: &rustls::pki_types::CertificateDer<'_>,
                _: &rustls::DigitallySignedStruct,
            ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error>
            {
                Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
            }

            fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
                vec![
                    rustls::SignatureScheme::RSA_PKCS1_SHA1,
                    rustls::SignatureScheme::ECDSA_SHA1_Legacy,
                    rustls::SignatureScheme::RSA_PKCS1_SHA256,
                    rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
                    rustls::SignatureScheme::RSA_PKCS1_SHA384,
                    rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
                    rustls::SignatureScheme::RSA_PKCS1_SHA512,
                    rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
                    rustls::SignatureScheme::RSA_PSS_SHA256,
                    rustls::SignatureScheme::RSA_PSS_SHA384,
                    rustls::SignatureScheme::RSA_PSS_SHA512,
                    rustls::SignatureScheme::ED25519,
                    rustls::SignatureScheme::ED448,
                ]
            }
        }
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            defer_idle_timeout: self.defer_idle_timeout,
            quic_iface_factory: self.quic_iface_factory,
            parameters: self.parameters,
            tls_config: self
                .tls_config
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(DangerousServerCertVerifier)),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }
}

impl QuicClientBuilder<TlsClientConfigBuilder<WantsClientCert>> {
    /// Sets a single certificate chain and matching private key for use
    /// in client authentication.
    ///
    /// Read [TlsClientConfigBuilder::with_single_cert] for more information.
    pub fn with_cert(
        self,
        cert_chain: impl ToCertificate,
        key_der: impl ToPrivateKey,
    ) -> QuicClientBuilder<TlsClientConfig> {
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            defer_idle_timeout: self.defer_idle_timeout,
            quic_iface_factory: self.quic_iface_factory,
            parameters: self.parameters,
            tls_config: self
                .tls_config
                .with_client_auth_cert(cert_chain.to_certificate(), key_der.to_private_key())
                .expect("The private key was wrong encoded or failed validation"),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }

    /// Do not support client auth.
    pub fn without_cert(self) -> QuicClientBuilder<TlsClientConfig> {
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            defer_idle_timeout: self.defer_idle_timeout,
            quic_iface_factory: self.quic_iface_factory,
            parameters: self.parameters,
            tls_config: self.tls_config.with_no_client_auth(),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }

    /// Sets a custom [`ResolvesClientCert`].
    pub fn with_cert_resolver(
        self,
        cert_resolver: Arc<dyn ResolvesClientCert>,
    ) -> QuicClientBuilder<TlsClientConfig> {
        QuicClientBuilder {
            bind_interfaces: self.bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            enable_happy_eyepballs: self.enable_happy_eyepballs,
            prefer_versions: self.prefer_versions,
            quic_iface_factory: self.quic_iface_factory,
            defer_idle_timeout: self.defer_idle_timeout,
            parameters: self.parameters,
            tls_config: self.tls_config.with_client_cert_resolver(cert_resolver),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger,
            token_sink: self.token_sink,
        }
    }
}

impl QuicClientBuilder<TlsClientConfig> {
    /// Specify the [alpn-protocol-ids] that will be sent in `ClientHello`.
    ///
    /// By default, its empty and the APLN extension wont be sent.
    ///
    /// If you call this multiple times, all the `alpn_protocol` will be used.
    ///
    /// [alpn-protocol-ids](https://www.iana.org/assignments/tls-extensiontype-values/tls-extensiontype-values.xhtml#alpn-protocol-ids)
    pub fn with_alpns(mut self, alpns: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> Self {
        self.tls_config
            .alpn_protocols
            .extend(alpns.into_iter().map(Into::into));
        self
    }

    /// Enable the `keylog` feature.
    ///
    /// This is useful when you want to debug the TLS connection.
    ///
    /// The keylog file will be in the file that environment veriable `SSLKEYLOGFILE` pointed to.
    ///
    /// Read [`rustls::KeyLogFile`] for more information.
    pub fn enable_sslkeylog(mut self) -> Self {
        self.tls_config.key_log = Arc::new(rustls::KeyLogFile::new());
        self
    }

    /// Build the QuicClient, ready to initiates connect to the servers.
    pub fn build(mut self) -> QuicClient {
        self.tls_config.resumption = rustls::client::Resumption::disabled();
        let bind_interfaces = if self.bind_interfaces.is_empty() {
            None
        } else {
            Some(self.bind_interfaces)
        };
        QuicClient {
            bind_interfaces,
            reuse_address: self.reuse_address,
            reuse_connection: self.reuse_connection,
            _enable_happy_eyepballs: self.enable_happy_eyepballs,
            _prefer_versions: self.prefer_versions,
            quic_iface_factory: self.quic_iface_factory,
            defer_idle_timeout: self.defer_idle_timeout,
            parameters: self.parameters,
            // TODO: 要能加载上次连接的parameters
            _remembered: None,
            tls_config: Arc::new(self.tls_config),
            stream_strategy_factory: self.stream_strategy_factory,
            logger: self.logger.unwrap_or_else(|| Arc::new(NoopLogger)),
            token_sink: self.token_sink,
        }
    }
}
