//! The Windows shell around the tray's logic: a message window, the tray icon, and the OS
//! calls behind each menu action. What the menu says and when lives in `novalink_tray::model`.

mod agent;
mod icon;
mod menu;
mod poll;
mod prompt;
mod system;

use agent::Agent;
use muda::MenuEvent;
use novalink_client::{Client, ClientError};
use novalink_tray::model::{
    ActionId, IconState, Item, PromptTracker, UiState, error_text, icon_state, menu as model_menu,
    os_failure_text, start_failed_text, turn_off_failure,
};
use novalink_tray::supervisor::{TRAY_RETRY_EVERY, TRAY_RETRY_FOR};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tray_icon::{MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, GetCurrentThreadId, INFINITE, WaitForSingleObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GUI_INMENUMODE, GUITHREADINFO,
    GetGUIThreadInfo, GetMessageW, HWND_MESSAGE, MSG, PostMessageW, PostQuitMessage,
    RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WNDCLASSW,
};
use windows::core::PCWSTR;

/// A new `UiState` is waiting from the poller.
const WM_STATE: u32 = WM_APP + 1;
/// A menu item was chosen.
const WM_MENU: u32 = WM_APP + 2;
/// Setup (or anything else) asked the tray to exit.
const WM_QUIT_REQUEST: u32 = WM_APP + 3;

/// The message window, stored as an integer so other threads can post to it.
static WINDOW: AtomicIsize = AtomicIsize::new(0);

fn post(msg: u32) {
    let raw = WINDOW.load(Ordering::SeqCst);
    if raw != 0 {
        // SAFETY: posting to a window handle that is either valid or gone is harmless.
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut c_void)), msg, WPARAM(0), LPARAM(0));
        }
    }
}

struct App {
    tray: TrayIcon,
    states: Receiver<UiState>,
    menu_events: Receiver<MenuEvent>,
    /// Menu ids to actions for the last few menus: a click can land just after a rebuild.
    actions: VecDeque<menu::Actions>,
    items: Vec<Item>,
    icon: IconState,
    state: UiState,
    tracker: PromptTracker,
    live: prompt::Live,
    agent: Arc<Agent>,
    poll: Sender<bool>,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Tells the user why the tray can't run; there is no icon to carry the message.
fn fatal(what: &str, reason: &str) {
    system::info("NovaLink", &os_failure_text(what, reason));
}

pub fn run() {
    // SAFETY: plain Win32 calls with NUL-terminated names.
    let (_instance, quit_event) = unsafe {
        let name = system::wide(r"Local\NovaLinkTray");
        let mutex = match CreateMutexW(None, true, PCWSTR(name.as_ptr())) {
            Ok(m) => m,
            Err(e) => return fatal("start the tray", &e.message()),
        };
        // Already running: that copy keeps the icon.
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        let quit = system::wide(r"Local\NovaLinkTrayQuit");
        let event = match CreateEventW(None, false, false, PCWSTR(quit.as_ptr())) {
            Ok(e) => e,
            Err(e) => return fatal("start the tray", &e.message()),
        };
        (mutex, event)
    };

    let Some(hwnd) = create_window() else {
        return fatal(
            "start the tray",
            "Windows wouldn't create its message window",
        );
    };
    WINDOW.store(hwnd.0 as isize, Ordering::SeqCst);

    let quit_raw = quit_event.0 as isize;
    std::thread::spawn(move || {
        // SAFETY: the event handle lives as long as the process.
        let r = unsafe {
            WaitForSingleObject(
                windows::Win32::Foundation::HANDLE(quit_raw as *mut c_void),
                INFINITE,
            )
        };
        if r == windows::Win32::Foundation::WAIT_OBJECT_0 {
            post(WM_QUIT_REQUEST);
        }
    });

    let off = system::is_off();
    let agent = Agent::start(off);
    let (poll_tx, poll_rx) = channel();
    let (state_tx, state_rx) = channel();
    poll::start(agent.clone(), poll_rx, state_tx, || post(WM_STATE));

    let (menu_tx, menu_rx) = channel();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_tx.send(e);
        post(WM_MENU);
    }));
    let doctor_tx = poll_tx.clone();
    TrayIconEvent::set_event_handler(Some(move |e| {
        // Refresh the doctor checks as the menu opens.
        if let TrayIconEvent::Click {
            button_state: MouseButtonState::Down,
            ..
        } = e
        {
            let _ = doctor_tx.send(true);
        }
    }));

    let state = if off {
        UiState::Off
    } else {
        UiState::AgentDown { restarting: true }
    };
    let items = model_menu(&state);
    let icon_kind = icon_state(&state);
    // At login the taskbar may not be ready yet, so keep trying for a while.
    let started = Instant::now();
    let (tray, actions) = loop {
        // Each attempt gets its own menu; the action map must come from the one shown.
        let (menu, actions) = menu::render(&items);
        let built = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon::icon(icon_kind))
            .with_tooltip("NovaLink")
            .build();
        match built {
            Ok(t) => break (t, actions),
            Err(e) if started.elapsed() >= TRAY_RETRY_FOR => {
                return fatal("show the NovaLink icon", &e.to_string());
            }
            Err(_) => std::thread::sleep(TRAY_RETRY_EVERY),
        }
    };

    APP.with(|a| {
        *a.borrow_mut() = Some(App {
            tray,
            states: state_rx,
            menu_events: menu_rx,
            actions: VecDeque::from([actions]),
            items,
            icon: icon_kind,
            state,
            tracker: PromptTracker::default(),
            live: Arc::new(Mutex::new(HashMap::new())),
            agent,
            poll: poll_tx,
        });
    });

    let mut msg = MSG::default();
    // SAFETY: the standard message loop, on the thread that owns the window and the icon.
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    // Dropping the app removes the icon from the tray.
    APP.with(|a| a.borrow_mut().take());
}

fn create_window() -> Option<HWND> {
    let class = system::wide("NovaLinkTrayWindow");
    // SAFETY: registers a class and creates a message-only window with a valid procedure.
    unsafe {
        let hinst = GetModuleHandleW(None).ok()?;
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )
        .ok()
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_STATE | WM_MENU => {
            APP.with(|a| {
                // Skip a message that arrives while we're already inside the app.
                if let Ok(mut a) = a.try_borrow_mut()
                    && let Some(app) = a.as_mut()
                {
                    if msg == WM_STATE {
                        app.on_states();
                    } else {
                        app.on_menu_events();
                    }
                }
            });
            LRESULT(0)
        }
        WM_QUIT_REQUEST => {
            // SAFETY: ends the message loop on this thread.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        // SAFETY: the default handling for everything else.
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// True while a menu is open on this thread; rebuilding it then would pull it from under
/// the user.
fn in_menu_mode() -> bool {
    let mut info = GUITHREADINFO {
        cbSize: size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: `info` is sized and writable.
    unsafe {
        GetGUIThreadInfo(GetCurrentThreadId(), &mut info).is_ok()
            && info.flags.0 & GUI_INMENUMODE.0 != 0
    }
}

impl App {
    fn on_states(&mut self) {
        let mut latest = None;
        while let Ok(s) = self.states.try_recv() {
            latest = Some(s);
        }
        if let Some(state) = latest {
            self.apply(state);
        }
    }

    fn apply(&mut self, state: UiState) {
        self.update_prompts(&state);
        let kind = icon_state(&state);
        if kind != self.icon {
            let _ = self.tray.set_icon(Some(icon::icon(kind)));
            self.icon = kind;
        }
        let items = model_menu(&state);
        self.state = state;
        if items != self.items && !in_menu_mode() {
            let (menu, actions) = menu::render(&items);
            self.tray.set_menu(Some(Box::new(menu)));
            self.actions.push_back(actions);
            if self.actions.len() > 3 {
                self.actions.pop_front();
            }
            self.items = items;
        }
    }

    fn update_prompts(&mut self, state: &UiState) {
        let pending = match state {
            UiState::Up { pending, .. } => pending.as_slice(),
            _ => &[],
        };
        let (new, gone) = self.tracker.update(pending);
        if let Ok(mut live) = self.live.lock() {
            for id in gone {
                live.remove(&id);
            }
            for p in &new {
                live.insert(p.peer.clone(), p.code.clone());
            }
        }
        for p in new {
            prompt::show(p, self.live.clone());
        }
    }

    fn on_menu_events(&mut self) {
        while let Ok(e) = self.menu_events.try_recv() {
            let action = self
                .actions
                .iter()
                .rev()
                .find_map(|m| m.get(&e.id))
                .cloned();
            if let Some(action) = action {
                self.run(action);
            }
        }
    }

    fn run(&mut self, action: ActionId) {
        match action {
            ActionId::Quit => unsafe { PostQuitMessage(0) },
            ActionId::OpenLogs => {
                std::thread::spawn(|| report_text(system::open_logs()));
            }
            ActionId::StartAtLogin => {
                let poll = self.poll.clone();
                std::thread::spawn(move || {
                    report_text(system::set_start_at_login(!system::start_at_login()));
                    let _ = poll.send(false);
                });
            }
            ActionId::TurnOn => {
                let agent = self.agent.clone();
                let poll = self.poll.clone();
                std::thread::spawn(move || {
                    report_text(system::set_off(false));
                    agent.turn_on();
                    let _ = poll.send(false);
                    // Say so if the agent can't be started at all.
                    for _ in 0..12 {
                        std::thread::sleep(Duration::from_millis(250));
                        if let Some(reason) = agent.spawn_error() {
                            system::info("NovaLink", &start_failed_text(&reason));
                            break;
                        }
                    }
                });
            }
            ActionId::TurnOff => {
                // The marker and flag go first so nothing restarts the agent as it stops.
                let marker = system::set_off(true);
                self.agent.set_off();
                let _ = self.poll.send(false);
                std::thread::spawn(move || {
                    report_text(marker);
                    let stopped = Client::local().and_then(|c| c.shutdown());
                    if let Err(e) = stopped {
                        let still_running = !agent::gone_within(Duration::from_secs(5));
                        report_text(turn_off_failure(&e, still_running).map_or(Ok(()), Err));
                    }
                });
            }
            ActionId::Diagnostics => {
                std::thread::spawn(diagnostics);
            }
            ActionId::Pair(id) => self.call(move |c| c.pair(&id)),
            ActionId::Accept(id) => self.call(move |c| c.answer(&id, true)),
            ActionId::Reject(id) => self.call(move |c| c.answer(&id, false)),
            ActionId::Layout { peer, edge } => {
                let edge = format!("{edge:?}").to_lowercase();
                self.call(move |c| c.layout(&edge, &peer));
            }
            ActionId::RequestPermissions => self.call(|c| c.request_permissions()),
            ActionId::Unpair(id) => {
                let name = match &self.state {
                    UiState::Up { status, .. } => status
                        .peers
                        .iter()
                        .find(|p| p.id == id)
                        .map(|p| p.name.clone()),
                    _ => None,
                }
                .unwrap_or_else(|| "this device".into());
                let poll = self.poll.clone();
                std::thread::spawn(move || {
                    let text = format!("Unpair {name}? You can pair it again later.");
                    if system::ask("NovaLink", &text) {
                        report(Client::local().and_then(|c| c.unpair(&id)));
                        let _ = poll.send(false);
                    }
                });
            }
        }
    }

    /// Runs an agent call off the UI thread, tells the user if it fails and refreshes.
    fn call(&self, f: impl FnOnce(&Client) -> Result<(), ClientError> + Send + 'static) {
        let poll = self.poll.clone();
        std::thread::spawn(move || {
            report(Client::local().and_then(|c| f(&c)));
            let _ = poll.send(false);
        });
    }
}

fn report_text(r: Result<(), String>) {
    if let Err(msg) = r {
        system::info("NovaLink", &msg);
    }
}

fn report(r: Result<(), ClientError>) {
    if let Err(e) = r {
        system::info("NovaLink", &error_text(&e));
    }
}

fn diagnostics() {
    let text = match Client::local().and_then(|c| c.doctor()) {
        Ok(checks) => checks
            .iter()
            .map(|c| {
                if c.ok {
                    format!("✓ {} — {}", c.name, c.detail)
                } else {
                    format!("✗ {} — {}\n    {}", c.name, c.detail, c.advice)
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Err(e) => error_text(&e),
    };
    system::info("NovaLink diagnostics", &text);
}
