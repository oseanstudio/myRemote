use std::env;
use std::io::{self, BufRead, IsTerminal};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::Arc;

#[cfg(not(target_os = "macos"))]
use myremote::detect_local_ip;
use myremote::{PairingState, Receiver, RemoteInput, TrackpadMode};

mod input;
#[cfg(target_os = "macos")]
mod mac_accessibility;
#[cfg(target_os = "macos")]
mod mac_bonjour;
#[cfg(target_os = "macos")]
mod mac_menu;
mod tailnet;
#[cfg(any(windows, target_os = "linux"))]
mod tray_menu;
mod update;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn version_label(version: &str) -> &str {
    version.strip_suffix(".0").unwrap_or(version)
}

const HELP: &str = r#"myRemote — use the built-in Apple TV Remote to control this desktop

Usage: myremote [OPTIONS]

  --pair              Open a two-minute pairing window at startup
  --menu              Open the menu bar or system tray app
  --name NAME         Set the name shown in the iPhone's Remote picker
  --ip ADDRESS        Advertise and listen on this local IPv4 address
  --tailscale-host IP Listen on this computer's Tailscale IPv4 for a bridge
  --tailscale-bridge IP  Publish the Tailscale host on the iPhone's local network
  --mode mouse|dpad    Use the touch surface as a pointer or as arrow keys
  --speed NUMBER      Set mouse sensitivity from 0.1 to 5 (default 0.8)
  --state-dir PATH    Override the per-user pairing directory
  --forget-phones     Remove all paired phones and exit
  --dry-run           Print received input instead of controlling the desktop
  --verbose           Show connection diagnostics
  -h, --help          Show this help
  -V, --version       Show the version

While running in a terminal, enter p to pair a phone or q to stop."#;

struct Options {
    menu: bool,
    name: String,
    ip: Option<Ipv4Addr>,
    tailnet_host: Option<Ipv4Addr>,
    tailnet_bridge: Option<Ipv4Addr>,
    mode: TrackpadMode,
    speed: f64,
    state_dir: Option<PathBuf>,
    pair: bool,
    forget: bool,
    dry_run: bool,
    verbose: bool,
}

impl Options {
    fn parse() -> Result<Option<Self>, String> {
        let mut args = env::args_os()
            .skip(1)
            .filter(|arg| {
                !cfg!(target_os = "macos")
                    || !arg.to_str().is_some_and(|arg| arg.starts_with("-psn_"))
            })
            .peekable();
        let mut options = Self {
            menu: args.peek().is_none()
                && (cfg!(windows)
                    || cfg!(target_os = "macos")
                        && env::current_exe().ok().is_some_and(|path| {
                            path.ancestors()
                                .nth(3)
                                .and_then(|dir| dir.extension())
                                .is_some_and(|extension| extension == "app")
                        })),
            name: default_name(),
            ip: None,
            tailnet_host: None,
            tailnet_bridge: None,
            mode: TrackpadMode::Mouse,
            speed: 0.8,
            state_dir: None,
            pair: false,
            forget: false,
            dry_run: false,
            verbose: false,
        };
        while let Some(arg) = args.next() {
            let arg = arg.to_str().ok_or("option must be valid Unicode")?;
            let mut value = || args.next().ok_or_else(|| format!("{arg} requires a value"));
            let mut text = || {
                value()?
                    .into_string()
                    .map_err(|_| format!("{arg} requires valid Unicode"))
            };
            match arg {
                "--menu" => options.menu = true,
                "--name" => options.name = text()?,
                "--tailscale-host" | "--tailscale-bridge" => {
                    let ip: Ipv4Addr = text()?
                        .parse()
                        .map_err(|_| format!("{arg} requires a Tailscale IPv4 address"))?;
                    if !tailnet::is_tailnet(ip) {
                        return Err(format!("{arg} requires a Tailscale IPv4 address"));
                    }
                    if arg == "--tailscale-host" {
                        options.tailnet_host = Some(ip);
                    } else {
                        options.tailnet_bridge = Some(ip);
                    }
                }
                "--ip" => {
                    let ip: Ipv4Addr = text()?
                        .parse()
                        .map_err(|_| "--ip requires an IPv4 address")?;
                    if ip.is_unspecified() || ip.is_multicast() || ip.is_broadcast() {
                        return Err("--ip must be an address assigned to the computer".into());
                    }
                    options.ip = Some(ip);
                }
                "--mode" => {
                    options.mode = match text()?.as_str() {
                        "mouse" => TrackpadMode::Mouse,
                        "dpad" => TrackpadMode::Direction,
                        _ => return Err("--mode requires mouse or dpad".into()),
                    }
                }
                "--speed" => {
                    options.speed = text()?.parse().map_err(|_| "--speed requires a number")?;
                    if !(0.1..=5.0).contains(&options.speed) {
                        return Err("--speed must be between 0.1 and 5".into());
                    }
                }
                "--state-dir" => options.state_dir = Some(PathBuf::from(value()?)),
                "--pair" => options.pair = true,
                "--forget-phones" => options.forget = true,
                "--dry-run" => options.dry_run = true,
                "--verbose" => options.verbose = true,
                "-h" | "--help" => {
                    println!("{HELP}");
                    return Ok(None);
                }
                "-V" | "--version" => {
                    println!("myRemote {VERSION}");
                    return Ok(None);
                }
                _ => return Err(format!("unknown option {arg}")),
            }
        }
        let name = options.name.trim();
        if name.is_empty() || name.len() > 63 || name.chars().any(char::is_control) {
            return Err(
                "--name must have 1 to 63 UTF-8 bytes and contain no control characters".into(),
            );
        }
        options.name = name.to_owned();
        if options.forget && (options.pair || options.menu) {
            return Err("--forget-phones must run separately from --pair and --menu".into());
        }
        if options.tailnet_host.is_some()
            && (options.tailnet_bridge.is_some() || options.ip.is_some())
        {
            return Err(
                "--tailscale-host selects its own address and runs separately from a bridge".into(),
            );
        }
        if options.tailnet_bridge.is_some()
            && (options.pair
                || options.forget
                || options.menu
                || options.ip.is_some_and(tailnet::is_tailnet))
        {
            return Err(
                "A Tailscale bridge runs on a local network address and uses pairing on the host"
                    .into(),
            );
        }
        Ok(Some(options))
    }
}

fn default_name() -> String {
    let host = env::var("COMPUTERNAME").ok().or_else(|| {
        let output = std::process::Command::new("hostname").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let host = String::from_utf8(output.stdout).ok()?;
        Some(host.trim().trim_end_matches(".local").to_owned())
    });
    let mut name = "myRemote".to_string();
    if let Some(host) =
        host.filter(|host| !host.trim().is_empty() && !host.chars().any(char::is_control))
    {
        name.push_str(" · ");
        for c in host.chars() {
            if name.len() + c.len_utf8() > 63 {
                break;
            }
            name.push(c);
        }
    }
    name
}

fn default_state_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    return env::var_os("LOCALAPPDATA").map(|path| PathBuf::from(path).join("myRemote"));

    #[cfg(target_os = "macos")]
    return env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Library/Application Support/myRemote"));

    #[cfg(target_os = "linux")]
    return env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .map(|path| path.join("myremote"));
}

fn show_pairing(state: &PairingState) {
    let pin = state.enable_pairing();
    println!("Pairing code {pin:04}. Open Control Center → Apple TV Remote and select this computer within two minutes.");
}

struct ActiveReceiver {
    state: Arc<PairingState>,
    receiver: Receiver,
    ip: IpAddr,
    #[cfg(target_os = "macos")]
    bonjour: Option<mac_bonjour::Bonjour>,
    tailnet: Option<tailnet::Host>,
}

impl ActiveReceiver {
    async fn stop(self) {
        drop(self.tailnet);
        #[cfg(target_os = "macos")]
        drop(self.bonjour);
        self.receiver.stop().await;
    }
}

fn open_state(options: &Options) -> Result<Arc<PairingState>, Box<dyn std::error::Error>> {
    let directory = options
        .state_dir
        .clone()
        .or_else(default_state_dir)
        .ok_or("no user state directory found. Pass --state-dir PATH.")?;
    Ok(Arc::new(PairingState::open(&directory)?))
}

async fn start_receiver(options: &Options) -> Result<ActiveReceiver, Box<dyn std::error::Error>> {
    let input: Arc<dyn RemoteInput> = if options.dry_run {
        Arc::new(input::DryRun::default())
    } else {
        Arc::new(input::Desktop::open(options.speed)?)
    };
    let state = open_state(options)?;
    if options.menu {
        state.enable_pairing_on_request();
    }
    if let Some(ip) = options.tailnet_host {
        let (receiver, records) =
            Receiver::listen_tailnet(options.name.clone(), ip, state.clone(), options.mode, input)
                .await?;
        let catalog = tailnet::Catalog {
            name: options.name.clone(),
            records,
        };
        return match tailnet::Host::start(ip, catalog).await {
            Ok(tailnet) => Ok(ActiveReceiver {
                state,
                receiver,
                ip: ip.into(),
                #[cfg(target_os = "macos")]
                bonjour: None,
                tailnet: Some(tailnet),
            }),
            Err(error) => {
                receiver.stop().await;
                Err(error)
            }
        };
    }
    #[cfg(target_os = "macos")]
    {
        let ip = options
            .ip
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
        let (receiver, records) =
            Receiver::listen(options.name.clone(), ip, state.clone(), options.mode, input).await?;
        match mac_bonjour::Bonjour::start(&options.name, options.ip, records) {
            Ok(bonjour) => Ok(ActiveReceiver {
                state,
                receiver,
                ip,
                bonjour: Some(bonjour),
                tailnet: None,
            }),
            Err(error) => {
                receiver.stop().await;
                Err(error.into())
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let ip = match options.ip {
            Some(ip) => ip,
            None => detect_local_ip()?,
        };
        let receiver =
            Receiver::start(options.name.clone(), ip, state.clone(), options.mode, input).await?;
        Ok(ActiveReceiver {
            state,
            receiver,
            ip: ip.into(),
            tailnet: None,
        })
    }
}

async fn run(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(remote) = options.tailnet_bridge {
        return run_bridge(&options, remote)
            .await
            .map_err(|error| error as Box<dyn std::error::Error>);
    }
    if options.forget {
        let count = open_state(&options)?.forget_phones()?;
        println!("Removed {count} paired phone(s).");
        return Ok(());
    }
    let active = start_receiver(&options).await?;
    println!("{} is listening on {}.", options.name, active.ip);
    if options.verbose {
        println!(
            "TCP ports   discovery {}   companion {}   AirPlay discovery {}",
            active.receiver.ports[0], active.receiver.ports[1], active.receiver.ports[2]
        );
    }
    if options.pair {
        show_pairing(&active.state);
    }

    let (commands, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    if io::stdin().is_terminal() {
        println!("Enter p to pair a phone or q to stop.");
        std::thread::spawn(move || {
            for line in io::stdin().lock().lines().map_while(Result::ok) {
                let quit = line.trim().eq_ignore_ascii_case("q");
                if commands.send(line).is_err() || quit {
                    break;
                }
            }
        });
    }
    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    let result = loop {
        tokio::select! {
            result = &mut shutdown => break result,
            Some(line) = inbox.recv() => match line.trim().to_ascii_lowercase().as_str() {
                "p" => show_pairing(&active.state),
                "q" => break Ok(()),
                _ => println!("Enter p to pair a phone or q to stop."),
            }
        }
    };
    active.stop().await;
    result?;
    Ok(())
}

async fn run_bridge(options: &Options, remote: Ipv4Addr) -> tailnet::Result<()> {
    let catalog = tailnet::fetch(remote).await?;
    let name = catalog.name.clone();
    let bridge = Bridge::start(options.ip, remote, catalog).await?;
    println!(
        "Bridging {name} on the local network. Companion port {}.",
        bridge.port
    );
    let result = shutdown_signal().await;
    bridge.stop().await;
    result?;
    Ok(())
}

struct Bridge {
    #[cfg(target_os = "macos")]
    bonjour: mac_bonjour::Bonjour,
    receiver: Receiver,
    port: u16,
}

impl Bridge {
    async fn start(
        ip: Option<Ipv4Addr>,
        remote: Ipv4Addr,
        mut catalog: tailnet::Catalog,
    ) -> tailnet::Result<Self> {
        #[cfg(target_os = "macos")]
        let listen_ip = ip
            .map(IpAddr::V4)
            .unwrap_or(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
        #[cfg(not(target_os = "macos"))]
        let local_ip = match ip {
            Some(ip) => ip,
            None => detect_local_ip()?,
        };
        #[cfg(not(target_os = "macos"))]
        let listen_ip = local_ip.into();
        let receiver = Receiver::relay(listen_ip, remote, &mut catalog.records).await?;
        let port = catalog
            .records
            .iter()
            .find(|record| record.service_type == "_companion-link._tcp")
            .unwrap()
            .port;
        #[cfg(target_os = "macos")]
        let bonjour = match mac_bonjour::Bonjour::start(&catalog.name, ip, catalog.records) {
            Ok(bonjour) => bonjour,
            Err(error) => {
                receiver.stop().await;
                return Err(error.into());
            }
        };
        #[cfg(not(target_os = "macos"))]
        let receiver = {
            let mut receiver = receiver;
            let host = format!("myremote-bridge-{:016x}.local.", rand::random::<u64>());
            if let Err(error) = receiver.advertise(&catalog.name, &host, local_ip, &catalog.records)
            {
                receiver.stop().await;
                return Err(error.into());
            }
            receiver
        };
        Ok(Self {
            #[cfg(target_os = "macos")]
            bonjour,
            receiver,
            port,
        })
    }

    async fn stop(self) {
        #[cfg(target_os = "macos")]
        drop(self.bonjour);
        self.receiver.stop().await;
    }
}
async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::select! {
            _ = terminate.recv() => Ok(()),
            result = tokio::signal::ctrl_c() => result,
        }
    }
    #[cfg(windows)]
    tokio::signal::ctrl_c().await
}

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    if let Some(result) = mac_accessibility::run_probe() {
        return result;
    }
    #[cfg(target_os = "macos")]
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == update::ACCESSIBILITY_RELAUNCH_ARG)
    {
        return match update::run_accessibility_relaunch() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("myRemote relaunch: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--apply-update")
    {
        return match update::run_helper() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("myRemote update: {error}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    let options = match Options::parse() {
        Ok(Some(options)) => options,
        Ok(None) => return std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("myRemote: {error}\nRun myremote --help for options.");
            return std::process::ExitCode::from(2);
        }
    };
    let level = if options.verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };
    tracing_subscriber::fmt()
        .with_target(false)
        .with_max_level(level)
        .with_ansi(false)
        .init();
    if let Err(error) = launch(options) {
        eprintln!("myRemote: {error}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

fn launch(options: Options) -> Result<(), Box<dyn std::error::Error>> {
    update::clean_helper();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    if options.menu {
        #[cfg(target_os = "macos")]
        return mac_menu::run(options, runtime);
        #[cfg(any(windows, target_os = "linux"))]
        return tray_menu::run(options, runtime);
    }
    runtime.block_on(run(options))
}
