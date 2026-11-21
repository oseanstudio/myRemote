use std::cell::RefCell;

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
    input,
    mac_accessibility::{refresh_updated_permissions, ApprovalWatch, RELAUNCHED_ENV},
    mac_bonjour, start_receiver,
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

struct State {
    options: Options,
    runtime: Runtime,
    active: RefCell<Option<ActiveReceiver>>,
    error: RefCell<Option<String>>,
    accessibility_approval: RefCell<Option<ApprovalWatch>>,
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
                    self.request_accessibility(true);
                }
            }
        }

        #[unsafe(method(myRemoteAccessibility:))]
        fn accessibility(&self, _sender: &NSMenuItem) {
            self.request_accessibility(true);
        }

        #[unsafe(method(myRemoteCheckAccessibility:))]
        fn check_accessibility(&self, _timer: &NSTimer) {
            self.request_accessibility(std::env::var_os(RELAUNCHED_ENV).is_none());
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
            if self.poll_accessibility_setup() {
                return;
            }
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
            accessibility_approval: RefCell::new(None),
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

    fn request_accessibility(&self, watch_for_grant: bool) {
        if !self.needs_accessibility() {
            return;
        }
        if watch_for_grant {
            let state = self.ivars();
            let mut approval = state.accessibility_approval.borrow_mut();
            if approval.as_ref().is_none_or(ApprovalWatch::expired) {
                if let Ok(bundle) = update::app_bundle_path() {
                    *approval = Some(ApprovalWatch::start(bundle, &state.runtime));
                }
            }
        }
        if input::request_accessibility_entry() && self.needs_accessibility() {
            input::request_input_permission();
        }
    }

    fn poll_accessibility_setup(&self) -> bool {
        let mut approval = self.ivars().accessibility_approval.borrow_mut();
        let Some(watch) = approval.as_mut() else {
            return false;
        };
        if watch.expired() {
            approval.take();
        } else if watch.granted() {
            approval.take();
            drop(approval);
            match update::prepare_accessibility_relaunch() {
                Ok(()) => {
                    self.stop();
                    NSApplication::sharedApplication(self.mtm()).terminate(None);
                    return true;
                }
                Err(error) => tracing::warn!("Unable to relaunch myRemote: {error}"),
            }
        }
        false
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
        controller.ivars().accessibility_approval.take();
        bar.removeStatusItem(&item);
    });
    Ok(())
}
