#![windows_subsystem = "windows"]
//! DeckSliders: drag-to-set for the two-key faders on OpenDeck (the phone deck).
//!
//! Stream Deck's virtual-device protocol (VSD2) carries only key presses, so a phone can't send a fader position
//! through it. OpenDeck opens a second connection to this helper instead:
//! - it names the page it shows; the helper reads that page's Stream Deck manifest and answers which key pairs are
//!   faders: Volume Controller NEO Adjust +/- pairs and com.xenside.brightness Up/Down pairs;
//! - while a finger drags along a pair it sends where on the pair the finger is; the helper turns that into the
//!   value under the finger on the drawn track and sets app volume through Core Audio, or hands brightness to the
//!   brightness plugin's pipe (the plugin owns DDC: two processes talking DDC at once collide on the I2C bus).
//!
//! The plugins redraw the keys from the new value (VC NEO through its Audio Router, brightness at once), so the
//! phone needs no value back. One blocking thread per phone and no timers: idle costs nothing.
//!
//! Wire (phone -> helper), little-endian:
//!   'P' seq:u8 page:u16 len:u8 profile[len]   which page is shown; answered with 'S'
//!   'B' id:u8                                 a drag on slider id starts
//!   'A' id:u8 position:u16                    the finger along the pair's two key images, 0..1000 from the outer
//!                                             edge of the 0 % end to the outer edge of the 100 % end
//! Wire (helper -> phone):
//!   'S' seq:u8 n:u8 n*(loCol loRow hiCol hiRow id)   the page's faders; lo is the 0 % end, hi the 100 % end,
//!                                                    id is the fader's index in this reply

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Deserialize;
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    eRender, IAudioSessionControl2, IAudioSessionManager2, IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
};

const PORT: u16 = 28199;
const PIPE: &str = r"\\.\pipe\com.xenside.brightness";
// Any context but VolumeProxy's own: Foreground then treats the write as external and forwards it.
const CTX: GUID = GUID::from_u128(0x5d3c8a41_7b2e_4f19_9c06_d1e2a3b4c5d6);
const MAX_PROFILE_LEN: usize = 64;
// Both plugins draw the track from 32 to 256 of the pair's 288 px, measured from the 0 % end (VC NEO's neoPair,
// the brightness plugin's pairSvg): the knob sits at the finger.
const PAIR: f32 = 288.0;
const TRACK_START: f32 = 32.0;
const TRACK_LEN: f32 = 224.0;

/// The value, 0..1, under a finger at [position] (0..1000 along the pair from the 0 % end).
fn value_at(position: u16) -> f32 {
    ((position.min(1000) as f32 / 1000.0 * PAIR - TRACK_START) / TRACK_LEN).clamp(0.0, 1.0)
}

static VERBOSE: AtomicBool = AtomicBool::new(false);

fn log(msg: &str) {
    if !VERBOSE.load(Ordering::Relaxed) {
        return;
    }
    let path = std::env::current_exe()
        .map(|p| p.with_file_name("DeckSliders.log"))
        .unwrap_or_else(|_| std::env::temp_dir().join("DeckSliders.log"));
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(f, "[{}.{:03}] {msg}", t.as_secs(), t.subsec_millis());
    }
}

// ---------- Stream Deck manifests ----------

#[derive(Deserialize)]
struct ProfileManifest {
    #[serde(rename = "Pages")]
    pages: PageList,
}

#[derive(Deserialize)]
struct PageList {
    #[serde(rename = "Pages", default)]
    pages: Vec<String>,
}

#[derive(Deserialize)]
struct PageManifest {
    #[serde(rename = "Controllers", default)]
    controllers: Vec<Controller>,
}

#[derive(Deserialize)]
struct Controller {
    #[serde(rename = "Type", default)]
    kind: String,
    #[serde(rename = "Actions", default)]
    actions: Option<HashMap<String, Action>>,
}

#[derive(Deserialize)]
struct Action {
    #[serde(rename = "UUID", default)]
    uuid: String,
    #[serde(rename = "Settings", default)]
    settings: Option<Settings>,
}

#[derive(Deserialize)]
struct Settings {
    #[serde(default)]
    application: Option<App>,
    #[serde(rename = "volumeChange", default)]
    volume_change: Option<String>,
    #[serde(rename = "volumeStep", default)]
    volume_step: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct App {
    #[serde(rename = "executableFile", default)]
    executable_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
enum Target {
    /// VC NEO's executableFile, lowercased: an exe path, or a package directory (Store apps such as Spotify).
    App(String),
    Brightness,
}

#[derive(Clone, Debug, PartialEq)]
struct Pair {
    lo: (u8, u8), // (col, row) of the 0 % end: the - key
    hi: (u8, u8), // the + key
    target: Target,
}

/// +1 / -1 and what the key changes, for keys that can be one end of a fader.
fn fader_key(a: &Action) -> Option<(i8, Target)> {
    match a.uuid.as_str() {
        "com.xenside.brightness.up" => Some((1, Target::Brightness)),
        "com.xenside.brightness.down" => Some((-1, Target::Brightness)),
        "com.xenside.volumecontroller.manual-detection" => {
            let s = a.settings.as_ref()?;
            if s.volume_change.as_deref() != Some("adjust") {
                return None;
            }
            // VC NEO reads the step with parseInt: "5" and 5 are steps, "0.5" is none.
            let step = match s.volume_step.as_ref()? {
                serde_json::Value::String(t) => t.trim().parse::<f64>().ok()?,
                v => v.as_f64()?,
            }
            .trunc();
            let exe = s.application.as_ref()?.executable_file.as_deref()?.trim();
            if step == 0.0 || exe.is_empty() {
                return None;
            }
            Some((if step > 0.0 { 1 } else { -1 }, Target::App(exe.to_lowercase())))
        }
        _ => None,
    }
}

/// The page's faders, by the plugins' own rule: a + key and a - key for the same target on adjacent keys.
fn pairs(page: &PageManifest) -> Vec<Pair> {
    let mut keys: Vec<(u8, u8, i8, Target)> = Vec::new();
    for c in page.controllers.iter().filter(|c| c.kind == "Keypad") {
        for (pos, a) in c.actions.iter().flatten() {
            let Some((col, row)) = pos.split_once(',').and_then(|(c, r)| Some((c.trim().parse().ok()?, r.trim().parse().ok()?)))
            else {
                continue;
            };
            if let Some((sign, target)) = fader_key(a) {
                keys.push((col, row, sign, target));
            }
        }
    }
    keys.sort_by_key(|k| (k.1, k.0)); // manifest order is a hash map's: make the pairing deterministic
    let mut used = vec![false; keys.len()];
    let mut out = Vec::new();
    for i in 0..keys.len() {
        let (pc, pr, sign, ref target) = keys[i];
        if used[i] || sign < 0 {
            continue;
        }
        let partner = (0..keys.len()).find(|&j| {
            let (mc, mr, s, ref t) = keys[j];
            !used[j] && s < 0 && t == target && pc.abs_diff(mc) + pr.abs_diff(mr) == 1
        });
        if let Some(j) = partner {
            used[i] = true;
            used[j] = true;
            out.push(Pair { lo: (keys[j].0, keys[j].1), hi: (pc, pr), target: target.clone() });
        }
    }
    out
}

/// Profile and page ids are uuids; anything else could walk out of the profiles folder.
fn is_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_PROFILE_LEN && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

/// The manifest of page [page] of profile [profile], or of the page or folder whose own id is [profile].
fn page_manifest_path(root: &Path, profile: &str, page: u16) -> Option<PathBuf> {
    if !is_id(profile) {
        return None;
    }
    let top = root.join(format!("{profile}.sdProfile"));
    if let Ok(bytes) = std::fs::read(top.join("manifest.json")) {
        let m: ProfileManifest = serde_json::from_slice(&bytes).ok()?;
        let id = m.pages.pages.get(page as usize)?;
        return is_id(id).then(|| top.join("Profiles").join(id.to_uppercase()).join("manifest.json"));
    }
    std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join("Profiles").join(profile.to_uppercase()).join("manifest.json"))
        .find(|p| p.is_file())
}

fn profiles_root() -> PathBuf {
    PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default()).join(r"Elgato\StreamDeck\ProfilesV3")
}

// ---------- Core Audio ----------

fn process_path(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut n = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, PROCESS_NAME_FORMAT(0), PWSTR(buf.as_mut_ptr()), &mut n).is_ok();
        let _ = CloseHandle(h);
        ok.then(|| String::from_utf16_lossy(&buf[..n as usize]).to_lowercase())
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('\\').next().unwrap_or(path)
}

/// Whether a session's process image matches VC NEO's executableFile. An exe matches by file name, because
/// Discord's path has a version folder that changes on every update; a package directory matches as a prefix
/// of the versioned install folder (Spotify: ...\SpotifyAB.SpotifyMusic_1.2.x_x64__zpd...\Spotify.exe).
fn app_matches(target: &str, path: &str) -> bool {
    if target.ends_with(".exe") {
        file_name(path) == file_name(target)
    } else {
        path.starts_with(target)
    }
}

/// Every audio session of the app, on every active output: apps can play on a device that isn't the default.
fn app_sessions(target: &str) -> Vec<ISimpleAudioVolume> {
    let mut out = Vec::new();
    unsafe {
        let Ok(en) = CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL) else { return out };
        let Ok(devs) = en.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) else { return out };
        let mut paths: HashMap<u32, Option<String>> = HashMap::new();
        for d in 0..devs.GetCount().unwrap_or(0) {
            let Ok(dev) = devs.Item(d) else { continue };
            let Ok(mgr) = dev.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else { continue };
            let Ok(list) = mgr.GetSessionEnumerator() else { continue };
            for i in 0..list.GetCount().unwrap_or(0) {
                let Ok(c) = list.GetSession(i) else { continue };
                let Ok(c2) = c.cast::<IAudioSessionControl2>() else { continue };
                let pid = c2.GetProcessId().unwrap_or(0);
                if pid == 0 {
                    continue; // system sounds
                }
                let path = paths.entry(pid).or_insert_with(|| process_path(pid));
                if path.as_deref().is_some_and(|p| app_matches(target, p)) {
                    if let Ok(v) = c.cast::<ISimpleAudioVolume>() {
                        out.push(v);
                    }
                }
            }
        }
    }
    out
}

// ---------- one phone ----------

enum Frame<'a> {
    Page { seq: u8, page: u16, profile: &'a str },
    Begin(u8),
    At(u8, u16),
}

/// The next frame and its length; Ok(None) if more bytes are needed, Err on anything malformed.
fn next_frame(b: &[u8]) -> Result<Option<(Frame<'_>, usize)>, ()> {
    let Some(&kind) = b.first() else { return Ok(None) };
    let need = match kind {
        b'P' => match b.get(4) {
            Some(&len) if (len as usize) <= MAX_PROFILE_LEN => 5 + len as usize,
            Some(_) => return Err(()),
            None => return Ok(None),
        },
        b'B' => 2,
        b'A' => 4,
        _ => return Err(()),
    };
    if b.len() < need {
        return Ok(None);
    }
    let f = match kind {
        b'P' => Frame::Page {
            seq: b[1],
            page: u16::from_le_bytes([b[2], b[3]]),
            profile: std::str::from_utf8(&b[5..need]).map_err(|_| ())?,
        },
        b'B' => Frame::Begin(b[1]),
        _ => Frame::At(b[1], u16::from_le_bytes([b[2], b[3]])),
    };
    Ok(Some((f, need)))
}

enum Drag {
    App { vols: Vec<ISimpleAudioVolume>, last: f32 },
    Brightness,
}

struct Conn {
    root: PathBuf,
    // Page manifest -> its faders, valid while the file's modification time is unchanged.
    cache: HashMap<PathBuf, (SystemTime, Vec<Pair>)>,
    // The targets of the last 'S' reply; a slider id is an index here. Ids depend only on the page, so a phone
    // that reconnects and names its page again keeps the ids it has. A drag keeps the target it began with.
    page: Vec<Target>,
    drags: HashMap<u8, Drag>,
    pipe: Option<File>,
}

impl Conn {
    fn page_pairs(&mut self, profile: &str, page: u16) -> Vec<Pair> {
        let Some(path) = page_manifest_path(&self.root, profile, page) else {
            log(&format!("page {profile}/{page}: no manifest"));
            return Vec::new();
        };
        let Ok(mtime) = std::fs::metadata(&path).and_then(|m| m.modified()) else { return Vec::new() };
        if let Some((t, p)) = self.cache.get(&path) {
            if *t == mtime {
                return p.clone();
            }
        }
        let found = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<PageManifest>(&b).ok())
            .map(|m| pairs(&m))
            .unwrap_or_default();
        log(&format!("page {profile}/{page}: {} -> {found:?}", path.display()));
        self.cache.insert(path, (mtime, found.clone()));
        found
    }

    fn reply_page(&mut self, seq: u8, page: u16, profile: &str) -> Vec<u8> {
        let mut out = vec![b'S', seq, 0];
        self.page.clear();
        for p in self.page_pairs(profile, page).into_iter().take(u8::MAX as usize) {
            out.extend([p.lo.0, p.lo.1, p.hi.0, p.hi.1, self.page.len() as u8]);
            self.page.push(p.target);
        }
        out[2] = self.page.len() as u8;
        out
    }

    fn begin(&mut self, id: u8) {
        let Some(target) = self.page.get(id as usize) else { return };
        let drag = match target {
            Target::App(exe) => {
                let vols = app_sessions(exe);
                log(&format!("drag {exe}: {} sessions", vols.len()));
                Drag::App { vols, last: f32::NAN }
            }
            Target::Brightness => Drag::Brightness,
        };
        self.drags.insert(id, drag);
    }

    fn drag_to(&mut self, id: u8, position: u16) {
        let value = value_at(position);
        match self.drags.get_mut(&id) {
            Some(Drag::App { vols, last }) => {
                // Whole percent, as VC NEO shows it: finer steps would only cost the plugin extra redraws.
                let v = (value * 100.0).round() / 100.0;
                if v == *last {
                    return;
                }
                *last = v;
                vols.retain(|s| unsafe { s.SetMasterVolume(v, &CTX) }.is_ok()); // a closed session drops out
            }
            Some(Drag::Brightness) => self.pipe_send(format!("s {}\n", (value * 1000.0).round() as i32).as_bytes()),
            None => {}
        }
    }

    /// Writes to the brightness plugin; reopens once if the plugin restarted, gives up if it isn't running.
    fn pipe_send(&mut self, line: &[u8]) {
        for _ in 0..2 {
            if self.pipe.is_none() {
                self.pipe = OpenOptions::new().write(true).open(PIPE).ok();
            }
            let Some(p) = self.pipe.as_mut() else { return };
            if p.write_all(line).is_ok() {
                return;
            }
            self.pipe = None;
        }
    }
}

fn serve(mut s: TcpStream, peer: SocketAddr) {
    log(&format!("{peer} connected"));
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let mut conn = Conn { root: profiles_root(), cache: HashMap::new(), page: Vec::new(), drags: HashMap::new(), pipe: None };
    let mut buf = [0u8; 4096];
    let mut len = 0;
    // Moves that arrived together: only the newest per slider is applied (a Wi-Fi stall delivers a burst).
    let mut moves: Vec<(u8, u16)> = Vec::new();
    'read: loop {
        let n = match s.read(&mut buf[len..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        len += n;
        let mut at = 0;
        loop {
            let (frame, used) = match next_frame(&buf[at..len]) {
                Ok(Some(f)) => f,
                Ok(None) => break,
                Err(()) => {
                    log(&format!("{peer}: bad frame, closing"));
                    break 'read;
                }
            };
            match frame {
                Frame::Page { seq, page, profile } => {
                    let reply = conn.reply_page(seq, page, profile);
                    if s.write_all(&reply).is_err() {
                        break 'read;
                    }
                }
                Frame::Begin(id) => {
                    // The previous drag's last position lands before the new drag starts.
                    if let Some(i) = moves.iter().position(|m| m.0 == id) {
                        let (_, at) = moves.remove(i);
                        conn.drag_to(id, at);
                    }
                    conn.begin(id);
                }
                Frame::At(id, position) => match moves.iter_mut().find(|m| m.0 == id) {
                    Some(m) => m.1 = position,
                    None => moves.push((id, position)),
                },
            }
            at += used;
        }
        for (id, position) in moves.drain(..) {
            conn.drag_to(id, position);
        }
        buf.copy_within(at..len, 0);
        len -= at;
    }
    log(&format!("{peer} gone"));
}

fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v) => v.is_private() || v.is_loopback() || v.is_link_local(),
        IpAddr::V6(v) => v.is_loopback(),
    }
}

fn main() {
    VERBOSE.store(std::env::args().any(|a| a == "--verbose"), Ordering::Relaxed);
    let listener = match TcpListener::bind(("0.0.0.0", PORT)) {
        Ok(l) => l,
        Err(e) => return log(&format!("port {PORT}: {e} (already running?)")),
    };
    log(&format!("listening on {PORT}"));
    // One live socket per phone: when it reconnects after Wi-Fi dropped, the old socket never sees a close, so
    // it is shut down here, which ends its thread.
    let live: Arc<Mutex<HashMap<IpAddr, TcpStream>>> = Arc::default();
    for s in listener.incoming() {
        let Ok(s) = s else { continue };
        let Ok(peer) = s.peer_addr() else { continue };
        if !is_lan(peer.ip()) {
            log(&format!("{peer} refused: not on the LAN"));
            continue;
        }
        let _ = s.set_nodelay(true); // moves are tiny frames: Nagle would hold each one for the last ACK
        if let Ok(c) = s.try_clone() {
            if let Some(old) = live.lock().unwrap().insert(peer.ip(), c) {
                let _ = old.shutdown(Shutdown::Both);
            }
        }
        let live = live.clone();
        std::thread::spawn(move || {
            serve(s, peer);
            let mut l = live.lock().unwrap();
            if l.get(&peer.ip()).and_then(|c| c.peer_addr().ok()) == Some(peer) {
                l.remove(&peer.ip());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(json: &str) -> PageManifest {
        serde_json::from_str(json).unwrap()
    }

    fn vc(exe: &str, step: &str) -> String {
        format!(
            r#"{{"UUID":"com.xenside.volumecontroller.manual-detection","Settings":{{"application":{{"executableFile":"{exe}"}},"volumeChange":"adjust","volumeStep":"{step}"}}}}"#
        )
    }

    #[test]
    fn pairs_like_the_phone_profile() {
        let d = r"C:\\Users\\x\\Discord.exe";
        let json = format!(
            r#"{{"Controllers":[{{"Type":"Keypad","Actions":{{
                "0,0":{},"0,1":{},
                "1,0":{},"1,1":{},
                "2,0":{},
                "4,0":{{"UUID":"com.xenside.brightness.up","Settings":{{}}}},
                "4,1":{{"UUID":"com.xenside.brightness.down","Settings":null}},
                "3,2":{{"UUID":"com.elgato.streamdeck.keys.logic"}}
            }}}}]}}"#,
            vc(d, "5"),
            vc(d, "-5"),
            vc(r"C:\\Program Files\\WindowsApps\\SpotifyAB.SpotifyMusic", "1"),
            vc(r"C:\\Program Files\\WindowsApps\\SpotifyAB.SpotifyMusic", "-1"),
            vc(r"C:\\Firefox\\firefox.exe", "5"), // no partner
        );
        let p = pairs(&page(&json));
        assert_eq!(p.len(), 3, "{p:?}");
        assert_eq!(p[0], Pair { lo: (0, 1), hi: (0, 0), target: Target::App(r"c:\users\x\discord.exe".into()) });
        assert_eq!(p[1].target, Target::App(r"c:\program files\windowsapps\spotifyab.spotifymusic".into()));
        assert_eq!(p[2], Pair { lo: (4, 1), hi: (4, 0), target: Target::Brightness });
    }

    #[test]
    fn no_pair_across_apps_or_diagonals_or_set_keys() {
        let set = r#"{"UUID":"com.xenside.volumecontroller.manual-detection","Settings":{"application":{"executableFile":"a.exe"},"volumeChange":"set","volumeStep":"-5"}}"#;
        let json = format!(
            r#"{{"Controllers":[{{"Type":"Keypad","Actions":{{"0,0":{},"0,1":{},"1,0":{},"2,1":{},"3,0":{},"3,1":{set}}}}}]}}"#,
            vc("a.exe", "5"),
            vc("b.exe", "-5"),
            vc("c.exe", "5"),
            vc("c.exe", "-5"),
            vc("a.exe", "5"),
        );
        assert!(pairs(&page(&json)).is_empty());
    }

    #[test]
    fn matches_discord_by_name_and_spotify_by_package() {
        assert!(app_matches(r"c:\users\x\appdata\local\discord\app-1.0.9262\discord.exe", r"c:\users\x\appdata\local\discord\app-1.0.9300\discord.exe"));
        assert!(!app_matches(r"c:\a\discord.exe", r"c:\a\discordptb.exe"));
        let spotify = r"c:\program files\windowsapps\spotifyab.spotifymusic";
        assert!(app_matches(spotify, r"c:\program files\windowsapps\spotifyab.spotifymusic_1.2.77.358.0_x64__zpdnekdrzrea0\spotify.exe"));
        assert!(!app_matches(spotify, r"c:\program files\mozilla firefox\firefox.exe"));
    }

    #[test]
    fn frames() {
        let mut b = vec![b'P', 7, 2, 0, 3];
        b.extend(b"abc");
        b.extend([b'A', 1, 0xe8, 0x03, b'B']); // at 1000, then half a begin
        let Ok(Some((Frame::Page { seq: 7, page: 2, profile: "abc" }, 8))) = next_frame(&b) else { panic!() };
        let Ok(Some((Frame::At(1, 1000), 4))) = next_frame(&b[8..]) else { panic!() };
        assert!(next_frame(&[b'M', 1, 0, 0]).is_err()); // the old relative move
        assert!(matches!(next_frame(&b[12..]), Ok(None)));
        assert!(next_frame(&[b'X']).is_err());
        assert!(next_frame(&[b'P', 0, 0, 0, 200]).is_err());
    }

    #[test]
    fn pages_resolve_and_ids_depend_only_on_the_page() {
        let root = std::env::temp_dir().join(format!("decksliders-test-{}", std::process::id()));
        let profile = root.join("AAAA-1.sdProfile");
        let p1 = profile.join(r"Profiles\BBBB-1");
        let p2 = profile.join(r"Profiles\CCCC-2");
        std::fs::create_dir_all(&p1).unwrap();
        std::fs::create_dir_all(&p2).unwrap();
        std::fs::write(profile.join("manifest.json"), r#"{"Pages":{"Pages":["bbbb-1","cccc-2"]}}"#).unwrap();
        let keys = |a: &str, b: &str| format!(r#"{{"Controllers":[{{"Type":"Keypad","Actions":{{"0,0":{},"0,1":{}}}}}]}}"#, vc(a, "5"), vc(a, "-5")).replace("0,0\":", &format!("{b},0\":")).replace("0,1\":", &format!("{b},1\":"));
        std::fs::write(p1.join("manifest.json"), keys("one.exe", "0")).unwrap();
        std::fs::write(p2.join("manifest.json"), keys("two.exe", "3")).unwrap();
        let mut c = Conn { root: root.clone(), cache: HashMap::new(), page: Vec::new(), drags: HashMap::new(), pipe: None };
        // By profile and index, and by the page's own id (what Stream Deck actually sends): same answer.
        assert_eq!(c.reply_page(1, 1, "AAAA-1"), vec![b'S', 1, 1, 3, 1, 3, 0, 0]);
        assert_eq!(c.reply_page(2, 0, "cccc-2"), vec![b'S', 2, 1, 3, 1, 3, 0, 0]);
        assert_eq!(c.page, vec![Target::App("two.exe".into())]);
        assert_eq!(c.reply_page(3, 0, "BBBB-1"), vec![b'S', 3, 1, 0, 1, 0, 0, 0]);
        assert_eq!(c.page, vec![Target::App("one.exe".into())]);
        assert_eq!(c.reply_page(4, 0, "DDDD-9"), vec![b'S', 4, 0]);
        assert!(c.page.is_empty());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_knob_sits_under_the_finger() {
        // The track runs from 32 to 256 of the pair's 288 px.
        assert_eq!(value_at(0), 0.0);
        assert_eq!(value_at(111), 0.0); // 32 px
        assert!((value_at(500) - 0.5).abs() < 1e-6); // the line between the keys
        assert_eq!(value_at(889), 1.0); // 256 px
        assert_eq!(value_at(1000), 1.0);
        assert_eq!(value_at(u16::MAX), 1.0);
    }

    #[test]
    fn ids_cannot_leave_the_profiles_folder() {
        assert!(is_id("52762A9B-1C60-4CC6-B871-6C053CC3EAE2"));
        assert!(!is_id(r"..\..\x"));
        assert!(!is_id(""));
        assert!(page_manifest_path(Path::new("."), "../x", 0).is_none());
    }
}
