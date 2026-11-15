//! The configuration editor in a window of its own (Windows): the page
//! served by [`crate::editor`] shown in a WebView2 control, without a
//! browser. Only one window exists; opening it again brings it to the
//! front. Closing it with unsaved changes asks first.

use std::num::NonZeroIsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::*;
use wry::raw_window_handle::{HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle};
use wry::{WebContext, WebViewBuilder};

/// The open window, 0 when there is none.
static OPEN: AtomicIsize = AtomicIsize::new(0);
/// The page has changes that are not saved.
static DIRTY: AtomicBool = AtomicBool::new(false);

const CLASS: &str = "nginx-rtmp-editor";
const TITLE: &str = "nginx configuration";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

struct Window(HWND);

impl HasWindowHandle for Window {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        let h = NonZeroIsize::new(self.0 as isize).ok_or(HandleError::Unavailable)?;
        // SAFETY: the window outlives the web view built on it (both live
        // on the window's thread until its message loop ends).
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(h))) })
    }
}

/// Show `url` in the editor window. Returns the window's thread when a
/// new window was opened, `None` when the open one was brought to the
/// front, or an error (e.g. no WebView2 runtime) so the caller can fall
/// back to the browser. Blocks until the page is created.
pub fn open(url: &str) -> Result<Option<JoinHandle<()>>, String> {
    let hwnd = OPEN.load(Ordering::SeqCst) as HWND;
    if !hwnd.is_null() {
        unsafe {
            if IsIconic(hwnd) != 0 {
                ShowWindow(hwnd, SW_RESTORE);
            }
            SetForegroundWindow(hwnd);
        }
        return Ok(None);
    }
    let (tx, rx) = mpsc::channel();
    let url = url.to_string();
    let thread = std::thread::Builder::new()
        .name("editor-window".into())
        .spawn(move || run(&url, tx))
        .map_err(|e| e.to_string())?;
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(())) => Ok(Some(thread)),
        Ok(Err(e)) => Err(e),
        Err(_) => Err("the editor window did not start".into()),
    }
}

/// WebView2 keeps its profile here instead of next to nginx.exe.
fn data_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("nginx-rtmp-module-rs").join("WebView2"))
}

fn run(url: &str, ready: mpsc::Sender<Result<(), String>>) {
    let hwnd = match create_window() {
        Ok(h) => h,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let window = Window(hwnd);
    let mut context = WebContext::new(data_dir());
    // The page and its API only; links elsewhere open in the browser.
    let base = url.to_string();
    let webview = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(url)
        .with_ipc_handler(|req| DIRTY.store(req.body() == "dirty", Ordering::SeqCst))
        .with_navigation_handler(move |to| {
            let ok = to.starts_with(&base);
            if !ok {
                crate::daemon::open_browser(&to);
            }
            ok
        })
        .with_new_window_req_handler(|to, _| {
            crate::daemon::open_browser(&to);
            wry::NewWindowResponse::Deny
        })
        .build(&window);
    let webview = match webview {
        Ok(w) => w,
        Err(e) => {
            unsafe { DestroyWindow(hwnd) };
            let _ = ready.send(Err(format!("WebView2: {e}")));
            return;
        }
    };
    DIRTY.store(false, Ordering::SeqCst);
    OPEN.store(hwnd as isize, Ordering::SeqCst);
    unsafe {
        ShowWindow(hwnd, SW_SHOWNORMAL);
        SetForegroundWindow(hwnd);
    }
    let _ = webview.focus();
    let _ = ready.send(Ok(()));
    unsafe {
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    OPEN.store(0, Ordering::SeqCst);
    drop(webview);
}

fn create_window() -> Result<HWND, String> {
    unsafe {
        let hinst = GetModuleHandleW(std::ptr::null());
        let class = wide(CLASS);
        let icon = |cx, cy| LoadImageW(hinst, std::ptr::without_provenance(1), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            hIcon: icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)),
            hIconSm: icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)),
            hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            lpszClassName: class.as_ptr(),
            ..std::mem::zeroed()
        };
        // Fails harmlessly when the class exists from an earlier window.
        RegisterClassExW(&wc);
        let title = wide(TITLE);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(format!("cannot create the editor window: {}", std::io::Error::last_os_error()));
        }
        // 1100 x 780 at 100 %, centred on the screen.
        let scale = |v: i32| v * GetDpiForWindow(hwnd).max(96) as i32 / 96;
        let (w, h) = (scale(1100), scale(780));
        let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
        let (w, h) = (w.min(sw), h.min(sh));
        SetWindowPos(hwnd, std::ptr::null_mut(), (sw - w) / 2, (sh - h) / 2, w, h, SWP_NOZORDER);
        Ok(hwnd)
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CLOSE => {
                if DIRTY.load(Ordering::SeqCst) {
                    let text = wide("The changes are not saved. Close the editor anyway?");
                    let title = wide(TITLE);
                    let r = MessageBoxW(hwnd, text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2);
                    if r != IDYES {
                        return 0;
                    }
                }
                DestroyWindow(hwnd);
                0
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp as *mut MINMAXINFO);
                let scale = |v: i32| v * GetDpiForWindow(hwnd).max(96) as i32 / 96;
                mmi.ptMinTrackSize.x = scale(640);
                mmi.ptMinTrackSize.y = scale(420);
                0
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                0
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
