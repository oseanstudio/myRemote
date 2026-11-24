use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(target_os = "linux")]
use std::time::Duration;

use tokio::runtime::Runtime;
use tray_icon::{
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon, TrayIconBuilder, TrayIconEvent,
};

use crate::{shutdown_signal, start_receiver, update::Updater, ActiveReceiver, Options, VERSION};

pub fn run(options: Options, runtime: Runtime) -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(windows)]
    windows_loop::hide_standalone_console();

    let menu = Menu::new();
    let version = MenuItem::new(format!("myRemote {VERSION}"), false, None);
    let startup_error = MenuItem::new("", false, None);
    let pair = MenuItem::new("Pair iPhone…", false, None);
    let code = MenuItem::new("Pairing closed", false, None);
    let retry = MenuItem::new("Retry Starting Receiver", true, None);
    let mut updater = Updater::new(&options);
    let update_status = MenuItem::new("", false, None);
    let mut displayed_update_title = updater.action_title().to_owned();
    let update_now = MenuItem::new(&displayed_update_title, true, None);
    let quit = MenuItem::new("Quit myRemote", true, None);
    menu.append_items(&[
        &version,
        &pair,
        &code,
        &update_now,
        &PredefinedMenuItem::separator(),
        &quit,
    ])?;
    let _icon = TrayIconBuilder::new()
        .with_tooltip(format!("myRemote {VERSION}"))
        .with_icon(make_icon()?)
        .with_menu(Box::new(menu.clone()))
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
    let mut active = start(&options, &runtime, &startup_error, &pair);
    if active.is_none() {
        menu.insert_items(&[&startup_error, &retry], 1)?;
    }
    if options.pair {
        if let Some(active) = active.as_ref() {
            active.state.enable_pairing();
        }
    }

    #[cfg(windows)]
    let message_loop = windows_loop::MessageLoop::new()?;
    let mut displayed_code = None;
    let mut displayed_update = String::new();
    let mut pending_event = None;
    let events = MenuEvent::receiver();
    // Menu commands arrive on MenuEvent. Discard unused tray pointer events.
    TrayIconEvent::set_event_handler(Some(|_| {}));
    while !quit_requested.load(Ordering::Relaxed) {
        while let Some(event) = pending_event.take().or_else(|| events.try_recv().ok()) {
            if event.id == *quit.id() {
                quit_requested.store(true, Ordering::Relaxed);
                break;
            }
            if event.id == *retry.id() && active.is_none() {
                active = start(&options, &runtime, &startup_error, &pair);
                if active.is_some() {
                    menu.remove(&startup_error)?;
                    menu.remove(&retry)?;
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
        let title = updater.action_title();
        if displayed_update_title != title {
            update_now.set_text(title);
            displayed_update_title = title.to_owned();
        }
        let status = updater.status();
        if displayed_update != status {
            if status.is_empty() {
                menu.remove(&update_status)?;
            } else {
                update_status.set_text(status);
                if displayed_update.is_empty() {
                    let index = menu
                        .items()
                        .iter()
                        .position(|item| item.id() == update_now.id())
                        .expect("Update Now is always in the tray menu");
                    menu.insert(&update_status, index)?;
                }
            }
            displayed_update = status.to_owned();
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
) -> Option<ActiveReceiver> {
    match runtime.block_on(start_receiver(options)) {
        Ok(receiver) => {
            pair.set_enabled(true);
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
