use std::cell::{Cell, RefCell};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSTimer, NSURL};
use tokio::runtime::Runtime;

use crate::{
    input, mac_bonjour, start_receiver, update::Updater, ActiveReceiver, Options, VERSION,
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

struct State {
    options: Options,
    runtime: Runtime,
    active: RefCell<Option<ActiveReceiver>>,
    error: RefCell<Option<String>>,
    accessibility_requested: Cell<bool>,
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
            if let Some(item) = menu.itemWithTag(PAIR) {
                item.setEnabled(active.is_some());
            }
            if let Some(item) = menu.itemWithTag(TOGGLE) {
                item.setTitle(if active.is_some() { objc2_foundation::ns_string!("Stop Receiver") }
                              else { objc2_foundation::ns_string!("Start Receiver") });
            }
            if let Some(item) = menu.itemWithTag(ACCESSIBILITY) {
                item.setHidden(self.ivars().options.dry_run || input::accessibility_is_trusted());
            }
            if let Some(item) = menu.itemWithTag(LOCAL_NETWORK) {
                item.setHidden(discovery.is_none() || discovery == Some(mac_bonjour::Status::Ready));
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
                    self.request_accessibility(false);
                }
            }
        }

        #[unsafe(method(myRemoteAccessibility:))]
        fn accessibility(&self, _sender: &NSMenuItem) {
            self.request_accessibility(true);
        }

        #[unsafe(method(myRemoteLocalNetwork:))]
        fn local_network(&self, _sender: &NSMenuItem) {
            open_settings("x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork");
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
            if self.ivars().active.borrow().is_some() {
                self.request_accessibility(false);
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
        let this = Self::alloc(mtm).set_ivars(State {
            options,
            runtime,
            active: RefCell::new(None),
            error: RefCell::new(None),
            accessibility_requested: Cell::new(false),
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
    }

    fn request_accessibility(&self, clicked: bool) {
        let state = self.ivars();
        if state.options.dry_run || state.accessibility_requested.get() && !clicked {
            return;
        }
        if input::accessibility_is_trusted() {
            state.accessibility_requested.set(true);
        } else if state.accessibility_requested.replace(true) && clicked {
            open_settings(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility",
            );
        } else {
            input::request_accessibility();
        }
    }

    fn stop(&self) {
        let state = self.ivars();
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
            self.alert(&format!("Pairing code {pin:04}"), &format!(
                "On your iPhone, open Control Center → Apple TV Remote and choose {}. Enter this code within two minutes.",
                self.ivars().options.name
            ));
        }
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

pub fn run(options: Options, runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    let mtm = MainThreadMarker::new().ok_or("macOS menu must run on the main thread")?;
    autoreleasepool(|_| {
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        let controller = MenuController::new(mtm, options, runtime);
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        menu.setDelegate(Some(ProtocolObject::from_ref(&*controller)));
        controller.add_item(&menu, &format!("myRemote {VERSION}"), None, 0);
        controller.add_item(&menu, "Receiver stopped", None, STATUS);
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
        if controller.ivars().options.pair {
            controller.show_pairing();
        }
        // Keeps update downloads and verification off the main thread.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                0.25,
                &controller,
                sel!(myRemotePollUpdates:),
                None,
                true,
            )
        };
        app.run();
        timer.invalidate();
        controller.stop();
        bar.removeStatusItem(&item);
    });
    Ok(())
}
