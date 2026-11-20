use std::cell::{Cell, RefCell};

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::{ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSImage, NSMenu, NSMenuDelegate,
    NSMenuItem, NSStatusBar, NSWorkspace,
};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString, NSURL};
use tokio::runtime::Runtime;

use crate::{start_receiver, ActiveReceiver, Options, VERSION};

const STATUS: isize = 1;
const PAIR: isize = 2;
const TOGGLE: isize = 3;

struct State {
    options: Options,
    runtime: Runtime,
    active: RefCell<Option<ActiveReceiver>>,
    error: RefCell<Option<String>>,
    needs_accessibility: Cell<bool>,
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
            let status = match active.as_ref() {
                Some(receiver) => format!("Listening on {}", receiver.ip),
                None if self.ivars().needs_accessibility.get() => "Accessibility permission required".to_owned(),
                None if self.ivars().error.borrow().is_some() => "Receiver could not start".to_owned(),
                None => "Receiver stopped".to_owned(),
            };
            if let Some(item) = menu.itemWithTag(STATUS) {
                item.setTitle(&NSString::from_str(&status));
            }
            if let Some(item) = menu.itemWithTag(PAIR) {
                item.setEnabled(active.is_some());
            }
            if let Some(item) = menu.itemWithTag(TOGGLE) {
                item.setTitle(if active.is_some() { objc2_foundation::ns_string!("Stop Receiver") }
                              else { objc2_foundation::ns_string!("Start Receiver") });
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
                }
            }
        }

        #[unsafe(method(myRemoteAccessibility:))]
        fn accessibility(&self, _sender: &NSMenuItem) {
            if let Some(url) = NSURL::URLWithString(objc2_foundation::ns_string!(
                "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
            )) {
                NSWorkspace::sharedWorkspace().openURL(&url);
            }
        }

        #[unsafe(method(myRemoteQuit:))]
        fn quit(&self, _sender: &NSMenuItem) {
            self.stop();
            NSApplication::sharedApplication(self.mtm()).stop(None);
        }
    }
);

impl MenuController {
    fn new(mtm: MainThreadMarker, options: Options, runtime: Runtime) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(State {
            options,
            runtime,
            active: RefCell::new(None),
            error: RefCell::new(None),
            needs_accessibility: Cell::new(false),
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
                state.needs_accessibility.set(false);
            }
            Err(error) => {
                state.needs_accessibility.set(
                    error
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
                );
                state.error.replace(Some(error.to_string()));
            }
        }
    }

    fn stop(&self) {
        let state = self.ivars();
        if let Some(active) = state.active.take() {
            state.runtime.block_on(active.receiver.stop());
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
            "Accessibility Settings…",
            Some(sel!(myRemoteAccessibility:)),
            0,
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
        app.run();
        controller.stop();
        bar.removeStatusItem(&item);
    });
    Ok(())
}
