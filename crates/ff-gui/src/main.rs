//! ff-gui: a search-as-you-type window over the ff index.

// On Windows, a window program: no console window opens with it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app_icon;
mod backend;
mod canvas;
mod complete;
mod debug;
mod dialog;
mod follow;
#[cfg(windows)]
mod gdi_text;
mod i18n;
#[cfg(unix)]
mod instance;
mod keys;
#[cfg(not(windows))]
mod linux_icons;
mod png;
mod saved;
mod settings;
#[cfg(windows)]
mod shell_icons;
#[cfg(not(windows))]
mod svg;
mod tree;
#[cfg(not(windows))]
mod wayland;
#[cfg(windows)]
mod win32;
#[cfg(windows)]
mod win_tray;
#[cfg(windows)]
mod winsys;
#[cfg(not(windows))]
mod x11;
#[cfg(not(windows))]
mod x11_backend;
#[cfg(not(windows))]
mod xkb;

use std::io;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use canvas::{Canvas, Icon, Icons, ON_ACCENT, Text, palette};
use ff_core::{BuildOptions, CaseMode, Index, Kind, Query, SearchOptions, Sort, desktop, group_digits};
use dialog::{Dialog, NavItem, Row};
use i18n::{tr, trf};
use settings::Config;
use backend::{Backend, Cursor, Event, Key, Mods, Win, WindowSpec};
use keys::*;

const USAGE: &str = "\
Fast search (ff-gui) - instant file search window

USAGE:
  ff-gui [--db FILE] [QUERY...]
  ff-gui --install     add Fast search to the applications menu, with its icon
  ff-gui --shortcut on|off|KEYS   make a global shortcut open Fast search
                       (GNOME): Alt+E, or the KEYS you give, e.g. Ctrl+Alt+F
  ff-gui --hidden      start without showing the window (on Windows, in the
                       notification area): how it starts at sign-in

KEYS:
  type               search as you type (same term syntax as `ff`)
  Up/Down/PgUp/PgDn, Ctrl+Home/End   move through the results
  Shift + those, Shift+click, Ctrl+click, Ctrl+Shift+A   select several
                     results; open, copy, drag and delete act on all of them
  Left/Right/Home/End   move in the search box (Shift selects, Ctrl by word)
  Ctrl+A / Ctrl+X    select all / cut the selected search text
  Enter              open the selected file         (double-click too)
  Ctrl+Enter         show it in the file manager    (double-click its path)
  Alt+Enter          properties
  Del                move the selected result to the trash (asks first)
  Shift+Del          delete it permanently (asks first)
  F4                 open in the default text editor
  Ctrl+C             copy the selected search text, else the file itself
                     (paste it in a file manager; as text it is the path)
  Ctrl+Shift+C       copy the full path
  Ctrl+V             paste into the search box
  Alt+Down           complete a filter: ext:  size:  modified:  and their
                     usual values (also Ctrl+Space)
  drag a result      drop it on a file manager to move it there
                     (hold Ctrl to copy); also editors, upload boxes...
  Menu / right-click actions menu
  menu bar           File, Edit, View, Bookmarks, Options and Help at the top
  Ctrl+D             bookmark the current search (Bookmarks menu; Alt+1 to
                     Alt+9 open the first nine)
  Ctrl+S             export the results as a CSV file
  Ctrl+P             options
  Ctrl + / - / 0     zoom in / out / reset
  F1                 help: search syntax, filters and shortcuts with examples
  F2                 show all / files only / folders only
  F3                 case: smart / sensitive / insensitive
  Ctrl+R, Ctrl+B     regular expressions / whole words only
  F5                 rebuild the index (not needed while ffd runs)
  F6                 folder sizes: a tree and a map of what takes the space
  F7                 show or hide that map
                     (arrows and Enter open folders, Esc goes back)
  click a column     sort by it (again to reverse)
  Esc                clear the search box, or quit when it is empty

ENVIRONMENT:
  FF_GUI_SCALE       zoom factor, e.g. 1.5 (default: the desktop scale)
  FF_GUI_FONT        font file to use (default: the system sans-serif)
  FF_GUI_DEBUG       file to append drag-and-drop diagnostics to
  FF_GUI_TITLEBAR    set to system to use the desktop title bar
                     instead of the built-in one (the menu bar, with the
                     icon and the window buttons)
";

/// Text height in pixels at 100% scale.
const FONT_PX: f32 = 14.5;
/// Side of the resize grip in the bottom-right corner, in layout units.
const GRIP: i32 = 16;
/// The main window's title (on Windows also how a second start finds it).
pub(crate) const WINDOW_TITLE: &str = "Fast search";
/// Width of each window button (minimize, maximize, close) in our title
/// bar, in layout units, as Windows draws them.
const TITLE_BUTTON: i32 = 46;

// The Windows 11 (Fluent) palette, light values; see `canvas::palette`.
const WHITE: u32 = palette::FIELD;
const PANEL: u32 = palette::BASE;
const LINE: u32 = palette::STROKE;
const TEXT: u32 = palette::INK;
const DIM: u32 = palette::DIM_INK;
const SELECT: u32 = canvas::ACCENT;
const HOVER: u32 = palette::HOVER;

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Open,
    Reveal,
    /// Open in the default text editor.
    Edit,
    /// Copy the file itself, to paste it in a file manager.
    CopyFile,
    CopyPath,
    CopyName,
    Properties,
    /// Ask, then move to the trash / delete for good.
    Trash,
    DeleteForever,
    /// The answers from those confirmation dialogs.
    ConfirmTrash,
    ConfirmDelete,
    ToggleRegex,
    ToggleWord,
    /// A page of the help sheet: 0 search, 1 filters, 2 shortcuts.
    Help(u8),
    /// A page of the Options window: 0 interface, 1 search, 2 results,
    /// 3 view, 4 desktop, 5 live updates, 6 drives.
    Options(u8),
    Toggle(Flag),
    /// Colours: 0 follow the desktop, 1 light, 2 dark.
    Theme(u8),
    /// Start at sign-in, in the notification area (Windows).
    ToggleAutostart,
    /// 0: the desktop's accent colour; n: canvas::ACCENTS[n - 1].
    Accent(u8),
    /// 0 follow the system, 1 English, 2 French, 3 Spanish, 4 Italian.
    Language(u8),
    /// How sizes are written: 0 automatic, 1 bytes, 2 KB, 3 MB.
    SizeFormat(u8),
    /// The Drives page: its checkboxes (by drive number), and Apply.
    DriveIndex(u8),
    DriveLive(u8),
    ApplyDrives,
    /// The Options window's OK and Apply: save the index settings.
    OptionsOk,
    OptionsApply,
    /// The Exclude page.
    ToggleExcludeHidden,
    SelectExclude(u8),
    RemoveExclude,
    /// Adds the folder typed into the text field / selected in the results.
    AddExclude,
    AddSelectedExclude,
    ApplyExcludes,
    /// Picks the drive whose settings are shown.
    SelectDrive(u8),
    ToggleAutoDrives,
    Cut,
    Copy,
    Paste,
    SelectAll,
    SelectAllResults,
    Show(Kind),
    Case(CaseMode),
    Reindex,
    /// The folder sizes view.
    TreeSize,
    /// Bookmarks: ask for a name, save, open one, and the Options page.
    AddBookmark,
    SaveBookmark,
    Bookmark(u8),
    SelectBookmark(u8),
    RaiseBookmark,
    RemoveBookmark,
    /// Write the results to a CSV file: ask where, then do it.
    ExportCsv,
    ConfirmExport,
    About,
    /// Install the newest release, if there is one.
    Update,
    Quit,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    ToggleTitleBar,
    ToggleShortcut,
    /// Use the keys typed in the Options window as the global shortcut.
    SetShortcut,
    ToggleLauncher,
    ToggleDaemon,
}

/// An index being built on another thread.
struct Building {
    /// The new index (already saved), or why there is none.
    rx: mpsc::Receiver<Result<Index, String>>,
    /// Entries found so far.
    found: Arc<AtomicUsize>,
    started: Instant,
    /// Entries expected (the last index's), 0 if unknown.
    expected: usize,
    /// Paths removed from the window meanwhile: the walk may have seen
    /// them before they went, so they are checked again at the end.
    forgotten: Vec<PathBuf>,
}

/// An on/off option, in the order of `FLAGS`.
#[derive(Clone, Copy, PartialEq)]
enum Flag {
    HideEmpty,
    CloseOnOpen,
    PathDoubleClick,
    DateDescFirst,
    SizeDescFirst,
    QuotePaths,
    AltRows,
    RowHover,
    SelectedInStatus,
    WildcardWholeName,
    MatchPath,
    /// Closing hides the window to the notification area (Windows).
    CloseToTray,
    /// File icons from the system (Explorer's, or the icon theme), not ours.
    SystemIcons,
    /// The map of the selected folder beside the folder sizes tree.
    SizeMap,
}

/// Each option's name in the settings file and its default.
const FLAGS: [(&str, bool); 14] = [
    ("hide_results_when_empty", false),
    ("close_on_open", false),
    ("path_double_click", true),
    ("date_descending_first", false),
    ("size_descending_first", false),
    ("quote_copied_paths", false),
    ("alternate_rows", false),
    ("row_mouseover", false),
    ("selected_in_status_bar", false),
    ("wildcards_whole_name", true),
    ("match_path", false),
    ("close_to_tray", true),
    ("system_icons", true),
    ("show_size_map", true),
];

/// Menu entries: label, shortcut hint, action.
type Items = &'static [(&'static str, &'static str, Action)];

/// The right-click menu of a result.
const CONTEXT_MENU: Items = &[
    ("Open", "Enter", Action::Open),
    ("Open folder", "Ctrl+Enter", Action::Reveal),
    ("Copy", "Ctrl+C", Action::CopyFile),
    ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
    ("Copy name", "", Action::CopyName),
    ("Move to trash", "Del", Action::Trash),
    ("Properties", "Alt+Enter", Action::Properties),
];

/// The same, for files that make sense in a text editor.
const TEXT_CONTEXT_MENU: Items = &[
    ("Open", "Enter", Action::Open),
    ("Open in text editor", "F4", Action::Edit),
    ("Open folder", "Ctrl+Enter", Action::Reveal),
    ("Copy", "Ctrl+C", Action::CopyFile),
    ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
    ("Copy name", "", Action::CopyName),
    ("Move to trash", "Del", Action::Trash),
    ("Properties", "Alt+Enter", Action::Properties),
];

/// The menu bar.
const BAR: [(&str, Items); 7] = [
    (
        "File",
        &[
            ("Open", "Enter", Action::Open),
            ("Open in text editor", "F4", Action::Edit),
            ("Open folder", "Ctrl+Enter", Action::Reveal),
            ("Copy full path", "Ctrl+Shift+C", Action::CopyPath),
            ("Copy name", "", Action::CopyName),
            ("Properties", "Alt+Enter", Action::Properties),
            ("Export results as CSV...", "Ctrl+S", Action::ExportCsv),
            ("Rebuild index", "F5", Action::Reindex),
            ("Quit", "Ctrl+Q", Action::Quit),
        ],
    ),
    (
        "Edit",
        &[
            ("Cut", "Ctrl+X", Action::Cut),
            ("Copy", "Ctrl+C", Action::Copy),
            ("Paste", "Ctrl+V", Action::Paste),
            ("Select all text", "Ctrl+A", Action::SelectAll),
            ("Select all results", "Ctrl+Shift+A", Action::SelectAllResults),
            ("Move to trash", "Del", Action::Trash),
            ("Delete permanently", "Shift+Del", Action::DeleteForever),
        ],
    ),
    (
        "View",
        &[
            ("All results", "F2", Action::Show(Kind::Any)),
            ("Files only", "", Action::Show(Kind::File)),
            ("Folders only", "", Action::Show(Kind::Dir)),
            ("Smart case", "F3", Action::Case(CaseMode::Smart)),
            ("Case sensitive", "", Action::Case(CaseMode::Sensitive)),
            ("Case insensitive", "", Action::Case(CaseMode::Insensitive)),
            ("Regular expressions", "Ctrl+R", Action::ToggleRegex),
            ("Whole words only", "Ctrl+B", Action::ToggleWord),
            ("Folder sizes", "F6", Action::TreeSize),
            ("Size map", "F7", Action::Toggle(Flag::SizeMap)),
        ],
    ),
    // Filled in from canvas::THEMES when it opens.
    ("Theme", &[]),
    // Filled in from the saved bookmarks when it opens.
    ("Bookmarks", &[]),
    (
        "Options",
        &[
            ("Options...", "Ctrl+P", Action::Options(0)),
            ("Drives...", "", Action::Options(6)),
            ("Zoom in", "Ctrl++", Action::ZoomIn),
            ("Zoom out", "Ctrl+-", Action::ZoomOut),
            ("Reset zoom", "Ctrl+0", Action::ZoomReset),
            ("Menus in the title bar", "", Action::ToggleTitleBar),
            ("Global shortcut", "", Action::ToggleShortcut),
            ("Show in applications menu", "", Action::ToggleLauncher),
            ("Live updates service", "asks for password", Action::ToggleDaemon),
        ],
    ),
    (
        "Help",
        &[
            ("Search syntax", "F1", Action::Help(0)),
            ("Filters", "", Action::Help(1)),
            ("Keyboard shortcuts", "", Action::Help(2)),
            ("Check for updates", "", Action::Update),
            ("About Fast search", "", Action::About),
        ],
    ),
];

/// "System" (follow the desktop) and every theme of `canvas::THEMES`, as
/// menu entries and preferences rows: built once.
fn theme_choices() -> Items {
    static CHOICES: std::sync::OnceLock<Vec<(&'static str, &'static str, Action)>> = std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        let themes = canvas::THEMES.iter().enumerate().map(|(i, t)| (t.name, "", Action::Theme(i as u8 + 1)));
        std::iter::once(("System", "", Action::Theme(0))).chain(themes).collect()
    })
}

/// The desktop's accent colour and every one of `canvas::ACCENTS`, as
/// menu entries: built once.
fn accent_choices() -> Items {
    static CHOICES: std::sync::OnceLock<Vec<(&'static str, &'static str, Action)>> = std::sync::OnceLock::new();
    CHOICES.get_or_init(|| {
        let accents = canvas::ACCENTS.iter().enumerate().map(|(i, a)| (a.0, "", Action::Accent(i as u8 + 1)));
        std::iter::once(("System accent", "", Action::Accent(0))).chain(accents).collect()
    })
}

/// The Theme menu: the themes, then the accent colours.
fn theme_menu() -> Items {
    static MENU: std::sync::OnceLock<Vec<(&'static str, &'static str, Action)>> = std::sync::OnceLock::new();
    MENU.get_or_init(|| theme_choices().iter().chain(accent_choices()).copied().collect())
}

/// An open menu: a result's right-click menu or one from the menu bar.
struct Menu {
    x: i32,
    y: i32,
    hover: Option<usize>,
    items: Items,
    /// Which menu-bar title it hangs from, if any.
    bar: Option<usize>,
}

struct App {
    /// The display server we are talking to.
    b: Box<dyn Backend>,
    /// A press on a result, until it is released or becomes a drag.
    drag_arm: Option<DragArm>,
    /// A drag is in progress; what it is called in messages.
    dragging: Option<String>,
    /// The files of the last drag, to forget them if they were moved away.
    drag_paths: String,
    /// The dragged files may not be moved away, only copied.
    drag_copy_only: bool,
    cv: Canvas,
    text: Text,
    icons: Icons,
    /// Pixels per layout unit (the rounded desktop scale).
    u: i32,
    /// Widths of the Name, Path, Size and Modified columns chosen by dragging
    /// their edges, at a scale of 1; None: automatic.
    column_widths: Option<[f32; 4]>,
    /// The header edge being dragged: 1 after Name ... 4 after Modified.
    column_drag: Option<usize>,
    size_w: i32,
    date_w: i32,

    db: PathBuf,
    /// Modification time of the index file when we loaded it.
    db_stamp: Option<SystemTime>,
    index: Index,
    query: String,
    /// Caret and selection anchor in the search box (byte offsets).
    cursor: usize,
    anchor: usize,
    /// Pixels the search text is scrolled left.
    box_scroll: i32,
    selecting: bool,
    last_box_click: u32,
    hits: Vec<u32>,
    /// The current result row (where the keyboard is).
    sel: usize,
    /// Extra selected rows; empty means just `sel` is selected.
    marks: BTreeSet<usize>,
    /// Where a Shift-selection started.
    anchor_row: usize,
    /// A selected row was pressed: if no drag follows, it becomes the
    /// only selected one when the button is released.
    collapse_on_release: Option<usize>,
    top: usize,
    kind: Kind,
    case: CaseMode,
    regex: bool,
    whole_word: bool,
    /// What the search engine could not understand in the query, if anything.
    query_error: Option<String>,
    sort: Sort,
    reverse: bool,
    status: String,
    menu: Option<Menu>,
    /// Open properties panel: label/value rows.
    props: Option<Vec<(&'static str, String)>>,
    about: bool,
    about_icon: Icons,
    quit: bool,

    /// Current zoom, and the desktop's own scale that "reset" returns to.
    scale: f32,
    auto_scale: f32,
    /// Follow the default index location (no --db given).
    follow_default: bool,
    /// Results of background jobs, shown in the status bar.
    notices: mpsc::Receiver<String>,
    notice_tx: mpsc::Sender<String>,
    /// State of the Options toggles, refreshed when the menu opens.
    shortcut_on: bool,
    /// The keys of the global shortcut, as in "Ctrl+Alt+F".
    shortcut_keys: String,
    launcher_on: bool,
    daemon_on: bool,
    /// We draw the title bar ourselves (so it can show the app icon).
    custom_title: bool,
    /// Title-bar button under the pointer: 0 minimize, 1 maximize, 2 close.
    hover_button: Option<usize>,
    last_title_click: u32,
    dragging_scrollbar: bool,

    /// The open dialog window, if any; it is modal.
    dialog: Option<Dialog>,
    /// The on/off options, indexed by `Flag`.
    flags: [bool; FLAGS.len()],
    size_format: u8,
    theme: u8,
    accent: u8,
    language: u8,
    /// Row under the pointer, for the mouseover highlight.
    hover_row: Option<usize>,
    /// Text of the open dialog's text field.
    dialog_input: String,
    /// The Drives and Exclude pages were filled from the current
    /// settings since the Options window opened (they then hold edits).
    index_settings_loaded: bool,
    /// The Drives or Exclude page has changes that are not saved yet.
    index_settings_dirty: bool,
    /// Folders left out of the index, as edited on the Exclude page.
    excludes: Vec<String>,
    exclude_selected: usize,
    exclude_hidden: bool,
    /// The page of the Options window last shown.
    options_page: Option<u8>,
    /// Options for the next rebuild, when they differ from the last one's.
    pending_build: Option<BuildOptions>,
    /// An index being built in the background, shown with its progress.
    building: Option<Building>,
    /// The drives as shown (and edited) on the Drives screen.
    drives: Vec<settings::Drive>,
    drive_selected: usize,
    /// Mounted drives are indexed without being asked.
    auto_drives: bool,
    /// The files a confirmation dialog is asking about.
    pending_paths: Vec<PathBuf>,
    /// The index entries of `pending_paths`, in the same order.
    pending_ids: Vec<u32>,
    /// A delete running in the background: its result arrives here.
    deleting: Option<mpsc::Receiver<Deleted>>,
    /// Without the ffd daemon: the window follows changes to the drives
    /// itself.
    follow: Option<follow::Follow>,
    /// The index changed while the window was hidden: search again when
    /// it shows.
    results_stale: bool,
    /// When the folder sizes view was last redone for followed changes.
    tree_refreshed: Instant,
    /// That drop was a move, so the files leave the results when it ends.
    drop_moved: bool,
    last_click: (u32, usize),
    /// The folder sizes view, while it is open instead of the results.
    tree: Option<tree::Tree>,
    /// The tiles of its map as last drawn, and the one under the pointer.
    tiles: Vec<tree::Tile>,
    /// Per root of the folder sizes tree: its drive's capacity and free
    /// space, where known.
    disks: Vec<(u32, (u64, u64))>,
    /// The window is hidden, the program still running (closed with
    /// "keep running", or started at sign-in).
    hidden: bool,
    /// Requests from later starts of the program to show the window.
    #[cfg(unix)]
    wake: Option<mpsc::Receiver<()>>,
    tile_hover: Option<u32>,
    /// The list of completions open under the search box.
    complete: Option<complete::Complete>,
    /// Saved searches, the one selected on their Options page, and the
    /// Bookmarks menu made from them.
    bookmarks: Vec<ff_core::bookmarks::Bookmark>,
    bookmark_selected: usize,
    bookmark_items: Items,

    /// The search windows other than the one in the fields above, by
    /// number (None: closed, or the current one). See `Session`.
    windows: Vec<Option<Session>>,
    /// The number of the search window whose state is in the fields.
    current: u8,
    /// The current window has changes not drawn yet (kept for each window
    /// while another is current).
    pending_draw: bool,
    /// Something every window shows changed (theme, scale, language, a
    /// new index): all are drawn again.
    redraw_all: bool,
    /// The search running in the background, if any, and searches given
    /// up on (a later key made them useless) that are still finishing.
    search_job: Option<SearchJob>,
    dropped_searches: Vec<(Arc<AtomicBool>, std::thread::JoinHandle<()>)>,
    /// The current window's results are out of date: a search waits.
    search_pending: bool,
    /// When the user last typed, clicked, scrolled or moved the pointer:
    /// followed file changes wait until they pause, so dragging and typing
    /// stay smooth.
    last_input: Instant,
}

/// A search on a thread of its own, so typing never waits for one: the
/// window keeps showing the last results until these arrive.
struct SearchJob {
    window: u8,
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<Option<Vec<u32>>>,
    handle: std::thread::JoinHandle<()>,
}

/// The index, lent to a search thread. Sound because the index is never
/// changed, replaced or dropped while a search thread runs: every change
/// first stops them all (`App::quiesce`), and `App` stops them when it
/// is dropped.
struct IndexRef(*const Index);

// SAFETY: see above; the index is only read through it.
unsafe impl Send for IndexRef {}

impl Drop for App {
    fn drop(&mut self) {
        self.quiesce();
    }
}

/// Declares the state each search window has of its own: `Session`
/// holds it for the windows not current, and `App::swap_session`
/// exchanges it with the fields of `App`, which always hold the current
/// window's. The index, settings, dialog and background work are shared
/// by all windows (one engine, as in Everything).
macro_rules! per_window {
    ($($field:ident: $ty:ty,)*) => {
        struct Session {
            $($field: $ty,)*
        }

        impl App {
            fn swap_session(&mut self, s: &mut Session) {
                $(std::mem::swap(&mut self.$field, &mut s.$field);)*
            }
        }
    };
}

per_window! {
    cv: Canvas,
    drag_arm: Option<DragArm>,
    column_drag: Option<usize>,
    query: String,
    cursor: usize,
    anchor: usize,
    box_scroll: i32,
    selecting: bool,
    last_box_click: u32,
    hits: Vec<u32>,
    sel: usize,
    marks: BTreeSet<usize>,
    anchor_row: usize,
    collapse_on_release: Option<usize>,
    top: usize,
    kind: Kind,
    case: CaseMode,
    regex: bool,
    whole_word: bool,
    query_error: Option<String>,
    sort: Sort,
    reverse: bool,
    status: String,
    menu: Option<Menu>,
    props: Option<Vec<(&'static str, String)>>,
    about: bool,
    hover_button: Option<usize>,
    last_title_click: u32,
    dragging_scrollbar: bool,
    hover_row: Option<usize>,
    results_stale: bool,
    tree_refreshed: Instant,
    last_click: (u32, usize),
    tree: Option<tree::Tree>,
    tiles: Vec<tree::Tile>,
    disks: Vec<(u32, (u64, u64))>,
    tile_hover: Option<u32>,
    complete: Option<complete::Complete>,
    pending_draw: bool,
    search_pending: bool,
}

fn main() -> ExitCode {
    // A window program has no console of its own: write to the one it
    // was started from, if any, and show fatal errors in a message box.
    #[cfg(windows)]
    winsys::attach_console();
    run()
}

fn run() -> ExitCode {
    let mut db = None;
    // Started at sign-in: to the notification area, no window.
    let mut hidden = false;
    let mut query = Vec::new();
    let mut args = std::env::args();
    args.next();
    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("ff-gui {}", ff_core::update::VERSION);
                return ExitCode::SUCCESS;
            }
            // For packaging: the icon as an .ico file (the Windows installer uses it).
            "--write-icon" => {
                let Some(file) = args.next() else { return ExitCode::from(2) };
                return match std::fs::write(&file, app_icon::ico()) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(e) => {
                        eprintln!("ff-gui: {file}: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            "--hidden" => hidden = true,
            "--db" => db = args.next().map(PathBuf::from),
            "--shortcut" => {
                // "on" uses the keys chosen before (Alt+E at first).
                let mut config = Config::load();
                let saved = config.more.get("shortcut").cloned().unwrap_or_else(|| settings::DEFAULT_SHORTCUT.into());
                let (on, keys) = match args.next().as_deref() {
                    Some("on") => (true, saved),
                    Some("off") => (false, saved),
                    Some(keys) => match settings::shortcut_binding(keys) {
                        Ok(binding) => (true, settings::shortcut_label(&binding)),
                        Err(e) => {
                            eprintln!("ff-gui: {e}");
                            return ExitCode::from(2);
                        }
                    },
                    None => {
                        eprintln!("ff-gui: --shortcut takes on, off or the keys, such as Ctrl+Alt+F");
                        return ExitCode::from(2);
                    }
                };
                return match settings::set_shortcut(on, &keys) {
                    Ok(true) => {
                        config.more.insert("shortcut".into(), keys.clone());
                        let _ = config.save();
                        println!("{keys} now opens Fast search");
                        ExitCode::SUCCESS
                    }
                    Ok(false) => {
                        println!("global shortcut removed");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ff-gui: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            #[cfg(not(windows))]
            "--hold-wayland-clipboard" => {
                return match wayland::hold_clipboard() {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                };
            }
            #[cfg(not(windows))]
            "--hold-clipboard" => {
                return match x11_backend::hold_clipboard(args.next().as_deref() == Some("file")) {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(_) => ExitCode::FAILURE,
                };
            }
            "--install" => {
                return match settings::install_launcher() {
                    Ok(file) => {
                        println!("installed {}", file.display());
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("ff-gui: {e}");
                        ExitCode::FAILURE
                    }
                };
            }
            _ => query.push(a),
        }
    }
    // Already running (perhaps hidden in the notification area): show that
    // window, which has the index loaded, instead of starting again; a
    // hidden start (at sign-in) then has nothing to do. Test runs, which
    // draw one frame and exit, always start their own.
    #[cfg(windows)]
    if std::env::var_os("FF_GUI_SNAPSHOT").is_none() && std::env::var_os("FF_GUI_TEST").is_none() {
        if hidden && win_tray::running(WINDOW_TITLE) {
            return ExitCode::SUCCESS;
        }
        if !hidden && win_tray::wake_running(WINDOW_TITLE) {
            return ExitCode::SUCCESS;
        }
    }
    #[cfg(unix)]
    if std::env::var_os("FF_GUI_SNAPSHOT").is_none() && std::env::var_os("FF_GUI_TEST").is_none() {
        if hidden && instance::running() {
            return ExitCode::SUCCESS;
        }
        if !hidden && instance::wake_running() {
            return ExitCode::SUCCESS;
        }
    }
    let follow_default = db.is_none();
    let Some(db) = db.or_else(ff_core::default_db_path) else {
        eprintln!("ff-gui: HOME is not set; use --db");
        return ExitCode::from(2);
    };
    // Mends a shortcut saved wrongly by an earlier version, out of the way.
    std::thread::spawn(settings::repair_shortcut);
    // Changes followed since the index was last written are kept for the
    // next start.
    match App::new(db, follow_default, query.join(" "), hidden).and_then(|mut app| {
        let result = app.run();
        app.save_followed();
        result
    }) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ff-gui: {e}");
            #[cfg(windows)]
            winsys::error_box(&format!("ff-gui: {e}"));
            ExitCode::FAILURE
        }
    }
}

/// Asks the kernel whether we may use a file in a way (`mode`: 4 read,
/// 2 write, 1 search). `None` if that could not be told, for instance
/// because the file is gone: the action itself then reports it.
#[cfg(unix)]
fn access(path: &std::path::Path, mode: i32) -> Option<bool> {
    use std::os::unix::ffi::OsStrExt;
    unsafe extern "C" {
        fn access(path: *const std::ffi::c_char, mode: std::ffi::c_int) -> std::ffi::c_int;
    }
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    if unsafe { access(path.as_ptr(), mode) } == 0 {
        return Some(true);
    }
    match io::Error::last_os_error().kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => Some(false),
        _ => None,
    }
}

/// False if we may not open the file, or list the folder.
#[cfg(unix)]
fn may_read(path: &std::path::Path) -> bool {
    let mode = if path.is_dir() { 4 | 1 } else { 4 };
    access(path, mode) != Some(false)
}

/// On Windows access is decided by access control lists, which only an
/// attempt tells reliably: a folder that refuses to be listed says no.
#[cfg(windows)]
fn may_read(path: &std::path::Path) -> bool {
    let denied = |r: io::Result<()>| r.is_err_and(|e| e.kind() == io::ErrorKind::PermissionDenied);
    if path.is_dir() { !denied(std::fs::read_dir(path).map(drop)) } else { !denied(std::fs::File::open(path).map(drop)) }
}

/// On Windows: anything but a drive's root; a refusal shows when the
/// action is tried.
#[cfg(windows)]
fn may_delete(path: &std::path::Path) -> bool {
    path.parent().is_some()
}

/// False if we may not remove the file from its folder (to delete it,
/// trash it or move it). That is decided by the folder, not the file;
/// in a "sticky" folder such as /tmp only the owner may.
#[cfg(unix)]
fn may_delete(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Some(folder) = path.parent() else { return false }; // the root itself
    if access(folder, 2 | 1) == Some(false) {
        return false;
    }
    // A folder is moved by rewriting its own link to the folder above,
    // and emptied before it is deleted: both need write access to it.
    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) && access(path, 2 | 1) == Some(false) {
        return false;
    }
    let (Ok(inside), Ok(holder)) = (path.symlink_metadata(), folder.metadata()) else { return true };
    let me = std::fs::metadata("/proc/self").map_or(holder.uid(), |m| m.uid());
    holder.mode() & 0o1000 == 0 || me == 0 || me == inside.uid() || me == holder.uid()
}

/// A typed folder without trailing separators, except where one is part
/// of a root (`/`, `C:\`).
fn trim_folder(typed: &str) -> String {
    let trimmed = typed.trim_end_matches(['/', std::path::MAIN_SEPARATOR]);
    if trimmed.is_empty() || trimmed.ends_with(':') {
        return typed.to_string();
    }
    trimmed.to_string()
}

fn debug(msg: impl AsRef<str>) {
    debug::log(msg);
}

/// A press on a result that may turn into a drag once the pointer moves.
struct DragArm {
    /// Where the button went down.
    start: (i32, i32),
    /// The paths to drag, one per line.
    paths: String,
    icon: Icon,
}

/// What a drag is called in messages: a file name, or "5 items".
fn drag_label(paths: &str) -> String {
    match paths.lines().count() {
        1 => paths.rsplit(['/', std::path::MAIN_SEPARATOR]).next().unwrap_or(paths).to_string(),
        n => trf("{} items", &[&n]),
    }
}

impl App {
    fn new(db: PathBuf, follow_default: bool, query: String, hidden: bool) -> io::Result<App> {
        let config = Config::load();
        let (notice_tx, notices) = mpsc::channel();
        // FF_GUI_SCALE beats the saved zoom, which beats the desktop scale.
        let env_scale = std::env::var("FF_GUI_SCALE").ok().and_then(|v| v.parse::<f32>().ok());
        let fixed = env_scale.or(config.scale).map(|s| s.clamp(0.5, 8.0));
        let mut b = backend::connect(&WindowSpec { title: WINDOW_TITLE, width: 1000, height: 640, scale: fixed, hidden })?;
        let auto_scale = if env_scale.is_some() { env_scale.unwrap() } else { b.scale() };
        debug(format!("display: {} at scale {auto_scale}", b.name()));
        let scale = fixed.unwrap_or(auto_scale);
        let u = (scale.round() as i32).max(1);
        let mut text = Text::load((FONT_PX * scale).round())?;
        let custom_title = std::env::var("FF_GUI_TITLEBAR").as_deref() != Ok("system") && config.own_title_bar;

        let mut app = App {
            b,
            drag_arm: None,
            dragging: None,
            drag_paths: String::new(),
            drag_copy_only: false,
            cv: Canvas::new(),
            column_widths: config.more.get("column_widths").and_then(|v| {
                let w: Vec<f32> = v.split(',').filter_map(|n| n.trim().parse().ok()).collect();
                (w.len() == 4 && w.iter().all(|&n| n > 0.0)).then(|| [w[0], w[1], w[2], w[3]])
            }),
            column_drag: None,
            size_w: text.width("000,000,000 KB") + 16 * u,
            date_w: text.width("0000-00-00 00:00") + 16 * u,
            text,
            icons: Icons::new((16 * u) as usize),
            u,
            index: Index::load(&db).unwrap_or_default(),
            db_stamp: std::fs::metadata(&db).and_then(|m| m.modified()).ok(),
            db,
            cursor: query.len(),
            anchor: query.len(),
            box_scroll: 0,
            selecting: false,
            last_box_click: 0,
            query,
            hits: Vec::new(),
            sel: 0,
            marks: BTreeSet::new(),
            anchor_row: 0,
            collapse_on_release: None,
            top: 0,
            kind: Kind::Any,
            case: match config.case.as_str() {
                "sensitive" => CaseMode::Sensitive,
                "insensitive" => CaseMode::Insensitive,
                _ => CaseMode::Smart,
            },
            regex: config.regex,
            whole_word: config.whole_word,
            query_error: None,
            sort: Sort::None,
            reverse: false,
            status: String::new(),
            menu: None,
            props: None,
            about: false,
            about_icon: Icons::new((72 * u) as usize),
            quit: false,
            scale,
            auto_scale,
            follow_default,
            notices,
            notice_tx,
            shortcut_on: false,
            shortcut_keys: config.more.get("shortcut").cloned().unwrap_or_else(|| settings::DEFAULT_SHORTCUT.into()),
            launcher_on: false,
            daemon_on: false,
            custom_title,
            hover_button: None,
            last_title_click: 0,
            dragging_scrollbar: false,
            dialog: None,
            flags: FLAGS.map(|(key, default)| config.more.get(key).map_or(default, |v| v == "on")),
            size_format: config.more.get("size_format").and_then(|v| v.parse().ok()).filter(|f| *f < 4).unwrap_or(2),
            theme: config.more.get("theme").and_then(|v| v.parse().ok()).filter(|&t: &u8| (t as usize) <= canvas::THEMES.len()).unwrap_or(0),
            accent: config.more.get("accent").and_then(|v| v.parse().ok()).filter(|&a: &u8| (a as usize) <= canvas::ACCENTS.len()).unwrap_or(0),
            language: config.more.get("language").and_then(|v| v.parse().ok()).filter(|l| *l < 5).unwrap_or(0),
            hover_row: None,
            dialog_input: String::new(),
            index_settings_loaded: false,
            index_settings_dirty: false,
            excludes: Vec::new(),
            exclude_selected: 0,
            exclude_hidden: false,
            options_page: None,
            drives: Vec::new(),
            pending_build: None,
            building: None,
            tree: None,
            tiles: Vec::new(),
            disks: Vec::new(),
            hidden,
            #[cfg(unix)]
            wake: (std::env::var_os("FF_GUI_SNAPSHOT").is_none() && std::env::var_os("FF_GUI_TEST").is_none())
                .then(instance::listen)
                .flatten(),
            tile_hover: None,
            complete: None,
            bookmarks: ff_core::bookmarks::load(),
            bookmark_selected: 0,
            bookmark_items: &[],
            drive_selected: 0,
            auto_drives: true,
            pending_paths: Vec::new(),
            pending_ids: Vec::new(),
            deleting: None,
            follow: None,
            results_stale: false,
            tree_refreshed: Instant::now(),
            windows: vec![None],
            current: 0,
            pending_draw: false,
            redraw_all: false,
            search_job: None,
            dropped_searches: Vec::new(),
            search_pending: false,
            last_input: Instant::now(),
            drop_moved: false,
            last_click: (0, usize::MAX),
        };
        app.apply_title_bar();
        app.apply_theme();
        app.apply_hotkey();
        // Started at sign-in: Windows made no window visible; elsewhere it
        // is hidden now.
        #[cfg(unix)]
        if app.hidden {
            app.hidden = app.b.hide_to_tray();
        }
        app.apply_language();
        app.cv.resize((1000 * u) as usize, (640 * u) as usize);
        Ok(app)
    }

    fn run(&mut self) -> io::Result<()> {
        self.rebuild_bookmark_menu();
        let result = self.event_loop();
        self.b.keep_clipboard();
        result
    }

    fn event_loop(&mut self) -> io::Result<()> {
        if self.index.is_empty() {
            self.reindex()?; // first start: no index yet
            // The test hooks below look at the first frame: give them the
            // finished index, not its progress.
            if std::env::var_os("FF_GUI_TEST").is_some() || std::env::var_os("FF_GUI_SNAPSHOT").is_some() {
                self.finish_build(true);
            }
        }
        self.search();
        // Test hooks: open a panel, save the first frame, exit.
        match std::env::var("FF_GUI_TEST").as_deref() {
            Ok("menu") => {
                let (x, y) = (300 * self.u, 150 * self.u);
                self.menu = Some(Menu { x, y, hover: Some(1), items: self.context_items(), bar: None });
            }
            Ok("bar") => {
                self.open_bar_menu(2);
                self.menu.as_mut().unwrap().hover = Some(1);
            }
            Ok("themes") => self.open_bar_menu(3),
            Ok("about") => self.about = true,
            // Copy as a real key press would, then quit: the copy must survive.
            Ok("copy") => self.act(Action::CopyFile, 123_456_789),
            Ok("select3") => self.marks = (0..3.min(self.hits.len())).collect(),
            Ok("confirm3") => {
                self.marks = (0..3.min(self.hits.len())).collect();
                self.act(Action::Trash, 0);
            }
            Ok("copy3-quit") => {
                self.marks = (0..3.min(self.hits.len())).collect();
                self.act(Action::CopyFile, 0);
                return Ok(());
            }
            Ok("confirm") => self.act(Action::Trash, 0),
            Ok("confirm-forever") => self.act(Action::DeleteForever, 0),
            Ok("prefs") => self.act(Action::Options(0), 0),
            Ok("options1") => self.act(Action::Options(1), 0),
            Ok("options2") => self.act(Action::Options(2), 0),
            Ok("options3") => self.act(Action::Options(3), 0),
            Ok("options4") => self.act(Action::Options(4), 0),
            Ok("options8") => self.act(Action::Options(8), 0),
            Ok("options9") => self.act(Action::Options(9), 0),
            Ok("options5") => self.act(Action::Options(5), 0),
            Ok("options7") => self.act(Action::Options(7), 0),
            Ok("bookmark1") => self.act(Action::Bookmark(0), 0),
            Ok("drives") => self.act(Action::Options(6), 0),
            Ok("apply-preview") => {
                // Ticks the first unmounted NTFS drive and prints the daemon
                // command Apply would run (tests only; nothing is executed).
                self.load_index_settings();
                if let Some(d) = self.drives.iter_mut().find(|d| d.mount.is_empty() && d.fstype == "ntfs") {
                    d.index = true;
                }
                let drives: Vec<_> = self.drives.iter().map(|d| (d.path.clone(), !d.mount.is_empty(), d.index, d.live)).collect();
                let args = settings::configure_args(&drives, self.auto_drives, &self.excludes, self.exclude_hidden);
                eprintln!("pkexec ffd {}", args.join(" "));
                return Ok(());
            }
            Ok("exclude-apply") => {
                // Excludes hidden names and the folder given in FF_GUI_TEST_DIR,
                // rebuilds, and reports (tests only; use with --db).
                self.load_index_settings();
                self.excludes = std::env::var("FF_GUI_TEST_DIR").into_iter().collect();
                self.exclude_hidden = true;
                self.index_settings_dirty = true;
                self.act(Action::OptionsOk, 0);
                eprintln!("entries: {} | hidden left out: {} | status: {}", self.index.len(), self.index.skips_hidden(), self.status);
                return Ok(());
            }
            Ok("exclude") => {
                self.act(Action::Options(7), 0);
                self.excludes = vec!["/home/me/Videos".into(), "/mnt/backup".into()];
                self.dialog_input = "/var/cache".into();
                self.act(Action::Options(7), 0);
            }
            Ok("help0") => self.act(Action::Help(0), 0),
            Ok("help1") => self.act(Action::Help(1), 0),
            Ok("help2") => self.act(Action::Help(2), 0),
            Ok("regex") => self.act(Action::ToggleRegex, 0),
            Ok("ghost") => {
                // Saves the drag image of the selected result and exits.
                if let (Some(&id), Some(file)) = (self.hits.get(self.sel), std::env::var_os("FF_GUI_SNAPSHOT")) {
                    let icon = Icon::of(self.index.name(id), self.index.is_dir(id), self.index.is_symlink(id));
                    let label = drag_label(&self.index.path(id).to_string_lossy());
                    let cv = &self.drag_image(&label, icon);
                    let mut ppm = format!("P6 {} {} 255\n", cv.w, cv.h).into_bytes();
                    ppm.extend(cv.px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
                    std::fs::write(file, ppm)?;
                }
                return Ok(());
            }
            Ok("trash-all-now") => {
                // Trashes every result of the query without asking (tests only).
                self.marks = (0..self.hits.len()).collect();
                (self.pending_ids, self.pending_paths) = self.selected_entries();
                self.act(Action::ConfirmTrash, 0);
                self.finish_delete(true);
                eprintln!("{} | results left: {}", self.status, self.hits.len());
                return Ok(());
            }
            Ok("trash-now") => {
                // Trashes the selected result without asking (tests only).
                if self.sel < self.hits.len() {
                    (self.pending_ids, self.pending_paths) = self.selected_entries();
                    self.act(Action::ConfirmTrash, 0);
                    self.finish_delete(true);
                    eprintln!("{} | results left: {}", self.status, self.hits.len());
                }
                return Ok(());
            }
            Ok("copy-shown-quit") => {
                // As copy-quit, but once the window is on screen and has
                // the keyboard: Wayland lets only such a window copy.
                self.render()?;
                let until = Instant::now() + Duration::from_millis(700);
                while Instant::now() < until {
                    self.b.next_event(Some(Duration::from_millis(50)))?;
                }
                self.act(Action::CopyFile, 0);
                return Ok(());
            }
            Ok("copy-quit") => {
                self.act(Action::CopyFile, 123_456_789);
                return Ok(());
            }
            Ok("reindex") => {
                self.reindex()?;
                self.finish_build(true);
            }
            Ok("tree") => self.toggle_tree(),
            // Times drawing while scrolling through the whole list (as by
            // dragging the scroll bar), into FF_GUI_DEBUG.
            Ok("scroll-bench") => {
                if self.cv.w == 0 {
                    self.cv.resize(1000, 640);
                }
                let rows = self.rows_visible().max(1);
                let (mut total, mut worst, mut frames) = (Duration::ZERO, Duration::ZERO, 0u32);
                let step = (self.hits.len() / 200).max(rows);
                let mut top = 0;
                while top < self.hits.len() && frames < 200 {
                    self.top = top;
                    let t = Instant::now();
                    let _ = self.render();
                    let took = t.elapsed();
                    total += took;
                    worst = worst.max(took);
                    frames += 1;
                    top += step;
                }
                debug::log(format!("scroll-bench: {frames} frames of {}x{}, average {:?}, worst {:?}", self.cv.w, self.cv.h, total / frames.max(1), worst));
                return Ok(());
            }
            // A second window, searching on its own (the first keeps its
            // search): the snapshot shows the second.
            Ok("second-window") => {
                self.query = "first".into();
                self.search();
                self.summoned();
                self.query = "second".into();
                self.search();
                self.switch_to(0);
                assert_eq!(self.query, "first");
                self.switch_to(1);
            }
            Ok("complete") => self.update_completion(),
            Ok("bookmarks") => self.open_bar_menu(4),
            Ok("bookmark-add") => self.act(Action::AddBookmark, 0),
            Ok("export") => self.act(Action::ExportCsv, 0),
            Ok("export-now") => {
                self.dialog_input = std::env::var("FF_GUI_EXPORT").unwrap_or_default();
                self.act(Action::ConfirmExport, 0);
                eprintln!("{}", self.status);
                return Ok(());
            }
            Ok("options") => self.open_bar_menu(5),
            Ok("props") => self.act(Action::Properties, 0),
            Ok("select") => self.anchor = self.step(self.cursor, false, true),
            _ => {}
        }
        let mut dirty = true;
        let mut shown = (String::new(), Instant::now());
        loop {
            if self.quit {
                return Ok(());
            }
            while let Ok(notice) = self.notices.try_recv() {
                self.status = notice;
                dirty = true;
                // A background job ended: a preferences dialog may be stale.
                self.refresh_option_states();
                self.render_dialog()?;
            }
            // Another start of the program (such as the global shortcut)
            // asked for the window (Linux; Windows sends a message).
            #[cfg(unix)]
            if self.wake.as_ref().is_some_and(|rx| rx.try_recv().is_ok()) {
                self.summoned();
                self.refresh_if_stale();
                self.act(Action::SelectAll, 0);
                dirty = true;
            }
            // Files' own icons read in the background came in.
            if self.icons.take_loaded() {
                dirty = true;
            }
            // A delete running in the background finished.
            if self.deleting.is_some() && self.finish_delete(false) {
                dirty = true;
            }
            // Results of a background search came in; searches that had to
            // wait start.
            if self.finish_search(false) {
                dirty = true;
            }
            self.resume_searches();
            // Files changed on the drives (without the daemon).
            if self.follow_changes() {
                dirty = true;
            }
            // Something all windows show changed: draw them all.
            if std::mem::take(&mut self.redraw_all) && self.windows.iter().any(Option::is_some) {
                self.render_all()?;
                dirty = false;
            }
            // A message in the status bar goes away by itself, unless it
            // reports something still going on.
            if self.status != shown.0 {
                shown = (self.status.clone(), Instant::now());
            } else if !self.status.is_empty()
                && shown.1.elapsed() >= Duration::from_secs(6)
                && self.dragging.is_none()
                && self.status != tr("Waiting for authorization...")
            {
                self.status.clear();
                dirty = true;
            }
            // Redraw only once the event queue is drained.
            // While idle, wake once a second to notice a refreshed index.
            // While indexing, wake often to move the progress along.
            // Hidden (Linux), wake often to answer another start quickly.
            // Shown and following the drives, wake often so changes show
            // at once; also while catching up, to keep the checks fed.
            let idle = if self.searching() {
                Duration::from_millis(10) // results show as soon as they are in
            } else if self.building.is_some() || self.deleting.is_some() || self.icons.loading() || (self.hidden && cfg!(unix)) {
                Duration::from_millis(150)
            } else if self.follow.as_ref().is_some_and(|f| !self.hidden || f.catching_up()) {
                Duration::from_millis(250)
            } else {
                Duration::from_secs(1)
            };
            let wait = if dirty { Duration::ZERO } else { idle };
            let Some(event) = self.b.next_event(Some(wait))? else {
                if !dirty {
                    dirty = if self.building.is_some() {
                        self.finish_build(false);
                        true
                    } else {
                        self.reload_if_changed()
                    };
                    continue;
                }
                self.render()?;
                if let Some(path) = std::env::var_os("FF_GUI_SNAPSHOT") {
                    // A test snapshot shows the dialog if one is open.
                    let cv = self.dialog.as_ref().map_or(&self.cv, |d| d.shown());
                    let mut ppm = format!("P6 {} {} 255\n", cv.w, cv.h).into_bytes();
                    ppm.extend(cv.px.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0]]));
                    return std::fs::write(path, ppm);
                }
                dirty = false;
                continue;
            };
            // Events go to the window they happened in: it becomes current.
            // (With the dialog open, the others take no input.)
            let target = match &event {
                Event::Redraw(w) | Event::Close(w) | Event::Resize(w, ..) => Some(*w),
                Event::Key { win, .. } | Event::Press { win, .. } | Event::Motion { win, .. } | Event::Scroll { win, .. } => Some(*win),
                Event::Release { win, .. } => Some(*win),
                Event::Shown => Some(backend::FIRST),
                _ => None,
            };
            if matches!(event, Event::Key { .. } | Event::Press { .. } | Event::Motion { .. } | Event::Scroll { .. }) {
                self.last_input = Instant::now();
            }
            if let Some(Win::Main(n)) = target.filter(|_| self.dialog.is_none()) {
                if n != self.current {
                    if dirty {
                        self.render()?;
                    }
                    self.pending_draw = false;
                    self.switch_to(n);
                    dirty = std::mem::take(&mut self.pending_draw) || n == self.current;
                }
            }
            if self.dialog.is_some() && matches!(target, Some(Win::Main(n)) if n != self.current) {
                continue;
            }
            // The dialog is modal: it gets its own events, and the main
            // window takes no input while it is open.
            if self.dialog.is_some() {
                match &event {
                    Event::Key { win: Win::Dialog, .. }
                    | Event::Press { win: Win::Dialog, .. }
                    | Event::Motion { win: Win::Dialog, .. }
                    | Event::Redraw(Win::Dialog)
                    | Event::Close(Win::Dialog) => {
                        self.dialog_event(&event)?;
                        dirty = true; // its action may have changed the main window
                        continue;
                    }
                    Event::Key { .. } | Event::Press { .. } | Event::Motion { .. } | Event::Scroll { .. } => continue,
                    _ => {}
                }
            }
            match event {
                Event::Redraw(_) => dirty = true,
                Event::Resize(_, w, h) => {
                    if (w, h) != (self.cv.w, self.cv.h) {
                        self.cv.resize(w, h);
                        self.menu = None;
                    }
                    dirty = true;
                }
                Event::Scale(scale) => {
                    // The desktop told us its scale: follow it unless the
                    // user chose a zoom of their own.
                    if scale != self.auto_scale {
                        let following = self.scale == self.auto_scale;
                        self.auto_scale = scale;
                        if following {
                            self.set_scale(None);
                            self.status.clear();
                        }
                        dirty = true;
                    }
                }
                Event::Close(_) => {
                    if !self.dismiss() {
                        return Ok(());
                    }
                }
                Event::Quit => return Ok(()),
                Event::Summoned => {
                    self.summoned();
                    dirty = true;
                }
                // Back from the notification area: typing replaces the
                // last search.
                Event::Shown => {
                    self.hidden = false;
                    self.refresh_if_stale();
                    self.act(Action::SelectAll, 0);
                    dirty = true;
                }
                Event::Key { key, mods, .. } => {
                    if !self.key(key, mods)? {
                        return Ok(());
                    }
                    dirty = true;
                }
                Event::Press { button, x, y, time, mods, .. } => {
                    self.press(button, x, y, time, mods);
                    dirty = true;
                }
                Event::Scroll { lines, .. } => {
                    let max = self.max_top();
                    let at = self.top_mut();
                    let top = at.saturating_add_signed(lines as isize).min(max);
                    dirty |= std::mem::replace(at, top) != top;
                }
                Event::Release { button: 1, .. } => {
                    if self.column_drag.take().is_some() {
                        self.save_config();
                    }
                    self.dragging_scrollbar = false;
                    self.selecting = false;
                    self.drag_arm = None;
                    // A plain click on one of several selected rows.
                    if let Some(row) = self.collapse_on_release.take().filter(|_| self.dragging.is_none()) {
                        self.select_only(row);
                    }
                    dirty = true;
                }
                Event::Release { .. } => {}
                Event::Motion { x, y, mods, .. } => {
                    if let Some(edge) = self.column_drag {
                        self.drag_column(edge, x);
                        self.b.set_cursor(Cursor::Resize(3));
                        dirty = true;
                        continue;
                    }
                    let hover = self.title_button_at(x, y);
                    dirty |= std::mem::replace(&mut self.hover_button, hover) != hover;
                    let cursor = self.cursor_at(x, y);
                    self.b.set_cursor(cursor);
                    dirty |= self.motion(x, y, mods)?;
                }
                Event::Paste(text) => {
                    let line: String = text.lines().next().unwrap_or("").chars().filter(|c| !c.is_control()).collect();
                    if self.dialog.is_some() {
                        // Pasted into the dialog's text field.
                        self.dialog_input.push_str(&line);
                        self.render_dialog()?;
                    } else {
                        self.insert(&line);
                    }
                    dirty = true;
                }
                Event::DragStatus { accepted, moves } => {
                    if let Some(label) = &self.dragging {
                        self.status = match (accepted, moves) {
                            (false, _) => trf("Dragging {} - cannot be dropped here", &[label]),
                            (true, true) => trf("Dragging {} - release to move it here (hold Ctrl to copy)", &[label]),
                            (true, false) => trf("Dragging {} - release to copy it here", &[label]),
                        };
                        dirty = true;
                    }
                }
                Event::DragDropped { moves } => {
                    let label = self.dragging.clone().unwrap_or_default();
                    self.status = trf(if moves { "Moved {}" } else { "Copied {}" }, &[&label]);
                    // Once the target reports it is done, moved files leave the list.
                    self.drop_moved = moves;
                    dirty = true;
                }
                Event::DragFinished => {
                    if std::mem::take(&mut self.drop_moved) {
                        // Only files really gone from disk are removed.
                        let paths: Vec<PathBuf> = self.drag_paths.lines().map(PathBuf::from).collect();
                        self.forget(&paths);
                        dirty = true;
                    }
                }
                Event::DragEnded => {
                    if self.dragging.take().is_some() && !self.drop_moved && !self.status.starts_with(tr("Copied {}").split("{}").next().unwrap_or("")) {
                        self.status.clear();
                    }
                    self.drag_arm = None;
                    self.collapse_on_release = None;
                    dirty = true;
                }
            }
        }
    }

    // ---- state ---------------------------------------------------------

    /// The search box's query and the list's options, as `search` uses.
    fn current_query(&self) -> (Query, SearchOptions) {
        let opts = SearchOptions {
            kind: self.kind,
            sort: self.sort,
            reverse: self.reverse,
            ..Default::default()
        };
        let how = ff_core::QueryOptions {
            case: self.case,
            regex: self.regex,
            whole_word: self.whole_word,
            match_path: self.flag(Flag::MatchPath),
            wildcards_anywhere: !self.flag(Flag::WildcardWholeName),
        };
        (Query::parse_opts(&self.query, &how), opts)
    }

    /// Searches the index for the current window's query, on a thread of
    /// its own: the window keeps working with its last results until the
    /// new ones arrive (`finish_search`). A search still running is given
    /// up: its results would be out of date.
    fn search(&mut self) {
        let (query, opts) = self.current_query();
        self.query_error = query.error().map(String::from);
        self.search_pending = false;
        self.drop_search();
        if self.flag(Flag::HideEmpty) && self.query.trim().is_empty() {
            self.show_results(Vec::new()); // an empty search box shows an empty list
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (index, flag) = (IndexRef(&self.index), stop.clone());
        let handle = std::thread::spawn(move || {
            let index = index;
            // SAFETY: see `IndexRef`.
            let index = unsafe { &*index.0 };
            let _ = tx.send(index.search_until(&query, &opts, &flag));
        });
        self.search_job = Some(SearchJob { window: self.current, stop, rx, handle });
        // Tests take pictures at once: they wait for the results.
        if std::env::var_os("FF_GUI_TEST").is_some() || std::env::var_os("FF_GUI_SNAPSHOT").is_some() {
            self.finish_search(true);
        }
    }

    /// New results for the current window, from the top.
    fn show_results(&mut self, hits: Vec<u32>) {
        self.hits = hits;
        self.hover_row = None;
        self.sel = 0;
        self.top = 0;
        self.marks.clear();
        self.anchor_row = 0;
    }

    /// Gives up the running search, if any, without waiting for its thread.
    /// A search for another window is done again later.
    fn drop_search(&mut self) {
        if let Some(job) = self.search_job.take() {
            job.stop.store(true, Ordering::Relaxed);
            self.dropped_searches.push((job.stop, job.handle));
            if job.window != self.current {
                if let Some(Some(w)) = self.windows.get_mut(job.window as usize) {
                    w.search_pending = true;
                }
            }
        }
        self.dropped_searches.retain(|(_, h)| !h.is_finished());
    }

    /// Takes in the results of the background search if they are there
    /// (`wait`: waits for them). True if a window got new results.
    fn finish_search(&mut self, wait: bool) -> bool {
        let Some(job) = &self.search_job else { return false };
        let result = if wait {
            job.rx.recv().ok().flatten()
        } else {
            match job.rx.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return false,
                Err(mpsc::TryRecvError::Disconnected) => None,
            }
        };
        let Some(job) = self.search_job.take() else { return false };
        let _ = job.handle.join();
        let Some(hits) = result else { return false };
        let current = self.current;
        let open = job.window == current || self.windows.get(job.window as usize).is_some_and(Option::is_some);
        if !open {
            return false; // its window was closed meanwhile
        }
        self.switch_to(job.window);
        self.show_results(hits);
        if job.window != current {
            if !(self.current == 0 && self.hidden) {
                let _ = self.render();
            }
            self.switch_to(current);
            return false;
        }
        true
    }

    /// True while a search runs in the background.
    fn searching(&self) -> bool {
        self.search_job.is_some()
    }

    /// Stops every search thread and waits for them: the index is about
    /// to change. The window whose search stopped searches again after.
    fn quiesce(&mut self) {
        if let Some(job) = self.search_job.take() {
            job.stop.store(true, Ordering::Relaxed);
            let _ = job.handle.join();
            if job.window == self.current {
                self.search_pending = true;
            } else if let Some(Some(w)) = self.windows.get_mut(job.window as usize) {
                w.search_pending = true;
            }
        }
        for (stop, handle) in self.dropped_searches.drain(..) {
            stop.store(true, Ordering::Relaxed);
            let _ = handle.join();
        }
    }

    /// Starts a search that had to wait (one at a time): the current
    /// window's first. True if one started.
    fn resume_searches(&mut self) -> bool {
        if self.searching() {
            return false;
        }
        if self.search_pending {
            self.search();
            return true;
        }
        let Some(n) = (0..self.windows.len()).find(|&n| self.windows[n].as_ref().is_some_and(|w| w.search_pending)) else { return false };
        let current = self.current;
        self.switch_to(n as u8);
        self.search();
        self.switch_to(current);
        true
    }

    // ---- search box editing -------------------------------------------

    /// The selected byte range of the search text (empty if none).
    fn selection(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }

    /// Removes the selected text; returns false if nothing was selected.
    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        self.query.replace_range(a..b, "");
        (self.cursor, self.anchor) = (a, a);
        a != b
    }

    /// Replaces the selection with `s` and searches again.
    fn insert(&mut self, s: &str) {
        self.delete_selection();
        self.query.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.anchor = self.cursor;
        self.search();
    }

    fn clear_query(&mut self) {
        self.query.clear();
        (self.cursor, self.anchor) = (0, 0);
        self.search();
    }

    /// Caret position one character or one word to the left or right.
    fn step(&self, from: usize, right: bool, word: bool) -> usize {
        let (before, after) = self.query.split_at(from);
        match (right, word) {
            (false, false) => before.chars().next_back().map_or(0, |c| from - c.len_utf8()),
            (true, false) => after.chars().next().map_or(from, |c| from + c.len_utf8()),
            (false, true) => {
                let word = before.trim_end();
                word.char_indices().rev().find(|(_, c)| c.is_whitespace()).map_or(0, |(i, c)| i + c.len_utf8())
            }
            (true, true) => {
                let word = after.trim_start();
                self.query.len() - word.len() + word.find(char::is_whitespace).unwrap_or(word.len())
            }
        }
    }

    /// Moves the caret, extending the selection if `select`.
    fn move_caret(&mut self, to: usize, select: bool) {
        self.cursor = to;
        if !select {
            self.anchor = to;
        }
    }

    /// Deletes the selection, or else from the caret to `to`.
    fn delete_to(&mut self, to: usize) {
        if self.selection().0 == self.selection().1 {
            self.anchor = to;
        }
        self.delete_selection();
        self.search();
    }

    /// X position where the search text starts (may be off to the left).
    fn box_text_x(&self) -> i32 {
        14 * self.u - self.box_scroll
    }

    /// Puts `text` on the clipboard; `file` says it is the path of a
    /// file being copied as a file.
    fn copy(&mut self, text: String, file: bool) {
        let copied = self.b.copy(&text, file);
        let lines = text.lines().count();
        self.status = if !copied {
            tr("Could not copy: the clipboard is not available").into()
        } else if file && lines > 1 {
            trf("Copied {} files (paste them in a file manager)", &[&lines])
        } else if file {
            trf("Copied the file {} (paste it in a file manager)", &[&text])
        } else if lines > 1 {
            trf("Copied {} lines", &[&lines])
        } else {
            trf("Copied {}", &[&text])
        };
    }

    /// Reloads the index if its file changed on disk (the ffd daemon or
    /// `ff index` rewrote it), keeping the query and scroll position.
    fn reload_if_changed(&mut self) -> bool {
        // The preferred index can change while we run: the daemon's index
        // appears when its service starts and vanishes when it is removed.
        if self.follow_default {
            if let Some(preferred) = ff_core::default_db_path().filter(|p| *p != self.db) {
                self.db = preferred;
                self.db_stamp = None;
            }
        }
        let stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
        if stamp == self.db_stamp {
            return false;
        }
        let Ok(index) = Index::load(&self.db) else { return false }; // mid-rewrite: retry next tick
        self.db_stamp = stamp;
        let kept = self.keep_tree();
        self.quiesce();
        self.index = index;
        if let Some(f) = &mut self.follow {
            f.replaced();
        }
        self.research_all();
        self.restore_tree(kept);
        let (sel, top) = (self.sel, self.top);
        self.search();
        self.sel = sel.min(self.hits.len().saturating_sub(1));
        self.top = top.min(self.max_top());
        true
    }

    /// Without the daemon, follows changes on the indexed drives and
    /// patches the index; true if the results changed. The index file is
    /// written again once quiet while the window is hidden.
    fn follow_changes(&mut self) -> bool {
        let wanted = self.db.as_os_str() != ff_core::SYSTEM_DB
            && std::env::var_os("FF_GUI_SNAPSHOT").is_none()
            && std::env::var_os("FF_GUI_TEST").is_none()
            && self.index.len() > 0;
        if !wanted {
            self.follow = None;
            return false;
        }
        // While a build or a search runs, events wait: they apply to the
        // new index, or once the search is done. So they do while the user
        // is busy in a window (a quarter of a second after the last input),
        // so dragging and typing are never held up.
        if self.building.is_some() || self.searching() || self.last_input.elapsed() < Duration::from_millis(250) {
            return false;
        }
        if self.follow.as_ref().is_none_or(|f| !f.suits(&self.index)) {
            self.follow = Some(follow::Follow::new(&self.index, &self.db));
        }
        let old_len = self.index.len() as u32;
        let Some(f) = self.follow.as_mut() else { return false };
        let found = f.check(&mut self.index, !self.hidden);
        if std::mem::take(&mut f.lost) {
            self.status = tr("Some folders cannot be followed; changes there show after the next indexing").into();
        }
        match found {
            follow::Found::Nothing => {
                if self.hidden && f.time_to_save() {
                    self.save_followed();
                }
                false
            }
            follow::Found::Restart => {
                // New watchers and a catch-up; what the index has unsaved
                // is still to be written.
                let unsaved = f.unsaved;
                let mut fresh = follow::Follow::new(&self.index, &self.db);
                fresh.unsaved = unsaved;
                self.follow = Some(fresh);
                false
            }
            follow::Found::Changed => {
                let touched = f.take_touched();
                // Every window's results; a hidden one is searched again
                // when it shows (nobody sees it meanwhile).
                let current = self.current;
                // Every window's results, a hidden one's too: it, or a new
                // window taking its list, shows at once when called.
                self.each_window(|app| {
                    app.update_results(old_len, &touched);
                    let shown = !(app.current == 0 && app.hidden);
                    if shown && app.current != current {
                        let _ = app.render();
                    }
                });
                !(self.current == 0 && self.hidden)
            }
        }
    }

    /// Searches again if the index changed while the window was hidden.
    fn refresh_if_stale(&mut self) {
        if std::mem::take(&mut self.results_stale) {
            let kept = self.keep_tree();
            self.restore_tree(kept);
            self.search();
        }
    }

    /// Brings the results up to date with a patched index without
    /// searching again (which, on millions of entries and with files
    /// changing several times a second, would keep every core busy): gone
    /// entries leave the list, and only new entries (from `old_len` on) and
    /// those whose size or time changed are matched and put in their place.
    /// The folder sizes view, costlier, is redone at most every few seconds.
    fn update_results(&mut self, old_len: u32, touched: &[u32]) {
        if self.tree.is_some() && self.tree_refreshed.elapsed() >= Duration::from_secs(5) {
            self.tree_refreshed = Instant::now();
            let kept = self.keep_tree();
            self.restore_tree(kept);
        }
        let selected = self.hits.get(self.sel).copied();
        let before = self.hits.len();
        let index = &self.index;
        self.hits.retain(|&h| !index.is_gone(h) && !touched.contains(&h));
        let mut changed = self.hits.len() != before;
        if !(self.flag(Flag::HideEmpty) && self.query.trim().is_empty()) {
            let (query, opts) = self.current_query();
            let candidates = touched.iter().copied().chain(old_len..self.index.len() as u32);
            for id in candidates {
                if !self.index.is_gone(id) && self.index.matches(&query, &opts, id) {
                    self.index.insert_sorted(&mut self.hits, id, &opts);
                    changed = true;
                }
            }
        }
        if changed {
            // Rows moved: extra selected rows no longer mean the same.
            self.marks.clear();
            self.hover_row = None;
        }
        // Keep the same entry selected when it is still there.
        self.sel = selected.and_then(|id| self.hits.iter().position(|&h| h == id)).unwrap_or(self.sel).min(self.hits.len().saturating_sub(1));
        self.top = self.top.min(self.max_top());
    }

    /// Writes the followed changes to the index file.
    fn save_followed(&mut self) {
        let Some(f) = self.follow.as_mut().filter(|f| f.unsaved) else { return };
        f.unsaved = false;
        let start = Instant::now();
        let saved = self.index.save(&self.db);
        debug::log(format!("follow: index saved ({saved:?}) in {:?}", start.elapsed()));
        if saved.is_ok() {
            // Our own write: nothing to load again.
            self.db_stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
        }
    }

    fn reindex(&mut self) -> io::Result<()> {
        if self.db.as_os_str() == ff_core::SYSTEM_DB {
            self.status = tr("The index is kept up to date by the ffd daemon").into();
            return Ok(());
        }
        if self.building.is_some() {
            return Ok(()); // already under way: its progress is showing
        }
        // The walk runs in the background; the window stays usable with
        // the old index and shows how far the new one has got.
        let opts = self.pending_build.take().unwrap_or_else(|| BuildOptions::refresh(&self.index));
        let found = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::channel();
        let (db, counter) = (self.db.clone(), found.clone());
        std::thread::spawn(move || {
            let result = Index::build_with_progress(&opts, &counter).map_err(|e| format!("indexing failed: {e}")).and_then(|index| {
                index.save(&db).map_err(|e| format!("could not save the index: {e}"))?;
                Ok(index)
            });
            let _ = tx.send(result);
        });
        // The last index's size is a good guess at the new one's.
        self.building = Some(Building { rx, found, started: Instant::now(), expected: self.index.len(), forgotten: Vec::new() });
        self.status.clear();
        Ok(())
    }

    /// Takes the finished index, if the background build is done
    /// (`wait`: block until it is).
    fn finish_build(&mut self, wait: bool) {
        let Some(build) = &self.building else { return };
        let result = if wait {
            build.rx.recv().map_err(|_| ())
        } else {
            match build.rx.try_recv() {
                Ok(result) => Ok(result),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err(()),
            }
        };
        let took = build.started.elapsed();
        let forgotten = self.building.take().map(|b| b.forgotten).unwrap_or_default();
        match result {
            Ok(Ok(index)) => {
                self.db_stamp = std::fs::metadata(&self.db).and_then(|m| m.modified()).ok();
                let kept = self.keep_tree();
                self.quiesce();
                self.index = index;
                if let Some(f) = &mut self.follow {
                    f.replaced();
                }
                self.research_all();
                self.restore_tree(kept);
                self.status = trf("Indexed {} entries in {} s", &[&group_digits(self.index.len() as u64), &format!("{:.1}", took.as_secs_f64())]);
                if !forgotten.is_empty() {
                    self.forget(&forgotten);
                }
            }
            Ok(Err(e)) => self.status = e,
            Err(()) => self.status = tr("indexing failed").into(),
        }
        self.search();
        // Options changed while this build ran: build again with them.
        if self.pending_build.is_some() {
            let _ = self.reindex();
        }
    }

    /// The status bar text while indexing: entries found, an estimate of
    /// how far along that is, and the time taken.
    fn build_status(&self) -> Option<(String, Option<f64>)> {
        let build = self.building.as_ref()?;
        let found = build.found.load(Ordering::Relaxed);
        let secs = build.started.elapsed().as_secs();
        let share = (build.expected > 0).then(|| (found as f64 / build.expected as f64).min(0.99));
        let count = trf("Indexing... {} entries", &[&group_digits(found as u64)]);
        let text = match share {
            Some(share) => format!("{count}  ·  {:.0} %  ·  {secs} s", share * 100.0),
            None => format!("{count}  ·  {secs} s"),
        };
        Some((text, share))
    }

    /// Runs a menu or keyboard action.
    fn act(&mut self, action: Action, time: u32) {
        // In the folder sizes view, the actions on a result are for the
        // entry selected there.
        use Action::*;
        if self.tree.is_some() && matches!(action, Open | Reveal | Edit | CopyFile | CopyPath | CopyName | Copy | Properties | Trash | DeleteForever) {
            self.on_tree_target(|app| app.act(action, time));
            return;
        }
        // What the user may not do to these files is not attempted:
        // this also stops the keys that stand for a greyed menu entry.
        if let Some(why) = self.refusal(action) {
            self.status = tr(why).into();
            return;
        }
        // First the actions that do not need a selected result.
        let (a, b) = self.selection();
        match action {
            Action::Cut | Action::Copy if a != b => {
                self.copy(self.query[a..b].to_string(), false);
                if action == Action::Cut {
                    self.insert("");
                }
                return;
            }
            Action::Cut => return,
            // With no search text selected, Copy means the selected file.
            Action::Copy => return self.act(Action::CopyFile, time),
            Action::Paste => {
                self.b.request_paste(); // the text arrives as an event
                return;
            }
            Action::SelectAll => return (self.anchor, self.cursor) = (0, self.query.len()),
            Action::SelectAllResults => {
                self.marks = (0..self.hits.len()).collect();
                return;
            }
            Action::Show(kind) => {
                self.kind = kind;
                return self.search();
            }
            Action::Case(case) => {
                self.case = case;
                self.save_config();
                return self.search();
            }
            Action::TreeSize => return self.toggle_tree(),
            Action::AddBookmark => return self.ask_bookmark(),
            Action::SaveBookmark => return self.save_bookmark(),
            Action::Bookmark(i) => return self.open_bookmark(i as usize),
            Action::SelectBookmark(i) => return self.bookmark_selected = i as usize,
            Action::RaiseBookmark => return self.raise_bookmark(),
            Action::RemoveBookmark => return self.remove_bookmark(),
            Action::ExportCsv => return self.ask_export(),
            Action::ConfirmExport => return self.export(),
            Action::Reindex => {
                if let Err(e) = self.reindex() {
                    self.status = e.to_string();
                }
                return;
            }
            Action::About => return self.about = true,
            Action::Toggle(flag) => {
                self.flags[flag as usize] = !self.flags[flag as usize];
                self.save_config();
                // Options that change which results are found.
                if matches!(flag, Flag::HideEmpty | Flag::MatchPath | Flag::WildcardWholeName) {
                    self.search();
                }
                return;
            }
            Action::SizeFormat(format) => {
                self.size_format = format;
                return self.save_config();
            }
            Action::Language(language) => {
                self.language = language;
                self.apply_language();
                self.status.clear();
                return self.save_config();
            }
            Action::Theme(theme) => {
                self.theme = theme;
                self.apply_theme();
                return self.save_config();
            }
            Action::Accent(accent) => {
                self.accent = accent;
                self.apply_theme();
                return self.save_config();
            }
            Action::ToggleRegex => {
                self.regex = !self.regex;
                self.save_config();
                return self.search();
            }
            Action::ToggleWord => {
                self.whole_word = !self.whole_word;
                self.save_config();
                return self.search();
            }
            Action::Options(page) => {
                if let Err(e) = self.open_options(page) {
                    self.status = e.to_string();
                }
                return;
            }
            Action::OptionsOk | Action::OptionsApply => {
                // Only the index settings wait for this; everything else
                // in the window takes effect as it is changed.
                if !std::mem::take(&mut self.index_settings_dirty) {
                    return;
                }
                let service = settings::daemon_installed();
                return self.act(if service { Action::ApplyDrives } else { Action::ApplyExcludes }, time);
            }
            Action::ToggleExcludeHidden => {
                self.index_settings_dirty = true;
                return self.exclude_hidden = !self.exclude_hidden;
            }
            Action::SelectExclude(i) => return self.exclude_selected = i as usize,
            Action::RemoveExclude => {
                self.index_settings_dirty = true;
                if self.exclude_selected < self.excludes.len() {
                    self.excludes.remove(self.exclude_selected);
                    self.exclude_selected = self.exclude_selected.min(self.excludes.len().saturating_sub(1));
                }
                return;
            }
            Action::AddExclude | Action::AddSelectedExclude => {
                // From the text field, or the folder selected in the results
                // (the folder holding it, if a file is selected).
                let folder = if action == Action::AddExclude {
                    trim_folder(std::mem::take(&mut self.dialog_input).trim())
                } else {
                    let Some(&id) = self.hits.get(self.sel) else {
                        self.status = tr("Select a folder in the results first").into();
                        return;
                    };
                    let folder = if self.index.is_dir(id) { Some(id) } else { self.index.parent(id) };
                    folder.map(|f| self.index.path(f).to_string_lossy().into_owned()).unwrap_or_default()
                };
                if !std::path::Path::new(&folder).is_absolute() {
                    self.status = tr("A folder to exclude must be a full path, like /home/me/Videos").into();
                } else if !self.excludes.contains(&folder) {
                    self.exclude_selected = self.excludes.len();
                    self.excludes.push(folder);
                    self.index_settings_dirty = true;
                }
                return;
            }
            Action::ApplyExcludes => {
                if self.db.as_os_str() == ff_core::SYSTEM_DB {
                    // The live updates service owns this index: its settings
                    // are saved together, drives included.
                    return self.act(Action::ApplyDrives, time);
                }
                if self.drives.iter().any(|d| d.mount.is_empty() && d.index) {
                    self.status = "Reading a drive that is not mounted needs the live updates service (Indexes > Live updates)".into();
                    return;
                }
                let mut opts = BuildOptions::refresh(&self.index);
                opts.excludes = BuildOptions::new(["/"]).excludes; // the standard ones
                opts.excludes.extend(self.excludes.iter().map(PathBuf::from));
                opts.skip_hidden = self.exclude_hidden;
                self.pending_build = Some(opts);
                if let Err(e) = self.reindex() {
                    self.status = e.to_string();
                }
                return;
            }
            Action::SelectDrive(i) => return self.drive_selected = i as usize,
            Action::ToggleAutoDrives => {
                self.index_settings_dirty = true;
                return self.auto_drives = !self.auto_drives;
            }
            Action::DriveIndex(i) | Action::DriveLive(i) => {
                self.index_settings_dirty = true;
                if let Some(drive) = self.drives.get_mut(i as usize) {
                    match action {
                        Action::DriveIndex(_) => drive.index = !drive.index,
                        _ => drive.live = !drive.live,
                    }
                }
                return;
            }
            Action::ApplyDrives => {
                if !settings::daemon_installed() {
                    self.status = "Drive settings need the live updates service: turn it on under Indexes > Live updates".into();
                    return;
                }
                self.load_index_settings();
                self.status = tr("Waiting for authorization...").into();
                let settings: Vec<_> =
                    self.drives.iter().map(|d| (d.path.clone(), !d.mount.is_empty(), d.index, d.live)).collect();
                let (tx, automatic) = (self.notice_tx.clone(), self.auto_drives);
                let (excludes, no_hidden) = (self.excludes.clone(), self.exclude_hidden);
                std::thread::spawn(move || tx.send(settings::apply_drives(settings, automatic, excludes, no_hidden)));
                return;
            }
            Action::Help(page) => {
                if let Err(e) = self.open_help(page) {
                    self.status = e.to_string();
                }
                return;
            }
            Action::ConfirmTrash | Action::ConfirmDelete => {
                let (ids, paths) = (std::mem::take(&mut self.pending_ids), std::mem::take(&mut self.pending_paths));
                return self.start_delete(ids, paths, action == Action::ConfirmTrash);
            }
            Action::Quit => return self.quit = true,
            Action::ZoomIn | Action::ZoomOut | Action::ZoomReset => {
                self.set_scale(match action {
                    Action::ZoomIn => Some(self.scale + 0.25),
                    Action::ZoomOut => Some(self.scale - 0.25),
                    _ => None,
                });
                // The Options window is sized for the old zoom: reopen it.
                if self.paged_dialog_open() {
                    let page = self.options_page.unwrap_or(0);
                    self.close_dialog();
                    let _ = self.open_options(page);
                }
                return;
            }
            Action::ToggleTitleBar => {
                self.custom_title = !self.custom_title;
                self.apply_title_bar();
                return self.save_config();
            }
            Action::ToggleShortcut => {
                self.status = match settings::set_shortcut(!settings::shortcut_enabled(), &self.shortcut_keys) {
                    Ok(true) => trf("{} now opens Fast search", &[&self.shortcut_keys]),
                    Ok(false) => "Global shortcut removed".into(),
                    Err(e) => format!("could not change the shortcut: {e}"),
                };
                self.apply_hotkey();
                self.refresh_option_states();
                return;
            }
            Action::SetShortcut => {
                let typed = std::mem::take(&mut self.dialog_input);
                let keys = match settings::shortcut_binding(&typed) {
                    Ok(binding) => settings::shortcut_label(&binding),
                    Err(e) => return self.status = e,
                };
                match settings::set_shortcut(true, &keys) {
                    Ok(_) => {
                        self.status = trf("{} now opens Fast search", &[&keys]);
                        self.shortcut_keys = keys;
                        let status = std::mem::take(&mut self.status);
                        self.save_config();
                        if self.status.is_empty() {
                            self.status = status;
                        }
                        // Registering may fail (keys taken): that is said instead.
                        self.apply_hotkey();
                        self.refresh_option_states();
                    }
                    Err(e) => self.status = format!("could not change the shortcut: {e}"),
                }
                return;
            }
            Action::ToggleAutostart => {
                let on = !settings::autostart_enabled();
                self.status = match settings::set_autostart(on) {
                    Ok(()) if on => "Fast search starts at sign-in, hidden, ready for its shortcut".into(),
                    Ok(()) => "Fast search no longer starts at sign-in".into(),
                    Err(e) => format!("could not change the start at sign-in: {e}"),
                };
                return;
            }
            Action::ToggleLauncher => {
                let done = if settings::launcher_installed() { settings::remove_launcher() } else { settings::install_launcher().map(drop) };
                self.status = match done {
                    Ok(()) if settings::launcher_installed() => "Added to the applications menu".into(),
                    Ok(()) => "Removed from the applications menu".into(),
                    Err(e) => format!("could not update the applications menu: {e}"),
                };
                return;
            }
            Action::ToggleDaemon => {
                self.status = tr("Waiting for authorization...").into();
                let (install, tx) = (!settings::daemon_installed(), self.notice_tx.clone());
                std::thread::spawn(move || tx.send(settings::set_daemon(install)));
                return;
            }
            Action::Update => {
                self.status = tr("Checking for updates...").into();
                let tx = self.notice_tx.clone();
                std::thread::spawn(move || {
                    // On Windows the installer replaces this program and
                    // starts it again; elsewhere packages do the updating.
                    #[cfg(windows)]
                    let done = ff_core::update::update(true, |step| drop(tx.send(step.to_string())));
                    #[cfg(not(windows))]
                    let done = ff_core::update::latest().map(|r| match r.is_newer() {
                        true => format!("Fast search {} is available: {}", r.version(), r.page()),
                        false => format!("Fast search is up to date ({})", ff_core::update::VERSION),
                    });
                    tx.send(done.unwrap_or_else(|e| format!("Could not update: {e}")))
                });
                return;
            }
            _ => {}
        }
        let Some(&id) = self.hits.get(self.sel) else { return };
        let path = self.index.path(id);
        // Open, copy and delete apply to the whole selection; the rest
        // to the current row.
        let paths = self.selected_paths();
        let many = paths.len() > 1;
        let lines = |paths: &[PathBuf]| paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
        match action {
            Action::Open if paths.len() > 20 => {
                self.status = format!("{} results selected: open at most 20 at a time", paths.len());
            }
            Action::Open if self.flag(Flag::CloseOnOpen) => {
                // Open first, then close (or hide to the notification area).
                paths.iter().for_each(|p| drop(desktop::open(p)));
                self.quit = !self.dismiss();
            }
            Action::Open => {
                self.status = if many { trf("Opening {} items", &[&paths.len()]) } else { trf("Opening {}", &[&path.display()]) };
                std::thread::spawn(move || paths.iter().for_each(|p| drop(desktop::open(p))));
            }
            Action::Reveal => {
                self.status = trf("Showing {} in the file manager", &[&path.display()]);
                std::thread::spawn(move || desktop::reveal(&path));
            }
            Action::Trash | Action::DeleteForever => {
                let forever = action == Action::DeleteForever;
                // Windows asks and shows the progress itself, as Explorer
                // does (test runs keep our dialog, to draw it).
                if cfg!(windows) && std::env::var_os("FF_GUI_TEST").is_none() {
                    let (ids, paths) = self.selected_entries();
                    return self.start_delete(ids, paths, !forever);
                }
                let question = match (forever, many, self.index.is_dir(id)) {
                    (false, true, _) => trf("Move these {} items to the trash?", &[&paths.len()]),
                    (false, false, true) => tr("Move this folder and everything in it to the trash?").to_string(),
                    (false, false, false) => tr("Move this file to the trash?").to_string(),
                    (true, true, _) => trf("Permanently delete these {} items? This cannot be undone.", &[&paths.len()]),
                    (true, false, true) => tr("Permanently delete this folder and everything in it? This cannot be undone.").to_string(),
                    (true, false, false) => tr("Permanently delete this file? This cannot be undone.").to_string(),
                };
                let mut rows = vec![Row::Text(question), Row::Gap];
                if many {
                    // Show the first few, so it is clear what is about to go.
                    rows.extend(paths.iter().take(6).map(|p| Row::Dim(p.display().to_string())));
                    if paths.len() > 6 {
                        rows.push(Row::Dim(trf("... and {} more", &[&(paths.len() - 6)])));
                    }
                } else {
                    let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
                    let location = path.parent().map(|p| p.display().to_string()).unwrap_or_default();
                    rows.extend([Row::Pair("Name", name), Row::Pair("Location", location)]);
                }
                let (title, buttons, focus) = if forever {
                    // Enter cancels; deleting for good takes a deliberate click or Tab.
                    ("Delete permanently", vec![("Cancel", None), ("Delete permanently", Some(Action::ConfirmDelete))], 0)
                } else {
                    ("Move to trash", vec![("Cancel", None), ("Move to trash", Some(Action::ConfirmTrash))], 1)
                };
                (self.pending_ids, self.pending_paths) = self.selected_entries();
                if let Err(e) = self.open_dialog(title, rows, buttons, focus, forever) {
                    self.status = e.to_string();
                }
            }
            Action::Edit => {
                self.status = trf("Opening {} in the text editor", &[&path.display()]);
                std::thread::spawn(move || desktop::open_in_editor(&path));
            }
            Action::CopyFile => self.copy(lines(&paths), true),
            Action::CopyPath if self.flag(Flag::QuotePaths) => {
                let quoted: Vec<String> = paths.iter().map(|p| format!("\"{}\"", p.display())).collect();
                self.copy(quoted.join("\n"), false);
            }
            Action::CopyPath => self.copy(lines(&paths), false),
            Action::CopyName => {
                let names: Vec<String> = paths
                    .iter()
                    .map(|p| p.file_name().map_or_else(|| p.to_string_lossy(), |n| n.to_string_lossy()).into_owned())
                    .collect();
                self.copy(names.join("\n"), false);
            }
            Action::Properties => match desktop::properties(&path) {
                Ok(props) => self.props = Some(props),
                Err(e) => self.status = format!("{}: {e}", path.display()),
            },
            _ => {}
        }
    }

    /// The right-click menu for the selected result: text-like files
    /// (text, code, unknown types) also offer the text editor.
    fn context_items(&self) -> Items {
        let Some(&id) = self.hits.get(self.sel) else { return CONTEXT_MENU };
        let icon = Icon::of(self.index.name(id), self.index.is_dir(id), self.index.is_symlink(id));
        if matches!(icon, Icon::Text | Icon::Code | Icon::File | Icon::Link) { TEXT_CONTEXT_MENU } else { CONTEXT_MENU }
    }

    /// Opens a page of the Options window: categories down the left,
    /// the chosen page on the right. Changes apply as they are made,
    /// except drive settings, which have their own Apply button.
    fn open_options(&mut self, page: u8) -> io::Result<()> {
        let page = page.min(9);
        self.refresh_option_states();
        self.load_index_settings();
        let nav = vec![
            NavItem { label: "General", child: false, action: Action::Options(0) },
            NavItem { label: "Interface", child: true, action: Action::Options(0) },
            NavItem { label: "Language", child: true, action: Action::Options(9) },
            NavItem { label: "Search", child: true, action: Action::Options(1) },
            NavItem { label: "Results", child: true, action: Action::Options(2) },
            NavItem { label: "View", child: true, action: Action::Options(3) },
            NavItem { label: "Desktop", child: true, action: Action::Options(4) },
            NavItem { label: "Bookmarks", child: true, action: Action::Options(8) },
            NavItem { label: "Indexes", child: false, action: Action::Options(5) },
            NavItem { label: "Live updates", child: true, action: Action::Options(5) },
            NavItem { label: "Drives", child: true, action: Action::Options(6) },
            NavItem { label: "Exclude", child: true, action: Action::Options(7) },
        ];
        let selected = [1, 3, 4, 5, 6, 9, 10, 11, 7, 2][page as usize];
        let (heading, rows) = match page {
            0 => (
                "Interface",
                [
                    Row::Check("Menus in the title bar, with the application icon", Action::ToggleTitleBar),
                    Row::Gap,
                    Row::Pair("Zoom", format!("{:.0}%", self.scale * 100.0)),
                    Row::Button("Zoom in", Action::ZoomIn),
                    Row::Button("Zoom out", Action::ZoomOut),
                    Row::Button("Reset to the desktop scale", Action::ZoomReset),
                    Row::Gap,
                    Row::Dim("Theme".into()),
                ]
                .into_iter()
                .chain(theme_choices().iter().map(|&(label, _, action)| Row::Check(label, action)))
                .collect(),
            ),
            9 => (
                "Language",
                vec![
                    Row::Check("Follow the system", Action::Language(0)),
                    Row::Check("English", Action::Language(1)),
                    Row::Check("Français", Action::Language(2)),
                    Row::Check("Español", Action::Language(3)),
                    Row::Check("Italiano", Action::Language(4)),
                ],
            ),
            1 => (
                "Search",
                vec![
                    Row::Dim("How the search text is read".into()),
                    Row::Check("Regular expressions", Action::ToggleRegex),
                    Row::Check("Match whole words only", Action::ToggleWord),
                    Row::Check("Match the full path, not just the name", Action::Toggle(Flag::MatchPath)),
                    Row::Check("Match whole filename when using wildcards", Action::Toggle(Flag::WildcardWholeName)),
                    Row::Gap,
                    Row::Dim("Letter case".into()),
                    Row::Check("Smart: exact only when you type an uppercase letter", Action::Case(CaseMode::Smart)),
                    Row::Check("Always case-sensitive", Action::Case(CaseMode::Sensitive)),
                    Row::Check("Never case-sensitive", Action::Case(CaseMode::Insensitive)),
                    Row::Gap,
                    Row::Dim("Show".into()),
                    Row::Check("Files and folders", Action::Show(Kind::Any)),
                    Row::Check("Files only", Action::Show(Kind::File)),
                    Row::Check("Folders only", Action::Show(Kind::Dir)),
                ],
            ),
            2 => (
                "Results",
                vec![
                    Row::Check("Hide results when the search is empty", Action::Toggle(Flag::HideEmpty)),
                    Row::Check("Close window on execute", Action::Toggle(Flag::CloseOnOpen)),
                    Row::Check("Open path with double click in path column", Action::Toggle(Flag::PathDoubleClick)),
                    Row::Check("Double quote copy as path", Action::Toggle(Flag::QuotePaths)),
                    Row::Gap,
                    Row::Dim("First click on a column header".into()),
                    Row::Check("Sort date descending first (newest on top)", Action::Toggle(Flag::DateDescFirst)),
                    Row::Check("Sort size descending first (largest on top)", Action::Toggle(Flag::SizeDescFirst)),
                ],
            ),
            3 => (
                "View",
                vec![
                    Row::Check("Alternate row color", Action::Toggle(Flag::AltRows)),
                    Row::Check("Show row mouseover", Action::Toggle(Flag::RowHover)),
                    Row::Check("Show selected item in status bar", Action::Toggle(Flag::SelectedInStatus)),
                    Row::Check("Show the size map beside the folder sizes", Action::Toggle(Flag::SizeMap)),
                    Row::Check("File icons from the system (as its file manager shows them)", Action::Toggle(Flag::SystemIcons)),
                    Row::Gap,
                    Row::Dim("Size format".into()),
                    Row::Check("Auto (B, KB, MB, GB)", Action::SizeFormat(0)),
                    Row::Check("Bytes", Action::SizeFormat(1)),
                    Row::Check("KB", Action::SizeFormat(2)),
                    Row::Check("MB", Action::SizeFormat(3)),
                ],
            ),
            4 => (
                "Desktop",
                vec![
                    Row::Check("A global shortcut opens Fast search", Action::ToggleShortcut),
                    Row::Pair("Shortcut", self.shortcut_keys.clone()),
                    Row::Dim("To change it, click the box below and press the new keys, such as Alt+Q:".into()),
                    Row::Input("Ctrl+Alt+F", Action::SetShortcut),
                    // What came of the last change, or why it was refused.
                    Row::Dim(self.status.clone()),
                    Row::Check("Show in the applications menu", Action::ToggleLauncher),
                    Row::Check("Closing the window keeps Fast search running, ready for its shortcut", Action::Toggle(Flag::CloseToTray)),
                    Row::Check("Start at sign-in, hidden", Action::ToggleAutostart),
                ],
            ),
            5 => (
                "Live updates",
                vec![
                    Row::Check("Keep the index current with the live updates service", Action::ToggleDaemon),
                    Row::Dim("Switching it asks for your password. Everyone in your group can then".into()),
                    Row::Dim("search the names of all files on this computer.".into()),
                    Row::Gap,
                    Row::Pair("Index file", self.db.display().to_string()),
                    Row::Pair("Indexed", trf("{} files and folders", &[&group_digits(self.index.len() as u64)])),
                    Row::Gap,
                    Row::Button("Rebuild the index now", Action::Reindex),
                ],
            ),
            8 => ("Bookmarks", self.bookmark_rows()),
            7 => {
                let mut rows = vec![
                    Row::Check("Exclude hidden files and folders (names starting with a dot)", Action::ToggleExcludeHidden),
                    Row::Gap,
                    Row::Dim("Excluded folders:".into()),
                ];
                if self.excludes.is_empty() {
                    rows.push(Row::Dim("      (none)".into()));
                }
                for (i, folder) in self.excludes.iter().enumerate().take(8) {
                    rows.push(Row::ListItem(folder.clone(), Action::SelectExclude(i as u8)));
                }
                if !self.excludes.is_empty() {
                    rows.push(Row::Button("Remove the selected folder", Action::RemoveExclude));
                }
                rows.push(Row::Gap);
                rows.push(Row::Dim("Add a folder: type or paste its full path, then press Enter".into()));
                rows.push(Row::Input("/path/to/folder", Action::AddExclude));
                rows.push(Row::Button("Add the folder selected in the results", Action::AddSelectedExclude));
                rows.push(Row::Gap);
                rows.push(Row::Dim("Always excluded: /proc, /sys, /dev, /run, /.snapshots and trash folders.".into()));
                rows.push(Row::Gap);
                rows.push(Row::Dim(self.save_hint().into()));
                ("Exclude", rows)
            }
            _ => {
                let mut rows = vec![
                    Row::Check("Automatically include drives when they are mounted", Action::ToggleAutoDrives),
                    Row::Dim("Drives that go away are always removed from the index.".into()),
                    Row::Gap,
                    Row::Dim("Drives:".into()),
                ];
                if self.drives.is_empty() {
                    rows.push(Row::Dim("No drives found (this needs the lsblk command).".into()));
                }
                for (i, drive) in self.drives.iter().enumerate().take(12) {
                    let place = if drive.mount.is_empty() { tr("not mounted") } else { &drive.mount };
                    let state = tr(if drive.index { "included" } else { "not included" });
                    let line = format!("{}      {}      {}      {place}      {state}", drive.path, drive.fstype, drive.size);
                    rows.push(Row::ListItem(line, Action::SelectDrive(i as u8)));
                }
                if let Some(drive) = self.drives.get(self.drive_selected) {
                    let i = self.drive_selected as u8;
                    rows.push(Row::Gap);
                    rows.push(Row::Dim(trf("Settings for {}", &[&drive.path])));
                    if !drive.can_index {
                        rows.push(Row::Dim("This kind of drive can only be indexed while it is mounted.".into()));
                    } else if drive.mount.is_empty() {
                        rows.push(Row::Check("Include in the index (read directly, without mounting it)", Action::DriveIndex(i)));
                        rows.push(Row::Dim("Its files can be found, but not opened until it is mounted.".into()));
                    } else {
                        rows.push(Row::Check("Include in the index", Action::DriveIndex(i)));
                        rows.push(Row::Check("Monitor changes", Action::DriveLive(i)));
                    }
                }
                rows.push(Row::Gap);
                rows.push(Row::Dim(self.save_hint().into()));
                ("Drives", rows)
            }
        };
        self.options_page = Some(page);
        self.open_paged_dialog("Options", nav, selected, heading, rows)
    }

    /// Opens a page of the help sheet: what to type, with examples.
    fn open_help(&mut self, page: u8) -> io::Result<()> {
        let pair = |example: &'static str, meaning: &str| Row::Pair(example, meaning.to_string());
        let head = |title: &str| Row::Dim(title.to_string());
        let (title, rows) = match page {
            0 => (
                "Search syntax",
                vec![
                    Row::Text("Type words: results contain all of them, in any order.".into()),
                    Row::Gap,
                    pair("report", "names containing \"report\""),
                    pair("report 2026", "names containing both words"),
                    pair("\"my file\"", "quotes keep a space inside a word"),
                    pair("*.pdf", "wildcard on the whole name: * any text, ? one character"),
                    pair("src/main", "a slash searches the full path, not just the name"),
                    pair("!draft", "names that do not contain \"draft\""),
                    pair("Report", "an uppercase letter makes that word case-sensitive"),
                    Row::Gap,
                    head("Options (also in the View menu)"),
                    pair("Ctrl+R", "regular expressions, such as  ^IMG_\\d+\\.jpe?g$"),
                    pair("Ctrl+B", "whole words only: \"port\" finds \"port 80\", not \"report\""),
                    pair("F3", "letter case: smart, sensitive, insensitive"),
                    pair("F2", "show everything, files only, or folders only"),
                ],
            ),
            1 => (
                "Filters",
                vec![
                    Row::Text("Filters narrow the results; mix them with words.".into()),
                    Row::Gap,
                    head("By extension"),
                    pair("ext:pdf", "PDF files"),
                    pair("ext:jpg;png;gif", "any of several extensions"),
                    pair("ext:tar.gz", "compound extensions"),
                    head("By size  (K, M, G, T;  1K = 1024 bytes)"),
                    pair("size:>100M", "larger than 100 MB  (also  >=  <  <=)"),
                    pair("size:10M..1G", "between 10 MB and 1 GB"),
                    pair("size:0", "empty files"),
                    head("By date modified  (dm: for short)"),
                    pair("modified:today", "since midnight  (also  yesterday)"),
                    pair("modified:3h", "in the last 3 hours"),
                    pair("modified:7d", "in the last 7 days"),
                    pair("modified:2026-10-24", "on that day"),
                    pair("modified:2026-01-01..2026-06-30", "between two dates"),
                    pair("modified:2026-01-01..", "since a date   ( ..date  means before )"),
                    head("Together"),
                    pair("invoice ext:pdf size:>1M dm:30d", "big PDF invoices from the last month"),
                ],
            ),
            _ => (
                "Keyboard shortcuts",
                vec![
                    pair("Up, Down, PgUp, PgDn", "move through the results"),
                    pair("Ctrl+Home, Ctrl+End", "first and last result"),
                    pair("Shift + those keys", "select several results  (also Shift+click, Ctrl+click)"),
                    pair("Ctrl+Shift+A", "select all results"),
                    pair("Enter", "open                        (double-click)"),
                    pair("F4", "open in the text editor"),
                    pair("Ctrl+Enter", "show in the file manager    (double-click the path)"),
                    pair("Alt+Enter", "properties"),
                    pair("Ctrl+C", "copy the files, to paste in a file manager"),
                    pair("Ctrl+Shift+C", "copy the full paths"),
                    pair("Del", "move to the trash  (asks first)"),
                    pair("Shift+Del", "delete permanently  (asks first)"),
                    pair("Drag", "move to another window; hold Ctrl to copy"),
                    pair("Ctrl+V, Ctrl+X, Ctrl+A", "paste, cut, select all in the search box"),
                    pair("Ctrl+R, Ctrl+B", "regular expressions, whole words"),
                    pair("F2, F3", "files or folders, letter case"),
                    pair("Alt+Down", "complete a filter and its values"),
                    pair("Ctrl+D, Alt+1 to Alt+9", "bookmark this search, open a bookmark"),
                    pair("Ctrl+S", "export the results as a CSV file"),
                    pair("F5", "rebuild the index"),
                    pair("F6", "folder sizes: what takes the space"),
                    pair("F7", "show or hide the size map beside the folder sizes"),
                    pair("Ctrl +, Ctrl -, Ctrl 0", "zoom in, out, reset"),
                    pair("Esc", "clear the search box; quit when it is empty"),
                ],
            ),
        };
        // Buttons lead to the other pages; Close has the focus.
        let pages: [crate::dialog::Button; 3] = [("Search syntax", Some(Action::Help(0))), ("Filters", Some(Action::Help(1))), ("Shortcuts", Some(Action::Help(2)))];
        let mut buttons: Vec<crate::dialog::Button> = pages.into_iter().enumerate().filter(|(i, _)| *i as u8 != page.min(2)).map(|(_, b)| b).collect();
        buttons.push(("Close", None));
        let focus = buttons.len() - 1;
        self.open_dialog(title, rows, buttons, focus, false)?;
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.sheet = true;
        }
        self.render_dialog()
    }

    /// Re-reads the state of the options that live outside this program.
    fn refresh_option_states(&mut self) {
        self.shortcut_on = settings::shortcut_enabled();
        if let Some(keys) = settings::current_shortcut() {
            self.shortcut_keys = keys;
        }
        self.launcher_on = settings::launcher_installed();
        self.daemon_on = settings::daemon_installed();
    }

    /// Makes the results reflect that `paths` were just removed, without
    /// waiting for the next index refresh.
    /// Deletes `paths` (index entries `ids`) on a worker thread, so the
    /// window stays responsive: to the trash, or for good. On Windows,
    /// Windows itself does it, with its own confirmation and progress.
    /// The outcome comes back through `deleting` (see `finish_delete`).
    fn start_delete(&mut self, ids: Vec<u32>, paths: Vec<PathBuf>, trash: bool) {
        if paths.is_empty() || self.deleting.is_some() {
            return;
        }
        let count = paths.len();
        self.status = if trash { trf("Moving {} items to the trash...", &[&count]) } else { trf("Deleting {} items...", &[&count]) };
        let (tx, rx) = mpsc::channel();
        self.deleting = Some(rx);
        #[cfg(windows)]
        let owner = self.b.native_window();
        std::thread::spawn(move || {
            #[cfg(windows)]
            let outcome = match ff_core::win::shell_delete(&paths, !trash, owner) {
                Ok(true) => None,
                Ok(false) => Some(tr("Cancelled").to_string()),
                Err(e) => Some(e.to_string()),
            };
            #[cfg(not(windows))]
            let outcome = {
                let failed: Vec<String> = paths
                    .iter()
                    .filter_map(|p| (if trash { desktop::trash(p) } else { desktop::delete(p) }).err().map(|e| format!("{}: {e}", p.display())))
                    .collect();
                failed.first().map(|first| format!("{} failed; first: {first}", failed.len()))
            };
            let _ = tx.send(Deleted { ids, paths, trash, problem: outcome });
        });
    }

    /// Takes a finished delete's outcome, if one is ready (or, with
    /// `wait`, waits for it): what is really gone leaves the results and
    /// the sizes tree at once, without rebuilding the index.
    fn finish_delete(&mut self, wait: bool) -> bool {
        let Some(rx) = &self.deleting else { return false };
        let done = if wait { rx.recv().ok() } else { rx.try_recv().ok() };
        let Some(done) = done else {
            // The worker died without a word: stop waiting for it.
            if wait || matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                self.deleting = None;
            }
            return false;
        };
        self.deleting = None;
        // Only what is really gone, and still the same entry.
        let mut gone = Vec::new();
        for (&id, path) in done.ids.iter().zip(&done.paths) {
            let same = (id as usize) < self.index.len() && self.index.path(id) == *path;
            if same && std::fs::symlink_metadata(path).is_err() {
                gone.push(id);
            }
        }
        if let Some(tree) = self.tree.as_mut() {
            for &id in &gone {
                tree.remove(&self.index, id);
            }
        }
        self.quiesce();
        self.index.remove(&gone);
        if let Some(build) = &mut self.building {
            build.forgotten.extend(done.paths.iter().filter(|p| std::fs::symlink_metadata(p).is_err()).cloned());
        }
        let (sel, top) = (self.sel, self.top);
        self.search();
        self.sel = sel.min(self.hits.len().saturating_sub(1));
        self.top = top.min(self.max_top());
        let what = match &done.paths[..] {
            [one] if gone.len() == 1 => one.display().to_string(),
            _ => trf("{} items", &[&gone.len()]),
        };
        self.status = match (done.problem, done.trash) {
            (Some(problem), _) if gone.is_empty() => problem,
            (Some(problem), _) => format!("{} {}      {problem}", gone.len(), tr("removed")),
            (None, true) => trf("Moved {} to the trash", &[&what]),
            (None, false) => trf("Deleted {}", &[&what]),
        };
        true
    }

    fn forget(&mut self, paths: &[PathBuf]) {
        if let Some(build) = &mut self.building {
            build.forgotten.extend_from_slice(paths);
        }
        let changes: std::collections::HashSet<_> = paths
            .iter()
            .filter_map(|p| Some((ff_core::os::path_bytes(p.parent()?).to_vec(), ff_core::os::bytes(p.file_name()?).to_vec())))
            .collect();
        // Checks the disk: an entry only goes if the file really is gone.
        // It is marked gone in place, so ids held elsewhere stay valid.
        let kept = self.keep_tree();
        self.quiesce();
        match &mut self.follow {
            Some(f) => {
                f.apply(&mut self.index, &changes);
            }
            None => {
                ff_core::live::Tracker::new(&self.index).apply(&mut self.index, &changes);
            }
        }
        self.restore_tree(kept);
        let (sel, top) = (self.sel, self.top);
        self.search();
        self.sel = sel.min(self.hits.len().saturating_sub(1));
        self.top = top.min(self.max_top());
    }

    /// Switches the texts to the chosen language.
    fn apply_language(&mut self) {
        self.redraw_all = true;
        i18n::set_language(match self.language {
            0 => i18n::system_language(),
            chosen => chosen as usize - 1,
        });
    }

    /// Switches the palette to the chosen theme; "follow the desktop"
    /// asks the desktop whether it prefers dark.
    fn apply_theme(&mut self) {
        self.redraw_all = true;
        // FF_GUI_THEME=<first word of a theme's name> pins it, for test
        // pictures: light, dark, graphite, ...
        let pinned = std::env::var("FF_GUI_THEME").ok().and_then(|t| {
            canvas::THEMES.iter().position(|th| th.name.split(' ').next().unwrap_or("").eq_ignore_ascii_case(&t))
        });
        // Theme 0 follows the desktop (Light or Dark); n is THEMES[n - 1].
        let index = match pinned {
            Some(i) => i,
            None if self.theme == 0 => settings::desktop_prefers_dark() as usize,
            None => self.theme as usize - 1,
        };
        canvas::set_theme(index);
        // A colour chosen here, else the desktop's: on Windows the accent
        // colour of its personalization settings (stored as 0xAABBGGRR),
        // elsewhere the first of the list.
        if let Some(&(_, rgb)) = (self.accent as usize).checked_sub(1).and_then(|i| canvas::ACCENTS.get(i)) {
            canvas::set_accent(rgb);
            return;
        }
        canvas::set_accent(canvas::ACCENTS[0].1);
        #[cfg(windows)]
        if let Some(abgr) = winsys::reg_dword(winsys::HKEY_CURRENT_USER, r"Software\Microsoft\Windows\DWM", "AccentColor") {
            canvas::set_accent((abgr & 0xff) << 16 | (abgr & 0xff00) | (abgr >> 16 & 0xff));
        }
    }

    /// The line on the index pages saying how their changes are saved.
    fn save_hint(&self) -> &'static str {
        match (self.index_settings_dirty, settings::daemon_installed()) {
            (true, true) => "Not saved yet: press Apply or OK (asks for your password, rebuilds the index).",
            (true, false) => "Not saved yet: press Apply or OK (rebuilds the index).",
            (false, true) => "Changes here are saved with Apply or OK, which asks for your password.",
            (false, false) => "Changes here are saved with Apply or OK, which rebuilds the index.",
        }
    }

    /// Fills the Drives and Exclude pages from the current settings, once
    /// per opening of the Options window; after that they hold the edits.
    fn load_index_settings(&mut self) {
        if std::mem::replace(&mut self.index_settings_loaded, true) {
            return;
        }
        self.index_settings_dirty = false;
        self.drives = settings::drives();
        self.auto_drives = settings::drives_automatic();
        self.drive_selected = 0;
        // The index records what was left out when it was built; the
        // standard exclusions are not listed.
        let standard = BuildOptions::new(["/"]).excludes;
        self.excludes = self.index.excludes().filter(|x| !standard.contains(x)).map(|x| x.to_string_lossy().into_owned()).collect();
        self.exclude_selected = 0;
        self.exclude_hidden = self.index.skips_hidden();
    }

    fn flag(&self, flag: Flag) -> bool {
        self.flags[flag as usize]
    }

    /// A file size as the Size column shows it.
    fn size_text(&self, bytes: u64) -> String {
        match self.size_format {
            1 => format!("{} B", group_digits(bytes)),
            3 => format!("{} MB", group_digits(bytes.div_ceil(1 << 20))),
            0 => {
                // Automatic: the largest unit that keeps the number readable.
                let (mut value, mut unit) = (bytes as f64, "B");
                for next in ["KB", "MB", "GB", "TB"] {
                    if value < 1024.0 {
                        break;
                    }
                    value /= 1024.0;
                    unit = next;
                }
                if unit == "B" { format!("{bytes} B") } else { format!("{value:.1} {unit}") }
            }
            _ => format!("{} KB", group_digits(bytes.div_ceil(1024))),
        }
    }

    /// True if a toggle or choice in a menu is currently on.
    fn checked(&self, action: Action) -> bool {
        match action {
            Action::Show(kind) => kind == self.kind,
            Action::ToggleRegex => self.regex,
            Action::Toggle(flag) => self.flag(flag),
            Action::SizeFormat(format) => format == self.size_format,
            Action::Theme(theme) => theme == self.theme,
            Action::Accent(accent) => accent == self.accent,
            Action::Language(language) => language == self.language,
            Action::SelectExclude(i) => i as usize == self.exclude_selected,
            Action::SelectBookmark(i) => i as usize == self.bookmark_selected,
            Action::ToggleExcludeHidden => self.exclude_hidden,
            Action::SelectDrive(i) => i as usize == self.drive_selected,
            Action::ToggleAutoDrives => self.auto_drives,
            Action::DriveIndex(i) => self.drives.get(i as usize).is_some_and(|d| d.index),
            Action::DriveLive(i) => self.drives.get(i as usize).is_some_and(|d| d.live && d.index),
            Action::ToggleWord => self.whole_word,
            Action::TreeSize => self.tree.is_some(),
            Action::Case(case) => case == self.case,
            Action::ToggleTitleBar => self.custom_title,
            Action::ToggleAutostart => settings::autostart_enabled(),
            Action::ToggleShortcut => self.shortcut_on,
            Action::ToggleLauncher => self.launcher_on,
            Action::ToggleDaemon => self.daemon_on,
            _ => false,
        }
    }

    /// Registers the global shortcut with the running program (Windows),
    /// or drops it when it is off.
    fn apply_hotkey(&mut self) {
        let keys = settings::shortcut_enabled().then(settings::current_shortcut).flatten();
        if let Err(e) = self.b.set_hotkey(keys.as_deref()) {
            self.status = e;
        }
    }

    /// Tells the desktop whether we draw the title bar and frame.
    fn apply_title_bar(&mut self) {
        self.custom_title = self.b.set_own_title_bar(self.custom_title);
    }

    fn save_config(&mut self) {
        // Only a zoom the user chose is saved; otherwise we keep following the desktop.
        let scale = (self.scale != self.auto_scale).then_some(self.scale);
        let case = ["smart", "sensitive", "insensitive"][self.case as usize].to_string();
        let mut more: std::collections::BTreeMap<String, String> =
            FLAGS.iter().zip(self.flags).map(|((key, _), on)| (key.to_string(), if on { "on" } else { "off" }.to_string())).collect();
        more.insert("size_format".into(), self.size_format.to_string());
        more.insert("theme".into(), self.theme.to_string());
        more.insert("accent".into(), self.accent.to_string());
        more.insert("language".into(), self.language.to_string());
        more.insert("shortcut".into(), self.shortcut_keys.clone());
        if let Some(w) = self.column_widths {
            more.insert("column_widths".into(), format!("{:.0},{:.0},{:.0},{:.0}", w[0], w[1], w[2], w[3]));
        }
        let config = Config { scale, own_title_bar: self.custom_title, regex: self.regex, whole_word: self.whole_word, case, more };
        if let Err(e) = config.save() {
            self.status = format!("could not save settings: {e}");
        }
    }

    /// Changes the zoom (`None`: back to the desktop scale) and remembers it.
    fn set_scale(&mut self, scale: Option<f32>) {
        let scale = scale.unwrap_or(self.auto_scale).clamp(0.75, 4.0);
        let Ok(mut text) = Text::load((FONT_PX * scale).round()) else { return };
        self.scale = scale;
        self.redraw_all = true;
        self.u = (scale.round() as i32).max(1);
        self.size_w = text.width("000,000,000 KB") + 16 * self.u;
        self.date_w = text.width("0000-00-00 00:00") + 16 * self.u;
        self.text = text;
        self.icons = Icons::new((16 * self.u) as usize);
        self.about_icon = Icons::new((72 * self.u) as usize);
        self.menu = None;
        self.status = trf("Zoom {}%", &[&format!("{:.0}", scale * 100.0)]);
        self.save_config();
    }

    /// Closes the properties or About panel; true if one was open.
    fn close_panel(&mut self) -> bool {
        self.props.take().is_some() | std::mem::take(&mut self.about)
    }

    // ---- dragging results out (XDND source) ---------------------------

    /// The small image of the dragged results that follows the pointer:
    /// an icon and a name (or a count).
    fn drag_image(&mut self, label: &str, icon: Icon) -> Canvas {
        let u = self.u;
        let size = self.icons.size as i32;
        let w = (size + self.text.width(label) + 26 * u).min(420 * u);
        let h = self.row_h() + 6 * u;
        let mut cv = Canvas::new();
        cv.resize(w as usize, h as usize);
        cv.fill(0, 0, w, h, 0xf4f8fe);
        cv.frame(0, 0, w, h, u.max(1), 0x6fa0e0);
        self.icons.draw(&mut cv, 8 * u, (h - size) / 2, icon);
        self.text.draw(&mut cv, 14 * u + size, (h - self.text.height) / 2, label, w - size - 22 * u, TEXT);
        cv
    }

    // ---- our own title bar --------------------------------------------

    fn title_button_at(&self, x: i32, y: i32) -> Option<usize> {
        let (w, bw) = (self.cv.w as i32, TITLE_BUTTON * self.u);
        (self.custom_title && y >= 0 && y < self.menu_h() && x >= w - 3 * bw && x < w).then(|| ((x - (w - 3 * bw)) / bw) as usize)
    }

    /// Resize direction if (x, y) is on the window's edge.
    /// The pointer shape for a position in the main window.
    fn cursor_at(&mut self, x: i32, y: i32) -> Cursor {
        let covered = self.menu.is_some() || self.props.is_some() || self.about || self.dialog.is_some();
        let on_frame = self.custom_title && self.title_button_at(x, y).is_none();
        if let Some(edge) = self.resize_edge(x, y).filter(|_| on_frame && !covered) {
            Cursor::Resize(edge)
        } else if !covered && self.column_edge_at(x, y).is_some() {
            Cursor::Resize(3) // a column edge: left-right arrows
        } else if !covered && self.tree.is_none() && y >= self.menu_h() + 6 * self.u && y < self.box_h() - 6 * self.u && x >= 6 * self.u && x < self.cv.w as i32 - 6 * self.u {
            Cursor::Text // the search box
        } else {
            Cursor::Arrow
        }
    }

    fn resize_edge(&self, x: i32, y: i32) -> Option<u32> {
        let (w, h, m) = (self.cv.w as i32, self.cv.h as i32, 5 * self.u);
        // The bottom-right corner is larger: it shows a grip to grab.
        if x >= w - GRIP * self.u && y >= h - GRIP * self.u {
            return Some(4);
        }
        let (left, right, top, bottom) = (x < m, x >= w - m, y < m, y >= h - m);
        // _NET_WM_MOVERESIZE: 0 top-left, then clockwise to 7 left.
        match (left, right, top, bottom) {
            (true, _, true, _) => Some(0),
            (_, true, true, _) => Some(2),
            (_, true, _, true) => Some(4),
            (true, _, _, true) => Some(6),
            (_, _, true, _) => Some(1),
            (_, true, _, _) => Some(3),
            (_, _, _, true) => Some(5),
            (true, _, _, _) => Some(7),
            _ => None,
        }
    }

    /// Closes the current window as the user asked (its close button,
    /// Alt+F4, Esc on an empty search), ending its search. Another
    /// window is closed for good; the first hides (on Windows to the
    /// notification area), to come back at once, empty, with the shortcut.
    /// True if it did; else the program should exit.
    fn dismiss(&mut self) -> bool {
        self.menu = None;
        if self.current != 0 {
            self.close_current_window();
            return true;
        }
        let others = self.windows.iter().skip(1).any(Option::is_some);
        self.hidden = (self.flag(Flag::CloseToTray) || others) && self.b.hide_to_tray();
        if self.hidden {
            self.reset_session();
        }
        self.hidden
    }

    /// A new window's state: an empty search with the current options.
    fn blank_session(&self) -> Session {
        Session {
            cv: Canvas::new(),
            drag_arm: None,
            column_drag: None,
            query: String::new(),
            cursor: 0,
            anchor: 0,
            box_scroll: 0,
            selecting: false,
            last_box_click: 0,
            hits: Vec::new(),
            sel: 0,
            marks: BTreeSet::new(),
            anchor_row: 0,
            collapse_on_release: None,
            top: 0,
            kind: self.kind,
            case: self.case,
            regex: self.regex,
            whole_word: self.whole_word,
            query_error: None,
            sort: self.sort,
            reverse: self.reverse,
            status: String::new(),
            menu: None,
            props: None,
            about: false,
            hover_button: None,
            last_title_click: 0,
            dragging_scrollbar: false,
            hover_row: None,
            results_stale: false,
            tree_refreshed: Instant::now(),
            last_click: (0, usize::MAX),
            tree: None,
            tiles: Vec::new(),
            disks: Vec::new(),
            tile_hover: None,
            complete: None,
            pending_draw: true,
            search_pending: false,
        }
    }

    /// The results of an empty search with the options of `blank`, from a
    /// window that shows exactly that, if one does.
    fn empty_results(&self, blank: &Session) -> Option<Vec<u32>> {
        let same = |query: &str, error: &Option<String>, stale: bool, opts: (Kind, CaseMode, bool, bool, Sort, bool)| {
            query.is_empty() && error.is_none() && !stale && opts == (blank.kind, blank.case, blank.regex, blank.whole_word, blank.sort, blank.reverse)
        };
        if same(&self.query, &self.query_error, self.results_stale, (self.kind, self.case, self.regex, self.whole_word, self.sort, self.reverse)) {
            return Some(self.hits.clone());
        }
        self.windows
            .iter()
            .flatten()
            .find(|w| same(&w.query, &w.query_error, w.results_stale, (w.kind, w.case, w.regex, w.whole_word, w.sort, w.reverse)))
            .map(|w| w.hits.clone())
    }

    /// Ends the current window's search: it starts again empty (its size
    /// kept).
    fn reset_session(&mut self) {
        let mut blank = self.blank_session();
        std::mem::swap(&mut blank.cv, &mut self.cv);
        self.swap_session(&mut blank);
        self.search();
    }

    /// Makes window `n` the current one: its state moves into the fields.
    fn switch_to(&mut self, n: u8) {
        if n == self.current {
            return;
        }
        let Some(mut s) = self.windows.get_mut(n as usize).and_then(Option::take) else { return };
        self.swap_session(&mut s);
        let old = self.current as usize;
        self.windows[old] = Some(s);
        self.current = n;
        self.b.select(n);
    }

    /// The shortcut was pressed (or the program started again): the first
    /// window shows if it was closed, else another window opens, each with
    /// a search of its own.
    fn summoned(&mut self) {
        if self.hidden {
            self.switch_to(0);
            self.b.show();
            self.hidden = false;
            return;
        }
        match self.b.open_window() {
            Ok(n) => {
                let n = n as usize;
                if self.windows.len() <= n {
                    self.windows.resize_with(n + 1, || None);
                }
                let mut blank = self.blank_session();
                // An empty search lists millions of entries: take the list
                // from a window that has it rather than search again, so the
                // window shows at once.
                let ready = self.empty_results(&blank);
                if let Some(hits) = &ready {
                    blank.hits = hits.clone();
                }
                self.windows[n] = Some(blank);
                self.switch_to(n as u8);
                if ready.is_none() {
                    self.search();
                }
            }
            // A desktop that cannot open another: a fresh search here.
            Err(_) => {
                self.b.show();
                self.reset_session();
            }
        }
    }

    /// Closes the current window (not the first) for good; the first
    /// becomes current.
    fn close_current_window(&mut self) {
        let closing = self.current;
        if closing == 0 {
            return;
        }
        self.b.close_window(closing);
        let Some(mut first) = self.windows[0].take() else { return };
        self.swap_session(&mut first); // the closed window's state is dropped
        self.current = 0;
        self.b.select(0);
        if self.windows.len() > closing as usize {
            self.windows[closing as usize] = None;
        }
        self.pending_draw = true;
    }

    /// Runs `f` for every open window, each made current in turn, then
    /// makes the original current again.
    fn each_window(&mut self, mut f: impl FnMut(&mut App)) {
        let start = self.current;
        let open: Vec<u8> = (0..self.windows.len() as u8).filter(|&n| n == start || self.windows[n as usize].is_some()).collect();
        for n in open {
            self.switch_to(n);
            f(self);
        }
        self.switch_to(start);
    }

    /// Draws every open window (after something they all show changed).
    fn render_all(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        self.each_window(|app| {
            if app.current == 0 && app.hidden {
                return;
            }
            if let Err(e) = app.render() {
                result = Err(e);
            }
            app.pending_draw = false;
        });
        result
    }

    /// The index was replaced: every window searches it again.
    fn research_all(&mut self) {
        let current = self.current;
        self.each_window(|app| {
            if app.current != current {
                let kept = app.keep_tree();
                app.restore_tree(kept);
                app.search();
            }
        });
        self.redraw_all = true;
    }

    /// A left press on the title bar or window edge; true if handled.
    fn press_frame(&mut self, x: i32, y: i32, time: u32) -> bool {
        if !self.custom_title {
            return false;
        }
        if let Some(button) = self.title_button_at(x, y) {
            match button {
                0 => self.b.minimize(),
                1 => self.b.toggle_maximize(),
                _ => self.quit = !self.dismiss(),
            }
        } else if let Some(edge) = self.resize_edge(x, y) {
            self.b.start_resize(edge);
        } else if y < self.menu_h() && self.bar_title_at(x, y).is_none() && self.bar_toggle_at(x, y).is_none() {
            // The menu bar's empty space is the title bar: drag to move.
            if time.wrapping_sub(self.last_title_click) < 400 {
                self.b.toggle_maximize();
            } else {
                self.b.start_move();
            }
            self.last_title_click = time;
        } else {
            return false;
        }
        true
    }

    /// Our title bar is the menu bar itself: the application icon at its
    /// left, the window buttons at its right.
    fn draw_title_bar(&mut self) {
        let (w, th, u) = (self.cv.w as i32, self.menu_h(), self.u);
        let icon = self.icons.size as i32;
        self.icons.draw(&mut self.cv, 10 * u, (th - icon) / 2, Icon::App);
        let bw = TITLE_BUTTON * u;
        for button in 0..3 {
            let x = w - (3 - button) * bw;
            let hovered = self.hover_button == Some(button as usize);
            let ink = if hovered && button == 2 { ON_ACCENT } else { TEXT };
            if hovered {
                self.cv.fill(x, 0, bw, th, if button == 2 { 0xc42b1c } else { HOVER });
            }
            // 10-unit glyphs centred in the button.
            let (gx, gy, g, t) = (x + (bw - 10 * u) / 2, (th - 10 * u) / 2, 10 * u, u.max(1));
            match button {
                0 => self.cv.fill(gx, gy + g - 2 * t, g, t, ink),
                1 => self.cv.frame(gx, gy, g, g, t, ink),
                _ => {
                    for i in 0..g {
                        self.cv.fill(gx + i, gy + i, t, t, ink);
                        self.cv.fill(gx + g - 1 - i, gy + i, t, t, ink);
                    }
                }
            }
        }
    }

    /// (x, width) of each menu-bar title.
    fn bar_titles(&mut self) -> [(i32, i32); BAR.len()] {
        // After the application icon when the menu bar is the title bar.
        let mut x = if self.custom_title { 16 * self.u + self.icons.size as i32 } else { 4 * self.u };
        BAR.map(|(title, _)| {
            let w = self.text.width(tr(title)) + 18 * self.u;
            x += w;
            (x - w, w)
        })
    }

    /// Toggle buttons at the right end of the menu bar, as (x, width,
    /// label, action): Folder sizes, and Size map while the sizes are
    /// shown. Left out when the window is too narrow to fit them.
    fn bar_toggles(&mut self) -> Vec<(i32, i32, &'static str, Action)> {
        let mut buttons: Vec<(&'static str, Action)> = vec![("Folder sizes", Action::TreeSize)];
        if self.tree.is_some() {
            buttons.push(("Size map", Action::Toggle(Flag::SizeMap)));
        }
        let (u, titles_end) = (self.u, self.bar_titles().last().map_or(0, |&(x, w)| x + w));
        // Left of the window buttons when the menu bar is the title bar.
        let buttons_w = if self.custom_title { 3 * TITLE_BUTTON * u } else { 0 };
        let mut x = self.cv.w as i32 - buttons_w - 6 * u;
        let mut out = Vec::new();
        for (label, action) in buttons.into_iter().rev() {
            let w = self.text.width(tr(label)) + 20 * u;
            x -= w;
            out.push((x, w, label, action));
            x -= 6 * u;
        }
        if x < titles_end + 12 * u {
            return Vec::new();
        }
        out.reverse();
        out
    }

    fn bar_toggle_at(&mut self, x: i32, y: i32) -> Option<Action> {
        if y < self.title_h() || y >= self.menu_h() {
            return None;
        }
        self.bar_toggles().into_iter().find(|&(bx, bw, _, _)| x >= bx && x < bx + bw).map(|(_, _, _, action)| action)
    }

    fn bar_title_at(&mut self, x: i32, y: i32) -> Option<usize> {
        let titles = self.bar_titles();
        titles.iter().position(|&(tx, tw)| y >= self.title_h() && y < self.menu_h() && x >= tx && x < tx + tw)
    }

    fn open_bar_menu(&mut self, i: usize) {
        if BAR[i].0 == "Options" {
            self.refresh_option_states();
        }
        let (x, _) = self.bar_titles()[i];
        let items = match BAR[i].0 {
            "Bookmarks" => self.bookmark_menu(),
            "Theme" => theme_menu(),
            _ => BAR[i].1,
        };
        self.menu = Some(Menu { x, y: self.menu_h(), hover: None, items, bar: Some(i) });
    }

    fn rows_visible(&self) -> usize {
        ((self.cv.h as i32 - self.list_top() - self.row_h()) / self.row_h()).max(1) as usize
    }

    /// Rows in the list shown: the results, or the folder sizes tree.
    fn list_len(&self) -> usize {
        self.tree.as_ref().map_or(self.hits.len(), |t| t.rows.len())
    }

    /// The first visible row of that list.
    fn top_mut(&mut self) -> &mut usize {
        match &mut self.tree {
            Some(tree) => &mut tree.top,
            None => &mut self.top,
        }
    }

    fn max_top(&self) -> usize {
        self.list_len().saturating_sub(self.rows_visible())
    }

    fn is_selected(&self, row: usize) -> bool {
        if self.marks.is_empty() { row == self.sel } else { self.marks.contains(&row) }
    }

    /// The selected rows, in list order.
    fn selected_rows(&self) -> Vec<usize> {
        if self.marks.is_empty() {
            return if self.sel < self.hits.len() { vec![self.sel] } else { Vec::new() };
        }
        self.marks.iter().copied().filter(|&r| r < self.hits.len()).collect()
    }

    /// An entry's path, and whether it is a whole drive (a root such as
    /// C:\ or /): for its icon.
    fn entry_path(&self, id: u32) -> (PathBuf, bool) {
        let path = self.index.path(id);
        let drive = self.index.parent(id).is_none() && path.parent().is_none();
        (path, drive)
    }

    /// The selected rows' index entries and paths.
    fn selected_entries(&self) -> (Vec<u32>, Vec<PathBuf>) {
        self.selected_rows().into_iter().map(|r| (self.hits[r], self.index.path(self.hits[r]))).unzip()
    }

    fn selected_paths(&self) -> Vec<PathBuf> {
        self.selected_rows().into_iter().map(|r| self.index.path(self.hits[r])).collect()
    }

    /// Makes `row` the only selected row.
    fn select_only(&mut self, row: usize) {
        self.sel = row;
        self.anchor_row = row;
        self.marks.clear();
    }

    /// Selects everything from where the selection started up to `row`.
    fn select_range_to(&mut self, row: usize) {
        self.sel = row;
        let (a, b) = (self.anchor_row.min(row), self.anchor_row.max(row));
        self.marks = (a..=b).collect();
    }

    /// Adds `row` to the selection or takes it out (Ctrl+click).
    fn toggle_row(&mut self, row: usize) {
        if self.marks.is_empty() && self.sel < self.hits.len() {
            self.marks.insert(self.sel);
        }
        if !self.marks.remove(&row) {
            self.marks.insert(row);
        }
        self.sel = row;
        self.anchor_row = row;
    }

    /// Moves the current row; `extend` (Shift) grows the selection to it.
    fn move_sel(&mut self, delta: isize, extend: bool) {
        let last = self.hits.len().saturating_sub(1);
        let to = self.sel.saturating_add_signed(delta).min(last);
        if extend { self.select_range_to(to) } else { self.select_only(to) }
        let rows = self.rows_visible();
        if self.sel < self.top {
            self.top = self.sel;
        } else if self.sel >= self.top + rows {
            self.top = self.sel + 1 - rows;
        }
    }

    /// Handles a key press; returns false to quit.
    fn key(&mut self, key: Key, mods: Mods) -> io::Result<bool> {
        let plain = self.menu.is_none() && self.tree.is_none() && self.props.is_none() && !self.about;
        if plain && self.completion_key(key, mods) {
            return Ok(true);
        }
        // Alt+Down (or Ctrl+Space) lists what can complete the word.
        let asked = matches!(key, Key::Sym(K_DOWN) if mods.alt) || matches!(key, Key::Char(' ') if mods.ctrl);
        if plain && asked {
            self.update_completion();
            if self.complete.is_none() {
                self.status = tr("Nothing to complete here: try it on an empty word, or after ext:  size:  modified:").into();
            }
            return Ok(true);
        }
        let was_open = self.complete.is_some();
        let (text, caret) = (self.query.clone(), self.cursor);
        let go_on = self.key_inner(key, mods)?;
        // An open list follows the typing; any other key closes it.
        if was_open {
            if (text.as_str(), caret) != (self.query.as_str(), self.cursor) && self.tree.is_none() {
                self.update_completion();
            } else {
                self.complete = None;
            }
        }
        Ok(go_on)
    }

    fn key_inner(&mut self, key: Key, mods: Mods) -> io::Result<bool> {
        let time = 0;
        if self.close_panel() {
            return Ok(true); // any key closes a panel
        }
        if let Some(mut menu) = self.menu.take() {
            let n = menu.items.len();
            match key {
                Key::Sym(K_LEFT | K_RIGHT) if menu.bar.is_some() => {
                    let step = if matches!(key, Key::Sym(K_LEFT)) { BAR.len() - 1 } else { 1 };
                    self.open_bar_menu((menu.bar.unwrap() + step) % BAR.len());
                    self.menu.as_mut().unwrap().hover = Some(0);
                    return Ok(true);
                }
                Key::Sym(K_UP) => menu.hover = Some(menu.hover.map_or(n - 1, |h| (h + n - 1) % n)),
                Key::Sym(K_DOWN) => menu.hover = Some(menu.hover.map_or(0, |h| (h + 1) % n)),
                Key::Sym(K_RETURN | K_KP_ENTER) => {
                    if let Some(h) = menu.hover {
                        self.act(menu.items[h].2, time);
                    }
                    return Ok(true);
                }
                _ => return Ok(true), // anything else closes the menu
            }
            self.menu = Some(menu);
            return Ok(true);
        }

        if let Some(done) = self.tree_key(key, mods) {
            return done;
        }
        let (ctrl, alt, shift) = (mods.ctrl, mods.alt, mods.shift);
        let page = self.rows_visible() as isize;
        let (sel_start, sel_end) = self.selection();
        self.status.clear();
        match key {
            Key::Char(ch) if ctrl => match ch.to_ascii_lowercase() {
                'q' => return Ok(false),
                'p' => self.act(Action::Options(0), time),
                'd' => self.act(Action::AddBookmark, time),
                's' => self.act(Action::ExportCsv, time),
                'r' => self.act(Action::ToggleRegex, time),
                'b' => self.act(Action::ToggleWord, time),
                '+' | '=' => self.act(Action::ZoomIn, time),
                '-' => self.act(Action::ZoomOut, time),
                '0' => self.act(Action::ZoomReset, time),
                'a' if shift => self.act(Action::SelectAllResults, time),
                'a' => self.act(Action::SelectAll, time),
                'c' if shift => self.act(Action::CopyPath, time),
                'c' => self.act(Action::Copy, time),
                'x' => self.act(Action::Cut, time),
                'v' => self.act(Action::Paste, time),
                'u' => self.clear_query(),
                'w' => self.delete_to(self.step(self.cursor, false, true)),
                _ => {}
            },
            Key::Char(ch @ '1'..='9') if alt => self.act(Action::Bookmark(ch as u8 - b'1'), time),
            Key::Char(ch) if !alt && !ch.is_control() => self.insert(ch.encode_utf8(&mut [0; 4])),
            Key::Sym(K_BACKSPACE) => self.delete_to(self.step(self.cursor, false, ctrl)),
            // With nothing after the caret to delete, Delete means the
            // selected result: to the trash, or for good with Shift.
            Key::Sym(K_DELETE) if self.cursor == self.query.len() && sel_start == sel_end => {
                self.act(if shift { Action::DeleteForever } else { Action::Trash }, time);
            }
            Key::Sym(K_DELETE) => self.delete_to(self.step(self.cursor, true, ctrl)),
            Key::Sym(K_LEFT | K_RIGHT) => {
                let right = matches!(key, Key::Sym(K_RIGHT));
                // Without Shift, a selection collapses to its near edge.
                let to = if !shift && !ctrl && sel_start != sel_end {
                    if right { sel_end } else { sel_start }
                } else {
                    self.step(self.cursor, right, ctrl)
                };
                self.move_caret(to, shift);
            }
            Key::Sym(K_HOME) if !ctrl => self.move_caret(0, shift),
            Key::Sym(K_END) if !ctrl => self.move_caret(self.query.len(), shift),
            Key::Sym(K_ESCAPE) => {
                if self.query.is_empty() {
                    return Ok(self.dismiss());
                }
                self.clear_query();
            }
            Key::Sym(K_RETURN | K_KP_ENTER) => {
                let action = if alt {
                    Action::Properties
                } else if ctrl {
                    Action::Reveal
                } else {
                    Action::Open
                };
                self.act(action, time);
            }
            Key::Sym(K_MENU) if !self.hits.is_empty() => {
                let row = self.sel.saturating_sub(self.top) as i32;
                let y = self.list_top() + (row + 1) * self.row_h();
                self.menu = Some(Menu { x: 40 * self.u, y, hover: Some(0), items: self.context_items(), bar: None });
            }
            Key::Sym(K_UP) => self.move_sel(-1, shift),
            Key::Sym(K_DOWN) => self.move_sel(1, shift),
            Key::Sym(K_PAGE_UP) => self.move_sel(-page, shift),
            Key::Sym(K_PAGE_DOWN) => self.move_sel(page, shift),
            Key::Sym(K_HOME) => self.move_sel(isize::MIN, shift),
            Key::Sym(K_END) => self.move_sel(isize::MAX, shift),
            Key::Sym(K_F2) => {
                self.kind = match self.kind {
                    Kind::Any => Kind::File,
                    Kind::File => Kind::Dir,
                    Kind::Dir => Kind::Any,
                };
                self.search();
            }
            Key::Sym(K_F3) => {
                self.case = match self.case {
                    CaseMode::Smart => CaseMode::Sensitive,
                    CaseMode::Sensitive => CaseMode::Insensitive,
                    CaseMode::Insensitive => CaseMode::Smart,
                };
                self.search();
            }
            Key::Sym(K_F1) => self.act(Action::Help(0), time),
            Key::Sym(K_F4) => self.act(Action::Edit, time),
            Key::Sym(K_F5) => self.reindex()?,
            Key::Sym(K_F6) => self.toggle_tree(),
            Key::Sym(K_F7) => self.act(Action::Toggle(Flag::SizeMap), time),
            _ => {}
        }
        Ok(true)
    }

    /// The result row under window position `y`, if any.
    fn row_at(&self, y: i32) -> Option<usize> {
        let row = self.top + ((y - self.list_top()) / self.row_h()) as usize;
        (y >= self.list_top() && row < self.hits.len() && row < self.top + self.rows_visible()).then_some(row)
    }

    fn press(&mut self, button: u8, x: i32, y: i32, time: u32, mods: Mods) {
        if self.close_panel() {
            return;
        }
        if self.completion_press(button, x, y) {
            return;
        }
        if let Some(menu) = self.menu.take() {
            if let (1, Some(item)) = (button, self.menu_item_at(&menu, x, y)) {
                self.act(menu.items[item].2, time);
            } else if let (1, Some(title)) = (button, self.bar_title_at(x, y)) {
                if menu.bar != Some(title) {
                    self.open_bar_menu(title); // another title: switch menus
                }
            }
            return; // any other click just closes the menu
        }
        if button == 1 && self.press_frame(x, y, time) {
            return;
        }
        if self.tree.is_some() && y >= self.menu_h() {
            return self.tree_press(button, x, y, time);
        }
        let in_scrollbar = x >= self.cv.w as i32 - self.bar_w() && y >= self.list_top();
        match button {
            1 if y < self.menu_h() => {
                if let Some(title) = self.bar_title_at(x, y) {
                    self.open_bar_menu(title);
                } else if let Some(action) = self.bar_toggle_at(x, y) {
                    self.act(action, time);
                }
            }
            1 if y < self.box_h() => {
                // Search box: place the caret; double-click selects a word.
                let query = self.query.clone();
                let at = self.text.index_at(&query, x - self.box_text_x());
                if time.wrapping_sub(self.last_box_click) < 400 {
                    self.anchor = self.step(self.step(at, true, false).min(query.len()), false, true);
                    self.cursor = self.step(self.anchor, true, true);
                } else {
                    self.move_caret(at, mods.shift);
                    self.selecting = true;
                }
                self.last_box_click = time;
            }
            4 => self.top = self.top.saturating_sub(3),
            5 => self.top = (self.top + 3).min(self.max_top()),
            1 if in_scrollbar => {
                self.dragging_scrollbar = true;
                self.scroll_to(y);
            }
            1 | 3 if y >= self.list_top() => {
                let Some(row) = self.row_at(y) else { return };
                debug(format!("press button {button} on row {row}"));
                self.status.clear();
                if button == 3 {
                    // The menu acts on the selection; a right-click outside
                    // it selects that row instead.
                    if !self.is_selected(row) {
                        self.select_only(row);
                    }
                    self.sel = row;
                    self.menu = Some(Menu { x, y, hover: None, items: self.context_items(), bar: None });
                    return;
                }
                if mods.shift {
                    self.select_range_to(row);
                } else if mods.ctrl {
                    self.toggle_row(row);
                } else if self.is_selected(row) && self.marks.len() > 1 {
                    // Keep the group for now: this may be the start of
                    // dragging all of it.
                    self.sel = row;
                    self.collapse_on_release = Some(row);
                } else {
                    self.select_only(row);
                }
                // A press on a selected result may turn into a drag once
                // the pointer moves; it carries the whole selection.
                if self.is_selected(row) && self.refusal(Action::CopyFile).is_none() {
                    let paths: Vec<String> = self.selected_paths().iter().map(|p| p.to_string_lossy().into_owned()).collect();
                    let id = self.hits[row];
                    let icon = match paths.len() {
                        1 => Icon::of(self.index.name(id), self.index.is_dir(id), self.index.is_symlink(id)),
                        _ => Icon::File,
                    };
                    self.drag_arm = Some(DragArm { start: (x, y), paths: paths.join("\n"), icon });
                }
                let (last_time, last_row) = self.last_click;
                if last_row == row && time.wrapping_sub(last_time) < 400 {
                    // Double-click: the path column shows the folder.
                    self.select_only(row);
                    self.collapse_on_release = None;
                    let [_, (path_x, path_w), ..] = self.columns();
                    let on_path = self.flag(Flag::PathDoubleClick) && x >= path_x && x < path_x + path_w;
                    self.act(if on_path { Action::Reveal } else { Action::Open }, time);
                }
                self.last_click = (time, row);
            }
            // A column edge in the header: drag to resize.
            1 if self.column_edge_at(x, y).is_some() => self.column_drag = self.column_edge_at(x, y),
            1 if y >= self.box_h() => {
                // Column header: sort by it, or flip the direction.
                let hit = self.columns().iter().rposition(|&(cx, _)| x >= cx).unwrap_or(0);
                let sort = [Sort::Name, Sort::Path, Sort::Size, Sort::Mtime][hit];
                // A second click flips the order; the first uses the
                // preferred direction for dates and sizes.
                let descending_first = match sort {
                    Sort::Mtime => self.flag(Flag::DateDescFirst),
                    Sort::Size => self.flag(Flag::SizeDescFirst),
                    _ => false,
                };
                self.reverse = if self.sort == sort { !self.reverse } else { descending_first };
                self.sort = sort;
                self.search();
            }
            _ => {}
        }
    }

    /// Pointer moved; returns true if something needs redrawing.
    fn motion(&mut self, x: i32, y: i32, mods: Mods) -> io::Result<bool> {
        if self.dragging.is_some() {
            self.b.drag_wants_copy(mods.ctrl || self.drag_copy_only);
            return Ok(false);
        }
        // A pressed result that has moved far enough becomes a drag.
        if let Some(arm) = self.drag_arm.take() {
            let far = (x - arm.start.0).abs().max((y - arm.start.1).abs()) > 6 * self.u;
            if !far {
                self.drag_arm = Some(arm);
                return Ok(false);
            }
            let label = drag_label(&arm.paths);
            let image = self.drag_image(&label, arm.icon);
            // Files that may not be removed from where they are can only
            // be copied.
            self.drag_copy_only = self.refusal(Action::Trash).is_some();
            self.b.start_drag(&arm.paths, &image, mods.ctrl || self.drag_copy_only)?;
            self.status = match self.drag_copy_only {
                true => trf("Dragging {} - drop it on another window to copy it", &[&label]),
                false => trf("Dragging {} - drop it on another window to move it (hold Ctrl to copy)", &[&label]),
            };
            self.drag_paths = arm.paths;
            self.dragging = Some(label);
            return Ok(true);
        }
        if self.completion_motion(x, y) {
            return Ok(true);
        }
        if self.tree.is_some() {
            self.hover_row = None;
            let changed = self.tree_motion(x, y);
            return Ok(self.motion_inner(x, y) | changed);
        }
        let hover = self.row_at(y).filter(|_| self.flag(Flag::RowHover) && x < self.cv.w as i32 - self.bar_w());
        let hover_changed = std::mem::replace(&mut self.hover_row, hover) != hover;
        Ok(self.motion_inner(x, y) | hover_changed)
    }

    fn motion_inner(&mut self, x: i32, y: i32) -> bool {
        if self.dragging_scrollbar {
            let before = *self.top_mut();
            self.scroll_to(y);
            return *self.top_mut() != before;
        }
        if self.selecting {
            let query = self.query.clone();
            let before = self.cursor;
            self.cursor = self.text.index_at(&query, x - self.box_text_x());
            return self.cursor != before;
        }
        if let Some(menu) = self.menu.take() {
            // With a bar menu open, pointing at another title switches to it.
            if let (Some(open), Some(title)) = (menu.bar, self.bar_title_at(x, y)) {
                if open != title {
                    self.open_bar_menu(title);
                    return true;
                }
            }
            let hover = self.menu_item_at(&menu, x, y);
            let changed = hover != menu.hover;
            self.menu = Some(Menu { hover, ..menu });
            return changed;
        }
        false
    }

    /// Scrolls so the scrollbar thumb is centred on window position `y`.
    fn scroll_to(&mut self, y: i32) {
        let (track, thumb) = self.scrollbar();
        let span = (track - thumb).max(1) as i64;
        let pos = ((y - self.list_top() - thumb / 2) as i64).clamp(0, span);
        *self.top_mut() = (pos * self.max_top() as i64 / span) as usize;
    }

    // ---- layout --------------------------------------------------------

    fn row_h(&self) -> i32 {
        self.text.height + 6 * self.u
    }

    /// Height of a separate title bar: none, since ours is the menu bar.
    fn title_h(&self) -> i32 {
        0
    }

    /// Bottom edge of the menu bar, taller when it is also the title bar.
    fn menu_h(&self) -> i32 {
        if self.custom_title { self.row_h() + 12 * self.u } else { self.row_h() }
    }

    /// Bottom edge of the search box area (where the column headers start).
    fn box_h(&self) -> i32 {
        self.menu_h() + self.row_h() + 14 * self.u
    }

    fn list_top(&self) -> i32 {
        self.box_h() + self.row_h() + 1
    }

    fn bar_w(&self) -> i32 {
        10 * self.u
    }

    /// (x, width) in pixels of the Name, Path, Size and Date columns.
    /// Until the user sizes them, Path takes the room the others leave;
    /// after that each keeps its width, and what passes the window's
    /// edge is cut off.
    fn columns(&self) -> [(i32, i32); 4] {
        let widths = match self.column_widths {
            Some(w) => w.map(|w| (w * self.scale).round() as i32),
            None => {
                let avail = (self.cv.w as i32 - self.bar_w() - self.size_w - self.date_w).max(0);
                let name = (avail * 2 / 5).max(120 * self.u);
                [name, (avail - name).max(0), self.size_w, self.date_w]
            }
        };
        let mut x = 0;
        widths.map(|w| {
            let col = (x, w);
            x += w;
            col
        })
    }

    /// The column edge in the header under (x, y), if any: edge `i` is
    /// the right edge of column `i - 1` (1 after Name ... 4 after Date).
    fn column_edge_at(&self, x: i32, y: i32) -> Option<usize> {
        if self.tree.is_some() || y < self.box_h() || y >= self.list_top() {
            return None;
        }
        let cols = self.columns();
        (1..=4).find(|&i| (x - (cols[i - 1].0 + cols[i - 1].1)).abs() <= 6 * self.u)
    }

    /// Moves column edge `edge` to `x`: the column on its left takes the
    /// new width and those on its right move along, keeping theirs.
    fn drag_column(&mut self, edge: usize, x: i32) {
        let cols = self.columns();
        let mut widths = cols.map(|(_, w)| w);
        widths[edge - 1] = (x - cols[edge - 1].0).max(40 * self.u);
        self.column_widths = Some(widths.map(|w| w as f32 / self.scale));
    }

    /// Scrollbar (track height, thumb height) in pixels.
    fn scrollbar(&self) -> (i32, i32) {
        let rows = self.rows_visible();
        let track = rows as i32 * self.row_h();
        let thumb = (track as i64 * rows as i64 / self.list_len().max(1) as i64).max(24 * self.u as i64);
        (track, (thumb as i32).min(track))
    }

    /// Menu rectangle (x, y, w, h), kept inside the window.
    fn menu_rect(&mut self, menu: &Menu) -> (i32, i32, i32, i32) {
        let widest = menu.items.iter().map(|(l, s, _)| self.text.width(tr(l)) + self.text.width(tr(s))).max().unwrap_or(0);
        let w = widest + 64 * self.u;
        let h = menu.items.len() as i32 * self.row_h() + 8 * self.u;
        let x = menu.x.min(self.cv.w as i32 - w).max(0);
        let y = menu.y.min(self.cv.h as i32 - h).max(0);
        (x, y, w, h)
    }

    fn menu_item_at(&mut self, menu: &Menu, px: i32, py: i32) -> Option<usize> {
        let (x, y, w, _) = self.menu_rect(menu);
        let item = (py - y - 4 * self.u).div_euclid(self.row_h());
        (px >= x && px < x + w && (0..menu.items.len() as i32).contains(&item)).then_some(item as usize)
    }

    // ---- drawing -------------------------------------------------------

    fn render(&mut self) -> io::Result<()> {
        let (w, h) = (self.cv.w as i32, self.cv.h as i32);
        let (u, row_h, bar) = (self.u, self.row_h(), self.bar_w());
        let pad = 8 * u;
        let cols = self.columns();
        // The window base, with the results list as a lighter layer on it.
        self.cv.fill(0, 0, w, h, PANEL);
        self.cv.fill(0, self.box_h(), w, h - self.box_h() - row_h, palette::CARD);
        self.cv.fill(0, self.box_h(), w, 1, LINE);

        // Search box (a Windows 11 text box: rounded, a hairline border,
        // and an accent line along the bottom while it has the focus):
        // text scrolled to keep the caret visible, selection highlighted,
        // all clipped to the inside of the box.
        let top = self.menu_h();
        let (bx, by, bw, bh) = (6 * u, top + 6 * u, w - 12 * u, row_h + 2 * u);
        self.cv.round_frame(bx, by, bw, bh, 4 * u, u.max(1), LINE, WHITE);
        self.cv.clip = (bx + 3 * u, bx + bw - 3 * u);
        self.cv.fill(bx, by + bh - 2 * u, bw, 2 * u, SELECT);
        self.cv.clip = (0, i32::MAX);

        // Title bar (ours) and menu bar.
        let th = self.title_h();
        if self.custom_title {
            self.draw_title_bar();
        }
        let open = self.menu.as_ref().and_then(|m| m.bar);
        for (i, (x, tw)) in self.bar_titles().into_iter().enumerate() {
            if open == Some(i) {
                self.cv.round(x + 2 * u, th + 2 * u, tw - 4 * u, top - th - 4 * u, 4 * u, palette::PICKED);
            }
            self.text.draw(&mut self.cv, x + 9 * u, (top - self.text.height) / 2, tr(BAR[i].0), tw, TEXT);
        }
        for (x, bw, label, action) in self.bar_toggles() {
            let bh = row_h - 5 * u;
            let y = (top - bh) / 2;
            let on = self.checked(action);
            if on {
                self.cv.round(x, y, bw, bh, 4 * u, SELECT);
            } else {
                self.cv.round_frame(x, y, bw, bh, 4 * u, u.max(1), LINE, WHITE);
            }
            let ty = y + (bh - self.text.height) / 2;
            self.text.draw(&mut self.cv, x + 10 * u, ty, tr(label), bw, if on { ON_ACCENT } else { TEXT });
        }
        if self.tree.is_some() {
            self.draw_tree();
        } else {
            let query = self.query.clone();
            let room = (w - 28 * u).max(4 * u);
            let caret = self.text.width(&query[..self.cursor]);
            let max_scroll = (self.text.width(&query) + 2 * u - room).max(0);
            self.box_scroll = self.box_scroll.clamp((caret + 2 * u - room).max(0), caret).min(max_scroll);
            let (tx, ty) = (self.box_text_x(), top + 10 * u);
            let box_clip = (14 * u, 14 * u + room);
            self.cv.clip = box_clip;
            self.text.draw(&mut self.cv, tx, ty, &query, i32::MAX / 2, TEXT);
            let (a, b) = self.selection();
            if a != b {
                let (xa, xb) = (tx + self.text.width(&query[..a]), tx + self.text.width(&query[..b]));
                self.cv.clip = (xa.max(box_clip.0), xb.min(box_clip.1));
                self.cv.fill(xa, ty - u, xb - xa, self.text.height + 2 * u, SELECT);
                self.text.draw(&mut self.cv, tx, ty, &query, i32::MAX / 2, ON_ACCENT);
                self.cv.clip = box_clip;
            }
            self.cv.fill(tx + caret, ty, u.max(1), self.text.height, TEXT);
            self.cv.clip = (0, i32::MAX);

            // Columns sized wider than the window are cut off before the
            // scrollbar.
            self.cv.clip = (0, w - bar);
            // Column headers with the sort direction, quiet, on the list.
            let y = self.box_h();
            self.cv.fill(pad, y + row_h, w - 2 * pad, 1, LINE);
            let heads = [("Name", Sort::Name), ("Path", Sort::Path), ("Size", Sort::Size), ("Modified", Sort::Mtime)];
            for (i, ((label, sort), (x, cw))) in heads.into_iter().zip(cols).enumerate() {
                let mark = if self.sort != sort { "" } else if self.reverse { "  ▼" } else { "  ▲" };
                if i > 0 {
                    self.cv.fill(x, y + 6 * u, 1, row_h - 12 * u, LINE);
                }
                let ink = if self.sort == sort { TEXT } else { DIM };
                self.text.draw(&mut self.cv, x + pad, y + 3 * u, &format!("{}{mark}", tr(label)), cw - 2 * pad, ink);
            }

            // Result rows.
            let rows = self.rows_visible();
            let list_top = self.list_top();
            let icon = self.icons.size as i32;
            let mut path = Vec::new();
            for row in 0..rows {
                let Some(&id) = self.hits.get(self.top + row) else { break };
                let y = list_top + row as i32 * row_h;
                let selected = self.is_selected(self.top + row);
                // Selected rows: filled across in the accent colour, with
                // white text; the row under the pointer: a rounded tint.
                let (fg, dim) = if selected { (ON_ACCENT, ON_ACCENT) } else { (TEXT, DIM) };
                if selected {
                    self.cv.fill(0, y, w - bar, row_h, SELECT);
                } else if self.hover_row == Some(self.top + row) {
                    self.cv.round(4 * u, y + u, w - bar - 6 * u, row_h - 2 * u, 4 * u, HOVER);
                } else if self.flag(Flag::AltRows) && (self.top + row) % 2 == 1 {
                    self.cv.fill(0, y, w - bar, row_h, 0xf6f6f6);
                }
                let is_dir = self.index.is_dir(id);
                let name = String::from_utf8_lossy(self.index.name(id)).into_owned();
                let parent = match self.index.parent(id) {
                    Some(p) => {
                        self.index.write_path(p, &mut path);
                        String::from_utf8_lossy(&path).into_owned()
                    }
                    None => String::new(),
                };
                let (entry_path, drive) = self.entry_path(id);
                let system = self.flag(Flag::SystemIcons);
                let entry_name = self.index.name(id);
                self.icons.draw_entry(&mut self.cv, cols[0].0 + pad, y + (row_h - icon) / 2, entry_name, is_dir, self.index.is_symlink(id), &entry_path, drive, system);
                let ty = y + 3 * u;
                let name_x = cols[0].0 + pad + icon + 6 * u;
                self.text.draw(&mut self.cv, name_x, ty, &name, cols[0].0 + cols[0].1 - pad - name_x, fg);
                self.text.draw(&mut self.cv, cols[1].0 + pad, ty, &parent, cols[1].1 - 2 * pad, dim);
                if !is_dir {
                    let size = self.size_text(self.index.size(id));
                    let x = cols[2].0 + cols[2].1 - pad - self.text.width(&size);
                    self.text.draw(&mut self.cv, x, ty, &size, cols[2].1, fg);
                }
                let (yr, mo, d, hh, mm) = ff_core::local_civil(self.index.mtime(id));
                let date = format!("{yr:04}-{mo:02}-{d:02} {hh:02}:{mm:02}");
                self.text.draw(&mut self.cv, cols[3].0 + pad, ty, &date, cols[3].1 - pad, dim);
            }

            self.cv.clip = (0, i32::MAX);
            // Scrollbar.
            if self.hits.len() > rows {
                let (track, thumb) = self.scrollbar();
                let pos = ((track - thumb) as i64 * self.top as i64 / self.max_top().max(1) as i64) as i32;
                // A thin rounded thumb, as Windows 11 draws scrollbars.
                let tw = 4 * u;
                self.cv.round(w - bar + (bar - tw) / 2, list_top + pos + 2 * u, tw, thumb - 4 * u, tw / 2, palette::QUIET);
            }
        }

        // Status bar.
        let y = h - row_h;
        self.cv.fill(0, y, w, row_h, PANEL);
        self.cv.fill(0, y, w, 1, LINE);
        let progress = self.build_status();
        let status = if let Some((text, _)) = &progress {
            text.clone()
        } else if self.status.is_empty() && self.tree.is_some() {
            self.tree_status()
        } else if self.status.is_empty() {
            let kind = tr(["all", "files", "folders"][self.kind as usize]);
            let case = tr(["smart", "sensitive", "insensitive"][self.case as usize]);
            let objects = tr("objects");
            let mut n = group_digits(self.hits.len() as u64);
            if self.marks.len() > 1 {
                n = trf("{} of {}", &[&group_digits(self.marks.len() as u64), &n]);
            }
            let mut modes = String::new();
            if self.regex {
                modes = format!("{modes}      {}", tr("Ctrl+R regex: on"));
            }
            if self.whole_word {
                modes = format!("{modes}      {}", tr("Ctrl+B whole words: on"));
            }
            match &self.query_error {
                // A filter or expression that makes no sense: say so, but
                // not while the completion list is open on an unfinished one.
                Some(problem) if self.complete.is_none() => format!("{n} {objects}      {problem}"),
                Some(_) => format!("{n} {objects}      {}", tr("Enter takes the highlighted entry, Esc closes the list")),
                None if self.flag(Flag::SelectedInStatus) && self.sel < self.hits.len() => {
                    let id = self.hits[self.sel];
                    let (yr, mo, d, hh, mm) = ff_core::local_civil(self.index.mtime(id));
                    let size = if self.index.is_dir(id) { tr("folder").to_string() } else { trf("{} bytes", &[&group_digits(self.index.size(id))]) };
                    format!("{n} {objects}      {}      {size}      {yr:04}-{mo:02}-{d:02} {hh:02}:{mm:02}", self.index.path(id).display())
                }
                None => format!(
                    "{n} {objects}      {}: {kind}      {}: {case}{modes}      {}      {}",
                    tr("F2 show"),
                    tr("F3 case"),
                    tr("F1 help"),
                    tr("Right-click for actions")
                ),
            }
        } else {
            self.status.clone()
        };
        let grip = if self.custom_title { GRIP * u } else { 0 };
        // While indexing, a progress bar at the right end: filled to the
        // share done when the size is known, else a block sweeping across.
        let mut text_room = w - 2 * pad - grip;
        if let Some((_, share)) = progress {
            let (bw, bh) = (220 * u, row_h - 10 * u);
            if w > 3 * bw {
                let (bx, by) = (w - pad - grip - bw, y + 5 * u);
                self.cv.fill(bx, by, bw, bh, LINE);
                match share {
                    Some(share) => self.cv.fill(bx, by, (bw as f64 * share) as i32, bh, SELECT),
                    None => {
                        let span = bw / 4;
                        let t = self.building.as_ref().map_or(0, |b| b.started.elapsed().as_millis() as i32);
                        let x = (t / 4) % (bw + span) - span;
                        let (from, to) = (x.max(0), (x + span).min(bw));
                        self.cv.fill(bx + from, by, (to - from).max(0), bh, SELECT);
                    }
                }
                text_room -= bw + pad;
            }
        }
        self.text.draw(&mut self.cv, pad, y + 3 * u, &status, text_room, TEXT);
        // The resize grip: dots in a triangle in the corner.
        if self.custom_title {
            let dot = ((2.0 * self.scale).round() as i32).max(2);
            for row in 0..4 {
                for col in (3 - row)..4 {
                    self.cv.fill(w - 3 * u - (4 - col) * 2 * dot, h - 3 * u - (4 - row) * 2 * dot, dot, dot, 0x767676);
                }
            }
        }

        self.draw_completion();
        if let Some(menu) = self.menu.take() {
            self.draw_menu(&menu);
            self.menu = Some(menu);
        }
        if let Some(props) = self.props.take() {
            self.draw_properties(&props);
            self.props = Some(props);
        }
        if self.about {
            self.draw_about();
        }
        if self.custom_title {
            // The window's own border (Windows draws one itself).
            if cfg!(not(windows)) {
                self.cv.frame(0, 0, w, h, 1, 0x8c8c8c);
            }
        }
        self.present()
    }

    /// Why `action` cannot be done to the selected files, if it cannot:
    /// its menu entry is greyed and its shortcut only says why.
    fn refusal(&mut self, action: Action) -> Option<&'static str> {
        let delete = match action {
            Action::Trash | Action::DeleteForever => true,
            Action::Open | Action::Edit | Action::CopyFile => false,
            _ => return None,
        };
        if self.tree.is_some() {
            return self.on_tree_target(|app| app.refusal(action)).flatten();
        }
        // A huge selection is not checked to the end: what cannot be
        // done is then reported when it is tried. On Windows a check
        // opens the file, so only the first is looked at.
        let checked = if cfg!(windows) { 1 } else { 200 };
        let blocked = self.selected_rows().into_iter().take(checked).any(|row| {
            let path = self.index.path(self.hits[row]);
            if delete { !may_delete(&path) } else { !may_read(&path) }
        });
        match (blocked, delete) {
            (false, _) => None,
            (true, true) => Some("You do not have permission to delete or move this"),
            (true, false) => Some("You do not have permission to read this"),
        }
    }

    fn draw_menu(&mut self, menu: &Menu) {
        let (u, row_h) = (self.u, self.row_h());
        let (x, y, w, h) = self.menu_rect(menu);
        // A Windows 11 flyout: rounded, a hairline border, a soft shadow.
        self.cv.round(x + u, y + 2 * u, w, h, 8 * u, 0xd6d6d6);
        self.cv.round_frame(x, y, w, h, 8 * u, u.max(1), 0xd0d0d0, WHITE);
        for (i, (label, shortcut, action)) in menu.items.iter().enumerate() {
            let iy = y + 4 * u + i as i32 * row_h;
            // Entries the user has no permission for are greyed.
            let off = self.refusal(*action).is_some();
            if menu.hover == Some(i) && !off {
                self.cv.round(x + 4 * u, iy + u, w - 8 * u, row_h - 2 * u, 4 * u, HOVER);
            }
            // A bullet marks active choices and switched-on options.
            if self.checked(*action) {
                self.text.draw(&mut self.cv, x + 6 * u, iy + 3 * u, "\u{2022}", w, TEXT);
            }
            self.text.draw(&mut self.cv, x + 18 * u, iy + 3 * u, tr(label), w, if off { 0xa8a8a8 } else { TEXT });
            // The global shortcut's keys are the user's choice.
            let shortcut = if *action == Action::ToggleShortcut { self.shortcut_keys.as_str() } else { tr(shortcut) };
            let sx = x + w - 14 * u - self.text.width(shortcut);
            self.text.draw(&mut self.cv, sx, iy + 3 * u, shortcut, w, if off { 0xa8a8a8 } else { DIM });
        }
    }

    fn draw_properties(&mut self, props: &[(&'static str, String)]) {
        let (u, row_h) = (self.u, self.row_h());
        let (cw, ch) = (self.cv.w as i32, self.cv.h as i32);
        let w = (620 * u).min(cw - 20 * u);
        let h = (props.len() as i32 + 2) * row_h + 34 * u;
        let (x, y) = ((cw - w) / 2, ((ch - h) / 2).max(0));
        self.cv.fill(x + 4 * u, y + 4 * u, w, h, 0xb0b0b0); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x8a8a8a);
        self.cv.fill(x + u, y + u, w - 2 * u, row_h + 10 * u, PANEL);
        self.cv.fill(x + u, y + row_h + 11 * u, w - 2 * u, 1, LINE);
        self.text.draw(&mut self.cv, x + 16 * u, y + 8 * u, tr("Properties"), w - 32 * u, TEXT);
        let label_w = 110 * u;
        for (i, (label, value)) in props.iter().enumerate() {
            let ry = y + row_h + 20 * u + i as i32 * row_h;
            self.text.draw(&mut self.cv, x + 16 * u, ry, tr(label), label_w, DIM);
            let value = &tr(value).to_string();
            self.text.draw(&mut self.cv, x + 16 * u + label_w, ry, value, w - 32 * u - label_w, TEXT);
        }
        let hint = tr("Press any key or click to close");
        self.text.draw(&mut self.cv, x + 16 * u, y + h - row_h - 4 * u, hint, w - 32 * u, DIM);
    }

    fn draw_about(&mut self) {
        let (u, row_h) = (self.u, self.row_h());
        let (cw, ch) = (self.cv.w as i32, self.cv.h as i32);
        let lines = [
            ("Fast search".to_string(), TEXT),
            (trf("Version {}", &[&ff_core::update::VERSION]), DIM),
            (String::new(), DIM),
            (tr("Instant file search for Linux").to_string(), TEXT),
            (trf("{} files and folders indexed", &[&group_digits(self.index.len() as u64)]), TEXT),
            (String::new(), DIM),
            ("github.com/khunder/fast-search".to_string(), SELECT),
            (tr("MIT License").to_string(), DIM),
            (String::new(), DIM),
            (tr("Press any key or click to close").to_string(), DIM),
        ];
        let icon = self.about_icon.size as i32;
        let w = (420 * u).min(cw - 20 * u);
        let h = icon + lines.len() as i32 * row_h + 44 * u;
        let (x, y) = ((cw - w) / 2, ((ch - h) / 2).max(0));
        self.cv.fill(x + 4 * u, y + 4 * u, w, h, 0xb0b0b0); // shadow
        self.cv.fill(x, y, w, h, WHITE);
        self.cv.frame(x, y, w, h, u.max(1), 0x8a8a8a);
        self.about_icon.draw(&mut self.cv, x + (w - icon) / 2, y + 16 * u, Icon::App);
        for (i, (line, color)) in lines.iter().enumerate() {
            let lx = x + (w - self.text.width(line)) / 2;
            self.text.draw(&mut self.cv, lx.max(x + 8 * u), y + icon + 26 * u + i as i32 * row_h, line, w - 16 * u, *color);
        }
    }

    /// Shows the frame in the main window.
    fn present(&mut self) -> io::Result<()> {
        self.b.present(Win::Main(self.current), &self.cv)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn permissions_decide_what_may_be_done() {
        // Root may do anything, which would make this test say nothing.
        if std::fs::metadata("/proc/self").is_ok_and(|m| std::os::unix::fs::MetadataExt::uid(&m) == 0) {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ff_gui_perm_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("locked")).unwrap();
        let (mine, secret, kept) = (dir.join("mine.txt"), dir.join("secret.txt"), dir.join("locked/kept.txt"));
        for file in [&mine, &secret, &kept] {
            std::fs::write(file, "x").unwrap();
        }
        let mode = |path: &std::path::Path, mode| std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        mode(&secret, 0o000);
        mode(&dir.join("locked"), 0o555);

        assert!(may_read(&mine) && may_delete(&mine));
        // Unreadable, but its folder is ours: it can still be deleted.
        assert!(!may_read(&secret) && may_delete(&secret));
        // Readable, but its folder cannot be changed.
        assert!(may_read(&kept) && !may_delete(&kept));
        // A folder we may not write into cannot be moved or emptied.
        assert!(!may_delete(&dir.join("locked")));
        // System files, and a file that is gone (left to the action itself).
        assert!(!may_delete(std::path::Path::new("/etc/hostname")) || !std::path::Path::new("/etc/hostname").exists());
        assert!(!may_delete(std::path::Path::new("/")));
        assert!(may_read(&dir.join("gone.txt")) && may_delete(&dir.join("gone.txt")));

        mode(&dir.join("locked"), 0o755);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[cfg(test)]
mod portable_tests {
    use super::trim_folder;

    #[test]
    fn typed_folders_lose_trailing_separators_but_roots_stay_whole() {
        assert_eq!(trim_folder("/home/me/Videos/"), "/home/me/Videos");
        assert_eq!(trim_folder("/"), "/");
        if cfg!(windows) {
            assert_eq!(trim_folder(r"C:\Users\me\"), r"C:\Users\me");
            assert_eq!(trim_folder(r"C:\"), r"C:\");
        }
    }
}

/// What a background delete reports back.
struct Deleted {
    ids: Vec<u32>,
    paths: Vec<PathBuf>,
    trash: bool,
    /// What went wrong, or that the user cancelled; None if all went.
    problem: Option<String>,
}
