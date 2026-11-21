//! Companion Link session: pairing, verification and the encrypted control
//! channel. Ported from `fake_atv.py`'s `CompanionSession`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::OwnedWriteHalf;
use tracing::info;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};

use crate::crypto::{hkdf_expand, CompanionCipher};
use crate::delegate::{RemoteInput, TrackpadMode};
use crate::error::{Error, Result};
use crate::opack::{self, Value};
use crate::pairing::PairingState;
use crate::srp::SrpServer;
use crate::tlv::{self, tags};
use crate::FrameType;

pub const DEVICE_MODEL: &str = "AppleTV14,1";
pub const MEDIA_CONTROL_VOLUME: u64 = 0x0100;

const SWIPE_MIN_DISTANCE: f64 = 150.0; // touchpad units; the surface is 1000x1000
const TAP_MAX_DISTANCE: f64 = 60.0; // below this a touch sequence counts as a tap

struct PendingVerification {
    shared: [u8; 32],
    client_public: [u8; 32],
    server_public: [u8; 32],
}

fn hid_command_name(command: u64) -> String {
    match command {
        1 => "up".into(),
        2 => "down".into(),
        3 => "left".into(),
        4 => "right".into(),
        5 => "menu".into(),
        6 => "select".into(),
        7 => "home".into(),
        8 => "volume_up".into(),
        9 => "volume_down".into(),
        10 => "siri".into(),
        11 => "screensaver".into(),
        12 => "sleep".into(),
        13 => "wake".into(),
        14 => "play_pause".into(),
        15 => "channel_up".into(),
        16 => "channel_down".into(),
        17 => "guide".into(),
        // iOS 26/27 repurposes the old PageUp/PageDown wire codes for the
        // Control Center remote's Mute and Power buttons.
        18 => "mute".into(),
        19 => "power".into(),
        other => format!("unknown:{other}"),
    }
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
    remote_started: bool,
    current_identifier: Option<String>,
    remote_session_id: u32,
    touch_origin: Option<(f64, f64)>,
    touch_last: Option<(f64, f64)>,
    touch_distance: f64,
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
            remote_started: false,
            current_identifier: None,
            remote_session_id: rand::random(),
            touch_origin: None,
            touch_last: None,
            touch_distance: 0.0,
            last_select: None,
            volume: 0.5,
            muted: false,
        }
    }

    pub(crate) fn is_verified(&self) -> bool {
        self.cipher.is_some()
    }

    pub(crate) fn remote_started(&self) -> bool {
        self.remote_started
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
            Value::kv("_rT", Value::Int(result_type)),
            Value::kv("_t", Value::Int(3)),
        ];
        if let Some(identifier) = &self.current_identifier {
            response.push(Value::kv("_i", Value::str(identifier)));
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
        frame: FrameType,
        payload: &[u8],
        header: &[u8],
    ) -> Result<()> {
        if frame == FrameType::EOpack {
            let cipher = self
                .cipher
                .as_mut()
                .ok_or_else(|| Error::Protocol("verify before sending input".into()))?;
            let payload = cipher.decrypt(payload, Some(header), None)?;
            let (message, _) = opack::unpack(&payload)?;
            return self.handle_control(&message).await;
        }
        if !matches!(
            frame,
            FrameType::PsStart | FrameType::PsNext | FrameType::PvStart | FrameType::PvNext
        ) {
            return Ok(());
        }
        if self.is_verified() {
            return Err(Error::Protocol("authorization is already complete".into()));
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
        self.remote_session_id = u32::from_le_bytes(session_id[..4].try_into().unwrap());
        self.auth_reply(FrameType::PvNext, &[(tags::SEQ_NO, &[4])])
            .await?;
        self.cipher = Some(CompanionCipher::new(
            &hkdf_expand("", "ServerEncrypt-main", &pending.shared),
            &hkdf_expand("", "ClientEncrypt-main", &pending.shared),
            12,
        )?);
        self.pairing.set_connection_status("iPhone verified");
        Ok(())
    }

    async fn handle_control(&mut self, data: &Value) -> Result<()> {
        self.current_identifier = data
            .get("_i")
            .or_else(|| data.get("model"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let xid = data.get("_x").and_then(Value::as_int);
        let content = data.get("_c").unwrap_or(&Value::Null);

        match self.current_identifier.as_deref() {
            Some("_sessionStart") => {
                self.remote_started = true;
                self.pairing
                    .set_connection_status("iPhone connected successfully");
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
                self.remote_started = true;
                self.pairing
                    .set_connection_status("iPhone connected successfully");
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
            }
            Some("_sessionStop" | "_touchStop" | "_tiStop") => {
                if self.current_identifier.as_deref() == Some("_sessionStop") {
                    self.remote_started = false;
                }
                self.touch_origin = None;
                self.touch_last = None;
                self.send_response(xid, None, 0).await?;
            }
            Some("_touchStart") => {
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv("_i", Value::Int(1))])),
                    0,
                )
                .await?;
            }
            Some("_hidC") => {
                self.send_response(xid, None, 0).await?;
                if content.get("_hBtS").and_then(Value::as_int) == Some(2) {
                    let button =
                        hid_command_name(content.get("_hidC").and_then(Value::as_int).unwrap_or(0));
                    if button == "select" {
                        self.select(false);
                    } else {
                        self.input.button(&button);
                    }
                    match button.as_str() {
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
                }
            }
            Some("_hidT") => {
                self.send_response(xid, None, 0).await?;
                self.handle_touch(content);
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
                self.send_response(xid, None, 0).await?;
                self.send_event("SystemStatus", None, Some(awake())).await?;
                self.send_event("TVSystemStatus", None, Some(awake()))
                    .await?;
            }
            Some("FetchAttentionState") => self.send_response(xid, Some(awake()), 0).await?,
            Some("FetchMediaControlStatus") => {
                self.send_response(xid, Some(media_flags()), 0).await?
            }
            Some("MediaControlCommand" | "_mcc") => self.handle_media_control(xid, content).await?,
            Some("_tiStart") => {
                self.send_response(
                    xid,
                    Some(Value::dict(vec![Value::kv("_tiE", Value::Bool(false))])),
                    0,
                )
                .await?;
            }
            Some("FetchLaunchableApplicationsEvent") => self.send_response(xid, None, 2).await?,
            _ if xid.is_some() => self.send_response(xid, None, 0).await?,
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
                Some(Value::dict(vec![Value::kv("_vol", Value::Double(volume))]))
            }
            Some(6) => {
                if let Some(volume) = content
                    .get("_vol")
                    .and_then(Value::as_float)
                    .filter(|v| v.is_finite())
                {
                    self.volume = volume.clamp(0.0, 1.0);
                    self.muted = false;
                    self.input.set_volume(self.volume);
                }
                None
            }
            Some(12) => Some(Value::dict(vec![Value::kv("_cse", Value::Bool(false))])),
            _ => None,
        };
        self.send_response(xid, response, 0).await
    }

    fn select(&mut self, from_touch: bool) {
        // A touch tap can also generate a HID Select. Suppress the second event
        // from the other source while allowing consecutive taps from one source.
        let now = Instant::now();
        if self.last_select.is_some_and(|(time, touch)| {
            touch != from_touch && now - time < Duration::from_millis(120)
        }) {
            return;
        }
        self.last_select = Some((now, from_touch));
        match self.mode {
            TrackpadMode::Mouse => self.input.click(),
            TrackpadMode::Direction => self.input.button("select"),
        }
    }

    fn handle_touch(&mut self, content: &Value) {
        let phase = content.get("_tPh").and_then(Value::as_int);
        let coordinate = |key| {
            content
                .get(key)
                .and_then(Value::as_float)
                .filter(|value| value.is_finite() && (-4000.0..=4000.0).contains(value))
        };
        let (Some(x), Some(y)) = (coordinate("_cx"), coordinate("_cy")) else {
            if phase == Some(4) {
                self.touch_origin = None;
                self.touch_last = None;
            }
            return;
        };
        let point = (x, y);
        match phase {
            Some(1) => {
                self.touch_origin = Some(point);
                self.touch_last = Some(point);
                self.touch_distance = 0.0;
            }
            Some(2..=4) => {
                let (Some(origin), Some(last)) = (self.touch_origin, self.touch_last) else {
                    return;
                };
                let dx = x - origin.0;
                let dy = y - origin.1;
                let distance = dx.abs().max(dy.abs());
                self.touch_distance = self.touch_distance.max(distance);
                if self.mode == TrackpadMode::Mouse
                    && (phase != Some(4) || self.touch_distance >= TAP_MAX_DISTANCE)
                {
                    self.input.move_pointer(x - last.0, y - last.1);
                }
                if phase != Some(4) {
                    self.touch_last = Some(point);
                    return;
                }
                self.touch_origin = None;
                self.touch_last = None;
                if self.touch_distance < TAP_MAX_DISTANCE {
                    self.select(true);
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
            _ => {}
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

fn media_flags() -> Value {
    Value::dict(vec![Value::kv(
        "MediaControlFlags",
        Value::Int(MEDIA_CONTROL_VOLUME),
    )])
}

fn awake() -> Value {
    Value::dict(vec![Value::kv("state", Value::Int(3))])
}
