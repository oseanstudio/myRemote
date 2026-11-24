use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(target_os = "linux")]
use std::time::Duration;

use tokio::runtime::Runtime;
use tray_icon::{
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder,
};

use crate::{shutdown_signal, start_receiver, update::Updater, ActiveReceiver, Options, VERSION};

pub fn run(options: Options, runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    windows_loop::hide_standalone_console();

    let menu = Menu::new();
    let version = MenuItem::new(format!("myRemote {VERSION}"), false, None);
    let status = MenuItem::new("Receiver stopped", false, None);
    let pair = MenuItem::new("Pair iPhone…", false, None);
    let code = MenuItem::new("Pairing closed", false, None);
    let toggle = MenuItem::new("Start Receiver", true, None);
    let mut updater = Updater::new(&options);
    let update_status = MenuItem::new(updater.status(), false, None);
    let update_now = MenuItem::new("Update Now", true, None);
    let check_updates = CheckMenuItem::new(
        "Check for Updates Automatically",
        true,
        updater.checking(),
        None,
    );
    let install_updates = CheckMenuItem::new(
        "Install Updates Automatically",
        true,
        updater.automatic(),
        None,
    );
    let quit = MenuItem::new("Quit myRemote", true, None);
    menu.append_items(&[
        &version,
        &status,
        &PredefinedMenuItem::separator(),
        &pair,
        &code,
        &toggle,
        &PredefinedMenuItem::separator(),
        &update_status,
        &update_now,
        &check_updates,
        &install_updates,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let _icon = TrayIconBuilder::new()
        .with_tooltip(format!("myRemote {VERSION}"))
        .with_icon(make_icon()?)
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(true)
        .build()?;

    let quit_requested = Arc::new(AtomicBool::new(false));
    runtime.spawn({
        let quit_requested = quit_requested.clone();
        async move {
            if shutdown_signal().await.is_ok() {
                quit_requested.store(true, Ordering::Relaxed);
            }
        }
    });
    let mut active = start(&options, &runtime, &status, &pair, &toggle);
    if options.pair {
        if let Some(active) = active.as_ref() {
            active.state.enable_pairing();
        }
    }

    #[cfg(windows)]
    let message_loop = windows_loop::MessageLoop::new()?;
    let mut displayed_code = None;
    let mut displayed_update = updater.status().to_owned();
    let mut update_busy = false;
    let mut pending_event = None;
    let events = MenuEvent::receiver();
    while !quit_requested.load(Ordering::Relaxed) {
        while let Some(event) = pending_event.take().or_else(|| events.try_recv().ok()) {
            if event.id == *quit.id() {
                quit_requested.store(true, Ordering::Relaxed);
                break;
            }
            if event.id == *toggle.id() {
                if let Some(receiver) = active.take() {
                    runtime.block_on(receiver.stop());
                    status.set_text("Receiver stopped");
                    pair.set_enabled(false);
                    toggle.set_text("Start Receiver");
                } else {
                    active = start(&options, &runtime, &status, &pair, &toggle);
                }
            }
            if event.id == *pair.id() {
                if let Some(receiver) = active.as_ref() {
                    receiver.state.enable_pairing();
                }
            }
            if event.id == *update_now.id() {
                updater.update_now();
            }
            if event.id == *check_updates.id() {
                updater.toggle_checks();
                check_updates.set_checked(updater.checking());
                install_updates.set_checked(updater.automatic());
            }
            if event.id == *install_updates.id() {
                updater.toggle_installation();
                check_updates.set_checked(updater.checking());
                install_updates.set_checked(updater.automatic());
            }
        }
        let pin = active
            .as_ref()
            .and_then(|receiver| receiver.state.pairing_code());
        if pin != displayed_code {
            displayed_code = pin;
            match pin {
                Some(pin) => {
                    code.set_text(format!("Pairing code {pin:04} · 2 minutes"));
                    #[cfg(windows)]
                    windows_loop::show_pairing(pin, &options.name);
                }
                None => code.set_text("Pairing closed"),
            }
        }
        if quit_requested.load(Ordering::Relaxed) {
            break;
        }
        if updater.poll() {
            break;
        }
        if displayed_update != updater.status() {
            displayed_update = updater.status().to_owned();
            update_status.set_text(&displayed_update);
        }
        if update_busy != updater.busy() {
            update_busy = updater.busy();
            update_now.set_enabled(!update_busy);
        }
        #[cfg(windows)]
        if !message_loop.dispatch()? {
            break;
        }
        #[cfg(target_os = "linux")]
        {
            pending_event = events.recv_timeout(Duration::from_millis(250)).ok();
        }
    }
    if let Some(active) = active {
        runtime.block_on(active.stop());
    }
    Ok(())
}

fn start(
    options: &Options,
    runtime: &Runtime,
    status: &MenuItem,
    pair: &MenuItem,
    toggle: &MenuItem,
) -> Option<ActiveReceiver> {
    match runtime.block_on(start_receiver(options)) {
        Ok(receiver) => {
            status.set_text(format!("Listening on {}", receiver.ip));
            pair.set_enabled(true);
            toggle.set_text("Stop Receiver");
            Some(receiver)
        }
        Err(error) => {
            status.set_text(error.to_string());
            tracing::warn!("Receiver could not start: {error}");
            None
        }
    }
}

fn make_icon() -> Result<Icon, tray_icon::BadIcon> {
    let mut rgba = Vec::with_capacity(20 * 20 * 4);
    for y in 0..20 {
        for x in 0..20 {
            let color = match (x, y) {
                (3..=16, 0..=19) | (2 | 17, 2..=17) => match (x, y) {
                    (6..=13, 4..=10) | (8..=11, 14..=15) => [255, 255, 255, 255],
                    _ => [35, 100, 217, 255],
                },
                _ => [0, 0, 0, 0],
            };
            rgba.extend_from_slice(&color);
        }
    }
    Icon::from_rgba(rgba, 20, 20)
}

#[cfg(windows)]
mod windows_loop {
    use windows::core::{w, HSTRING};
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, KillTimer, MessageBoxW, SetTimer, TranslateMessage, MB_OK,
        MB_SETFOREGROUND, MSG,
    };

    pub fn hide_standalone_console() {
        // Keep the parent's terminal attached when myRemote was started from a shell.
        unsafe {
            if GetConsoleProcessList(&mut [0]) == 1 {
                let _ = FreeConsole();
            }
        }
    }

    pub fn show_pairing(pin: u32, name: &str) {
        let message = HSTRING::from(format!(
            "Pairing code {pin:04}\n\nOpen Control Center → Apple TV Remote on your iPhone, choose {name}, and enter the code within two minutes."
        ));
        unsafe { MessageBoxW(None, &message, w!("Pair iPhone"), MB_OK | MB_SETFOREGROUND) };
    }

    pub struct MessageLoop(usize);

    impl MessageLoop {
        pub fn new() -> std::io::Result<Self> {
            // The timer wakes the native message loop so it can consume tray events and expiry.
            let timer = unsafe { SetTimer(None, 0, 150, None) };
            if timer == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(Self(timer))
        }

        pub fn dispatch(&self) -> std::io::Result<bool> {
            let mut message = MSG::default();
            match unsafe { GetMessageW(&mut message, None, 0, 0).0 } {
                -1 => Err(std::io::Error::last_os_error()),
                0 => Ok(false),
                _ => {
                    unsafe {
                        let _ = TranslateMessage(&message);
                        DispatchMessageW(&message);
                    }
                    Ok(true)
                }
            }
        }
    }

    impl Drop for MessageLoop {
        fn drop(&mut self) {
            let _ = unsafe { KillTimer(None, self.0) };
        }
    }
}
