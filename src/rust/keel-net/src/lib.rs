//! keel's QUIC transport (Linear NEW-1102): content-addressed object transfer + a live event
//! channel over one multiplexed connection, so keel can run across machines — a remote agent can
//! fetch objects and subscribe to fleet coordination events, not just talk to a local Unix socket.
//!
//! Wire protocol (one bidi QUIC stream per request):
//!   GET       — `[0x01][32-byte oid]` → `[0x00]` (miss) or `[0x01][u64 len][bytes]` (the object's
//!               canonical encoding, which hashes back to the oid — content-addressed + verifiable)
//!   SUBSCRIBE — `[0x02]` → a stream of `[u32 len][event bytes]` frames, one per coordination event,
//!               until the connection closes
//!
//! TLS is required by QUIC; for now the server mints a self-signed cert and the client accepts any
//! (dev / single-tenant). Real deployments pin the server's cert — a small swap of the verifier.

use keel_store::object::{Object, ObjectId};
use keel_store::store::Store;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::broadcast;

const OP_GET: u8 = 0x01;
const OP_SUBSCRIBE: u8 = 0x02;
const ALPN: &[u8] = b"keel/1";

/// Hard ceiling on a length-prefixed frame the client will allocate for. The server accepts *any*
/// cert (`AcceptAny`), so a malicious or MITM'd peer can put an arbitrary u64/u32 on the wire; without
/// a cap `vec![0u8; n]` reserves it up front and aborts the process before a single byte is read.
/// Mirrors keel-git's `MAX_OBJECT_SIZE`.
const MAX_FRAME: usize = 4 * 1024 * 1024 * 1024;

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

// ── server ───────────────────────────────────────────────────────────────────────────────────

/// A running QUIC server. Serves objects from `store` and lets callers `publish` coordination
/// events to every subscribed client. Dropping it stops accepting new connections.
pub struct Server {
    local: SocketAddr,
    events: broadcast::Sender<Vec<u8>>,
    _endpoint: Endpoint,
}

impl Server {
    /// Bind a QUIC endpoint and start accepting connections in the background.
    pub async fn bind(addr: SocketAddr, store: Store) -> Result<Server> {
        let (cfg, _) = server_config()?;
        let endpoint = Endpoint::server(cfg, addr)?;
        let local = endpoint.local_addr()?;
        let (events, _) = broadcast::channel::<Vec<u8>>(1024);
        let ev = events.clone();
        let ep = endpoint.clone();
        tokio::spawn(async move {
            while let Some(incoming) = ep.accept().await {
                let store = store.clone();
                let ev = ev.clone();
                tokio::spawn(async move {
                    if let Ok(conn) = incoming.await {
                        let _ = serve_conn(conn, store, ev).await;
                    }
                });
            }
        });
        Ok(Server { local, events, _endpoint: endpoint })
    }

    /// The address actually bound (useful when binding to port 0).
    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// Broadcast a coordination event to all current subscribers (best-effort; no subscribers = a
    /// no-op).
    pub fn publish(&self, event: Vec<u8>) {
        let _ = self.events.send(event);
    }

    /// A cheap, cloneable, **synchronous** publish handle. `broadcast::Sender::send` isn't async,
    /// so the daemon can hold a `Publisher` and broadcast coordination events straight from its
    /// (blocking) request handlers while the server's accept loop runs on a background runtime.
    pub fn publisher(&self) -> Publisher {
        Publisher(self.events.clone())
    }
}

/// A detached handle for broadcasting coordination events without holding the [`Server`].
#[derive(Clone)]
pub struct Publisher(broadcast::Sender<Vec<u8>>);

impl Publisher {
    /// Broadcast an event to all current subscribers (best-effort; no subscribers = a no-op).
    pub fn publish(&self, event: Vec<u8>) {
        let _ = self.0.send(event);
    }
}

async fn serve_conn(conn: Connection, store: Store, events: broadcast::Sender<Vec<u8>>) -> Result<()> {
    loop {
        let (send, recv) = match conn.accept_bi().await {
            Ok(s) => s,
            Err(_) => return Ok(()), // connection closed
        };
        let store = store.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let _ = serve_stream(send, recv, store, events).await;
        });
    }
}

async fn serve_stream(
    mut send: SendStream,
    mut recv: RecvStream,
    store: Store,
    events: broadcast::Sender<Vec<u8>>,
) -> Result<()> {
    let mut op = [0u8; 1];
    recv.read_exact(&mut op).await?;
    match op[0] {
        OP_GET => {
            let mut oid = [0u8; 32];
            recv.read_exact(&mut oid).await?;
            // LMDB read is blocking; keep it off the async worker.
            let obj = tokio::task::spawn_blocking(move || store.get(&ObjectId(oid)).ok().flatten())
                .await
                .ok()
                .flatten();
            match obj {
                Some(o) => {
                    let bytes = o.encode();
                    send.write_all(&[1u8]).await?;
                    send.write_all(&(bytes.len() as u64).to_le_bytes()).await?;
                    send.write_all(&bytes).await?;
                }
                None => send.write_all(&[0u8]).await?,
            }
            let _ = send.finish();
        }
        OP_SUBSCRIBE => {
            let mut rx = events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        send.write_all(&(ev.len() as u32).to_le_bytes()).await?;
                        send.write_all(&ev).await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        _ => {}
    }
    Ok(())
}

// ── client ───────────────────────────────────────────────────────────────────────────────────

/// A QUIC client connection to a keel server.
pub struct Client {
    conn: Connection,
}

impl Client {
    /// Connect to a keel QUIC server.
    pub async fn connect(server: SocketAddr) -> Result<Client> {
        let mut endpoint = Endpoint::client("[::]:0".parse()?)?;
        endpoint.set_default_client_config(client_config()?);
        let conn = endpoint.connect(server, "localhost")?.await?;
        Ok(Client { conn })
    }

    /// Fetch an object's canonical bytes by id (`None` if the server doesn't have it). The bytes
    /// hash back to `oid`, so the caller can verify integrity.
    pub async fn get_object(&self, oid: &ObjectId) -> Result<Option<Vec<u8>>> {
        let (mut send, mut recv) = self.conn.open_bi().await?;
        send.write_all(&[OP_GET]).await?;
        send.write_all(&oid.0).await?;
        let _ = send.finish();
        let mut found = [0u8; 1];
        recv.read_exact(&mut found).await?;
        if found[0] == 0 {
            return Ok(None);
        }
        let mut len = [0u8; 8];
        recv.read_exact(&mut len).await?;
        let n = u64::from_le_bytes(len) as usize;
        if n > MAX_FRAME {
            return Err("object length exceeds frame limit".into());
        }
        let mut buf = vec![0u8; n];
        recv.read_exact(&mut buf).await?;
        Ok(Some(buf))
    }

    /// Fetch and decode an object by id, verifying it hashes back to `oid`.
    pub async fn get(&self, oid: &ObjectId) -> Result<Option<Object>> {
        match self.get_object(oid).await? {
            Some(bytes) => {
                let obj = Object::decode(&bytes)?;
                if obj.id() != *oid {
                    return Err("fetched object failed integrity check".into());
                }
                Ok(Some(obj))
            }
            None => Ok(None),
        }
    }

    /// Subscribe to the server's coordination event stream.
    pub async fn subscribe(&self) -> Result<Events> {
        let (mut send, recv) = self.conn.open_bi().await?;
        send.write_all(&[OP_SUBSCRIBE]).await?;
        let _ = send.finish();
        Ok(Events { recv })
    }
}

/// A live event subscription — `next()` yields each coordination event as it arrives.
pub struct Events {
    recv: RecvStream,
}
impl Events {
    /// The next event, or `None` when the stream ends.
    pub async fn next(&mut self) -> Option<Vec<u8>> {
        let mut len = [0u8; 4];
        self.recv.read_exact(&mut len).await.ok()?;
        let n = u32::from_le_bytes(len) as usize;
        let mut buf = vec![0u8; n];
        self.recv.read_exact(&mut buf).await.ok()?;
        Some(buf)
    }
}

// ── TLS plumbing (self-signed server cert; client accepts any — dev) ───────────────────────────

fn server_config() -> Result<(quinn::ServerConfig, Vec<u8>)> {
    use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert_der = CertificateDer::from(cert.cert.der().to_vec());
    let key = PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der());
    let mut crypto = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key.into())?;
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?;
    Ok((quinn::ServerConfig::with_crypto(Arc::new(qsc)), cert_der.to_vec()))
}

fn client_config() -> Result<quinn::ClientConfig> {
    let mut crypto = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![ALPN.to_vec()];
    let qcc = quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?;
    Ok(quinn::ClientConfig::new(Arc::new(qcc)))
}

/// A cert verifier that accepts any server certificate (dev / single-tenant). Production pins the
/// server's cert here.
#[derive(Debug)]
struct AcceptAny;
impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _m: &[u8],
        _c: &rustls::pki_types::CertificateDer<'_>,
        _d: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fetch_object_and_receive_event_over_quic() {
        // a store with one blob
        let dir = std::env::temp_dir().join(format!("keelnet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let content = b"hello over quic".to_vec();
        let oid = store.put(&Object::Blob(content.clone())).unwrap();

        let server = Server::bind("127.0.0.1:0".parse().unwrap(), store).await.unwrap();
        let addr = server.local_addr();
        let client = Client::connect(addr).await.unwrap();

        // content-addressed fetch, verified
        let got = client.get(&oid).await.unwrap().unwrap();
        assert_eq!(got, Object::Blob(content));
        // a miss
        assert!(client.get_object(&ObjectId([9u8; 32])).await.unwrap().is_none());

        // event channel: subscribe, publish, receive
        let mut events = client.subscribe().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await; // let the subscribe land
        server.publish(b"reserve src/auth/login.ts by alice".to_vec());
        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev, b"reserve src/auth/login.ts by alice");
    }

    #[tokio::test]
    async fn client_rejects_oversized_frame_instead_of_aborting() {
        // The client trusts any server cert (`AcceptAny`), so a malicious or MITM'd peer can put an
        // arbitrary u64 length on the wire. Without the MAX_FRAME cap the client would `vec![0u8; n]`
        // and abort the process before reading a byte. It must return an error instead.
        let (cfg, _) = server_config().unwrap();
        let endpoint = Endpoint::server(cfg, "127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = endpoint.local_addr().unwrap();
        tokio::spawn(async move {
            if let Some(incoming) = endpoint.accept().await {
                if let Ok(conn) = incoming.await {
                    if let Ok((mut send, mut recv)) = conn.accept_bi().await {
                        let mut req = [0u8; 33]; // OP_GET + 32-byte oid
                        let _ = recv.read_exact(&mut req).await;
                        let _ = send.write_all(&[1u8]).await; // "found"
                        let _ = send.write_all(&u64::MAX.to_le_bytes()).await; // absurd length
                        let _ = send.finish();
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                }
            }
        });
        let client = Client::connect(addr).await.unwrap();
        let res = client.get_object(&ObjectId([7u8; 32])).await;
        assert!(res.is_err(), "oversized frame must be rejected, got {res:?}");
    }
}
