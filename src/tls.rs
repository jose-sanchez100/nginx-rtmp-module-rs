//! RTMPS: RTMP over TLS (rustls with the `ring` provider).
//!
//! Not in nginx-rtmp-module, where RTMPS needs an external proxy (stunnel or
//! nginx's `stream { ssl }`). Here `push` / `pull` accept `rtmps://` URLs
//! and `listen ... ssl` accepts RTMPS clients.

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::sync::{Arc, OnceLock};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, ServerConfig};
use tokio_rustls::{TlsAcceptor, TlsConnector};

/// A plain or TLS connection.
pub trait Io: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Io for T {}
pub type BoxIo = Box<dyn Io>;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Client configuration: Mozilla's root certificates (like stunnel's
/// `ca-certs.pem`), certificate and host name checked.
fn connector() -> &'static TlsConnector {
    static C: OnceLock<TlsConnector> = OnceLock::new();
    C.get_or_init(|| {
        let roots = RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
        let config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .expect("default TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConnector::from(Arc::new(config))
    })
}

/// Open a TCP connection to `host:port`, wrapped in TLS when `tls` is set.
pub async fn connect(host: &str, port: u16, tls: bool) -> io::Result<BoxIo> {
    let sock = TcpStream::connect((host, port)).await?;
    let _ = sock.set_nodelay(true);
    if !tls {
        return Ok(Box::new(sock));
    }
    let name = ServerName::try_from(host.to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("invalid TLS server name \"{host}\"")))?;
    let stream = connector()
        .connect(name, sock)
        .await
        .map_err(|e| io::Error::new(e.kind(), format!("TLS handshake with {host}:{port} failed: {e}")))?;
    Ok(Box::new(LaxEof(stream)))
}

/// Ingest servers often drop the TCP connection without a TLS
/// `close_notify`; report that as a normal end of stream instead of an
/// error (the RTMP layer has its own framing, so truncation is detected
/// there).
struct LaxEof<T>(T);

impl<T: AsyncRead + Unpin> AsyncRead for LaxEof<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match Pin::new(&mut self.0).poll_read(cx, buf) {
            Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::UnexpectedEof => Poll::Ready(Ok(())),
            r => r,
        }
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for LaxEof<T> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}

/// Server configuration from PEM files: the certificate chain (leaf first)
/// and its private key (PKCS#8, PKCS#1 or SEC1).
pub fn acceptor(cert: &Path, key: &Path) -> io::Result<TlsAcceptor> {
    let bad = |what: &str, p: &Path, e: &dyn std::fmt::Display| {
        io::Error::new(io::ErrorKind::InvalidData, format!("cannot load {what} \"{}\": {e}", p.display()))
    };
    let certs = CertificateDer::pem_file_iter(cert)
        .and_then(|it| it.collect::<Result<Vec<_>, _>>())
        .map_err(|e| bad("certificate", cert, &e))?;
    if certs.is_empty() {
        return Err(bad("certificate", cert, &"no certificate found"));
    }
    let key_der = PrivateKeyDer::from_pem_file(key).map_err(|e| bad("certificate key", key, &e))?;
    let config = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("default TLS versions")
        .with_no_client_auth()
        .with_single_cert(certs, key_der)
        .map_err(|e| bad("certificate key", key, &e))?;
    Ok(TlsAcceptor::from(Arc::new(config)))
}
