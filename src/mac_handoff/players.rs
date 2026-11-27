use std::io::{self, Write};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::Track;

#[derive(Clone, Copy)]
pub enum Destination {
    Music,
    YouTubeMusic,
    Shortcut,
}

pub enum Outcome {
    Playing,
    Browser(String),
}

const MUSIC_SCRIPT: &str = r#"
on run argv
    set wantedTitle to item 1 of argv
    set wantedArtist to item 2 of argv
    set wantedAlbum to item 3 of argv
    set seekTime to (item 4 of argv as integer) / 1000.0
    set wantedDuration to (item 5 of argv as integer) / 1000.0
    tell application id "com.apple.Music"
        set candidates to every track of library playlist 1 whose name is wantedTitle
        set best to missing value
        set bestScore to 100
        repeat with candidate in candidates
            set candidateTrack to contents of candidate
            if (artist of candidateTrack as text) is wantedArtist then
                set difference to (duration of candidateTrack) - wantedDuration
                if difference < 0 then set difference to -difference
                if difference is less than or equal to 4 and (duration of candidateTrack) > seekTime then
                    set score to difference
                    if wantedAlbum is not "" and (album of candidateTrack as text) is not wantedAlbum then set score to score + 10
                    if score < bestScore then
                        set best to candidateTrack
                        set bestScore to score
                    end if
                end if
            end if
        end repeat
        if best is missing value then return "no-match"
        set wantedID to persistent ID of best
        play best
        repeat 40 times
            if player state is playing then
                if persistent ID of current track is wantedID then
                    set player position to seekTime
                    delay 0.2
                    set seekDifference to player position - seekTime
                    if seekDifference > -2 and seekDifference < 3 then
                        activate
                        return "playing"
                    end if
                    return "seek-failed"
                end if
            end if
            delay 0.2
        end repeat
        return "seek-failed"
    end tell
end run
"#;

pub async fn continue_playback(destination: Destination, track: Track) -> io::Result<Outcome> {
    match destination {
        Destination::Music => music(&track).await,
        Destination::YouTubeMusic => youtube_music(&track).await,
        Destination::Shortcut => shortcut(&track).await,
    }
}

async fn music(track: &Track) -> io::Result<Outcome> {
    let output = timeout(
        Duration::from_secs(45),
        Command::new("/usr/bin/osascript")
            .args([
                "-e",
                MUSIC_SCRIPT,
                &track.title,
                &track.artist,
                track.album.as_deref().unwrap_or(""),
            ])
            .arg(track.position_ms.to_string())
            .arg(track.duration_ms.to_string())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if !output.status.success() {
        if String::from_utf8_lossy(&output.stderr).contains("-1743") {
            return Err(io::Error::other("Allow myRemote to control Music in System Settings → Privacy & Security → Automation."));
        }
        return Err(io::Error::other("Music could not start the track. Check that Music is open and can play the song normally."));
    }
    match String::from_utf8_lossy(&output.stdout).trim() {
        "playing" => Ok(Outcome::Playing),
        "no-match" => Err(io::Error::other("No matching recording in your Music library. Add the song to your library and try again.")),
        _ => Err(io::Error::other("Music did not confirm that it reached the requested position.")),
    }
}

async fn youtube_music(track: &Track) -> io::Result<Outcome> {
    // Search the public YouTube Music catalog. This does not send browser cookies
    // or account credentials. The desktop browser owns playback and sign-in.
    let body = json!({
        "context": {"client": {
            "clientName": "WEB_REMIX",
            "clientVersion": "1.20251101.01.00",
            "hl": "en",
        }},
        "query": format!("{} {}", track.title, track.artist),
        "params": "EgWKAQIIAWoKEAMQBBAJEAoQBQ%3D%3D",
    })
    .to_string();
    let mut process = Command::new("/usr/bin/curl")
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "12",
            "--max-filesize",
            "4194304",
            "--header",
            "Content-Type: application/json",
            "--header",
            "Origin: https://music.youtube.com",
            "--user-agent",
            "Mozilla/5.0",
            "--data-binary",
            "@-",
            "https://music.youtube.com/youtubei/v1/search?prettyPrint=false",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    process
        .stdin
        .take()
        .expect("catalog request stdin is piped")
        .write_all(body.as_bytes())
        .await?;
    let response = timeout(Duration::from_secs(15), process.wait_with_output()).await??;
    if !response.status.success() {
        return Err(io::Error::other("YouTube Music search is unavailable. Check the Mac’s internet connection and try again."));
    }
    let results: Value = serde_json::from_slice(&response.stdout)?;
    let video = find_song(&results, track).ok_or_else(|| {
        io::Error::other("YouTube Music returned no matching song with a compatible duration.")
    })?;
    Ok(Outcome::Browser(format!(
        "https://music.youtube.com/watch?v={video}&t={}",
        track.position_ms / 1000
    )))
}

fn find_song<'a>(results: &'a Value, track: &Track) -> Option<&'a str> {
    let sections = results.pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")?.as_array()?;
    let title = track.title.to_lowercase();
    let artist = track.artist.to_lowercase();
    let album = track.album.as_ref().map(|album| album.to_lowercase());
    sections
        .iter()
        .filter_map(|section| section.pointer("/musicShelfRenderer/contents")?.as_array())
        .flatten()
        .filter_map(|result| {
            let item = result.get("musicResponsiveListItemRenderer")?;
            let id = item.pointer("/playlistItemData/videoId")?.as_str()?;
            if id.len() != 11
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            {
                return None;
            }
            let name = item
                .pointer(
                    "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text",
                )?
                .as_str()?;
            if name.to_lowercase() != title {
                return None;
            }
            let details: Vec<_> = item
                .pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")?
                .as_array()?
                .iter()
                .filter_map(|run| run.get("text")?.as_str())
                .collect();
            if !details.iter().any(|part| part.to_lowercase() == artist) {
                return None;
            }
            let duration = details.iter().find_map(|part| timestamp_ms(part))?;
            let difference = duration.abs_diff(track.duration_ms);
            if difference > 4000 || track.position_ms >= duration {
                return None;
            }
            let album_match = album
                .as_ref()
                .is_some_and(|album| details.iter().any(|part| part.to_lowercase() == *album));
            Some(((u8::from(!album_match), difference), id))
        })
        .min_by_key(|(rank, _)| *rank)
        .map(|(_, id)| id)
}

fn timestamp_ms(value: &str) -> Option<u64> {
    let (minutes, seconds) = value.split_once(':')?;
    let seconds: u64 = seconds.parse().ok()?;
    let minutes: u64 = minutes.parse().ok()?;
    if seconds < 60 {
        minutes
            .checked_mul(60)?
            .checked_add(seconds)?
            .checked_mul(1000)
    } else {
        None
    }
}

async fn shortcut(track: &Track) -> io::Result<Outcome> {
    let mut input = tempfile::NamedTempFile::with_suffix(".json")?;
    serde_json::to_writer(&mut input, track)?;
    input.flush()?;
    let result = timeout(
        Duration::from_secs(60),
        Command::new("/usr/bin/shortcuts")
            .args(["run", "myRemote Continue Playback", "--input-path"])
            .arg(input.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    if result.status.success() && String::from_utf8_lossy(&result.stdout).trim() == "playing" {
        Ok(Outcome::Playing)
    } else {
        Err(io::Error::other("The shortcut must be named “myRemote Continue Playback” and return the text “playing” after seeking."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_matches_title_artist_album_and_length_before_building_a_link() {
        fn result(id: &str, artist: &str, album: &str, length: &str) -> Value {
            json!({"musicResponsiveListItemRenderer": {
                "playlistItemData": {"videoId": id},
                "flexColumns": [
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Wonderwall"}]}}},
                    {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                        {"text": artist}, {"text": " • "}, {"text": album}, {"text": " • "}, {"text": length}
                    ]}}}
                ]
            }})
        }
        let results = json!({"contents": {"tabbedSearchResultsRenderer": {"tabs": [
            {"tabRenderer": {"content": {"sectionListRenderer": {"contents": [
                {"musicShelfRenderer": {"contents": [
                    result("WRONGARTIST", "Cover band", "Original", "4:19"),
                    result("WRONGLENGTH", "Oasis", "Original", "3:00"),
                    result("FVdjZYfDuLE", "Oasis", "Deluxe", "4:19"),
                    result("hpSrLjc5SMs", "Oasis", "Original", "4:19"),
                ]}}
            ]}}}}
        ]}}});
        let track = Track {
            title: "Wonderwall".into(),
            artist: "Oasis".into(),
            album: Some("Original".into()),
            position_ms: 54_000,
            duration_ms: 259_000,
        };
        assert_eq!(find_song(&results, &track), Some("hpSrLjc5SMs"));
        assert_eq!(
            find_song(
                &results,
                &Track {
                    artist: "Another".into(),
                    ..track
                }
            ),
            None
        );
    }

    #[test]
    fn compiles_music_handoff_script() {
        let temporary = tempfile::tempdir().unwrap();
        let output = std::process::Command::new("/usr/bin/osacompile")
            .arg("-o")
            .arg(temporary.path().join("handoff.scpt"))
            .args(["-e", MUSIC_SCRIPT])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
