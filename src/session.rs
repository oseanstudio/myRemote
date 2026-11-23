//! Companion Link session: pairing, verification and the encrypted control
//! channel, including legacy Rapport and TVRemoteCore input messages.

use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tokio::time::Instant;
use tracing::{debug, info};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::crypto::{hkdf_expand, CompanionCipher};
use crate::delegate::{RemoteInput, TrackpadMode};
use crate::error::{Error, Result};
use crate::opack::{self, Value};
use crate::pairing::PairingState;
use crate::remote_keyboard::KeyboardSession;
use crate::srp::SrpServer;
use crate::tlv::{self, tags};
use crate::FrameType;

pub const DEVICE_MODEL: &str = "AppleTV14,1";
pub(crate) const SOURCE_VERSION: &str = "715.2";
const REMOTE_SERVICE: &str = "com.apple.tvremoteservices";
pub const MEDIA_CONTROL_VOLUME: u64 = 0x0100;

const SWIPE_MIN_DISTANCE: f64 = 150.0; // touchpad units; the surface is 1000x1000
const TAP_MAX_DISTANCE: f64 = 60.0; // below this a touch sequence counts as a tap
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(350);
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
    mode: TrackpadMode,
    signing_key: SigningKey,
    setup: Option<(SrpServer, u64)>,
    setup_proved: bool,
    pending_verify: Option<PendingVerification>,
    cipher: Option<CompanionCipher>,
    remote: RemoteState,
    background_probe: bool,
    current_identifier: Option<String>,
    remote_session_id: u32,
    keyboard_subscribed: bool,
    keyboard: Option<KeyboardSession>,
    touch: Option<ActiveTouch>,
    last_tap: Option<Instant>,
    last_select: Option<(Instant, bool)>,
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
    ) -> Self {
        Self {
            writer,
            input,
            signing_key: SigningKey::from_bytes(&pairing.private_key),
            pairing,
            device_name,
            mode,
            setup: None,
            setup_proved: false,
            pending_verify: None,
            cipher: None,
            remote: RemoteState::Waiting,
            background_probe: false,
            current_identifier: None,
            remote_session_id: 0,
            keyboard_subscribed: false,
            keyboard: None,
            touch: None,
            last_tap: None,
            last_select: None,
            volume: 0.5,
            muted: false,
        }
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
            RemoteState::Connected | RemoteState::ReceivingInput => Some(Duration::from_secs(1800)),
            // A paired phone may reopen Remote on the same authenticated connection.
            RemoteState::Closed => Some(Duration::from_secs(5 * 60)),
            RemoteState::Waiting if self.is_verified() => Some(Duration::from_secs(15)),
            RemoteState::Waiting => None,
        }
    }

    pub(crate) fn report_disconnect(&self, error: &Error) {
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
        if let Some(xid) = xid {
            event.push(Value::kv("_x", Value::Int(xid)));
        }
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
            if frame == Some(FrameType::EOpack) {
                match opack::unpack(&payload) {
                    Ok((message, _)) => self.handle_control(&message).await?,
                    Err(error) => {
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
        self.pairing
            .set_connection_status("iPhone verified · waiting for Remote session");
        Ok(())
    }

    async fn handle_control(&mut self, data: &Value) -> Result<()> {
        self.current_identifier = data
            .get("_i")
            .and_then(Value::as_str)
            .or_else(|| data.get("model").and_then(Value::as_str))
            .map(str::to_owned);
        let xid = data.get("_x").and_then(Value::as_int);
        let is_event = data.get("_t").and_then(Value::as_int) == Some(1);
        let content = data.get("_c").unwrap_or(&Value::Null);

        match self.current_identifier.as_deref() {
            Some("_sessionStart") => {
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv(
                        "_sid",
                        Value::Int(self.remote_session_id.into()),
                    )])),
                    0,
                )
                .await?;
                self.start_remote();
            }
            Some("TVRCSessionStart") => {
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
                self.send_media_state(xid).await?;
                self.start_remote();
            }
            Some("_sessionStop" | "_touchStop" | "HIDTouchStop") => {
                self.clear_touch();
                if self.current_identifier.as_deref() == Some("_sessionStop") {
                    self.keyboard_subscribed = false;
                    if self.keyboard.take().is_some() {
                        self.send_event("_tiStopped", None, None).await?;
                    }
                    self.stop_remote();
                    self.last_select = None;
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
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
                self.handle_hid_command(content, xid).await?;
            }
            Some("_hidT" | "HIDTouch") => {
                if !is_event {
                    self.send_response(xid, None, 0).await?;
                }
                let modern = self.current_identifier.as_deref() == Some("HIDTouch");
                self.handle_touch(content, modern);
            }
            Some("_interest") => {
                if let Some(Value::Array(events)) = content.get("_regEvents") {
                    for event in events.iter().filter_map(Value::as_str) {
                        match event {
                            "MediaControlStatus" => {
                                self.send_event(event, xid, Some(media_flags())).await?
                            }
                            "NowPlayingInfo" => self.send_event(event, xid, None).await?,
                            "SystemStatus" | "TVSystemStatus" => {
                                self.send_event(event, xid, Some(awake())).await?
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
                self.send_response(xid, Some(media_flags()), 0).await?
            }
            Some("MediaControlCommand" | "_mcc") => self.handle_media_control(xid, content).await?,
            Some("_tiStart") if !is_event => {
                self.keyboard_subscribed = true;
                let focused = self.focused_text_field().await;
                self.update_keyboard(focused);
                let payload = self.keyboard.as_ref().map(KeyboardSession::focus_payload);
                self.send_response(xid, payload, 0).await?;
            }
            Some("_tiStop") if !is_event => {
                self.keyboard_subscribed = false;
                self.keyboard = None;
                self.send_response(xid, None, 0).await?;
            }
            Some("_tiC") if is_event => self.type_text(content).await,
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
        self.keyboard_subscribed && self.remote_started()
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
            self.keyboard = focus.map(KeyboardSession::new);
        }
    }

    pub(crate) async fn refresh_keyboard(&mut self) -> Result<()> {
        let focus = self.focused_text_field().await;
        if self.keyboard.as_ref().map(|keyboard| keyboard.field) != focus {
            if self.keyboard.is_some() {
                self.send_event("_tiStopped", None, None).await?;
            }
            self.update_keyboard(focus);
            if let Some(keyboard) = &self.keyboard {
                let payload = keyboard.focus_payload();
                self.send_event("_tiStarted", None, Some(payload)).await?;
            }
        }
        Ok(())
    }

    async fn type_text(&mut self, content: &Value) {
        let Some(keyboard) = &self.keyboard else {
            return;
        };
        let Some(edit) = keyboard.text_edit(content) else {
            return;
        };
        let input = self.input.clone();
        let field = keyboard.field;
        let applied = tokio::task::spawn_blocking(move || {
            input.type_text(field, edit.backspaces, edit.deletes, &edit.text)
        })
        .await
        .unwrap_or(false);
        if applied {
            self.record_input();
        }
    }

    fn system_info(&self) -> Value {
        let mac = self.pairing.mac.map(|byte| format!("{byte:02X}")).join(":");
        Value::dict(vec![
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
        ])
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
        if button == "select" {
            if self.mode == TrackpadMode::Direction || self.touch.is_none() {
                self.select(false, false);
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
        let mut status = media_flags();
        if let Value::Dict(entries) = &mut status {
            entries.push(Value::kv("Muted", Value::Bool(self.muted)));
        }
        self.send_event("MediaControlStatus", xid, Some(status))
            .await?;
        self.send_event(
            "_iMC",
            xid,
            Some(Value::dict(vec![Value::kv(
                "_mcF",
                Value::Int(MEDIA_CONTROL_VOLUME),
            )])),
        )
        .await
    }

    async fn handle_media_control(&mut self, xid: Option<u64>, content: &Value) -> Result<()> {
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
            Some(5) => {
                let volume =
                    self.input
                        .volume()
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
            Some(12) => Some(Value::dict(vec![Value::kv("_cse", Value::Bool(false))])),
            _ => None,
        };
        self.send_response(xid, response, 0).await
    }

    fn record_input(&mut self) {
        self.start_remote();
        if self.remote == RemoteState::Connected {
            self.pairing.remote_received_input();
            self.remote = RemoteState::ReceivingInput;
        }
    }

    fn start_remote(&mut self) {
        if !self.remote_started() {
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

    fn select(&mut self, from_touch: bool, double: bool) -> bool {
        // A touch tap can also generate a HID Select. Suppress the second event
        // from the other source while allowing consecutive taps from one source.
        let now = Instant::now();
        if self.last_select.is_some_and(|(time, touch)| {
            touch != from_touch && now - time < Duration::from_millis(120)
        }) {
            return false;
        }
        self.last_select = Some((now, from_touch));
        match self.mode {
            TrackpadMode::Mouse if double => self.input.double_click(),
            TrackpadMode::Mouse => self.input.click(),
            TrackpadMode::Direction => self.input.button("select"),
        }
        true
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
        }
        self.last_tap = None;
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
        let (Some(x), Some(y)) = (coordinate(x_key), coordinate(y_key)) else {
            if phase == TouchPhase::Ended {
                self.clear_touch();
            }
            return;
        };
        self.record_input();
        let point = (x, y);
        let now = Instant::now();
        match phase {
            TouchPhase::Began => {
                if self.touch.is_some() {
                    self.clear_touch();
                }
                let second_tap = self.mode == TrackpadMode::Mouse
                    && self
                        .last_tap
                        .take()
                        .is_some_and(|time| now - time < DOUBLE_TAP_WINDOW);
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
                    self.last_select = Some((now, true));
                } else if touch.right_clicked {
                    self.last_select = Some((now, true));
                } else if self.mode == TrackpadMode::Mouse
                    && !touch.second_tap
                    && touch.distance < DRAG_MOVE_DISTANCE
                    && now - touch.started >= RIGHT_CLICK_HOLD
                {
                    self.input.right_click();
                    self.last_select = Some((now, true));
                } else if touch.distance < TAP_MAX_DISTANCE
                    && !touch.drag_attempted
                    && (self.mode == TrackpadMode::Direction
                        || now - touch.started < RIGHT_CLICK_HOLD)
                {
                    if self.select(true, touch.second_tap)
                        && !touch.second_tap
                        && self.mode == TrackpadMode::Mouse
                    {
                        self.last_tap = Some(now);
                    }
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

fn media_flags() -> Value {
    Value::dict(vec![Value::kv(
        "MediaControlFlags",
        Value::Int(MEDIA_CONTROL_VOLUME),
    )])
}

fn awake() -> Value {
    Value::dict(vec![Value::kv("state", Value::Int(3))])
}
