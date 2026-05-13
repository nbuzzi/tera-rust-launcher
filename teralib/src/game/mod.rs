// External crate imports
use crate::{
    config,
    global_credentials::{set_credentials, GLOBAL_CREDENTIALS},
};
use lazy_static::lazy_static;
use log::{error, info, Level, Metadata, Record};
use once_cell::sync::Lazy;
use prost::Message;
use quick_xml::de::from_str as xml_from_str;
use reqwest;
use serde::Deserialize;
use serde_json::Value;
use std::{
    ffi::OsStr,
    os::windows::ffi::OsStrExt,
    process::{Command, ExitStatus},
    ptr::null_mut,
    slice,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc, {Arc, Mutex},
    },
    time::Duration,
};
use tokio::sync::{mpsc as other_mpsc, watch, Notify};
use winapi::{
    shared::{
        minwindef::{BOOL, DWORD, LPARAM, LRESULT, TRUE, UINT, WPARAM},
        windef::HWND,
    },
    um::{
        errhandlingapi::GetLastError,
        libloaderapi::GetModuleHandleW,
        winuser::{GetClassInfoExW, GetWindowThreadProcessId, *},
    },
};

// Constants
const WM_GAME_EXITED: u32 = WM_USER + 1;

/// Module for handling server list functionality.
///
/// This module includes the generated code from the `_serverlist_proto.rs` file,
/// which likely contains protobuf-generated structures and functions for
/// managing server list data.
mod serverlist {
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "\\src\\_serverlist_proto.rs"
    ));
}
use serverlist::{server_list::ServerInfo, ServerList};

// Global static variables
lazy_static! {
    static ref SERVER_LIST_SENDER: Mutex<Option<mpsc::Sender<(WPARAM, usize)>>> = Mutex::new(None);

    /// Cache of the protobuf-encoded server list, populated once before TERA.exe
    /// is spawned. Why: TERA's IPC pump on a UI thread cannot tolerate the long
    /// blocking HTTP fetch that `handle_server_list_request` used to do
    /// (it spun up a fresh tokio Runtime inside a SendMessage handler — runtime
    /// inside runtime panics, slow handshakes, lost server list). With the
    /// cache we can answer event 5 synchronously with the pre-baked bytes.
    static ref SERVER_LIST_CACHE: Mutex<Option<Vec<u8>>> = Mutex::new(None);
}

/// PID of the spawned TERA.exe process. Used by `find_game_main_window` to
/// locate the game's IPC window (TERA creates it asynchronously, so the
/// handshake task has to poll for it after spawn).
static GAME_PID: Lazy<AtomicU32> = Lazy::new(|| AtomicU32::new(0));

/// Handle to the game window.
///
/// This static variable holds a mutex-protected optional `SafeHWND`,
/// which represents the handle to the game window.
static WINDOW_HANDLE: Lazy<Mutex<Option<SafeHWND>>> = Lazy::new(|| Mutex::new(None));

/// Flag indicating whether the game is currently running.
///
/// This atomic boolean is used to track the running state of the game
/// across multiple threads.
static GAME_RUNNING: Lazy<AtomicBool> = Lazy::new(|| AtomicBool::new(false));

/// Sender for game status updates.
///
/// This channel sender is used to broadcast changes in the game's running state
/// to any interested receivers.
static GAME_STATUS_SENDER: Lazy<watch::Sender<bool>> = Lazy::new(|| {
    let (tx, _) = watch::channel(false);
    tx
});

// Struct definitions
#[derive(Clone, Copy)]
struct SafeHWND(HWND);

// Implementations
unsafe impl Send for SafeHWND {}
unsafe impl Sync for SafeHWND {}

impl SafeHWND {
    /// Creates a new `SafeHWND` instance.
    ///
    /// This function wraps a raw `HWND` into a `SafeHWND` struct, providing a safer interface
    /// for handling window handles.
    ///
    /// # Arguments
    ///
    /// * `hwnd` - A raw window handle of type `HWND`.
    ///
    /// # Returns
    ///
    /// A new `SafeHWND` instance containing the provided window handle.
    fn new(hwnd: HWND) -> Self {
        SafeHWND(hwnd)
    }

    /// Retrieves the raw window handle.
    ///
    /// This method provides access to the underlying `HWND` stored in the `SafeHWND` instance.
    ///
    /// # Returns
    ///
    /// The raw `HWND` window handle.
    fn get(&self) -> HWND {
        self.0
    }
}

/// A custom logger for the Tera application.
///
/// This struct implements the `log::Log` trait and provides a way to send log messages
/// through a channel, allowing for asynchronous logging.
pub struct TeraLogger {
    /// The sender half of a channel for log messages.
    sender: other_mpsc::Sender<String>,
}

impl log::Log for TeraLogger {
    /// Checks if a log message with the given metadata should be recorded.
    ///
    /// This method filters log messages based on the target and log level.
    ///
    /// # Arguments
    ///
    /// * `metadata` - The metadata associated with the log record.
    ///
    /// # Returns
    ///
    /// `true` if the log message should be recorded, `false` otherwise.
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.target().starts_with("teralib") && metadata.level() <= Level::Info
    }

    /// Records a log message.
    ///
    /// If the log message is enabled based on its metadata, this method formats the message
    /// and sends it through the channel.
    ///
    /// # Arguments
    ///
    /// * `record` - The log record to be processed.
    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let log_message = format!("{} - {}", record.level(), record.args());
            let _ = self.sender.try_send(log_message);
        }
    }

    /// Flushes any buffered records.
    ///
    /// This implementation does nothing as there is no buffering.
    fn flush(&self) {}
}

/// Sets up logging for the application.
///
/// This function initializes the global logger with an Info level filter.
/// It uses a lazy initialization pattern to ensure the logger is only set up once.
pub fn setup_logging() -> (TeraLogger, other_mpsc::Receiver<String>) {
    let (sender, receiver) = other_mpsc::channel(100);
    (TeraLogger { sender }, receiver)
}

/// Runs the game with the provided credentials and language.
///
/// This function sets the credentials, checks if the game is already running,
/// and launches the game asynchronously.
///
/// # Arguments
///
/// * `account_name` - The account name as a &str.
/// * `ticket` - The session ticket as a &str.
/// * `game_lang` - The game language as a &str.
///
/// # Returns
///
/// A Result containing the exit status of the game process or an error.
pub async fn run_game(
    account_name: &str,
    characters_count: &str,
    ticket: &str,
    game_lang: &str,
    game_path: &str,
) -> Result<ExitStatus, Box<dyn std::error::Error>> {
    info!("Starting run_game function");

    if is_game_running() {
        return Err("Game is already running".into());
    }

    set_credentials(account_name, characters_count, ticket, game_lang, game_path);

    info!(
        "Set credentials - Account: {}, Characters_count: {}, Ticket: {}, Lang: {}, Game Path: {}",
        GLOBAL_CREDENTIALS.get_account_name(),
        GLOBAL_CREDENTIALS.get_characters_count(),
        GLOBAL_CREDENTIALS.get_ticket(),
        GLOBAL_CREDENTIALS.get_game_lang(),
        GLOBAL_CREDENTIALS.get_game_path()
    );

    launch_game().await
}

/// Launches the game and handles the game process lifecycle.
///
/// This function spawns the game process, manages the game window, and handles
/// server list requests asynchronously.
///
/// # Returns
///
/// A Result containing the exit status of the game process or an error.
async fn launch_game() -> Result<ExitStatus, Box<dyn std::error::Error>> {
    if GAME_RUNNING.load(Ordering::SeqCst) {
        return Err("Game is already running".into());
    }

    GAME_RUNNING.store(true, Ordering::SeqCst);
    GAME_STATUS_SENDER.send(true).unwrap();
    info!("Game status set to running");

    info!(
        "Launching game for account: {}",
        GLOBAL_CREDENTIALS.get_account_name()
    );

    let (tx, rx) = mpsc::channel::<(WPARAM, usize)>();
    *SERVER_LIST_SENDER.lock().unwrap() = Some(tx);

    let tcs = Arc::new(tokio::sync::Notify::new());
    let tcs_clone = Arc::clone(&tcs);

    let handle =
        tokio::task::spawn_blocking(move || unsafe { create_and_run_game_window(tcs_clone) });

    tokio::spawn(async move {
        while let Ok((w_param, sender)) = rx.recv() {
            unsafe {
                handle_server_list_request(w_param, sender);
            }
        }
    });

    tcs.notified().await;

    // Pre-fetch the server list BEFORE we spawn TERA.exe. The IPC handler that
    // services event 5 runs on the Win32 message-pump thread; doing the HTTP
    // fetch from there used to spawn a fresh tokio Runtime inside a
    // SendMessage callback, which panics ("Cannot start a runtime from within
    // a runtime") and silently leaves the launcher unable to answer the
    // server list request — TERA then sits forever on a blank dropdown.
    info!("Pre-fetching server list before game launch...");
    match prefetch_server_list().await {
        Ok(bytes) => {
            info!("Server list pre-fetched: {} bytes", bytes.len());
            *SERVER_LIST_CACHE.lock().unwrap() = Some(bytes);
        }
        Err(e) => {
            error!(
                "Server list pre-fetch failed: {}. Will fall back to live fetch on demand.",
                e
            );
            *SERVER_LIST_CACHE.lock().unwrap() = None;
        }
    }

    // --- Robust spawn ---
    // 1. Set CWD to the Binaries folder so TERA finds its DLLs (PhysX, GFx, dxvk, etc.)
    //    This is CRITICAL: without it, users who launch from a shortcut without a
    //    "Start in" path get a silent crash (DLL load failure).
    // 2. Capture stdout/stderr to a file so we can diagnose crashes after the fact.
    let game_path_str = GLOBAL_CREDENTIALS.get_game_path();
    let game_path = std::path::Path::new(&game_path_str);
    let work_dir = game_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));

    // Log file lives next to TERA.exe so it's easy to find.
    let log_path = work_dir.join("tera_launch.log");
    info!(
        "Spawning game: exe={:?} cwd={:?} log={:?} lang={}",
        game_path,
        work_dir,
        log_path,
        GLOBAL_CREDENTIALS.get_game_lang()
    );

    let log_stdout = std::fs::File::create(&log_path).ok();
    let log_stderr = log_stdout.as_ref().and_then(|f| f.try_clone().ok());

    let mut cmd = Command::new(GLOBAL_CREDENTIALS.get_game_path());
    cmd.arg(format!(
        "-LANGUAGEEXT={}",
        GLOBAL_CREDENTIALS.get_game_lang()
    ))
    .current_dir(&work_dir);

    if let (Some(out), Some(err)) = (log_stdout, log_stderr) {
        cmd.stdout(out).stderr(err);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            error!(
                "Failed to spawn TERA.exe at {:?} (cwd={:?}): {} (os_error={:?})",
                game_path,
                work_dir,
                e,
                e.raw_os_error()
            );
            GAME_RUNNING.store(false, Ordering::SeqCst);
            GAME_STATUS_SENDER.send(false).unwrap();
            return Err(format!("Failed to launch TERA.exe: {}", e).into());
        }
    };

    let pid = child.id();
    info!("Game process spawned with PID: {}", pid);
    GAME_PID.store(pid, Ordering::SeqCst);

    let status = child.wait()?;
    info!("Game process exited with status: {:?}", status);

    GAME_RUNNING.store(false, Ordering::SeqCst);
    GAME_STATUS_SENDER.send(false).unwrap();
    info!("Game status set to not running");

    if let Ok(handle) = WINDOW_HANDLE.lock() {
        if let Some(safe_hwnd) = *handle {
            let hwnd = safe_hwnd.get();
            unsafe {
                PostMessageW(hwnd, WM_GAME_EXITED, 0, 0);
            }
        } else {
            error!("Window handle not found when trying to post WM_GAME_EXITED message");
        }
    } else {
        error!("Failed to acquire lock on WINDOW_HANDLE");
    }
    handle.await?;

    Ok(status)
}

/// Converts a Rust string slice to a null-terminated wide string (UTF-16).
///
/// This function is useful for interoperability with Windows API functions
/// that expect wide string parameters.
///
/// # Arguments
///
/// * `s` - The input string slice to convert.
///
/// # Returns
///
/// A vector of u16 values representing the wide string, including a null terminator.
fn to_wstring(s: &str) -> Vec<u16> {
    OsStr::new(s).encode_wide().chain(Some(0)).collect()
}

/// Returns a receiver for game status updates.
///
/// This function provides a way to subscribe to game status changes.
///
/// # Returns
///
/// A `watch::Receiver<bool>` that can be used to receive game status updates.
pub fn get_game_status_receiver() -> watch::Receiver<bool> {
    GAME_STATUS_SENDER.subscribe()
}

/// Checks if the game is currently running.
///
/// # Returns
///
/// A boolean indicating whether the game is running (true) or not (false).
pub fn is_game_running() -> bool {
    GAME_RUNNING.load(Ordering::SeqCst)
}

/// Resets the global state of the application.
///
/// This function performs the following actions:
/// 1. Sets the game running status to false.
/// 2. Sends a game status update.
/// 3. Clears the stored window handle.
///
/// It's typically called when cleaning up or restarting the application state.
pub fn reset_global_state() {
    GAME_RUNNING.store(false, Ordering::SeqCst);
    if let Err(e) = GAME_STATUS_SENDER.send(false) {
        error!("Failed to send game status: {:?}", e);
    }
    if let Ok(mut handle) = WINDOW_HANDLE.lock() {
        *handle = None;
    }
    info!("Global state reset completed");
}

/// Window procedure for handling Windows messages.
///
/// This function is called by the Windows operating system to process messages
/// for the application's window.
///
/// # Safety
///
/// This function is unsafe because it deals directly with raw pointers and
/// Windows API calls.
///
/// # Arguments
///
/// * `h_wnd` - The handle to the window.
/// * `msg` - The message identifier.
/// * `w_param` - Additional message information (depends on the message).
/// * `l_param` - Additional message information (depends on the message).
///
/// # Returns
///
/// The result of the message processing.
unsafe extern "system" fn wnd_proc(
    h_wnd: HWND,
    msg: UINT,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_COPYDATA => {
            let copy_data = &*(l_param as *const COPYDATASTRUCT);
            let event_id = copy_data.dwData;

            // The sender's HWND comes through as wParam per the WM_COPYDATA
            // contract. Looking it up to a PID lets us correlate launcher
            // logs with the spawned TERA.exe and detect spoofed/UIPI-blocked
            // messages from other processes.
            let sender_hwnd = w_param as HWND;
            let mut sender_pid: DWORD = 0;
            if !sender_hwnd.is_null() {
                GetWindowThreadProcessId(sender_hwnd, &mut sender_pid);
            }

            let payload = if copy_data.cbData > 0 {
                slice::from_raw_parts(copy_data.lpData as *const u8, copy_data.cbData as usize)
            } else {
                &[]
            };

            info!(
                "WM_COPYDATA received: sender_hwnd={:?} sender_pid={} dwData={} cbData={}",
                sender_hwnd, sender_pid, event_id, copy_data.cbData
            );
            // Only hex-dump small payloads. Event 1000's 520-byte struct dump
            // floods the log every launch and obscures the actual handshake
            // signal.
            if payload.len() <= 64 {
                let hex_payload: Vec<String> =
                    payload.iter().map(|b| format!("{:02X}", b)).collect();
                info!("  payload (hex): {}", hex_payload.join(" "));
            } else {
                info!("  payload: {} bytes (truncated)", payload.len());
            }

            match event_id {
                1 => handle_account_name_request(w_param, h_wnd),
                3 => handle_session_ticket_request(w_param, h_wnd),
                5 => handle_server_list_request(w_param, h_wnd as usize),
                7 => handle_enter_lobby_or_world(w_param, h_wnd, payload),
                1000 => handle_game_start(w_param, h_wnd, payload),
                1001..=1016 => handle_game_event(w_param, h_wnd, event_id, payload),
                1020 => handle_game_exit(w_param, h_wnd, payload),
                1021 => handle_game_crash(w_param, h_wnd, payload),
                _ => {
                    info!("Unhandled event ID: {}", event_id);
                }
            }
            1
        }
        WM_GAME_EXITED => {
            info!("Received WM_GAME_EXITED in wnd_proc");
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h_wnd, msg, w_param, l_param),
    }
}

/// Creates and runs the game window.
///
/// This function sets up the window class, creates the window, and enters
/// the message loop for processing window messages. It also handles cleanup
/// when the window is closed.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `tcs` - An `Arc<Notify>` used to signal when the window has been created.
unsafe fn create_and_run_game_window(tcs: Arc<Notify>) {
    let launcher_class_name = "LAUNCHER_CLASS";
    let launcher_window_title = "LAUNCHER_WINDOW";
    let class_name = to_wstring(launcher_class_name);
    let window_name = to_wstring(launcher_window_title);
    let wnd_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: GetModuleHandleW(null_mut()),
        hIcon: null_mut(),
        hCursor: null_mut(),
        hbrBackground: null_mut(),
        lpszMenuName: null_mut(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: null_mut(),
    };

    let atom = RegisterClassExW(&wnd_class);
    if atom == 0 {
        error!("Failed to register window class");
        return;
    }

    let hwnd = CreateWindowExW(
        0,
        class_name.as_ptr(),
        window_name.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        null_mut(),
        null_mut(),
        GetModuleHandleW(null_mut()),
        null_mut(),
    );

    if hwnd.is_null() {
        error!("Failed to create window");
        UnregisterClassW(class_name.as_ptr(), GetModuleHandleW(null_mut()));
        return;
    }

    info!("Window created with HWND: {:?}", hwnd);

    // Allow WM_COPYDATA to be received from processes running at a different
    // (typically lower) integrity level. Without this, Windows UIPI silently
    // drops the messages the game sends to the launcher window when the
    // launcher is elevated and Tera.exe is not (or vice versa), and the
    // client gets stuck on the "Fate of Arun" splash waiting for the account
    // name / ticket / server list responses.
    {
        type ChangeWindowMessageFilterExFn = unsafe extern "system" fn(
            HWND,
            UINT,
            u32,
            *mut winapi::um::winuser::CHANGEFILTERSTRUCT,
        ) -> BOOL;
        const MSGFLT_ALLOW: u32 = 1;
        let user32 = winapi::um::libloaderapi::LoadLibraryA(
            b"user32.dll\0".as_ptr() as *const i8,
        );
        if !user32.is_null() {
            let proc_addr = winapi::um::libloaderapi::GetProcAddress(
                user32,
                b"ChangeWindowMessageFilterEx\0".as_ptr() as *const i8,
            );
            if !proc_addr.is_null() {
                let func: ChangeWindowMessageFilterExFn = std::mem::transmute(proc_addr);
                if func(hwnd, WM_COPYDATA, MSGFLT_ALLOW, null_mut()) == 0 {
                    let err = GetLastError();
                    error!(
                        "ChangeWindowMessageFilterEx(WM_COPYDATA) failed, error code: {}",
                        err
                    );
                } else {
                    info!("UIPI filter set: WM_COPYDATA allowed for launcher window");
                }
            } else {
                info!("ChangeWindowMessageFilterEx not available on this Windows version");
            }
        }
    }

    if let Ok(mut handle) = WINDOW_HANDLE.lock() {
        handle.replace(SafeHWND::new(hwnd));
    } else {
        error!("Failed to acquire lock on WINDOW_HANDLE");
    }

    tcs.notify_one();

    let mut msg = std::mem::zeroed();
    info!("Entering message loop");
    while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
        if msg.message == WM_GAME_EXITED {
            info!("Received WM_GAME_EXITED message");
            break;
        }
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    info!("Exiting message loop");

    DestroyWindow(hwnd);
    UnregisterClassW(class_name.as_ptr(), GetModuleHandleW(null_mut()));

    reset_global_state();

    let mut wcex: WNDCLASSEXW = std::mem::zeroed();
    wcex.cbSize = std::mem::size_of::<WNDCLASSEXW>() as u32;

    EnumWindows(Some(enum_window_proc), class_name.as_ptr() as LPARAM);

    if GetClassInfoExW(GetModuleHandleW(null_mut()), class_name.as_ptr(), &mut wcex) != 0 {
        if UnregisterClassW(class_name.as_ptr(), GetModuleHandleW(null_mut())) == 0 {
            let error = GetLastError();
            error!("Failed to unregister class. Error code: {}", error);
        } else {
            info!("Tera ClassName Unregistered successfully");
        }
    } else {
        info!("Tera ClassName does not exist or is already unregistered");
    }
}

/// Callback function for enumerating windows.
///
/// This function is called for each top-level window on the screen.
/// It checks if the window's class name matches the given class name,
/// and if so, destroys the window.
///
/// # Safety
///
/// This function is unsafe because it deals with raw window handles and
/// destroys windows, which can have system-wide effects.
///
/// # Arguments
///
/// * `hwnd` - Handle to a top-level window.
/// * `lparam` - Application-defined value given in EnumWindows.
///
/// # Returns
///
/// Returns TRUE to continue enumeration, FALSE to stop.
unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let mut class_name: [u16; 256] = [0; 256];
    let len = GetClassNameW(hwnd, class_name.as_mut_ptr(), 256) as usize;
    let class_name = &class_name[..len];

    let search_class = slice::from_raw_parts(lparam as *const u16, 256);
    let search_len = search_class.iter().position(|&c| c == 0).unwrap_or(256);
    let search_class = &search_class[..search_len];

    if class_name.starts_with(search_class) {
        DestroyWindow(hwnd);
    }
    TRUE
}

/// Sends a response message to a specified recipient.
///
/// This function constructs a COPYDATASTRUCT and sends it using the SendMessageW Windows API function.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `recipient` - The HWND of the recipient window as a WPARAM.
/// * `sender` - The sender's window handle as a HWND.
/// * `game_event` - The event identifier as a usize.
/// * `payload` - The data payload to be sent as a slice of bytes.
unsafe fn send_response_message(
    recipient: WPARAM,
    sender: HWND,
    game_event: usize,
    payload: &[u8],
) {
    info!(
        "Sending response message - Event: {}, Payload length: {}",
        game_event,
        payload.len()
    );
    let copy_data = COPYDATASTRUCT {
        dwData: game_event,
        cbData: payload.len() as u32,
        lpData: payload.as_ptr() as *mut _,
    };
    let result = SendMessageW(
        recipient as HWND,
        WM_COPYDATA,
        sender as WPARAM,
        &copy_data as *const _ as LPARAM,
    );
    info!("SendMessageW result: {}", result);
}

/// Handles the account name request from the game client.
///
/// This function retrieves the account name and sends it back to the game client.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `recipient` - The HWND of the recipient window as a WPARAM.
/// * `sender` - The sender's window handle as a HWND.
unsafe fn handle_account_name_request(recipient: WPARAM, sender: HWND) {
    let account_name = GLOBAL_CREDENTIALS.get_account_name();
    info!("Account Name Request - Sending: {}", account_name);
    let account_name_utf16: Vec<u8> = account_name
        .encode_utf16()
        .flat_map(|c| c.to_le_bytes().to_vec())
        .collect();
    send_response_message(recipient, sender, 2, &account_name_utf16);
}

/// Handles the session ticket request from the game client.
///
/// This function retrieves the session ticket and sends it back to the game client.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `recipient` - The HWND of the recipient window as a WPARAM.
/// * `sender` - The sender's window handle as a HWND.
unsafe fn handle_session_ticket_request(recipient: WPARAM, sender: HWND) {
    let session_ticket = GLOBAL_CREDENTIALS.get_ticket();
    info!("Session Ticket Request - Sending: {}", session_ticket);
    send_response_message(recipient, sender, 4, session_ticket.as_bytes());
}

/// Handles the server list request from the game client.
///
/// This function retrieves the server list asynchronously and sends it back to the game client.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `recipient` - The HWND of the recipient window as a WPARAM.
/// * `sender` - The sender's window handle as a usize.
unsafe fn handle_server_list_request(recipient: WPARAM, sender: usize) {
    // Fast path: serve the bytes pre-fetched in launch_game(). This is the
    // common case once the user clicks PLAY.
    if let Some(bytes) = SERVER_LIST_CACHE.lock().unwrap().clone() {
        info!(
            "handle_server_list_request: serving from pre-fetch cache ({} bytes)",
            bytes.len()
        );
        send_response_message(recipient, sender as HWND, 6, &bytes);
        return;
    }

    // Fallback: pre-fetch failed earlier (network was down) but the user is
    // here anyway, so try once more synchronously. We MUST NOT spin up a new
    // tokio Runtime here — wnd_proc runs on the Win32 message-pump thread,
    // and `Runtime::new()` inside a SendMessage handler panics ("Cannot start
    // a runtime from within a runtime"). Use a dedicated current-thread
    // runtime on a worker thread instead.
    info!("handle_server_list_request: cache empty, attempting on-demand fetch...");
    let result = std::thread::spawn(|| {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        rt.block_on(async { prefetch_server_list().await.ok() })
    })
    .join();

    match result {
        Ok(Some(bytes)) => {
            info!(
                "handle_server_list_request: on-demand fetch ok ({} bytes)",
                bytes.len()
            );
            *SERVER_LIST_CACHE.lock().unwrap() = Some(bytes.clone());
            send_response_message(recipient, sender as HWND, 6, &bytes);
        }
        _ => {
            error!(
                "handle_server_list_request: failed to obtain server list, sending empty payload"
            );
            send_response_message(recipient, sender as HWND, 6, &[]);
        }
    }
}

/// Handles the event of entering a lobby or world.
///
/// This function processes the payload to determine if the player is entering a lobby or a specific world,
/// and sends an appropriate response.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers and Windows API calls.
///
/// # Arguments
///
/// * `recipient` - The HWND of the recipient window as a WPARAM.
/// * `sender` - The HWND of the sender window.
/// * `payload` - The payload containing world information, if any.
unsafe fn handle_enter_lobby_or_world(recipient: WPARAM, sender: HWND, payload: &[u8]) {
    if payload.is_empty() {
        on_lobby_entered();
        send_response_message(recipient, sender, 8, &[]);
    } else {
        let world_name = String::from_utf8_lossy(payload);
        on_world_entered(&world_name);
        send_response_message(recipient, sender, 8, payload);
    }
}

/// Handles TERA's game-start notification (event 0x3e8 / 1000).
///
/// The classic launcher does not answer this notification. TERA asks for the
/// account name, ticket, and server list with separate events 0x1, 0x3, and
/// 0x5, which are handled independently.
unsafe fn handle_game_start(_recipient: WPARAM, _sender: HWND, payload: &[u8]) {
    info!("Game start notification (event 1000), payload {} bytes", payload.len());
}

/// Handles various game events.
///
/// This function is called for various game events identified by the event_id.
/// Currently, it only logs the event.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers, but it doesn't perform any unsafe operations.
///
/// # Arguments
///
/// * `_recipient` - The HWND of the recipient window as a WPARAM (unused).
/// * `_sender` - The HWND of the sender window (unused).
/// * `event_id` - The identifier of the game event.
/// * `_payload` - The payload associated with the game event (unused).
unsafe fn handle_game_event(_recipient: WPARAM, _sender: HWND, event_id: usize, _payload: &[u8]) {
    info!("Game event {} received", event_id);
}

/// Handles the game exit event.
///
/// This function is called when the game exits normally. Currently, it only logs the event.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers, but it doesn't perform any unsafe operations.
///
/// # Arguments
///
/// * `_recipient` - The HWND of the recipient window as a WPARAM (unused).
/// * `_sender` - The HWND of the sender window (unused).
/// * `_payload` - The payload associated with the game exit event (unused).
unsafe fn handle_game_exit(_recipient: WPARAM, _sender: HWND, _payload: &[u8]) {
    info!("Game ended");
}

/// Handles the game crash event.
///
/// This function is called when the game crashes. Currently, it only logs the event as an error.
///
/// # Safety
///
/// This function is unsafe due to its use of raw pointers, but it doesn't perform any unsafe operations.
///
/// # Arguments
///
/// * `_recipient` - The HWND of the recipient window as a WPARAM (unused).
/// * `_sender` - The HWND of the sender window (unused).
/// * `_payload` - The payload associated with the game crash event (unused).
unsafe fn handle_game_crash(_recipient: WPARAM, _sender: HWND, _payload: &[u8]) {
    error!("Game crash detected");
}

/// Logs the event of entering the lobby.
fn on_lobby_entered() {
    info!("Entered the lobby");
}

/// Logs the event of entering a world.
///
/// # Arguments
///
/// * `world_name` - The name of the world being entered.
fn on_world_entered(world_name: &str) {
    info!("Entered the world: {}", world_name);
}

// `get_server_list` was the original JSON-only fetcher; it has been
// superseded by `prefetch_server_list`, which auto-detects XML vs JSON and
// supports both the retail XML wire format and the tera-api JSON flavor.

#[derive(Debug, Deserialize)]
struct XmlServerList {
    #[serde(rename = "server", default)]
    servers: Vec<XmlServer>,
}

#[derive(Debug, Deserialize)]
struct XmlServer {
    id: u32,
    ip: String,
    port: u32,
    category: XmlText,
    name: XmlName,
    crowdness: XmlText,
    open: XmlText,
    #[serde(default)]
    popup: String,
}

#[derive(Debug, Deserialize)]
struct XmlText {
    #[serde(rename = "$text", default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct XmlName {
    #[serde(rename = "@raw_name", default)]
    raw_name: String,
    #[serde(rename = "$text", default)]
    text: String,
}

/// Parses JSON into ServerList struct.
///
/// Converts server list JSON to ServerList with error checking.
///
/// # Arguments
///
/// * `json` - Reference to serde_json::Value with server list data.
///
/// # Returns
///
/// Result<ServerList, Box<dyn std::error::Error>>:
/// - Ok(ServerList): Populated ServerList struct
/// - Err: Parsing error description
fn parse_server_list_json(
    json: &Value,
) -> Result<ServerList, Box<dyn std::error::Error + Send + Sync>> {
    let (player_last_server_id, character_counts) = parse_character_counts();

    info!(
        "Parsed values - Last server ID: {}, Character counts: {:?}",
        player_last_server_id, character_counts
    );

    let servers = json["servers"]
        .as_array()
        .ok_or("No servers found in JSON")?;
    let mut server_list = ServerList {
        servers: Vec::with_capacity(servers.len()),
        last_server_id: 0,
        sort_criterion: 0,
    };

    for server in servers {
        let server_id = server["id"]
            .as_u64()
            .ok_or("Missing or invalid 'id' field")? as u32;
        let character_count = character_counts.get(&server_id).cloned().unwrap_or(0);

        let json_available = server["available"].as_u64().unwrap_or(0);

        info!(
            "Processing server: id={}, name={}, json_available={}",
            server_id, server["name"], json_available
        );

        let display_count = format!("({})", character_count);
        let name = server["name"]
            .as_str()
            .ok_or("Missing or invalid 'name' field")?
            .to_string();
        let title = format!(
            "{} {}",
            server["title"]
                .as_str()
                .ok_or("Missing or invalid 'title' field")?,
            display_count
        );

        info!("Formatted server name: {}", name);

        // Modify population field based on 'available' in JSON
        let population = if json_available == 0 {
            "<b><font color=\"#FF0000\">Offline</font></b>".to_string()
        } else {
            server["population"]
                .as_str()
                .ok_or("Missing or invalid 'population' field")?
                .to_string()
        };

        // Handle address and host fields.
        // If 'address' is present but is not a valid IPv4 literal (e.g. a hostname),
        // fall back to using it as 'host' so the client resolves it instead of
        // connecting to 0.0.0.0.
        let address_str = server["address"].as_str();
        let host_str = server["host"].as_str();

        let parse_addr_or_host = |addr: &str| -> (u32, Vec<u8>) {
            match addr.parse::<std::net::Ipv4Addr>() {
                Ok(ip) => (u32::from_be_bytes(ip.octets()), Vec::new()),
                Err(_) => (0, utf16_to_bytes(addr)),
            }
        };

        let (address, host) = match (address_str, host_str) {
            (Some(addr), _) => parse_addr_or_host(addr),
            (None, Some(h)) => {
                // If host happens to be a literal IPv4, use the address field too.
                match h.parse::<std::net::Ipv4Addr>() {
                    Ok(ip) => (u32::from_be_bytes(ip.octets()), Vec::new()),
                    Err(_) => (0, utf16_to_bytes(h)),
                }
            }
            (None, None) => return Err("Either 'address' or 'host' must be set".into()),
        };

        let server_info = ServerInfo {
            id: server_id,
            name: utf16_to_bytes(&name),
            category: utf16_to_bytes(
                server["category"]
                    .as_str()
                    .ok_or("Missing or invalid 'category' field")?,
            ),
            title: utf16_to_bytes(&title),
            queue: utf16_to_bytes(
                server["queue"]
                    .as_str()
                    .ok_or("Missing or invalid 'queue' field")?,
            ),
            population: utf16_to_bytes(&population),
            address,
            port: server["port"]
                .as_u64()
                .ok_or("Missing or invalid 'port' field")? as u32,
            available: json_available as u32,
            unavailable_message: utf16_to_bytes(
                server["unavailable_message"].as_str().unwrap_or(""),
            ),
            host: if host.is_empty() { None } else { Some(host) },
        };
        server_list.servers.push(server_info);
    }

    server_list.last_server_id = if player_last_server_id == 0 {
        server_list.servers.first().map(|s| s.id).unwrap_or(0)
    } else {
        player_last_server_id
    };
    server_list.sort_criterion = json["sort_criterion"].as_u64().unwrap_or(0) as u32;

    Ok(server_list)
}

fn parse_server_list_xml(
    xml: &str,
) -> Result<ServerList, Box<dyn std::error::Error + Send + Sync>> {
    let xml: XmlServerList = xml_from_str(xml)?;
    if xml.servers.is_empty() {
        return Err("No servers found in XML".into());
    }

    let (player_last_server_id, character_counts) = parse_character_counts();
    let mut server_list = ServerList {
        servers: Vec::with_capacity(xml.servers.len()),
        last_server_id: 0,
        sort_criterion: 0,
    };

    for server in xml.servers {
        let character_count = character_counts.get(&server.id).cloned().unwrap_or(0);
        let base_name = if server.name.raw_name.trim().is_empty() {
            server.name.text.trim()
        } else {
            server.name.raw_name.trim()
        };
        let display_count = format!("({})", character_count);
        let title = format!("{} {}", base_name, display_count);
        let address = ipv4_to_u32(&server.ip);
        let available = if server.open.text.trim().is_empty() || address == 0 {
            0
        } else {
            1
        };

        info!(
            "XML server id={} name='{}' ip={} port={} chars={} available={}",
            server.id, base_name, server.ip, server.port, character_count, available
        );

        server_list.servers.push(ServerInfo {
            id: server.id,
            name: utf16_to_bytes(base_name),
            category: utf16_to_bytes(server.category.text.trim()),
            title: utf16_to_bytes(&title),
            queue: utf16_to_bytes(server.crowdness.text.trim()),
            population: utf16_to_bytes(server.open.text.trim()),
            address,
            port: server.port,
            available,
            unavailable_message: utf16_to_bytes(server.popup.trim()),
            host: None,
        });
    }

    server_list.last_server_id = if player_last_server_id == 0 {
        server_list.servers.first().map(|s| s.id).unwrap_or(0)
    } else {
        player_last_server_id
    };

    Ok(server_list)
}

fn parse_character_counts() -> (u32, std::collections::HashMap<u32, u32>) {
    let credentials = GLOBAL_CREDENTIALS.get_characters_count();
    info!("Raw credentials string: {}", credentials);

    // Format from Portal API: "lastLoginServer|serverId,charCount|serverId,charCount|"
    let parts: Vec<&str> = credentials.split('|').collect();
    let player_last_server_id = parts
        .first()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);

    let character_counts = parts
        .iter()
        .skip(1)
        .filter_map(|entry| {
            if entry.is_empty() {
                return None;
            }
            let mut it = entry.split(',');
            let id = it.next()?.trim().parse::<u32>().ok()?;
            let count = it.next()?.trim().parse::<u32>().ok()?;
            Some((id, count))
        })
        .collect();

    (player_last_server_id, character_counts)
}

/// Converts a Rust string to UTF-16 little-endian bytes.
///
/// This function is useful for preparing strings for Windows API calls that expect UTF-16.
///
/// # Arguments
///
/// * `s` - A string slice that holds the text to be converted.
///
/// # Returns
///
/// A vector of bytes representing the UTF-16 little-endian encoded string.
fn utf16_to_bytes(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .flat_map(|c| c.to_le_bytes().to_vec())
        .collect()
}

/// Resolve the server-list endpoint, parse either the retail XML format or the
/// tera-api JSON format, then return the protobuf bytes expected by TERA for
/// WM_COPYDATA event 6.
async fn prefetch_server_list() -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let url = config::get_config_value("SERVER_LIST_URL");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()?;
    let mut last_err: Option<String> = None;
    for attempt in 1..=3u32 {
        info!(
            "Fetching server list (attempt {}/3) from {}",
            attempt, url
        );
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let content_type = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let body = resp.text().await?;
                let first_non_ws = body.chars().find(|c| !c.is_whitespace()).unwrap_or('\0');
                let server_list = if content_type.contains("json") || first_non_ws == '{' {
                    let json: Value = serde_json::from_str(&body)?;
                    parse_server_list_json(&json)?
                } else {
                    parse_server_list_xml(&body)?
                };
                let payload = server_list.encode_to_vec();
                info!(
                    "Server list parsed (content-type='{}', source {} bytes, servers={}, last_server_id={}, sort_criterion={}); event 6 protobuf {} bytes",
                    content_type,
                    body.len(),
                    server_list.servers.len(),
                    server_list.last_server_id,
                    server_list.sort_criterion,
                    payload.len()
                );
                return Ok(payload);
            }
            Ok(resp) => {
                last_err = Some(format!("HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("transport: {}", e));
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    Err(last_err
        .unwrap_or_else(|| "unknown server list fetch error".into())
        .into())
}

/// Converts an IPv4 address string to a u32 representation.
///
/// # Arguments
///
/// * `ip` - A string slice that holds the IPv4 address.
///
/// # Returns
///
/// A u32 representation of the IP address, or 0 if parsing fails.
#[allow(dead_code)]
fn ipv4_to_u32(ip: &str) -> u32 {
    ip.parse::<std::net::Ipv4Addr>()
        .map(|addr| u32::from_be_bytes(addr.octets()))
        .unwrap_or(0)
}
