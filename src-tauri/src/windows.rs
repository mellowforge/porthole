//! Extra windows and the quit sequence that asks each window in turn.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use tauri::{AppHandle, Emitter, Manager, State, WebviewUrl, WebviewWindowBuilder};

use crate::error::{AppError, AppResult};
use crate::export::ExportJobs;

/// The label of the window `tauri.conf.json` creates at startup.
pub const MAIN_LABEL: &str = "main";
/// Sent to one window at a time with `emit_to`; the UI answers with `quit_continue` or `quit_cancel`.
pub const QUIT_REQUESTED: &str = "quit-requested";

/// The smallest `window-N`, with N >= 2, that isn't already in use.
pub fn next_label(existing: &[String]) -> String {
    (2u32..)
        .map(|n| format!("window-{n}"))
        .find(|label| !existing.contains(label))
        .expect("an unused window label")
}

/// The order in which Quit asks: the focused window first, then `main`, then
/// `window-N` by number, then anything else by name.
pub fn quit_order(focused: Option<&str>, mut labels: Vec<String>) -> Vec<String> {
    fn key(label: &str) -> (u8, u32, &str) {
        if label == MAIN_LABEL {
            return (0, 0, label);
        }
        match label.strip_prefix("window-").and_then(|n| n.parse().ok()) {
            Some(n) => (1, n, label),
            None => (2, 0, label),
        }
    }
    labels.sort_by(|a, b| key(a).cmp(&key(b)));
    if let Some(i) = focused.and_then(|f| labels.iter().position(|l| l == f)) {
        let first = labels.remove(i);
        labels.insert(0, first);
    }
    labels
}

/// What to do next in a quit sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Send `quit-requested` to this window.
    Ask(String),
    /// Every window agreed: quit.
    Exit,
    /// Nothing to do.
    Ignore,
}

/// The windows still to ask during a quit; the head is the one asking now.
/// `None` when no quit is running.
#[derive(Default)]
pub struct QuitSequence {
    inner: Mutex<Option<VecDeque<String>>>,
}

impl QuitSequence {
    fn lock(&self) -> MutexGuard<'_, Option<VecDeque<String>>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The window being asked now, if a quit is running.
    pub fn head(&self) -> Option<String> {
        self.lock().as_ref().and_then(|q| q.front().cloned())
    }

    /// Quit was chosen: starts a quit and returns the first window to ask. While a quit is
    /// already running it returns the window being asked, to ask it again: its page may not
    /// have been listening yet, the event may have been lost, or its answer may have failed.
    /// `None` when there is no window.
    pub fn request(&self, labels: Vec<String>) -> Option<String> {
        let mut inner = self.lock();
        if let Some(queue) = inner.as_ref() {
            return queue.front().cloned();
        }
        let first = labels.first()?.clone();
        *inner = Some(labels.into());
        Some(first)
    }

    /// Starts a quit and returns the first window to ask. `None` when a quit is already
    /// running or there is no window.
    pub fn begin(&self, labels: Vec<String>) -> Option<String> {
        let mut inner = self.lock();
        if inner.is_some() || labels.is_empty() {
            return None;
        }
        let first = labels[0].clone();
        *inner = Some(labels.into());
        Some(first)
    }

    /// The window `from` agreed. Only the window being asked can move the sequence on.
    pub fn advance(&self, from: &str) -> Step {
        let mut inner = self.lock();
        match inner.as_ref().and_then(|q| q.front()) {
            Some(head) if head == from => Self::pop_head(&mut inner),
            _ => Step::Ignore,
        }
    }

    /// A window cancelled: the quit stops and every window stays.
    pub fn cancel(&self) {
        *self.lock() = None;
    }

    /// A window went away. If it was being asked, the next one is; if it was waiting,
    /// it is skipped.
    pub fn window_gone(&self, label: &str) -> Step {
        let mut inner = self.lock();
        let Some(queue) = inner.as_mut() else {
            return Step::Ignore;
        };
        match queue.iter().position(|l| l == label) {
            Some(0) => Self::pop_head(&mut inner),
            Some(i) => {
                queue.remove(i);
                Step::Ignore
            }
            None => Step::Ignore,
        }
    }

    /// Pops the head and returns the next window to ask, or `Exit` (clearing the
    /// sequence) when none is left.
    fn pop_head(inner: &mut Option<VecDeque<String>>) -> Step {
        let Some(queue) = inner.as_mut() else {
            return Step::Ignore;
        };
        queue.pop_front();
        match queue.front() {
            Some(next) => Step::Ask(next.clone()),
            None => {
                *inner = None;
                Step::Exit
            }
        }
    }
}

/// Where `drive` stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Driven {
    /// This window was asked; the quit waits for its answer.
    Asked(String),
    /// No window is left to ask: quit.
    Exit,
    /// Nothing to do.
    Idle,
}

/// Carries out quit steps until a window has been asked or the quit is over. `ask` sends
/// the question to one window; a window it fails for counts as gone, and the next is asked.
pub fn drive(quit: &QuitSequence, mut step: Step, mut ask: impl FnMut(&str) -> Result<(), String>) -> Driven {
    loop {
        match step {
            Step::Ignore => return Driven::Idle,
            Step::Exit => return Driven::Exit,
            Step::Ask(label) => match ask(&label) {
                Ok(()) => return Driven::Asked(label),
                Err(e) => {
                    eprintln!("could not ask {label} about quitting, skipping it: {e}");
                    step = quit.window_gone(&label);
                }
            },
        }
    }
}

/// Brings the window forward (it may be minimized or behind another) and asks it.
fn ask_window(app: &AppHandle, label: &str) -> Result<(), String> {
    let window = app.get_webview_window(label).ok_or_else(|| "the window is gone".to_string())?;
    if let Err(e) = window.unminimize() {
        eprintln!("could not unminimize {label} to ask about quitting: {e}");
    }
    if let Err(e) = window.set_focus() {
        eprintln!("could not focus {label} to ask about quitting: {e}");
    }
    app.emit_to(label, QUIT_REQUESTED, ()).map_err(|e| e.to_string())
}

/// How long quitting waits for cancelled exports to remove their temporary files.
const EXPORT_CLEANUP_LIMIT: Duration = Duration::from_secs(1);

/// Carries out a quit step. A window that can't be asked counts as gone.
pub fn act(app: &AppHandle, step: Step) {
    if drive(&app.state::<QuitSequence>(), step, |label| ask_window(app, label)) == Driven::Exit {
        exit(app);
    }
}

/// Quits. Running exports are cancelled first and get a moment to remove their `.part` files.
fn exit(app: &AppHandle) {
    if app.state::<ExportJobs>().cancel_all() == 0 {
        return app.exit(0);
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if !app.state::<ExportJobs>().wait_idle(EXPORT_CLEANUP_LIMIT).await {
            eprintln!("quitting with exports still cleaning up");
        }
        app.exit(0);
    });
}

/// The menu's Quit: asks the focused window first, then the others in label order.
/// With no window there is nothing to ask, so it quits. Quit during a quit asks the
/// window being asked again.
pub fn start_quit(app: &AppHandle) {
    let windows = app.webview_windows();
    if windows.is_empty() {
        return exit(app);
    }
    let focused = windows.values().find(|w| w.is_focused().unwrap_or(false)).map(|w| w.label().to_string());
    let order = quit_order(focused.as_deref(), windows.into_keys().collect());
    if let Some(label) = app.state::<QuitSequence>().request(order) {
        act(app, Step::Ask(label));
    }
}

/// Per window: whether quitting would ask anything (a prompt is up, unsaved grid edits, an open
/// transaction, a save still running, or tab state not yet written). Each window's UI reports it;
/// a window that hasn't reported counts as asking. Lets a system quit (Dock ▸ Quit, logout) with
/// nothing to ask end the app at once instead of interrupting it.
#[derive(Default)]
pub struct QuitPrompts {
    asks: Mutex<HashMap<String, bool>>,
}

impl QuitPrompts {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, bool>> {
        self.asks.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn set(&self, label: &str, asks: bool) {
        self.lock().insert(label.to_string(), asks);
    }

    /// A window that is gone asks nothing.
    pub fn forget(&self, label: &str) {
        self.lock().remove(label);
    }

    /// True when none of `labels` would ask anything. Unreported windows count as asking.
    pub fn none_ask(&self, labels: &[String]) -> bool {
        let asks = self.lock();
        labels.iter().all(|l| asks.get(l) == Some(&false))
    }
}

#[tauri::command]
pub fn report_quit_prompt(window: tauri::Window, prompts: State<'_, QuitPrompts>, asks: bool) {
    prompts.set(window.label(), asks);
}

/// Whether a system quit can end the app now: no window would ask anything and no export is
/// running (an export needs its cleanup).
pub fn quit_needs_no_prompt(app: &AppHandle) -> bool {
    let labels: Vec<String> = app.webview_windows().into_keys().collect();
    app.state::<ExportJobs>().is_idle() && app.state::<QuitPrompts>().none_ask(&labels)
}

/// How far a new window sits from the one it cascades from, in logical pixels.
const CASCADE: f64 = 28.0;

/// Where a new window opens: down and right of the focused window, else of `main`.
/// `None` (neither is known) leaves the position to the system.
pub fn cascade_position(focused: Option<(f64, f64)>, main: Option<(f64, f64)>) -> Option<(f64, f64)> {
    focused.or(main).map(|(x, y)| (x + CASCADE, y + CASCADE))
}

/// The window's outer position in logical pixels, when it can be read.
fn logical_position(window: &tauri::WebviewWindow) -> Option<(f64, f64)> {
    let read = || -> tauri::Result<(f64, f64)> {
        let p = window.outer_position()?.to_logical::<f64>(window.scale_factor()?);
        Ok((p.x, p.y))
    };
    read().map_err(|e| eprintln!("could not read the position of {}: {e}", window.label())).ok()
}

/// Opens a new window like `main`, on the Connections screen.
pub fn open(app: &AppHandle) -> AppResult<()> {
    // Two opens at once must not pick the same label.
    static OPENING: Mutex<()> = Mutex::new(());
    let _guard = OPENING.lock().unwrap_or_else(|e| e.into_inner());

    let mut config = app
        .config()
        .app
        .windows
        .iter()
        .find(|w| w.label == MAIN_LABEL)
        .cloned()
        .ok_or_else(|| AppError::Query("There is no main window in the config.".into()))?;
    let windows = app.webview_windows();
    let focused = windows.values().find(|w| w.is_focused().unwrap_or(false)).and_then(logical_position);
    let main = windows.get(MAIN_LABEL).and_then(logical_position);
    let existing: Vec<String> = windows.into_keys().collect();
    config.label = next_label(&existing);
    config.url = WebviewUrl::default();
    if let Some((x, y)) = cascade_position(focused, main) {
        config.x = Some(x);
        config.y = Some(y);
        config.center = false;
    }
    WebviewWindowBuilder::from_config(app, &config)
        .and_then(|b| b.build())
        .map(|_| ())
        .map_err(|e| AppError::Query(e.to_string()))
}

/// Opens a window off the main thread (building one on it can deadlock), reporting a
/// failure in the window the user is looking at. Used by File ▸ New Window and the Dock.
pub fn open_in_background(app: &AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = open(&app) {
            eprintln!("could not open a window: {e}");
            report_open_failure(&app, &e.to_string());
        }
    });
}

/// Whether an exit request is the last window closing, which on macOS leaves the app
/// running (as Mac apps do): Quit and `app.exit` pass a code, a closing window doesn't.
pub fn keeps_running(code: Option<i32>) -> bool {
    cfg!(target_os = "macos") && code.is_none()
}

/// Sent to the focused window (else `main`) when File ▸ New Window from the menu fails;
/// the payload is the error message.
pub const OPEN_WINDOW_FAILED: &str = "open-window-failed";

/// Tells the user that the menu's New Window failed, in the window they are looking at.
pub fn report_open_failure(app: &AppHandle, message: &str) {
    let windows = app.webview_windows();
    let target = windows
        .values()
        .find(|w| w.is_focused().unwrap_or(false))
        .or_else(|| windows.get(MAIN_LABEL))
        .or_else(|| windows.values().next());
    let Some(target) = target else {
        return;
    };
    if let Err(e) = app.emit_to(target.label(), OPEN_WINDOW_FAILED, message) {
        eprintln!("could not report the failed New Window: {e}");
    }
}

/// File ▸ New Window. Async, because building a window from a sync command (on the
/// main thread) can deadlock.
#[tauri::command]
pub async fn open_window(app: AppHandle) -> AppResult<()> {
    open(&app)
}

/// The asking window has nothing (more) to ask: move on to the next window, or quit.
#[tauri::command]
pub fn quit_continue(app: AppHandle, window: tauri::Window, quit: State<'_, QuitSequence>) {
    act(&app, quit.advance(window.label()));
}

/// The user cancelled in the asking window: stop the quit, keep every window.
#[tauri::command]
pub fn quit_cancel(quit: State<'_, QuitSequence>) {
    quit.cancel();
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_system_quit_asks_unless_every_window_reported_nothing_to_ask() {
        let p = QuitPrompts::default();
        let labels = vec!["main".to_string(), "window-2".to_string()];
        // No windows: nothing to ask.
        assert!(p.none_ask(&[]));
        // Unreported windows count as asking.
        assert!(!p.none_ask(&labels));
        p.set("main", false);
        assert!(!p.none_ask(&labels));
        p.set("window-2", false);
        assert!(p.none_ask(&labels));
        p.set("window-2", true);
        assert!(!p.none_ask(&labels));
        // A window that is gone asks nothing.
        p.forget("window-2");
        assert!(p.none_ask(&["main".to_string()]));
    }

    #[test]
    fn the_last_window_closing_keeps_the_app_running_but_quit_exits() {
        assert_eq!(keeps_running(None), cfg!(target_os = "macos"));
        assert!(!keeps_running(Some(0)));
    }

    use super::*;

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn next_label_fills_the_first_gap() {
        assert_eq!(next_label(&labels(&["main"])), "window-2");
        assert_eq!(next_label(&labels(&["main", "window-2", "window-4"])), "window-3");
        assert_eq!(next_label(&labels(&["main", "window-2", "window-3"])), "window-4");
        assert_eq!(next_label(&[]), "window-2");
    }

    #[test]
    fn quit_sequence_asks_each_window_in_turn() {
        let q = QuitSequence::default();
        assert_eq!(q.begin(labels(&["a", "b"])), Some("a".to_string()));
        assert_eq!(q.advance("b"), Step::Ignore);
        assert_eq!(q.advance("a"), Step::Ask("b".into()));
        assert_eq!(q.advance("b"), Step::Exit);
        // Finished: nothing is running any more.
        assert_eq!(q.advance("b"), Step::Ignore);
        assert_eq!(q.begin(labels(&["a"])), Some("a".to_string()));
    }

    #[test]
    fn quit_sequence_ignores_a_second_quit() {
        let q = QuitSequence::default();
        assert_eq!(q.begin(labels(&["a", "b"])), Some("a".to_string()));
        assert_eq!(q.begin(labels(&["b", "a"])), None);
        q.cancel();
        assert_eq!(q.advance("a"), Step::Ignore);
        assert_eq!(q.begin(labels(&["b", "a"])), Some("b".to_string()));
    }

    #[test]
    fn quit_sequence_with_no_windows_does_not_start() {
        let q = QuitSequence::default();
        assert_eq!(q.begin(vec![]), None);
        assert_eq!(q.begin(labels(&["a"])), Some("a".to_string()));
    }

    #[test]
    fn quit_sequence_window_gone() {
        let q = QuitSequence::default();
        assert_eq!(q.window_gone("a"), Step::Ignore);

        // The head goes away: ask the next one.
        q.begin(labels(&["a", "b"]));
        assert_eq!(q.window_gone("a"), Step::Ask("b".into()));
        // The last one goes away: exit.
        assert_eq!(q.window_gone("b"), Step::Exit);
        assert_eq!(q.window_gone("b"), Step::Ignore);

        // A later window goes away: it is skipped.
        q.begin(labels(&["a", "b", "c"]));
        assert_eq!(q.window_gone("b"), Step::Ignore);
        assert_eq!(q.advance("a"), Step::Ask("c".into()));
        assert_eq!(q.advance("c"), Step::Exit);

        // An unknown window changes nothing.
        q.begin(labels(&["a", "b"]));
        assert_eq!(q.window_gone("zzz"), Step::Ignore);
        assert_eq!(q.advance("a"), Step::Ask("b".into()));
    }

    #[test]
    fn quit_request_starts_a_quit_or_re_asks_the_current_window() {
        let q = QuitSequence::default();
        assert_eq!(q.request(vec![]), None);
        assert_eq!(q.request(labels(&["a", "b"])), Some("a".to_string()));
        // ⌘Q again while `a` is being asked: ask `a` again, keep the order.
        assert_eq!(q.request(labels(&["b", "a"])), Some("a".to_string()));
        assert_eq!(q.advance("a"), Step::Ask("b".into()));
        assert_eq!(q.request(labels(&["a", "b"])), Some("b".to_string()));
        assert_eq!(q.advance("b"), Step::Exit);
        // Finished: the next ⌘Q starts over with the new order.
        assert_eq!(q.request(labels(&["b", "a"])), Some("b".to_string()));
        q.cancel();
        assert_eq!(q.request(labels(&["a"])), Some("a".to_string()));
    }

    #[test]
    fn drive_asks_the_next_window_when_the_head_is_destroyed() {
        let q = QuitSequence::default();
        q.begin(labels(&["a", "b"]));
        let mut asked = vec![];
        let step = q.window_gone("a");
        let got = drive(&q, step, |l| {
            asked.push(l.to_string());
            Ok(())
        });
        assert_eq!(got, Driven::Asked("b".into()));
        assert_eq!(asked, labels(&["b"]));
        // The last window is destroyed: exit.
        assert_eq!(drive(&q, q.window_gone("b"), |_| Ok(())), Driven::Exit);
        assert_eq!(drive(&q, Step::Ignore, |_| panic!("nothing to ask")), Driven::Idle);
    }

    #[test]
    fn drive_skips_windows_that_cannot_be_asked() {
        let q = QuitSequence::default();
        q.begin(labels(&["a", "b", "c"]));
        let mut asked = vec![];
        let got = drive(&q, Step::Ask("a".into()), |l| {
            asked.push(l.to_string());
            if l == "c" { Ok(()) } else { Err("gone".into()) }
        });
        assert_eq!(got, Driven::Asked("c".into()));
        assert_eq!(asked, labels(&["a", "b", "c"]));
        assert_eq!(q.head(), Some("c".to_string()));
        // None can be asked: every window counts as gone, so the app exits.
        let q = QuitSequence::default();
        q.begin(labels(&["a", "b"]));
        assert_eq!(drive(&q, Step::Ask("a".into()), |_| Err("gone".into())), Driven::Exit);
        assert_eq!(q.head(), None);
    }

    #[test]
    fn cascade_offsets_from_the_focused_window_then_main() {
        assert_eq!(cascade_position(Some((100.0, 50.0)), Some((0.0, 0.0))), Some((128.0, 78.0)));
        assert_eq!(cascade_position(None, Some((10.0, 20.0))), Some((38.0, 48.0)));
        assert_eq!(cascade_position(None, None), None);
    }

    #[test]
    fn quit_order_puts_the_focused_window_first_then_label_order() {
        let all = labels(&["window-10", "window-2", "main", "window-3"]);
        assert_eq!(
            quit_order(Some("window-3"), all.clone()),
            labels(&["window-3", "main", "window-2", "window-10"])
        );
        assert_eq!(quit_order(None, all), labels(&["main", "window-2", "window-3", "window-10"]));
        assert_eq!(quit_order(Some("gone"), labels(&["main"])), labels(&["main"]));
    }
}
