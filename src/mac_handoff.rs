mod audio;
mod players;

use std::io::{self, BufRead, Read, Write};
use std::net::{Ipv4Addr, TcpListener};
use std::process::{ExitCode, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use openairplay1::{Event, Receiver};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

pub use players::{continue_playback, Destination, Outcome};

const HELPER: &str = "--music-handoff-receiver";
const SUFFIX: &str = " · Music Handoff";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Track {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    pub position_ms: u64,
    pub duration_ms: u64,
}

impl Track {
    pub fn same_recording(&self, other: &Self) -> bool {
        self.title == other.title
            && self.artist == other.artist
            && self.album == other.album
            && self.duration_ms == other.duration_ms
    }
}

#[derive(Default)]
pub struct State {
    pub track: Option<Track>,
    pub status: Option<&'static str>,
}

pub struct Handoff {
    pub name: String,
    pub password: String,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

#[derive(Serialize, Deserialize)]
struct Setup {
    name: String,
    mac: [u8; 6],
    password: String,
}

#[derive(Serialize, Deserialize)]
enum Update {
    Ready,
    WaitingForPosition,
    WaitingForPhone,
    Track(Track),
}

impl Handoff {
    pub fn start(runtime: &Handle, receiver_name: &str) -> io::Result<Self> {
        let mut name = receiver_name.to_owned();
        // Bonjour allows 63 bytes, including the RAOP MAC and @ prefix.
        while name.len() + SUFFIX.len() > 50 {
            name.pop();
        }
        name.push_str(SUFFIX);
        let mut mac: [u8; 6] = rand::random();
        mac[0] = (mac[0] & !1) | 2;
        let password = rand::thread_rng().gen_range(100_000..=999_999).to_string();
        let setup = Setup {
            name: name.clone(),
            mac,
            password: password.clone(),
        };
        let mut child = Command::new(std::env::current_exe()?)
            .arg(HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let mut input = child.stdin.take().expect("handoff stdin is piped");
        let mut events =
            BufReader::new(child.stdout.take().expect("handoff stdout is piped")).lines();
        let state = Arc::new(Mutex::new(State {
            track: None,
            status: Some("Starting Music Handoff…"),
        }));
        let shared = Arc::clone(&state);
        let task = runtime.spawn(async move {
            let mut settings = serde_json::to_vec(&setup).expect("receiver settings serialize");
            settings.push(b'\n');
            if input.write_all(&settings).await.is_ok() {
                // Keep stdin open as the helper's parent-lifetime signal.
                while let Ok(Some(line)) = events.next_line().await {
                    let Ok(update) = serde_json::from_str(&line) else {
                        continue;
                    };
                    let mut state = shared.lock().unwrap();
                    match update {
                        Update::Ready | Update::WaitingForPhone => {
                            state.track = None;
                            state.status = Some("Waiting for iPhone music");
                        }
                        Update::WaitingForPosition => {
                            state.track = None;
                            state.status = Some("Waiting for song and position");
                        }
                        Update::Track(track) => {
                            state.track = Some(track);
                            state.status = None;
                        }
                    }
                }
            }
            *shared.lock().unwrap() = State {
                track: None,
                status: Some("Music Handoff receiver stopped"),
            };
            let _ = child.wait().await;
        });
        Ok(Self {
            name,
            password,
            state,
            task,
        })
    }

    pub fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    pub fn running(&self) -> bool {
        !self.task.is_finished()
    }
}

impl Drop for Handoff {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn run_helper() -> Option<ExitCode> {
    if std::env::args_os().nth(1).as_deref() != Some(HELPER.as_ref()) {
        return None;
    }
    let result = (|| {
        let mut input = io::BufReader::new(io::stdin());
        let mut line = String::new();
        (&mut input).take(2048).read_line(&mut line)?;
        let setup: Setup = serde_json::from_str(&line)?;
        let (closed, parent) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = input.read(&mut [0]);
            let _ = closed.send(());
        });
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(async {
                tokio::select! {
                    result = receive(setup) => result,
                    _ = parent => Ok(()),
                }
            })
    })();
    Some(if result.is_ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn receive(setup: Setup) -> io::Result<()> {
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?
        .local_addr()?
        .port();
    let receiver = Receiver::builder()
        .name(setup.name)
        .mac(setup.mac)
        .port(port)
        .password(setup.password)
        .advertise(false)
        .build()?;
    let name = receiver.config().service_name();
    let txt = receiver
        .txt_records()
        .iter()
        .filter_map(|record| {
            record
                .split_once('=')
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
        })
        .collect();
    let record = myremote::DiscoveryRecord {
        service_type: "_raop._tcp".into(),
        port,
        txt,
    };
    let volume = Arc::new(AtomicU32::new(1.0f32.to_bits()));
    let output_volume = Arc::clone(&volume);
    let (send, mut events) = tokio::sync::mpsc::unbounded_channel();
    let server = receiver.run(
        move |rate, channels| {
            Box::new(audio::Speaker::new(
                rate,
                channels,
                Arc::clone(&output_volume),
            ))
        },
        send,
    );
    let pump = async {
        // Run registration in parallel with the listener, then announce readiness
        // only when Bonjour has accepted the record.
        let bonjour = crate::mac_bonjour::Bonjour::start(&name, None, vec![record])?;
        let start = async {
            loop {
                match bonjour.status() {
                    crate::mac_bonjour::Status::Ready => return Ok(()),
                    crate::mac_bonjour::Status::Failed
                    | crate::mac_bonjour::Status::LocalNetworkPermission => {
                        return Err(io::Error::other(
                            "Music Handoff Bonjour registration failed",
                        ))
                    }
                    _ => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(8), start).await??;
        publish(Update::Ready)?;
        let mut now_playing = NowPlaying::default();
        while let Some(event) = events.recv().await {
            if let Event::Volume { db } = event {
                let gain = if db.is_finite() {
                    10.0f32.powf(db.clamp(-144.0, 0.0) / 20.0)
                } else {
                    1.0
                };
                volume.store(gain.to_bits(), Ordering::Relaxed);
            } else if let Some(update) = now_playing.apply(event) {
                publish(update)?;
            }
        }
        Ok(())
    };
    tokio::select! { result = server => result, result = pump => result }
}

fn publish(update: Update) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, &update)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}

#[derive(Default)]
struct NowPlaying {
    metadata: Option<(String, String, Option<String>)>,
}

impl NowPlaying {
    fn apply(&mut self, event: Event) -> Option<Update> {
        match event {
            Event::SessionStarted { .. } => {
                self.metadata = None;
                Some(Update::WaitingForPosition)
            }
            Event::SessionEnded => {
                self.metadata = None;
                Some(Update::WaitingForPhone)
            }
            Event::Metadata {
                title,
                artist,
                album,
            } => {
                let metadata = clean(title)
                    .zip(clean(artist))
                    .map(|(title, artist)| (title, artist, clean(album)));
                if metadata == self.metadata {
                    return None;
                }
                self.metadata = metadata;
                Some(Update::WaitingForPosition)
            }
            Event::Flushed => Some(Update::WaitingForPosition),
            Event::Progress { elapsed, duration } if !duration.is_zero() && elapsed < duration => {
                let (title, artist, album) = self.metadata.clone()?;
                Some(Update::Track(Track {
                    title,
                    artist,
                    album,
                    position_ms: elapsed.as_millis().try_into().ok()?,
                    duration_ms: duration.as_millis().try_into().ok()?,
                }))
            }
            _ => None,
        }
    }
}

fn clean(value: Option<String>) -> Option<String> {
    let value = value?;
    let value = value.trim();
    (!value.is_empty() && value.len() <= 512 && !value.chars().any(char::is_control))
        .then(|| value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handoff_requires_fresh_progress_after_a_song_change_or_seek() {
        let mut state = NowPlaying::default();
        let progress = || Event::Progress {
            elapsed: Duration::from_secs(45),
            duration: Duration::from_secs(200),
        };
        assert!(state.apply(progress()).is_none());
        let metadata = |title: &str| Event::Metadata {
            title: Some(title.into()),
            artist: Some("Artist".into()),
            album: None,
        };
        assert!(matches!(
            state.apply(metadata("First")),
            Some(Update::WaitingForPosition)
        ));
        assert!(
            matches!(state.apply(progress()), Some(Update::Track(track)) if track.title == "First" && track.position_ms == 45_000)
        );
        assert!(matches!(
            state.apply(Event::Flushed),
            Some(Update::WaitingForPosition)
        ));
        assert!(matches!(
            state.apply(metadata("Second")),
            Some(Update::WaitingForPosition)
        ));
        assert!(
            matches!(state.apply(progress()), Some(Update::Track(track)) if track.title == "Second")
        );
        assert!(matches!(
            state.apply(Event::SessionEnded),
            Some(Update::WaitingForPhone)
        ));
        assert!(state.apply(progress()).is_none());
    }
}
