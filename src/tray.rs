//! Notification-area icon (Windows): live state at a glance, a tooltip with
//! the current bitrate, a toast when a stream starts or ends, and a menu with
//! every stream and connection.
//!
//! Plain Win32 on a thread of its own with its own message loop; it reads
//! snapshots from the live registry once a second.

use rtmp_server::live::{Registry, SessionId, StreamInfo};
use rtmp_server::monitor::{ConnInfo, Role};
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_NOSOUND, NIIF_USER, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW,
    Shell_NotifyIconW, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
/// `NIN_KEYSELECT` (NIN_SELECT | NINF_KEY), missing from windows-sys.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const TIMER_ID: usize = 1;
const CLASS: &str = "rtmp-server-tray";

/// What the tray needs to know about the running server.
pub struct TrayOptions {
    pub registry: Arc<Registry>,
    pub listen: Vec<(usize, SocketAddr, bool)>,
    pub conf_path: PathBuf,
    pub logs_dir: PathBuf,
    /// Notified when the user picks "Stop server".
    pub stop: Arc<Notify>,
}

/// Removes the icon and ends the tray thread when dropped.
pub struct Tray {
    hwnd: usize,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Tray {
    fn drop(&mut self) {
        // SAFETY: posting to a window owned by the tray thread; harmless if it is gone.
        unsafe { PostMessageW(self.hwnd as HWND, WM_CLOSE, 0, 0) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

pub fn start(opts: TrayOptions) -> Option<Tray> {
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("tray".into())
        .spawn(move || {
            // SAFETY: the window and its message loop live on this thread.
            match unsafe { create_window() } {
                Some(hwnd) => {
                    let _ = tx.send(Some(hwnd as usize));
                    STATE.with(|s| *s.borrow_mut() = Some(State::new(hwnd, opts)));
                    STATE.with(|s| s.borrow_mut().as_mut().unwrap().add_icon());
                    message_loop();
                }
                None => {
                    let _ = tx.send(None);
                }
            }
        })
        .ok()?;
    let hwnd = rx.recv().ok().flatten()?;
    Some(Tray { hwnd, thread: Some(thread) })
}

// ---------------------------------------------------------------------
// Window plumbing
// ---------------------------------------------------------------------

thread_local! {
    static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Copy `s` into a fixed, NUL-terminated buffer, truncating if needed.
fn fill(dst: &mut [u16], s: &str) {
    let mut n = 0;
    let max = dst.len() - 1;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16()).take(max) {
        *d = c;
        n += 1;
    }
    dst[n] = 0;
}

unsafe fn create_window() -> Option<HWND> {
    unsafe {
        // Crisp menus and icon on high-DPI screens; the process has no other UI.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        RegisterClassW(&wc);
        let title = wide("rtmp-server");
        // A regular (never shown) window: menus need one that can take the foreground.
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            tracing::warn!("tray: CreateWindowEx failed: {}", std::io::Error::last_os_error());
            None
        } else {
            Some(hwnd)
        }
    }
}

fn message_loop() {
    // SAFETY: standard message loop on the thread that owns the window.
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn taskbar_created() -> u32 {
    static MSG: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    // SAFETY: NUL-terminated name.
    *MSG.get_or_init(|| unsafe { RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()) })
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let with = |f: &mut dyn FnMut(&mut State)| {
        STATE.with(|s| {
            if let Ok(mut s) = s.try_borrow_mut()
                && let Some(s) = s.as_mut()
            {
                f(s)
            }
        })
    };
    match msg {
        WM_TIMER => with(&mut |s| s.tick()),
        WM_TRAY => {
            let event = (lp & 0xffff) as u32;
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT {
                show_menu(hwnd);
            }
        }
        WM_CLOSE => {
            with(&mut |s| s.remove_icon());
            // SAFETY: our own window, on its thread.
            unsafe {
                DestroyWindow(hwnd);
                PostQuitMessage(0);
            }
        }
        m if m == taskbar_created() => with(&mut |s| s.add_icon()),
        // SAFETY: default handling for everything else.
        _ => return unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
    0
}

/// Build the menu from fresh data, run it, then act on the choice. The
/// state is not borrowed while the menu is open: its modal loop keeps
/// delivering timer ticks.
fn show_menu(hwnd: HWND) {
    let Some(menu) = STATE.with(|s| s.borrow_mut().as_mut().map(|s| s.build_menu())) else { return };
    // SAFETY: the menu handle is valid until DestroyMenu below.
    let cmd = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);
        SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            0,
            hwnd,
            std::ptr::null(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
        cmd
    };
    if cmd > 0 {
        STATE.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.run(cmd as usize);
            }
        });
    }
}

// ---------------------------------------------------------------------
// State
// ---------------------------------------------------------------------

enum Action {
    Copy(String),
    Launch(PathBuf, String),
    Open(PathBuf),
    Disconnect(SessionId),
    ToggleNotify,
    Stop,
}

struct Seen {
    since: Instant,
    announced: bool,
    bytes: u64,
    label: String,
}

struct State {
    hwnd: HWND,
    opts: TrayOptions,
    icon_idle: HICON,
    icon_live: HICON,
    live: bool,
    started: Instant,
    /// Last byte counters per connection, for rates.
    prev: HashMap<SessionId, (u64, u64, Instant)>,
    /// Bits per second (in, out) per connection.
    rates: HashMap<SessionId, (f64, f64)>,
    /// Streams with a publisher, by "server/app/name".
    seen: HashMap<String, Seen>,
    notify: bool,
    actions: Vec<Action>,
    ffplay: Option<PathBuf>,
    vlc: Option<PathBuf>,
    lan_ip: Option<IpAddr>,
    conns: Vec<ConnInfo>,
    streams: Vec<StreamInfo>,
}

fn load_icon(id: usize) -> HICON {
    // SAFETY: loading an icon resource embedded by build.rs, by integer id.
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let (cx, cy) = (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let h = LoadImageW(hinst, id as *const u16, IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR);
        if h.is_null() { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) } else { h as HICON }
    }
}

fn find_in_path(exe: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Address of the interface that routes outside (no packet is sent: a UDP
/// "connect" only picks the route).
fn lan_ip() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

impl State {
    fn new(hwnd: HWND, opts: TrayOptions) -> Self {
        let vlc = ["ProgramFiles", "ProgramFiles(x86)"]
            .iter()
            .filter_map(std::env::var_os)
            .map(|d| PathBuf::from(d).join("VideoLAN").join("VLC").join("vlc.exe"))
            .find(|p| p.is_file());
        let mut s = State {
            hwnd,
            opts,
            icon_idle: load_icon(1),
            icon_live: load_icon(2),
            live: false,
            started: Instant::now(),
            prev: HashMap::new(),
            rates: HashMap::new(),
            seen: HashMap::new(),
            notify: true,
            actions: Vec::new(),
            ffplay: find_in_path("ffplay.exe"),
            vlc,
            lan_ip: lan_ip(),
            conns: Vec::new(),
            streams: Vec::new(),
        };
        s.refresh();
        // SAFETY: timer on our own window.
        unsafe { SetTimer(hwnd, TIMER_ID, 1000, None) };
        s
    }

    fn nid(&self) -> NOTIFYICONDATAW {
        // SAFETY: plain C struct, all-zero is a valid empty value.
        let mut n: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        n.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        n.hWnd = self.hwnd;
        n.uID = 1;
        n
    }

    fn add_icon(&mut self) {
        let mut n = self.nid();
        n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP;
        n.uCallbackMessage = WM_TRAY;
        n.hIcon = if self.live { self.icon_live } else { self.icon_idle };
        fill(&mut n.szTip, &self.tooltip());
        n.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: valid, initialised NOTIFYICONDATAW.
        let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &n) } != 0;
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_SETVERSION, &n) };
        if !ok {
            tracing::warn!("tray: could not add the icon: {}", std::io::Error::last_os_error());
        } else {
            tracing::debug!("tray: icon added");
        }
    }

    fn remove_icon(&mut self) {
        let n = self.nid();
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_DELETE, &n) };
    }

    fn toast(&self, title: &str, text: &str) {
        let mut n = self.nid();
        n.uFlags = NIF_INFO;
        fill(&mut n.szInfoTitle, title);
        fill(&mut n.szInfo, text);
        n.dwInfoFlags = NIIF_USER | NIIF_NOSOUND;
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
    }

    /// Take new snapshots and work out per-connection rates.
    fn refresh(&mut self) {
        let now = Instant::now();
        self.conns = self.opts.registry.monitor.snapshot();
        self.streams = self.opts.registry.streams();
        let mut prev = HashMap::new();
        for c in &self.conns {
            if let Some(&(bi, bo, t)) = self.prev.get(&c.id) {
                let dt = now.duration_since(t).as_secs_f64().max(0.001);
                let r = ((c.bytes_in - bi) as f64 * 8.0 / dt, (c.bytes_out - bo) as f64 * 8.0 / dt);
                // Light smoothing so the numbers don't jump around.
                let old = self.rates.get(&c.id).copied().unwrap_or(r);
                self.rates.insert(c.id, (old.0 * 0.4 + r.0 * 0.6, old.1 * 0.4 + r.1 * 0.6));
            }
            prev.insert(c.id, (c.bytes_in, c.bytes_out, now));
        }
        self.rates.retain(|id, _| prev.contains_key(id));
        self.prev = prev;
    }

    fn conn(&self, id: SessionId) -> Option<&ConnInfo> {
        self.conns.iter().find(|c| c.id == id)
    }

    fn rate(&self, id: SessionId) -> (f64, f64) {
        self.rates.get(&id).copied().unwrap_or_default()
    }

    fn tick(&mut self) {
        self.refresh();

        // Stream start / end toasts. A stream is announced on its second
        // tick, once its metadata (resolution, frame rate) has arrived.
        let mut current = HashMap::new();
        for s in self.streams.iter().filter(|s| s.publisher.is_some()) {
            let key = format!("{}/{}/{}", s.server, s.app, s.name);
            let bytes = s.publisher.and_then(|p| self.conn(p)).map_or(0, |c| c.bytes_in);
            let label = stream_label(s);
            let mut seen = self.seen.remove(&key).unwrap_or(Seen {
                since: Instant::now(),
                announced: false,
                bytes,
                label: String::new(),
            });
            seen.bytes = seen.bytes.max(bytes);
            if !label.is_empty() {
                seen.label = label;
            }
            if !seen.announced && seen.since.elapsed().as_millis() >= 1500 {
                seen.announced = true;
                if self.notify {
                    let from = s.publisher.and_then(|p| self.conn(p)).map(|c| c.addr.ip().to_string());
                    let mut text = seen.label.clone();
                    if let Some(ip) = from {
                        text = if text.is_empty() { format!("from {ip}") } else { format!("{text} · from {ip}") };
                    }
                    self.toast(&format!("● {}/{} is live", s.app, s.name), &text);
                }
            }
            current.insert(key, seen);
        }
        for (key, seen) in std::mem::take(&mut self.seen) {
            if seen.announced && self.notify {
                let name = key.split_once('/').map_or(key.as_str(), |(_, r)| r);
                self.toast(
                    &format!("■ {name} ended"),
                    &format!(
                        "Live for {} · {} received",
                        fmt_dur(seen.since.elapsed().as_secs()),
                        fmt_bytes(seen.bytes)
                    ),
                );
            }
        }
        self.seen = current;

        let live = !self.seen.is_empty();
        let mut n = self.nid();
        n.uFlags = NIF_TIP | NIF_SHOWTIP | NIF_ICON;
        n.hIcon = if live { self.icon_live } else { self.icon_idle };
        fill(&mut n.szTip, &self.tooltip());
        // SAFETY: as above.
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &n) };
        self.live = live;
    }

    fn totals(&self) -> (f64, f64) {
        self.rates.values().fold((0.0, 0.0), |a, r| (a.0 + r.0, a.1 + r.1))
    }

    fn tooltip(&self) -> String {
        let live: Vec<&StreamInfo> = self.streams.iter().filter(|s| s.publisher.is_some()).collect();
        let viewers: usize = self.streams.iter().map(|s| s.players.len()).sum();
        let mut t = String::from("rtmp-server");
        match live.as_slice() {
            [] => t.push_str(" · idle"),
            [s] => t.push_str(&format!("\n● {}/{} · {} viewer{}", s.app, s.name, viewers, plural(viewers))),
            l => t.push_str(&format!("\n● {} streams live · {} viewer{}", l.len(), viewers, plural(viewers))),
        }
        let (rin, rout) = self.totals();
        if rin + rout > 0.0 {
            t.push_str(&format!("\n↓ {}  ↑ {}", fmt_rate(rin), fmt_rate(rout)));
        }
        t
    }

    // -----------------------------------------------------------------
    // Menu
    // -----------------------------------------------------------------

    fn add(&mut self, menu: HMENU, text: &str, action: Option<Action>, flags: MENU_ITEM_FLAGS) {
        let id = match action {
            Some(a) => {
                self.actions.push(a);
                self.actions.len()
            }
            None => 0,
        };
        let flags = if id == 0 && flags & MF_POPUP == 0 { flags | MF_GRAYED } else { flags };
        let text = wide(&text.replace('&', "&&"));
        // SAFETY: valid menu handle, NUL-terminated text.
        unsafe { AppendMenuW(menu, MF_STRING | flags, id, text.as_ptr()) };
    }

    fn sub(&mut self, menu: HMENU, text: &str, sub: HMENU) {
        let text = wide(&text.replace('&', "&&"));
        // SAFETY: `sub` becomes owned by `menu`.
        unsafe { AppendMenuW(menu, MF_STRING | MF_POPUP, sub as usize, text.as_ptr()) };
    }

    fn sep(menu: HMENU) {
        // SAFETY: valid menu handle.
        unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null()) };
    }

    fn new_menu() -> HMENU {
        // SAFETY: plain allocation; freed with the root menu.
        unsafe { CreatePopupMenu() }
    }

    /// `rtmp://` URLs to reach a stream: local, and on the LAN when the
    /// server listens on every interface.
    fn urls(&self, s: &StreamInfo) -> Vec<(&'static str, String)> {
        // Prefer a plain listener: players connect to it without certificate trouble.
        let mut mine: Vec<_> = self.opts.listen.iter().filter(|(i, _, _)| *i == s.server).collect();
        mine.sort_by_key(|(_, _, tls)| *tls);
        let Some(&&(_, addr, tls)) = mine.first() else { return vec![] };
        let (scheme, default_port) = if tls { ("rtmps", 443) } else { ("rtmp", 1935) };
        let port = if addr.port() == default_port { String::new() } else { format!(":{}", addr.port()) };
        let path = format!("{}/{}", s.app, s.name);
        let mut v = Vec::new();
        if addr.ip().is_unspecified() {
            v.push(("Copy URL", format!("{scheme}://127.0.0.1{port}/{path}")));
            if let Some(ip) = self.lan_ip {
                v.push(("Copy LAN URL", format!("{scheme}://{ip}{port}/{path}")));
            }
        } else {
            v.push(("Copy URL", format!("{scheme}://{}{port}/{path}", addr.ip())));
        }
        v
    }

    fn build_menu(&mut self) -> HMENU {
        self.refresh();
        self.actions.clear();
        let root = Self::new_menu();

        let ports: Vec<String> = self
            .opts
            .listen
            .iter()
            .map(|(_, a, tls)| if *tls { format!("{a} (rtmps)") } else { a.to_string() })
            .collect();
        self.add(root, &format!("rtmp-server · {}", ports.join(", ")), None, 0);
        let (rin, rout) = self.totals();
        self.add(
            root,
            &format!(
                "Up {} · {} connection{} · ↓ {} ↑ {}",
                fmt_dur(self.started.elapsed().as_secs()),
                self.conns.len(),
                plural(self.conns.len()),
                fmt_rate(rin),
                fmt_rate(rout)
            ),
            None,
            0,
        );
        Self::sep(root);

        let streams = self.streams.clone();
        if streams.is_empty() {
            let hint = self
                .opts
                .listen
                .first()
                .map(|(_, a, _)| format!("No streams · publish to rtmp://127.0.0.1:{}/<app>/<key>", a.port()))
                .unwrap_or_else(|| "No streams".into());
            self.add(root, &hint, None, 0);
        }
        for s in &streams {
            let sm = self.stream_menu(s);
            let rate = s.publisher.map(|p| self.rate(p).0).unwrap_or(0.0);
            let n = s.players.len();
            let title = match s.publisher {
                Some(_) => {
                    let mut info = vec![];
                    let res = short_res(&s.meta);
                    if !res.is_empty() {
                        info.push(res);
                    }
                    info.push(fmt_rate(rate));
                    info.push(format!("{n} viewer{}", plural(n)));
                    format!("● {}/{}\t{}", s.app, s.name, info.join(" · "))
                }
                None => format!("○ {}/{}\twaiting · {n} viewer{}", s.app, s.name, plural(n)),
            };
            self.sub(root, &title, sm);
        }
        Self::sep(root);

        let cm = Self::new_menu();
        let conns = self.conns.clone();
        if conns.is_empty() {
            self.add(cm, "No connections", None, 0);
        }
        for c in &conns {
            let (rin, rout) = self.rate(c.id);
            let what = match c.role {
                Role::Publishing => format!("PUBLISH {}/{}\t↓ {}", c.app, c.name, fmt_rate(rin)),
                Role::Playing => format!("PLAY {}/{}\t↑ {}", c.app, c.name, fmt_rate(rout)),
                Role::Idle if c.app.is_empty() => "connecting".into(),
                Role::Idle => format!("connected to {}", c.app),
            };
            let one = Self::new_menu();
            if !c.flashver.is_empty() {
                self.add(one, &c.flashver, None, 0);
            }
            self.add(
                one,
                &format!("Connected {} · ↓ {} · ↑ {}", fmt_dur(c.secs), fmt_bytes(c.bytes_in), fmt_bytes(c.bytes_out)),
                None,
                0,
            );
            Self::sep(one);
            self.add(one, "Disconnect", Some(Action::Disconnect(c.id)), 0);
            self.sub(cm, &format!("#{} {} · {}", c.id, c.addr.ip(), what), one);
        }
        self.sub(root, &format!("Connections ({})", conns.len()), cm);
        Self::sep(root);

        self.add(root, "Notify when streams start and end", Some(Action::ToggleNotify), if self.notify { MF_CHECKED } else { 0 });
        self.add(root, "Open nginx.conf", Some(Action::Open(self.opts.conf_path.clone())), 0);
        self.add(root, "Open logs folder", Some(Action::Open(self.opts.logs_dir.clone())), 0);
        Self::sep(root);
        self.add(root, "Stop server", Some(Action::Stop), 0);
        root
    }

    fn stream_menu(&mut self, s: &StreamInfo) -> HMENU {
        let m = Self::new_menu();
        match s.publisher.and_then(|p| self.conn(p).cloned()) {
            Some(p) => {
                let sw = if p.flashver.is_empty() { String::new() } else { format!(" · {}", p.flashver) };
                self.add(m, &format!("Publisher {}{} · live {}", p.addr.ip(), sw, fmt_dur(p.secs)), None, 0);
                let label = stream_label(s);
                if !label.is_empty() {
                    self.add(m, &label, None, 0);
                }
            }
            None if s.publisher.is_some() => self.add(m, "Published by a relay", None, 0),
            None => self.add(m, "Waiting for a publisher", None, 0),
        }
        Self::sep(m);
        let urls = self.urls(s);
        for (what, url) in &urls {
            self.add(m, &format!("{what}\t{url}"), Some(Action::Copy(url.clone())), 0);
        }
        if let Some((_, url)) = urls.first() {
            if let Some(ff) = self.ffplay.clone() {
                let args = format!("-fflags nobuffer -window_title \"{}/{}\" \"{url}\"", s.app, s.name);
                self.add(m, "Watch in ffplay", Some(Action::Launch(ff, args)), 0);
            }
            if let Some(vlc) = self.vlc.clone() {
                self.add(m, "Watch in VLC", Some(Action::Launch(vlc, format!("\"{url}\""))), 0);
            }
        }
        if !s.players.is_empty() {
            Self::sep(m);
            self.add(m, &format!("Viewers ({})", s.players.len()), None, 0);
            for &id in &s.players {
                let Some(c) = self.conn(id).cloned() else { continue };
                let v = Self::new_menu();
                self.add(v, "Disconnect", Some(Action::Disconnect(id)), 0);
                self.sub(m, &format!("   {} · {}\t{}", c.addr.ip(), fmt_dur(c.secs), fmt_rate(self.rate(id).1)), v);
            }
        }
        if let Some(p) = s.publisher.filter(|&p| self.conn(p).is_some()) {
            Self::sep(m);
            self.add(m, "Disconnect publisher", Some(Action::Disconnect(p)), 0);
        }
        m
    }

    fn run(&mut self, cmd: usize) {
        let Some(action) = self.actions.get(cmd - 1) else { return };
        match action {
            Action::Copy(text) => {
                let text = text.clone();
                if set_clipboard(self.hwnd, &text) {
                    self.toast("Copied to the clipboard", &text);
                }
            }
            Action::Launch(exe, args) => {
                if !shell_open(&exe.display().to_string(), Some(args)) {
                    self.toast("Could not start the player", &exe.display().to_string());
                }
            }
            Action::Open(path) => {
                let _ = std::fs::create_dir_all(path.parent().filter(|_| path.extension().is_some()).unwrap_or(path));
                shell_open(&path.display().to_string(), None);
            }
            Action::Disconnect(id) => {
                self.opts.registry.monitor.disconnect(*id);
            }
            Action::ToggleNotify => self.notify = !self.notify,
            Action::Stop => self.opts.stop.notify_one(),
        }
    }
}

fn shell_open(file: &str, args: Option<&str>) -> bool {
    let (verb, file) = (wide("open"), wide(file));
    let args = args.map(wide);
    // SAFETY: NUL-terminated strings that outlive the call.
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            args.as_ref().map_or(std::ptr::null(), |a| a.as_ptr()),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    r as usize > 32
}

fn set_clipboard(hwnd: HWND, text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let w = wide(text);
    // SAFETY: standard clipboard sequence; on success the system owns the memory.
    unsafe {
        if OpenClipboard(hwnd) == 0 {
            return false;
        }
        EmptyClipboard();
        let mem = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        let mut ok = false;
        if !mem.is_null() {
            let p = GlobalLock(mem) as *mut u16;
            if !p.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), p, w.len());
                GlobalUnlock(mem);
                ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
            }
        }
        CloseClipboard();
        ok
    }
}

// ---------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn fmt_rate(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1} Mbps", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.0} kbps", bps / 1e3)
    } else {
        format!("{bps:.0} bps")
    }
}

fn fmt_bytes(b: u64) -> String {
    let b = b as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

fn fmt_dur(s: u64) -> String {
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
    }
}

fn fourcc(v: f64) -> Option<[u8; 4]> {
    (v > 255.0 && v < 4294967296.0).then(|| (v as u32).to_be_bytes())
}

fn video_codec(id: f64) -> String {
    match id as u32 {
        0 => String::new(),
        2 => "H.263".into(),
        4 | 5 => "VP6".into(),
        7 => "H.264".into(),
        12 => "HEVC".into(),
        _ => match fourcc(id).as_ref() {
            Some(b"avc1") => "H.264".into(),
            Some(b"hvc1") => "HEVC".into(),
            Some(b"av01") => "AV1".into(),
            Some(b"vp09") => "VP9".into(),
            Some(c) => String::from_utf8_lossy(c).into_owned(),
            None => format!("codec {id}"),
        },
    }
}

fn audio_codec(id: f64) -> String {
    match id as u32 {
        2 | 14 => "MP3".into(),
        10 => "AAC".into(),
        11 => "Speex".into(),
        _ => match fourcc(id).as_ref() {
            Some(b"mp4a") => "AAC".into(),
            Some(b"Opus") => "Opus".into(),
            Some(c) => String::from_utf8_lossy(c).into_owned(),
            None => String::new(),
        },
    }
}

/// "1080p30".
fn short_res(m: &rtmp_server::codec::MetaInfo) -> String {
    match (m.height as u32, m.frame_rate.round() as u32) {
        (0, _) => String::new(),
        (h, 0) => format!("{h}p"),
        (h, f) => format!("{h}p{f}"),
    }
}

/// "H.264 1920×1080 @ 30 fps · AAC".
fn stream_label(s: &StreamInfo) -> String {
    let m = &s.meta;
    let mut v = video_codec(m.video_codec_id);
    if m.width > 0.0 && m.height > 0.0 {
        v = format!("{v} {}×{}", m.width as u32, m.height as u32).trim().to_string();
    }
    if m.frame_rate > 0.0 {
        v = format!("{v} @ {} fps", (m.frame_rate * 100.0).round() / 100.0);
    }
    let a = audio_codec(m.audio_codec_id);
    match (v.is_empty(), a.is_empty()) {
        (false, false) => format!("{v} · {a}"),
        (false, true) => v,
        (true, false) => a,
        (true, true) => String::new(),
    }
}
