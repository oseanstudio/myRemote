use std::cell::{Cell, RefCell};
use std::io;
use std::path::PathBuf;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSApplication, NSApplicationActivationPolicy, NSImage,
    NSMenu, NSMenuDelegate, NSMenuItem, NSPasteboard, NSStatusBar, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSRunLoop, NSRunLoopCommonModes, NSString, NSTimer, NSURL,
};
use objc2_service_management::SMAppServiceStatus;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use crate::{
    input,
    mac_accessibility::{refresh_updated_permissions, ApprovalWatch, Permission},
    mac_bonjour,
    mac_handoff::{self, Destination, Handoff, Outcome, Track},
    mac_login::LoginItem,
    mac_peer, start_receiver,
    update::{self, Updater},
    version_label, ActiveReceiver, Options, VERSION,
};

const STATUS: isize = 1;
const PAIR: isize = 2;
const START_RECEIVER: isize = 3;
const UPDATE_STATUS: isize = 4;
const UPDATE_ACTION: isize = 5;
const ACCESSIBILITY: isize = 8;
const LOCAL_NETWORK: isize = 9;
const CONNECTION: isize = 14;
const NEARBY: isize = 15;
const NEARBY_STATUS: isize = 16;
const NEARBY_DIAGNOSTIC: isize = 17;
const AIRPLAY_AUDIO: isize = 18;
const HANDOFF: isize = 19;
const HANDOFF_STATUS: isize = 20;
const HANDOFF_MUSIC: isize = 21;
const HANDOFF_YOUTUBE: isize = 22;
const HANDOFF_SHORTCUT: isize = 23;
const OPEN_AT_LOGIN: isize = 24;
const NEARBY_DIAGNOSTIC_TITLE: &str = "Run Nearby Diagnostic…";

type NearbyDiagnostic = JoinHandle<io::Result<Option<String>>>;
type HandoffAction = JoinHandle<(Track, io::Result<Outcome>)>;

struct State {
    options: Options,
    runtime: Runtime,
    menu: Retained<NSMenu>,
    active: RefCell<Option<ActiveReceiver>>,
    error: RefCell<Option<String>>,
    accessibility_approval: RefCell<Option<ApprovalWatch>>,
    post_events_requested: Cell<bool>,
    nearby_enabled: Cell<bool>,
    nearby_preference: Option<PathBuf>,
    nearby: RefCell<Option<mac_peer::Nearby>>,
    nearby_failed: Cell<bool>,
    nearby_diagnostic: RefCell<Option<NearbyDiagnostic>>,
    handoff: RefCell<Option<Handoff>>,
    handoff_action: RefCell<Option<HandoffAction>>,
    login_item: Option<LoginItem>,
    updater: RefCell<Updater>,
}

define_class!(
    // NSObject imposes no subclass initialization requirements beyond init.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = State]
    struct MenuController;

    unsafe impl NSObjectProtocol for MenuController {}

    unsafe impl NSMenuDelegate for MenuController {
        #[unsafe(method(menuNeedsUpdate:))]
        fn menu_needs_update(&self, menu: &NSMenu) {
            let active = self.ivars().active.borrow();
            let discovery = active.as_ref().and_then(|receiver| receiver.bonjour.as_ref())
                .map(mac_bonjour::Bonjour::status);
            let status = match discovery {
                Some(mac_bonjour::Status::Starting) => Some("Starting Bonjour discovery…"),
                Some(mac_bonjour::Status::Reconnecting) => Some("Reconnecting Bonjour discovery…"),
                Some(mac_bonjour::Status::LocalNetworkPermission) => Some("Local Network permission needed"),
                Some(mac_bonjour::Status::Failed) => Some("Bonjour discovery could not start"),
                None if self.ivars().error.borrow().is_some() => Some("Receiver could not start"),
                Some(mac_bonjour::Status::Ready) | None => None,
            };
            if let Some(item) = menu.itemWithTag(STATUS) {
                item.setHidden(status.is_none());
                if let Some(status) = status {
                    item.setTitle(&NSString::from_str(status));
                }
            }
            if let Some(item) = menu.itemWithTag(CONNECTION) {
                let status = active.as_ref().map(|receiver| receiver.state.connection_status())
                    .filter(|&status| status != "Waiting for iPhone");
                item.setHidden(status.is_none());
                if let Some(status) = status {
                    item.setTitle(&NSString::from_str(status));
                }
            }
            if let Some(item) = menu.itemWithTag(PAIR) {
                item.setEnabled(active.is_some());
            }
            if let Some(item) = menu.itemWithTag(AIRPLAY_AUDIO) {
                let setting = crate::mac_airplay::setting();
                item.setState(isize::from(setting.enabled));
                item.setEnabled(setting.editable);
            }
            if let Some(item) = menu.itemWithTag(OPEN_AT_LOGIN) {
                self.refresh_login_item(&item);
            }
            self.refresh_handoff_menu(menu);
            if let Some(item) = menu.itemWithTag(START_RECEIVER) {
                item.setHidden(active.is_some());
            }
            if let Some(item) = menu.itemWithTag(ACCESSIBILITY) {
                item.setHidden(!self.needs_accessibility());
            }
            if let Some(item) = menu.itemWithTag(LOCAL_NETWORK) {
                item.setHidden(!matches!(
                    discovery,
                    Some(mac_bonjour::Status::LocalNetworkPermission | mac_bonjour::Status::Failed)
                ));
            }
            let nearby_supported = active.as_ref().is_some_and(|receiver| receiver.peer_target.is_some());
            if let Some(item) = menu.itemWithTag(NEARBY) {
                item.setHidden(!nearby_supported);
                item.setState(isize::from(self.ivars().nearby_enabled.get()));
                item.setToolTip(Some(objc2_foundation::ns_string!("Try connecting a nearby iPhone without joining the same Wi‑Fi network")));
            }
            if let Some(item) = menu.itemWithTag(NEARBY_STATUS) {
                let failed = self.ivars().nearby_failed.get()
                    || self.ivars().nearby.borrow().as_ref().is_some_and(mac_peer::Nearby::unavailable);
                item.setHidden(!nearby_supported || !failed);
            }
            if let Some(item) = menu.itemWithTag(NEARBY_DIAGNOSTIC) {
                let running = self.ivars().nearby_diagnostic.borrow().is_some();
                item.setHidden(!nearby_supported || !self.ivars().nearby_enabled.get());
                item.setEnabled(!running);
                let title = if running { "Nearby Diagnostic in Progress…" } else { NEARBY_DIAGNOSTIC_TITLE };
                item.setTitle(&NSString::from_str(title));
            }
            let updater = self.ivars().updater.borrow();
            if let Some(item) = menu.itemWithTag(UPDATE_ACTION) {
                let (symbol, label) = if updater.has_update() {
                    (objc2_foundation::ns_string!("arrow.down.circle.fill"), updater.action_title())
                } else {
                    (objc2_foundation::ns_string!("arrow.clockwise"), "Check for Updates…")
                };
                let label = NSString::from_str(label);
                let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(symbol, Some(&label));
                if let Some(icon) = icon.as_ref() {
                    icon.setTemplate(true);
                }
                item.setImage(icon.as_deref());
                item.setToolTip(Some(&label));
            }
            if let Some(item) = menu.itemWithTag(UPDATE_STATUS) {
                let status = updater.status();
                item.setHidden(status.is_empty());
                if !status.is_empty() {
                    item.setTitle(&NSString::from_str(status));
                }
            }
        }
    }

    impl MenuController {
        #[unsafe(method(myRemotePair:))]
        fn pair(&self, _sender: &NSMenuItem) {
            self.show_pairing();
        }

        #[unsafe(method(myRemoteStartReceiver:))]
        fn start_receiver(&self, _sender: &NSMenuItem) {
            if self.ivars().active.borrow().is_some() {
                return;
            }
            self.start();
            let error = self.ivars().error.borrow().clone();
            if let Some(error) = error {
                self.alert("Unable to start myRemote", &error);
            } else {
                self.request_accessibility(true);
            }
        }

        #[unsafe(method(myRemoteAccessibility:))]
        fn accessibility(&self, _sender: &NSMenuItem) {
            self.request_accessibility(true);
        }

        #[unsafe(method(myRemotePromptAccessibility:))]
        fn prompt_accessibility(&self, _timer: &NSTimer) {
            self.request_accessibility(false);
        }

        #[unsafe(method(myRemotePollMenu:))]
        fn poll_menu(&self, _timer: &NSTimer) {
            self.poll_accessibility_setup();
            self.poll_nearby_diagnostic();
            self.poll_handoff();
        }

        #[unsafe(method(myRemoteLocalNetwork:))]
        fn local_network(&self, _sender: &NSMenuItem) {
            open_settings("x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork");
        }

        #[unsafe(method(myRemoteOpenAtLogin:))]
        fn open_at_login(&self, sender: &NSMenuItem) {
            if let Some(login_item) = &self.ivars().login_item {
                if let Err(error) = login_item.toggle() {
                    self.alert("Could Not Change Open at Login", &error.localizedDescription().to_string());
                }
            }
            self.refresh_login_item(sender);
        }

        #[unsafe(method(myRemoteReceiveAirPlayAudio:))]
        fn receive_airplay_audio(&self, sender: &NSMenuItem) {
            let setting = crate::mac_airplay::setting();
            if setting.editable && !crate::mac_airplay::set_enabled(!setting.enabled) {
                self.alert(
                    "Could Not Change AirPlay Receiver",
                    "macOS did not save the new setting. Check AirPlay Receiver in System Settings → General → AirDrop & Continuity.",
                );
            }
            let setting = crate::mac_airplay::setting();
            sender.setState(isize::from(setting.enabled));
            sender.setEnabled(setting.editable);
        }

        #[unsafe(method(myRemoteToggleMusicHandoff:))]
        fn toggle_music_handoff(&self, _sender: &NSMenuItem) {
            let state = self.ivars();
            if state.handoff.borrow().as_ref().is_some_and(Handoff::running) {
                self.stop_handoff();
                return;
            }
            match Handoff::start(state.runtime.handle(), &state.options.name) {
                Ok(receiver) => {
                    let instructions = format!(
                        "Choose {} in the iPhone’s AirPlay picker. Enter {} if asked. Then choose a Mac player from the myRemote menu. Keep both devices on the same local network.",
                        receiver.name, receiver.password
                    );
                    state.handoff.replace(Some(receiver));
                    self.alert("Music Handoff (Beta)", &instructions);
                }
                Err(error) => self.alert("Could Not Start Music Handoff", &error.to_string()),
            }
        }

        #[unsafe(method(myRemoteContinueMusic:))]
        fn continue_music(&self, sender: &NSMenuItem) {
            let state = self.ivars();
            if state.handoff_action.borrow().is_some() {
                return;
            }
            let destination = match sender.tag() {
                HANDOFF_MUSIC => Destination::Music,
                HANDOFF_YOUTUBE => Destination::YouTubeMusic,
                HANDOFF_SHORTCUT => Destination::Shortcut,
                _ => return,
            };
            let track = state.handoff.borrow().as_ref().and_then(|handoff| handoff.state().track.clone());
            if let Some(track) = track {
                state.handoff_action.replace(Some(
                    state.runtime.spawn(async move {
                        let outcome = mac_handoff::continue_playback(destination, track.clone()).await;
                        (track, outcome)
                    }),
                ));
            }
        }

        #[unsafe(method(myRemoteToggleNearby:))]
        fn toggle_nearby(&self, _sender: &NSMenuItem) {
            let state = self.ivars();
            let enabled = !state.nearby_enabled.get();
            if let Some(path) = &state.nearby_preference {
                let saved = if enabled {
                    std::fs::write(path, b"enabled\n")
                } else {
                    match std::fs::remove_file(path) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                        result => result,
                    }
                };
                if let Err(error) = saved {
                    self.alert("Could not save Nearby iPhone", &error.to_string());
                    return;
                }
            }
            state.nearby_enabled.set(enabled);
            self.refresh_nearby();
        }

        #[unsafe(method(myRemoteRunNearbyDiagnostic:))]
        fn run_nearby_diagnostic(&self, _sender: &NSMenuItem) {
            let state = self.ivars();
            if !state.nearby_enabled.get() || state.nearby_diagnostic.borrow().is_some() {
                return;
            }
            let alert = self.make_alert(
                "Nearby Discovery Diagnostic",
                "After you approve the macOS prompt, open the Apple TV Remote device picker on your iPhone. This Mac will count direct Wi‑Fi discovery traffic for 30 seconds and show a summary you can copy.",
            );
            alert.addButtonWithTitle(objc2_foundation::ns_string!("Start"));
            alert.addButtonWithTitle(objc2_foundation::ns_string!("Cancel"));
            if alert.runModal() == NSAlertFirstButtonReturn
                && state.nearby_enabled.get()
                && state.nearby_diagnostic.borrow().is_none()
            {
                let task = state.runtime.spawn(mac_peer::diagnostics::capture());
                state.nearby_diagnostic.replace(Some(task));
            }
        }

        #[unsafe(method(myRemoteQuit:))]
        fn quit(&self, _sender: &NSMenuItem) {
            self.stop();
            NSApplication::sharedApplication(self.mtm()).stop(None);
        }

        #[unsafe(method(myRemoteUpdateNow:))]
        fn update_now(&self, _sender: &NSMenuItem) {
            self.ivars().updater.borrow_mut().update_now();
        }

        #[unsafe(method(myRemotePollUpdates:))]
        fn poll_updates(&self, _timer: &NSTimer) {
            let pin = self.ivars().active.borrow().as_ref()
                .and_then(|receiver| receiver.state.take_requested_code());
            if let Some(pin) = pin {
                self.show_code(pin);
            }
            if self.ivars().updater.borrow_mut().poll() {
                self.stop();
                NSApplication::sharedApplication(self.mtm()).terminate(None);
                return;
            }
        }
    }
);

impl MenuController {
    fn new(
        mtm: MainThreadMarker,
        options: Options,
        runtime: Runtime,
        menu: Retained<NSMenu>,
    ) -> Retained<Self> {
        let updater = RefCell::new(Updater::new(&options));
        let approval = if options.dry_run {
            None
        } else {
            update::app_bundle_path()
                .ok()
                .map(|bundle| ApprovalWatch::start(bundle, &runtime, Permission::current()))
        };
        let nearby_preference = options
            .state_dir
            .clone()
            .or_else(crate::default_state_dir)
            .map(|path| path.join("nearby-beta"));
        let nearby_enabled = options.nearby
            || nearby_preference
                .as_ref()
                .is_some_and(|path| path.is_file());
        let this = Self::alloc(mtm).set_ivars(State {
            options,
            runtime,
            menu,
            active: RefCell::new(None),
            error: RefCell::new(None),
            accessibility_approval: RefCell::new(approval),
            post_events_requested: Cell::new(false),
            nearby_enabled: Cell::new(nearby_enabled),
            nearby_preference,
            nearby: RefCell::new(None),
            nearby_failed: Cell::new(false),
            nearby_diagnostic: RefCell::new(None),
            handoff: RefCell::new(None),
            handoff_action: RefCell::new(None),
            login_item: LoginItem::for_installed_app(),
            updater,
        });
        // Initialize the superclass after assigning the Rust ivars.
        unsafe { msg_send![super(this), init] }
    }

    fn start(&self) {
        let state = self.ivars();
        match state.runtime.block_on(start_receiver(&state.options)) {
            Ok(receiver) => {
                state.active.replace(Some(receiver));
                state.error.replace(None);
            }
            Err(error) => {
                state.error.replace(Some(error.to_string()));
            }
        }
        self.refresh_nearby();
    }

    fn refresh_login_item(&self, item: &NSMenuItem) {
        item.setHidden(self.ivars().login_item.is_none());
        if let Some(login_item) = &self.ivars().login_item {
            let status = login_item.status();
            let title = if status == SMAppServiceStatus::RequiresApproval {
                objc2_foundation::ns_string!("Allow Open at Login…")
            } else {
                objc2_foundation::ns_string!("Open at Login")
            };
            item.setTitle(title);
            item.setState(isize::from(status == SMAppServiceStatus::Enabled));
        }
    }

    fn refresh_nearby(&self) {
        let state = self.ivars();
        state.nearby.take();
        state.nearby_failed.set(false);
        if !state.nearby_enabled.get() {
            return;
        }
        let target = state
            .active
            .borrow()
            .as_ref()
            .and_then(|receiver| receiver.peer_target.clone());
        if let Some(target) = target {
            match mac_peer::Nearby::start(state.runtime.handle(), target) {
                Ok(nearby) => {
                    state.nearby.replace(Some(nearby));
                }
                Err(error) => {
                    tracing::warn!("Could not start Nearby iPhone: {error}");
                    state.nearby_failed.set(true);
                }
            }
        }
    }

    fn needs_accessibility(&self) -> bool {
        !self.ivars().options.dry_run && self.accessibility_permission() != Permission::PostEvents
    }

    fn accessibility_permission(&self) -> Permission {
        self.ivars()
            .accessibility_approval
            .borrow()
            .as_ref()
            .map_or_else(Permission::current, ApprovalWatch::permission)
    }

    fn request_accessibility(&self, manual: bool) {
        let state = self.ivars();
        if state.options.dry_run {
            return;
        }
        if manual {
            if let Some(approval) = state.accessibility_approval.borrow().as_ref() {
                approval.check_now();
            }
        }
        match self.accessibility_permission() {
            Permission::Missing if input::request_accessibility_entry() => {
                self.request_post_events(manual);
            }
            Permission::Accessibility => self.request_post_events(manual),
            Permission::Missing | Permission::PostEvents => {}
        }
    }

    fn request_post_events(&self, manual: bool) {
        let requested = &self.ivars().post_events_requested;
        if (manual || !requested.get()) && !input::can_post_input() {
            requested.set(true);
            input::request_input_permission();
        }
    }

    fn poll_accessibility_setup(&self) {
        let (grant, permission) = {
            let mut approval = self.ivars().accessibility_approval.borrow_mut();
            let Some(approval) = approval.as_mut() else {
                return;
            };
            (approval.granted(), approval.permission())
        };
        let Some(grant) = grant else {
            if permission == Permission::Accessibility {
                self.request_post_events(false);
            }
            return;
        };
        match update::prepare_accessibility_relaunch(grant) {
            Ok(()) => {
                self.ivars().accessibility_approval.take();
                self.stop();
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
            Err(error) => {
                tracing::warn!("Unable to relaunch myRemote: {error}");
            }
        }
    }

    fn stop(&self) {
        let state = self.ivars();
        self.stop_handoff();
        if let Some(capture) = state.nearby_diagnostic.take() {
            capture.abort();
        }
        state.nearby.take();
        if let Some(active) = state.active.take() {
            state.runtime.block_on(active.stop());
        }
    }

    fn refresh_handoff_menu(&self, menu: &NSMenu) {
        let state = self.ivars();
        let receiver = state.handoff.borrow();
        let running = receiver.as_ref().is_some_and(Handoff::running);
        let track_ready = running
            && receiver
                .as_ref()
                .is_some_and(|handoff| handoff.state().track.is_some());
        let busy = state.handoff_action.borrow().is_some();
        if let Some(item) = menu.itemWithTag(HANDOFF) {
            item.setState(isize::from(running));
        }
        if let Some(item) = menu.itemWithTag(HANDOFF_STATUS) {
            item.setHidden(receiver.is_none());
            if let Some(handoff) = receiver.as_ref() {
                let snapshot = handoff.state();
                let status = if busy {
                    "Finding the matching recording…".to_owned()
                } else if let Some(track) = &snapshot.track {
                    let seconds = track.position_ms / 1000;
                    format!(
                        "{} · {} · {}:{:02}",
                        track.title,
                        track.artist,
                        seconds / 60,
                        seconds % 60
                    )
                } else {
                    snapshot
                        .status
                        .unwrap_or("Waiting for iPhone music")
                        .to_owned()
                };
                item.setTitle(&NSString::from_str(&status));
            }
        }
        for tag in [HANDOFF_MUSIC, HANDOFF_YOUTUBE, HANDOFF_SHORTCUT] {
            if let Some(item) = menu.itemWithTag(tag) {
                item.setHidden(!running);
                item.setEnabled(track_ready && !busy);
            }
        }
    }

    fn stop_handoff(&self) {
        let state = self.ivars();
        if let Some(action) = state.handoff_action.take() {
            action.abort();
        }
        state.handoff.take();
    }

    fn poll_handoff(&self) {
        let state = self.ivars();
        if !state
            .handoff_action
            .borrow()
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        let action = state.handoff_action.take().expect("completed song handoff");
        state.menu.cancelTracking();
        match state.runtime.block_on(action) {
            Ok((track, Ok(Outcome::Playing))) => self.finish_handoff(&track),
            Ok((track, Ok(Outcome::Browser(url)))) if open_url(&url) => {
                let alert = self.make_alert(
                    "Continue in YouTube Music",
                    "Once the song is playing at the right spot in your browser, stop the iPhone’s AirPlay stream.",
                );
                alert.addButtonWithTitle(objc2_foundation::ns_string!("Stop AirPlay Stream"));
                alert.addButtonWithTitle(objc2_foundation::ns_string!("Keep Streaming"));
                if alert.runModal() == NSAlertFirstButtonReturn {
                    self.finish_handoff(&track);
                }
            }
            Ok((_, Ok(Outcome::Browser(_)))) => self.alert(
                "Could Not Continue Playback",
                "The Mac could not open YouTube Music in your browser.",
            ),
            Ok((_, Err(error))) => self.alert("Could Not Continue Playback", &error.to_string()),
            Err(_) => self.alert(
                "Could Not Continue Playback",
                "The handoff was interrupted.",
            ),
        }
    }

    fn finish_handoff(&self, track: &Track) {
        let same_recording = self
            .ivars()
            .handoff
            .borrow()
            .as_ref()
            .is_some_and(|handoff| {
                handoff
                    .state()
                    .track
                    .as_ref()
                    .is_some_and(|current| current.same_recording(track))
            });
        if same_recording {
            self.stop_handoff();
        }
    }

    fn poll_nearby_diagnostic(&self) {
        let state = self.ivars();
        if !state
            .nearby_diagnostic
            .borrow()
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            return;
        }
        let capture = state
            .nearby_diagnostic
            .take()
            .expect("completed diagnostic");
        // Dismiss the menu if it is tracking so the result can appear immediately.
        state.menu.cancelTracking();
        match state.runtime.block_on(capture) {
            Ok(Ok(Some(report))) => {
                let alert = self.make_alert("Nearby Discovery Diagnostic", &report);
                alert.addButtonWithTitle(objc2_foundation::ns_string!("Copy Summary"));
                alert.addButtonWithTitle(objc2_foundation::ns_string!("Close"));
                if alert.runModal() == NSAlertFirstButtonReturn {
                    let text = NSString::from_str(&report);
                    let pasteboard = NSPasteboard::generalPasteboard();
                    pasteboard.clearContents();
                    pasteboard
                        .writeObjects(&NSArray::from_slice(&[ProtocolObject::from_ref(&*text)]));
                }
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => {
                self.alert("Nearby Diagnostic Could Not Complete", &error.to_string())
            }
            Err(error) => {
                tracing::debug!("Nearby diagnostic task stopped: {error}");
                self.alert(
                    "Nearby Diagnostic Could Not Complete",
                    "The diagnostic was interrupted.",
                );
            }
        }
    }

    fn show_pairing(&self) {
        let pin = self
            .ivars()
            .active
            .borrow()
            .as_ref()
            .map(|active| active.state.enable_pairing());
        if let Some(pin) = pin {
            self.show_code(pin);
        }
    }

    fn show_code(&self, pin: u32) {
        self.alert(&format!("Pairing code {pin:04}"), &format!(
            "On your iPhone, open Control Center → Apple TV Remote and choose {}. Enter this code within two minutes.",
            self.ivars().options.name
        ));
    }

    fn alert(&self, title: &str, text: &str) {
        self.make_alert(title, text).runModal();
    }

    fn make_alert(&self, title: &str, text: &str) -> Retained<NSAlert> {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        NSApplication::sharedApplication(self.mtm()).activate();
        alert
    }

    fn add_item(&self, menu: &NSMenu, title: &str, action: Option<Sel>, tag: isize) {
        let title = NSString::from_str(title);
        // Selectors with actions point to the live controller and take one NSMenuItem argument.
        let item = unsafe {
            let item = NSMenuItem::initWithTitle_action_keyEquivalent(
                NSMenuItem::alloc(self.mtm()),
                &title,
                action,
                objc2_foundation::ns_string!(""),
            );
            if action.is_some() {
                item.setTarget(Some(self));
            }
            item
        };
        item.setTag(tag);
        item.setEnabled(action.is_some());
        menu.addItem(&item);
    }
}

fn open_settings(location: &str) {
    open_url(location);
}

fn open_url(location: &str) -> bool {
    NSURL::URLWithString(&NSString::from_str(location))
        .is_some_and(|url| NSWorkspace::sharedWorkspace().openURL(&url))
}

pub fn run(options: Options, runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    let mtm = MainThreadMarker::new().ok_or("macOS menu must run on the main thread")?;
    if !options.dry_run {
        if let (Ok(bundle), Some(directory)) =
            (update::app_bundle_path(), crate::default_state_dir())
        {
            if let Err(error) = runtime.block_on(refresh_updated_permissions(&bundle, &directory)) {
                tracing::warn!("Unable to refresh macOS permissions for this release: {error}");
            }
        }
    }
    autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let menu = NSMenu::new(mtm);
        let controller = MenuController::new(mtm, options, runtime, menu.clone());
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
        controller.add_item(
            &menu,
            &format!("myRemote {}", version_label(VERSION)),
            Some(sel!(myRemoteUpdateNow:)),
            UPDATE_ACTION,
        );
        controller.add_item(&menu, "", None, STATUS);
        controller.add_item(&menu, "", None, CONNECTION);
        controller.add_item(&menu, "Pair iPhone…", Some(sel!(myRemotePair:)), PAIR);
        controller.add_item(
            &menu,
            "Receive AirPlay Audio",
            Some(sel!(myRemoteReceiveAirPlayAudio:)),
            AIRPLAY_AUDIO,
        );
        controller.add_item(
            &menu,
            "Music Handoff (Beta)",
            Some(sel!(myRemoteToggleMusicHandoff:)),
            HANDOFF,
        );
        controller.add_item(&menu, "", None, HANDOFF_STATUS);
        for (title, tag) in [
            ("Continue in Apple Music", HANDOFF_MUSIC),
            ("Continue in YouTube Music", HANDOFF_YOUTUBE),
            ("Continue with a Shortcut", HANDOFF_SHORTCUT),
        ] {
            controller.add_item(&menu, title, Some(sel!(myRemoteContinueMusic:)), tag);
        }
        controller.add_item(
            &menu,
            "Nearby iPhone (Beta)",
            Some(sel!(myRemoteToggleNearby:)),
            NEARBY,
        );
        controller.add_item(
            &menu,
            NEARBY_DIAGNOSTIC_TITLE,
            Some(sel!(myRemoteRunNearbyDiagnostic:)),
            NEARBY_DIAGNOSTIC,
        );
        controller.add_item(&menu, "Nearby iPhone unavailable", None, NEARBY_STATUS);
        controller.add_item(
            &menu,
            "Retry Starting Receiver",
            Some(sel!(myRemoteStartReceiver:)),
            START_RECEIVER,
        );
        controller.add_item(
            &menu,
            "Allow Accessibility…",
            Some(sel!(myRemoteAccessibility:)),
            ACCESSIBILITY,
        );
        controller.add_item(
            &menu,
            "Local Network Settings…",
            Some(sel!(myRemoteLocalNetwork:)),
            LOCAL_NETWORK,
        );
        controller.add_item(&menu, "", None, UPDATE_STATUS);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        controller.add_item(
            &menu,
            "Open at Login",
            Some(sel!(myRemoteOpenAtLogin:)),
            OPEN_AT_LOGIN,
        );
        controller.add_item(&menu, "Quit myRemote", Some(sel!(myRemoteQuit:)), 0);

        let bar = NSStatusBar::systemStatusBar();
        let item = bar.statusItemWithLength(-1.0);
        if let Some(button) = item.button(mtm) {
            let label = objc2_foundation::ns_string!("myRemote");
            let icon = NSImage::imageWithSystemSymbolName_accessibilityDescription(
                objc2_foundation::ns_string!("appletvremote.gen4"),
                Some(label),
            );
            if let Some(icon) = icon {
                icon.setTemplate(true);
                button.setImage(Some(&icon));
            } else {
                button.setTitle(label);
            }
            button.setToolTip(Some(label));
        }
        item.setMenu(Some(&menu));
        controller.start();
        if let Some(login_item) = &controller.ivars().login_item {
            login_item.register_default();
        }
        // Wait for AppKit's event loop before asking macOS to show a permission prompt.
        // Each selector targets the live controller and takes one NSTimer argument.
        let (prompt_timer, menu_timer, poll_timer) = unsafe {
            let prompt = NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.0,
                &controller,
                sel!(myRemotePromptAccessibility:),
                None,
                false,
            );
            let menu = NSTimer::timerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                &controller,
                sel!(myRemotePollMenu:),
                None,
                true,
            );
            NSRunLoop::mainRunLoop().addTimer_forMode(&menu, NSRunLoopCommonModes);
            let poll = NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                &controller,
                sel!(myRemotePollUpdates:),
                None,
                true,
            );
            (prompt, menu, poll)
        };
        if controller.ivars().options.pair {
            controller.show_pairing();
        }
        app.run();
        prompt_timer.invalidate();
        menu_timer.invalidate();
        poll_timer.invalidate();
        controller.stop();
        controller.ivars().accessibility_approval.take();
        bar.removeStatusItem(&item);
    });
    Ok(())
}
