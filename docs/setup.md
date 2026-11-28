# Set up myRemote

myRemote is an MIT-licensed desktop receiver for the **Apple TV Remote** already in iPhone and iPad Control Center. Run myRemote on each computer you want to control. Each one appears by name in the Remote’s device picker, so you can switch computers from the same iPhone.

## Pair your iPhone

1. Connect your computer and iPhone to the same local network. Ethernet on the computer works if it shares the Wi‑Fi network’s LAN.
2. Install or open the [latest download](https://github.com/oseanstudio/myRemote/releases/latest) using the steps below.
3. Open **Control Center → Apple TV Remote** on the iPhone. If it’s missing, edit Control Center and add **Remote**. Select **myRemote · COMPUTERNAME** in the picker and enter the code shown on the computer. On Linux, open the tray menu to see the code.
4. To retry pairing, choose **Pair iPhone…** from myRemote’s menu and select the computer again on your iPhone.

The phone can reconnect after a server restart. Run the receiver on another computer and repeat the pairing steps to add it to the same picker. Use **Quit myRemote** to close the app. From a terminal, press **Ctrl+C**.

To use two iPhones at once, pair each one individually. They share the same desktop pointer and keyboard focus, and either phone can disconnect while the other keeps working.

### Desktop permissions

| OS | Setup |
| --- | --- |
| Windows | Extract the ZIP and open `myremote.exe`. If Windows Firewall asks, allow access on your private network. Windows limits synthetic input into elevated apps and secure screens. |
| macOS 14+ | Open the universal DMG and drag **myRemote** into **Applications**. Open the app to start the receiver. Allow Accessibility and Local Network access when macOS asks. |
| Linux | Make the AppImage executable and launch it. The tray requires a desktop that supports StatusNotifier/AppIndicator. The receiver uses `/dev/uinput` for Wayland and X11. |

myRemote uses the macOS permission prompt when mouse and keyboard access is missing. In the prompt, open System Settings and enable **myRemote**. myRemote reopens automatically after the grant and continues checking if macOS asks separately for permission to send input events. The menu action **Allow Accessibility…** lets you request permission again if you dismissed the prompt.

The macOS build is ad-hoc signed. If access is missing at the first launch of a new release, myRemote asks macOS to clear its old Accessibility and mouse and keyboard permissions so the new copy can request them. Approve the new grant in System Settings. If macOS blocks the first launch, open **System Settings → Privacy & Security → Open Anyway** after attempting to open it.

On Mac, the installed app sets up **Open at Login** the first time you open it. Use **Open at Login** in the myRemote menu to change it. If macOS asks for approval, choose **Allow Open at Login…** to open Login Items in System Settings.

If your iPhone cannot see myRemote, check the status at the top of the Mac menu. Use **Local Network Settings…** if macOS blocked discovery. Connect the iPhone and the computer to the same LAN when using local discovery.

If your iPhone connects but the controls do nothing, press Back on the iPhone and check the myRemote menu. **iPhone input received** confirms that the button reached myRemote. **iPhone verified · waiting for Remote session** means the phone proved its identity and has yet to open the controls. If **Allow Accessibility…** appears, open it and grant this copy of myRemote access.

### Updates

myRemote checks GitHub in the background once every 24 hours while it is running. On Mac, click the update icon to the left of the app name to check now and install any available update. The icon changes to a download arrow when a release is available. Hover over it to see the version. On Windows and Linux, use the update action in the tray menu.

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
| Touch surface | Move the mouse pointer. Tap to click, double-tap to double-click, and hold for a right-click. Tap once, then hold the second tap and move to drag. |
| Right edge of the touch surface | Start a vertical swipe in the rightmost 15% to scroll. The strip has no visible divider, and swipes that start outside it move the pointer. |
| Direction buttons | Arrow keys. Start with `--mode dpad` to also map touch swipes to arrows and taps to Enter. |
| Back | Escape |
| TV button | Home key |
| Guide and channel buttons, when shown | Tab and Page Up/Down |
| Play/Pause, volume and mute, when shown | Desktop media and volume controls |
| Keyboard, on macOS | Type in the focused editable field using the Apple TV Remote keyboard. |

On macOS, connect in Control Center and click an editable field, including a password field in an app. myRemote signals the iPhone to show its keyboard and forwards text, including completed swipe-typed words when the iPhone offers QuickPath. This uses the same Accessibility permission as pointer control. myRemote leaves the clipboard alone, and the contents of the Mac's text field stay on the Mac.

On macOS, Play/Pause and TV control playback and fullscreen in the active browser or supported media player.

While a Jellyfin or YouTube video page is in the foreground, myRemote uses macOS Accessibility to identify the player and advertise the built-in Remote's skip controls. This works with browsers that expose the foreground web page, including Zen. YouTube's shortcuts skip 10 seconds. In Jellyfin, set both **Skip forward length** and **Skip back length** to **10 seconds** in **Settings → Playback**. myRemote also reports subtitle availability when it can read the YouTube player's caption toggle.

### Receive AirPlay audio on Mac

Click **Receive AirPlay Audio** in the myRemote menu to turn the Mac’s built-in AirPlay Receiver on or off. A checkmark shows that receiving is enabled. To change who can stream to the Mac, use **Allow AirPlay for** in **System Settings → General → AirDrop & Continuity**.

Connect the iPhone and Mac to the same local network. Start a song on the iPhone, open the **AirPlay** output picker in its Now Playing controls, and select the Mac’s computer name. The iPhone keeps its place in the song while the Mac plays the audio. The **myRemote · COMPUTERNAME** entry is for desktop controls.

### Continue a song in a Mac player

1. Enable **Beta → Music Handoff** in myRemote. With both devices on the same LAN, select the AirPlay destination ending in **· Music Handoff** on the iPhone. Use the code shown by myRemote if the iPhone asks for a password.
2. Play a song and wait for its title and elapsed time to appear in the **Beta** submenu. The beta needs an audio source that sends both song details and AirPlay playback progress.
3. Choose **Continue in Apple Music** to play a matching track from your Music library. Allow macOS Automation access if prompted. If the song is missing, add it to your library and retry.
4. Or choose **Continue in YouTube Music**. myRemote searches YouTube Music using the title and artist and opens the match at the captured time in your browser. Click Play if your browser requires it, then confirm that the song is playing before stopping AirPlay.

For another Mac player, create a shortcut named **myRemote Continue Playback** and choose **Continue with a Shortcut**. The shortcut receives a JSON file with `title`, `artist`, `album`, `position_ms`, and `duration_ms`. Return the text `playing` once the player has started and sought to the requested position.

## Configuration

```sh
./myremote --name "Living Room PC" --mode dpad --pair
./myremote --ip 192.168.1.20 --speed 1.2
./myremote --help
```

The receiver uses Bonjour for discovery. On macOS, it advertises through the system Bonjour service and listens on IPv4 and IPv6 by default. On Windows and Linux, use `--ip` to select a LAN adapter if needed. Guest Wi‑Fi and client isolation can block discovery.

On macOS, **Beta → Nearby iPhone** in the menu enables experimental peer-to-peer Wi‑Fi alongside normal discovery. Keep Wi‑Fi and Bluetooth turned on on both devices. To test without a shared network, disconnect both devices from their Wi‑Fi networks while leaving the radios on, unplug Ethernet if connected, and open Apple TV Remote on the nearby iPhone. Look for the name ending in **· Nearby**. If prompted, use **Pair iPhone…** on the Mac. Apple TV Remote discovery and reconnection over this link need testing on an iPhone.

If the iPhone does not find myRemote, choose **Beta → Run Nearby Diagnostic…** with Nearby iPhone enabled. Start the check, approve the macOS administrator prompt if it appears, and open the Apple TV Remote device picker during the 30-second check. **Copy Summary** copies only packet counts for troubleshooting.

The menu app opens a two-minute pairing window when a phone requests a code. The code is shown only on the target computer, and pairing closes once the phone is enrolled. The server verifies each paired controller and encrypts the input session. To revoke access, stop myRemote and run `myremote --forget-phones` with the same `--state-dir` override, if used. If the iPhone still shows outdated credentials, remove the old device from the picker if available, reopen Control Center, and pair again.

## Tailscale command-line setup

If the iPhone and computer share a LAN, use normal Bonjour discovery even while Tailscale is connected. For different networks, run a myRemote bridge on a computer that shares the iPhone's Wi‑Fi and can reach the target over Tailscale. Tailscale carries the connection between the bridge and the target. The Apple TV Remote still discovers the bridge on local Wi‑Fi.

1. Connect both computers to Tailscale. Quit any existing myRemote instance on the computer you want to control, then run `myremote --tailscale-host TARGET_IP` using that computer’s Tailscale IPv4 address.
2. On a computer connected to the iPhone’s Wi-Fi network, run `myremote --tailscale-bridge TARGET_IP` with the same target address.
3. On the iPhone, open Apple TV Remote and pick the target’s name. If the phone needs to pair, enter the code shown on the target computer. You can pair on the target’s local network before traveling.

Allow the bridge to reach the target on TCP ports 49151–49154 in your tailnet policy. Pairing and encryption still terminate at the target. The bridge forwards encrypted input without handling the pairing keys.

## Build and test

Install current stable Rust and your platform’s standard linker. Windows requires the MSVC C++ Build Tools. On macOS, install Xcode Command Line Tools.

```sh
cargo test --locked
cargo build --release --locked
```

The executable is `target/release/myremote` on macOS and Linux, or `target\release\myremote.exe` on Windows. Pass `--menu` to launch the native interface from an unbundled binary. The [build and release workflow](../.github/workflows/build.yml) publishes a universal DMG, Linux AppImages, and Windows ZIPs when a version tag matches the version in `Cargo.toml`.

An optional interoperability test runs against independent Companion clients and uses the receiver’s dry-run input backend. It checks pairing, reconnection, rejection of unpaired input, simultaneous controllers, and keyboard focus and edits. Use the path to your platform’s binary.

```sh
python -m pip install pyatv==0.18.0 srptools==1.0.1
python tests/companion_smoke.py target/release/myremote
```

## Credits

The receiver adapts the MIT-licensed [atv-core](https://github.com/corvofeng/atv-core/tree/8a4ada29bb7cad31e0315c3387b277eb9ab8c988) protocol implementation, which builds on [pyatv](https://github.com/postlund/pyatv). Both projects document the Companion protocol used for local Apple TV Remote interoperability. See [Apple’s Control Center Remote setup](https://support.apple.com/en-us/108778) for the iPhone controls. myRemote is an independent open-source project.
