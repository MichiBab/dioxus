//! The internal edit queue facilitating native <-> webview communication.
//!
//! Originally, we used long-polling on the wry custom protocol to send edits to the webview.
//! Due to bugs in wry on android, we switched to a websocket connection that the webview connects to.
//! We use the sledgehammer crate to build batches of edits and send them through the websocket to
//! the webview.
//!
//! Using a websocket lets us send binary data to the webview quite efficiently and does encounter
//! many of the issues with regular request/response protocols. Note that the websocket max frame
//! size is quite large (9.22 exabytes), so we can have very large batches without issue.
//!
//! Using websockets does mean we need to handle security and content security policies ourselves.
//! The code here generates a random key that the webview must use to connect to the websocket.
//! We use the initialization script API to setup the websocket connection without leaking the key
//! to the webview itself in case there's untrusted content in the webview.
//!
//! Some operating systems (like iOS) will kill the websocket connection when the device goes to sleep.
//! If this happens, we will automatically switch to a new port and notify the webview of the new location
//! and key. The webview will then reconnect to the new port and continue receiving edits.

use dioxus_interpreter_js::MutationState;
use futures_channel::oneshot;
use futures_util::FutureExt;
use rand::{RngCore, SeedableRng};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::{TcpListener, TcpStream};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::AtomicU32;
use std::sync::Mutex;
use std::time::Instant;
use std::{
    net::IpAddr,
    sync::{Arc, RwLock},
};
use tokio::sync::Notify;

/// This handles communication between the requests that the webview makes and the interpreter.
#[derive(Clone)]
pub(crate) struct WryQueue {
    inner: Rc<RefCell<WryQueueInner>>,
}

impl WryQueue {
    pub(crate) fn with_mutation_state_mut<O: 'static>(
        &self,
        callback: impl FnOnce(&mut MutationState) -> O,
    ) -> O {
        let mut inner = self.inner.borrow_mut();
        callback(&mut inner.mutation_state)
    }

    /// Send a list of mutations to the webview.
    /// Returns `true` if the edit channel is dead (message queued for reconnect).
    pub(crate) fn send_edits(&self) -> bool {
        let mut myself = self.inner.borrow_mut();
        let webview_id = myself.location.webview_id;
        let serialized_edits: Arc<[u8]> = myself.mutation_state.export_memory().into();
        let batch_id = myself.next_batch_id;
        myself.next_batch_id = batch_id
            .checked_add(1)
            .expect("Mutation sequence exhausted");
        #[cfg(any(target_os = "android", target_os = "ios"))]
        eprintln!(
            "[MUTATION_PERF] Rust sending mutation batch: {} bytes",
            serialized_edits.len()
        );
        let receiver = myself
            .websocket
            .send_edits(webview_id, batch_id, serialized_edits.clone());
        myself.unacknowledged_batch = Some((batch_id, serialized_edits));
        let channel_dead = myself.websocket.is_pending(webview_id);
        myself.edits_in_progress = Some(receiver);
        myself.edit_sent_at = Some(Instant::now());
        channel_dead
    }

    /// Check if the current in-progress edit has been pending long enough to
    /// indicate the WebSocket is dead. Returns `true` if a reconnect should be
    /// triggered (debounced: at most once per 3 s). Does NOT clear
    /// `edits_in_progress` — the edit will be re-sent through the new connection
    /// and the original oneshot will resolve naturally.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub(crate) fn has_stale_edit(&self) -> bool {
        const STALE_THRESHOLD: std::time::Duration = std::time::Duration::from_secs(3);
        const RECONNECT_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);
        match self.inner.try_borrow_mut() {
            Ok(mut inner) => {
                if inner.suspended {
                    return false;
                }
                if let Some(sent_at) = inner.edit_sent_at {
                    if inner.edits_in_progress.is_some() && sent_at.elapsed() >= STALE_THRESHOLD {
                        // Debounce: don't spam JS reconnects
                        if let Some(last) = inner.reconnect_triggered_at {
                            if last.elapsed() < RECONNECT_COOLDOWN {
                                return false;
                            }
                        }
                        eprintln!(
                            "[EDITS] Stale ACK detected ({}ms) — triggering WS reconnect",
                            sent_at.elapsed().as_millis()
                        );
                        inner.reconnect_triggered_at = Some(Instant::now());
                        return true;
                    }
                }
            }
            Err(_) => {} // mid-render re-entrancy — skip
        }
        false
    }

    /// Wait until all pending edits have been rendered in the webview
    pub(crate) fn poll_edits_flushed(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let mut self_mut = self.inner.borrow_mut();
        if let Some(receiver) = self_mut.edits_in_progress.as_mut() {
            match receiver.poll_unpin(cx) {
                std::task::Poll::Ready(Ok(())) => {
                    // ACK received — clear so check_and_reset_stale_edit
                    // doesn't mistake this resolved receiver for a stale one.
                    #[cfg(any(target_os = "android", target_os = "ios"))]
                    if let Some(sent_at) = self_mut.edit_sent_at {
                        eprintln!(
                            "[MUTATION_PERF] Rust received mutation ACK after {:.2}ms",
                            sent_at.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                    self_mut.edits_in_progress = None;
                    self_mut.edit_sent_at = None;
                    self_mut.unacknowledged_batch = None;
                    std::task::Poll::Ready(())
                }
                std::task::Poll::Ready(Err(_)) => {
                    // A failed sender is not evidence that the DOM was updated.
                    // Retain/replay the same batch ID, including after worker failure.
                    let (id, bytes) = self_mut.unacknowledged_batch.as_ref().unwrap().clone();
                    let webview = self_mut.location.webview_id;
                    self_mut.edits_in_progress =
                        Some(self_mut.websocket.send_edits(webview, id, bytes));
                    tracing::warn!("Lost mutation ACK sender; replaying batch {id}");
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            }
        } else {
            std::task::Poll::Ready(())
        }
    }

    /// Lifecycle changes do not complete edits. Only their matching ACK does.
    #[cfg(any(target_os = "android", target_os = "ios"))]
    pub(crate) fn set_suspended(&self, suspended: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.suspended = suspended;
        inner.reconnect_triggered_at = None;
        if !suspended && inner.edits_in_progress.is_some() {
            inner.edit_sent_at = Some(Instant::now());
        }
    }

    /// Check if there is a new location for the websocket edits server.
    pub(crate) fn poll_new_edits_location(
        &self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let mut self_mut = self.inner.borrow_mut();
        let poll = self_mut
            .server_location_changed_future
            .as_mut()
            .poll_unpin(cx);
        if poll.is_ready() {
            // If the future is ready, we need to reset it to wait for the next change
            self_mut.server_location_changed_future =
                owned_notify_future(self_mut.server_location_changed.clone());
        }
        poll
    }

    /// Get the websocket path that the webview should connect to in order to receive edits
    pub(crate) fn edits_path(&self) -> String {
        let WebviewWebsocketLocation {
            webview_id, server, ..
        } = &self.inner.borrow().location;
        let server = server.lock().unwrap();
        let port = server.port;
        let key = &server.client_key;
        let key_hex = encode_key_string(key);
        format!("ws://127.0.0.1:{port}/{webview_id}/{key_hex}")
    }

    /// Get the key the client should expect from the server when connecting to the websocket.
    pub(crate) fn required_server_key(&self) -> String {
        let server = &self.inner.borrow().location.server;
        let server = server.lock().unwrap();
        encode_key_string(&server.server_key)
    }
}

pub(crate) struct WryQueueInner {
    location: WebviewWebsocketLocation,
    websocket: EditWebsocket,
    // If this webview is currently waiting for an edit to be flushed. We don't run the virtual dom while this is true to avoid running effects before the dom has been updated
    edits_in_progress: Option<oneshot::Receiver<()>>,
    /// When the current edit batch was dispatched (for stale ACK detection)
    edit_sent_at: Option<Instant>,
    next_batch_id: u64,
    unacknowledged_batch: Option<(u64, Arc<[u8]>)>,
    #[cfg(any(target_os = "android", target_os = "ios"))]
    suspended: bool,
    /// When we last triggered a JS WebSocket reconnect (debounce)
    #[cfg(any(target_os = "android", target_os = "ios"))]
    reconnect_triggered_at: Option<Instant>,
    // The socket may be killed by the OS while running. If it does, this channel will receive the new server location
    server_location_changed: Arc<Notify>,
    server_location_changed_future: Pin<Box<dyn Future<Output = ()>>>,
    mutation_state: MutationState,
}

/// The location of a webview websocket connection. This is used to identify the webview and the port it is connected to.
#[derive(Clone)]
pub(crate) struct WebviewWebsocketLocation {
    /// The id of the webview that this websocket is connected to
    webview_id: u32,
    server: Arc<Mutex<ServerLocation>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ServerLocation {
    /// The port the websocket is on
    port: u16,
    /// A key that every websocket connection that originates from this application will use to identify itself.
    /// We use this to make sure no external applications can connect to our websocket and receive UI updates.
    client_key: [u8; KEY_SIZE],
    /// The key that the server must respond with for the client to connect to the websocket
    server_key: [u8; KEY_SIZE],
}

/// Start a new server on an available port on localhost. Return the server location and the TCP listener that is bound to the port.
pub(crate) fn start_server() -> std::io::Result<(ServerLocation, TcpListener)> {
    let client_key = create_secure_key();
    let server_key = create_secure_key();
    let server = TcpListener::bind((IpAddr::from([127, 0, 0, 1]), 0))?;
    let port = server.local_addr()?.port();
    let location = ServerLocation {
        port,
        client_key,
        server_key,
    };
    Ok((location, server))
}

/// The websocket listener that the webview will connect to in order to receive edits and send requests. There
/// is only one websocket listener per application even if there are multiple windows so we don't use all the
/// open ports.
#[derive(Clone)]
pub(crate) struct EditWebsocket {
    current_location: Arc<Mutex<ServerLocation>>,
    max_webview_id: Arc<AtomicU32>,
    connections: Arc<RwLock<HashMap<u32, WebviewConnectionState>>>,
    server_location: Arc<Notify>,
}

impl EditWebsocket {
    pub(crate) fn start() -> Self {
        let connections = Arc::new(RwLock::new(HashMap::new()));

        let notify = Arc::new(Notify::new());
        let (location, server) = start_server().expect("Failed to bind initial edit socket");
        let current_location = Arc::new(Mutex::new(location));

        let connections_ = connections.clone();
        let current_location_ = current_location.clone();
        let notify_ = notify.clone();
        std::thread::spawn(move || {
            Self::accept_loop(notify_, server, current_location_, connections_)
        });

        Self {
            connections,
            max_webview_id: Default::default(),
            current_location,
            server_location: notify,
        }
    }

    /// Accepts incoming websocket connections and handles them in a loop.
    ///
    /// New sockets are accepted and then put in to a new thread to handle the connection.
    /// This is implemented using traditional sync code to allow us to be independent of the async runtime.
    fn accept_loop(
        notify: Arc<Notify>,
        mut server: TcpListener,
        current_location: Arc<Mutex<ServerLocation>>,
        connections: Arc<RwLock<HashMap<u32, WebviewConnectionState>>>,
    ) {
        loop {
            // Accept connections until we hit an error
            while let Ok((stream, _)) = server.accept() {
                // A half-open handshake must not block the listener for every webview.
                let location = current_location.clone();
                let connections = connections.clone();
                std::thread::spawn(move || Self::handle_connection(stream, location, connections));
            }

            // Switch ports and reconnect on a different port if the server is killed by the OS. This
            // will happen if an IOS device goes to sleep
            //
            // For security, it is important that the keys are also regenerated when the server is restarted.
            // The client may try to reconnect to the old port that is now being used by an attacker who steals the client
            // key and uses it to read the edits from the new port.
            let (location, new_server) = loop {
                match start_server() {
                    Ok(server) => break server,
                    Err(error) => {
                        tracing::warn!("Edit listener restart failed; retrying: {error}");
                        std::thread::sleep(std::time::Duration::from_millis(500));
                    }
                }
            };
            *current_location.lock().unwrap() = location;
            server = new_server;
            notify.notify_waiters();
        }
    }

    fn handle_connection(
        stream: TcpStream,
        server_location: Arc<Mutex<ServerLocation>>,
        connections: Arc<RwLock<HashMap<u32, WebviewConnectionState>>>,
    ) {
        use tungstenite::handshake::server::{Request, Response};

        // Bound handshake, authentication, write, and ACK waits on this OS worker.
        // Neither the renderer nor the Android UI thread waits on this socket.
        let timeout = Some(std::time::Duration::from_secs(3));
        if let Err(error) = stream
            .set_read_timeout(timeout)
            .and_then(|_| stream.set_write_timeout(timeout))
        {
            tracing::warn!("Could not bound edit socket I/O: {error}");
            return;
        }

        let current_server_location = { *server_location.lock().unwrap() };
        let hex_encoded_client_key = encode_key_string(&current_server_location.client_key);
        let hex_encoded_server_key = encode_key_string(&current_server_location.server_key);
        let mut location = None;
        let mut connection_generation = 0;

        #[allow(clippy::result_large_err)]
        let on_request = |req: &Request, res| {
            connection_generation = req
                .uri()
                .query()
                .and_then(|query| query.strip_prefix("generation="))
                .and_then(|generation| generation.parse::<u64>().ok())
                .unwrap_or(0);
            // Try to parse the webview id and key from the path
            let path = req.uri().path();

            // The path should have two parts `/webview_id/key`
            let mut segments = path.trim_matches('/').split('/');
            let webview_id = segments
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| {
                    Response::builder()
                        .status(400)
                        .body(Some("Bad Request: Invalid webview ID".to_string()))
                        .unwrap()
                })?;
            let key = segments.next().ok_or_else(|| {
                Response::builder()
                    .status(400)
                    .body(Some("Bad Request: Missing key".to_string()))
                    .unwrap()
            })?;

            // Make sure the key matches the expected key.
            // VERY IMPORTANT: We cannot use normal string comparison here because it reveals information
            // about the key based on timing information. Instead we use a constant time comparison method.
            let key_matches: bool =
                subtle::ConstantTimeEq::ct_eq(hex_encoded_client_key.as_ref(), key.as_bytes())
                    .into();
            if !key_matches {
                return Err(Response::builder()
                    .status(403)
                    .body(Some("Forbidden: Invalid key".to_string()))
                    .unwrap());
            }

            location = Some(WebviewWebsocketLocation {
                webview_id,
                server: server_location,
            });

            Ok(res)
        };

        // Accept the websocket connection while reading the path and setting the location
        let mut websocket = match tungstenite::accept_hdr(stream, on_request) {
            Ok(ws) => ws,
            Err(e) => {
                tracing::error!("Error accepting websocket connection: {}", e);
                return;
            }
        };

        // Immediately send the key to authenticate the server
        if let Err(error) =
            websocket.send(tungstenite::Message::Text(hex_encoded_server_key.into()))
        {
            tracing::warn!("Could not authenticate edit socket: {error}");
            return;
        }

        let location = match location {
            Some(loc) => loc,
            None => {
                tracing::error!("WebSocket connection without a valid webview ID");
                return;
            }
        };

        // Handle the websocket connection in a separate thread
        let (edits_outgoing, edits_incoming_rx) = std::sync::mpsc::channel::<MsgPair>();

        // Parallel handshakes may finish out of order. An old socket must never
        // replace the current one (including one already back in Pending).
        let mut guard = connections.write().unwrap();
        if guard
            .get(&location.webview_id)
            .is_some_and(|state| state.generation() >= connection_generation)
        {
            tracing::warn!(
                "Ignoring superseded edit connection for webview {}",
                location.webview_id
            );
            return;
        }
        let mut connected = WebviewConnectionState::Connected {
            generation: connection_generation,
            edits_outgoing,
            socket: match websocket.get_ref().try_clone() {
                Ok(socket) => socket,
                Err(error) => {
                    tracing::warn!("Could not retain edit socket shutdown handle: {error}");
                    return;
                }
            },
        };
        match guard.remove(&location.webview_id) {
            Some(WebviewConnectionState::Pending { pending, .. }) => {
                for pair in pending {
                    connected.add_message_pair(pair);
                }
            }
            Some(WebviewConnectionState::Connected { socket, .. }) => {
                // Wake an old worker waiting for its ACK immediately, rather
                // than making the new socket wait for its I/O timeout.
                let _ = socket.shutdown(std::net::Shutdown::Both);
            }
            None => {}
        }
        guard.insert(location.webview_id, connected);
        drop(guard);

        let connections_ = connections.clone();
        // Spawn a task to handle the websocket connection
        std::thread::spawn(move || {
            let mut queued_message = None;
            // Wait until there are edits ready to send
            'connection: while let Ok(msg) = edits_incoming_rx.recv() {
                let data = msg.frame();
                let batch_id = msg.batch_id;
                queued_message = Some(msg);
                // Send the edits to the webview
                if let Err(e) = websocket.send(tungstenite::Message::Binary(data.into())) {
                    tracing::error!("Error sending edits to webview: {}", e);
                    break 'connection;
                }

                // Wait for the webview to ACK the edits.
                // Properly distinguish ACK (binary), close, timeout, and errors.
                let got_ack = loop {
                    match websocket.read() {
                        Ok(tungstenite::Message::Binary(ack)) => {
                            if ack.as_ref() == batch_id.to_le_bytes() {
                                break true;
                            }
                            tracing::warn!("Ignoring mismatched mutation ACK; expected={batch_id}");
                        }
                        Ok(tungstenite::Message::Close(_)) => break false,
                        Ok(_) => continue,
                        // Timeout / WouldBlock from SO_RCVTIMEO — WS is likely dead
                        Err(tungstenite::Error::Io(ref e))
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut =>
                        {
                            break false
                        }
                        // EINTR — just retry
                        Err(tungstenite::Error::Io(ref e))
                            if e.kind() == std::io::ErrorKind::Interrupted =>
                        {
                            continue
                        }
                        Err(_) => break false,
                    }
                };

                if !got_ack {
                    // Unknown whether applied. Replay the SAME ID; JS deduplicates it.
                    break 'connection;
                }

                let msg = queued_message.take().expect("Message should be set here");

                // Notify that the edits have been applied
                if msg.response.send(()).is_err() {
                    tracing::error!("Error sending edits applied notification");
                }
            }
            tracing::trace!("Webview {} handler exiting", location.webview_id);

            // Drop the receiver *before* touching the connections map.
            // This ensures that if the Connected state still holds OUR sender,
            // any attempt to send through it will fail (receiver gone),
            // correctly triggering the Pending conversion inside
            // add_message_pair instead of silently buffering the message
            // into a channel that is about to be destroyed.
            // Hold the map lock while draining and dropping the receiver so a
            // producer cannot enqueue between the drain and disconnection.
            let mut guard = connections_.write().unwrap();
            let mut unsent: VecDeque<_> = queued_message.into_iter().collect();
            unsent.extend(edits_incoming_rx.try_iter());
            drop(edits_incoming_rx);
            for msg in unsent {
                guard
                    .entry(location.webview_id)
                    .or_default()
                    .add_message_pair(msg);
            }
        });
    }

    pub(crate) fn create_queue(&self) -> WryQueue {
        let webview_id = self
            .max_webview_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let server = self.current_location.clone();
        let server_location = self.server_location.clone();
        WryQueue {
            inner: Rc::new(RefCell::new(WryQueueInner {
                server_location_changed: server_location.clone(),
                server_location_changed_future: owned_notify_future(server_location),
                location: WebviewWebsocketLocation { webview_id, server },
                websocket: self.clone(),
                edits_in_progress: None,
                edit_sent_at: None,
                next_batch_id: 1,
                unacknowledged_batch: None,
                #[cfg(any(target_os = "android", target_os = "ios"))]
                suspended: false,
                #[cfg(any(target_os = "android", target_os = "ios"))]
                reconnect_triggered_at: None,
                mutation_state: MutationState::default(),
            })),
        }
    }

    fn send_edits(
        &mut self,
        webview: u32,
        batch_id: u64,
        edits: impl Into<Arc<[u8]>>,
    ) -> oneshot::Receiver<()> {
        let mut connections_mut = self.connections.write().unwrap();
        let connection = connections_mut.entry(webview).or_default();
        connection.add_message(batch_id, edits.into())
    }

    /// Check if the connection for a given webview is in the Pending state
    /// (i.e. the handler thread exited and no live WebSocket connection exists).
    fn is_pending(&self, webview: u32) -> bool {
        let connections = self.connections.read().unwrap();
        matches!(
            connections.get(&webview),
            Some(WebviewConnectionState::Pending { .. }) | None
        )
    }
}

/// The state of a webview websocket connection. This may be pending while the webview is booting.
/// If it is, we queue up edits until the webview is ready to receive them.
enum WebviewConnectionState {
    Pending {
        generation: u64,
        pending: VecDeque<MsgPair>,
    },
    Connected {
        generation: u64,
        socket: TcpStream,
        edits_outgoing: std::sync::mpsc::Sender<MsgPair>,
    },
}

impl Default for WebviewConnectionState {
    fn default() -> Self {
        WebviewConnectionState::Pending {
            generation: 0,
            pending: VecDeque::new(),
        }
    }
}

impl WebviewConnectionState {
    fn generation(&self) -> u64 {
        match self {
            Self::Pending { generation, .. } | Self::Connected { generation, .. } => *generation,
        }
    }

    /// Add a message to the active connection or queue and return a receiver that will be resolved
    /// when the webview has applied the edits.
    fn add_message(&mut self, batch_id: u64, edits: Arc<[u8]>) -> oneshot::Receiver<()> {
        let (response_sender, response_receiver) = oneshot::channel();
        let pair = MsgPair {
            batch_id,
            edits,
            response: response_sender,
        };
        self.add_message_pair(pair);
        response_receiver
    }

    /// Add a message pair to the connection state. The receiver in the message pair will be resolved
    /// when the webview has applied the edits.
    fn add_message_pair(&mut self, pair: MsgPair) {
        match self {
            WebviewConnectionState::Pending { pending: queue, .. } => {
                queue.push_back(pair);
            }
            WebviewConnectionState::Connected {
                edits_outgoing,
                generation,
                ..
            } => {
                // If the handler thread has exited (receiver dropped), the send
                // fails.  Recover the message and convert to Pending so it can
                // be forwarded through the next connection instead of being lost.
                if let Err(std::sync::mpsc::SendError(pair)) = edits_outgoing.send(pair) {
                    tracing::warn!("Edit channel dead — re-queuing message as Pending");
                    *self = WebviewConnectionState::Pending {
                        generation: *generation,
                        pending: VecDeque::from([pair]),
                    };
                }
            }
        }
    }
}

struct MsgPair {
    batch_id: u64,
    edits: Arc<[u8]>,
    response: oneshot::Sender<()>,
}

impl MsgPair {
    fn frame(&self) -> Vec<u8> {
        let mut frame = Vec::with_capacity(8 + self.edits.len());
        frame.extend_from_slice(&self.batch_id.to_le_bytes());
        frame.extend_from_slice(&self.edits);
        frame
    }
}

const KEY_SIZE: usize = 256;
type EncodedKey = [u8; KEY_SIZE];

/// Base64 encode the key to a string to be used in the websocket URL.
fn encode_key_string(key: &EncodedKey) -> String {
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE, key)
}

/// Create a secure key for the websocket connection.
/// Returns the key as a byte array and a hex-encoded string representation of the key.
fn create_secure_key() -> EncodedKey {
    // Helper function to assert that the RNG is a CryptoRng - make sure we use a secure RNG
    fn assert_crypto_random<R: rand::CryptoRng>(val: R) -> R {
        val
    }

    let mut secure_rng = assert_crypto_random(rand::rngs::StdRng::from_os_rng());
    let mut expected_key: EncodedKey = [0u8; KEY_SIZE];
    secure_rng.fill_bytes(&mut expected_key);
    expected_key
}

#[test]
fn test_key_encoding_length() {
    let mut rand = rand::rngs::StdRng::from_os_rng();
    for _ in 0..100 {
        let mut key: EncodedKey = [0u8; KEY_SIZE];
        rand.fill_bytes(&mut key);
        let encoded = encode_key_string(&key);
        // The encoded key length should be the same regardless of the value of the key
        assert_eq!(encoded.len(), 344);
    }
}

// Take an Arc<Notify> and create a future that waits for the notify to be triggered.
fn owned_notify_future(notify: Arc<Notify>) -> Pin<Box<dyn Future<Output = ()>>> {
    let mut notify_owned = Box::pin(async move {
        let notified = notify.notified();

        // The future should be after this statement once it is polled bellow
        tokio::task::yield_now().await;
        notified.await;
    });

    // Start tracking notify before the output future is polled
    _ = (&mut notify_owned).now_or_never();
    notify_owned
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use std::time::Duration;
    use tungstenite::{Message, WebSocket};

    fn connect(queue: &WryQueue, generation: u64) -> WebSocket<TcpStream> {
        let path = format!("{}?generation={generation}", queue.edits_path());
        let port = queue.inner.borrow().location.server.lock().unwrap().port;
        let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let (mut ws, _) = tungstenite::client(path, stream).unwrap();
        assert_eq!(
            ws.read().unwrap().into_text().unwrap(),
            queue.required_server_key()
        );
        ws
    }

    #[test]
    fn lost_ack_replays_same_batch_and_keeps_original_barrier() {
        let mut server = EditWebsocket::start();
        let queue = server.create_queue();
        let mut ack = server.send_edits(0, 1, vec![11, 22]);
        let mut first = connect(&queue, 1);
        let frame = first.read().unwrap().into_data();
        assert_eq!(&frame[..8], &1u64.to_le_bytes());
        assert_eq!(&frame[8..], &[11, 22]);
        assert!(ack.try_recv().unwrap().is_none());

        // Keep the old TCP socket open, like a frozen Android WebView. The
        // replacement must wake its worker without waiting for the ACK timeout.
        let mut replacement = connect(&queue, 2);
        assert_eq!(replacement.read().unwrap().into_data(), frame);
        assert!(ack.try_recv().unwrap().is_none());
        replacement
            .send(Message::Binary(999u64.to_le_bytes().to_vec().into()))
            .unwrap();
        assert!(ack.try_recv().unwrap().is_none());
        replacement
            .send(Message::Binary(1u64.to_le_bytes().to_vec().into()))
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while ack.try_recv().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "matching ACK did not release render barrier"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ack2 = server.send_edits(0, 2, vec![33]);
        let next = replacement.read().unwrap().into_data();
        assert_eq!(&next[..8], &2u64.to_le_bytes());
        assert_eq!(&next[8..], &[33]);
        replacement
            .send(Message::Binary(2u64.to_le_bytes().to_vec().into()))
            .unwrap();
    }

    #[test]
    fn silent_handshake_does_not_block_other_webviews() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        let port = queue.inner.borrow().location.server.lock().unwrap().port;
        let _silent_client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let start = Instant::now();
        let _working_client = connect(&queue, 1);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn dropped_ack_sender_requeues_without_releasing_barrier() {
        let server = EditWebsocket::start();
        let queue = server.create_queue();
        queue.send_edits();
        // Simulate an edit worker losing ownership of its response sender.
        server.connections.write().unwrap().remove(&0);
        let mut cx = std::task::Context::from_waker(futures_util::task::noop_waker_ref());
        assert!(queue.poll_edits_flushed(&mut cx).is_pending());
        let connections = server.connections.read().unwrap();
        let WebviewConnectionState::Pending { pending, .. } = &connections[&0] else {
            panic!("edit was not requeued")
        };
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].batch_id, 1);
    }

    #[test]
    fn stale_handshake_cannot_replace_newer_socket() {
        let mut server = EditWebsocket::start();
        let queue = server.create_queue();
        let mut current = connect(&queue, 2);
        let _late_old = connect(&queue, 1);
        let _ack = server.send_edits(0, 1, vec![7]);
        let frame = current.read().unwrap().into_data();
        assert_eq!(&frame[8..], &[7]);
        current
            .send(Message::Binary(1u64.to_le_bytes().to_vec().into()))
            .unwrap();
    }
}
