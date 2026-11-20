# Set up myRemote

myRemote is an MIT-licensed desktop receiver for the **Apple TV Remote** already in iPhone and iPad Control Center. Run myRemote on each computer you want to control. Each one appears by name in the Remote’s device picker, so you can switch computers from the same iPhone.

## Pair your iPhone

1. Connect your computer and iPhone to the same local network. Ethernet on the computer works if it shares the Wi‑Fi network’s LAN.
2. Install or open the [latest download](https://github.com/oseanstudio/myRemote/releases/latest) using the steps below.
3. Click the myRemote icon in your menu bar or system tray and choose **Pair iPhone…**. On Linux, reopen the menu to see the code. You can also run `myremote --pair` from a terminal to show the code there.
4. Open **Control Center → Apple TV Remote** on the iPhone. If it’s missing, edit Control Center and add **Remote**. Select **myRemote · COMPUTERNAME** in the picker and enter the code from the computer.

The phone can reconnect after a server restart. Run the receiver on another computer and repeat the pairing steps to add it to the same picker. Use **Stop Receiver** to pause the service or **Quit myRemote** to close the app. From a terminal, press **Ctrl+C**.

### Desktop permissions

| OS | Setup |
| --- | --- |
| Windows | Extract the ZIP and open `myremote.exe`. If Windows Firewall asks, allow access on your private network. Windows limits synthetic input into elevated apps and secure screens. |
| macOS 14+ | Open the universal DMG and drag **myRemote** into **Applications**. The app opens in the menu bar. In its menu, open **Accessibility Settings…**, grant myRemote access, then choose **Start Receiver**. Allow Local Network access if prompted. |
| Linux | Make the AppImage executable and launch it. The tray requires a desktop that supports StatusNotifier/AppIndicator. The receiver uses `/dev/uinput` for Wayland and X11. |

The macOS build is ad-hoc signed. If macOS blocks the first launch, open **System Settings → Privacy & Security → Open Anyway** after attempting to open it. Grant Accessibility again after an update if macOS prompts.

### Updates

**Check for Updates Automatically** is on by default. myRemote checks GitHub shortly after launch and every six hours. **Update Now** checks, downloads, verifies, installs, and relaunches myRemote when a newer release is available. Enable **Install Updates Automatically** to do the same whenever a check finds an update. Turning off automatic checks also turns off automatic installation.

Install myRemote in a writable folder to enable replacement. On macOS, move the app out of the DMG first. On Linux, launch the AppImage itself and have `curl` installed. An update briefly interrupts the receiver and preserves pairing and settings.

On Linux, get the [udev rule](../packaging/70-myremote-uinput.rules) from the source repository and run these commands once from the folder containing it.

```sh
sudo groupadd -f myremote
sudo usermod -aG myremote "$USER"
sudo install -m 644 70-myremote-uinput.rules /etc/udev/rules.d/70-myremote-uinput.rules
sudo modprobe uinput
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=misc --sysname-match=uinput
```

Sign out and back in, then run the AppImage as your desktop user. If FUSE is unavailable, start it with `--appimage-extract-and-run`. If your desktop has no tray host, run the AppImage with `--pair` in a terminal. The server uses `wpctl` for absolute volume changes on PipeWire. Media and volume keys use Linux input events.

## Controls

| iPhone Remote | Desktop |
| --- | --- |
| Touch surface | Move the mouse pointer. Tap or press Select to click. |
| Direction buttons | Arrow keys. Start with `--mode dpad` to also map touch swipes to arrows and taps to Enter. |
| Back | Escape |
| TV button | Home key |
| Guide and channel buttons, when shown | Tab and Page Up/Down |
| Play/Pause, volume and mute, when shown | Desktop media and volume controls |

The current release covers pointer movement, left-click, navigation, and media controls. Text entry, right-click, drag, scrolling, Siri, power, screen sharing, and remote access over the internet are outside this release.

## Configuration

```sh
./myremote --name "Living Room PC" --mode dpad --pair
./myremote --ip 192.168.1.20 --speed 1.2
./myremote --help
```

The server uses the selected LAN IPv4 address and Bonjour/mDNS. Use `--ip` if the automatic selection picks a VPN or a different adapter. The iPhone must be able to reach UDP 5353 for discovery and the three TCP ports reported by `--verbose`. Guest Wi‑Fi and client isolation may block discovery. Pairing and input stay on your local network.

Pairing opens only when you request it, and closes after a successful pairing or after two minutes. The server verifies each paired controller and encrypts the input session. To revoke access, stop myRemote and run `myremote --forget-phones` with the same `--state-dir` override, if used. If the iPhone still shows outdated credentials, remove the old device from the picker if available, reopen Control Center, and pair again.

## Build and test

Install current stable Rust and your platform’s standard linker. Windows requires the MSVC C++ Build Tools. On macOS, install Xcode Command Line Tools.

```sh
cargo test --locked
cargo build --release --locked
```

The executable is `target/release/myremote` on macOS and Linux, or `target\release\myremote.exe` on Windows. Pass `--menu` to launch the native interface from an unbundled binary. The [build and release workflow](../.github/workflows/build.yml) publishes a universal DMG, Linux AppImages, and Windows ZIPs when a version tag matches the version in `Cargo.toml`.

An optional interoperability test runs against an independent Companion client and uses the receiver’s dry-run input backend. It checks pairing, reconnection, rejection of unpaired input, and event delivery. Use the path to your platform’s binary.

```sh
python -m pip install pyatv==0.18.0 srptools==1.0.1
python tests/companion_smoke.py target/release/myremote
```

## Credits

The receiver adapts the MIT-licensed [atv-core](https://github.com/corvofeng/atv-core/tree/8a4ada29bb7cad31e0315c3387b277eb9ab8c988) protocol implementation, which builds on [pyatv](https://github.com/postlund/pyatv). Both projects document the Companion protocol used for local Apple TV Remote interoperability. See [Apple’s Control Center Remote setup](https://support.apple.com/en-us/108778) for the iPhone controls. myRemote is an independent open-source project.
