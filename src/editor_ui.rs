//! The configuration editor's window (Windows), drawn with iced.
//!
//! iced has to run on a process's main thread and only once per process,
//! so the window is a process of its own: `nginx.exe --editor-ui`, started
//! by [`open`]. It reads the editor's address (token included) from its
//! standard input and talks to the editor API served by [`crate::editor`]
//! in the parent. It ends when the window is closed, or when its standard
//! input closes because the parent has exited.
//!
//! Phase 1 of the move from WebView2: an empty window that proves the
//! process model, size and start-up time.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdout, Command, ExitCode, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;

use iced::widget::{column, container, text};
use iced::{Element, Length, Size, Subscription, Task, window};

/// Hidden command-line flag that runs the window.
pub const FLAG: &str = "--editor-ui";

const TITLE: &str = "nginx configuration";
/// The child writes this line once its window is open.
const READY: &str = "ready";

/// The open window's process.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);

// ---------------------------------------------------------------- parent

/// Show the editor at `url` in its window. Starts the window's process,
/// or brings the open one to the front. Returns once the window is shown,
/// or an error (so the caller can fall back to the browser).
pub fn open(url: &str) -> Result<(), String> {
    let mut slot = CHILD.lock().unwrap();
    if let Some(child) = slot.as_mut() {
        if matches!(child.try_wait(), Ok(None)) {
            front(child.id());
            return Ok(());
        }
        *slot = None;
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(exe);
    cmd.arg(FLAG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = cmd.spawn().map_err(|e| format!("cannot start the editor window: {e}"))?;
    // The address goes through the pipe, not the command line, which
    // other programs can read. The pipe stays open: closing it tells the
    // window that this process has ended.
    let sent = child.stdin.as_mut().map(|s| writeln!(s, "{url}").and_then(|()| s.flush()));
    if !matches!(sent, Some(Ok(()))) {
        let _ = child.kill();
        return Err("cannot pass the address to the editor window".into());
    }
    let ready = child.stdout.take().map(wait_ready);
    if !matches!(ready, Some(Ok(()))) {
        let _ = child.kill();
        let _ = child.wait();
        return Err(ready.unwrap_or(Err("no output".into())).unwrap_err());
    }
    // Let the window take the foreground from this process, which has it
    // after a click on the tray menu.
    unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(child.id()) };
    *slot = Some(child);
    Ok(())
}

/// Wait until the open window is closed (`-e`).
pub fn wait() {
    let child = CHILD.lock().unwrap().take();
    if let Some(mut c) = child {
        // wait() closes the child's standard input first, which would
        // tell the window to end; keep it open until the window is closed.
        let _stdin = c.stdin.take();
        let _ = c.wait();
    }
}

fn wait_ready(out: ChildStdout) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let r = BufReader::new(out).read_line(&mut line);
        let _ = tx.send(r.map(|_| line));
    });
    match rx.recv_timeout(Duration::from_secs(30)) {
        Ok(Ok(l)) if l.trim() == READY => Ok(()),
        Ok(_) => Err("the editor window ended at start-up".into()),
        Err(_) => Err("the editor window did not start".into()),
    }
}

/// Bring the top-level windows of process `pid` to the front.
fn front(pid: u32) {
    use windows_sys::Win32::Foundation::{HWND, LPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;
    unsafe extern "system" fn each(hwnd: HWND, lp: LPARAM) -> i32 {
        unsafe {
            let mut owner = 0u32;
            GetWindowThreadProcessId(hwnd, &mut owner);
            if owner == lp as u32 && IsWindowVisible(hwnd) != 0 && GetWindow(hwnd, GW_OWNER).is_null() {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                SetForegroundWindow(hwnd);
                return 0;
            }
            1
        }
    }
    unsafe { EnumWindows(Some(each), pid as LPARAM) };
}

// ----------------------------------------------------------------- child

/// `nginx.exe --editor-ui`: run the window.
pub fn run() -> ExitCode {
    let mut stdin = std::io::stdin().lock();
    let mut url = String::new();
    if stdin.read_line(&mut url).unwrap_or(0) == 0 {
        eprintln!("nginx: {FLAG} reads the editor's address from its standard input");
        return ExitCode::FAILURE;
    }
    drop(stdin);
    // End with the parent: its end of the pipe closes when it exits.
    std::thread::spawn(|| {
        let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink());
        std::process::exit(0);
    });
    let url = url.trim().to_string();
    let r = iced::application(move || Editor { url: url.clone() }, Editor::update, Editor::view)
        .title(TITLE)
        .subscription(Editor::subscription)
        .window(window::Settings {
            size: Size::new(1100.0, 780.0),
            min_size: Some(Size::new(640.0, 420.0)),
            position: window::Position::Centered,
            ..window::Settings::default()
        })
        .run();
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("nginx: editor window: {e}");
            ExitCode::FAILURE
        }
    }
}

struct Editor {
    url: String,
}

#[derive(Debug, Clone)]
enum Message {
    Opened(window::Id),
}

impl Editor {
    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Opened(id) => {
                println!("{READY}");
                let _ = std::io::stdout().flush();
                window::gain_focus(id)
            }
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        window::open_events().map(Message::Opened)
    }

    fn view(&self) -> Element<'_, Message> {
        // Show the address without the token.
        let host = self.url.split('/').nth(2).unwrap_or("");
        container(column![text(TITLE).size(24), text(format!("Editor API on {host}"))].spacing(8))
            .padding(20)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}
