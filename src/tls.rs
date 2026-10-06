//! :9092's transport (#19; stormcos SECURITY.md, the owner's rule of
//! 2026-09-25): the API is served over TLS with a stormcert serving
//! certificate, and only health answers anonymously.
//!
//! One port serves both, so stormd's liveness probe of `/api/v1/health`
//! keeps working unchanged:
//! - a connection that opens with a TLS handshake gets TLS. Client
//!   certificates are requested and verified against the node CA, but not
//!   required (a bearer is the other way in); the gate in `api` decides what
//!   each caller may do;
//! - a plain-HTTP connection is health only; every other path is refused
//!   there (unless `[api] allow_anonymous`, the transition).
//!
//! The serving pair is read from disk and re-read when it changes, so a
//! renewal (or a pair that appears after start) needs no restart. While
//! there is none, a TLS handshake fails and plain health still answers. The
//! client CAs are read at start. Taken from vmimages' listener
//! (vmcloud-image-operator#16), which follows stormcluster#5's.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use axum::extract::connect_info::Connected;
use axum::serve::IncomingStream;
use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;

/// A slow or silent client holds only its own connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// A verified client certificate's subject: CN, and each O as a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    pub cn: String,
    pub groups: Vec<String>,
}

/// A connection's peer, as handlers see it (`ConnectInfo<Peer>`).
#[derive(Debug, Clone)]
pub struct Peer {
    pub addr: SocketAddr,
    /// Over TLS (and so the server was who it said).
    pub tls: bool,
    /// A client certificate rustls verified against the client CAs.
    pub client: Option<ClientIdentity>,
}

/// The serving pair, re-read from its files when they change.
#[derive(Debug)]
pub struct ServingCert {
    cert_file: PathBuf,
    key_file: PathBuf,
    current: RwLock<Option<(SystemTime, SystemTime, Arc<CertifiedKey>)>>,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

impl ServingCert {
    pub fn new(cert_file: PathBuf, key_file: PathBuf) -> Arc<Self> {
        let me = Arc::new(Self { cert_file, key_file, current: RwLock::new(None) });
        if me.get().is_none() {
            tracing::warn!(
                "no serving certificate at {} / {}: a TLS handshake fails, and plain HTTP answers health only, until there is one",
                me.cert_file.display(),
                me.key_file.display()
            );
        }
        me
    }

    /// The pair in force: the one loaded, or the files again when they
    /// changed. A pair that does not load keeps the last good one.
    pub fn get(&self) -> Option<Arc<CertifiedKey>> {
        let (Some(cm), Some(km)) = (mtime(&self.cert_file), mtime(&self.key_file)) else {
            return self.current.read().ok()?.as_ref().map(|c| c.2.clone());
        };
        if let Some((c, k, ck)) = self.current.read().ok()?.as_ref() {
            if *c == cm && *k == km {
                return Some(ck.clone());
            }
        }
        match load_pair(&self.cert_file, &self.key_file) {
            Ok(ck) => {
                tracing::info!("serving certificate loaded from {}", self.cert_file.display());
                *self.current.write().ok()? = Some((cm, km, ck.clone()));
                Some(ck)
            }
            Err(e) => {
                tracing::warn!("serving certificate {}: {e:#}", self.cert_file.display());
                self.current.read().ok()?.as_ref().map(|c| c.2.clone())
            }
        }
    }
}

impl ResolvesServerCert for ServingCert {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.get()
    }
}

fn load_pair(cert: &Path, key: &Path) -> Result<Arc<CertifiedKey>> {
    let cert_pem = std::fs::read(cert).with_context(|| format!("reading {}", cert.display()))?;
    let key_pem = std::fs::read(key).with_context(|| format!("reading {}", key.display()))?;
    let chain = CertificateDer::pem_slice_iter(&cert_pem)
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("certificate PEM")?;
    anyhow::ensure!(!chain.is_empty(), "no certificate in {}", cert.display());
    let key = PrivateKeyDer::from_pem_slice(&key_pem).context("key PEM")?;
    let signing = rustls::crypto::ring::default_provider().key_provider.load_private_key(key).context("key")?;
    let ck = CertifiedKey::new(chain, signing);
    ck.keys_match().context("the key does not match the certificate")?;
    Ok(Arc::new(ck))
}

/// Every certificate in the CA files that are there; a missing one is
/// skipped (a node before its CA exists).
pub fn roots(files: &[PathBuf]) -> Result<rustls::RootCertStore> {
    let mut roots = rustls::RootCertStore::empty();
    for f in files {
        let Ok(pem) = std::fs::read(f) else { continue };
        for ca in CertificateDer::pem_slice_iter(&pem) {
            roots.add(ca.with_context(|| format!("CA in {}", f.display()))?).with_context(|| f.display().to_string())?;
        }
    }
    Ok(roots)
}

/// TLS 1.2+, the serving pair from `cert`, client certificates requested
/// and verified against `client_cas` (not required). With no client CA at
/// all, only bearer tokens can authenticate.
pub fn server_config(cert: Arc<ServingCert>, client_cas: &[PathBuf]) -> Result<Arc<rustls::ServerConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let roots = roots(client_cas)?;
    let builder = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .context("TLS versions")?;
    let builder = if roots.is_empty() {
        tracing::warn!("no client CA found: client certificates cannot authenticate, only bearers");
        builder.with_no_client_auth()
    } else {
        let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider)
            .allow_unauthenticated()
            .build()
            .context("client certificate verifier")?;
        builder.with_client_cert_verifier(verifier)
    };
    let mut config = builder.with_cert_resolver(cert);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// CN and each O of a certificate rustls has verified.
pub fn identity_of(der: &[u8]) -> Option<ClientIdentity> {
    let (_, cert) = x509_parser::parse_x509_certificate(der).ok()?;
    let subject = cert.subject();
    let cn = subject.iter_common_name().find_map(|a| a.as_str().ok()).unwrap_or_default().to_string();
    let groups = subject.iter_organization().filter_map(|a| a.as_str().ok()).map(String::from).collect();
    Some(ClientIdentity { cn, groups })
}

/// A connection: TLS, or plain (health only).
pub enum Conn {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl AsyncRead for Conn {
    fn poll_read(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Conn {
    fn poll_write(self: Pin<&mut Self>, cx: &mut TaskContext<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_flush(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Conn::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Conn::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Accepts TCP, tells TLS from plain by the first byte (a handshake record
/// is 0x16), and yields connections ready to serve.
pub struct Listener {
    ready: mpsc::Receiver<(Conn, Peer)>,
    local: SocketAddr,
}

impl Listener {
    pub fn new(tcp: TcpListener, config: Arc<rustls::ServerConfig>) -> std::io::Result<Self> {
        let local = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(config);
        let (tx, ready) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                let (stream, addr) = match tcp.accept().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::debug!("accept: {e}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    let mut first = [0u8; 1];
                    let tls = match tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.peek(&mut first)).await {
                        Ok(Ok(1)) => first[0] == 0x16,
                        _ => return,
                    };
                    if !tls {
                        let _ = tx.send((Conn::Plain(stream), Peer { addr, tls: false, client: None })).await;
                        return;
                    }
                    let s = match tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => return tracing::debug!(%addr, "TLS handshake: {e}"),
                        Err(_) => return tracing::debug!(%addr, "TLS handshake timed out"),
                    };
                    let client =
                        s.get_ref().1.peer_certificates().and_then(|c| c.first()).and_then(|leaf| identity_of(leaf.as_ref()));
                    let _ = tx.send((Conn::Tls(Box::new(s)), Peer { addr, tls: true, client })).await;
                });
            }
        });
        Ok(Self { ready, local })
    }
}

impl axum::serve::Listener for Listener {
    type Io = Conn;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(c) => c,
            // The accept task lives as long as the listener.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(Peer { addr: self.local, tls: false, client: None })
    }
}

impl Connected<IncomingStream<'_, Listener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, Listener>) -> Self {
        stream.remote_addr().clone()
    }
}
