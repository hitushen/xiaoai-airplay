//! Per-connection state and RTSP request handling.

use super::MAX_NONCE_LEN;
use super::handlers_ap1 as handlers;
use super::rtsp;
use super::types::*;
use crate::crypto::fairplay::FairPlay;
use crate::crypto::pairing::Pairing;
use crate::crypto::rsa::RsaKey;
use crate::net::server::{ConnectionHandler, HttpdCallbacks};
use crate::proto::digest;
use crate::proto::http::{HttpRequest, HttpResponse};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::watch;

/// Shared state passed to each connection.
pub(crate) struct RaopShared {
    pub(crate) rsakey: Arc<RsaKey>,
    pub(crate) pairing: Arc<Pairing>,
    pub(crate) hwaddr: Vec<u8>,
    pub(crate) password: String,
    #[cfg(feature = "pipewire-auth-setup-compat")]
    pub(crate) pipewire_auth_setup_compat: bool,
    pub(crate) handler: Arc<dyn AudioHandler>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_store: Arc<dyn PairingStore>,
    /// Accessory's long-term Ed25519 identity seed (random, persisted via the store).
    #[cfg(feature = "ap2")]
    pub(crate) identity_seed: [u8; 32],
    pub(crate) output_sample_rate: Option<u32>,
    /// Only consulted by the AP2 mixdown path; dead in AP1-only builds.
    #[cfg_attr(not(feature = "ap2"), allow(dead_code))]
    pub(crate) output_max_channels: Option<u8>,
    #[cfg(feature = "ap2")]
    pub(crate) pin: Option<String>,
    #[cfg(feature = "video")]
    pub(crate) video_handler: Option<Arc<dyn crate::raop::video::VideoHandler>>,
    /// Shared video encryption keys — set by audio SETUP, read by video SETUP.
    #[cfg(feature = "video")]
    pub(crate) video_ekey: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "video")]
    pub(crate) video_eiv: Arc<std::sync::RwLock<Option<[u8; 16]>>>,
    #[cfg(feature = "ap2")]
    pub(crate) pairing_id: String,
    /// Accessory device id (`hwaddr_airplay(hwaddr)`), computed once at build.
    #[cfg(feature = "ap2")]
    pub(crate) device_id: String,
    #[cfg(feature = "ap2")]
    pub(crate) airplay_name: String,
    /// Stop-handle for the currently-active audio session. iOS opens parallel
    /// connections (Happy Eyeballs) and switches between them; registering each
    /// new session here — and stopping the previous — keeps only the newest
    /// playout feeding the output (avoids overlapping / post-disconnect audio).
    #[cfg(feature = "ap2")]
    pub(crate) active_audio: std::sync::Mutex<Option<(std::net::IpAddr, Box<dyn FnOnce() + Send>)>>,
    pub(crate) connections: std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, std::collections::HashMap<u64, watch::Sender<bool>>>>,
    pub(crate) next_connection_id: std::sync::atomic::AtomicU64,
    #[cfg(feature = "hls")]
    pub(crate) hls_handler: Option<Arc<dyn crate::raop::hls::HlsHandler>>,
}

impl RaopShared {
    /// Register a newly-started audio session, stopping previous playout and
    /// kicking all control connections from other sender IPs.
    #[cfg(feature = "ap2")]
    pub(crate) fn set_active_audio(&self, remote_ip: std::net::IpAddr, stop: Box<dyn FnOnce() + Send>) {
        let prev = self.active_audio.lock().ok().and_then(|mut g| g.replace((remote_ip, stop)));
        if let Some((_previous_ip, prev)) = prev { prev(); }
        self.takeover_sender(remote_ip);
    }

    /// Keep all control connections from the active sender (iOS can use
    /// parallel Happy Eyeballs/AP2 connections) and kick connections from
    /// other senders.
    pub(crate) fn takeover_sender(&self, keep_ip: std::net::IpAddr) {
        self.kick_other_clients(keep_ip);
    }

    pub(crate) fn register_connection(&self, remote_ip: std::net::IpAddr) -> (u64, watch::Receiver<bool>) {
        let id = self.next_connection_id.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        let (tx, rx) = watch::channel(false);
        if let Ok(mut clients) = self.connections.lock() { clients.entry(remote_ip).or_default().insert(id, tx); }
        (id, rx)
    }

    pub(crate) fn unregister_connection(&self, remote_ip: std::net::IpAddr, id: u64) {
        if let Ok(mut clients) = self.connections.lock() {
            if let Some(by_id) = clients.get_mut(&remote_ip) {
                by_id.remove(&id);
                if by_id.is_empty() { clients.remove(&remote_ip); }
            }
        }
    }

    fn kick_other_clients(&self, keep_ip: std::net::IpAddr) {
        let senders = self
            .connections
            .lock()
            .ok()
            .map(|clients| connection_senders_to_kick(&clients, keep_ip))
            .unwrap_or_default();
        for tx in senders {
            let _ = tx.send(true);
        }
    }
}

fn connection_senders_to_kick(
    clients: &std::collections::HashMap<
        std::net::IpAddr,
        std::collections::HashMap<u64, watch::Sender<bool>>,
    >,
    keep_ip: std::net::IpAddr,
) -> Vec<watch::Sender<bool>> {
    clients
        .iter()
        .filter(|(ip, _)| **ip != keep_ip)
        .flat_map(|(_, by_id)| by_id.values().cloned())
        .collect()
}

#[cfg(test)]
mod takeover_tests {
    use super::*;

    #[test]
    fn takeover_preserves_parallel_current_sender_connections() {
        let current_ip: std::net::IpAddr = "192.0.2.10".parse().unwrap();
        let previous_ip: std::net::IpAddr = "192.0.2.20".parse().unwrap();
        let (current_a, mut current_a_rx) = watch::channel(false);
        let (current_b, mut current_b_rx) = watch::channel(false);
        let (previous_a, mut previous_a_rx) = watch::channel(false);
        let (previous_b, mut previous_b_rx) = watch::channel(false);
        let clients = std::collections::HashMap::from([
            (current_ip, std::collections::HashMap::from([(1, current_a), (2, current_b)])),
            (previous_ip, std::collections::HashMap::from([(3, previous_a), (4, previous_b)])),
        ]);

        for sender in connection_senders_to_kick(&clients, current_ip) {
            sender.send(true).unwrap();
        }

        assert!(!*current_a_rx.borrow_and_update());
        assert!(!*current_b_rx.borrow_and_update());
        assert!(*previous_a_rx.borrow_and_update());
        assert!(*previous_b_rx.borrow_and_update());
    }
}

fn socket_ip_bytes(address: SocketAddr) -> Vec<u8> {
    match address.ip() {
        std::net::IpAddr::V4(ip) => ip.octets().to_vec(),
        std::net::IpAddr::V6(ip) => ip.octets().to_vec(),
    }
}

fn new_raop_connection(
    shared: Arc<RaopShared>,
    local: SocketAddr,
    remote: SocketAddr,
) -> handlers::RaopConnection {
    handlers::RaopConnection {
        raop_rtp: None,
        fairplay: FairPlay::new(),
        pairing: shared.pairing.create_session(),
        local_addr: socket_ip_bytes(local),
        remote_addr: socket_ip_bytes(remote),
        remote_socket: remote,
        nonce: digest::generate_nonce(MAX_NONCE_LEN),
        #[cfg(feature = "ap2")]
        srp_server: None,
        #[cfg(feature = "ap2")]
        pair_verify: None,
        #[cfg(feature = "ap2")]
        ap2_shared_secret: None,
        #[cfg(feature = "ap2")]
        pair_verify_secret: None,
        #[cfg(feature = "ap2")]
        is_ap2: false,
        #[cfg(feature = "ap2")]
        playout_cmd: None,
        #[cfg(feature = "ap2")]
        event_sender: None,
        #[cfg(feature = "video")]
        ekey: None,
        #[cfg(feature = "video")]
        eiv: None,
        #[cfg(feature = "hls")]
        hls_state: crate::raop::hls::HlsState::new(),
        shared,
    }
}

impl HttpdCallbacks for RaopShared {
    fn conn_init(
        self: Arc<Self>,
        local: SocketAddr,
        remote: SocketAddr,
    ) -> Option<Box<dyn ConnectionHandler>> {
        let remote_str = remote.ip().to_string();
        self.handler.on_client_connected(&remote_str);
        let remote_ip = remote.ip();
        let (connection_id, shutdown_rx) = self.register_connection(remote_ip);
        Some(Box::new(RaopConnectionHandler {
            conn: new_raop_connection(self, local, remote),
            remote_addr: remote_str,
            remote_ip,
            connection_id,
            shutdown_rx: Some(shutdown_rx),
            connected_at: std::time::Instant::now(),
            #[cfg(feature = "ap2")]
            cipher: None,
            #[cfg(feature = "ap2")]
            pending_secret: None,
        }))
    }
}

struct RaopConnectionHandler {
    conn: handlers::RaopConnection,
    remote_addr: String,
    remote_ip: std::net::IpAddr,
    connection_id: u64,
    shutdown_rx: Option<watch::Receiver<bool>>,
    /// Connection-start instant, used to log per-request elapsed time for
    /// connect-latency diagnostics (AP2 PTP-sync wait vs AP1 fast path).
    connected_at: std::time::Instant,
    #[cfg(feature = "ap2")]
    cipher: Option<crate::crypto::chacha_transport::EncryptedChannel>,
    #[cfg(feature = "ap2")]
    pending_secret: Option<Vec<u8>>,
}

impl Drop for RaopConnectionHandler {
    fn drop(&mut self) {
        self.conn.shared.unregister_connection(self.remote_ip, self.connection_id);
        self.conn.shared.handler.on_client_disconnected(&self.remote_addr);
    }
}

impl ConnectionHandler for RaopConnectionHandler {
    fn take_shutdown_receiver(&mut self) -> Option<watch::Receiver<bool>> {
        self.shutdown_rx.take()
    }

    #[cfg(feature = "pipewire-auth-setup-compat")]
    fn request_body_policy(
        &self,
        request: &HttpRequest,
    ) -> Result<crate::net::server::RequestBodyPolicy, crate::error::ProtocolError> {
        rtsp::request_body_policy(&self.conn, request)
    }

    fn conn_request(&mut self, request: &HttpRequest) -> HttpResponse {
        // Connect-latency timeline: one line per RTSP request, elapsed since the
        // connection opened. `/feedback` is a ~2s keep-alive heartbeat, so it drops
        // to `debug` to keep the connect sequence readable; everything else at info.
        let elapsed_ms = self.connected_at.elapsed().as_millis() as u64;
        let method = request.method().unwrap_or("");
        let url = request.url().unwrap_or("");
        if url == "/feedback" {
            tracing::debug!(elapsed_ms, method, url, "RTSP request");
        } else {
            tracing::info!(elapsed_ms, method, url, "RTSP request");
        }
        let resp = rtsp::dispatch(&mut self.conn, request);

        // Queue encryption activation for AFTER this response is sent
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = &self.conn.ap2_shared_secret
        {
            self.pending_secret = Some(secret.clone());
        }

        resp
    }

    fn is_encrypted(&self) -> bool {
        #[cfg(feature = "ap2")]
        {
            self.cipher.is_some()
        }
        #[cfg(not(feature = "ap2"))]
        {
            false
        }
    }

    fn after_response(&mut self) {
        #[cfg(feature = "ap2")]
        if self.cipher.is_none()
            && let Some(secret) = self.pending_secret.take()
        {
            tracing::debug!(
                secret_len = secret.len(),
                "Activating cipher from pending_secret"
            );
            match crate::crypto::chacha_transport::EncryptedChannel::control(&secret) {
                Ok(ch) => {
                    tracing::info!("Encrypted RTSP transport activated");
                    self.cipher = Some(ch);
                }
                Err(e) => tracing::warn!("Failed to create cipher: {e}"),
            }
        }
    }

    fn decrypt_incoming(&mut self, data: &[u8]) -> Option<(Vec<u8>, usize)> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            return ch.decrypt_ctx.decrypt(data).ok();
        }
        Some((data.to_vec(), data.len()))
    }

    fn encrypt_outgoing(&mut self, data: &[u8]) -> Vec<u8> {
        #[cfg(feature = "ap2")]
        if let Some(ch) = &mut self.cipher {
            // Once the channel is encrypted the peer expects ciphertext; never
            // fall back to emitting plaintext (which would leak the response and
            // desync the stream). On the practically-impossible AEAD encrypt
            // failure, return no bytes so the connection tears down instead.
            return ch.encrypt_ctx.encrypt(data).unwrap_or_else(|e| {
                tracing::warn!("Outgoing encryption failed; dropping response: {e}");
                Vec::new()
            });
        }
        data.to_vec()
    }
}

// On drop, RTP session is cleaned up automatically (RaopRtp dropped → shutdown sent)
