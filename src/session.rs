//! Companion Link session: pairing, verification and the encrypted control
//! channel, including legacy Rapport and TVRemoteCore input messages.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, info};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::crypto::{hkdf_expand, CompanionCipher};
use crate::delegate::{RemoteInput, TrackpadMode};
use crate::error::{Error, Result};
use crate::opack::{self, Value};
use crate::pairing::PairingState;
use crate::remote_keyboard::{KeyboardField, KeyboardInput, KeyboardSession};
use crate::remote_media;
use crate::srp::SrpServer;
use crate::tlv::{self, tags};
use crate::{FrameType, PlaybackAction, PlaybackState, VideoSource};

pub const DEVICE_MODEL: &str = "AppleTV14,1";
pub(crate) const SOURCE_VERSION: &str = "715.2";
const REMOTE_SERVICE: &str = "com.apple.tvremoteservices";
const DEFAULT_TOUCH_SIZE: f64 = 1000.0;
const SCROLL_STRIP_START: f64 = 0.85;
const SWIPE_MIN_DISTANCE: f64 = 150.0; // touchpad units; the surface is 1000x1000
const TAP_MAX_DISTANCE: f64 = 60.0; // below this a touch sequence counts as a tap
const MULTI_TAP_WINDOW: Duration = Duration::from_millis(500);
const SELECT_ECHO_WINDOW: Duration = Duration::from_millis(90);
const RIGHT_CLICK_HOLD: Duration = Duration::from_millis(500);
const RIGHT_CLICK_MOVE_DISTANCE: f64 = 40.0;
const DRAG_HOLD: Duration = Duration::from_millis(160);
const DRAG_IDLE_LIMIT: Duration = Duration::from_secs(10);

struct PendingVerification {
    shared: [u8; 32],
    client_public: [u8; 32],
    server_public: [u8; 32],
}

fn hid_command_name(command: u64) -> Option<&'static str> {
    Some(match command {
        1 => "up",
        2 => "down",
        3 => "left",
        4 => "right",
        5 => "menu",
        6 => "select",
        7 => "home",
        8 => "volume_up",
        9 => "volume_down",
        10 => "siri",
        11 => "screensaver",
        12 => "sleep",
        13 => "wake",
        14 => "play_pause",
        15 => "channel_up",
        16 => "channel_down",
        17 => "guide",
        // iOS 26/27 repurposes the old PageUp/PageDown wire codes for the
        // Control Center remote's Mute and Power buttons.
        18 => "mute",
        19 => "power",
        _ => return None,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TouchPhase {
    Began,
    Moved,
    Ended,
    Cancelled,
}

struct ActiveTouch {
    origin: (f64, f64),
    last: (f64, f64),
    started: TapTime,
    updated: Instant,
    distance: f64,
    click_count: u8,
    drag_attempted: bool,
    dragging: bool,
    right_clicked: bool,
    scrolling: bool,
}

impl ActiveTouch {
    fn can_right_click(&self) -> bool {
        self.click_count == 1 && !self.right_clicked && self.distance < RIGHT_CLICK_MOVE_DISTANCE
    }

    fn drag_pending(&self) -> bool {
        self.click_count == 2 && !self.drag_attempted && !self.dragging && !self.right_clicked
    }

    fn start_drag(&mut self, input: &dyn RemoteInput) {
        self.drag_attempted = true;
        self.dragging = input.start_drag();
        if self.dragging && self.last != self.origin {
            input.move_drag(self.last.0 - self.origin.0, self.last.1 - self.origin.1);
        }
    }
}

#[derive(Clone, Copy)]
struct TapTime {
    received: Instant,
    phone_ns: Option<u64>,
}

impl TapTime {
    fn elapsed_since(self, previous: Self) -> Duration {
        if let (Some(previous), Some(current)) = (previous.phone_ns, self.phone_ns) {
            if let Some(elapsed) = current.checked_sub(previous).filter(|elapsed| *elapsed > 0) {
                return Duration::from_nanos(elapsed);
            }
        }
        self.received.saturating_duration_since(previous.received)
    }

    fn follows(self, previous: Self, within: Duration) -> bool {
        self.elapsed_since(previous) < within
    }
}

struct Click {
    time: TapTime,
    sources: u8,
    count: u8,
}

enum SelectPress {
    Down { started: Instant, count: u8 },
    Dragging(Instant),
    Consumed,
}

fn select_hold(count: u8) -> Option<Duration> {
    match count {
        1 => Some(RIGHT_CLICK_HOLD),
        2 => Some(DRAG_HOLD),
        _ => None,
    }
}

struct SeekHold {
    action: PlaybackAction,
    next: Instant,
    expires: Instant,
    active: Arc<()>,
}

enum MediaCommand {
    Button(&'static str),
    Playback(VideoSource, PlaybackAction),
}

struct QueuedMediaCommand {
    command: MediaCommand,
    expires: Instant,
    seek: Option<Weak<()>>,
}

struct MediaCommands {
    sender: mpsc::Sender<QueuedMediaCommand>,
    worker: tokio::task::JoinHandle<()>,
}

impl MediaCommands {
    fn start(input: Arc<dyn RemoteInput>) -> Self {
        let (sender, mut receiver) = mpsc::channel::<QueuedMediaCommand>(16);
        let worker = tokio::spawn(async move {
            // A browser can stall in Accessibility calls. Run these in order
            // on a separate lane so a slow action cannot hold up input frames.
            let mut work = JoinSet::new();
            while let Some(command) = receiver.recv().await {
                let input = input.clone();
                work.spawn_blocking(move || {
                    if Instant::now() >= command.expires
                        || command.seek.is_some_and(|hold| hold.strong_count() == 0)
                    {
                        return;
                    }
                    match command.command {
                        MediaCommand::Button(button) => input.button(button),
                        MediaCommand::Playback(source, action) if input.playback_enabled() => {
                            input.playback_action(source, action);
                        }
                        MediaCommand::Playback(..) => {}
                    }
                });
                let _ = work.join_next().await;
            }
        });
        Self { sender, worker }
    }
}

impl Drop for MediaCommands {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteState {
    Waiting,
    Connected,
    ReceivingInput,
    Closed,
}

#[derive(Clone, Copy)]
enum KeyboardEditScope {
    FirstField,
    Field([u8; 16]),
}

/// One Companion connection's state machine.
pub(crate) struct CompanionSession {
    writer: Box<dyn AsyncWrite + Send + Sync + Unpin>,
    input: Arc<dyn RemoteInput>,
    pairing: Arc<PairingState>,
    device_name: Arc<str>,
    advertised_port: Option<u16>,
    mode: TrackpadMode,
    signing_key: SigningKey,
    setup: Option<(SrpServer, u64)>,
    setup_proved: bool,
    pending_verify: Option<PendingVerification>,
    cipher: Option<CompanionCipher>,
    connection_number: u64,
    accepted_at: Instant,
    verified_at: Option<Instant>,
    controller: Option<[u8; 32]>,
    close_signal: Option<oneshot::Sender<()>>,
    remote: RemoteState,
    background_probe: bool,
    current_identifier: Option<String>,
    remote_session_id: u32,
    keyboard_subscribed: bool,
    keyboard: Option<KeyboardSession>,
    keyboard_edit_scope: Option<KeyboardEditScope>,
    focus_missing: bool,
    playback: Option<PlaybackState>,
    media_commands: Option<MediaCommands>,
    seek_hold: Option<SeekHold>,
    next_event_id: u32,
    touch_width: f64,
    touch_height: f64,
    touch: Option<ActiveTouch>,
    select_pressed: Option<SelectPress>,
    last_click: Option<Click>,
    suppress_select_until: Option<Instant>,
    volume: f64,
    muted: bool,
}

impl CompanionSession {
    pub(crate) fn new(
        writer: impl AsyncWrite + Send + Sync + Unpin + 'static,
        input: Arc<dyn RemoteInput>,
        pairing: Arc<PairingState>,
        device_name: Arc<str>,
        mode: TrackpadMode,
        advertised_port: Option<u16>,
    ) -> (Self, oneshot::Receiver<()>) {
        let connection_number = pairing
            .diagnostics
            .connections
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        let (close_signal, replaced) = oneshot::channel();
        let session = Self {
            writer: Box::new(writer),
            input,
            signing_key: SigningKey::from_bytes(&pairing.private_key),
            pairing,
            device_name,
            advertised_port,
            mode,
            setup: None,
            setup_proved: false,
            pending_verify: None,
            cipher: None,
            connection_number,
            accepted_at: Instant::now(),
            verified_at: None,
            controller: None,
            close_signal: Some(close_signal),
            remote: RemoteState::Waiting,
            background_probe: false,
            current_identifier: None,
            remote_session_id: 0,
            keyboard_subscribed: false,
            keyboard: None,
            keyboard_edit_scope: None,
            focus_missing: false,
            playback: None,
            media_commands: None,
            seek_hold: None,
            next_event_id: 0x8000_0000 | rand::random::<u32>(),
            touch_width: DEFAULT_TOUCH_SIZE,
            touch_height: DEFAULT_TOUCH_SIZE,
            touch: None,
            select_pressed: None,
            last_click: None,
            suppress_select_until: None,
            volume: 0.5,
            muted: false,
        };
        (session, replaced)
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.cipher.is_some()
    }

    fn remote_started(&self) -> bool {
        matches!(
            self.remote,
            RemoteState::Connected | RemoteState::ReceivingInput
        )
    }

    pub(crate) fn idle_limit(&self) -> Option<Duration> {
        match self.remote {
            RemoteState::Connected | RemoteState::ReceivingInput => Some(Duration::from_secs(3600)),
            // A paired phone may reopen Remote on the same authenticated connection.
            RemoteState::Closed => Some(Duration::from_secs(3600)),
            RemoteState::Waiting if self.watches_keyboard_focus() => {
                Some(Duration::from_secs(3600))
            }
            RemoteState::Waiting if self.is_verified() => Some(Duration::from_secs(60)),
            RemoteState::Waiting if self.setup.is_none() => Some(Duration::from_secs(15)),
            RemoteState::Waiting => None,
        }
    }

    pub(crate) fn report_disconnect(&self, error: &Error) {
        let diagnostics = &self.pairing.diagnostics;
        match error {
            Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                diagnostics.read_timeouts.fetch_add(1, Ordering::Relaxed);
            }
            Error::Io(_) => {}
            _ => {
                diagnostics.protocol_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
        if !self.is_verified() || self.background_probe && self.remote == RemoteState::Waiting {
            return;
        }
        let status = match error {
            Error::Crypto(_) => "iPhone session lost encryption",
            Error::Io(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                "iPhone connection timed out"
            }
            Error::Io(_) if self.remote != RemoteState::Waiting => "iPhone remote disconnected",
            Error::Io(_) => "iPhone disconnected before opening Remote",
            _ => "iPhone session ended on a protocol error",
        };
        self.pairing.set_connection_status(status);
    }

    async fn write_raw(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes).await?;
        Ok(())
    }

    async fn send_frame(&mut self, frame_type: FrameType, payload: &[u8]) -> Result<()> {
        let mut out = Vec::with_capacity(4 + payload.len());
        out.push(frame_type as u8);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
        out.extend_from_slice(payload);
        self.write_raw(&out).await
    }

    async fn send_opack(&mut self, frame_type: FrameType, data: &Value) -> Result<()> {
        let payload = opack::pack(data);
        self.send_frame(frame_type, &payload).await
    }

    async fn send_encrypted_opack(&mut self, data: &Value) -> Result<()> {
        let cipher = self
            .cipher
            .as_mut()
            .ok_or_else(|| Error::Protocol("encryption is not enabled".into()))?;
        let plain = opack::pack(data);
        let mut header = Vec::with_capacity(4);
        header.push(FrameType::EOpack as u8);
        header.extend_from_slice(&((plain.len() + 16) as u32).to_be_bytes()[1..]);
        let encrypted = cipher.encrypt(&plain, Some(&header), None)?;
        let mut out = header;
        out.extend_from_slice(&encrypted);
        self.write_raw(&out).await
    }

    async fn send_response(
        &mut self,
        xid: Option<u64>,
        content: Option<Value>,
        result_type: u64,
    ) -> Result<()> {
        let mut response = vec![
            Value::kv("_c", content.unwrap_or(Value::dict(vec![]))),
            Value::kv("_t", Value::Int(3)),
        ];
        if !matches!(
            self.current_identifier.as_deref(),
            Some("_systemInfo" | "SystemInfo" | "_sessionStart" | "_sessionStop")
        ) {
            response.push(Value::kv("_rT", Value::Int(result_type)));
        }
        if let Some(xid) = xid {
            response.push(Value::kv("_x", Value::Int(xid)));
        }
        self.send_encrypted_opack(&Value::Dict(response)).await
    }

    async fn send_event(
        &mut self,
        identifier: &str,
        xid: Option<u64>,
        content: Option<Value>,
    ) -> Result<()> {
        let mut event = vec![
            Value::kv("_i", Value::str(identifier)),
            Value::kv("_t", Value::Int(1)),
            Value::kv("_c", content.unwrap_or(Value::dict(vec![]))),
        ];
        let xid = xid.unwrap_or_else(|| {
            let id = self.next_event_id;
            self.next_event_id = self.next_event_id.wrapping_add(1) | 0x8000_0000;
            u64::from(id)
        });
        event.push(Value::kv("_x", Value::Int(xid)));
        self.send_encrypted_opack(&Value::Dict(event)).await
    }

    pub(crate) async fn handle_frame(
        &mut self,
        frame_id: u8,
        payload: &[u8],
        header: &[u8],
    ) -> Result<()> {
        let frame = FrameType::from_u8(frame_id);
        if let Some(cipher) = self.cipher.as_mut() {
            if payload.is_empty() {
                return Ok(());
            }
            let payload = cipher.decrypt(payload, Some(header), None)?;
            // Non-control frames still use a nonce on the encrypted transport.
            if matches!(
                frame,
                Some(FrameType::UOpack | FrameType::EOpack | FrameType::POpack)
            ) {
                match opack::unpack(&payload) {
                    Ok((message, _)) => self.handle_control(&message).await?,
                    Err(error) => {
                        self.pairing
                            .diagnostics
                            .protocol_errors
                            .fetch_add(1, Ordering::Relaxed);
                        debug!("Skipping undecodable encrypted Companion message: {error}")
                    }
                }
            } else {
                debug!("Ignoring encrypted Companion frame {frame_id:#04x}");
            }
            return Ok(());
        }
        let frame = frame.ok_or_else(|| Error::Protocol("unknown Companion frame type".into()))?;
        if frame == FrameType::EOpack {
            return Err(Error::Protocol("verify before sending input".into()));
        }
        if !matches!(
            frame,
            FrameType::PsStart | FrameType::PsNext | FrameType::PvStart | FrameType::PvNext
        ) {
            return Ok(());
        }
        let (message, _) = opack::unpack(payload)?;
        let pairing = tlv::read(field(&message, "_pd")?);
        match (frame, pairing.get(&tags::SEQ_NO).map(Vec::as_slice)) {
            (FrameType::PsStart, Some([1])) => self.pair_setup_m1().await,
            (FrameType::PsNext, Some([3])) => self.pair_setup_m3(&pairing).await,
            (FrameType::PsNext, Some([5])) => self.pair_setup_m5(&pairing).await,
            (FrameType::PvStart, Some([1])) => self.pair_verify_m1(&pairing).await,
            (FrameType::PvNext, Some([3])) => self.pair_verify_m3(&pairing).await,
            _ => Err(Error::Protocol("unexpected pairing step".into())),
        }
    }

    async fn auth_reply(&mut self, frame: FrameType, entries: &[(u8, &[u8])]) -> Result<()> {
        self.send_opack(
            frame,
            &Value::dict(vec![Value::kv("_pd", Value::Bytes(tlv::write(entries)))]),
        )
        .await
    }

    async fn pair_setup_m1(&mut self) -> Result<()> {
        self.setup = None;
        self.setup_proved = false;
        let Some((pin, generation)) = self.pairing.begin_pairing() else {
            return self
                .auth_reply(
                    FrameType::PsNext,
                    &[(tags::SEQ_NO, &[2]), (tags::ERROR, &[6])],
                )
                .await;
        };
        let srp = SrpServer::new(pin, &rand::random::<[u8; 32]>());
        let reply = tlv::write(&[
            (tags::SEQ_NO, &[2]),
            (tags::SALT, srp.salt()),
            (tags::PUBLIC_KEY, &srp.public_key()),
            (27, &[1]),
        ]);
        self.setup = Some((srp, generation));
        self.send_opack(
            FrameType::PsNext,
            &Value::dict(vec![
                Value::kv("_pd", Value::Bytes(reply)),
                Value::kv("_pwTy", Value::Int(1)),
            ]),
        )
        .await
    }

    async fn pair_setup_m3(&mut self, data: &Tlv) -> Result<()> {
        let (mut srp, generation) = self
            .setup
            .take()
            .filter(|_| !self.setup_proved)
            .ok_or_else(|| Error::Protocol("pair setup was not started".into()))?;
        let public_key = tlv_field(data, tags::PUBLIC_KEY)?;
        if public_key.len() > 384 {
            return Err(Error::Protocol("invalid SRP public key".into()));
        }
        srp.process(public_key)?;
        if !srp.verify_proof(tlv_field(data, tags::PROOF)?) {
            self.pairing.set_connection_status("Pairing code rejected");
            return self
                .auth_reply(
                    FrameType::PsNext,
                    &[(tags::SEQ_NO, &[4]), (tags::ERROR, &[2])],
                )
                .await;
        }
        let proof = srp.key_proof_hash()?;
        self.setup = Some((srp, generation));
        self.setup_proved = true;
        self.auth_reply(
            FrameType::PsNext,
            &[(tags::SEQ_NO, &[4]), (tags::PROOF, &proof)],
        )
        .await
    }

    async fn pair_setup_m5(&mut self, data: &Tlv) -> Result<()> {
        let (srp, generation) = self
            .setup
            .take()
            .filter(|_| self.setup_proved)
            .ok_or_else(|| Error::Protocol("PIN proof is required".into()))?;
        let shared = srp.session_key()?;
        let key = hkdf_expand("Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info", shared);
        let mut cipher = CompanionCipher::new(&key, &key, 8)?;
        let controller = tlv::read(&cipher.decrypt(
            tlv_field(data, tags::ENCRYPTED_DATA)?,
            None,
            Some(b"PS-Msg05"),
        )?);
        let id = controller_id(&controller)?;
        let public: [u8; 32] = tlv_field(&controller, tags::PUBLIC_KEY)?
            .try_into()
            .map_err(|_| Error::Protocol("invalid controller public key".into()))?;
        let signing_key = VerifyingKey::from_bytes(&public)
            .map_err(|_| Error::Protocol("invalid controller public key".into()))?;
        let controller_x = hkdf_expand(
            "Pair-Setup-Controller-Sign-Salt",
            "Pair-Setup-Controller-Sign-Info",
            shared,
        );
        check_signature(
            &signing_key,
            &controller,
            &[&controller_x, id, &public].concat(),
        )?;

        let accessory_x = hkdf_expand(
            "Pair-Setup-Accessory-Sign-Salt",
            "Pair-Setup-Accessory-Sign-Info",
            shared,
        );
        let server_id = self.pairing.identifier.as_bytes();
        let server_public = self.signing_key.verifying_key().to_bytes();
        let signature = self
            .signing_key
            .sign(&[&accessory_x[..], server_id, &server_public].concat());
        let extra = Value::dict(vec![
            Value::kv(
                "altIRK",
                Value::Bytes(
                    hkdf_expand("myRemote", "altIRK", &self.pairing.private_key)[..16].to_vec(),
                ),
            ),
            Value::kv("accountID", Value::str(&self.pairing.identifier)),
            Value::kv("model", Value::str(DEVICE_MODEL)),
            Value::kv("wifiMAC", Value::Bytes(self.pairing.mac.to_vec())),
            Value::kv("name", Value::str(&self.device_name)),
            Value::kv("mac", Value::Bytes(self.pairing.mac.to_vec())),
        ]);
        let reply = tlv::write(&[
            (tags::IDENTIFIER, server_id),
            (tags::PUBLIC_KEY, &server_public),
            (tags::SIGNATURE, &signature.to_bytes()),
            (17, &opack::pack(&extra)),
        ]);
        let encrypted = cipher.encrypt(&reply, None, Some(b"PS-Msg06"))?;
        self.pairing.trust(generation, id, public)?;
        self.auth_reply(
            FrameType::PsNext,
            &[(tags::SEQ_NO, &[6]), (tags::ENCRYPTED_DATA, &encrypted)],
        )
        .await?;
        info!("Phone paired. Pairing window closed.");
        Ok(())
    }

    async fn pair_verify_m1(&mut self, data: &Tlv) -> Result<()> {
        let client_public: [u8; 32] = tlv_field(data, tags::PUBLIC_KEY)?
            .try_into()
            .map_err(|_| Error::Protocol("invalid X25519 public key".into()))?;
        let secret = StaticSecret::random_from_rng(OsRng);
        let server_public = X25519PublicKey::from(&secret).to_bytes();
        let shared = secret.diffie_hellman(&X25519PublicKey::from(client_public));
        if !shared.was_contributory() {
            return Err(Error::Protocol("invalid X25519 exchange".into()));
        }
        let shared = shared.to_bytes();
        let server_id = self.pairing.identifier.as_bytes();
        let signature = self
            .signing_key
            .sign(&[&server_public[..], server_id, &client_public].concat());
        let reply = tlv::write(&[
            (tags::IDENTIFIER, server_id),
            (tags::SIGNATURE, &signature.to_bytes()),
        ]);
        let encrypted = verification_cipher(&shared)?.encrypt(&reply, None, Some(b"PV-Msg02"))?;
        self.pending_verify = Some(PendingVerification {
            shared,
            client_public,
            server_public,
        });
        self.auth_reply(
            FrameType::PvNext,
            &[
                (tags::SEQ_NO, &[2]),
                (tags::PUBLIC_KEY, &server_public),
                (tags::ENCRYPTED_DATA, &encrypted),
            ],
        )
        .await
    }

    async fn pair_verify_m3(&mut self, data: &Tlv) -> Result<()> {
        let pending = self
            .pending_verify
            .take()
            .ok_or_else(|| Error::Protocol("verify was not started".into()))?;
        let controller = tlv::read(&verification_cipher(&pending.shared)?.decrypt(
            tlv_field(data, tags::ENCRYPTED_DATA)?,
            None,
            Some(b"PV-Msg03"),
        )?);
        let id = controller_id(&controller)?;
        let verified = self.pairing.controller_key(id).and_then(|key| {
            check_signature(
                &key,
                &controller,
                &[&pending.client_public[..], id, &pending.server_public].concat(),
            )
        });
        if verified.is_err() {
            self.pairing
                .diagnostics
                .verification_rejections
                .fetch_add(1, Ordering::Relaxed);
            self.pairing
                .set_connection_status("iPhone credentials were rejected · pair again");
            return self
                .auth_reply(
                    FrameType::PvNext,
                    &[(tags::SEQ_NO, &[4]), (tags::ERROR, &[2])],
                )
                .await;
        }
        let session_id = Sha256::new()
            .chain_update(self.pairing.identifier.as_bytes())
            .chain_update(id)
            .finalize();
        // Renew the same logical session for this authenticated controller, using fresh
        // encryption keys for each connection.
        self.remote_session_id = u32::from_le_bytes(session_id[..4].try_into().unwrap()).max(1);
        self.auth_reply(FrameType::PvNext, &[(tags::SEQ_NO, &[4])])
            .await?;
        self.cipher = Some(CompanionCipher::new(
            &hkdf_expand("", "ServerEncrypt-main", &pending.shared),
            &hkdf_expand("", "ClientEncrypt-main", &pending.shared),
            12,
        )?);
        self.controller = Some(session_id.into());
        self.verified_at = Some(Instant::now());
        *self.pairing.diagnostics.last_verify_ms.lock().unwrap() =
            Some(self.accepted_at.elapsed().as_millis());
        self.pairing
            .diagnostics
            .verified_connections
            .fetch_add(1, Ordering::Relaxed);
        self.pairing
            .set_connection_status("iPhone verified · waiting for Remote session");
        Ok(())
    }

    async fn handle_control(&mut self, data: &Value) -> Result<()> {
        let is_event = match data.get("_t").and_then(Value::as_int) {
            Some(1) => true,
            Some(2) => false,
            _ => return Ok(()),
        };
        self.current_identifier = data
            .get("_i")
            .and_then(Value::as_str)
            .or_else(|| data.get("model").and_then(Value::as_str))
            .map(str::to_owned);
        let xid = data.get("_x").and_then(Value::as_int);
        let content = data.get("_c").unwrap_or(&Value::Null);

        match self.current_identifier.as_deref() {
            Some("_sessionStart") => {
                if content
                    .get("_srvT")
                    .is_none_or(|service| service.as_str() == Some(REMOTE_SERVICE))
                {
                    self.claim_remote()?;
                    self.start_remote();
                }
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv(
                        "_sid",
                        Value::Int(self.remote_session_id.into()),
                    )])),
                    0,
                )
                .await?;
            }
            Some("TVRCSessionStart") => {
                self.claim_remote()?;
                self.start_remote();
                let version = content
                    .get("ProtocolVersionKey")
                    .and_then(Value::as_str)
                    .unwrap_or("1.2");
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv(
                        "ProtocolVersionKey",
                        Value::str(version),
                    )])),
                    0,
                )
                .await?;
                self.send_media_state(None).await?;
                if self.playback.is_some() {
                    self.send_now_playing().await?;
                }
            }
            Some("_sessionStop" | "TVRCSessionStop" | "_touchStop" | "HIDTouchStop") => {
                self.clear_touch();
                if matches!(
                    self.current_identifier.as_deref(),
                    Some("_sessionStop" | "TVRCSessionStop")
                ) {
                    // Closing the TVRC UI leaves the Rapport RTI subscription
                    // available when the same verified connection reopens.
                    if self.current_identifier.as_deref() == Some("_sessionStop") {
                        self.keyboard_subscribed = false;
                    }
                    self.playback = None;
                    self.seek_hold = None;
                    self.media_commands = None;
                    self.keyboard_edit_scope = None;
                    if self.keyboard.is_some() {
                        self.update_keyboard(None);
                        self.send_event("_tiStopped", None, None).await?;
                    }
                    self.stop_remote();
                    self.last_click = None;
                    self.suppress_select_until = None;
                    self.pairing.set_connection_status("iPhone remote closed");
                }
                self.send_response(xid, None, 0).await?;
            }
            Some("_touchStart" | "HIDTouchStart") => {
                let (key, width, height) =
                    if self.current_identifier.as_deref() == Some("HIDTouchStart") {
                        ("Identifier", "Width", "Height")
                    } else {
                        ("_i", "_width", "_height")
                    };
                self.clear_touch();
                let size = |key| {
                    content
                        .get(key)
                        .and_then(Value::as_float)
                        .filter(|value| (1.0..=4000.0).contains(value))
                        .unwrap_or(DEFAULT_TOUCH_SIZE)
                };
                self.touch_width = size(width);
                self.touch_height = size(height);
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv(key, Value::Int(1))])),
                    0,
                )
                .await?;
            }
            Some("_hidC" | "HIDCommand") => {
                self.claim_remote()?;
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
                self.handle_hid_command(content, xid).await?;
            }
            Some("_hidT" | "HIDTouch") => {
                self.claim_remote()?;
                let modern = self.current_identifier.as_deref() == Some("HIDTouch");
                self.handle_touch(content, modern);
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
            }
            Some("_interest") => {
                if let Some(Value::Array(events)) = content.get("_regEvents") {
                    for event in events.iter().filter_map(Value::as_str) {
                        match event {
                            "MediaControlStatus" => {
                                self.send_event(event, None, Some(self.media_flags()))
                                    .await?
                            }
                            "_iMC" => self.send_legacy_media_flags(None).await?,
                            "NowPlayingInfo" => self.send_now_playing().await?,
                            "SystemStatus" | "TVSystemStatus" => {
                                self.send_event(event, None, Some(awake())).await?
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some("_systemInfo" | "SystemInfo") => {
                // Rapport uses the high XID bit for background system-info probes.
                self.background_probe = self.remote == RemoteState::Waiting
                    && xid.is_some_and(|xid| xid & 0x8000_0000 != 0);
                self.send_response(xid, Some(self.system_info()), 0).await?;
                self.send_event("SystemStatus", None, Some(awake())).await?;
                self.send_event("TVSystemStatus", None, Some(awake()))
                    .await?;
            }
            Some("FetchAttentionState") => self.send_response(xid, Some(awake()), 0).await?,
            Some("FetchMediaControlStatus") => {
                self.send_response(xid, Some(self.media_flags()), 0).await?
            }
            Some("FetchCurrentNowPlayingInfoEvent") => {
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
                self.send_media_state(None).await?;
                self.send_now_playing().await?;
            }
            Some("MediaControlCommand" | "_mcc") => {
                self.handle_media_control(xid, content, is_event).await?
            }
            Some("ToggleCaptions") => {
                self.queue_playback_action(PlaybackAction::ToggleCaptions, None);
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
            }
            Some("_tiStart") => {
                self.pairing
                    .diagnostics
                    .keyboard_subscriptions
                    .fetch_add(1, Ordering::Relaxed);
                self.keyboard_subscribed = true;
                if !is_event {
                    if let Ok(focused) =
                        tokio::time::timeout(Duration::from_millis(100), self.focused_text_field())
                            .await
                    {
                        self.update_keyboard(focused);
                        self.focus_missing = false;
                    }
                    let payload = self.keyboard.as_ref().map(KeyboardSession::focus_payload);
                    if payload.is_some() {
                        self.pairing
                            .diagnostics
                            .keyboard_focuses
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    self.send_response(xid, payload, 0).await?;
                }
                self.keyboard_edit_scope = Some(
                    self.keyboard
                        .as_ref()
                        .map_or(KeyboardEditScope::FirstField, |keyboard| {
                            KeyboardEditScope::Field(keyboard.id)
                        }),
                );
            }
            Some("_tiStop") => {
                self.keyboard_subscribed = false;
                self.keyboard_edit_scope = None;
                self.update_keyboard(None);
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
            }
            Some("_tiC") => {
                self.type_text(content, data).await;
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
            }
            Some("FetchLaunchableApplicationsEvent") => {
                self.send_response(
                    xid,
                    Some(Value::dict(vec![
                        Value::kv("AppCountKey", Value::Int(0)),
                        Value::kv("AppInfos", Value::dict(vec![])),
                    ])),
                    2,
                )
                .await?;
            }
            _ if xid.is_some() && !is_event => self.send_response(xid, None, 0).await?,
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn watches_keyboard_focus(&self) -> bool {
        self.keyboard_subscribed && self.remote != RemoteState::Closed
    }

    pub(crate) fn watches_playback(&self) -> bool {
        self.remote_started() && (self.playback.is_some() || self.input.playback_enabled())
    }

    pub(crate) fn queue_playback_probe(&self, probes: &mut JoinSet<Option<PlaybackState>>) {
        let input = self.input.clone();
        probes.spawn_blocking(move || {
            if input.playback_enabled() {
                input.playback_state()
            } else {
                None
            }
        });
    }

    pub(crate) async fn update_playback(&mut self, next: Option<PlaybackState>) -> Result<()> {
        let next = next.filter(|_| self.watches_playback() && self.input.playback_enabled());
        if next != self.playback {
            if self.playback.map(|old| old.source) != next.map(|new| new.source) {
                self.seek_hold = None;
            }
            self.playback = next;
            let player = match next.map(|state| state.source) {
                Some(VideoSource::Jellyfin) => 1,
                Some(VideoSource::YouTube) => 2,
                None => 0,
            };
            self.send_media_state(None).await?;
            self.send_now_playing().await?;
            if player != 0 {
                self.pairing
                    .diagnostics
                    .player
                    .store(player, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    fn queue_media_command(&mut self, command: MediaCommand, seek: Option<Weak<()>>) {
        let expires = Instant::now()
            + if seek.is_some() {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            };
        let queue = self
            .media_commands
            .get_or_insert_with(|| MediaCommands::start(self.input.clone()));
        // Under sustained stalls, bound the pending work and avoid replaying
        // old presses once the browser starts responding again.
        if queue
            .sender
            .try_send(QueuedMediaCommand {
                command,
                expires,
                seek,
            })
            .is_ok()
        {
            self.record_input();
        }
    }

    fn queue_playback_action(&mut self, action: PlaybackAction, seek: Option<Weak<()>>) {
        let Some(playback) = self.playback else {
            return;
        };
        self.queue_media_command(MediaCommand::Playback(playback.source, action), seek);
    }

    pub(crate) fn seek_deadline(&self) -> Option<Instant> {
        self.seek_hold
            .as_ref()
            .map(|hold| hold.next.min(hold.expires))
    }

    pub(crate) fn repeat_seek(&mut self) {
        let now = Instant::now();
        let Some(hold) = &mut self.seek_hold else {
            return;
        };
        if now >= hold.expires || !self.input.playback_enabled() {
            self.seek_hold = None;
            return;
        }
        hold.next = now + Duration::from_millis(500);
        let action = hold.action;
        let seek = Arc::downgrade(&hold.active);
        self.queue_playback_action(action, Some(seek));
    }

    async fn focused_text_field(&self) -> Option<u64> {
        if !self.watches_keyboard_focus() {
            return None;
        }
        let input = self.input.clone();
        tokio::task::spawn_blocking(move || input.focused_text_field())
            .await
            .ok()
            .flatten()
    }

    fn update_keyboard(&mut self, focus: Option<u64>) {
        if self.keyboard.as_ref().map(|keyboard| keyboard.field) != focus {
            let controller = self
                .controller
                .expect("keyboard is only available after verification");
            let mut fields = self.pairing.keyboard_fields.lock().unwrap();
            if let Some(old) = self.keyboard.take() {
                fields.remove(&(controller, old.id));
            }
            self.keyboard = focus.map(KeyboardSession::new);
            if let Some(keyboard) = &self.keyboard {
                fields.insert(
                    (controller, keyboard.id),
                    KeyboardField::new(keyboard.field),
                );
            }
        }
    }

    pub(crate) fn queue_keyboard_probe(&self, probes: &mut JoinSet<Option<u64>>) {
        let input = self.input.clone();
        probes.spawn_blocking(move || input.focused_text_field());
    }

    pub(crate) async fn publish_keyboard_focus(&mut self, focus: Option<u64>) -> Result<()> {
        if !self.watches_keyboard_focus() {
            return Ok(());
        }
        // A busy app can miss one Accessibility query. Keep the session UUID
        // through that gap so a valid edit can still target the same field.
        if focus.is_none() && self.keyboard.is_some() && !self.focus_missing {
            self.focus_missing = true;
            return Ok(());
        }
        self.focus_missing = false;
        if self.keyboard.as_ref().map(|keyboard| keyboard.field) != focus {
            if self.keyboard.is_some() {
                self.send_event("_tiStopped", None, None).await?;
            }
            self.update_keyboard(focus);
            if let Some(keyboard) = &self.keyboard {
                let payload = keyboard.focus_payload();
                self.send_event("_tiStarted", None, Some(payload)).await?;
                if matches!(
                    self.keyboard_edit_scope,
                    Some(KeyboardEditScope::FirstField)
                ) {
                    self.keyboard_edit_scope = self
                        .keyboard
                        .as_ref()
                        .map(|field| KeyboardEditScope::Field(field.id));
                }
                self.pairing
                    .diagnostics
                    .keyboard_focuses
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn type_text(&mut self, content: &Value, message: &Value) {
        let diagnostics = &self.pairing.diagnostics;
        diagnostics
            .keyboard_messages
            .fetch_add(1, Ordering::Relaxed);
        let version = content
            .get("_tiV")
            .or_else(|| message.get("_tiV"))
            .and_then(Value::as_int);
        *diagnostics.keyboard_version.lock().unwrap() = version;
        let edit = match KeyboardSession::text_edit(content, message, version) {
            Ok(edit) => edit,
            Err(error) => {
                diagnostics.keyboard_failures[error as usize].fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        diagnostics.keyboard_edits.fetch_add(1, Ordering::Relaxed);
        if edit.marked.is_some() {
            diagnostics
                .keyboard_composition_updates
                .fetch_add(1, Ordering::Relaxed);
        }
        if edit.suggestion.is_some() {
            diagnostics
                .keyboard_suggestions
                .fetch_add(1, Ordering::Relaxed);
        }
        let target = match edit.target {
            Some(target) => target,
            None => {
                // Only the connection that started the keyboard can supply a
                // target implicitly. Keep the original offer even after focus
                // changes: a delayed edit cannot fall through to another field.
                diagnostics
                    .keyboard_session_edits
                    .fetch_add(1, Ordering::Relaxed);
                let Some(target) = self.keyboard.as_ref().map(|keyboard| keyboard.id).filter(|id| {
                    matches!(self.keyboard_edit_scope, Some(KeyboardEditScope::Field(armed)) if armed == *id)
                }) else {
                    diagnostics.keyboard_scope_rejections.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                target
            }
        };
        let controller = self
            .controller
            .expect("keyboard is only available after verification");
        // A separate connection can name a field from the same pairing if it
        // supplies the UUID explicitly.
        let pairing = self.pairing.clone();
        let input = self.input.clone();
        let applied = tokio::task::spawn_blocking(move || {
            let (field, keystrokes) = {
                let mut fields = pairing.keyboard_fields.lock().unwrap();
                let Some(field) = fields.get_mut(&(controller, target)) else {
                    return false;
                };
                pairing
                    .diagnostics
                    .keyboard_matched
                    .fetch_add(1, Ordering::Relaxed);
                let Some(keystrokes) = field.apply(edit) else {
                    return false;
                };
                (field.id, keystrokes)
            };
            match keystrokes {
                KeyboardInput::Clear => input.clear_text(field),
                KeyboardInput::Suggestion(suggestion, following) => {
                    input.accept_suggestion(field, &suggestion.typed, &suggestion.word, &following)
                }
                KeyboardInput::Text {
                    backspaces,
                    deletes,
                    text,
                } => input.type_text(field, backspaces, deletes, &text),
            }
        })
        .await
        .unwrap_or(false);
        if applied {
            diagnostics.keyboard_posted.fetch_add(1, Ordering::Relaxed);
            if self.remote_started() {
                self.record_input();
            }
        }
    }

    fn system_info(&self) -> Value {
        let mac = self.pairing.mac.map(|byte| format!("{byte:02X}")).join(":");
        let mut fields = vec![
            Value::kv("_sf", Value::Int(65536)),
            Value::kv("_cf", Value::Int(512)),
            Value::kv("_i", Value::str(&hex::encode_upper(self.pairing.mac))),
            Value::kv("_pubID", Value::str(&mac)),
            Value::kv("_spID", Value::str(&self.pairing.identifier)),
            // This is the UUID advertised as UniqueIdentifier in the MediaRemote TXT record.
            Value::kv("_mrID", Value::str(&self.pairing.identifier)),
            Value::kv("_stA", Value::Array(vec![Value::str(REMOTE_SERVICE)])),
            Value::kv("_sv", Value::str(SOURCE_VERSION)),
            Value::kv("model", Value::str(DEVICE_MODEL)),
            Value::kv("name", Value::str(&self.device_name)),
        ];
        // A relay has its own advertised LAN port, which the backend cannot
        // know. On the direct LAN connection, report the actual receiver port.
        if let Some(port) = self.advertised_port {
            fields.push(Value::kv("_lP", Value::Int(u64::from(port))));
        }
        Value::dict(fields)
    }

    async fn handle_hid_command(&mut self, content: &Value, xid: Option<u64>) -> Result<()> {
        let (command_key, state_key) = if self.current_identifier.as_deref() == Some("HIDCommand") {
            ("HIDCommand", "HIDButtonState")
        } else {
            ("_hidC", "_hBtS")
        };
        let Some(button) = content
            .get(command_key)
            .and_then(Value::as_int)
            .and_then(hid_command_name)
        else {
            return Ok(());
        };
        match content.get(state_key).and_then(Value::as_int) {
            Some(1) if button == "select" && self.mode == TrackpadMode::Mouse => {
                if self.touch.is_none() {
                    let started = Instant::now();
                    let count = self.click_count(TapTime {
                        received: started,
                        phone_ns: None,
                    });
                    self.select_pressed
                        .get_or_insert(SelectPress::Down { started, count });
                    self.record_input();
                }
                return Ok(());
            }
            Some(2) => {}
            _ => return Ok(()),
        }
        self.record_input();
        if button == "menu" {
            self.pairing
                .diagnostics
                .back_presses
                .fetch_add(1, Ordering::Relaxed);
        }
        if button == "select" {
            let now = Instant::now();
            let press = self.select_pressed.take();
            if self.mode == TrackpadMode::Direction || self.touch.is_none() {
                let time = TapTime {
                    received: now,
                    phone_ns: None,
                };
                match press {
                    Some(SelectPress::Consumed) => self.suppress_click_echo(now),
                    Some(SelectPress::Dragging(_)) => {
                        self.input.end_drag();
                        self.suppress_click_echo(now);
                    }
                    Some(SelectPress::Down { started, count: 1 })
                        if now - started >= RIGHT_CLICK_HOLD =>
                    {
                        self.right_click();
                        self.suppress_click_echo(now);
                    }
                    Some(SelectPress::Down { count, .. }) => self.select(None, time, Some(count)),
                    None => self.select(None, time, None),
                }
            }
        } else if matches!(button, "play_pause" | "home") && self.input.playback_enabled() {
            self.queue_media_command(MediaCommand::Button(button), None);
        } else {
            self.input.button(button);
        }
        match button {
            "volume_up" | "volume_down" => {
                let step = if button == "volume_up" {
                    1.0 / 16.0
                } else {
                    -1.0 / 16.0
                };
                self.volume = (self.volume + step).clamp(0.0, 1.0);
                self.muted = false;
                self.send_media_state(xid).await?;
            }
            "mute" => {
                self.muted = !self.muted;
                self.send_media_state(xid).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn send_media_state(&mut self, xid: Option<u64>) -> Result<()> {
        let mut status = self.media_flags();
        if let Value::Dict(entries) = &mut status {
            entries.push(Value::kv("Muted", Value::Bool(self.muted)));
        }
        self.send_event("MediaControlStatus", xid, Some(status))
            .await?;
        self.send_legacy_media_flags(xid).await
    }

    fn media_flags(&self) -> Value {
        Value::dict(vec![Value::kv(
            "MediaControlFlags",
            Value::Int(remote_media::flags(self.playback)),
        )])
    }

    async fn send_legacy_media_flags(&mut self, xid: Option<u64>) -> Result<()> {
        self.send_event(
            "_iMC",
            xid,
            Some(Value::dict(vec![Value::kv(
                "_mcF",
                Value::Int(remote_media::flags(self.playback)),
            )])),
        )
        .await
    }

    async fn send_now_playing(&mut self) -> Result<()> {
        self.send_event(
            "NowPlayingInfo",
            None,
            Some(remote_media::now_playing(self.playback)),
        )
        .await
    }

    async fn handle_media_control(
        &mut self,
        xid: Option<u64>,
        content: &Value,
        is_event: bool,
    ) -> Result<()> {
        let volume_key = if self.current_identifier.as_deref() == Some("MediaControlCommand") {
            "MessageKeyVolume"
        } else {
            "_vol"
        };
        let command = content
            .get("MediaControlCommand")
            .or_else(|| content.get("_mcc"))
            .and_then(Value::as_int);
        let response = match command {
            Some(1 | 2) => {
                let action = if command == Some(1) {
                    PlaybackAction::Play
                } else {
                    PlaybackAction::Pause
                };
                self.queue_playback_action(action, None);
                None
            }
            Some(5) => {
                let input = self.input.clone();
                let volume = tokio::time::timeout(
                    Duration::from_millis(100),
                    tokio::task::spawn_blocking(move || input.volume()),
                )
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
                .unwrap_or(if self.muted { 0.0 } else { self.volume });
                Some(Value::dict(vec![Value::kv(
                    volume_key,
                    Value::Double(volume),
                )]))
            }
            Some(6) => {
                if let Some(volume) = content
                    .get(volume_key)
                    .or_else(|| content.get("_vol"))
                    .and_then(Value::as_float)
                    .filter(|v| v.is_finite())
                {
                    self.volume = volume.clamp(0.0, 1.0);
                    self.muted = false;
                    self.record_input();
                    self.input.set_volume(self.volume);
                }
                None
            }
            Some(7) => {
                if let Some(seconds) = content
                    .get("_skpS")
                    .or_else(|| content.get("MessageKeySkipSeconds"))
                    .and_then(Value::as_float)
                    .filter(|seconds| seconds.is_finite() && *seconds != 0.0)
                {
                    self.pairing
                        .diagnostics
                        .native_skips
                        .fetch_add(1, Ordering::Relaxed);
                    let action = if seconds > 0.0 {
                        PlaybackAction::SkipForward
                    } else {
                        PlaybackAction::SkipBackward
                    };
                    self.queue_playback_action(action, None);
                }
                None
            }
            Some(8 | 10) if self.playback.is_some() => {
                let action = if command == Some(8) {
                    PlaybackAction::SkipForward
                } else {
                    PlaybackAction::SkipBackward
                };
                let now = Instant::now();
                self.seek_hold = Some(SeekHold {
                    action,
                    next: now + Duration::from_millis(500),
                    expires: now + Duration::from_secs(8),
                    active: Arc::new(()),
                });
                self.queue_playback_action(action, None);
                None
            }
            Some(9 | 11) => {
                self.seek_hold = None;
                None
            }
            Some(12) => Some(Value::dict(vec![Value::kv(
                "_cse",
                Value::Bool(
                    self.playback
                        .and_then(|state| state.captions)
                        .unwrap_or(false),
                ),
            )])),
            Some(13) => {
                if let Some(Value::Bool(enabled)) = content.get("_cse") {
                    self.queue_playback_action(PlaybackAction::SetCaptions(*enabled), None);
                }
                None
            }
            _ => None,
        };
        if !is_event {
            self.send_response(xid, response, 0).await?;
        }
        Ok(())
    }

    fn record_input(&mut self) {
        self.start_remote();
        if self.remote == RemoteState::Connected {
            self.pairing.remote_received_input();
            self.remote = RemoteState::ReceivingInput;
        }
    }

    fn claim_remote(&mut self) -> Result<()> {
        if self.close_signal.is_some() {
            let controller = self.controller.expect("encrypted connection was verified");
            if !self.pairing.claim_remote(
                controller,
                self.connection_number,
                &mut self.close_signal,
            ) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionAborted,
                    "a newer Remote session is active",
                )
                .into());
            }
        }
        Ok(())
    }

    fn start_remote(&mut self) {
        if !self.remote_started() {
            if let Some(verified_at) = self.verified_at.take() {
                *self.pairing.diagnostics.last_open_ms.lock().unwrap() =
                    Some(verified_at.elapsed().as_millis());
            } else {
                self.pairing
                    .diagnostics
                    .warm_reopens
                    .fetch_add(1, Ordering::Relaxed);
            }
            self.pairing.remote_started();
            self.remote = RemoteState::Connected;
        }
    }

    fn stop_remote(&mut self) {
        if self.remote_started() {
            self.pairing
                .remote_stopped(self.remote == RemoteState::ReceivingInput);
            self.remote = RemoteState::Closed;
        }
    }

    fn select(&mut self, touch_started: Option<Instant>, time: TapTime, count: Option<u8>) {
        let from_touch = touch_started.is_some();
        let source = if from_touch { 1 } else { 2 };
        if !from_touch
            && self
                .suppress_select_until
                .is_some_and(|until| time.received < until)
        {
            return;
        }
        // A physical tap can arrive as both Touch End and HID Select. Only
        // consume one echo, leaving the next touch or button tap eligible.
        if let Some(last) = &mut self.last_click {
            if last.sources & source == 0
                && !touch_started.is_some_and(|start| start >= last.time.received)
                && time.follows(last.time, SELECT_ECHO_WINDOW)
            {
                last.sources |= source;
                return;
            }
        }
        let count = count.unwrap_or_else(|| self.click_count(time));
        self.last_click = Some(Click {
            time,
            sources: source,
            count,
        });
        match (self.mode, count) {
            (TrackpadMode::Direction, _) => self.input.button("select"),
            (TrackpadMode::Mouse, 3) => {
                self.input.triple_click();
                self.pairing
                    .diagnostics
                    .triple_clicks
                    .fetch_add(1, Ordering::Relaxed);
            }
            (TrackpadMode::Mouse, 2) => {
                self.input.double_click();
                self.pairing
                    .diagnostics
                    .double_clicks
                    .fetch_add(1, Ordering::Relaxed);
            }
            (TrackpadMode::Mouse, _) => self.input.click(),
        }
    }

    fn click_count(&self, time: TapTime) -> u8 {
        self.last_click
            .as_ref()
            .filter(|last| {
                self.mode == TrackpadMode::Mouse
                    && last.count < 3
                    && time.follows(last.time, MULTI_TAP_WINDOW)
            })
            .map_or(1, |last| last.count + 1)
    }

    fn suppress_click_echo(&mut self, now: Instant) {
        self.last_click = None;
        self.suppress_select_until = Some(now + SELECT_ECHO_WINDOW);
    }

    fn right_click(&self) {
        self.input.right_click();
        self.pairing
            .diagnostics
            .right_clicks
            .fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn touch_deadline(&self) -> Option<Instant> {
        match self.select_pressed {
            Some(SelectPress::Down { started, count }) => {
                return select_hold(count).map(|hold| started + hold)
            }
            Some(SelectPress::Dragging(started)) => return Some(started + DRAG_IDLE_LIMIT),
            _ => {}
        }
        let touch = self.touch.as_ref()?;
        if touch.scrolling {
            None
        } else if touch.dragging {
            Some(touch.updated + DRAG_IDLE_LIMIT)
        } else if touch.drag_pending() {
            Some(touch.started.received + DRAG_HOLD)
        } else if self.mode == TrackpadMode::Mouse && touch.can_right_click() {
            Some(touch.started.received + RIGHT_CLICK_HOLD)
        } else {
            None
        }
    }

    pub(crate) fn touch_timer(&mut self) {
        if let Some(SelectPress::Down { count, .. }) = self.select_pressed {
            self.select_pressed = Some(if count == 2 && self.input.start_drag() {
                SelectPress::Dragging(Instant::now())
            } else {
                if count == 1 {
                    self.right_click();
                }
                SelectPress::Consumed
            });
        } else if matches!(self.select_pressed, Some(SelectPress::Dragging(_)))
            || self.touch.as_ref().is_some_and(|touch| touch.dragging)
        {
            self.clear_touch();
        } else if let Some(touch) = &mut self.touch {
            if touch.drag_pending() {
                touch.start_drag(self.input.as_ref());
            } else {
                touch.right_clicked = true;
                self.right_click();
            }
        }
    }

    fn clear_touch(&mut self) {
        let button_drag = matches!(self.select_pressed.take(), Some(SelectPress::Dragging(_)));
        let touch = self.touch.take();
        let dragging = button_drag || touch.as_ref().is_some_and(|touch| touch.dragging);
        if dragging {
            self.input.end_drag();
        }
        if dragging || touch.is_some_and(|touch| touch.scrolling) {
            self.suppress_click_echo(Instant::now());
        }
    }

    fn handle_touch(&mut self, content: &Value, modern: bool) {
        let (phase_key, x_key, y_key) = if modern {
            ("TouchPhase", "CoordinateX", "CoordinateY")
        } else {
            ("_tPh", "_cx", "_cy")
        };
        let phase = match (modern, content.get(phase_key).and_then(Value::as_int)) {
            (_, Some(1)) => TouchPhase::Began,
            (true, Some(2 | 5)) | (false, Some(2 | 3)) => TouchPhase::Moved,
            (true, Some(3)) | (false, Some(4)) => TouchPhase::Ended,
            (true, Some(4)) => TouchPhase::Cancelled,
            _ => return,
        };
        if phase == TouchPhase::Cancelled {
            self.clear_touch();
            return;
        }
        let coordinate = |key| {
            content
                .get(key)
                .and_then(Value::as_float)
                .filter(|value| value.is_finite() && (-4000.0..=4000.0).contains(value))
        };
        let point = coordinate(x_key).zip(coordinate(y_key)).or_else(|| {
            // A lift can omit coordinates. The last reported touch position
            // still determines whether this gesture was a tap or a drag.
            (phase == TouchPhase::Ended)
                .then(|| self.touch.as_ref().map(|touch| touch.last))
                .flatten()
        });
        let Some(point) = point else {
            return;
        };
        self.record_input();
        let (x, y) = point;
        let now = Instant::now();
        let time = TapTime {
            received: now,
            phone_ns: content
                .get(if modern { "Nanoseconds" } else { "_ns" })
                .and_then(Value::as_int)
                .filter(|value| *value != 0),
        };
        match phase {
            TouchPhase::Began => {
                // A touch stream takes over from the Select button so the
                // same physical hold cannot trigger a second right click.
                let press = self.select_pressed.take();
                if self.touch.is_some() {
                    self.clear_touch();
                }
                let dragging = matches!(press, Some(SelectPress::Dragging(_)));
                let right_clicked = matches!(press, Some(SelectPress::Consumed));
                let scrolling = self.mode == TrackpadMode::Mouse
                    && !dragging
                    && !right_clicked
                    && (self.touch_width * SCROLL_STRIP_START..=self.touch_width).contains(&x);
                self.touch = Some(ActiveTouch {
                    origin: point,
                    last: point,
                    started: time,
                    updated: now,
                    distance: 0.0,
                    click_count: self.click_count(time),
                    drag_attempted: false,
                    dragging,
                    right_clicked,
                    scrolling,
                });
            }
            TouchPhase::Moved | TouchPhase::Ended => {
                let Some(touch) = &mut self.touch else {
                    return;
                };
                if touch.scrolling {
                    let dy = y.clamp(0.0, self.touch_height)
                        - touch.last.1.clamp(0.0, self.touch_height);
                    if dy != 0.0 {
                        self.input
                            .scroll_vertical(dy * (DEFAULT_TOUCH_SIZE / self.touch_height));
                    }
                    touch.last = point;
                    if phase == TouchPhase::Ended {
                        self.clear_touch();
                    }
                    return;
                }
                let dx = x - touch.origin.0;
                let dy = y - touch.origin.1;
                let distance = dx.abs().max(dy.abs());
                touch.distance = touch.distance.max(distance);
                touch.updated = now;
                let elapsed = time.elapsed_since(touch.started);
                let mut previous = touch.last;
                if touch.drag_pending() {
                    if elapsed >= DRAG_HOLD
                        && (phase == TouchPhase::Moved || touch.distance >= TAP_MAX_DISTANCE)
                    {
                        touch.start_drag(self.input.as_ref());
                    } else if touch.distance >= TAP_MAX_DISTANCE {
                        // A quick swipe after a tap is still pointer movement.
                        touch.drag_attempted = true;
                        previous = touch.origin;
                    }
                }
                let right_click = self.mode == TrackpadMode::Mouse
                    && elapsed >= RIGHT_CLICK_HOLD
                    && touch.can_right_click();
                if right_click {
                    touch.right_clicked = true;
                }
                // While the second contact is still deciding between a tap,
                // a hold, and a swipe, keep the cursor on its original target.
                if !touch.drag_pending() && point != previous {
                    let (dx, dy) = (x - previous.0, y - previous.1);
                    if touch.dragging {
                        self.input.move_drag(dx, dy);
                    } else if self.mode == TrackpadMode::Mouse
                        && !touch.right_clicked
                        && (phase == TouchPhase::Moved || touch.distance >= TAP_MAX_DISTANCE)
                    {
                        self.input.move_pointer(dx, dy);
                    }
                }
                if phase == TouchPhase::Moved {
                    touch.last = point;
                    if right_click {
                        self.right_click();
                    }
                    return;
                }
                let touch = self.touch.take().unwrap();
                if touch.dragging {
                    self.input.end_drag();
                    self.suppress_click_echo(now);
                } else if touch.right_clicked {
                    if right_click {
                        self.right_click();
                    }
                    self.suppress_click_echo(now);
                } else if touch.distance < TAP_MAX_DISTANCE
                    && !touch.drag_attempted
                    && (self.mode == TrackpadMode::Direction || elapsed < RIGHT_CLICK_HOLD)
                {
                    self.select(Some(touch.started.received), time, Some(touch.click_count));
                } else if self.mode == TrackpadMode::Direction && distance >= SWIPE_MIN_DISTANCE {
                    let button = if dx.abs() > dy.abs() {
                        if dx > 0.0 {
                            "right"
                        } else {
                            "left"
                        }
                    } else if dy > 0.0 {
                        "down"
                    } else {
                        "up"
                    };
                    self.input.button(button);
                } else if self.mode == TrackpadMode::Mouse {
                    self.last_click = None;
                }
            }
            TouchPhase::Cancelled => unreachable!(),
        }
    }
}

impl Drop for CompanionSession {
    fn drop(&mut self) {
        self.clear_touch();
        self.stop_remote();
        self.update_keyboard(None);
        if self.close_signal.is_none() {
            if let Some(controller) = self.controller {
                self.pairing
                    .release_remote(controller, self.connection_number);
            }
        }
    }
}

type Tlv = std::collections::BTreeMap<u8, Vec<u8>>;

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a [u8]> {
    value
        .get(key)
        .and_then(Value::as_bytes)
        .ok_or_else(|| Error::Protocol(format!("missing {key}")))
}

fn tlv_field(data: &Tlv, tag: u8) -> Result<&[u8]> {
    data.get(&tag)
        .map(Vec::as_slice)
        .ok_or_else(|| Error::Protocol(format!("missing pairing field {tag}")))
}

fn controller_id(data: &Tlv) -> Result<&[u8]> {
    let id = tlv_field(data, tags::IDENTIFIER)?;
    if id.is_empty() || id.len() > 256 {
        return Err(Error::Protocol("invalid controller identifier".into()));
    }
    Ok(id)
}

fn check_signature(key: &VerifyingKey, data: &Tlv, message: &[u8]) -> Result<()> {
    let signature = Signature::from_slice(tlv_field(data, tags::SIGNATURE)?)
        .map_err(|_| Error::Protocol("invalid controller signature".into()))?;
    key.verify_strict(message, &signature)
        .map_err(|_| Error::Protocol("controller signature verification failed".into()))
}

fn verification_cipher(shared: &[u8; 32]) -> Result<CompanionCipher> {
    let key = hkdf_expand(
        "Pair-Verify-Encrypt-Salt",
        "Pair-Verify-Encrypt-Info",
        shared,
    );
    CompanionCipher::new(&key, &key, 8)
}

fn awake() -> Value {
    Value::dict(vec![Value::kv("state", Value::Int(3))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc as blocking, Mutex};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::time::timeout;

    struct SlowPlayback {
        received: mpsc::UnboundedSender<&'static str>,
        resume: Mutex<blocking::Receiver<()>>,
    }

    impl RemoteInput for SlowPlayback {
        fn button(&self, _: &str) {
            self.received.send("button").unwrap();
        }

        fn move_pointer(&self, dx: f64, dy: f64) {
            assert_eq!((dx, dy), (70.0, 0.0));
            self.received.send("pointer").unwrap();
        }

        fn click(&self) {
            self.received.send("click").unwrap();
        }

        fn right_click(&self) {
            self.received.send("right click").unwrap();
        }

        fn playback_enabled(&self) -> bool {
            true
        }

        fn playback_action(&self, _: VideoSource, action: PlaybackAction) -> bool {
            assert_eq!(action, PlaybackAction::SkipForward);
            self.received.send("seek").unwrap();
            self.resume.lock().unwrap().recv().is_ok()
        }
    }

    #[tokio::test]
    async fn slow_media_keeps_pointer_live_and_drops_released_seek_repeats() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _phone = TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        let (_, writer) = server.into_split();
        let (received, mut events) = mpsc::unbounded_channel();
        let (resume, paused) = blocking::channel();
        let input = Arc::new(SlowPlayback {
            received,
            resume: Mutex::new(paused),
        });
        let directory = tempfile::tempdir().unwrap();
        let pairing = Arc::new(PairingState::open(directory.path()).unwrap());
        let (mut session, _replaced) = CompanionSession::new(
            writer,
            input,
            pairing,
            "test".into(),
            TrackpadMode::Mouse,
            None,
        );
        session.controller = Some([1; 32]);
        session.playback = Some(PlaybackState {
            source: VideoSource::Jellyfin,
            playing: Some(true),
            captions: None,
            duration_seconds: None,
        });

        async fn event(session: &mut CompanionSession, name: &str, fields: Vec<(Value, Value)>) {
            timeout(
                Duration::from_secs(1),
                session.handle_control(&Value::dict(vec![
                    Value::kv("_t", Value::Int(1)),
                    Value::kv("_i", Value::str(name)),
                    Value::kv("_c", Value::Dict(fields)),
                ])),
            )
            .await
            .expect("input must stay responsive while playback is blocked")
            .unwrap();
        }

        async fn expect(events: &mut mpsc::UnboundedReceiver<&'static str>, expected: &str) {
            let actual = timeout(Duration::from_secs(1), events.recv())
                .await
                .unwrap();
            assert_eq!(actual, Some(expected));
        }

        fn touch(phase: u64, x: f64, ns: u64) -> Vec<(Value, Value)> {
            vec![
                Value::kv("TouchPhase", Value::Int(phase)),
                Value::kv("CoordinateX", Value::Double(x)),
                Value::kv("CoordinateY", Value::Double(100.0)),
                Value::kv("Nanoseconds", Value::Int(ns)),
            ]
        }

        event(&mut session, "_mcc", vec![Value::kv("_mcc", Value::Int(8))]).await;
        expect(&mut events, "seek").await;
        session.repeat_seek();
        event(&mut session, "_mcc", vec![Value::kv("_mcc", Value::Int(9))]).await;

        event(&mut session, "HIDTouch", touch(1, 100.0, 10_000_000)).await;
        event(&mut session, "HIDTouch", touch(2, 170.0, 20_000_000)).await;
        event(&mut session, "HIDTouch", touch(3, 170.0, 30_000_000)).await;
        expect(&mut events, "pointer").await;

        event(&mut session, "HIDTouch", touch(1, 100.0, 100_000_000)).await;
        // A lift that has already reached the receiver can arrive with a
        // short phone duration even if the local hold timer is overdue.
        session.touch.as_mut().unwrap().started.received -= RIGHT_CLICK_HOLD;
        event(&mut session, "HIDTouch", touch(3, 100.0, 160_000_000)).await;
        expect(&mut events, "click").await;

        event(
            &mut session,
            "HIDCommand",
            vec![
                Value::kv("HIDCommand", Value::Int(7)),
                Value::kv("HIDButtonState", Value::Int(2)),
            ],
        )
        .await;
        resume.send(()).unwrap();
        expect(&mut events, "button").await;
    }
}
