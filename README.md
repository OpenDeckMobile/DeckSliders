# DeckSliders

Lets a finger drag the two-key faders on [OpenDeck Mobile](https://github.com/OpenDeckMobile/opendeck-mobile), the
Android Stream Deck keypad.

Stream Deck's virtual-device protocol only carries key presses, so a phone can't send a fader position through
Stream Deck. OpenDeck opens a second connection to this helper on TCP 28199:

1. On every page OpenDeck names the page it shows. The helper reads that page's manifest under
   `%APPDATA%\Elgato\StreamDeck\ProfilesV3` and answers which key pairs are faders. It uses the plugins' own rule:
   a + key and a − key for the same target on adjacent keys.
   - Volume Controller NEO `manual-detection` keys with `volumeChange: adjust` and a ± `volumeStep`.
   - `com.xenside.brightness.up` / `.down` ([streamdeck-brightness](https://github.com/OpenDeckMobile/streamdeck-brightness)).
2. While a finger drags along a pair, OpenDeck sends where on the pair the finger is. The helper maps that onto the
   track the plugin draws, so the knob sits under the finger and the whole range fits between the two keys. A
   drag moves the value in steps of 5 %; the keys' own presses still step finer.
   - App volume: Core Audio `SetMasterVolume` on every session of the app on every active output. An exe matches
     by file name (Discord's path has a version folder), a package folder matches as a prefix (Spotify). The Audio
     Router sees the change and VC NEO redraws the keys.
   - Brightness: `s <0..1000>` lines on `\\.\pipe\com.xenside.brightness`. The brightness plugin keeps DDC to itself,
     because two processes talking DDC at once collide on the monitor's I2C bus.
3. On request it answers with the MAC address of the network adapter the phone reached. OpenDeck saves it for
   Wake-on-LAN: Android 11 and later no longer let an app read it from the phone's own neighbour table.

It uses one blocking thread per phone and no timers. Idle, it uses 0 CPU and 0 wakeups. Positions that arrive
together are coalesced, and only the newest per fader is applied. The wire format is at the top of `src/main.rs`
and in OpenDeck's `core/.../Sliders.kt`.

## Build and run

```powershell
cargo build --release
```

Run `target\release\DeckSliders.exe`. It has no window, so put a shortcut to it in the Startup folder to start it
at login. `--verbose` logs page lookups and drag starts to `DeckSliders.log` next to the exe. A second copy exits
because the port is taken.

The port needs an inbound firewall rule for the exe on the private network. The helper accepts only LAN addresses,
but it has no authentication: anything on your LAN can set app volumes and brightness.

## Test

```powershell
cargo test
```
