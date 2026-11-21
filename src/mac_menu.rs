use std::cell::RefCell;
use std::path::PathBuf;

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSImage,
    NSMenu, NSMenuDelegate, NSMenuItem, NSPanel, NSStatusBar, NSTextField, NSWindowStyleMask,
    NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSObject, NSObjectProtocol, NSPoint, NSRect, NSSize, NSString, NSTimer, NSURL,
};
use tokio::runtime::Runtime;

use crate::{
    input, mac_bonjour, start_receiver,
    tailnet_menu::MenuNetwork,
    update::{self, Updater},
    ActiveReceiver, Options, VERSION,
};

const STATUS: isize = 1;
const PAIR: isize = 2;
const TOGGLE: isize = 3;
const UPDATE_STATUS: isize = 4;
const UPDATE_NOW: isize = 5;
const AUTO_CHECK: isize = 6;
const AUTO_INSTALL: isize = 7;
const ACCESSIBILITY: isize = 8;
const LOCAL_NETWORK: isize = 9;
const TAILSCALE: isize = 10;
const TAILSCALE_STATUS: isize = 11;
const TAILSCALE_SHARE: isize = 12;
const TAILSCALE_BRIDGE: isize = 13;
const CONNECTION: isize = 14;
const ACCESSIBILITY_SETTINGS: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

struct State {
    options: Options,
    runtime: Runtime,
    active: RefCell<Option<ActiveReceiver>>,
    error: RefCell<Option<String>>,
    accessibility_panel: RefCell<Option<Retained<NSPanel>>>,
    relaunch_target: RefCell<Option<PathBuf>>,
    network: RefCell<MenuNetwork>,
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
                Some(mac_bonjour::Status::Starting) => "Starting Bonjour discovery…",
                Some(mac_bonjour::Status::Reconnecting) => "Reconnecting Bonjour discovery…",
                Some(mac_bonjour::Status::Ready) => "Receiver ready on the local network",
                Some(mac_bonjour::Status::LocalNetworkPermission) => "Local Network permission needed",
                Some(mac_bonjour::Status::Failed) => "Bonjour discovery could not start",
                None if active.is_some() => "Receiver ready for a Tailscale bridge",
                None if self.ivars().error.borrow().is_some() => "Receiver could not start",
                None => "Receiver stopped",
            };
            if let Some(item) = menu.itemWithTag(STATUS) {
                item.setTitle(&NSString::from_str(status));
            }
            if let Some(item) = menu.itemWithTag(CONNECTION) {
                item.setHidden(active.is_none());
                if let Some(receiver) = active.as_ref() {
                    item.setTitle(&NSString::from_str(receiver.state.connection_status()));
                }
            }
            if let Some(item) = menu.itemWithTag(PAIR) {
                item.setEnabled(active.is_some());
            }
            if let Some(item) = menu.itemWithTag(TOGGLE) {
                item.setTitle(if active.is_some() { objc2_foundation::ns_string!("Stop Receiver") }
                              else { objc2_foundation::ns_string!("Start Receiver") });
            }
            if let Some(item) = menu.itemWithTag(ACCESSIBILITY) {
                item.setHidden(!self.needs_accessibility());
            }
            let network = self.ivars().network.borrow();
            if let Some(item) = menu.itemWithTag(LOCAL_NETWORK) {
                item.setHidden(!matches!(
                    discovery,
                    Some(mac_bonjour::Status::LocalNetworkPermission | mac_bonjour::Status::Failed)
                ) && (discovery.is_some() || !network.bridging()));
            }
            let submenu = menu.itemWithTag(TAILSCALE).and_then(|item| item.submenu());
            let tailscale_menu = submenu.as_deref().unwrap_or(menu);
            if let Some(item) = tailscale_menu.itemWithTag(TAILSCALE_STATUS) {
                item.setTitle(&NSString::from_str(&network.status()));
            }
            if let Some(item) = tailscale_menu.itemWithTag(TAILSCALE_SHARE) {
                item.setState(isize::from(network.sharing()));
            }
            if let Some(item) = tailscale_menu.itemWithTag(TAILSCALE_BRIDGE) {
                item.setState(isize::from(network.bridging()));
            }
            let updater = self.ivars().updater.borrow();
            if let Some(item) = menu.itemWithTag(UPDATE_STATUS) {
                item.setTitle(&NSString::from_str(updater.status()));
            }
            if let Some(item) = menu.itemWithTag(UPDATE_NOW) {
                item.setEnabled(!updater.busy());
            }
            if let Some(item) = menu.itemWithTag(AUTO_CHECK) {
                item.setState(isize::from(updater.checking()));
            }
            if let Some(item) = menu.itemWithTag(AUTO_INSTALL) {
                item.setState(isize::from(updater.automatic()));
            }
        }
    }

    impl MenuController {
        #[unsafe(method(myRemotePair:))]
        fn pair(&self, _sender: &NSMenuItem) {
            self.show_pairing();
        }

        #[unsafe(method(myRemoteToggle:))]
        fn toggle(&self, _sender: &NSMenuItem) {
            if self.ivars().active.borrow().is_some() {
                self.stop();
            } else {
                self.start();
                let error = self.ivars().error.borrow().clone();
                if let Some(error) = error {
                    self.alert("Unable to start myRemote", &error);
                } else {
                    self.request_accessibility_if_needed();
                }
            }
        }

        #[unsafe(method(myRemoteAccessibility:))]
        fn accessibility(&self, _sender: &NSMenuItem) {
            if self.needs_accessibility() {
                self.show_accessibility_setup();
                open_settings(ACCESSIBILITY_SETTINGS);
            }
        }

        #[unsafe(method(myRemoteOpenAccessibilitySettings:))]
        fn open_accessibility_settings(&self, _sender: &NSButton) {
            open_settings(ACCESSIBILITY_SETTINGS);
        }

        #[unsafe(method(myRemoteRevealInstalledApp:))]
        fn reveal_installed_app(&self, _sender: &NSButton) {
            if let Ok(bundle) = update::app_bundle_path() {
                if let Some(path) = bundle.to_str() {
                    let url = NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), true);
                    NSWorkspace::sharedWorkspace().activateFileViewerSelectingURLs(
                        &NSArray::from_retained_slice(&[url]),
                    );
                }
            }
        }

        #[unsafe(method(myRemoteRelaunch:))]
        fn relaunch(&self, _sender: &NSButton) {
            if !self.needs_accessibility() {
                return;
            }
            match update::app_bundle_path() {
                Ok(path) => {
                    self.ivars().relaunch_target.replace(Some(path));
                    NSApplication::sharedApplication(self.mtm()).stop(None);
                }
                Err(error) => self.alert("Unable to relaunch myRemote", &error.to_string()),
            }
        }

        #[unsafe(method(myRemoteCheckAccessibility:))]
        fn check_accessibility(&self, _timer: &NSTimer) {
            self.request_accessibility_if_needed();
        }

        #[unsafe(method(myRemoteLocalNetwork:))]
        fn local_network(&self, _sender: &NSMenuItem) {
            open_settings("x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork");
        }

        #[unsafe(method(myRemoteTailscaleShare:))]
        fn tailscale_share(&self, _sender: &NSMenuItem) {
            let state = self.ivars();
            state.network.borrow_mut().toggle_share(
                &state.runtime, &state.options, state.active.borrow().as_ref(),
            );
        }

        #[unsafe(method(myRemoteTailscaleBridge:))]
        fn tailscale_bridge(&self, _sender: &NSMenuItem) {
            let state = self.ivars();
            state.network.borrow_mut().toggle_bridge(&state.runtime, &state.options);
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

        #[unsafe(method(myRemoteAutoCheck:))]
        fn toggle_checks(&self, _sender: &NSMenuItem) {
            self.ivars().updater.borrow_mut().toggle_checks();
        }

        #[unsafe(method(myRemoteAutoInstall:))]
        fn toggle_install(&self, _sender: &NSMenuItem) {
            self.ivars().updater.borrow_mut().toggle_installation();
        }

        #[unsafe(method(myRemotePollUpdates:))]
        fn poll_updates(&self, _timer: &NSTimer) {
            self.finish_accessibility_setup();
            let pin = self.ivars().active.borrow().as_ref()
                .and_then(|receiver| receiver.state.take_requested_code());
            if let Some(pin) = pin {
                self.show_code(pin);
            }
            if self.ivars().updater.borrow_mut().poll() {
                self.stop();
                NSApplication::sharedApplication(self.mtm()).terminate(None);
            }
        }
    }
);

impl MenuController {
    fn new(mtm: MainThreadMarker, options: Options, runtime: Runtime) -> Retained<Self> {
        let updater = RefCell::new(Updater::new(&options));
        let network = RefCell::new(MenuNetwork::new(&options));
        let this = Self::alloc(mtm).set_ivars(State {
            options,
            runtime,
            active: RefCell::new(None),
            error: RefCell::new(None),
            accessibility_panel: RefCell::new(None),
            relaunch_target: RefCell::new(None),
            network,
            updater,
        });
        // Initialize the superclass after assigning the Rust ivars.
        unsafe { msg_send![super(this), init] }
    }

    fn start(&self) {
        let state = self.ivars();
        match state.runtime.block_on(start_receiver(&state.options)) {
            Ok(receiver) => {
                state.network.borrow_mut().sync_receiver(
                    &state.runtime,
                    &state.options,
                    Some(&receiver),
                );
                state.active.replace(Some(receiver));
                state.error.replace(None);
            }
            Err(error) => {
                state.error.replace(Some(error.to_string()));
            }
        }
    }

    fn needs_accessibility(&self) -> bool {
        !self.ivars().options.dry_run && !input::can_post_input()
    }

    fn request_accessibility_if_needed(&self) {
        if self.needs_accessibility() {
            input::request_input_permission();
        }
    }

    fn show_accessibility_setup(&self) {
        let panel = self
            .ivars()
            .accessibility_panel
            .borrow_mut()
            .get_or_insert_with(|| self.make_accessibility_panel())
            .clone();
        NSApplication::sharedApplication(self.mtm()).activate();
        panel.makeKeyAndOrderFront(None);
    }

    fn make_accessibility_panel(&self) -> Retained<NSPanel> {
        let mtm = self.mtm();
        let panel = NSPanel::initWithContentRect_styleMask_backing_defer(
            NSPanel::alloc(mtm),
            rect(0.0, 0.0, 474.0, 180.0),
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        );
        // The controller owns the panel until it is dismissed or access becomes available.
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setHidesOnDeactivate(false);
        panel.setTitle(objc2_foundation::ns_string!(
            "Allow myRemote to Control This Mac"
        ));
        panel.center();

        if let Some(view) = panel.contentView() {
            let instructions = NSString::from_str(
                "Turn on myRemote in Accessibility settings.\n\nIf it’s already on, remove the old entry and add the copy shown in Finder. If this window stays open after you grant access, click Relaunch myRemote.",
            );
            let label = NSTextField::wrappingLabelWithString(&instructions, mtm);
            label.setFrame(rect(22.0, 56.0, 430.0, 104.0));
            view.addSubview(&label);
            let bundled = update::app_bundle_path().is_ok();
            for (label, action, x, width, enabled) in [
                (
                    "Open Settings",
                    sel!(myRemoteOpenAccessibilitySettings:),
                    18.0,
                    130.0,
                    true,
                ),
                (
                    "Show in Finder",
                    sel!(myRemoteRevealInstalledApp:),
                    152.0,
                    130.0,
                    bundled,
                ),
                (
                    "Relaunch myRemote",
                    sel!(myRemoteRelaunch:),
                    286.0,
                    168.0,
                    bundled,
                ),
            ] {
                // These selectors target the live controller and receive the NSButton sender.
                let button = unsafe {
                    NSButton::buttonWithTitle_target_action(
                        &NSString::from_str(label),
                        Some(self),
                        Some(action),
                        mtm,
                    )
                };
                button.setFrame(rect(x, 17.0, width, 31.0));
                button.setEnabled(enabled);
                view.addSubview(&button);
            }
        }
        panel
    }

    fn finish_accessibility_setup(&self) {
        let mut setup = self.ivars().accessibility_panel.borrow_mut();
        if setup
            .as_ref()
            .is_some_and(|panel| !panel.isVisible() || !self.needs_accessibility())
        {
            if let Some(panel) = setup.take() {
                panel.orderOut(None);
            }
        }
    }

    fn stop(&self) {
        let state = self.ivars();
        state
            .network
            .borrow_mut()
            .sync_receiver(&state.runtime, &state.options, None);
        if let Some(active) = state.active.take() {
            state.runtime.block_on(active.stop());
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
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(&NSString::from_str(title));
        alert.setInformativeText(&NSString::from_str(text));
        NSApplication::sharedApplication(self.mtm()).activate();
        alert.runModal();
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
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(location)) {
        NSWorkspace::sharedWorkspace().openURL(&url);
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
}

pub fn run(options: Options, runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    let mtm = MainThreadMarker::new().ok_or("macOS menu must run on the main thread")?;
    let relaunch_target = autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let controller = MenuController::new(mtm, options, runtime);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
        controller.add_item(&menu, &format!("myRemote {VERSION}"), None, 0);
        controller.add_item(&menu, "Receiver stopped", None, STATUS);
        controller.add_item(&menu, "Waiting for iPhone", None, CONNECTION);
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        controller.add_item(&menu, "Pair iPhone…", Some(sel!(myRemotePair:)), PAIR);
        controller.add_item(&menu, "Start Receiver", Some(sel!(myRemoteToggle:)), TOGGLE);
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
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        controller.add_item(&menu, "Tailscale", None, TAILSCALE);
        if let Some(item) = menu.itemWithTag(TAILSCALE) {
            let submenu = NSMenu::new(mtm);
            submenu.setAutoenablesItems(false);
            submenu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
            controller.add_item(&submenu, "Tailscale is off", None, TAILSCALE_STATUS);
            controller.add_item(
                &submenu,
                "Share This Computer",
                Some(sel!(myRemoteTailscaleShare:)),
                TAILSCALE_SHARE,
            );
            controller.add_item(
                &submenu,
                "Bridge Remote Computers to This Wi-Fi",
                Some(sel!(myRemoteTailscaleBridge:)),
                TAILSCALE_BRIDGE,
            );
            item.setSubmenu(Some(&submenu));
            item.setEnabled(true);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        controller.add_item(&menu, "Updates", None, UPDATE_STATUS);
        controller.add_item(
            &menu,
            "Update Now",
            Some(sel!(myRemoteUpdateNow:)),
            UPDATE_NOW,
        );
        controller.add_item(
            &menu,
            "Check for Updates Automatically",
            Some(sel!(myRemoteAutoCheck:)),
            AUTO_CHECK,
        );
        controller.add_item(
            &menu,
            "Install Updates Automatically",
            Some(sel!(myRemoteAutoInstall:)),
            AUTO_INSTALL,
        );
        menu.addItem(&NSMenuItem::separatorItem(mtm));
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
        {
            let state = controller.ivars();
            state
                .network
                .borrow_mut()
                .start_bridge(&state.runtime, &state.options);
        }
        if controller.ivars().options.pair {
            controller.show_pairing();
        }
        // Wait for AppKit's event loop before asking macOS to show a permission prompt.
        // Both selectors target the live controller and take one NSTimer argument.
        let (accessibility_timer, poll_timer) = unsafe {
            let accessibility =
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    0.0,
                    &controller,
                    sel!(myRemoteCheckAccessibility:),
                    None,
                    false,
                );
            let poll = NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                &controller,
                sel!(myRemotePollUpdates:),
                None,
                true,
            );
            (accessibility, poll)
        };
        app.run();
        accessibility_timer.invalidate();
        poll_timer.invalidate();
        controller.stop();
        if let Some(panel) = controller.ivars().accessibility_panel.take() {
            panel.orderOut(None);
        }
        bar.removeStatusItem(&item);
        controller.ivars().relaunch_target.take()
    });
    if let Some(target) = relaunch_target {
        update::relaunch_current(&target)?;
    }
    Ok(())
}
