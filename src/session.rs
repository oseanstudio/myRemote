//! Companion Link session: pairing, verification and the encrypted control
//! channel, including legacy Rapport and TVRemoteCore input messages.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{debug, info};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::crypto::{hkdf_expand, CompanionCipher};
use crate::delegate::{RemoteInput, TrackpadMode};
use crate::error::{Error, Result};
use crate::opack::{self, Value};
use crate::pairing::PairingState;
use crate::remote_keyboard::KeyboardSession;
use crate::remote_media;
use crate::srp::SrpServer;
use crate::tlv::{self, tags};
use crate::{FrameType, PlaybackAction, PlaybackState, VideoSource};

pub const DEVICE_MODEL: &str = "AppleTV14,1";
pub(crate) const SOURCE_VERSION: &str = "715.2";
const REMOTE_SERVICE: &str = "com.apple.tvremoteservices";
const SWIPE_MIN_DISTANCE: f64 = 150.0; // touchpad units; the surface is 1000x1000
const TAP_MAX_DISTANCE: f64 = 60.0; // below this a touch sequence counts as a tap
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(500);
const SELECT_ECHO_WINDOW: Duration = Duration::from_millis(90);
const RIGHT_CLICK_HOLD: Duration = Duration::from_millis(500);
const DRAG_HOLD: Duration = Duration::from_millis(200);
const DRAG_IDLE_LIMIT: Duration = Duration::from_secs(10);
const DRAG_MOVE_DISTANCE: f64 = 24.0;

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
    started: Instant,
    updated: Instant,
    distance: f64,
    second_tap: bool,
    drag_attempted: bool,
    dragging: bool,
    right_clicked: bool,
}

#[derive(Clone, Copy)]
struct TapTime {
    received: Instant,
    phone_ns: Option<u64>,
}

impl TapTime {
    fn follows(self, previous: Self, within: Duration) -> bool {
        if let (Some(previous), Some(current)) = (previous.phone_ns, self.phone_ns) {
            if let Some(elapsed) = current.checked_sub(previous).filter(|elapsed| *elapsed > 0) {
                return Duration::from_nanos(elapsed) < within;
            }
        }
        self.received.saturating_duration_since(previous.received) < within
    }
}

struct Click {
    time: TapTime,
    sources: u8,
    double: bool,
}

struct SeekHold {
    action: PlaybackAction,
    next: Instant,
    expires: Instant,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemoteState {
    Waiting,
    Connected,
    ReceivingInput,
    Closed,
}

/// One Companion connection's state machine.
pub(crate) struct CompanionSession {
    writer: OwnedWriteHalf,
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
    focus_missing: bool,
    playback: Option<PlaybackState>,
    seek_hold: Option<SeekHold>,
    next_event_id: u32,
    touch: Option<ActiveTouch>,
    last_click: Option<Click>,
    suppress_select_until: Option<Instant>,
    volume: f64,
    muted: bool,
}

impl CompanionSession {
    pub(crate) fn new(
        writer: OwnedWriteHalf,
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
            writer,
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
            focus_missing: false,
            playback: None,
            seek_hold: None,
            next_event_id: 0x8000_0000 | rand::random::<u32>(),
            touch: None,
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
                let key = if self.current_identifier.as_deref() == Some("HIDTouchStart") {
                    "Identifier"
                } else {
                    "_i"
                };
                self.clear_touch();
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
                self.apply_playback_action(PlaybackAction::ToggleCaptions)
                    .await;
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
            }
            Some("_tiStop") => {
                self.keyboard_subscribed = false;
                self.update_keyboard(None);
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
            }
            Some("_tiC") => {
                self.type_text(if matches!(content, Value::Null) {
                    data
                } else {
                    content
                })
                .await;
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

    async fn apply_playback_action(&mut self, action: PlaybackAction) {
        let Some(playback) = self.playback else {
            return;
        };
        let input = self.input.clone();
        if tokio::task::spawn_blocking(move || input.playback_action(playback.source, action))
            .await
            .unwrap_or(false)
        {
            self.record_input();
        }
    }

    pub(crate) fn seek_deadline(&self) -> Option<Instant> {
        self.seek_hold
            .as_ref()
            .map(|hold| hold.next.min(hold.expires))
    }

    pub(crate) async fn repeat_seek(&mut self) {
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
        self.apply_playback_action(action).await;
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
                fields.insert((controller, keyboard.id), keyboard.field);
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
                self.pairing
                    .diagnostics
                    .keyboard_focuses
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    async fn type_text(&mut self, content: &Value) {
        let diagnostics = &self.pairing.diagnostics;
        diagnostics
            .keyboard_messages
            .fetch_add(1, Ordering::Relaxed);
        let Some(edit) = KeyboardSession::text_edit(content) else {
            return;
        };
        let controller = self
            .controller
            .expect("keyboard is only available after verification");
        // Rapport can carry text input on a separate connection verified with
        // the same pairing. The phone must still present the offered field UUID.
        let pairing = self.pairing.clone();
        let input = self.input.clone();
        let applied = tokio::task::spawn_blocking(move || {
            let field = pairing
                .keyboard_fields
                .lock()
                .unwrap()
                .get(&(controller, edit.target))
                .copied();
            let Some(field) = field else {
                return false;
            };
            pairing
                .diagnostics
                .keyboard_edits
                .fetch_add(1, Ordering::Relaxed);
            input.type_text(field, edit.backspaces, edit.deletes, &edit.text)
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
        if content.get(state_key).and_then(Value::as_int) != Some(2) {
            return Ok(());
        }
        let Some(button) = content
            .get(command_key)
            .and_then(Value::as_int)
            .and_then(hid_command_name)
        else {
            return Ok(());
        };
        self.record_input();
        if button == "menu" {
            self.pairing
                .diagnostics
                .back_presses
                .fetch_add(1, Ordering::Relaxed);
        }
        if button == "select" {
            if self.mode == TrackpadMode::Direction || self.touch.is_none() {
                self.select(
                    None,
                    TapTime {
                        received: Instant::now(),
                        phone_ns: None,
                    },
                    false,
                );
            }
        } else if matches!(button, "play_pause" | "home" | "menu") {
            let input = self.input.clone();
            let _ = tokio::task::spawn_blocking(move || input.button(button)).await;
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
                self.apply_playback_action(action).await;
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
                    self.apply_playback_action(action).await;
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
                    next: now,
                    expires: now + Duration::from_secs(8),
                });
                self.repeat_seek().await;
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
                    self.apply_playback_action(PlaybackAction::SetCaptions(*enabled))
                        .await;
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

    fn select(&mut self, touch_started: Option<Instant>, time: TapTime, continuation: bool) {
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
        let double = self.mode == TrackpadMode::Mouse && (continuation || self.is_second_tap(time));
        self.last_click = Some(Click {
            time,
            sources: source,
            double,
        });
        match self.mode {
            TrackpadMode::Mouse if double => {
                self.input.double_click();
                self.pairing
                    .diagnostics
                    .double_clicks
                    .fetch_add(1, Ordering::Relaxed);
            }
            TrackpadMode::Mouse => self.input.click(),
            TrackpadMode::Direction => self.input.button("select"),
        }
    }

    fn is_second_tap(&self, time: TapTime) -> bool {
        self.mode == TrackpadMode::Mouse
            && self
                .last_click
                .as_ref()
                .is_some_and(|last| !last.double && time.follows(last.time, DOUBLE_TAP_WINDOW))
    }

    fn suppress_click_echo(&mut self, now: Instant) {
        self.last_click = None;
        self.suppress_select_until = Some(now + SELECT_ECHO_WINDOW);
    }

    pub(crate) fn touch_deadline(&self) -> Option<Instant> {
        let touch = self.touch.as_ref()?;
        if touch.dragging {
            Some(touch.updated + DRAG_IDLE_LIMIT)
        } else if touch.second_tap && !touch.drag_attempted {
            Some(touch.started + DRAG_HOLD)
        } else if self.mode == TrackpadMode::Mouse
            && !touch.second_tap
            && !touch.right_clicked
            && touch.distance < DRAG_MOVE_DISTANCE
        {
            Some(touch.started + RIGHT_CLICK_HOLD)
        } else {
            None
        }
    }

    pub(crate) fn touch_timer(&mut self) {
        if self.touch.as_ref().is_some_and(|touch| touch.dragging) {
            self.clear_touch();
        } else if let Some(touch) = &mut self.touch {
            if touch.second_tap {
                touch.drag_attempted = true;
                touch.dragging = self.input.start_drag();
            } else {
                touch.right_clicked = true;
                self.input.right_click();
            }
        }
    }

    fn clear_touch(&mut self) {
        if self.touch.take().is_some_and(|touch| touch.dragging) {
            self.input.end_drag();
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
                if self.touch.is_some() {
                    self.clear_touch();
                }
                let second_tap = self.is_second_tap(time);
                self.touch = Some(ActiveTouch {
                    origin: point,
                    last: point,
                    started: now,
                    updated: now,
                    distance: 0.0,
                    second_tap,
                    drag_attempted: false,
                    dragging: false,
                    right_clicked: false,
                });
            }
            TouchPhase::Moved | TouchPhase::Ended => {
                let Some(touch) = &mut self.touch else {
                    return;
                };
                let dx = x - touch.origin.0;
                let dy = y - touch.origin.1;
                let distance = dx.abs().max(dy.abs());
                touch.distance = touch.distance.max(distance);
                touch.updated = now;
                if touch.second_tap && !touch.drag_attempted {
                    if phase == TouchPhase::Moved && now - touch.started >= DRAG_HOLD {
                        touch.drag_attempted = true;
                        touch.dragging = self.input.start_drag();
                    } else if touch.distance >= DRAG_MOVE_DISTANCE {
                        // A quick swipe after a tap is still pointer movement.
                        touch.drag_attempted = true;
                    }
                }
                if touch.dragging {
                    self.input.move_drag(x - touch.last.0, y - touch.last.1);
                } else if self.mode == TrackpadMode::Mouse
                    && !touch.right_clicked
                    && (phase == TouchPhase::Moved || touch.distance >= TAP_MAX_DISTANCE)
                {
                    self.input.move_pointer(x - touch.last.0, y - touch.last.1);
                }
                if phase == TouchPhase::Moved {
                    touch.last = point;
                    return;
                }
                let touch = self.touch.take().unwrap();
                if touch.dragging {
                    self.input.end_drag();
                    self.suppress_click_echo(now);
                } else if touch.right_clicked {
                    self.suppress_click_echo(now);
                } else if self.mode == TrackpadMode::Mouse
                    && !touch.second_tap
                    && touch.distance < DRAG_MOVE_DISTANCE
                    && now - touch.started >= RIGHT_CLICK_HOLD
                {
                    self.input.right_click();
                    self.suppress_click_echo(now);
                } else if touch.distance < TAP_MAX_DISTANCE
                    && !touch.drag_attempted
                    && (self.mode == TrackpadMode::Direction
                        || now - touch.started < RIGHT_CLICK_HOLD)
                {
                    self.select(Some(touch.started), time, touch.second_tap);
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
