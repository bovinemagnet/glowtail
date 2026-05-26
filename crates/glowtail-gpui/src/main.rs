use anyhow::{Context as AnyhowContext, Result};
use clap::Parser;
use glowtail_core::prelude::*;
use glowtail_ui_common::{
    LevelArg, LiveTail, apply_filters, load_session, parser_from_flags, save_session, start_tailers,
};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, InteractiveElement, IntoElement, KeyBinding,
    ListAlignment, ListOffset, ListState, ParentElement, Pixels, Render, SharedString, Styled,
    Window, WindowBounds, WindowOptions, actions, div, list, prelude::*, px, rgb, rgba, size,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

const ROW_OVERDRAW: f32 = 640.0;
/// How often the live-tail refresh task wakes the view to drain pending
/// events. 16ms ≈ 60 Hz — matches typical monitor refresh and, combined with
/// `DEFAULT_TAILER_CHANNEL_CAPACITY`, lifts sustained tail throughput from
/// ~10k rows/s (at 100ms) to ~1M rows/s without per-row render cost since
/// `drain_live_events` already coalesces all pending events per notify.
const LIVE_REFRESH_MS: u64 = 16;
const HORIZONTAL_STEP_PX: f32 = 8.0;
/// Approximate visible row count used for Page Up/Down. The window default is
/// 900 px tall with ~24 px rows minus chrome — 25 leaves headroom on small
/// windows without overshooting on the default.
const PAGE_SIZE_HINT: usize = 25;

actions!(
    glowtail_gpui,
    [
        ScrollUp,
        ScrollDown,
        PageUp,
        PageDown,
        ScrollHome,
        ScrollEnd,
        ScrollLeft,
        ScrollRight,
        ScrollLineStart,
        ToggleFollow,
        FilterTrace,
        FilterDebug,
        FilterInfo,
        FilterWarn,
        FilterError,
        FilterFatal,
        FilterClear,
        CycleSavedFilter,
        FocusFilterInput,
        BlurInput,
        SubmitInput,
        InputBackspace,
        SelectUp,
        SelectDown,
        SelectPageUp,
        SelectPageDown,
        ToggleBookmark,
        FocusSearchInput,
        NextSearchMatch,
        PrevSearchMatch,
        ClearSearch,
        TogglePalette,
        Quit
    ]
);

/// One row in the command palette. The fixed `Action` variants cover the
/// common operations that mirror the egui GUI's palette; `ApplySavedFilter`
/// is generated dynamically per saved filter loaded from the session.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PaletteCommand {
    ClearFilter,
    ClearSearch,
    NextSearch,
    PrevSearch,
    ToggleBookmark,
    ToggleFollow,
    ApplySavedFilter(String),
    Quit,
}

impl PaletteCommand {
    fn label(&self) -> String {
        match self {
            Self::ClearFilter => "Clear filter (text and level)".to_string(),
            Self::ClearSearch => "Clear search".to_string(),
            Self::NextSearch => "Next search match".to_string(),
            Self::PrevSearch => "Previous search match".to_string(),
            Self::ToggleBookmark => "Toggle bookmark on selected row".to_string(),
            Self::ToggleFollow => "Toggle follow mode".to_string(),
            Self::ApplySavedFilter(name) => format!("Apply saved filter: {name}"),
            Self::Quit => "Quit".to_string(),
        }
    }
}

/// Which (if any) of the in-window text inputs currently captures
/// keystrokes. The two inputs (filter, search) are mutually exclusive
/// by design: pressing `/` or `?` switches focus, and `escape`/`enter`/
/// `backspace` route to whichever is active. Modelling this as an enum
/// keeps "exactly one input is focused at a time" a type-level
/// invariant instead of an implicit pair-of-bools rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputFocus {
    None,
    Filter,
    Search,
}

/// Append-only single-line text buffer for the filter input. No cursor
/// positioning, no selection — pressing a key appends, backspace removes
/// the trailing character. Kept as a plain struct (no GPUI dependency) so
/// the mutation rules can be unit-tested in isolation.
#[derive(Debug, Default, Clone)]
struct TextInputState {
    value: String,
}

impl TextInputState {
    fn new(initial: Option<String>) -> Self {
        Self {
            value: initial.unwrap_or_default(),
        }
    }

    fn append_char(&mut self, c: char) {
        self.value.push(c);
    }

    /// Pop the trailing character — UTF-8 safe because `String::pop` operates
    /// on `Option<char>`, not bytes.
    fn backspace(&mut self) {
        self.value.pop();
    }

    fn value(&self) -> &str {
        &self.value
    }
}

/// Decide whether a [`gpui::Keystroke`] should be appended to a focused
/// text input. Accepts single-char `key_char`s when no non-shift modifier
/// is held — so plain letters, digits, punctuation and shifted variants
/// (capitals, `!`, `@`, etc.) flow through, while `cmd-a`, `ctrl-c`, and
/// special keys like `tab`/`enter`/`backspace` (whose `key_char` is
/// `None` on every platform we target) are rejected. Pulled out as a
/// pure fn so the filter logic can be unit-tested.
fn keystroke_to_input_char(
    key_char: Option<&str>,
    has_command: bool,
    has_control: bool,
    has_alt: bool,
    has_function: bool,
) -> Option<char> {
    if has_command || has_control || has_alt || has_function {
        return None;
    }
    let s = key_char?;
    let mut chars = s.chars();
    let first = chars.next()?;
    // Reject multi-grapheme inputs (composed emoji, IME output) for the
    // MVP; this matches the "ASCII-and-shifted-ASCII only" scope.
    if chars.next().is_some() {
        return None;
    }
    if first.is_control() {
        return None;
    }
    Some(first)
}

/// Compute the next index into the saved-filter cycle. `None` means "no
/// saved filter applied"; pressing past the last filter wraps back to
/// `None` so the user can iterate `None -> first -> ... -> last -> None`.
/// Pulled out as a pure fn so the cycle can be unit-tested without a GPUI
/// context.
fn next_saved_filter_cycle(current: Option<usize>, total: usize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    match current {
        None => Some(0),
        Some(i) if i + 1 >= total => None,
        Some(i) => Some(i + 1),
    }
}

/// Compute the next selected position after pressing `j`/`k` (or arrow
/// navigation). `delta` is signed (positive = down, negative = up).
/// `total` is the number of currently-visible filtered rows. Returns
/// `None` when the filtered view is empty; otherwise clamps to
/// `[0, total - 1]`. From `None`, a downward move selects the first row
/// and an upward move selects the last (matches the TUI behaviour).
/// Pulled out as a pure fn so the clamping rules can be unit-tested
/// without spinning up a GPUI window.
fn next_selected_position(current: Option<usize>, delta: isize, total: usize) -> Option<usize> {
    if total == 0 {
        return None;
    }
    let max = total as isize - 1;
    let next = match current {
        None if delta >= 0 => 0,
        None => max,
        Some(pos) => (pos as isize).saturating_add(delta).clamp(0, max),
    };
    Some(next as usize)
}

#[derive(Debug, Parser)]
#[command(name = "glowtail-gpui")]
#[command(about = "Native GPUI glowtail desktop UI")]
struct Args {
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    plain: bool,
    #[arg(long)]
    filter: Option<String>,
    #[arg(long)]
    level: Option<LevelArg>,
    /// Don't tail the file. The viewport opens at the top and stays there;
    /// no live updates are streamed from disk.
    #[arg(long)]
    no_follow: bool,
    /// Open with the viewport pinned to the tail (newest row visible) and
    /// follow new rows as they arrive — like `tail -f`. This is the default;
    /// the flag exists so it can be set explicitly in scripts or shell history.
    /// Scrolling up detaches; press `f` or `End` to re-attach.
    #[arg(long, short = 'f', conflicts_with = "no_follow")]
    follow: bool,
    #[arg(long)]
    from_start: bool,
    #[arg(long)]
    session: Option<PathBuf>,
    #[arg(long)]
    use_filter: Option<String>,
    #[arg(long)]
    save_filter: Option<String>,
    /// Retain at most this many rows; older rows are dropped from the front
    /// of the buffer when the cap is exceeded. `0` means unbounded (default).
    #[arg(long)]
    max_rows: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let parser = parser_from_flags(args.json, args.plain);
    let session = load_session(args.session.as_ref())?;
    let mut engine = Engine::with_session(session);
    // Accumulate per-path load errors so a single unreadable file doesn't
    // prevent the window from opening on the readable ones.
    let mut load_errors: Vec<String> = Vec::new();
    if args.no_follow || !args.from_start {
        for path in &args.paths {
            if let Err(err) = engine.load_file(path, parser.as_ref()) {
                load_errors.push(format!("failed to read {}: {err}", path.display()));
            }
        }
    }
    engine.set_max_rows(normalise_max_rows(args.max_rows));
    apply_filters(
        &mut engine,
        args.filter.clone(),
        args.level,
        args.use_filter.clone(),
        args.save_filter,
    )?;

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("glowtail-gpui-tail")
        .build()
        .context("failed to create async runtime")?;
    let live_tail = if args.no_follow {
        None
    } else {
        Some(start_tailers(
            &runtime,
            &args.paths,
            Arc::clone(&parser),
            args.from_start,
        ))
    };
    let initial_status = if load_errors.is_empty() {
        None
    } else {
        Some(load_errors.join("; "))
    };
    let app = GlowtailGpui::new(
        engine,
        runtime,
        live_tail,
        args.session,
        initial_status,
        args.level,
        args.use_filter,
        args.filter,
    );

    let launch_error: Arc<std::sync::Mutex<Option<anyhow::Error>>> =
        Arc::new(std::sync::Mutex::new(None));
    let launch_error_clone = Arc::clone(&launch_error);
    Application::new().run(move |cx: &mut App| {
        cx.bind_keys([
            KeyBinding::new("up", ScrollUp, None),
            KeyBinding::new("down", ScrollDown, None),
            KeyBinding::new("pageup", PageUp, None),
            KeyBinding::new("pagedown", PageDown, None),
            KeyBinding::new("home", ScrollHome, None),
            KeyBinding::new("end", ScrollEnd, None),
            KeyBinding::new("left", ScrollLeft, None),
            KeyBinding::new("right", ScrollRight, None),
            KeyBinding::new("cmd-left", ScrollLineStart, None),
            KeyBinding::new("f", ToggleFollow, None),
            KeyBinding::new("1", FilterTrace, None),
            KeyBinding::new("2", FilterDebug, None),
            KeyBinding::new("3", FilterInfo, None),
            KeyBinding::new("4", FilterWarn, None),
            KeyBinding::new("5", FilterError, None),
            KeyBinding::new("6", FilterFatal, None),
            KeyBinding::new("0", FilterClear, None),
            KeyBinding::new("s", CycleSavedFilter, None),
            KeyBinding::new("/", FocusFilterInput, None),
            KeyBinding::new("escape", BlurInput, None),
            KeyBinding::new("enter", SubmitInput, None),
            KeyBinding::new("backspace", InputBackspace, None),
            // Row selection cursor — independent of the scroll keys so
            // arrows/PgUp/PgDn keep their existing "pan the viewport"
            // behaviour while `j`/`k` move a separate selection marker.
            KeyBinding::new("j", SelectDown, None),
            KeyBinding::new("k", SelectUp, None),
            KeyBinding::new("shift-j", SelectPageDown, None),
            KeyBinding::new("shift-k", SelectPageUp, None),
            KeyBinding::new("b", ToggleBookmark, None),
            // Search: `?` opens the search input (pairs with `/` for
            // filter); `n` / `shift-n` step through matches in the
            // currently-filtered view. `cmd-shift-f` clears the search
            // without having to re-focus and submit an empty value.
            KeyBinding::new("?", FocusSearchInput, None),
            KeyBinding::new("n", NextSearchMatch, None),
            KeyBinding::new("shift-n", PrevSearchMatch, None),
            KeyBinding::new("cmd-shift-f", ClearSearch, None),
            KeyBinding::new("ctrl-shift-f", ClearSearch, None),
            // Command palette: Cmd/Ctrl+K toggles the modal overlay.
            // Inside the palette, j/k/PgUp/PgDn move the highlight,
            // Enter executes, Escape closes.
            KeyBinding::new("cmd-k", TogglePalette, None),
            KeyBinding::new("ctrl-k", TogglePalette, None),
        ]);
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let result = cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |cx| {
                    let mut app = app;
                    app.focus_handle = Some(cx.focus_handle());
                    app.start_refresh_loop(cx);
                    app
                })
            },
        );
        if let Err(err) = result {
            *launch_error_clone.lock().unwrap() =
                Some(anyhow::anyhow!("failed to open GPUI window: {err}"));
            return;
        }
        cx.activate(true);
    });

    if let Some(err) = launch_error.lock().unwrap().take() {
        return Err(err);
    }
    Ok(())
}

/// Treat `--max-rows 0` and an absent flag as "unbounded" so the CLI surface
/// is forgiving — `0` reading as "no rows retained" is a usability trap.
fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
}

struct GlowtailGpui {
    /// Engine wrapped in `Rc<RefCell<_>>` so the per-row render closure can
    /// borrow it mutably without us materialising every row up-front.
    engine: Rc<RefCell<Engine>>,
    metadata: Arc<ViewportSnapshot>,
    list_state: ListState,
    /// Kept alive to host the spawned `FileTailer` tasks. Not read after
    /// construction — `Drop` order ensures it outlives `live_tail`, so the
    /// runtime drives the tasks to completion after `signal_stop` (M5).
    #[allow(dead_code)]
    runtime: Runtime,
    live_tail: Option<LiveTail>,
    status_message: Option<String>,
    session_path: Option<PathBuf>,
    horizontal_offset_px: f32,
    /// When true, every newly appended row scrolls the viewport to the bottom.
    /// Disabled by any upward navigation; re-enabled by `End` or the `f` toggle.
    follow: bool,
    /// Set at construction when `follow` is true so that the very first
    /// `render()` snaps the viewport to the bottom of the preloaded file.
    /// Without this, `ListState::new(_, ListAlignment::Top, _)` lands the user
    /// at row 0 and `refresh_metadata()` only re-scrolls on subsequent
    /// appends — so users on stationary or slowly-growing files never saw
    /// the tail. Cleared on the first frame that succeeds in scrolling.
    pending_initial_scroll_to_bottom: bool,
    /// Populated in the `cx.new(...)` constructor closure once a `Context` is
    /// available. Used by `track_focus` on the root div so keyboard actions
    /// have somewhere to dispatch.
    focus_handle: Option<FocusHandle>,
    focused_once: bool,
    /// Current `--level`-style filter; mutated by the digit-key actions
    /// (`1`–`6` set, `0` clears). Re-applied via [`Self::recompute_filter`]
    /// so it composes with the text filter and the saved-filter cycle.
    current_level: Option<LevelArg>,
    /// Name of the saved filter currently applied, or `None` if no saved
    /// filter is active. Mutated by [`Self::on_cycle_saved_filter`].
    current_use_filter: Option<String>,
    /// Free-text filter substring (from `--filter` today; Part B will hook
    /// in-window text input into this same field).
    current_text: Option<String>,
    /// Currently-applied search needle, mirrored from the engine's
    /// internal state so we can restore the input buffer on blur. The
    /// engine is the source of truth (`set_search_text` / `search_results`);
    /// we just remember what was last submitted.
    current_search: Option<String>,
    /// Cycle cursor for `CycleSavedFilter` (`s`). `None` = no saved filter
    /// applied; otherwise the 0-based index into `session.saved_filters`.
    saved_filter_cycle_idx: Option<usize>,
    /// Buffered text the user is currently typing into the filter input.
    /// Becomes `current_text` only on `Enter`; until then the engine sees
    /// the previously-submitted value.
    filter_input: TextInputState,
    /// Buffered text for the search input. Becomes the engine's search
    /// needle on `Enter`; until then the engine sees the previously
    /// submitted search value (or `None`).
    search_input: TextInputState,
    /// Which (if any) input currently captures keystrokes. While any
    /// input is focused, the navigation keybindings (digits, `f`, `s`,
    /// arrow keys, j/k, b, n/N, …) are suppressed so keys type into the
    /// input instead of triggering an action.
    input_focus: InputFocus,
    /// Currently-selected row, identified by `RowId` so the selection
    /// survives filter changes that reorder/hide rows. The viewport
    /// position is re-derived on each render via
    /// [`Engine::filtered_position_for_row`]; if the row is filtered out
    /// the cursor disappears but the row id is retained so the user can
    /// restore the filter and recover the selection.
    selected_row_id: Option<RowId>,
    /// True while the command-palette modal is open. While open, the
    /// `j`/`k`/arrow/page actions move the palette cursor, `Enter`
    /// executes the highlighted command, and `Escape` closes the palette.
    /// `any_input_focused()` treats this the same as a focused text
    /// input, so the level/filter/scroll bindings stay suppressed.
    palette_open: bool,
    /// Index of the highlighted palette command. Clamped on each render
    /// against the dynamically-sized command list so a session reload
    /// with fewer saved filters doesn't leave the cursor dangling.
    palette_cursor: usize,
}

impl GlowtailGpui {
    #[allow(clippy::too_many_arguments)]
    fn new(
        engine: Engine,
        runtime: Runtime,
        live_tail: Option<LiveTail>,
        session_path: Option<PathBuf>,
        status_message: Option<String>,
        initial_level: Option<LevelArg>,
        initial_use_filter: Option<String>,
        initial_text: Option<String>,
    ) -> Self {
        let engine = Rc::new(RefCell::new(engine));
        let (metadata, item_count) = {
            let mut engine = engine.borrow_mut();
            let metadata = engine.metadata_snapshot();
            let count = engine.matching_rows_count();
            (metadata, count)
        };
        let list_state = ListState::new(item_count, ListAlignment::Top, px(ROW_OVERDRAW));
        let follow = live_tail.is_some();
        Self {
            engine,
            metadata: Arc::new(metadata),
            list_state,
            runtime,
            live_tail,
            status_message,
            session_path,
            horizontal_offset_px: 0.0,
            focus_handle: None,
            focused_once: false,
            follow,
            pending_initial_scroll_to_bottom: follow,
            current_level: initial_level,
            current_use_filter: initial_use_filter,
            current_text: initial_text.clone(),
            current_search: None,
            saved_filter_cycle_idx: None,
            filter_input: TextInputState::new(initial_text),
            search_input: TextInputState::new(None),
            input_focus: InputFocus::None,
            selected_row_id: None,
            palette_open: false,
            palette_cursor: 0,
        }
    }

    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        if self.live_tail.is_none() {
            return;
        }

        cx.spawn(
            async move |view: gpui::WeakEntity<GlowtailGpui>, cx: &mut gpui::AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(LIVE_REFRESH_MS))
                        .await;
                    if view.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            },
        )
        .detach();
    }

    fn drain_live_events(&mut self) -> bool {
        let Some(live_tail) = self.live_tail.as_mut() else {
            return false;
        };

        let mut changed = false;
        loop {
            match live_tail.receiver.try_recv() {
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    self.engine
                        .borrow_mut()
                        .add_source(source_id, path.display().to_string());
                    changed = true;
                }
                Ok(LogEvent::RowAppended(row)) => {
                    self.engine.borrow_mut().append_row(row);
                    changed = true;
                }
                Ok(LogEvent::SourceRotated { source_id }) => {
                    self.status_message = Some(format!("source {} rotated", source_id.0));
                }
                Ok(LogEvent::SourceError { source_id, message }) => {
                    self.status_message = Some(format!("source {} error: {message}", source_id.0));
                }
                Ok(LogEvent::SourceRemoved { .. }) => {}
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.status_message = Some("live tail disconnected".into());
                    break;
                }
            }
        }

        if changed {
            self.refresh_metadata();
            self.save_session();
        }
        changed
    }

    fn refresh_metadata(&mut self) {
        let (metadata, new_count) = {
            let mut engine = self.engine.borrow_mut();
            (engine.metadata_snapshot(), engine.matching_rows_count())
        };
        self.metadata = Arc::new(metadata);
        // Only rebuild the ListState if the row count actually changed —
        // otherwise we'd snap the scroll position to the top on every frame.
        if new_count != self.list_state.item_count() {
            // Capture the current logical offset so we can restore it after
            // the rebuild and avoid jumping to row 0 on every appended line.
            let logical_offset = self.list_state.logical_scroll_top();
            self.list_state = ListState::new(new_count, ListAlignment::Top, px(ROW_OVERDRAW));
            self.list_state.scroll_to(logical_offset);
        }
        if self.follow {
            self.scroll_to_bottom();
        }
    }

    fn scroll_to_bottom(&mut self) {
        let total = self.list_state.item_count();
        if total > 0 {
            self.list_state.scroll_to_reveal_item(total - 1);
        }
    }

    fn save_session(&self) {
        let _ = save_session(self.session_path.as_ref(), self.engine.borrow().session());
    }

    fn scroll_by_items(&mut self, delta: isize) {
        let total = self.list_state.item_count();
        if total == 0 {
            return;
        }
        let top = self.list_state.logical_scroll_top();
        let max = total as isize - 1;
        let new_ix = (top.item_ix as isize + delta).clamp(0, max) as usize;
        self.list_state.scroll_to(ListOffset {
            item_ix: new_ix,
            offset_in_item: Pixels::ZERO,
        });
    }

    fn on_scroll_up(&mut self, _: &ScrollUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.follow = false;
        self.scroll_by_items(-1);
        cx.notify();
    }
    fn on_scroll_down(&mut self, _: &ScrollDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.scroll_by_items(1);
        cx.notify();
    }
    fn on_page_up(&mut self, _: &PageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.follow = false;
        self.scroll_by_items(-(PAGE_SIZE_HINT as isize));
        cx.notify();
    }
    fn on_page_down(&mut self, _: &PageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.scroll_by_items(PAGE_SIZE_HINT as isize);
        cx.notify();
    }
    fn on_scroll_home(&mut self, _: &ScrollHome, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.follow = false;
        self.list_state.scroll_to(ListOffset {
            item_ix: 0,
            offset_in_item: Pixels::ZERO,
        });
        cx.notify();
    }
    fn on_scroll_end(&mut self, _: &ScrollEnd, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.follow = true;
        self.scroll_to_bottom();
        cx.notify();
    }
    fn on_toggle_follow(&mut self, _: &ToggleFollow, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.follow = !self.follow;
        if self.follow {
            self.scroll_to_bottom();
        }
        cx.notify();
    }
    fn on_scroll_left(&mut self, _: &ScrollLeft, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.horizontal_offset_px = (self.horizontal_offset_px - HORIZONTAL_STEP_PX).max(0.0);
        cx.notify();
    }
    fn on_scroll_right(&mut self, _: &ScrollRight, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.horizontal_offset_px += HORIZONTAL_STEP_PX;
        cx.notify();
    }
    fn on_scroll_line_start(
        &mut self,
        _: &ScrollLineStart,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.any_input_focused() {
            return;
        }
        self.horizontal_offset_px = 0.0;
        cx.notify();
    }

    /// Recompose level + saved-filter + text into a single [`FilterExpr`]
    /// and apply it. Errors set the status message instead of crashing —
    /// the only realistic source is a missing saved filter name, which
    /// shouldn't happen given the cycle only ever picks names we just
    /// read from the session, but we surface it rather than panic.
    fn recompute_filter(&mut self) {
        let result = {
            let mut engine = self.engine.borrow_mut();
            apply_filters(
                &mut engine,
                self.current_text.clone(),
                self.current_level,
                self.current_use_filter.clone(),
                None,
            )
        };
        if let Err(err) = result {
            self.status_message = Some(format!("filter error: {err}"));
            return;
        }
        self.refresh_metadata();
    }

    fn set_level_filter(&mut self, level: Option<LevelArg>, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.current_level = level;
        self.status_message = Some(match level {
            None => "level filter cleared".into(),
            Some(level) => format!("level filter: {level:?}").to_lowercase(),
        });
        self.recompute_filter();
        cx.notify();
    }

    fn on_filter_trace(&mut self, _: &FilterTrace, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Trace), cx);
    }
    fn on_filter_debug(&mut self, _: &FilterDebug, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Debug), cx);
    }
    fn on_filter_info(&mut self, _: &FilterInfo, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Info), cx);
    }
    fn on_filter_warn(&mut self, _: &FilterWarn, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Warn), cx);
    }
    fn on_filter_error(&mut self, _: &FilterError, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Error), cx);
    }
    fn on_filter_fatal(&mut self, _: &FilterFatal, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(Some(LevelArg::Fatal), cx);
    }
    fn on_filter_clear(&mut self, _: &FilterClear, _: &mut Window, cx: &mut Context<Self>) {
        self.set_level_filter(None, cx);
    }

    fn on_cycle_saved_filter(
        &mut self,
        _: &CycleSavedFilter,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.any_input_focused() {
            return;
        }
        let names: Vec<String> = self
            .engine
            .borrow()
            .session()
            .saved_filters
            .iter()
            .map(|filter| filter.name.to_string())
            .collect();
        if names.is_empty() {
            self.status_message = Some("no saved filters in session".into());
            cx.notify();
            return;
        }
        let next = next_saved_filter_cycle(self.saved_filter_cycle_idx, names.len());
        self.saved_filter_cycle_idx = next;
        self.current_use_filter = next.map(|i| names[i].clone());
        self.status_message = Some(match &self.current_use_filter {
            None => "saved filter cleared".into(),
            Some(name) => format!("saved filter: {name}"),
        });
        self.recompute_filter();
        cx.notify();
    }

    fn any_input_focused(&self) -> bool {
        self.input_focus != InputFocus::None || self.palette_open
    }

    /// Build the current palette command list. Order is stable so
    /// keyboard muscle-memory holds across sessions; saved-filter
    /// commands come last because their count varies per session.
    fn palette_commands(&self) -> Vec<PaletteCommand> {
        let mut commands = vec![
            PaletteCommand::ClearFilter,
            PaletteCommand::ClearSearch,
            PaletteCommand::NextSearch,
            PaletteCommand::PrevSearch,
            PaletteCommand::ToggleBookmark,
            PaletteCommand::ToggleFollow,
        ];
        for saved in &self.engine.borrow().session().saved_filters {
            commands.push(PaletteCommand::ApplySavedFilter(saved.name.to_string()));
        }
        commands.push(PaletteCommand::Quit);
        commands
    }

    fn execute_palette_command(&mut self, command: PaletteCommand, cx: &mut Context<Self>) {
        match command {
            PaletteCommand::ClearFilter => {
                self.current_text = None;
                self.current_level = None;
                self.current_use_filter = None;
                self.saved_filter_cycle_idx = None;
                self.filter_input = TextInputState::new(None);
                self.status_message = Some("filter cleared".into());
                self.recompute_filter();
            }
            PaletteCommand::ClearSearch => {
                self.current_search = None;
                self.search_input = TextInputState::new(None);
                self.engine.borrow_mut().set_search_text(None);
                self.status_message = Some("search cleared".into());
                self.refresh_metadata();
            }
            PaletteCommand::NextSearch => self.jump_to_match(false),
            PaletteCommand::PrevSearch => self.jump_to_match(true),
            PaletteCommand::ToggleBookmark => {
                if let Some(row_id) = self.selected_row_id {
                    let is_bookmarked = self.engine.borrow_mut().toggle_bookmark(row_id, None);
                    self.status_message = Some(if is_bookmarked {
                        format!("bookmarked row {}", row_id.0)
                    } else {
                        format!("removed bookmark on row {}", row_id.0)
                    });
                    self.save_session();
                } else {
                    self.status_message = Some("select a row first (press j/k)".into());
                }
            }
            PaletteCommand::ToggleFollow => {
                self.follow = !self.follow;
                if self.follow {
                    self.scroll_to_bottom();
                }
            }
            PaletteCommand::ApplySavedFilter(name) => {
                self.current_use_filter = Some(name.clone());
                // Sync the cycle cursor so subsequent presses of `s`
                // continue from the just-applied filter instead of
                // restarting from the first saved entry.
                self.saved_filter_cycle_idx = self
                    .engine
                    .borrow()
                    .session()
                    .saved_filters
                    .iter()
                    .position(|saved| saved.name.as_ref() == name);
                self.status_message = Some(format!("saved filter: {name}"));
                self.recompute_filter();
            }
            PaletteCommand::Quit => {
                cx.quit();
            }
        }
        cx.notify();
    }

    fn on_toggle_palette(&mut self, _: &TogglePalette, _: &mut Window, cx: &mut Context<Self>) {
        // Reject toggle while a text input owns focus — pressing cmd-k
        // while typing should add a character to the input (handled by
        // `on_key_down_capture`), not open the palette.
        if self.input_focus != InputFocus::None {
            return;
        }
        self.palette_open = !self.palette_open;
        if self.palette_open {
            self.palette_cursor = 0;
        }
        cx.notify();
    }

    fn on_focus_filter_input(
        &mut self,
        _: &FocusFilterInput,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_open || self.input_focus == InputFocus::Filter {
            return;
        }
        // Switching away from a half-typed search: reset the search buffer to
        // the last submitted value so a future re-focus doesn't show stale
        // keystrokes. Symmetric with `on_focus_search_input`.
        if self.input_focus == InputFocus::Search {
            self.search_input = TextInputState::new(self.current_search.clone());
        }
        self.input_focus = InputFocus::Filter;
        self.status_message = Some("filter: typing... (enter to apply, esc to cancel)".into());
        cx.notify();
    }

    fn on_focus_search_input(
        &mut self,
        _: &FocusSearchInput,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.palette_open || self.input_focus == InputFocus::Search {
            return;
        }
        if self.input_focus == InputFocus::Filter {
            self.filter_input = TextInputState::new(self.current_text.clone());
        }
        self.input_focus = InputFocus::Search;
        self.status_message = Some("search: typing... (enter to apply, esc to cancel)".into());
        cx.notify();
    }

    fn on_blur_input(&mut self, _: &BlurInput, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.palette_open = false;
            cx.notify();
            return;
        }
        match self.input_focus {
            InputFocus::None => {}
            InputFocus::Filter => {
                // Restore the input buffer to whatever was last submitted so a
                // future refocus doesn't show abandoned keystrokes.
                self.filter_input = TextInputState::new(self.current_text.clone());
                self.input_focus = InputFocus::None;
                self.status_message = Some("filter input cancelled".into());
                cx.notify();
            }
            InputFocus::Search => {
                self.search_input = TextInputState::new(self.current_search.clone());
                self.input_focus = InputFocus::None;
                self.status_message = Some("search input cancelled".into());
                cx.notify();
            }
        }
    }

    fn on_submit_input(&mut self, _: &SubmitInput, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            let commands = self.palette_commands();
            if let Some(command) = commands.get(self.palette_cursor).cloned() {
                self.palette_open = false;
                self.execute_palette_command(command, cx);
            }
            return;
        }
        match self.input_focus {
            InputFocus::None => {}
            InputFocus::Filter => {
                let trimmed = self.filter_input.value().trim();
                self.current_text = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                self.input_focus = InputFocus::None;
                self.status_message = Some(match &self.current_text {
                    None => "text filter cleared".into(),
                    Some(text) => format!("text filter: {text}"),
                });
                self.recompute_filter();
                cx.notify();
            }
            InputFocus::Search => {
                let trimmed = self.search_input.value().trim();
                self.current_search = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                };
                self.input_focus = InputFocus::None;
                self.engine
                    .borrow_mut()
                    .set_search_text(self.current_search.clone());
                self.status_message = Some(match &self.current_search {
                    None => "search cleared".into(),
                    Some(text) => format!("search: {text} (n/N to navigate)"),
                });
                // Jump to the first match so the user sees the result of
                // pressing Enter, mirroring the egui GUI's behaviour.
                self.jump_to_match(false);
                self.refresh_metadata();
                cx.notify();
            }
        }
    }

    fn on_input_backspace(&mut self, _: &InputBackspace, _: &mut Window, cx: &mut Context<Self>) {
        match self.input_focus {
            InputFocus::None => {}
            InputFocus::Filter => {
                self.filter_input.backspace();
                cx.notify();
            }
            InputFocus::Search => {
                self.search_input.backspace();
                cx.notify();
            }
        }
    }

    fn jump_to_match(&mut self, reverse: bool) {
        let current = self.selected_row_id;
        let next = self
            .engine
            .borrow_mut()
            .next_search_result(current, reverse);
        let Some(row_id) = next else {
            self.status_message = Some("no search matches".into());
            return;
        };
        let position = self.engine.borrow_mut().filtered_position_for_row(row_id);
        self.selected_row_id = Some(row_id);
        if let Some(pos) = position {
            self.list_state.scroll_to_reveal_item(pos);
        }
        self.follow = false;
    }

    fn on_next_search_match(
        &mut self,
        _: &NextSearchMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.any_input_focused() {
            return;
        }
        self.jump_to_match(false);
        cx.notify();
    }

    fn on_prev_search_match(
        &mut self,
        _: &PrevSearchMatch,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.any_input_focused() {
            return;
        }
        self.jump_to_match(true);
        cx.notify();
    }

    fn on_clear_search(&mut self, _: &ClearSearch, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        self.current_search = None;
        self.search_input = TextInputState::new(None);
        self.engine.borrow_mut().set_search_text(None);
        self.status_message = Some("search cleared".into());
        self.refresh_metadata();
        cx.notify();
    }

    /// Look up the visible position of [`Self::selected_row_id`] in the
    /// current filtered view. Returns `None` if no row is selected or if
    /// the selection has been filtered out — both are valid states and
    /// the UI degrades gracefully (no highlight, bookmark action surfaces
    /// a status message).
    fn selected_position(&self) -> Option<usize> {
        let row_id = self.selected_row_id?;
        self.engine.borrow_mut().filtered_position_for_row(row_id)
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.list_state.item_count();
        let current = self.selected_position();
        let Some(next) = next_selected_position(current, delta, total) else {
            self.selected_row_id = None;
            cx.notify();
            return;
        };
        // Map the new position back to a stable RowId so subsequent filter
        // changes don't drop the cursor onto a different row.
        let row_id = self
            .engine
            .borrow_mut()
            .present_row_at(next)
            .map(|row| row.row_id);
        self.selected_row_id = row_id;
        self.list_state.scroll_to_reveal_item(next);
        // Manual selection moves disengage follow — same convention as
        // the scroll keys: the user is now investigating, not tailing.
        self.follow = false;
        cx.notify();
    }

    fn move_palette_cursor(&mut self, delta: isize, cx: &mut Context<Self>) {
        let total = self.palette_commands().len();
        if let Some(next) = next_selected_position(Some(self.palette_cursor), delta, total) {
            self.palette_cursor = next;
            cx.notify();
        }
    }

    fn on_select_up(&mut self, _: &SelectUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.move_palette_cursor(-1, cx);
            return;
        }
        if self.any_input_focused() {
            return;
        }
        self.move_selection(-1, cx);
    }
    fn on_select_down(&mut self, _: &SelectDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.move_palette_cursor(1, cx);
            return;
        }
        if self.any_input_focused() {
            return;
        }
        self.move_selection(1, cx);
    }
    fn on_select_page_up(&mut self, _: &SelectPageUp, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.move_palette_cursor(-(PAGE_SIZE_HINT as isize), cx);
            return;
        }
        if self.any_input_focused() {
            return;
        }
        self.move_selection(-(PAGE_SIZE_HINT as isize), cx);
    }
    fn on_select_page_down(&mut self, _: &SelectPageDown, _: &mut Window, cx: &mut Context<Self>) {
        if self.palette_open {
            self.move_palette_cursor(PAGE_SIZE_HINT as isize, cx);
            return;
        }
        if self.any_input_focused() {
            return;
        }
        self.move_selection(PAGE_SIZE_HINT as isize, cx);
    }

    fn on_toggle_bookmark(&mut self, _: &ToggleBookmark, _: &mut Window, cx: &mut Context<Self>) {
        if self.any_input_focused() {
            return;
        }
        let Some(row_id) = self.selected_row_id else {
            self.status_message = Some("select a row first (press j/k)".into());
            cx.notify();
            return;
        };
        let is_bookmarked = self.engine.borrow_mut().toggle_bookmark(row_id, None);
        self.status_message = Some(if is_bookmarked {
            format!("bookmarked row {}", row_id.0)
        } else {
            format!("removed bookmark on row {}", row_id.0)
        });
        self.save_session();
        cx.notify();
    }

    /// Bubble-phase key handler that captures printable input into the
    /// currently-focused text input ([`InputFocus::Filter`] or
    /// [`InputFocus::Search`]). Action bindings still fire alongside this
    /// (GPUI dispatches both), but the navigation handlers each guard on
    /// [`Self::any_input_focused`] so the net effect is "keys type into
    /// the input, nothing else moves".
    fn on_key_down_capture(
        &mut self,
        event: &gpui::KeyDownEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let target = match self.input_focus {
            InputFocus::None => return,
            InputFocus::Filter => &mut self.filter_input,
            InputFocus::Search => &mut self.search_input,
        };
        let ks = &event.keystroke;
        let m = &ks.modifiers;
        if let Some(c) = keystroke_to_input_char(
            ks.key_char.as_deref(),
            m.platform,
            m.control,
            m.alt,
            m.function,
        ) {
            target.append_char(c);
            cx.notify();
        }
    }
}

impl Drop for GlowtailGpui {
    fn drop(&mut self) {
        self.save_session();
        // Non-blocking shutdown: signal each tailer and let the runtime drop
        // drive the spawned tasks to completion. The previous `block_on`
        // blocked the UI thread and panics when called from a Tokio worker.
        if let Some(live_tail) = self.live_tail.take() {
            for tailer in &live_tail.tailers {
                tailer.signal_stop();
            }
        }
    }
}

impl Render for GlowtailGpui {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.drain_live_events();
        // Honour `--follow`/default-follow on first paint: snap to the bottom
        // of whatever was preloaded so the user sees the tail instead of
        // row 0. Stays armed across no-op frames (empty file) until there's
        // something to scroll to.
        if self.pending_initial_scroll_to_bottom && self.list_state.item_count() > 0 {
            self.scroll_to_bottom();
            self.pending_initial_scroll_to_bottom = false;
        }
        let metadata = Arc::clone(&self.metadata);
        let engine = Rc::clone(&self.engine);
        // Snapshot both the selected position and a row to drive the
        // detail panel up front. The detail panel prefers the cursor row
        // and falls back to the first visible row so the panel still
        // shows something on first load before the user has pressed
        // j/k. Pre-fetching here also avoids overlapping `borrow_mut()`
        // calls with the lazy list-row render closures below — see M7.
        let selected_position = self.selected_position();
        let detail_position = selected_position.unwrap_or(0);
        let detail_row = engine.borrow_mut().present_row_at(detail_position);
        let bookmark_count = engine.borrow().session().bookmarks.len();
        let focus_handle = self
            .focus_handle
            .clone()
            .expect("focus_handle initialised in cx.new closure");
        // Grab focus once at startup so the root div receives keyboard actions.
        // We don't keep stealing it back so future focusable widgets still work.
        if !self.focused_once {
            window.focus(&focus_handle);
            self.focused_once = true;
        }
        let horizontal_offset = self.horizontal_offset_px;
        let palette = if self.palette_open {
            let commands = self.palette_commands();
            // Defensive clamp: the saved-filter list can shrink between
            // renders (e.g. a session reload), so re-anchor a stale
            // cursor instead of trusting the field blindly.
            if self.palette_cursor >= commands.len() {
                self.palette_cursor = commands.len().saturating_sub(1);
            }
            Some(palette_overlay(commands, self.palette_cursor))
        } else {
            None
        };
        div()
            .size_full()
            .relative()
            .track_focus(&focus_handle)
            .on_action(cx.listener(Self::on_scroll_up))
            .on_action(cx.listener(Self::on_scroll_down))
            .on_action(cx.listener(Self::on_page_up))
            .on_action(cx.listener(Self::on_page_down))
            .on_action(cx.listener(Self::on_scroll_home))
            .on_action(cx.listener(Self::on_scroll_end))
            .on_action(cx.listener(Self::on_scroll_left))
            .on_action(cx.listener(Self::on_scroll_right))
            .on_action(cx.listener(Self::on_scroll_line_start))
            .on_action(cx.listener(Self::on_toggle_follow))
            .on_action(cx.listener(Self::on_filter_trace))
            .on_action(cx.listener(Self::on_filter_debug))
            .on_action(cx.listener(Self::on_filter_info))
            .on_action(cx.listener(Self::on_filter_warn))
            .on_action(cx.listener(Self::on_filter_error))
            .on_action(cx.listener(Self::on_filter_fatal))
            .on_action(cx.listener(Self::on_filter_clear))
            .on_action(cx.listener(Self::on_cycle_saved_filter))
            .on_action(cx.listener(Self::on_focus_filter_input))
            .on_action(cx.listener(Self::on_focus_search_input))
            .on_action(cx.listener(Self::on_blur_input))
            .on_action(cx.listener(Self::on_submit_input))
            .on_action(cx.listener(Self::on_input_backspace))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_select_page_up))
            .on_action(cx.listener(Self::on_select_page_down))
            .on_action(cx.listener(Self::on_toggle_bookmark))
            .on_action(cx.listener(Self::on_next_search_match))
            .on_action(cx.listener(Self::on_prev_search_match))
            .on_action(cx.listener(Self::on_clear_search))
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_key_down(cx.listener(Self::on_key_down_capture))
            .bg(rgb(0x101418))
            .text_color(rgb(0xd8dee9))
            .font_family("monospace")
            .flex()
            .flex_col()
            .child(top_bar(
                &metadata,
                self.live_tail.is_some(),
                self.follow,
                self.status_message.as_deref(),
                self.engine.borrow().evicted_row_count(),
                self.current_level,
                self.current_use_filter.as_deref(),
                self.filter_input.value(),
                self.search_input.value(),
                self.input_focus,
                bookmark_count,
            ))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(source_sidebar(&metadata))
                    .child(log_viewport(
                        engine,
                        self.list_state.clone(),
                        horizontal_offset,
                        selected_position,
                    ))
                    .child(detail_panel(detail_row)),
            )
            .child(timeline_panel(&metadata))
            .children(palette)
    }
}

#[allow(clippy::too_many_arguments)]
fn top_bar(
    snapshot: &ViewportSnapshot,
    live_tail_enabled: bool,
    follow: bool,
    status_message: Option<&str>,
    evicted_count: u64,
    current_level: Option<LevelArg>,
    current_use_filter: Option<&str>,
    filter_input: &str,
    search_input: &str,
    input_focus: InputFocus,
    bookmark_count: usize,
) -> impl IntoElement {
    let mode = if live_tail_enabled { "live" } else { "static" };
    let follow_label = if !live_tail_enabled {
        "—"
    } else if follow {
        "on"
    } else {
        "off"
    };
    let status = status_message.unwrap_or(if live_tail_enabled {
        if follow {
            "following appended lines"
        } else {
            "paused — press End or f to follow"
        }
    } else {
        "loaded once"
    });
    let level_label = match current_level {
        None => "off".to_string(),
        Some(level) => format!("{level:?}").to_lowercase(),
    };
    let saved_label = current_use_filter.unwrap_or("—").to_string();

    let mut bar = div()
        .h(px(52.))
        .w_full()
        .flex()
        .items_center()
        .gap_4()
        .px_4()
        .border_b_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x151b21))
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child("glowtail-gpui"),
        )
        .child(metric("matching", snapshot.total_matching_rows))
        .child(metric("total", snapshot.total_rows))
        .child(metric("warn", snapshot.level_counts.warn))
        .child(metric(
            "error",
            snapshot.level_counts.error + snapshot.level_counts.fatal,
        ))
        .child(metric_text("mode", mode))
        .child(metric_text("follow", follow_label))
        .child(metric_string("level", level_label))
        .child(metric_string("saved", saved_label))
        .child(metric("bookmarks", bookmark_count))
        .child(filter_input_cell(
            "filter",
            filter_input,
            input_focus == InputFocus::Filter,
        ))
        .child(filter_input_cell(
            "search",
            search_input,
            input_focus == InputFocus::Search,
        ));

    if evicted_count > 0 {
        bar = bar.child(
            div()
                .text_sm()
                .text_color(rgb(0xd6a33d))
                .child(format!("truncated: -{evicted_count}")),
        );
    }

    bar.child(
        div()
            .ml_auto()
            .text_sm()
            .text_color(rgb(0x9aa7b2))
            .child(status.to_string()),
    )
}

fn metric(label: &'static str, value: usize) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .text_sm()
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(value.to_string()))
}

fn metric_text(label: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .text_sm()
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(value))
}

fn metric_string(label: &'static str, value: String) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .text_sm()
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(value))
}

/// Top-bar cell that doubles as a text input. When unfocused it shows
/// the currently-applied value as plain text (or an em-dash placeholder).
/// When focused it gains a coloured border and a trailing caret glyph
/// so the user can see they're typing into something live. The `label`
/// distinguishes the filter cell from the search cell at a glance.
fn filter_input_cell(label: &'static str, value: &str, focused: bool) -> impl IntoElement {
    let display = if value.is_empty() && !focused {
        "—".to_string()
    } else if focused {
        // Visual caret. No real cursor positioning yet (MVP appends only),
        // so a trailing block is honest about where the next char will land.
        format!("{value}▌")
    } else {
        value.to_string()
    };
    let border_color = if focused {
        rgb(0x4f9ee3)
    } else {
        rgb(0x26313b)
    };
    div()
        .flex()
        .gap_1()
        .text_sm()
        .px_2()
        .border_1()
        .border_color(border_color)
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(display))
}

fn source_sidebar(snapshot: &ViewportSnapshot) -> impl IntoElement {
    let mut panel = div()
        .w(px(240.))
        .h_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .border_r_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x111820))
        .child(div().text_lg().child("Sources"));

    for source in &snapshot.source_summaries {
        panel = panel.child(
            div()
                .rounded_md()
                .p_2()
                .bg(rgb(0x17212b))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xe6edf3))
                        .child(source.name.to_string()),
                )
                .child(div().text_xs().text_color(rgb(0x9aa7b2)).child(format!(
                    "{} rows · {} warn · {} error",
                    source.rows,
                    source.level_counts.warn,
                    source.level_counts.error + source.level_counts.fatal
                ))),
        );
    }

    panel
}

fn log_viewport(
    engine: Rc<RefCell<Engine>>,
    list_state: ListState,
    horizontal_offset: f32,
    selected_position: Option<usize>,
) -> impl IntoElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(0x0d1117))
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px_3()
                .text_xs()
                .text_color(rgb(0x9aa7b2))
                .border_b_1()
                .border_color(rgb(0x26313b))
                .child("Log viewport"),
        )
        .child(
            list(list_state, move |index, _window, _cx| {
                let row = engine.borrow_mut().present_row_at(index);
                let selected = selected_position == Some(index);
                row_element(row, index, horizontal_offset, selected)
            })
            .flex_1(),
        )
}

fn row_element(
    row: Option<RowPresentation>,
    index: usize,
    horizontal_offset: f32,
    selected: bool,
) -> gpui::AnyElement {
    let Some(row) = row else {
        return div().h(px(24.)).into_any();
    };

    // Inner content div absorbs the horizontal offset via negative left margin;
    // the outer line's `w_full()` plus the parent's `overflow_hidden()` clip
    // anything shifted past the viewport edges.
    let mut content = div()
        .flex()
        .items_center()
        .gap_1()
        .ml(px(-horizontal_offset));

    if row.is_bookmarked {
        content = content.child(div().text_color(rgb(0xdc8cff)).child("*"));
    }
    if row.folded_stack_rows > 0 {
        content = content.child(
            div()
                .text_color(rgb(0x8b949e))
                .child(format!("+{} ", row.folded_stack_rows)),
        );
    }
    if let Some(source) = row.source_name.as_ref() {
        content = content.child(
            div()
                .text_color(rgb(0x8b949e))
                .child(format!("[{source}] ")),
        );
    }

    for span in &row.spans {
        let mut span_div = div()
            .text_color(span_color(span.kind))
            .child(SharedString::from(span.text.to_string()));
        if span.kind == SpanKind::SearchMatch {
            span_div = span_div.bg(rgb(0xc9d96f));
        }
        content = content.child(span_div);
    }

    // Selected row gets a brighter background and a cyan severity-strip
    // overlay so the cursor is visible even on rows that already carry a
    // severity colour (warn/error rows stay severity-tinted on the left).
    let bg = if selected {
        rgb(0x1f2a36)
    } else if index.is_multiple_of(2) {
        rgb(0x10161d)
    } else {
        rgb(0x0d1117)
    };
    div()
        .h(px(24.))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .border_b_1()
        .border_color(if selected {
            rgb(0x4f9ee3)
        } else {
            rgb(0x1c2530)
        })
        .overflow_hidden()
        .bg(bg)
        .child(
            div()
                .w(px(4.))
                .h_full()
                .bg(severity_color(row.severity_role()))
                .mr_2(),
        )
        .child(content)
        .into_any()
}

/// Modal command palette rendered as a centered overlay. Positioned
/// absolutely (parent root is `.relative()`) so it covers the regular
/// UI without reflowing it. Highlighted row matches the row-selection
/// styling so the highlight contract is consistent across surfaces.
fn palette_overlay(commands: Vec<PaletteCommand>, cursor: usize) -> impl IntoElement {
    let mut list = div()
        .flex()
        .flex_col()
        .gap_1()
        .p_2()
        .w(px(520.))
        .max_h(px(420.))
        .overflow_hidden()
        .rounded_md()
        .bg(rgb(0x161c25))
        .border_1()
        .border_color(rgb(0x4f9ee3))
        .child(
            div()
                .px_2()
                .pb_2()
                .text_sm()
                .text_color(rgb(0x9aa7b2))
                .child("Command palette · ↑↓/jk select · enter run · esc close"),
        );
    for (i, command) in commands.into_iter().enumerate() {
        let selected = i == cursor;
        let bg = if selected {
            rgb(0x1f2a36)
        } else {
            rgb(0x10161d)
        };
        let border = if selected {
            rgb(0x4f9ee3)
        } else {
            rgb(0x1c2530)
        };
        list = list.child(
            div()
                .px_3()
                .py_1()
                .text_sm()
                .text_color(rgb(0xe6edf3))
                .bg(bg)
                .border_l_2()
                .border_color(border)
                .child(command.label()),
        );
    }
    div()
        .absolute()
        .top_0()
        .left_0()
        .right_0()
        .bottom_0()
        .flex()
        .items_center()
        .justify_center()
        .bg(rgba(0x00000080))
        .child(list)
}

fn detail_panel(selected: Option<RowPresentation>) -> impl IntoElement {
    // Show the first visible row's details. Without a click handler in
    // gpui 0.2.2, the prototype settles for "first row" semantics that match
    // the previous behaviour. The row is snapshotted in the parent `render`
    // so this function doesn't borrow the engine — see review M7.
    let mut panel = div()
        .w(px(320.))
        .h_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .border_l_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x111820))
        .child(div().text_lg().child("Details"));

    if let Some(row) = selected {
        panel = panel
            .child(detail_line("row", row.row_id.0.to_string()))
            .child(detail_line("source", row.source_id.0.to_string()));
        if let Some(level) = row.level {
            panel = panel.child(detail_line("level", format!("{level:?}")));
        }
        let fields = row.json_fields();
        if fields.is_empty() {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8b949e))
                    .child("No structured JSON fields on the first visible row."),
            );
        } else {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9aa7b2))
                    .child("JSON fields"),
            );
            for (key, value) in fields {
                panel = panel.child(detail_line(key.to_string(), value.to_string()));
            }
        }
    } else {
        panel = panel.child(
            div()
                .text_sm()
                .text_color(rgb(0x8b949e))
                .child("No rows match the current input."),
        );
    }

    panel
}

fn detail_line(label: impl Into<SharedString>, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .rounded_md()
        .p_2()
        .bg(rgb(0x17212b))
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x8b949e))
                .child(label.into()),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(0xe6edf3))
                .child(value.into()),
        )
}

fn timeline_panel(snapshot: &ViewportSnapshot) -> impl IntoElement {
    let mut row = div()
        .h(px(82.))
        .w_full()
        .flex()
        .items_end()
        .gap_1()
        .p_3()
        .border_t_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x151b21));

    if snapshot.timeline.is_empty() {
        return row
            .items_center()
            .justify_center()
            .text_color(rgb(0x8b949e))
            .child("No timestamps available for timeline")
            .into_any();
    }

    let max_total = snapshot
        .timeline
        .iter()
        .map(|bucket| bucket.total)
        .max()
        .unwrap_or(1) as f32;

    for bucket in &snapshot.timeline {
        let height = 12.0 + (bucket.total as f32 / max_total) * 52.0;
        row = row.child(div().flex_1().h(px(height)).rounded_sm().bg(
            if bucket.error_count() > 0 {
                rgb(0xdc4f4f)
            } else if bucket.warn_count() > 0 {
                rgb(0xd6a33d)
            } else {
                rgb(0x4f9ee3)
            },
        ));
    }

    row.into_any()
}

fn severity_color(role: SeverityRole) -> gpui::Rgba {
    match role {
        SeverityRole::Fatal | SeverityRole::Error => rgb(0xdc4f4f),
        SeverityRole::Warn => rgb(0xd6a33d),
        SeverityRole::Info => rgb(0x4f9ee3),
        SeverityRole::Debug | SeverityRole::Trace => rgb(0x7c75d8),
        SeverityRole::Unknown => rgb(0x4b5563),
    }
}

fn span_color(kind: SpanKind) -> gpui::Rgba {
    match kind {
        SpanKind::Timestamp => rgb(0x8ab4f8),
        SpanKind::Error => rgb(0xff7b72),
        SpanKind::Warning => rgb(0xd6a33d),
        SpanKind::SearchMatch => rgb(0x0d1117),
        SpanKind::JsonKey => rgb(0x7ee7e7),
        SpanKind::JsonValue => rgb(0xa5d6a7),
        _ => rgb(0xe6edf3),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PaletteCommand, TextInputState, keystroke_to_input_char, next_saved_filter_cycle,
        next_selected_position,
    };

    #[test]
    fn cycle_with_no_saved_filters_returns_none() {
        assert_eq!(next_saved_filter_cycle(None, 0), None);
        assert_eq!(next_saved_filter_cycle(Some(0), 0), None);
    }

    #[test]
    fn cycle_from_none_jumps_to_first() {
        assert_eq!(next_saved_filter_cycle(None, 3), Some(0));
    }

    #[test]
    fn cycle_advances_within_range() {
        assert_eq!(next_saved_filter_cycle(Some(0), 3), Some(1));
        assert_eq!(next_saved_filter_cycle(Some(1), 3), Some(2));
    }

    #[test]
    fn cycle_past_last_wraps_to_none() {
        assert_eq!(next_saved_filter_cycle(Some(2), 3), None);
    }

    #[test]
    fn cycle_with_single_saved_filter_toggles_none_and_zero() {
        assert_eq!(next_saved_filter_cycle(None, 1), Some(0));
        assert_eq!(next_saved_filter_cycle(Some(0), 1), None);
    }

    #[test]
    fn text_input_appends_and_backspaces() {
        let mut input = TextInputState::new(None);
        assert_eq!(input.value(), "");
        input.append_char('a');
        input.append_char('b');
        input.append_char('c');
        assert_eq!(input.value(), "abc");
        input.backspace();
        assert_eq!(input.value(), "ab");
    }

    #[test]
    fn text_input_backspace_on_empty_is_noop() {
        let mut input = TextInputState::new(None);
        input.backspace();
        input.backspace();
        assert_eq!(input.value(), "");
    }

    #[test]
    fn text_input_backspace_pops_whole_utf8_char() {
        // "é" is two bytes in UTF-8; backspace must not split it.
        let mut input = TextInputState::new(Some("café".into()));
        input.backspace();
        assert_eq!(input.value(), "caf");
    }

    #[test]
    fn keystroke_to_input_char_accepts_plain_chars() {
        assert_eq!(
            keystroke_to_input_char(Some("a"), false, false, false, false),
            Some('a')
        );
        assert_eq!(
            keystroke_to_input_char(Some("A"), false, false, false, false),
            Some('A')
        );
        assert_eq!(
            keystroke_to_input_char(Some("!"), false, false, false, false),
            Some('!')
        );
    }

    #[test]
    fn keystroke_to_input_char_rejects_modifier_combos() {
        // cmd-a / ctrl-c / alt-f4 should never reach the text buffer.
        assert_eq!(
            keystroke_to_input_char(Some("a"), true, false, false, false),
            None
        );
        assert_eq!(
            keystroke_to_input_char(Some("c"), false, true, false, false),
            None
        );
        assert_eq!(
            keystroke_to_input_char(Some("f"), false, false, true, false),
            None
        );
    }

    #[test]
    fn selection_empty_view_stays_none() {
        assert_eq!(next_selected_position(None, 1, 0), None);
        assert_eq!(next_selected_position(Some(0), -1, 0), None);
    }

    #[test]
    fn selection_from_none_picks_first_on_down_last_on_up() {
        assert_eq!(next_selected_position(None, 1, 5), Some(0));
        assert_eq!(next_selected_position(None, -1, 5), Some(4));
        // `delta == 0` from `None` still anchors to the first row so the
        // first-ever cursor placement is deterministic.
        assert_eq!(next_selected_position(None, 0, 5), Some(0));
    }

    #[test]
    fn selection_clamps_to_view_edges() {
        assert_eq!(next_selected_position(Some(0), -1, 5), Some(0));
        assert_eq!(next_selected_position(Some(4), 1, 5), Some(4));
        // Large deltas (e.g. Page Down) saturate at the last row instead
        // of wrapping or going off the end.
        assert_eq!(next_selected_position(Some(2), 100, 5), Some(4));
        assert_eq!(next_selected_position(Some(2), -100, 5), Some(0));
    }

    #[test]
    fn selection_moves_within_range() {
        assert_eq!(next_selected_position(Some(2), 1, 5), Some(3));
        assert_eq!(next_selected_position(Some(2), -1, 5), Some(1));
    }

    #[test]
    fn palette_command_label_includes_saved_filter_name() {
        let cmd = PaletteCommand::ApplySavedFilter("warnings".to_string());
        assert!(cmd.label().contains("warnings"));
    }

    #[test]
    fn palette_command_labels_are_user_facing() {
        // Smoke check on the static variants — these strings appear in the
        // palette UI, so an accidental rename should fail a test, not
        // ship to users.
        assert_eq!(
            PaletteCommand::ClearFilter.label(),
            "Clear filter (text and level)"
        );
        assert_eq!(PaletteCommand::ClearSearch.label(), "Clear search");
        assert_eq!(PaletteCommand::Quit.label(), "Quit");
    }

    #[test]
    fn keystroke_to_input_char_rejects_special_and_multi_char_input() {
        // Special keys (backspace, enter, escape) have no key_char on every
        // platform we target — the platform layer surfaces them through
        // `key` instead.
        assert_eq!(
            keystroke_to_input_char(None, false, false, false, false),
            None
        );
        // Composed/IME multi-grapheme: MVP doesn't accept these.
        assert_eq!(
            keystroke_to_input_char(Some("ab"), false, false, false, false),
            None
        );
        // Control characters (e.g. \r, \t) should not be appended.
        assert_eq!(
            keystroke_to_input_char(Some("\r"), false, false, false, false),
            None
        );
    }
}
