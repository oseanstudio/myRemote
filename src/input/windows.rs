use std::io;
use std::mem::size_of;

use windows::Win32::Media::Audio::{
    eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

pub struct Desktop;

impl Desktop {
    pub fn open() -> io::Result<Self> {
        Ok(Self)
    }

    pub fn button(&mut self, name: &str) {
        let key = match name {
            "up" => VK_UP,
            "down" => VK_DOWN,
            "left" => VK_LEFT,
            "right" => VK_RIGHT,
            "select" => VK_RETURN,
            "menu" => VK_ESCAPE,
            "home" => VK_HOME,
            "channel_up" => VK_PRIOR,
            "channel_down" => VK_NEXT,
            "guide" => VK_TAB,
            "play_pause" => VK_MEDIA_PLAY_PAUSE,
            "volume_up" => VK_VOLUME_UP,
            "volume_down" => VK_VOLUME_DOWN,
            "mute" => VK_VOLUME_MUTE,
            _ => return,
        };
        let flags = match key {
            VK_UP | VK_DOWN | VK_LEFT | VK_RIGHT | VK_HOME | VK_PRIOR | VK_NEXT
            | VK_MEDIA_PLAY_PAUSE | VK_VOLUME_UP | VK_VOLUME_DOWN | VK_VOLUME_MUTE => {
                KEYEVENTF_EXTENDEDKEY
            }
            _ => KEYBD_EVENT_FLAGS(0),
        };
        let event = |flags| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    dwFlags: flags,
                    ..Default::default()
                },
            },
        };
        send(&[event(flags), event(flags | KEYEVENTF_KEYUP)]);
    }

    pub fn move_pointer(&mut self, dx: i32, dy: i32) {
        send(&[mouse(MOUSEEVENTF_MOVE, dx, dy)]);
    }

    pub fn click(&mut self) {
        send(&[
            mouse(MOUSEEVENTF_LEFTDOWN, 0, 0),
            mouse(MOUSEEVENTF_LEFTUP, 0, 0),
        ]);
    }

    pub fn volume(&self) -> Option<f64> {
        with_audio(|audio| unsafe {
            if audio.GetMute()?.as_bool() {
                Ok(0.0)
            } else {
                Ok(f64::from(audio.GetMasterVolumeLevelScalar()?))
            }
        })
        .ok()
    }

    pub fn set_volume(&self, volume: f64) {
        if let Err(error) = with_audio(|audio| unsafe {
            audio.SetMasterVolumeLevelScalar(volume as f32, std::ptr::null())?;
            if volume > 0.0 {
                audio.SetMute(false, std::ptr::null())?;
            }
            Ok(())
        }) {
            tracing::warn!("cannot set Windows output volume: {error}");
        }
    }
}

fn mouse(flags: MOUSE_EVENT_FLAGS, dx: i32, dy: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

fn send(events: &[INPUT]) {
    // Win32 copies these fully initialized INPUT records synchronously.
    let sent = unsafe { SendInput(events, size_of::<INPUT>() as i32) };
    if sent as usize != events.len() {
        tracing::warn!("Windows accepted {sent} of {} input events", events.len());
    }
}

fn with_audio<T>(
    use_audio: impl FnOnce(IAudioEndpointVolume) -> windows::core::Result<T>,
) -> windows::core::Result<T> {
    // COM initialization and all interface use stay on this thread.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com = ComGuard;
    unsafe {
        let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let output = devices.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
        use_audio(output.Activate(CLSCTX_ALL, None)?)
    }
}
