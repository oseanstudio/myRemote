use plist::Value as Plist;

use crate::keyed_archive::{self, class, dictionary, uid};
use crate::opack::Value;
use crate::{PlaybackState, VideoSource};

const VOLUME: u64 = 0x0100;
const SKIP_FORWARD: u64 = 0x0200;
const SKIP_BACKWARD: u64 = 0x0400;
const PLAY: u64 = 0x0001;
const PAUSE: u64 = 0x0002;

pub(crate) fn flags(playback: Option<PlaybackState>) -> u64 {
    VOLUME
        | playback.map_or(0, |state| {
            SKIP_FORWARD | SKIP_BACKWARD | state.playing.map_or(0, |_| PLAY | PAUSE)
        })
}

pub(crate) fn now_playing(playback: Option<PlaybackState>) -> Value {
    // TVRemoteCore expects a keyed archive in NowPlayingInfoKey. The object
    // shape is also visible in the public Apple TV capture in pyatv issue #2325.
    let mut objects = vec![Plist::String("$null".into()), Plist::Boolean(false)];
    let mut add = |value| {
        objects.push(value);
        uid((objects.len() - 1) as u64)
    };
    let info_class = add(class("TVRCNowPlayingInfo"));
    let (identifier, metadata, playback_state, rate, captions, has_captions) = match playback {
        Some(state) => {
            let source = match state.source {
                VideoSource::Jellyfin => "Jellyfin",
                VideoSource::YouTube => "YouTube",
            };
            let id = add(Plist::String(format!("myRemote.{source}")));
            let title = add(Plist::String(source.into()));
            let meta_class = add(class("TVRCNowPlayingMetadata"));
            let metadata = add(dictionary([("title", title), ("$class", meta_class)]));
            let playback_state = add(Plist::Integer(
                state
                    .playing
                    .map_or(0, |playing| if playing { 1 } else { 2 })
                    .into(),
            ));
            let rate = add(Plist::Real(if state.playing == Some(true) {
                1.0
            } else {
                0.0
            }));
            let captions = state
                .captions
                .map_or_else(|| uid(0), |enabled| add(Plist::Boolean(enabled)));
            let has_captions = add(Plist::Boolean(state.captions.is_some()));
            (id, metadata, playback_state, rate, captions, has_captions)
        }
        None => (uid(0), uid(0), uid(0), uid(0), uid(0), uid(0)),
    };
    objects[1] = dictionary([
        ("$class", info_class),
        ("playerIdentifier", identifier.clone()),
        ("identifier", identifier),
        ("metadata", metadata),
        ("playbackState", playback_state),
        ("playbackRate", rate),
        ("captionsEnabled", captions),
        ("hasValidCaptionOptions", has_captions),
    ]);
    Value::dict(vec![Value::kv(
        "NowPlayingInfoKey",
        Value::Bytes(keyed_archive::encode(
            "NSKeyedArchiver",
            dictionary([("root", uid(1))]),
            objects,
        )),
    )])
}
