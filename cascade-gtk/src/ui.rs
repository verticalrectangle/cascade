//! cascade-gtk UI — Rust port of the omperator linux GTK app
//! (AppWindow/TranscriptWidgets/PanesFactory). Layout: rail (left, drag
//! resize), slim topbar, full-width transcript with durable + live-tail
//! boxes, plan slide-over, composer with attachments, question card, inbox,
//! browser pane (right), first-run login overlay.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use cascade_core::{
    ListedSession, SessionEvent, SessionSnapshot, TodoPhase, TodoStatus, UiAnswer, UiMethod,
    UiRequest,
};
use gtk4::gdk;
use gtk4::gio;
use gtk4::glib;
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, CheckButton, DropTarget, Entry,
    EventControllerKey, EventControllerMotion, GestureClick, GestureDrag, Label, MenuButton,
    Orientation, Overlay, PasswordEntry, Picture, Popover, ProgressBar, Revealer,
    RevealerTransitionType, ScrolledWindow, Separator, TextBuffer, TextTag, TextTagTable,
    TextView, Window,
};

use crate::markdown::{self, Block};
use crate::settings::Settings;
use crate::worker::{BackendKind, Cmd, InboxItem, UiMsg};
use crate::{apply_theme, highlight};
use webkit6::prelude::*;

const RAIL_MIN: i32 = 180;
const RAIL_MAX: i32 = 400;
const PANE_MIN: i32 = 280;
const PANE_MAX: i32 = 600;
const FOLLOW_MARGIN: f64 = 48.0;
/// Scroll distance from the transcript top that triggers a history page.
const HISTORY_TRIGGER: f64 = 150.0;
/// Rolling window of rendered-message fingerprints (room + tailer / replay).
const FINGERPRINT_CAP: usize = 64;

// ── theme palettes (verbatim values from theme-dawn/moon.css) ────────

struct Palette {
    text: &'static str,
    muted: &'static str,
    subtle: &'static str,
    gold: &'static str,
    pine: &'static str,
    foam: &'static str,
    iris: &'static str,
    love: &'static str,
    code_bg: &'static str,
    comment: &'static str,
    function: &'static str,
    inline_code_bg: &'static str,
    highlight_bg: &'static str,
    diff_add_bg: &'static str,
    diff_remove_bg: &'static str,
}

fn palette(theme: &str) -> Palette {
    if theme == "moon" {
        Palette {
            text: "#E0DEF4",
            muted: "#6E6A86",
            subtle: "#908CAA",
            gold: "#F6C177",
            pine: "#3E8FB0",
            foam: "#9CCFD8",
            iris: "#C4A7E7",
            love: "#EB6F92",
            code_bg: "#2A273F",
            comment: "#6E6A86",
            function: "#EBBCBA",
            inline_code_bg: "rgba(156,207,216,0.12)",
            highlight_bg: "rgba(246,193,119,0.22)",
            diff_add_bg: "rgba(49,116,143,0.35)",
            diff_remove_bg: "rgba(235,111,146,0.18)",
        }
    } else {
        Palette {
            text: "#575279",
            muted: "#797593",
            subtle: "#6E6A8A",
            gold: "#EA9D34",
            pine: "#286983",
            foam: "#56949F",
            iris: "#907AA9",
            love: "#B4637A",
            code_bg: "#F2E9E1",
            comment: "#9893A5",
            function: "#D7827E",
            inline_code_bg: "rgba(86,148,159,0.16)",
            highlight_bg: "rgba(234,157,52,0.22)",
            diff_add_bg: "rgba(86,148,159,0.22)",
            diff_remove_bg: "rgba(180,99,122,0.16)",
        }
    }
}

const SERIF: &str = "New York, Charter, Georgia, serif";
const SANS: &str = "Fira Sans, Helvetica Neue, sans-serif";
const MONO: &str = "JetBrains Mono, Cascadia Code, monospace";

#[derive(Clone, Copy, Default)]
struct TagSpec {
    fg: Option<&'static str>,
    bg: Option<&'static str>,
    weight: i32,
    italic: bool,
    font: Option<&'static str>,
    size_pt: f64,
    scale: f64,
    underline: bool,
    strikethrough: bool,
}

fn tag_specs(theme: &str) -> Vec<(&'static str, TagSpec)> {
    let p = palette(theme);
    let mut v: Vec<(&'static str, TagSpec)> = Vec::new();
    let mut push = |name: &'static str, s: TagSpec| v.push((name, s));
    push("assistant", TagSpec {
        fg: Some(p.text), font: Some(SERIF), size_pt: 13.5, ..Default::default()
    }); // 18px
    push("user", TagSpec {
        fg: Some(p.text), font: Some(SANS), size_pt: 11.25, ..Default::default()
    }); // 15px
    push("md-h1", TagSpec { fg: Some(p.gold), weight: 700, scale: 1.5, ..Default::default() });
    push("md-h2", TagSpec { fg: Some(p.gold), weight: 700, scale: 1.3, ..Default::default() });
    push("md-h3", TagSpec { fg: Some(p.text), weight: 700, scale: 1.15, ..Default::default() });
    push("md-h4", TagSpec { fg: Some(p.text), weight: 700, scale: 1.0, ..Default::default() });
    push("md-bold", TagSpec { fg: Some(p.gold), weight: 700, ..Default::default() });
    push("md-italic", TagSpec { italic: true, ..Default::default() });
    push("md-bold-italic", TagSpec { weight: 700, italic: true, ..Default::default() });
    push("md-inline-code", TagSpec {
        fg: Some(if theme == "moon" { p.foam } else { p.pine }),
        bg: Some(p.inline_code_bg),
        font: Some(MONO),
        size_pt: 12.0,
        ..Default::default()
    });
    push("md-quote", TagSpec { fg: Some(p.subtle), italic: true, ..Default::default() });
    push("md-list", TagSpec { fg: Some(p.text), ..Default::default() });
    push("md-list-marker", TagSpec { fg: Some(p.muted), weight: 700, ..Default::default() });
    push("md-link", TagSpec { fg: Some(p.iris), underline: true, ..Default::default() });
    push("md-strike", TagSpec { strikethrough: true, ..Default::default() });
    push("md-highlight", TagSpec { bg: Some(p.highlight_bg), ..Default::default() });
    push("md-table", TagSpec {
        fg: Some(p.text), font: Some(MONO), size_pt: 12.0, ..Default::default()
    });
    push("code", TagSpec {
        fg: Some(p.foam), bg: Some(p.code_bg), font: Some(MONO), size_pt: 12.5,
        ..Default::default()
    });
    push("syn-keyword", TagSpec { fg: Some(p.iris), weight: 600, ..Default::default() });
    push("syn-string", TagSpec { fg: Some(p.foam), ..Default::default() });
    push("syn-comment", TagSpec { fg: Some(p.comment), italic: true, ..Default::default() });
    push("syn-number", TagSpec { fg: Some(p.gold), ..Default::default() });
    push("syn-type", TagSpec { fg: Some(p.pine), ..Default::default() });
    push("syn-function", TagSpec { fg: Some(p.function), ..Default::default() });
    push("syn-attribute", TagSpec { fg: Some(p.iris), italic: true, ..Default::default() });
    push("syn-plain", TagSpec { fg: Some(p.text), ..Default::default() });
    push("diff-add", TagSpec { fg: Some(p.pine), bg: Some(p.diff_add_bg), ..Default::default() });
    push("diff-remove", TagSpec {
        fg: Some(p.love), bg: Some(p.diff_remove_bg), ..Default::default()
    });
    push("thinking", TagSpec { fg: Some(p.muted), italic: true, ..Default::default() });
    push("tool", TagSpec { fg: Some(p.text), font: Some(MONO), size_pt: 9.5, ..Default::default() });
    v
}

fn apply_tag(table: &TextTagTable, name: &str, spec: &TagSpec) {
    let tag = match table.lookup(name) {
        Some(t) => t,
        None => {
            let t = TextTag::new(Some(name));
            table.add(&t);
            t
        }
    };
    tag.set_foreground(spec.fg);
    tag.set_background(spec.bg);
    tag.set_weight(spec.weight);
    tag.set_style(if spec.italic {
        pango::Style::Italic
    } else {
        pango::Style::Normal
    });
    tag.set_font(spec.font);
    if spec.size_pt > 0.0 {
        tag.set_size_points(spec.size_pt);
    } else {
        tag.set_property("size-set", false);
    }
    if spec.scale > 0.0 {
        tag.set_scale(spec.scale);
    } else {
        tag.set_property("scale-set", false);
    }
    tag.set_underline(if spec.underline {
        pango::Underline::Single
    } else {
        pango::Underline::None
    });
    tag.set_strikethrough(spec.strikethrough);
    // Bold/heading/checked-marker colors must win over muted md-list-marker.
    if name == "md-bold" || name.starts_with("md-h") {
        tag.set_priority(1);
    }
}

/// Create (or restyle) all transcript tags on a buffer for `theme`.
fn style_buffer(buf: &TextBuffer, theme: &str) {
    let table = buf.tag_table();
    for (name, spec) in tag_specs(theme) {
        apply_tag(&table, name, &spec);
    }
}

// ── state ────────────────────────────────────────────────────────────

#[derive(Default)]
struct StreamState {
    assistant: Option<TextView>,
    thinking_body: Option<TextView>,
    pending_ui: Option<String>,
    streaming: bool,
    thinking_text: String,
    pending_text: String,
    flush_scheduled: bool,
    /// Text of the last optimistically-rendered user bubble + when, used to
    /// suppress the duplicate from the MessageEnd echo of the same message.
    last_user_echo: Option<(String, std::time::Instant)>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum RailStatus {
    Live,
    Idle,
    Ended,
}

impl RailStatus {
    fn from_meta(m: &ListedSession) -> Self {
        match m.live {
            Some(true) if m.working == Some(true) => Self::Live,
            Some(true) => Self::Idle,
            Some(false) | None => Self::Ended,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Live => "LIVE",
            Self::Idle => "IDLE",
            Self::Ended => "ENDED",
        }
    }

    fn css_class(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Idle => "idle",
            Self::Ended => "ended",
        }
    }
}

/// Rows with no content never render — a session that never said anything
/// isn't a session.
fn is_empty_row(m: &ListedSession) -> bool {
    m.empty.unwrap_or(false)
}

pub struct Ui {
    window: ApplicationWindow,
    toast: Label,
    toast_reveal: Revealer,
    login_overlay: GtkBox,
    login_error: Label,
    email: Entry,
    password: PasswordEntry,
    invite: Entry,
    login_btn: Button,
    login_mode_link: Button,
    login_sub: Label,
    share_btn: Button,
    unshare_btn: Button,
    registering: Cell<bool>,
    sharing: HashSet<String>,
    status_label: Label,
    model_pill: Label,
    inbox_btn: MenuButton,
    inbox_list: GtkBox,
    rail_revealer: Revealer,
    rail_wrap: GtkBox,
    rail_search: Entry,
    rail_list: GtkBox,
    transcript_scroll: ScrolledWindow,
    durable_box: GtkBox,
    live_box: GtkBox,
    follow: Cell<bool>,
    programmatic: Cell<bool>,
    current_strip: Option<ToolStrip>,
    // transcript history paging (tail-first render, older pages prepend)
    history: VecDeque<serde_json::Value>,
    history_oldest_rendered: u64,
    history_server_more: bool,
    history_loading: bool,
    history_has_content: bool,
    history_status: Label,
    plan_reveal: Revealer,
    plan_box: GtkBox,
    question_host: GtkBox,
    composer: TextView,
    attach_btn: Button,
    composer_hint: Label,
    share_link_entry: Entry,
    share_link_reveal: Revealer,
    read_only: bool,
    send_btn: Button,
    stop_btn: Button,
    queue_btn: Button,
    chips_strip: GtkBox,
    queue_strip: GtkBox,
    attachments: Vec<PathBuf>,
    pane_revealer: Revealer,
    pane_wrap: GtkBox,
    url_entry: Entry,
    webview: Option<webkit6::WebView>,
    pane_url: Option<String>,
    lightbox: Revealer,
    lightbox_pic: Picture,
    cmd: async_channel::Sender<Cmd>,
    stream: StreamState,
    /// Per-attach fingerprints of messages already drawn. Second arrival
    /// (room + tailer, or reconnect replay) skips the whole render.
    seen_fingerprints: VecDeque<u64>,
    selected_id: Option<String>,
    attached_kind: Option<BackendKind>,
    metas: Vec<ListedSession>,
    machine_names: HashMap<String, String>,
    settings: Settings,
    ended_collapsed: bool,
    session_models: HashMap<String, String>,
    context_usage: HashMap<String, f64>,
    scroll_mem: HashMap<String, (f64, bool)>,
    buffers: Vec<TextBuffer>,
    inbox_items: Vec<InboxItem>,
    pane_visible: bool,
    local_mode: bool,
}

/// Display name for a session: live omp title → cwd basename → short id.
fn session_display_name(m: &ListedSession) -> String {
    if let Some(name) = m.name.as_deref().filter(|n| !n.trim().is_empty()) {
        return name.to_string();
    }
    // Untitled: an abbreviated path that reads as a place, never a bare home
    // basename (~/home/alexis → "alexis" reads as a username, not a location).
    abbreviate_path(&m.cwd)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("session {}", &m.id[..8.min(m.id.len())]))
}

/// "~/dev/cascade" for home-relative paths, full path otherwise; "~" for home.
fn abbreviate_path(cwd: &str) -> Option<String> {
    if cwd.is_empty() {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() {
        if cwd == home {
            return Some("~".to_string());
        }
        if let Some(rest) = cwd.strip_prefix(&format!("{home}/")) {
            return Some(format!("~/{rest}"));
        }
    }
    Some(cwd.to_string())
}

/// Window title: bare session name while attached, "cascade" at the rail.
fn set_window_title(u: &Ui) {
    let title = u
        .selected_id
        .as_deref()
        .and_then(|id| u.metas.iter().find(|m| m.id == id))
        .map(session_display_name)
        .unwrap_or_else(|| "cascade".to_string());
    u.window.set_title(Some(&title));
}

pub fn build(app: &Application, cmd: async_channel::Sender<Cmd>, ui_rx: async_channel::Receiver<UiMsg>) {
    let settings = Settings::load();

    let window = ApplicationWindow::builder()
        .application(app)
        .title("cascade")
        .default_width(1280)
        .default_height(840)
        .build();


    let overlay = Overlay::new();
    window.set_child(Some(&overlay));

    // toast
    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(gtk4::Align::Center);
    toast.set_valign(gtk4::Align::Start);
    let toast_reveal = Revealer::new();
    toast_reveal.set_transition_type(RevealerTransitionType::SlideDown);
    toast_reveal.set_child(Some(&toast));
    // Overlay children with Fill alignment get full-window allocation and eat
    // every click even when hidden (revealer at 0 natural size). Pin to
    // center-top natural size and never accept pointer input — toasts are
    // informational only.
    toast_reveal.set_halign(gtk4::Align::Center);
    toast_reveal.set_valign(gtk4::Align::Start);
    toast_reveal.set_can_target(false);
    overlay.add_overlay(&toast_reveal);

    // ── topbar ────────────────────────────────────────────────────
    let topbar = GtkBox::new(Orientation::Horizontal, 6);
    topbar.add_css_class("topbar");

    let rail_toggle = Button::with_label("☰");
    rail_toggle.add_css_class("flat-btn");
    rail_toggle.set_tooltip_text(Some("Toggle sidebar (Ctrl+B)"));

    let status_label = Label::new(Some("connecting…"));
    status_label.add_css_class("status-label");
    status_label.set_xalign(0.0);
    status_label.set_ellipsize(pango::EllipsizeMode::End);
    status_label.set_hexpand(true);

    let model_pill = Label::new(None);
    model_pill.add_css_class("model-pill");
    model_pill.set_visible(false);

    let inbox_pop = Popover::new();
    inbox_pop.add_css_class("inbox-popover");
    let inbox_list = GtkBox::new(Orientation::Vertical, 0);
    inbox_pop.set_child(Some(&inbox_list));
    let inbox_btn = MenuButton::new();
    inbox_btn.add_css_class("inbox-badge");
    inbox_btn.set_popover(Some(&inbox_pop));
    inbox_btn.set_visible(false);

    let pane_toggle = Button::with_label("▤");
    pane_toggle.add_css_class("flat-btn");
    pane_toggle.set_tooltip_text(Some("Toggle browser pane"));

    let settings_btn = MenuButton::new();
    settings_btn.add_css_class("flat-btn");
    settings_btn.set_label("⚙");

    let theme_btn = Button::with_label("◐");
    theme_btn.add_css_class("flat-btn");
    theme_btn.set_tooltip_text(Some("Toggle theme"));

    topbar.append(&rail_toggle);
    topbar.append(&status_label);
    topbar.append(&model_pill);
    topbar.append(&inbox_btn);
    topbar.append(&pane_toggle);
    topbar.append(&settings_btn);
    topbar.append(&theme_btn);

    // ── rail ──────────────────────────────────────────────────────
    let rail_wrap = GtkBox::new(Orientation::Vertical, 0);
    rail_wrap.add_css_class("rail");
    rail_wrap.set_width_request(settings.rail_width.clamp(RAIL_MIN, RAIL_MAX));
    rail_wrap.set_halign(gtk4::Align::Start);
    rail_wrap.set_overflow(gtk4::Overflow::Hidden);

    // CenterBox: title pinned start, buttons pinned end — WITHOUT hexpand,
    // which leaks expansion up through the rail revealer and starves the
    // center column of width (verified: any hexpand inside the rail makes the
    // rail compete for window space).
    let rail_head = gtk4::CenterBox::new();
    let rail_title = Label::new(Some("Sessions"));
    rail_title.add_css_class("rail-section");
    rail_title.set_xalign(0.0);
    rail_title.set_halign(gtk4::Align::Start);
    let open_link_btn = Button::with_label("🔗");
    open_link_btn.add_css_class("flat-btn");
    open_link_btn.set_tooltip_text(Some("Open a view link"));
    let new_btn = Button::with_label("+");
    new_btn.add_css_class("flat-btn");
    new_btn.set_tooltip_text(Some("New session (Ctrl+N)"));
    let rail_theme_btn = Button::with_label("◐");
    rail_theme_btn.add_css_class("flat-btn");
    rail_theme_btn.set_tooltip_text(Some("Toggle theme"));
    let rail_btns = GtkBox::new(Orientation::Horizontal, 0);
    rail_btns.append(&open_link_btn);
    rail_btns.append(&new_btn);
    rail_btns.append(&rail_theme_btn);
    rail_head.set_start_widget(Some(&rail_title));
    rail_head.set_end_widget(Some(&rail_btns));

    let share_link_entry = Entry::new();
    share_link_entry.add_css_class("rail-search");
    share_link_entry.set_placeholder_text(Some("paste a view link"));
    let share_link_reveal = Revealer::new();
    share_link_reveal.set_transition_type(RevealerTransitionType::SlideDown);
    share_link_reveal.set_transition_duration(180);
    share_link_reveal.set_reveal_child(false);
    share_link_reveal.set_child(Some(&share_link_entry));

    let rail_search = Entry::new();
    rail_search.add_css_class("rail-search");
    rail_search.set_placeholder_text(Some("Filter…"));

    let rail_list = GtkBox::new(Orientation::Vertical, 0);
    rail_list.set_vexpand(true);
    let rail_scroll = ScrolledWindow::new();
    rail_scroll.set_child(Some(&rail_list));
    rail_scroll.set_vexpand(true);
    rail_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

    rail_wrap.append(&rail_head);
    rail_wrap.append(&share_link_reveal);
    rail_wrap.append(&rail_search);
    rail_wrap.append(&rail_scroll);

    let rail_revealer = Revealer::new();
    rail_revealer.set_transition_type(RevealerTransitionType::SlideRight);
    rail_revealer.set_transition_duration(220);
    rail_revealer.set_reveal_child(settings.rail_visible);
    rail_revealer.set_child(Some(&rail_wrap));

    let rail_divider = GtkBox::new(Orientation::Vertical, 0);
    rail_divider.add_css_class("pane-divider");
    rail_divider.set_cursor_from_name(Some("col-resize"));

    // ── transcript ────────────────────────────────────────────────
    let durable_box = GtkBox::new(Orientation::Vertical, 12);
    durable_box.set_valign(gtk4::Align::Start);
    let live_box = GtkBox::new(Orientation::Vertical, 12);
    live_box.set_valign(gtk4::Align::Start);
    let transcript_box = GtkBox::new(Orientation::Vertical, 12);
    transcript_box.add_css_class("transcript");
    transcript_box.set_valign(gtk4::Align::Start);
    // History paging status row: "loading earlier…" while a page is in
    // flight, "start of session" once the top is reached. Survives
    // clear_box(durable_box) on re-attach because it lives outside it.
    let history_status = Label::new(None);
    history_status.add_css_class("rail-section");
    history_status.set_visible(false);
    transcript_box.append(&history_status);
    transcript_box.append(&durable_box);
    transcript_box.append(&live_box);

    let transcript_scroll = ScrolledWindow::new();
    transcript_scroll.set_child(Some(&transcript_box));
    transcript_scroll.set_vexpand(true);
    transcript_scroll.set_hexpand(true);

    // plan slide-over (right edge of the transcript area)
    let plan_box = GtkBox::new(Orientation::Vertical, 6);
    plan_box.add_css_class("plan-panel");
    plan_box.set_size_request(280, -1);
    let plan_title = Label::new(Some("Plan"));
    plan_title.add_css_class("plan-phase");
    plan_title.set_xalign(0.0);
    plan_box.append(&plan_title);
    let plan_scroll = ScrolledWindow::new();
    plan_scroll.set_child(Some(&plan_box));
    plan_scroll.set_propagate_natural_height(true);
    let plan_reveal = Revealer::new();
    plan_reveal.set_transition_type(RevealerTransitionType::SlideLeft);
    plan_reveal.set_transition_duration(220);
    plan_reveal.set_reveal_child(false);
    plan_reveal.set_child(Some(&plan_scroll));
    plan_reveal.set_halign(gtk4::Align::End);
    plan_reveal.set_valign(gtk4::Align::Fill);

    let transcript_overlay = Overlay::new();
    transcript_overlay.set_child(Some(&transcript_scroll));
    transcript_overlay.add_overlay(&plan_reveal);
    transcript_overlay.set_vexpand(true);

    // ── composer ──────────────────────────────────────────────────
    let question_host = GtkBox::new(Orientation::Vertical, 8);

    let composer_box = GtkBox::new(Orientation::Vertical, 4);
    composer_box.add_css_class("composer");

    let chips_strip = GtkBox::new(Orientation::Horizontal, 6);
    chips_strip.add_css_class("attachment-strip");
    chips_strip.set_visible(false);

    let composer = TextView::new();
    composer.add_css_class("composer-view");
    composer.set_wrap_mode(gtk4::WrapMode::WordChar);
    composer.set_hexpand(true);
    let composer_scroll = ScrolledWindow::new();
    composer_scroll.set_child(Some(&composer));
    composer_scroll.set_propagate_natural_height(true);
    composer_scroll.set_min_content_height(28);
    composer_scroll.set_max_content_height(140); // ~6 lines, then internal scroll
    composer_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    composer_scroll.set_hexpand(true);

    let attach_btn = Button::with_label("📎");
    attach_btn.add_css_class("flat-btn");
    attach_btn.set_tooltip_text(Some("Attach image"));
    attach_btn.set_valign(gtk4::Align::End);

    let send_btn = Button::with_label("➤");
    send_btn.add_css_class("send-button");
    send_btn.set_tooltip_text(Some("Send"));
    send_btn.set_valign(gtk4::Align::End);
    let stop_btn = Button::with_label("Stop");
    stop_btn.add_css_class("stop-button");
    stop_btn.set_valign(gtk4::Align::End);
    let queue_btn = Button::with_label("Queue");
    queue_btn.add_css_class("queue-button");
    queue_btn.set_tooltip_text(Some("Queue follow-up while streaming"));
    queue_btn.set_valign(gtk4::Align::End);
    stop_btn.set_visible(false);
    queue_btn.set_visible(false);

    let composer_row = GtkBox::new(Orientation::Horizontal, 6);
    composer_row.append(&attach_btn);
    composer_row.append(&composer_scroll);
    composer_row.append(&send_btn);
    composer_row.append(&stop_btn);
    composer_row.append(&queue_btn);

    let queue_strip = GtkBox::new(Orientation::Horizontal, 6);
    queue_strip.add_css_class("queue-strip");
    queue_strip.set_visible(false);

    let hint = Label::new(Some("esc stops a running turn"));
    hint.add_css_class("composer-hint");
    hint.set_xalign(0.0);

    composer_box.append(&chips_strip);
    composer_box.append(&composer_row);
    composer_box.append(&queue_strip);
    composer_box.append(&hint);

    // ── browser pane (right sidebar) ──────────────────────────────
    let pane_wrap = GtkBox::new(Orientation::Vertical, 0);
    pane_wrap.add_css_class("pane-sidebar");
    pane_wrap.set_width_request(settings.pane_width.clamp(PANE_MIN, PANE_MAX));
    pane_wrap.set_halign(gtk4::Align::Start);
    pane_wrap.set_overflow(gtk4::Overflow::Hidden);

    let pane_head = GtkBox::new(Orientation::Horizontal, 4);
    let url_entry = Entry::new();
    url_entry.set_placeholder_text(Some("https://…"));
    pane_head.append(&url_entry);

    // WebView created lazily on first pane open (see toggle_pane) —
    // WebKitGTK can paint outside a collapsed revealer.
    pane_wrap.append(&pane_head);

    let pane_revealer = Revealer::new();
    pane_revealer.set_transition_type(RevealerTransitionType::SlideLeft);
    pane_revealer.set_transition_duration(220);
    pane_revealer.set_reveal_child(false); // opened via toggle_pane (lazy webview)
    pane_revealer.set_child(Some(&pane_wrap));

    let pane_divider = GtkBox::new(Orientation::Vertical, 0);
    pane_divider.add_css_class("pane-divider");
    pane_divider.set_cursor_from_name(Some("col-resize"));

    // ── center column ─────────────────────────────────────────────
    let center = GtkBox::new(Orientation::Vertical, 0);
    center.set_hexpand(true);
    center.set_vexpand(true);
    center.append(&transcript_overlay);
    center.append(&question_host);
    center.append(&composer_box);

    let body = GtkBox::new(Orientation::Horizontal, 0);
    body.set_hexpand(true);
    body.set_vexpand(true);
    body.append(&rail_revealer);
    body.append(&rail_divider);
    body.append(&center);
    body.append(&pane_divider);
    body.append(&pane_revealer);

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&topbar);
    root.append(&body);
    overlay.set_child(Some(&root));

    // ── login overlay ─────────────────────────────────────────────
    let login_overlay = GtkBox::new(Orientation::Vertical, 0);
    login_overlay.add_css_class("onboarding");
    login_overlay.set_hexpand(true);
    login_overlay.set_vexpand(true);

    let login_card = GtkBox::new(Orientation::Vertical, 12);
    login_card.add_css_class("login-card");
    login_card.set_width_request(400);
    login_card.set_halign(gtk4::Align::Center);
    login_card.set_valign(gtk4::Align::Center);

    let login_title = Label::new(Some("Welcome to Cascade"));
    login_title.add_css_class("login-title");
    let login_sub = Label::new(Some("Sign in to sync sessions across machines."));
    login_sub.add_css_class("login-subtle");
    login_sub.set_wrap(true);

    let email = Entry::new();
    email.add_css_class("login-entry");
    email.set_placeholder_text(Some("email"));
    let password = PasswordEntry::new();
    password.add_css_class("login-entry");
    password.set_show_peek_icon(true);
    password.set_placeholder_text(Some("password"));
    let invite = Entry::new();
    invite.add_css_class("login-entry");
    invite.set_placeholder_text(Some("invite code"));
    invite.set_visible(false);
    let login_btn = Button::with_label("Sign in");
    login_btn.add_css_class("login-button");
    let login_error = Label::new(None);
    login_error.add_css_class("login-error");
    login_error.set_wrap(true);
    let login_mode_link = Button::with_label("Create account");
    login_mode_link.add_css_class("login-link");
    let local_link = Button::with_label("Use locally without account");
    local_link.add_css_class("login-link");

    login_card.append(&login_title);
    login_card.append(&login_sub);
    login_card.append(&email);
    login_card.append(&password);
    login_card.append(&invite);
    login_card.append(&login_btn);
    login_card.append(&login_error);
    login_card.append(&login_mode_link);
    login_card.append(&local_link);
    login_overlay.append(&login_card);
    login_overlay.set_valign(gtk4::Align::Fill);
    login_overlay.set_halign(gtk4::Align::Fill);
    login_overlay.set_visible(false);
    overlay.add_overlay(&login_overlay);


    // ── lightbox ──────────────────────────────────────────────────
    let lightbox_box = GtkBox::new(Orientation::Vertical, 0);
    lightbox_box.add_css_class("lightbox");
    lightbox_box.set_hexpand(true);
    lightbox_box.set_vexpand(true);
    let lightbox_pic = Picture::new();
    lightbox_pic.set_can_shrink(true);
    lightbox_pic.set_hexpand(true);
    lightbox_pic.set_vexpand(true);
    lightbox_pic.set_halign(gtk4::Align::Center);
    lightbox_pic.set_valign(gtk4::Align::Center);
    lightbox_box.append(&lightbox_pic);
    let lightbox = Revealer::new();
    lightbox.set_transition_type(RevealerTransitionType::Crossfade);
    lightbox.set_transition_duration(150);
    lightbox.set_child(Some(&lightbox_box));
    lightbox.set_reveal_child(false);
    // Hidden overlay children with Fill alignment still swallow all pointer
    // input (full-window allocation). Only accept input while revealed.
    lightbox.set_can_target(false);
    overlay.add_overlay(&lightbox);

    // settings popover
    let settings_pop = Popover::new();
    settings_pop.add_css_class("settings-popover");
    let sp_col = GtkBox::new(Orientation::Vertical, 6);
    let dark_check = CheckButton::with_label("Dark mode");
    dark_check.set_active(settings.theme == "moon");
    let sidebar_check = CheckButton::with_label("Show sidebar");
    sidebar_check.set_active(settings.rail_visible);
    let sp_sep = Separator::new(Orientation::Horizontal);
    let url_row = Entry::new();
    url_row.set_text(&settings.cloud_url);
    let save_url_btn = Button::with_label("Save cloud URL");
    save_url_btn.add_css_class("flat-btn");
    let logout_btn = Button::with_label("Log out");
    logout_btn.add_css_class("flat-btn");
    let share_btn = Button::with_label("Share view link");
    share_btn.add_css_class("flat-btn");
    share_btn.set_visible(false);
    let unshare_btn = Button::with_label("Stop sharing");
    unshare_btn.add_css_class("flat-btn");
    unshare_btn.set_visible(false);
    sp_col.append(&dark_check);
    sp_col.append(&sidebar_check);
    sp_col.append(&sp_sep);
    sp_col.append(&url_row);
    sp_col.append(&save_url_btn);
    sp_col.append(&share_btn);
    sp_col.append(&unshare_btn);
    sp_col.append(&logout_btn);
    settings_pop.set_child(Some(&sp_col));
    settings_btn.set_popover(Some(&settings_pop));

    let local_mode = settings.local_mode;
    let pane_visible_init = settings.pane_visible;
    let ui = Rc::new(RefCell::new(Ui {
        window: window.clone(),
        toast,
        toast_reveal,
        login_overlay,
        login_error,
        email: email.clone(),
        password: password.clone(),
        invite: invite.clone(),
        login_btn: login_btn.clone(),
        login_mode_link: login_mode_link.clone(),
        login_sub: login_sub.clone(),
        share_btn: share_btn.clone(),
        unshare_btn: unshare_btn.clone(),
        registering: Cell::new(false),
        sharing: HashSet::new(),
        status_label,
        model_pill,
        inbox_btn: inbox_btn.clone(),
        inbox_list,
        rail_revealer,
        rail_wrap,
        rail_search,
        rail_list,
        transcript_scroll,
        durable_box,
        live_box,
        follow: Cell::new(true),
        programmatic: Cell::new(false),
        current_strip: None,
        history: VecDeque::new(),
        history_oldest_rendered: 0,
        history_server_more: false,
        history_loading: false,
        history_has_content: false,
        history_status,
        plan_reveal,
        plan_box,
        question_host,
        composer,
        attach_btn: attach_btn.clone(),
        composer_hint: hint.clone(),
        share_link_entry: share_link_entry.clone(),
        share_link_reveal: share_link_reveal.clone(),
        read_only: false,
        send_btn: send_btn.clone(),
        stop_btn: stop_btn.clone(),
        queue_btn: queue_btn.clone(),
        chips_strip,
        queue_strip,
        attachments: Vec::new(),
        pane_revealer,
        pane_wrap,
        url_entry,
        webview: None,
        pane_url: None,
        lightbox,
        lightbox_pic,
        cmd: cmd.clone(),
        stream: StreamState::default(),
        seen_fingerprints: VecDeque::new(),
        selected_id: None,
        attached_kind: None,
        metas: Vec::new(),
        machine_names: HashMap::new(),
        settings,
        ended_collapsed: true,
        session_models: HashMap::new(),
        context_usage: HashMap::new(),
        scroll_mem: HashMap::new(),
        buffers: Vec::new(),
        inbox_items: Vec::new(),
        pane_visible: pane_visible_init,
        local_mode,
    }));

    // ── topbar actions ────────────────────────────────────────────
    rail_toggle.connect_clicked(glib::clone!(#[strong] ui, move |_| toggle_rail(&ui)));
    theme_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| toggle_theme(&ui)));
    rail_theme_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| toggle_theme(&ui)));
    new_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| show_new_session_dialog(&ui)));
    open_link_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let u = ui.borrow();
        let next = !u.share_link_reveal.reveals_child();
        u.share_link_reveal.set_reveal_child(next);
        if next {
            u.share_link_entry.grab_focus();
        }
    }));
    share_link_entry.connect_activate(glib::clone!(#[strong] ui, move |entry| {
        let url = entry.text().to_string();
        if url.trim().is_empty() {
            return;
        }
        entry.set_text("");
        ui.borrow().share_link_reveal.set_reveal_child(false);
        let _ = ui.borrow().cmd.try_send(Cmd::OpenShareLink(url));
    }));
    {
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(#[strong] ui, move |_, key, _, _| {
            if key == gdk::Key::Escape {
                ui.borrow().share_link_reveal.set_reveal_child(false);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }));
        share_link_entry.add_controller(keys);
    }
    pane_toggle.connect_clicked(glib::clone!(#[strong] ui, move |_| toggle_pane(&ui)));

    dark_check.connect_toggled(glib::clone!(#[strong] ui, move |c| {
        let want = if c.is_active() { "moon" } else { "dawn" };
        if ui.borrow().settings.theme != want {
            set_theme(&ui, want);
        }
    }));
    sidebar_check.connect_toggled(glib::clone!(#[strong] ui, move |c| {
        set_rail_visible(&ui, c.is_active());
    }));
    save_url_btn.connect_clicked(glib::clone!(#[strong] ui, #[strong] url_row, move |_| {
        let _ = ui.borrow().cmd.try_send(Cmd::SaveCloudUrl(url_row.text().to_string()));
        show_toast(&ui, "cloud URL saved");
    }));
    logout_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let _ = ui.borrow().cmd.try_send(Cmd::Logout);
    }));
    share_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let _ = ui.borrow().cmd.try_send(Cmd::ShareSession);
    }));
    unshare_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let _ = ui.borrow().cmd.try_send(Cmd::UnshareSession);
    }));
    settings_pop.connect_show(glib::clone!(#[strong] ui, move |_| {
        sync_share_buttons(&ui);
    }));

    // inbox: opening the dropdown clears unseen entries
    inbox_btn.connect_notify_local(Some("active"), glib::clone!(#[strong] ui, move |btn, _| {
        if btn.is_active() {
            render_inbox(&ui);
            let _ = ui.borrow().cmd.try_send(Cmd::InboxOpen);
        }
    }));

    // login
    login_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| do_login(&ui)));
    password.connect_activate(glib::clone!(#[strong] ui, move |_| do_login(&ui)));
    invite.connect_activate(glib::clone!(#[strong] ui, move |_| do_login(&ui)));
    login_mode_link.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let next = !ui.borrow().registering.get();
        apply_registering(&ui.borrow(), next);
    }));
    local_link.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let mut u = ui.borrow_mut();
        u.local_mode = true;
        u.settings.local_mode = true;
        let _ = u.settings.save();
        u.login_overlay.set_visible(false);
        drop(u);
        let _ = ui.borrow().cmd.try_send(Cmd::RefreshSessions);
    }));

    // rail search
    {
        let u = ui.borrow();
        u.rail_search.connect_changed(glib::clone!(#[strong] ui, move |_| render_rail(&ui)));
    }

    // ── rail resize drag ──────────────────────────────────────────
    {
        let drag = GestureDrag::new();
        let start_width = Rc::new(Cell::new(0i32));
        drag.connect_drag_begin(glib::clone!(#[strong] ui, #[strong] start_width, move |_, _, _| {
            start_width.set(ui.borrow().rail_wrap.width_request());
        }));
        drag.connect_drag_update(glib::clone!(#[strong] ui, #[strong] start_width, move |_, ox, _| {
            let mut u = ui.borrow_mut();
            let w = start_width.get() + ox as i32;
            if !u.rail_revealer.reveals_child() {
                // collapsed: drag right from the edge reopens
                if ox as i32 > 24 {
                    u.rail_revealer.set_reveal_child(true);
                    u.settings.rail_visible = true;
                }
                return;
            }
            if w < RAIL_MIN - 24 {
                // drag past the minimum snaps the rail closed
                u.rail_revealer.set_reveal_child(false);
                u.settings.rail_visible = false;
                return;
            }
            let w = w.clamp(RAIL_MIN, RAIL_MAX);
            u.rail_wrap.set_width_request(w);
            u.settings.rail_width = w;
        }));
        drag.connect_drag_end(glib::clone!(#[strong] ui, move |_, _, _| {
            let _ = ui.borrow().settings.save();
        }));
        rail_divider.add_controller(drag);
    }

    // ── pane resize drag ──────────────────────────────────────────
    {
        let drag = GestureDrag::new();
        let start_width = Rc::new(Cell::new(0i32));
        drag.connect_drag_begin(glib::clone!(#[strong] ui, #[strong] start_width, move |_, _, _| {
            start_width.set(ui.borrow().pane_wrap.width_request());
        }));
        drag.connect_drag_update(glib::clone!(#[strong] ui, #[strong] start_width, move |_, ox, _| {
            let mut u = ui.borrow_mut();
            let w = (start_width.get() - ox as i32).clamp(PANE_MIN, PANE_MAX);
            u.pane_wrap.set_width_request(w);
            u.settings.pane_width = w;
        }));
        drag.connect_drag_end(glib::clone!(#[strong] ui, move |_, _, _| {
            let _ = ui.borrow().settings.save();
        }));
        pane_divider.add_controller(drag);
    }

    // ── scroll pinning ────────────────────────────────────────────
    {
        let adj = ui.borrow().transcript_scroll.vadjustment();
        adj.connect_value_changed(glib::clone!(#[strong] ui, move |adj| {
            let near_top = {
                let u = ui.borrow();
                !u.programmatic.get() && {
                    let at_bottom = adj.value() >= adj.upper() - adj.page_size() - FOLLOW_MARGIN;
                    u.follow.set(at_bottom);
                    adj.value() < HISTORY_TRIGGER
                }
            };
            if near_top {
                // Defer: the follow tick-callback emits value-changed from
                // inside set_value while holding a Ui borrow — paging here
                // synchronously would double-borrow the RefCell.
                glib::idle_add_local_once(glib::clone!(#[strong] ui, move || {
                    try_load_history(&ui);
                }));
            }
        }));
        // frame-tick follow: scroll after layout when pinned
        ui.borrow().transcript_scroll.add_tick_callback(glib::clone!(#[strong] ui, move |_, _| {
            let u = ui.borrow();
            if u.follow.get() {
                let adj = u.transcript_scroll.vadjustment();
                let target = (adj.upper() - adj.page_size()).max(0.0);
                if (adj.value() - target).abs() > 1.0 {
                    u.programmatic.set(true);
                    adj.set_value(target);
                    u.programmatic.set(false);
                }
            }
            glib::ControlFlow::Continue
        }));
    }

    // ── composer ──────────────────────────────────────────────────
    send_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| send_prompt(&ui)));
    stop_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let _ = ui.borrow().cmd.try_send(Cmd::Abort);
    }));
    queue_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| queue_prompt(&ui)));
    attach_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| pick_attachment(&ui)));

    {
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(glib::clone!(#[strong] ui, move |_, key, _, mods| {
            match key {
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                        return glib::Propagation::Proceed;
                    }
                    send_prompt(&ui);
                    glib::Propagation::Stop
                }
                gdk::Key::Escape => {
                    if ui.borrow().stream.streaming {
                        let _ = ui.borrow().cmd.try_send(Cmd::Abort);
                        return glib::Propagation::Stop;
                    }
                    glib::Propagation::Proceed
                }
                gdk::Key::v | gdk::Key::V if mods.contains(gdk::ModifierType::CONTROL_MASK) => {
                    if paste_image(&ui) {
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                }
                _ => glib::Propagation::Proceed,
            }
        }));
        ui.borrow().composer.add_controller(keys);
    }

    // drag & drop image files onto the composer
    {
        let drop_target = DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
        drop_target.connect_drop(glib::clone!(#[strong] ui, move |_, value, _, _| {
            let Ok(list) = value.get::<gdk::FileList>() else {
                return false;
            };
            let mut added = false;
            for f in list.files() {
                if let Some(path) = f.path() {
                    if is_image_path(&path) {
                        add_attachment(&ui, path);
                        added = true;
                    }
                }
            }
            added
        }));
        ui.borrow().composer.add_controller(drop_target);
    }

    // ── pane URL entry + persistence ──────────────────────────────
    {
        let u = ui.borrow();
        u.url_entry.connect_activate(glib::clone!(#[strong] ui, move |e| {
            let mut url = e.text().to_string();
            if url.trim().is_empty() {
                return;
            }
            if !url.contains("://") {
                url = format!("https://{url}");
            }
            if let Some(wv) = ui.borrow().webview.as_ref() { wv.load_uri(&url); }
        }));
    }

    // ── lightbox click-to-close ───────────────────────────────────
    {
        let click = GestureClick::new();
        click.connect_pressed(glib::clone!(#[strong] ui, move |_, _, _, _| {
            { let lb = &ui.borrow().lightbox; lb.set_reveal_child(false); lb.set_can_target(false); }
        }));
        ui.borrow().lightbox.first_child().unwrap().add_controller(click);
    }

    // ── window shortcuts: Ctrl+B rail, Ctrl+N new, Esc lightbox ───
    {
        let keys = EventControllerKey::new();
        keys.set_propagation_phase(gtk4::PropagationPhase::Capture);
        keys.connect_key_pressed(glib::clone!(#[strong] ui, move |_, key, _, mods| {
            if mods.contains(gdk::ModifierType::CONTROL_MASK) {
                match key {
                    gdk::Key::b | gdk::Key::B => {
                        toggle_rail(&ui);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::n | gdk::Key::N => {
                        show_new_session_dialog(&ui);
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
            }
            if key == gdk::Key::Escape && ui.borrow().lightbox.reveals_child() {
                { let lb = &ui.borrow().lightbox; lb.set_reveal_child(false); lb.set_can_target(false); }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }));
        window.add_controller(keys);
    }

    // relative-time refresh ~30s
    glib::timeout_add_local(Duration::from_secs(30), glib::clone!(#[strong] ui, move || {
        render_rail(&ui);
        glib::ControlFlow::Continue
    }));

    // restore persisted pane state (creates the lazy webview)
    if ui.borrow().settings.pane_visible {
        let ui2 = ui.clone();
        toggle_pane(&ui2);
    }

    // UiMsg pump
    glib::timeout_add_local(Duration::from_millis(16), move || {
        while let Ok(msg) = ui_rx.try_recv() {
            dispatch(&ui, msg);
        }
        glib::ControlFlow::Continue
    });

    window.present();
}

// ── small actions ────────────────────────────────────────────────────

fn do_login(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    let email = u.email.text().to_string();
    let password = u.password.text().to_string();
    u.login_error.set_text("");
    if u.registering.get() {
        let invite = u.invite.text().to_string();
        let _ = u.cmd.try_send(Cmd::Register {
            email,
            password,
            invite,
        });
    } else {
        let _ = u.cmd.try_send(Cmd::Login { email, password });
    }
}

fn apply_registering(u: &Ui, on: bool) {
    u.registering.set(on);
    u.invite.set_visible(on);
    if on {
        u.login_btn.set_label("Create account");
        u.login_mode_link.set_label("Sign in");
        u.login_sub.set_text("Create an account with an invite code.");
    } else {
        u.login_btn.set_label("Sign in");
        u.login_mode_link.set_label("Create account");
        u.login_sub.set_text("Sign in to sync sessions across machines.");
    }
    u.login_error.set_text("");
}

fn sync_share_buttons(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    if u.read_only {
        u.share_btn.set_visible(false);
        u.unshare_btn.set_visible(false);
        return;
    }
    let cloud = u.attached_kind == Some(BackendKind::Cloud);
    let shared = match &u.selected_id {
        Some(id) => u.sharing.contains(id),
        None => false,
    };
    u.share_btn
        .set_visible(cloud && u.selected_id.is_some() && !shared);
    u.unshare_btn
        .set_visible(cloud && u.selected_id.is_some() && shared);
}

fn toggle_rail(ui: &Rc<RefCell<Ui>>) {
    let next = !ui.borrow().rail_revealer.reveals_child();
    set_rail_visible(ui, next);
}

fn set_rail_visible(ui: &Rc<RefCell<Ui>>, visible: bool) {
    let mut u = ui.borrow_mut();
    u.rail_revealer.set_reveal_child(visible);
    u.settings.rail_visible = visible;
    let _ = u.settings.save();
}

fn toggle_pane(ui: &Rc<RefCell<Ui>>) {
    let mut u = ui.borrow_mut();
    let next = !u.pane_revealer.reveals_child();
    if next && u.webview.is_none() {
        // lazy-create: keeps a hidden WebKit view from painting outside the
        // collapsed revealer
        let wv = webkit6::WebView::new();
        wv.set_vexpand(true);
        wv.connect_load_changed(glib::clone!(#[strong] ui, move |wv, ev| {
            if ev == webkit6::LoadEvent::Committed {
                if let Some(uri) = wv.uri() {
                    let uri = uri.to_string();
                    let mut u = ui.borrow_mut();
                    u.pane_url = Some(uri.clone());
                    u.url_entry.set_text(&uri);
                    let _ = u.cmd.try_send(Cmd::PaneToggle(uri));
                }
            }
        }));
        u.pane_wrap.append(&wv);
        if let Some(url) = u.pane_url.clone() {
            wv.load_uri(&url);
        }
        u.webview = Some(wv);
    }
    u.pane_revealer.set_reveal_child(next);
    u.pane_visible = next;
    u.settings.pane_visible = next;
    let _ = u.settings.save();
}

fn toggle_theme(ui: &Rc<RefCell<Ui>>) {
    let cur = ui.borrow().settings.theme.clone();
    set_theme(ui, if cur == "moon" { "dawn" } else { "moon" });
}

fn set_theme(ui: &Rc<RefCell<Ui>>, name: &str) {
    apply_theme(name);
    let mut u = ui.borrow_mut();
    u.settings.theme = name.to_string();
    let _ = u.settings.save();
    for buf in &u.buffers {
        style_buffer(buf, name);
    }
}

// ── transcript helpers ───────────────────────────────────────────────

/// New non-editable transcript TextView with styled buffer (registered for
/// theme switching).
fn new_view(ui: &Rc<RefCell<Ui>>) -> TextView {
    let theme = ui.borrow().settings.theme.clone();
    let buf = TextBuffer::new(None);
    style_buffer(&buf, &theme);
    let tv = TextView::with_buffer(&buf);
    tv.set_editable(false);
    tv.set_cursor_visible(false);
    tv.set_wrap_mode(gtk4::WrapMode::WordChar);
    tv.add_css_class("prose");
    tv.set_left_margin(0);
    tv.set_right_margin(0);
    ui.borrow_mut().buffers.push(buf);
    attach_link_handlers(&tv);
    tv
}

fn insert_tagged(buf: &TextBuffer, text: &str, tag: &str) {
    let mut end = buf.end_iter();
    buf.insert_with_tags_by_name(&mut end, text, &[tag]);
}

fn insert_named(buf: &TextBuffer, text: &str, tags: &[&str]) {
    let mut end = buf.end_iter();
    buf.insert_with_tags_by_name(&mut end, text, tags);
}

fn insert_run(buf: &TextBuffer, run: &markdown::Run, extra: &[&str]) {
    let start_off = buf.end_iter().offset();
    // "assistant" is first in the tag table (lowest priority): fontless
    // modifiers inherit serif 13.5, while md-inline-code's mono 12 still wins.
    let mut names: Vec<&str> = Vec::with_capacity(extra.len() + 3);
    if run.tag != "assistant" {
        names.push("assistant");
    }
    names.push(run.tag);
    names.extend_from_slice(extra);
    if run.link.is_some() && run.tag != "md-link" {
        names.push("md-link");
    }
    insert_named(buf, &run.text, &names);
    if let Some(url) = run.link.as_deref().filter(|u| !u.is_empty()) {
        apply_link_url(buf, start_off, url);
    }
}

fn insert_runs(buf: &TextBuffer, runs: &[markdown::Run], extra: &[&str]) {
    for run in runs {
        insert_run(buf, run, extra);
    }
}

const LINK_URL_PREFIX: &str = "md-url:";

fn apply_link_url(buf: &TextBuffer, start_off: i32, url: &str) {
    let name = format!("{LINK_URL_PREFIX}{url}");
    let table = buf.tag_table();
    if table.lookup(&name).is_none() {
        table.add(&TextTag::new(Some(&name)));
    }
    let start = buf.iter_at_offset(start_off);
    let end = buf.end_iter();
    buf.apply_tag_by_name(&name, &start, &end);
}

fn link_url_at(view: &TextView, x: f64, y: f64) -> Option<String> {
    let (bx, by) = view.window_to_buffer_coords(gtk4::TextWindowType::Widget, x as i32, y as i32);
    let iter = view.iter_at_location(bx, by)?;
    for tag in iter.tags() {
        if let Some(name) = tag.name() {
            if let Some(url) = name.strip_prefix(LINK_URL_PREFIX) {
                if !url.is_empty() {
                    return Some(url.to_string());
                }
            }
        }
    }
    None
}

fn attach_link_handlers(view: &TextView) {
    let click = GestureClick::new();
    click.set_button(1);
    let view_c = view.clone();
    click.connect_pressed(move |gest, _, x, y| {
        if let Some(url) = link_url_at(&view_c, x, y) {
            gest.set_state(gtk4::EventSequenceState::Claimed);
            let parent = view_c.root().and_then(|r| r.downcast::<gtk4::Window>().ok());
            #[allow(deprecated)]
            gtk4::show_uri(parent.as_ref(), &url, 0);
        }
    });
    view.add_controller(click);

    let motion = EventControllerMotion::new();
    let view_m = view.clone();
    motion.connect_motion(move |_, x, y| {
        if link_url_at(&view_m, x, y).is_some() {
            view_m.set_cursor_from_name(Some("pointer"));
        } else {
            view_m.set_cursor_from_name(None);
        }
    });
    let view_l = view.clone();
    motion.connect_leave(move |_| {
        view_l.set_cursor_from_name(None);
    });
    view.add_controller(motion);
}

fn heading_tag(level: u8) -> &'static str {
    match level {
        1 => "md-h1",
        2 => "md-h2",
        3 => "md-h3",
        _ => "md-h4",
    }
}

fn list_marker(level: u8, kind: markdown::ListKind) -> String {
    match kind {
        markdown::ListKind::Bullet => {
            let glyph = match level {
                0 => "•",
                1 => "◦",
                _ => "▪",
            };
            format!("{glyph} ")
        }
        markdown::ListKind::Numbered(n) => format!("{n}. "),
        markdown::ListKind::Task { checked } => {
            if checked {
                "☑ ".into()
            } else {
                "☐ ".into()
            }
        }
    }
}

fn ensure_hang_tag(buf: &TextBuffer, level: u8) -> String {
    let name = format!("md-hang-{level}");
    let table = buf.tag_table();
    if table.lookup(&name).is_none() {
        let tag = TextTag::new(Some(&name));
        tag.set_left_margin(i32::from(level) * 24 + 28);
        tag.set_indent(-28);
        table.add(&tag);
    }
    name
}

fn flatten_runs(runs: &[markdown::Run]) -> String {
    runs.iter()
        .map(|r| r.text.as_str())
        .collect::<String>()
        .replace(['\n', '\r'], " ")
}

fn char_display_width(c: char) -> usize {
    let u = c as u32;
    if c.is_control() {
        0
    } else if (0x1100..=0x115F).contains(&u)
        || (0x2329..=0x232A).contains(&u)
        || (0x2E80..=0xA4CF).contains(&u)
        || (0xAC00..=0xD7A3).contains(&u)
        || (0xF900..=0xFAFF).contains(&u)
        || (0xFE10..=0xFE19).contains(&u)
        || (0xFE30..=0xFE6F).contains(&u)
        || (0xFF00..=0xFF60).contains(&u)
        || (0xFFE0..=0xFFE6).contains(&u)
        || (0x1F300..=0x1F64F).contains(&u)
        || (0x1F900..=0x1F9FF).contains(&u)
        || (0x20000..=0x3FFFD).contains(&u)
    {
        2
    } else {
        1
    }
}

fn display_width(s: &str) -> usize {
    s.chars().map(char_display_width).sum()
}

/// Greedy word-boundary wrap for bodies rendered with wrap=None —
/// keeps prose readable while the widget's height stays an exact line
/// count (no height-for-width slab).
fn flush_pending_text(ui: &Rc<RefCell<Ui>>, tv: &TextView) {
    let text = {
        let mut u = ui.borrow_mut();
        std::mem::take(&mut u.stream.pending_text)
    };
    if text.is_empty() {
        return;
    }
    insert_faded(ui, tv, &text);
    tv.queue_resize();
}

fn schedule_text_flush(ui: &Rc<RefCell<Ui>>, tv: TextView) {
    if ui.borrow().stream.flush_scheduled {
        return;
    }
    ui.borrow_mut().stream.flush_scheduled = true;
    let ui2 = ui.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(40), move || {
        ui2.borrow_mut().stream.flush_scheduled = false;
        flush_pending_text(&ui2, &tv);
        glib::ControlFlow::Break
    });
}

/// Parse "#RRGGBB" into an RGBA at the given alpha.
fn hex_rgba(hex: &str, alpha: f64) -> gdk::RGBA {
    let h = hex.trim_start_matches('#');
    let p = |i: usize| f64::from(u8::from_str_radix(&h[i..i + 2], 16).unwrap_or(0)) / 255.0;
    gdk::RGBA::new(p(0) as f32, p(2) as f32, p(4) as f32, alpha as f32)
}

/// Insert a text batch with an alpha-ramp fade-in: the run materializes over
/// ~180ms, then the fade tag comes off and the base tag's color shows.
fn insert_faded(ui: &Rc<RefCell<Ui>>, tv: &TextView, text: &str) {
    let theme = ui.borrow().settings.theme.clone();
    let hex = palette(&theme).text;
    let buf = tv.buffer();
    let Some(base) = buf.tag_table().lookup("assistant") else {
        insert_tagged(&buf, text, "assistant");
        return;
    };
    let fade = gtk4::TextTag::new(None);
    fade.set_foreground_rgba(Some(&hex_rgba(hex, 0.0)));
    buf.tag_table().add(&fade);
    let start = buf.end_iter();
    let mut end = buf.end_iter();
    buf.insert_with_tags(&mut end, text, &[&fade, &base]);
    let mut step = 0u32;
    let hex = hex.to_string();
    glib::timeout_add_local(std::time::Duration::from_millis(20), move || {
        step += 1;
        let a = (step as f64 / 9.0).min(1.0);
        fade.set_foreground_rgba(Some(&hex_rgba(&hex, a)));
        if step >= 9 {
            let mut e = buf.end_iter();
            buf.remove_tag(&fade, &start, &mut e);
            buf.tag_table().remove(&fade);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

fn soft_wrap(text: &str, width: usize) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let mut rest = line.trim_start();
        while rest.chars().count() > width {
            let cut = rest
                .char_indices()
                .nth(width)
                .map(|(i, _)| i)
                .unwrap_or(rest.len());
            let break_at = rest[..cut].rfind(' ').map(|i| i + 1).unwrap_or(cut);
            out.push_str(rest[..break_at].trim_end());
            out.push('\n');
            rest = rest[break_at..].trim_start();
        }
        out.push_str(rest);
        out.push('\n');
    }
    out.trim_end_matches('\n').to_string()
}

fn pad_cell(text: &str, width: usize, align: markdown::Align) -> String {
    let w = display_width(text);
    let pad = width.saturating_sub(w);
    match align {
        markdown::Align::Left => format!("{text}{}", " ".repeat(pad)),
        markdown::Align::Right => format!("{}{text}", " ".repeat(pad)),
        markdown::Align::Center => {
            let left = pad / 2;
            format!("{}{text}{}", " ".repeat(left), " ".repeat(pad - left))
        }
    }
}

fn format_table_lines(
    header: &[Vec<markdown::Run>],
    aligns: &[markdown::Align],
    rows: &[Vec<Vec<markdown::Run>>],
) -> (String, String, Vec<String>) {
    let cols = header.len();
    let mut widths = vec![1usize; cols];
    let head_txt: Vec<String> = header.iter().map(|c| flatten_runs(c)).collect();
    for (i, t) in head_txt.iter().enumerate() {
        widths[i] = widths[i].max(display_width(t));
    }
    let row_txt: Vec<Vec<String>> = rows
        .iter()
        .map(|row| {
            (0..cols)
                .map(|i| row.get(i).map(|c| flatten_runs(c)).unwrap_or_default())
                .collect()
        })
        .collect();
    for row in &row_txt {
        for (i, t) in row.iter().enumerate() {
            widths[i] = widths[i].max(display_width(t));
        }
    }
    let align_at = |i: usize| aligns.get(i).copied().unwrap_or(markdown::Align::Left);
    let fmt_row = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, t)| pad_cell(t, widths[i], align_at(i)))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header_line = fmt_row(&head_txt);
    let sep = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("  ");
    let body = row_txt.iter().map(|r| fmt_row(r)).collect();
    (header_line, sep, body)
}

fn append_alt_prose(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, alt: &str) {
    if alt.is_empty() {
        return;
    }
    let tv = new_view(ui);
    insert_tagged(&tv.buffer(), alt, "assistant");
    attach_copy_menu(&tv, "Copy text");
    parent.append(&tv);
}

fn markdown_picture(ui: &Rc<RefCell<Ui>>, texture: &gdk::Texture) -> Picture {
    let pic = Picture::for_paintable(texture);
    pic.add_css_class("transcript-image");
    pic.set_can_shrink(true);
    pic.set_halign(gtk4::Align::Start);
    let w = texture.width();
    let h = texture.height();
    let max_w = 240;
    if w > max_w && w > 0 {
        let nh = ((h as i64) * (max_w as i64) / (w as i64)) as i32;
        pic.set_size_request(max_w, nh.max(1));
    } else if w > 0 {
        pic.set_size_request(w, h);
    } else {
        pic.set_size_request(max_w, -1);
    }
    let ui2 = ui.clone();
    let tex = texture.clone();
    let click = GestureClick::new();
    click.connect_pressed(move |_, _, _, _| {
        show_lightbox(&ui2, &tex);
    });
    pic.add_controller(click);
    pic
}

fn local_path_from_target(target: &str) -> PathBuf {
    if let Some(rest) = target.strip_prefix("file://") {
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        PathBuf::from(rest)
    } else {
        PathBuf::from(target)
    }
}

const MAX_MD_IMAGE: usize = 10 * 1024 * 1024;

fn append_markdown_image(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, alt: &str, target: &str) {
    if target.starts_with("http://") || target.starts_with("https://") {
        append_remote_image(ui, parent, alt, target);
        return;
    }
    let path = local_path_from_target(target);
    match gdk::Texture::from_filename(&path) {
        Ok(tex) => parent.append(&markdown_picture(ui, &tex)),
        Err(_) => append_alt_prose(ui, parent, alt),
    }
}

fn append_remote_image(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, alt: &str, url: &str) {
    let holder = GtkBox::new(Orientation::Vertical, 0);
    parent.append(&holder);
    let ui = ui.clone();
    let alt = alt.to_string();
    let file = gio::File::for_uri(url);
    file.load_contents_async(None::<&gio::Cancellable>, move |res| {
        let outcome = (|| {
            let (bytes, _) = res.ok()?;
            if bytes.len() > MAX_MD_IMAGE {
                return None;
            }
            let (ctype, _) = gio::content_type_guess(None::<&Path>, &bytes);
            let mime = gio::content_type_get_mime_type(&ctype)
                .map(|m| m.to_string())
                .unwrap_or_else(|| ctype.to_string());
            if !mime.starts_with("image/") {
                return None;
            }
            let ext = mime.rsplit('/').next().unwrap_or("img");
            let ext = ext.split('+').next().unwrap_or(ext);
            let path = std::env::temp_dir().join(format!("cascade-md-{}.{ext}", uuid::Uuid::new_v4()));
            std::fs::write(&path, &bytes).ok()?;
            gdk::Texture::from_filename(&path).ok()
        })();
        glib::idle_add_local_once(move || match outcome {
            Some(tex) => holder.append(&markdown_picture(&ui, &tex)),
            None => append_alt_prose(&ui, &holder, &alt),
        });
    });
}

fn copy_text(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

/// Right-click "Copy text" menu on a prose view.
fn attach_copy_menu(view: &TextView, label: &str) {
    let click = GestureClick::new();
    click.set_button(3);
    let view = view.clone();
    let label = label.to_string();
    let view_c = view.clone();
    click.connect_pressed(move |gest, _, x, y| {
        gest.set_state(gtk4::EventSequenceState::Claimed);
        let pop = Popover::new();
        pop.add_css_class("copy-menu");
        pop.set_has_arrow(false);
        let copy_btn = Button::with_label(&label);
        copy_btn.add_css_class("copy-menu-item");
        let buf = view_c.buffer();
        let view_p = view_c.clone();
        copy_btn.connect_clicked(move |b| {
            let text = if buf.has_selection() {
                let (mut s, mut e) = buf.selection_bounds().unwrap();
                buf.text(&mut s, &mut e, false).to_string()
            } else {
                buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
            };
            copy_text(&text);
            if let Some(p) = b
                .ancestor(Popover::static_type())
                .and_then(|w| w.downcast::<Popover>().ok())
            {
                p.popdown();
            }
        });
        pop.set_child(Some(&copy_btn));
        pop.set_parent(&view_p);
        pop.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        pop.popup();
    });
    view.add_controller(click);
}

/// Render one assistant markdown body into `parent` (durable or live box).
fn render_markdown_into(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, body: &str) {
    clear_strip(ui); // prose closes the current tool burst
    let poke = ui.borrow().transcript_scroll.clone();
    glib::idle_add_local_once(move || poke.queue_resize());
    if body.contains("Wall time") {
    }
    for block in markdown::parse_blocks(body) {
        match block {
            Block::Prose(runs) => {
                let tv = new_view(ui);
                insert_runs(&tv.buffer(), &runs, &[]);
                attach_copy_menu(&tv, "Copy text");
                parent.append(&tv);
            }
            Block::Heading { level, runs } => {
                let tv = new_view(ui);
                insert_runs(&tv.buffer(), &runs, &[heading_tag(level)]);
                attach_copy_menu(&tv, "Copy text");
                parent.append(&tv);
            }
            Block::ListItem { level, kind, runs } => {
                let tv = new_view(ui);
                let buf = tv.buffer();
                let hang = ensure_hang_tag(&buf, level);
                let marker = list_marker(level, kind);
                let checked = matches!(kind, markdown::ListKind::Task { checked: true });
                if checked {
                    insert_named(&buf, &marker, &["md-list-marker", "md-bold"]);
                } else {
                    insert_named(&buf, &marker, &["md-list-marker"]);
                }
                insert_runs(&buf, &runs, &[]);
                let start = buf.start_iter();
                let end = buf.end_iter();
                buf.apply_tag_by_name(&hang, &start, &end);
                attach_copy_menu(&tv, "Copy text");
                parent.append(&tv);
            }
            Block::Quote { runs } => {
                let card = GtkBox::new(Orientation::Vertical, 0);
                card.add_css_class("md-quote-card");
                let tv = new_view(ui);
                insert_runs(&tv.buffer(), &runs, &["md-quote"]);
                attach_copy_menu(&tv, "Copy text");
                card.append(&tv);
                parent.append(&card);
            }
            Block::Rule => {
                let rule = Separator::new(Orientation::Horizontal);
                rule.add_css_class("md-rule");
                parent.append(&rule);
            }
            Block::Table {
                header,
                aligns,
                rows,
            } => {
                let card = GtkBox::new(Orientation::Vertical, 0);
                card.add_css_class("md-table-card");
                let scroll = ScrolledWindow::new();
                scroll.set_policy(gtk4::PolicyType::Automatic, gtk4::PolicyType::Never);
                scroll.set_propagate_natural_height(true);
                scroll.set_hexpand(true);
                let tv = new_view(ui);
                tv.set_wrap_mode(gtk4::WrapMode::None);
                let buf = tv.buffer();
                let (head, sep, body_lines) = format_table_lines(&header, &aligns, &rows);
                insert_named(&buf, &head, &["md-table", "md-bold"]);
                insert_named(&buf, "\n", &["md-table"]);
                insert_named(&buf, &sep, &["md-table"]);
                for line in body_lines {
                    insert_named(&buf, "\n", &["md-table"]);
                    insert_named(&buf, &line, &["md-table"]);
                }
                attach_copy_menu(&tv, "Copy text");
                scroll.set_child(Some(&tv));
                card.append(&scroll);
                parent.append(&card);
            }
            Block::Code { lang, code } => {
                parent.append(&code_block_widget(ui, &lang, &code));
            }
            Block::Advisory {
                severity,
                guidance,
                body,
            } => {
                let card = GtkBox::new(Orientation::Vertical, 4);
                card.add_css_class("advisory-card");
                let sev = severity.as_deref().unwrap_or("");
                card.add_css_class(if sev.eq_ignore_ascii_case("error") {
                    "advisory-error"
                } else {
                    "advisory-info"
                });
                if !sev.is_empty() {
                    let chip = Label::new(Some(&sev.to_uppercase()));
                    chip.add_css_class("advisory-chip");
                    chip.set_xalign(0.0);
                    card.append(&chip);
                }
                if let Some(g) = guidance.as_deref().filter(|s| !s.is_empty()) {
                    let gl = Label::new(Some(g));
                    gl.add_css_class("advisory-guidance");
                    gl.set_wrap(true);
                    gl.set_xalign(0.0);
                    gl.set_selectable(true);
                    card.append(&gl);
                }
                if !body.trim().is_empty() {
                    let inner = GtkBox::new(Orientation::Vertical, 0);
                    render_markdown_into(ui, &inner, &body);
                    card.append(&inner);
                }
                parent.append(&card);
            }
            Block::Image { alt, target } => {
                append_markdown_image(ui, parent, &alt, &target);
            }
        }
    }
}

/// Code block: header (lang + copy) + highlighted body + hover ghost copy.
/// Code block: language label header + highlighted body. Copy buttons are
/// gone (right-click "Copy code" remains); tall blocks cap at CAP_LINES
/// with an expander bar so a huge dump can't eat the scroll.
const CODE_CAP_PX: i32 = 340; // ~20 lines of 12.5pt mono

fn code_block_widget(ui: &Rc<RefCell<Ui>>, lang: &str, code: &str) -> GtkBox {
    let card = GtkBox::new(Orientation::Vertical, 0);
    card.add_css_class("code-block");

    let header = GtkBox::new(Orientation::Horizontal, 6);
    let lang_label = Label::new(Some(&if lang.is_empty() {
        "CODE".to_string()
    } else {
        lang.to_uppercase()
    }));
    lang_label.add_css_class("code-header");
    lang_label.set_xalign(0.0);
    lang_label.set_hexpand(true);
    header.append(&lang_label);
    card.append(&header);

    let tv = new_view(ui);
    tv.set_wrap_mode(gtk4::WrapMode::None);
    let buf = tv.buffer();
    for (text, tag) in highlight::highlight(code, lang) {
        insert_tagged(&buf, &text, tag);
    }
    attach_copy_menu(&tv, "Copy code");

    let lines: Vec<&str> = code.lines().collect();
    let cap_lines = (CODE_CAP_PX as f64 / 17.0) as usize;
    if lines.len() > cap_lines {
        // Cap by swapping buffer content, not a nested scroll window — a
        // TextView inside ScrolledWindow with natural-height propagation
        // measures to zero and shows an empty body.
        let hidden = lines.len() - cap_lines;
        let code_owned = code.to_string();
        let capped_slice = lines[..cap_lines].join("\n");
        let lang_owned = lang.to_string();
        let render = move |buf: &gtk4::TextBuffer, full: bool| {
            buf.set_text("");
            let slice = if full { &code_owned } else { &capped_slice };
            for (text, tag) in highlight::highlight(slice, &lang_owned) {
                insert_tagged(buf, &text, tag);
            }
        };
        render(&buf, false);
        tv.queue_resize();
        card.append(&tv);

        let more = Button::with_label(&format!("{hidden} more lines ▾"));
        more.add_css_class("code-more");
        let expanded = std::cell::Cell::new(false);
        let ui_c = ui.clone();
        let tv_c = tv.clone();
        more.connect_clicked(move |btn| {
            // Same rule as chips and card chevrons: expanding pushes content
            // down; the bottom-pin must not convert it into an upward shove.
            ui_c.borrow().follow.set(false);
            let now = !expanded.get();
            expanded.set(now);
            render(&buf, now);
            tv_c.queue_resize();
            btn.set_label(if now {
                "show less ▴"
            } else {
                ""
            });
            if !now {
                btn.set_label(&format!("{hidden} more lines ▾"));
            }
        });
        card.append(&more);
    } else {
        card.append(&tv);
    }
    card
}

/// Right-aligned user bubble card.
fn append_user_bubble(ui: &Rc<RefCell<Ui>>, text: &str, images: &[PathBuf]) {
    append_user_bubble_inner(ui, text, images, false)
}

/// Optimistic render at send time; MessageEnd(user) echoes the same message
/// back — skip the duplicate.
fn append_user_bubble_inner(ui: &Rc<RefCell<Ui>>, text: &str, images: &[PathBuf], from_echo: bool) {
    // Always the live tail. Discovered sessions never settle (no turn
    // lifecycle events), so the durable-box branch dropped bubbles in the
    // middle of the transcript — right after the previous user message —
    // while the turn's work rendered below in live_box.
    let target = ui.borrow().live_box.clone();
    user_bubble_into(ui, &target, text, images, from_echo);
}

/// Build a user bubble into an explicit parent — shared by the live append
/// path and history-page prepending (off-tree staging box).
fn user_bubble_into(
    ui: &Rc<RefCell<Ui>>,
    target: &GtkBox,
    text: &str,
    images: &[PathBuf],
    from_echo: bool,
) {
    clear_strip(ui);
    if !from_echo {
        ui.borrow_mut().stream.last_user_echo =
            Some((text.to_string(), std::time::Instant::now()));
    }
    let bubble = GtkBox::new(Orientation::Vertical, 6);
    bubble.add_css_class("user-bubble");
    bubble.set_halign(gtk4::Align::End);
    bubble.set_margin_start(120);

    // A wrapping TextView reports ~zero natural width (text wraps to any
    // width), which collapsed the bubble to a 28px sliver. A Label with
    // max-width-chars measures its text and sizes the card correctly.
    let l = Label::new(Some(text));
    l.add_css_class("user-bubble-text");
    l.set_wrap(true);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    l.set_selectable(true);
    l.set_xalign(0.0);
    l.set_max_width_chars(72);
    bubble.append(&l);

    for path in images {
        if let Ok(texture) = gdk::Texture::from_filename(path) {
            let pic = Picture::for_paintable(&texture);
            pic.add_css_class("transcript-image");
            pic.set_can_shrink(true);
            pic.set_size_request(240, -1);
            let ui2 = ui.clone();
            let tex = texture.clone();
            let click = GestureClick::new();
            click.connect_pressed(move |_, _, _, _| {
                show_lightbox(&ui2, &tex);
            });
            pic.add_controller(click);
            bubble.append(&pic);
        }
    }

    target.append(&bubble);
}

fn show_lightbox(ui: &Rc<RefCell<Ui>>, texture: &gdk::Texture) {
    let u = ui.borrow();
    u.lightbox_pic.set_paintable(Some(texture));
    u.lightbox.set_can_target(true);
    u.lightbox.set_reveal_child(true);
}

/// Collapsible tool/thinking card.
// ── tool strip ───────────────────────────────────────────────────────
// One horizontal row of compact chips per contiguous burst of tool calls.
// Chips fly in live; tapping one expands the full card inline below.

#[derive(Clone, Copy, PartialEq)]
enum ChipStatus {
    Running,
    Done,
    Error,
}

struct Chip {
    btn: Button,
    dot: Label,
    name: String,
    intent: String,
    args: String,
    result: Option<String>,
    status: ChipStatus,
    is_thinking: bool,
}

#[derive(Default)]
struct StripState {
    chips: HashMap<String, Chip>,
    open: Option<String>,
}

#[derive(Clone)]
struct ToolStrip {
    state: Rc<RefCell<StripState>>,
    chips_box: GtkBox,
    expansion: GtkBox,
}

impl ToolStrip {
    fn new(parent: &GtkBox) -> Self {
        let container = GtkBox::new(Orientation::Vertical, 4);
        container.add_css_class("tool-strip");
        let scroll = ScrolledWindow::new();
        scroll.add_css_class("tool-strip-scroll");
        scroll.set_hscrollbar_policy(gtk4::PolicyType::Automatic);
        scroll.set_vscrollbar_policy(gtk4::PolicyType::Never);
        scroll.set_propagate_natural_height(true);
        let chips_box = GtkBox::new(Orientation::Horizontal, 6);
        chips_box.add_css_class("tool-chips");
        scroll.set_child(Some(&chips_box));
        container.append(&scroll);
        let expansion = GtkBox::new(Orientation::Vertical, 4);
        container.append(&expansion);
        parent.append(&container);
        Self {
            state: Rc::new(RefCell::new(StripState::default())),
            chips_box,
            expansion,
        }
    }

    fn has(&self, id: &str) -> bool {
        !id.is_empty() && self.state.borrow().chips.contains_key(id)
    }

    fn add_call(
        &self,
        ui: &Rc<RefCell<Ui>>,
        id: &str,
        name: &str,
        intent: &str,
        args: &str,
        animate: bool,
    ) {
        if self.has(id) {
            return;
        }
        let key = if id.is_empty() {
            format!("anon-{}", self.state.borrow().chips.len())
        } else {
            id.to_string()
        };
        let btn = Button::new();
        btn.add_css_class("chip");
        if animate {
            btn.add_css_class("chip-enter");
        }
        let row = GtkBox::new(Orientation::Horizontal, 6);
        let dot = Label::new(Some("●"));
        dot.add_css_class("chip-dot");
        dot.add_css_class("chip-dot-running");
        row.append(&dot);
        let name_l = Label::new(Some(&name.to_uppercase()));
        name_l.add_css_class("chip-name");
        row.append(&name_l);
        if !intent.is_empty() {
            let intent_l = Label::new(Some(intent));
            intent_l.add_css_class("chip-intent");
            intent_l.set_ellipsize(pango::EllipsizeMode::End);
            intent_l.set_max_width_chars(30);
            row.append(&intent_l);
        }
        btn.set_child(Some(&row));
        let strip = self.clone();
        let ui2 = ui.clone();
        let key2 = key.clone();
        btn.connect_clicked(move |_| strip.toggle_open(&ui2, &key2));
        self.chips_box.append(&btn);
        self.state.borrow_mut().chips.insert(
            key,
            Chip {
                btn: btn.clone(),
                dot,
                name: name.to_string(),
                intent: intent.to_string(),
                args: args.to_string(),
                result: None,
                status: ChipStatus::Running,
                is_thinking: false,
            },
        );
        if animate {
            glib::idle_add_local_once(move || {
                btn.remove_css_class("chip-enter");
                btn.add_css_class("chip-in");
            });
        }
    }

    fn add_result(
        &self,
        ui: &Rc<RefCell<Ui>>,
        id: &str,
        name: &str,
        result: &str,
        is_error: bool,
    ) {
        if !self.has(id) {
            self.add_call(ui, id, name, "", "", false);
        }
        let key = self.resolve_key(id);
        let mut st = self.state.borrow_mut();
        let Some(chip) = st.chips.get_mut(&key) else { return };
        chip.result = Some(result.to_string());
        chip.status = if is_error {
            ChipStatus::Error
        } else {
            ChipStatus::Done
        };
        chip.dot.remove_css_class("chip-dot-running");
        chip.dot.add_css_class(if is_error {
            "chip-dot-error"
        } else {
            "chip-dot-done"
        });
        // Errors go red and stay CLOSED — a failed tool never hijacks the
        // transcript with a forced expansion. Tap to read the failure.
        if is_error {
            chip.btn.add_css_class("chip-error");
        }
        let should_open = st.open.as_deref() == Some(key.as_str());
        drop(st);
        if should_open {
            self.render_expansion(ui, &key);
        }
    }

    /// A thinking block joins the burst as a muted chip — same one-line
    /// footprint as a tool, tap to read the reasoning below.
    /// Create the thinking chip if missing, else update its text in place.
    /// Drives the streaming-thinking chip (key "think-live") and finalizes it
    /// on message_end without ever rendering a second chip.
    fn upsert_thinking(&self, ui: &Rc<RefCell<Ui>>, key: &str, text: &str, animate: bool) {
        if !self.has(key) {
            self.add_thinking_keyed(ui, key, text, animate);
            return;
        }
        let should_render = {
            let mut st = self.state.borrow_mut();
            if let Some(chip) = st.chips.get_mut(key) {
                chip.result = Some(text.to_string());
            }
            st.open.as_deref() == Some(key)
        };
        if should_render {
            self.render_expansion(ui, key);
        }
    }

    fn add_thinking(&self, ui: &Rc<RefCell<Ui>>, text: &str, animate: bool) {
        let key = format!("think-{}", self.state.borrow().chips.len());
        self.add_thinking_keyed(ui, &key, text, animate);
    }

    fn add_thinking_keyed(&self, ui: &Rc<RefCell<Ui>>, key: &str, text: &str, animate: bool) {
        let key = key.to_string();
        let btn = Button::new();
        btn.add_css_class("chip");
        btn.add_css_class("chip-thinking");
        if animate {
            btn.add_css_class("chip-enter");
        }
        let row = GtkBox::new(Orientation::Horizontal, 6);
        let name_l = Label::new(Some("thinking"));
        name_l.add_css_class("chip-name");
        name_l.add_css_class("chip-thinking-name");
        row.append(&name_l);
        btn.set_child(Some(&row));
        let strip = self.clone();
        let ui2 = ui.clone();
        let key2 = key.clone();
        btn.connect_clicked(move |_| strip.toggle_open(&ui2, &key2));
        self.chips_box.append(&btn);
        self.state.borrow_mut().chips.insert(
            key,
            Chip {
                btn: btn.clone(),
                dot: Label::new(None),
                name: "THINKING".to_string(),
                intent: String::new(),
                args: String::new(),
                result: Some(text.to_string()),
                status: ChipStatus::Done,
                is_thinking: true,
            },
        );
        if animate {
            glib::idle_add_local_once(move || {
                btn.remove_css_class("chip-enter");
                btn.add_css_class("chip-in");
            });
        }
    }

    fn set_partial(&self, id: &str, partial: &str) {
        let key = self.resolve_key(id);
        if let Some(chip) = self.state.borrow_mut().chips.get_mut(&key) {
            chip.result = Some(partial.to_string());
        }
    }

    fn resolve_key(&self, id: &str) -> String {
        if !id.is_empty() && self.has(id) {
            return id.to_string();
        }
        // Anonymous chips created for id-less calls: results without ids
        // land on the most recent chip.
        self.state
            .borrow()
            .chips
            .keys()
            .last()
            .cloned()
            .unwrap_or_else(|| id.to_string())
    }

    fn toggle_open(&self, ui: &Rc<RefCell<Ui>>, id: &str) {
        // Unpin from the bottom: the expansion must push content below it
        // down — with follow set, the bottom-pin converts that into an
        // upward shove that moves the tapped chip off screen.
        ui.borrow().follow.set(false);
        let mut st = self.state.borrow_mut();
        if st.open.as_deref() == Some(id) {
            st.open = None;
            drop(st);
            clear_box(&self.expansion);
        } else {
            st.open = Some(id.to_string());
            drop(st);
            self.render_expansion(ui, id);
        }
    }

    fn render_expansion(&self, ui: &Rc<RefCell<Ui>>, id: &str) {
        clear_box(&self.expansion);
        let st = self.state.borrow();
        let Some(chip) = st.chips.get(id) else { return };
        let meta = if chip.intent.is_empty() {
            String::new()
        } else {
            format!("· {}", chip.intent)
        };
        let (card, body, _chev) = if chip.is_thinking {
            make_tool_card(ui, "tool-thinking", "THINKING", "", "thinking")
        } else {
            make_tool_card(ui, "tool-tool-use", &chip.name.to_uppercase(), &meta, "tool")
        };
        let buf = body.buffer();
        if chip.is_thinking {
            if let Some(r) = &chip.result {
                insert_tagged(&buf, &soft_wrap(r, 110), "thinking");
            }
            self.expansion.append(&card);
            return;
        }
        // Compose args + result, then cap tall bodies with an expander —
        // a giant payload can't eat the screen even when tapped open.
        let mut full = chip.args.clone();
        if let Some(r) = &chip.result {
            if !full.is_empty() {
                full.push('\n');
            }
            full.push_str(r);
        } else {
            full.push_str("\nrunning…");
        }
        let lines: Vec<&str> = full.lines().collect();
        const EXPANSION_CAP: usize = 20;
        let mut expand_btn: Option<Button> = None;
        if lines.len() > EXPANSION_CAP {
            insert_tagged(&buf, &lines[..EXPANSION_CAP].join("\n"), "tool");
            let hidden = lines.len() - EXPANSION_CAP;
            let more = Button::with_label(&format!("{hidden} more lines ▾"));
            more.add_css_class("code-more");
            let buf_c = buf.clone();
            let full_c = full.clone();
            let capped = lines[..EXPANSION_CAP].join("\n");
            let expanded = std::cell::Cell::new(false);
            more.connect_clicked(move |btn| {
                let now = !expanded.get();
                expanded.set(now);
                buf_c.set_text("");
                insert_tagged(&buf_c, if now { &full_c } else { &capped }, "tool");
                btn.set_label(if now {
                    "show less ▴"
                } else {
                    ""
                });
                if !now {
                    btn.set_label(&format!("{hidden} more lines ▾"));
                }
            });
            expand_btn = Some(more);
        } else {
            insert_tagged(&buf, &full, "tool");
        }
        if chip.status == ChipStatus::Error {
            card.add_css_class("advisory-error");
        }
        // Reveal after one frame: the first measure can be wrong for a frame
        // (that's the jitter) — hide, force re-measure, then show the card
        // already-correct.
        card.set_visible(false);
        self.expansion.append(&card);
        if let Some(more) = expand_btn {
            self.expansion.append(&more);
        }
        self.expansion.queue_resize();
        glib::idle_add_local_once(move || card.set_visible(true));
    }
}

/// Get or create the active strip for a parent container. A new burst gets
/// a new strip; `clear_strip` on non-tool content closes the burst.
fn strip_for(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, animate: bool) -> ToolStrip {
    let mut u = ui.borrow_mut();
    if let Some(strip) = &u.current_strip {
        return strip.clone();
    }
    let strip = ToolStrip::new(parent);
    u.current_strip = Some(strip.clone());
    // Chips animate only on the live path; replay renders them settled.
    let _ = animate;
    strip
}

fn clear_strip(ui: &Rc<RefCell<Ui>>) {
    ui.borrow_mut().current_strip = None;
}

fn make_tool_card(
    ui: &Rc<RefCell<Ui>>,
    kind_class: &str,
    head_text: &str,
    meta_text: &str,
    body_tag: &str,
) -> (GtkBox, TextView, Label) {
    let card = GtkBox::new(Orientation::Vertical, 0);
    card.add_css_class("tool-card");
    card.add_css_class(kind_class);

    let header_btn = Button::new();
    header_btn.add_css_class("card-header-btn");
    let header_row = GtkBox::new(Orientation::Horizontal, 6);
    let chevron = Label::new(Some("▾"));
    chevron.add_css_class("card-chevron");
    let head = Label::new(Some(head_text));
    head.add_css_class("tool-head");
    head.set_xalign(0.0);
    let meta = Label::new(Some(meta_text));
    meta.add_css_class("tool-meta");
    meta.set_xalign(0.0);
    meta.set_hexpand(true);
    meta.set_ellipsize(pango::EllipsizeMode::End);
    header_row.append(&chevron);
    header_row.append(&head);
    header_row.append(&meta);
    header_btn.set_child(Some(&header_row));
    card.append(&header_btn);

    let body = new_view(ui);
    body.add_css_class("tool-body");
    // Mono tool output must not wrap: height-for-width with very long
    // lines (JSON args, literal \n) mis-measures into an over-tall card
    // with a blank slab below the text. wrap=None makes the height an
    // exact line count, like code blocks. Prose cards (thinking,
    // advisory) keep word wrap.
    // Thinking too — reasoning prose is riddled with long lines and code
    // fragments that mis-measure the same way. The insert path soft-wraps
    // thinking text so lines stay readable with wrap off.
    if kind_class == "tool-tool-use" || kind_class == "tool-thinking" {
        body.set_wrap_mode(gtk4::WrapMode::None);
    }
    let _ = body_tag; // caller inserts with the given tag
    card.append(&body);

    let body_c = body.clone();
    let chev_c = chevron.clone();
    let card_c = card.clone();
    let ui_c = ui.clone();
    header_btn.connect_clicked(move |_| {
        ui_c.borrow().follow.set(false); // expanding pushes down, not up
        let show = !body_c.is_visible();
        body_c.set_visible(show);
        chev_c.set_text(if show { "▾" } else { "▸" });
        if show {
            card_c.remove_css_class("collapsed");
        } else {
            card_c.add_css_class("collapsed");
        }
    });

    (card, body, chevron)
}

fn append_advisory(ui: &Rc<RefCell<Ui>>, level: &str, message: &str) {
    clear_strip(ui);
    let card = GtkBox::new(Orientation::Vertical, 4);
    card.add_css_class("advisory-card");
    card.add_css_class(if level == "error" {
        "advisory-error"
    } else {
        "advisory-info"
    });
    let l = Label::new(Some(message));
    l.set_wrap(true);
    l.set_xalign(0.0);
    l.set_selectable(true);
    card.append(&l);
    ui.borrow().live_box.append(&card);
}

// ── composer actions ─────────────────────────────────────────────────

fn composer_text(ui: &Rc<RefCell<Ui>>) -> String {
    let u = ui.borrow();
    let buf = u.composer.buffer();
    buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string()
}

fn send_prompt(ui: &Rc<RefCell<Ui>>) {
    if ui.borrow().read_only {
        return;
    }
    let text = composer_text(ui);
    if text.trim().is_empty() {
        return;
    }
    let (images, full_text) = {
        let mut u = ui.borrow_mut();
        u.composer.buffer().set_text("");
        let images = std::mem::take(&mut u.attachments);
        clear_box(&u.chips_strip);
        u.chips_strip.set_visible(false);
        let full = if images.is_empty() {
            text.clone()
        } else {
            let mut t = text.clone();
            for p in &images {
                t.push_str(&format!("\n[attached image: {}]", p.display()));
            }
            t
        };
        (images, full)
    };
    append_user_bubble(ui, &text, &images);
    let _ = ui.borrow().cmd.try_send(Cmd::Prompt(full_text));
}

fn queue_prompt(ui: &Rc<RefCell<Ui>>) {
    if ui.borrow().read_only {
        return;
    }
    let text = composer_text(ui);
    if text.trim().is_empty() {
        return;
    }
    let u = ui.borrow_mut();
    u.composer.buffer().set_text("");
    let chip = Label::new(Some(&format!("queued: {}", text.chars().take(40).collect::<String>())));
    chip.add_css_class("queue-chip");
    u.queue_strip.append(&chip);
    u.queue_strip.set_visible(true);
    let _ = u.cmd.try_send(Cmd::Queue(text));
}

fn is_image_path(p: &std::path::Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_lowercase()),
        Some(ext) if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp")
    )
}

fn add_attachment(ui: &Rc<RefCell<Ui>>, path: PathBuf) {
    let mut u = ui.borrow_mut();
    u.attachments.push(path.clone());
    let chip = GtkBox::new(Orientation::Horizontal, 0);
    chip.add_css_class("attachment-chip");
    let name = Label::new(Some(
        &path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
    ));
    name.add_css_class("chip-name");
    let remove = Button::with_label("×");
    remove.add_css_class("chip-remove");
    let ui2 = ui.clone();
    let chip_c = chip.clone();
    let path_c = path.clone();
    remove.connect_clicked(move |_| {
        let mut u = ui2.borrow_mut();
        u.attachments.retain(|p| *p != path_c);
        u.chips_strip.remove(&chip_c);
        u.chips_strip.set_visible(u.chips_strip.first_child().is_some());
    });
    chip.append(&name);
    chip.append(&remove);
    u.chips_strip.append(&chip);
    u.chips_strip.set_visible(true);
}

fn pick_attachment(ui: &Rc<RefCell<Ui>>) {
    let dialog = gtk4::FileDialog::new();
    let filter = gtk4::FileFilter::new();
    filter.add_suffix("png");
    filter.add_suffix("jpg");
    filter.add_suffix("jpeg");
    filter.add_suffix("gif");
    filter.add_suffix("webp");
    filter.set_name(Some("Images"));
    let store = gio::ListStore::new::<gtk4::FileFilter>();
    store.append(&filter);
    dialog.set_filters(Some(&store));
    dialog.set_default_filter(Some(&filter));
    let win = ui.borrow().window.clone();
    dialog.open(Some(&win), None::<&gio::Cancellable>, glib::clone!(#[strong] ui, move |res| {
        if let Ok(file) = res {
            if let Some(path) = file.path() {
                if is_image_path(&path) {
                    add_attachment(&ui, path);
                }
            }
        }
    }));
}

/// Ctrl+V image paste: true when the clipboard holds an image (handled).
fn paste_image(ui: &Rc<RefCell<Ui>>) -> bool {
    let Some(display) = gdk::Display::default() else {
        return false;
    };
    let clipboard = display.clipboard();
    let has_image = clipboard
        .formats()
        .mime_types()
        .iter()
        .any(|m| m.starts_with("image/"));
    if !has_image {
        return false;
    }
    let ui = ui.clone();
    clipboard.read_texture_async(None::<&gio::Cancellable>, move |res| {
        if let Ok(Some(texture)) = res {
            let dir = std::env::temp_dir().join("cascade-paste");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("paste-{}.png", uuid::Uuid::new_v4()));
            if texture.save_to_png(&path).is_ok() {
                add_attachment(&ui, path);
            }
        }
    });
    true
}

// ── rail rendering ───────────────────────────────────────────────────

fn meta_kind(m: &ListedSession) -> BackendKind {
    // Discovered rows with a join_handle attach through the proxy for
    // history and open the room as a prompt side-channel (dual-channel).
    // Pure terminal rows (no discovered origin) attach straight to the room.
    if m.join_handle.is_some() && m.origin.as_deref() != Some("discovered") {
        BackendKind::Terminal
    } else if m.machine == "cloud" || m.machine.is_empty() {
        BackendKind::Cloud
    } else {
        BackendKind::Local
    }
}

fn relative_time(ts: chrono::DateTime<chrono::Utc>) -> String {
    let secs = (chrono::Utc::now() - ts).num_seconds().max(0);
    if secs < 60 {
        "now".into()
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn render_rail(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    clear_box(&u.rail_list);
    let query = u.rail_search.text().to_lowercase();
    let mut filtered: Vec<&ListedSession> = u
        .metas
        .iter()
        .filter(|m| {
            if query.is_empty() {
                return true;
            }
            let title = m.name.clone().unwrap_or_default().to_lowercase();
            let model = u
                .session_models
                .get(&m.id)
                .cloned()
                .unwrap_or_default()
                .to_lowercase();
            let status = RailStatus::from_meta(m).label().to_lowercase();
            title.contains(&query)
                || m.cwd.to_lowercase().contains(&query)
                || m.machine.to_lowercase().contains(&query)
                || model.contains(&query)
                || status.contains(&query)
        })
        .collect();

    filtered.retain(|m| !is_empty_row(m));

    // The division is the organizing principle: what's alive right now vs
    // history. Idle counts as alive — a session must not jump sections every
    // time a turn pauses; only a dead process drops it to ENDED.
    let (mut live, mut ended): (Vec<&ListedSession>, Vec<&ListedSession>) = filtered
        .into_iter()
        .partition(|m| !matches!(RailStatus::from_meta(m), RailStatus::Ended));
    live.sort_by(|a, b| {
        RailStatus::from_meta(a)
            .cmp(&RailStatus::from_meta(b))
            .then_with(|| b.last_active.cmp(&a.last_active))
    });
    ended.sort_by(|a, b| b.last_active.cmp(&a.last_active));

    if !live.is_empty() {
        let h = Label::new(Some("LIVE"));
        h.add_css_class("rail-section");
        h.set_xalign(0.0);
        u.rail_list.append(&h);
        for meta in live {
            u.rail_list.append(&rail_row(ui, meta));
        }
    }
    if !ended.is_empty() {
        // History collapses to a header by default — 600 dead rows would
        // drown the LIVE group. The count stays visible; search forces
        // expansion so filtering never hides matches behind the fold.
        let collapsed = u.ended_collapsed && query.is_empty();
        let header_btn = Button::new();
        header_btn.add_css_class("card-header-btn");
        let row = GtkBox::new(Orientation::Horizontal, 4);
        let chev = Label::new(Some(if collapsed { "▸" } else { "▾" }));
        chev.add_css_class("card-chevron");
        let lbl = Label::new(Some(&format!("ENDED ({})", ended.len())));
        lbl.add_css_class("rail-section");
        lbl.set_xalign(0.0);
        row.append(&chev);
        row.append(&lbl);
        header_btn.set_child(Some(&row));
        header_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
            let mut u = ui.borrow_mut();
            u.ended_collapsed = !u.ended_collapsed;
            drop(u);
            render_rail(&ui);
        }));
        u.rail_list.append(&header_btn);
        if !collapsed {
            for meta in ended {
                u.rail_list.append(&rail_row(ui, meta));
            }
        }
    }
}

fn rail_row(ui: &Rc<RefCell<Ui>>, meta: &ListedSession) -> GtkBox {
    let u = ui.borrow();
    let row = GtkBox::new(Orientation::Vertical, 2);
    row.add_css_class("rail-item");
    if u.selected_id.as_deref() == Some(meta.id.as_str()) {
        row.add_css_class("rail-item-selected");
    }

    let top = GtkBox::new(Orientation::Horizontal, 6);
    let title = session_display_name(meta);
    let t = Label::new(Some(&title));
    t.add_css_class("rail-row-title");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let st = RailStatus::from_meta(meta);
    let status = Label::new(Some(st.label()));
    status.add_css_class("rail-status");
    status.add_css_class(st.css_class());
    let top_cb = gtk4::CenterBox::new();
    top_cb.set_start_widget(Some(&t));
    top_cb.set_end_widget(Some(&status));
    top.append(&top_cb);

    let sub = GtkBox::new(Orientation::Horizontal, 6);
    let device = if meta.machine == "cloud" || meta.machine.is_empty() {
        "cloud server".to_string()
    } else {
        u.machine_names
            .get(&meta.machine)
            .cloned()
            .unwrap_or_else(|| meta.machine.clone())
    };
    let model = u
        .session_models
        .get(&meta.id)
        .cloned()
        .unwrap_or(device);
    let sub_l = Label::new(Some(&model));
    sub_l.add_css_class("rail-row-sub");
    sub_l.set_xalign(0.0);
    sub_l.set_ellipsize(pango::EllipsizeMode::Middle);
    let time = Label::new(Some(&relative_time(meta.last_active)));
    time.add_css_class("rail-row-sub");
    let sub_cb = gtk4::CenterBox::new();
    sub_cb.set_start_widget(Some(&sub_l));
    sub_cb.set_end_widget(Some(&time));
    sub.append(&sub_cb);

    row.append(&top);
    row.append(&sub);

    if let Some(usage) = u.context_usage.get(&meta.id) {
        let bar = ProgressBar::new();
        bar.add_css_class("context-bar");
        bar.set_fraction(usage.clamp(0.0, 1.0));
        row.append(&bar);
    }

    let click = GestureClick::new();
    let id = meta.id.clone();
    let kind = meta_kind(meta);
    let join_handle = meta.join_handle.clone();
    // Discovered row with no collab room: the server drops prompts for
    // these — mark it view-only instead of letting the composer lie.
    let read_only = meta.origin.as_deref() == Some("discovered") && join_handle.is_none();
    click.connect_pressed(glib::clone!(#[strong] ui, move |_, _, _, _| {
        let _ = ui.borrow().cmd.try_send(Cmd::OpenSession {
            id: id.clone(),
            kind,
            join_handle: join_handle.clone(),
            read_only,
        });
    }));
    row.add_controller(click);
    row
}

// ── inbox ────────────────────────────────────────────────────────────

fn render_inbox(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    clear_box(&u.inbox_list);
    if u.inbox_items.is_empty() {
        let l = Label::new(Some("no unseen events"));
        l.add_css_class("inbox-empty");
        u.inbox_list.append(&l);
        return;
    }
    for item in &u.inbox_items {
        let b = Button::with_label(&item.text);
        b.add_css_class("inbox-item");
        if let Some(sid) = &item.session_id {
            let meta = u.metas.iter().find(|m| &m.id == sid).cloned();
            if let Some(meta) = meta {
                let kind = meta_kind(&meta);
                let id = meta.id.clone();
                let jh = meta.join_handle.clone();
                let read_only =
                    meta.origin.as_deref() == Some("discovered") && jh.is_none();
                b.connect_clicked(glib::clone!(#[strong] ui, move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::OpenSession {
                        id: id.clone(),
                        kind,
                        join_handle: jh.clone(),
                        read_only,
                    });
                }));
            }
        }
        u.inbox_list.append(&b);
    }
}

fn update_inbox_badge(ui: &Rc<RefCell<Ui>>, count: usize) {
    let u = ui.borrow();
    if count == 0 {
        u.inbox_btn.set_visible(false);
    } else {
        u.inbox_btn.set_label(&format!("✉ {count}"));
        u.inbox_btn.set_visible(true);
    }
}

// ── dispatch ─────────────────────────────────────────────────────────

fn dispatch(ui: &Rc<RefCell<Ui>>, msg: UiMsg) {
    match msg {
        UiMsg::NeedLogin { error } => {
            let u = ui.borrow();
            if !u.local_mode {
                u.login_overlay.set_visible(true);
            }
            if let Some(e) = error {
                u.login_error.set_text(&e);
            }
            u.status_label.set_text("connecting…");
        }
        UiMsg::LoggedIn { url } => {
            let u = ui.borrow();
            u.login_overlay.set_visible(false);
            u.login_error.set_text("");
            let host = url.trim_start_matches("https://").trim_start_matches("http://").to_string();
            u.status_label.set_text(&host);
            drop(u);
            show_toast(ui, &format!("connected {url}"));
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshSessions);
        }
        UiMsg::LoggedOut => {
            let mut u = ui.borrow_mut();
            if !u.local_mode {
                u.login_overlay.set_visible(true);
            }
            u.login_error.set_text("");
            u.selected_id = None;
            u.attached_kind = None;
            u.sharing.clear();
            u.read_only = false;
            apply_registering(&u, false);
            u.status_label.set_text("connecting…");
            u.window.set_title(Some("cascade"));
            clear_box(&u.durable_box);
            clear_box(&u.live_box);
            u.buffers.clear();
            drop(u);
            sync_composer_mode(ui);
            sync_share_buttons(ui);
        }
        UiMsg::SessionList(list) => {
            // Reconcile streaming with server truth (file freshness) — a lost
            // agent_end can never wedge Stop/Queue again.
            if let Some(sel) = ui.borrow().selected_id.clone() {
                if let Some(working) = list
                    .iter()
                    .find(|m| m.id == sel)
                    .and_then(|m| m.working)
                {
                    set_streaming(ui, working);
                    // Server says the turn is over but a stale streaming view
                    // is still open (its agent_end died in a flap) — settle
                    // it, or text parts keep getting skipped forever.
                    if !working && ui.borrow().stream.assistant.is_some() {
                        settle_live(ui);
                    }
                }
            }
            ui.borrow_mut().metas = list;
            render_rail(ui);
        }
        UiMsg::MachineNames(names) => {
            ui.borrow_mut().machine_names = names;
            render_rail(ui);
        }
        UiMsg::Attached { id, kind, snapshot, read_only } => {
            {
                let mut u = ui.borrow_mut();
                // per-session scroll memory: stash the old, restore the new
                if let Some(old) = u.selected_id.take() {
                    let adj = u.transcript_scroll.vadjustment();
                    let state = (adj.value(), u.follow.get());
                    u.scroll_mem.insert(old, state);
                }
                let (value, follow) = u.scroll_mem.get(&id).copied().unwrap_or((0.0, true));
                u.follow.set(follow);
                u.selected_id = Some(id.clone());
                u.stream = StreamState::default();
                u.seen_fingerprints.clear();
                u.attached_kind = Some(kind);
                u.read_only = read_only;
                u.share_link_reveal.set_reveal_child(false);
                let title = u
                    .metas
                    .iter()
                    .find(|m| m.id == id)
                    .map(session_display_name)
                    .unwrap_or_else(|| format!("session {}", &id[..8.min(id.len())]));
                u.status_label.set_text(&title);
                set_window_title(&u);
                let model = u.session_models.get(&id).cloned();
                match model {
                    Some(m) => {
                        u.model_pill.set_text(&m);
                        u.model_pill.set_visible(true);
                    }
                    None => u.model_pill.set_visible(false),
                }
                u.history.clear();
                u.history_oldest_rendered = 0;
                u.history_server_more = false;
                u.history_loading = false;
                u.history_has_content = false;
                u.history_status.set_visible(false);
                clear_box(&u.durable_box);
                clear_box(&u.live_box);
                clear_box(&u.question_host);
                u.buffers.clear();
                let scroll = u.transcript_scroll.clone();
                let programmatic = u.programmatic.clone();
                glib::idle_add_local_once(move || {
                    let adj = scroll.vadjustment();
                    programmatic.set(true);
                    adj.set_value(value);
                    programmatic.set(false);
                });
            }
            render_rail(ui);
            sync_composer_mode(ui);
            if let Some(snap) = snapshot {
                apply_snapshot(ui, snap);
            }
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshState);
            sync_share_buttons(ui);
        }
        UiMsg::ShareLink { session_id, url } => {
            copy_text(&url);
            show_toast(ui, &url);
            ui.borrow_mut().sharing.insert(session_id);
            sync_share_buttons(ui);
        }
        UiMsg::SharingStopped { session_id } => {
            show_toast(ui, "stopped sharing");
            ui.borrow_mut().sharing.remove(&session_id);
            sync_share_buttons(ui);
        }
        UiMsg::Event(ev) => handle_event(ui, ev),
        UiMsg::Toast(t) => show_toast(ui, &t),
        UiMsg::ReadOnly(v) => {
            ui.borrow_mut().read_only = v;
            sync_composer_mode(ui);
        }
        UiMsg::Error(e) => show_toast(ui, &e),
        UiMsg::InboxCount(n) => update_inbox_badge(ui, n),
        UiMsg::InboxItems(items) => {
            ui.borrow_mut().inbox_items = items;
        }
        UiMsg::PaneUrl(url) => {
            let mut u = ui.borrow_mut();
            u.pane_url = url.clone();
            if let Some(url) = url {
                u.url_entry.set_text(&url);
                if let Some(wv) = u.webview.as_ref() { wv.load_uri(&url); }
            }
        }
        UiMsg::SessionState(st) => {
            let model = st.model.as_ref().and_then(model_name);
            let mut u = ui.borrow_mut();
            let sel = u.selected_id.clone();
            if let (Some(id), Some(m)) = (sel, model.clone()) {
                u.session_models.insert(id, m);
            }
            match model {
                Some(m) => {
                    u.model_pill.set_text(&m);
                    u.model_pill.set_visible(true);
                }
                None => u.model_pill.set_visible(false),
            }
        }
    }
}

fn model_name(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(o) => o
            .get("id")
            .or_else(|| o.get("model_id"))
            .or_else(|| o.get("modelId"))
            .or_else(|| o.get("name"))
            .and_then(|x| x.as_str())
            .map(str::to_string),
        _ => None,
    }
}

// ── snapshot / events ────────────────────────────────────────────────

fn parse_agent_message(v: &serde_json::Value) -> Option<(String, String)> {
    let role = v
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant")
        .to_string();
    if role == "toolResult" {
    }
    Some((role, message_plain_text(v)))
}

fn message_plain_text(v: &serde_json::Value) -> String {
    match v.get("content") {
        Some(c) => flatten_content_value(c),
        None => v
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    }
}

fn flatten_content_value(c: &serde_json::Value) -> String {
    if let Some(s) = c.as_str() {
        s.to_string()
    } else if let Some(arr) = c.as_array() {
        arr.iter()
            .filter_map(part_plain_text)
            .collect::<Vec<_>>()
            .join("")
    } else if let Some(s) = c.get("text").and_then(|t| t.as_str()) {
        s.to_string()
    } else {
        String::new()
    }
}

fn part_plain_text(p: &serde_json::Value) -> Option<String> {
    p.as_str().map(|s| s.to_string()).or_else(|| {
        p.get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.to_string())
    })
}

fn content_display(c: &serde_json::Value) -> String {
    let flat = flatten_content_value(c);
    if !flat.is_empty() {
        return flat;
    }
    match c {
        serde_json::Value::Null => String::new(),
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn str_field<'a>(v: &'a serde_json::Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| v.get(*n).and_then(|x| x.as_str()))
}

fn json_pretty(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

fn thinking_text(part: &serde_json::Value) -> Option<String> {
    let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if !matches!(ty, "thinking" | "redactedThinking" | "reasoning") {
        return None;
    }
    for key in ["thinking", "text", "data", "reasoning"] {
        if let Some(s) = part.get(key).and_then(|t| t.as_str()).filter(|s| !s.is_empty()) {
            return Some(s.to_string());
        }
    }
    part.get("content")
        .map(flatten_content_value)
        .filter(|s| !s.is_empty())
}

/// Hash of role + content-part types, text, toolCall ids, and thinking text.
fn message_fingerprint(msg: &serde_json::Value) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    msg.get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant")
        .hash(&mut h);
    if let Some(id) = str_field(msg, &["toolCallId", "tool_call_id"]) {
        id.hash(&mut h);
    }
    match msg.get("content") {
        Some(serde_json::Value::Array(arr)) => {
            for part in arr {
                fingerprint_part(&mut h, part);
            }
        }
        Some(serde_json::Value::String(s)) => s.hash(&mut h),
        Some(other) => {
            if let Some(s) = other.get("text").and_then(|t| t.as_str()) {
                s.hash(&mut h);
            }
        }
        None => {
            if let Some(s) = msg.get("text").and_then(|t| t.as_str()) {
                s.hash(&mut h);
            }
        }
    }
    h.finish()
}

fn fingerprint_part(h: &mut impl std::hash::Hasher, part: &serde_json::Value) {
    use std::hash::Hash;
    let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    ty.hash(h);
    if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
        text.hash(h);
    } else if let Some(s) = part.as_str() {
        s.hash(h);
    }
    if ty == "toolCall" || ty == "tool_call" {
        if let Some(id) = str_field(part, &["id", "toolCallId"]) {
            id.hash(h);
        }
    }
    if let Some(t) = thinking_text(part) {
        t.hash(h);
    }
}

/// Record `msg` in the rolling window. True if it was already rendered.
fn message_already_rendered(ui: &Rc<RefCell<Ui>>, msg: &serde_json::Value) -> bool {
    let fp = message_fingerprint(msg);
    let mut u = ui.borrow_mut();
    if u.seen_fingerprints.contains(&fp) {
        return true;
    }
    if u.seen_fingerprints.len() >= FINGERPRINT_CAP {
        u.seen_fingerprints.pop_front();
    }
    u.seen_fingerprints.push_back(fp);
    false
}

fn fill_tool_result(
    body: &TextView,
    container: &GtkBox,
    tool_name: &str,
    text: &str,
    is_error: bool,
) {
    let buf = body.buffer();
    buf.set_text("");
    body.queue_resize();
    if is_error {
        insert_tagged(&buf, &format!("{tool_name} failed\n"), "md-bold");
        container.add_css_class("advisory-error");
    }
    insert_tagged(&buf, text, "tool");
    body.queue_resize();
}

fn append_history_tool_call(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, part: &serde_json::Value) {
    append_history_tool_call_anim(ui, parent, part, false);
}

fn append_history_tool_call_anim(
    ui: &Rc<RefCell<Ui>>,
    parent: &GtkBox,
    part: &serde_json::Value,
    animate: bool,
) {
    let name = str_field(part, &["name", "toolName"]).unwrap_or("tool");
    let id = str_field(part, &["id", "toolCallId"]).unwrap_or("");
    let intent = str_field(part, &["intent"]).unwrap_or("");
    let args_text = part
        .get("arguments")
        .or_else(|| part.get("args"))
        .map(json_pretty)
        .unwrap_or_default();
    strip_for(ui, parent, animate).add_call(ui, id, name, intent, &args_text, animate);
}

fn append_history_thinking(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, text: &str) {
    append_thinking_chip(ui, parent, text, false);
}

fn append_thinking_chip(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, text: &str, animate: bool) {
    strip_for(ui, parent, animate).add_thinking(ui, text, animate);
}

fn apply_history_tool_result(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, msg: &serde_json::Value) {
    let call_id = str_field(msg, &["toolCallId", "tool_call_id"]).unwrap_or("");
    let tool_name = str_field(msg, &["toolName", "tool_name"]).unwrap_or("tool");
    let is_error = msg
        .get("isError")
        .or_else(|| msg.get("is_error"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let body_text = msg.get("content").map(content_display).unwrap_or_default();
    if body_text.is_empty() && !is_error {
        return;
    }
    strip_for(ui, parent, false).add_result(ui, call_id, tool_name, &body_text, is_error);
}

fn render_history_part(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, part: &serde_json::Value) {
    let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "toolCall" | "tool_call" => append_history_tool_call(ui, parent, part),
        "thinking" | "redactedThinking" | "reasoning" => {
            if let Some(text) = thinking_text(part) {
                append_history_thinking(ui, parent, &text);
            }
        }
        _ => {
            if let Some(text) = part_plain_text(part).filter(|s| !s.is_empty()) {
                render_markdown_into(ui, parent, &text);
            }
        }
    }
}

fn apply_snapshot(ui: &Rc<RefCell<Ui>>, snap: SessionSnapshot) {
    let is_page = {
        let u = ui.borrow();
        u.history_loading && snap.oldest_index < u.history_oldest_rendered
    };
    if is_page {
        apply_history_page(ui, snap);
        return;
    }
    apply_snapshot_tail(ui, snap);
}

/// Fresh attach / re-snapshot: render only the newest page, pin to the
/// bottom. Older messages are buffered client-side (full snapshots from
/// local/shared attaches) or left on the server (`has_more`).
fn apply_snapshot_tail(ui: &Rc<RefCell<Ui>>, snap: SessionSnapshot) {
    const PAGE: usize = crate::worker::HISTORY_PAGE_U32 as usize;
    // A tail snapshot REPLACES history — the proxy re-sends it on every
    // re-attach, and appending stacked a full duplicate tail.
    let durable = ui.borrow().durable_box.clone();
    {
        let mut u = ui.borrow_mut();
        clear_box(&u.durable_box);
        u.current_strip = None;
        // Replacing the transcript: old fingerprints would skip every row
        // and leave an empty box.
        u.seen_fingerprints.clear();
    }
    let mut msgs = snap.messages;
    {
        let mut u = ui.borrow_mut();
        u.history.clear();
        u.history_loading = false;
        if msgs.len() > PAGE {
            let keep = msgs.split_off(msgs.len() - PAGE);
            u.history = msgs.into_iter().collect();
            msgs = keep;
        }
        let total = if snap.total_messages > 0 {
            snap.total_messages
        } else {
            u.history.len() as u64 + msgs.len() as u64
        };
        // Absolute index of the oldest message now on screen. For unpaged
        // (full) snapshots oldest_index is 0, so this is the buffer size.
        u.history_oldest_rendered = snap.oldest_index + (total - snap.oldest_index - msgs.len() as u64);
        u.history_server_more = snap.has_more;
        u.history_has_content = !msgs.is_empty() || !u.history.is_empty();
        u.follow.set(true);
        u.current_strip = None;
    }
    for msg in msgs {
        render_history_message(ui, &durable, &msg);
    }
    render_plan(ui, &snap.todos);
    if snap.streaming {
        set_streaming(ui, true);
    }
    for req in snap.pending_ui {
        show_ui_request(ui, req);
    }
    update_history_status(ui);
}

/// One message into an explicit parent — shared by tail render and prepend.
fn render_history_message(ui: &Rc<RefCell<Ui>>, parent: &GtkBox, msg: &serde_json::Value) {
    if message_already_rendered(ui, msg) {
        return;
    }
    let role = msg
        .get("role")
        .and_then(|r| r.as_str())
        .unwrap_or("assistant");
    if role == "user" || role == "human" {
        let text = message_plain_text(msg);
        if !text.is_empty() {
            user_bubble_into(ui, parent, &text, &[], true);
        }
        return;
    }
    if role == "toolResult" {
        apply_history_tool_result(ui, parent, msg);
        return;
    }
    if let Some(arr) = msg.get("content").and_then(|c| c.as_array()) {
        for part in arr {
            render_history_part(ui, parent, part);
        }
        return;
    }
    let text = message_plain_text(msg);
    if !text.is_empty() {
        render_markdown_into(ui, parent, &text);
    }
}

/// Older page from the server: prepend above the current transcript while
/// holding the viewport steady.
fn apply_history_page(ui: &Rc<RefCell<Ui>>, snap: SessionSnapshot) {
    {
        let mut u = ui.borrow_mut();
        u.history_oldest_rendered = snap.oldest_index;
        u.history_server_more = snap.has_more;
        u.history_loading = false;
    }
    prepend_messages(ui, snap.messages);
    update_history_status(ui);
}

/// Render older messages off-tree, insert them at the top of the durable
/// transcript, and restore the scroll position so the content on screen
/// does not jump.
fn prepend_messages(ui: &Rc<RefCell<Ui>>, msgs: Vec<serde_json::Value>) {
    let scroll = ui.borrow().transcript_scroll.clone();
    let adj = scroll.vadjustment();
    let old_value = adj.value();
    let old_upper = adj.upper();
    let staging = GtkBox::new(Orientation::Vertical, 12);
    for msg in &msgs {
        render_history_message(ui, &staging, msg);
    }
    let durable = ui.borrow().durable_box.clone();
    let mut anchor: Option<gtk4::Widget> = None;
    while let Some(child) = staging.first_child() {
        staging.remove(&child);
        match &anchor {
            None => durable.prepend(&child),
            Some(a) => durable.insert_child_after(&child, Some(a)),
        }
        anchor = Some(child);
    }
    let programmatic = ui.borrow().programmatic.clone();
    glib::idle_add_local_once(move || {
        let adj = scroll.vadjustment();
        let grown = adj.upper() - old_upper;
        if grown > 0.0 {
            programmatic.set(true);
            adj.set_value(old_value + grown);
            programmatic.set(false);
        }
    });
}

/// Scroll near the top: render the next page from the local buffer, or ask
/// the server for the next older page. No-op while a page is in flight.
fn try_load_history(ui: &Rc<RefCell<Ui>>) {
    enum Next {
        Buffer(Vec<serde_json::Value>),
        Server(u64),
        None,
    }
    let next = {
        let u = ui.borrow();
        if u.history_loading || u.selected_id.is_none() {
            Next::None
        } else if !u.history.is_empty() {
            const PAGE: usize = crate::worker::HISTORY_PAGE_U32 as usize;
            let n = PAGE.min(u.history.len());
            Next::Buffer(u.history.iter().skip(u.history.len() - n).cloned().collect())
        } else if u.history_server_more
            && !u.read_only
            && matches!(u.attached_kind, Some(BackendKind::Cloud))
        {
            Next::Server(u.history_oldest_rendered)
        } else {
            Next::None
        }
    };
    match next {
        Next::Buffer(page) => {
            let n = page.len() as u64;
            {
                let mut u = ui.borrow_mut();
                let keep = u.history.len() - page.len();
                u.history.truncate(keep);
                u.history_oldest_rendered = u.history_oldest_rendered.saturating_sub(n);
            }
            prepend_messages(ui, page);
            update_history_status(ui);
        }
        Next::Server(before) => {
            ui.borrow_mut().history_loading = true;
            let _ = ui.borrow().cmd.try_send(Cmd::LoadHistory { before });
            update_history_status(ui);
        }
        Next::None => {}
    }
}

fn update_history_status(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    let label = &u.history_status;
    if u.history_loading {
        label.set_text("loading earlier…");
        label.set_visible(true);
    } else if !u.history.is_empty() || u.history_server_more {
        label.set_visible(false);
    } else if u.history_has_content && u.history_oldest_rendered == 0 {
        label.set_text("start of session");
        label.set_visible(true);
    } else {
        label.set_visible(false);
    }
}

fn set_streaming(ui: &Rc<RefCell<Ui>>, on: bool) {
    {
        let mut u = ui.borrow_mut();
        u.stream.streaming = on;
        let id = u.selected_id.clone();
        if let Some(id) = id {
            if let Some(m) = u.metas.iter_mut().find(|m| m.id == id) {
                m.working = Some(on);
                if on {
                    m.live = Some(true);
                }
            }
        }
    }
    sync_composer_mode(ui);
    render_rail(ui);
}

fn sync_composer_mode(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    let ro = u.read_only;
    u.composer.set_editable(!ro);
    u.attach_btn.set_sensitive(!ro);
    u.send_btn.set_sensitive(!ro);
    u.queue_btn.set_sensitive(!ro);
    u.stop_btn.set_sensitive(!ro);
    if ro {
        u.send_btn.set_visible(false);
        u.stop_btn.set_visible(false);
        u.queue_btn.set_visible(false);
        u.composer_hint.set_text("view-only share");
    } else {
        let streaming = u.stream.streaming;
        u.send_btn.set_visible(!streaming);
        u.stop_btn.set_visible(streaming);
        u.queue_btn.set_visible(streaming);
        u.composer_hint.set_text("esc stops a running turn");
    }
}

/// Settle the live tail into the durable rows (AgentEnd).
fn settle_live(ui: &Rc<RefCell<Ui>>) {
    let mut u = ui.borrow_mut();
    while let Some(child) = u.live_box.first_child() {
        u.live_box.remove(&child);
        u.durable_box.append(&child);
    }
    u.stream.assistant = None;
    u.stream.thinking_body = None;
    u.current_strip = None;
    clear_box(&u.queue_strip);
    u.queue_strip.set_visible(false);
    // Content moved and heights changed — re-measure now instead of
    // leaving stale allocations until the next scroll.
    u.transcript_scroll.queue_resize();
}

fn handle_event(ui: &Rc<RefCell<Ui>>, ev: SessionEvent) {
    match ev {
        SessionEvent::Ready { .. } => {
            set_streaming(ui, false);
            // (Re)connect welcome: the mapper resets and re-delivers deltas
            // whole. Reset ONLY the replay-duplicating accumulators — live
            // content stays; a just-rendered message must survive the flap.
            let mut u = ui.borrow_mut();
            if let Some(tv) = u.stream.assistant.take() {
                tv.buffer().set_text("");
            }
            u.stream.thinking_body = None;
            u.stream.thinking_text.clear();
            u.stream.pending_text.clear();
            u.current_strip = None;
            u.seen_fingerprints.clear();
        }
        SessionEvent::TurnStarted => {
            set_streaming(ui, true);
            // A new turn starts a fresh strip zone: thinking and tools for
            // the turn land in a strip at the bottom, above its text.
            clear_strip(ui);
            let mut u = ui.borrow_mut();
            u.stream.assistant = None;
            u.stream.thinking_body = None;
        }
        SessionEvent::TextDelta { delta, .. } => {
            let existing = ui.borrow().stream.assistant.clone();
            let tv = match existing {
                Some(tv) => tv,
                None => {
                    let tv = new_view(ui);
                    attach_copy_menu(&tv, "Copy text");
                    // Streamed text ends the burst: tools after this text
                    // start a fresh strip BELOW it, keeping chronological
                    // order instead of one strip parked above the prose.
                    clear_strip(ui);
                    let mut u = ui.borrow_mut();
                    u.live_box.append(&tv);
                    u.stream.assistant = Some(tv.clone());
                    tv
                }
            };
            // Batch deltas (~40ms) and fade each batch in — text materializes
            // like it's being written instead of slamming in per-token.
            {
                let mut u = ui.borrow_mut();
                u.stream.pending_text.push_str(&delta);
            }
            let force = ui.borrow().stream.pending_text.len() >= 30;
            if force {
                flush_pending_text(ui, &tv);
            } else {
                schedule_text_flush(ui, tv);
            }
        }
        SessionEvent::ThinkingDelta { delta, .. } => {
            // Thinking streams into a live strip chip that updates in place —
            // same design as settled thinking, no full-width streaming card.
            let live = ui.borrow().live_box.clone();
            let text = {
                let mut u = ui.borrow_mut();
                let t = u.stream.thinking_text.clone() + &delta;
                u.stream.thinking_text = t.clone();
                t
            };
            strip_for(ui, &live, true).upsert_thinking(ui, "think-live", &text, true);
        }
        SessionEvent::MessageStart { role } => {
            if role == "assistant" || role == "model" {
                clear_strip(ui); // fresh strip zone for this message's chips
                if ui.borrow().stream.assistant.is_none() {
                    let tv = new_view(ui);
                    attach_copy_menu(&tv, "Copy text");
                    let mut u = ui.borrow_mut();
                    u.live_box.append(&tv);
                    u.stream.assistant = Some(tv);
                }
            }
        }
        SessionEvent::MessageEnd { message } => {
            if message_already_rendered(ui, &message) {
                ui.borrow_mut().stream.assistant = None;
                ui.borrow_mut().stream.thinking_body = None;
            } else {
                let role = message
                    .get("role")
                    .and_then(|r| r.as_str())
                    .unwrap_or("assistant")
                    .to_string();
                if role == "toolResult" {
                    // Discovered sessions stream tool results ONLY as MessageEnd
                    // lines — route them into the card path or they spill into
                    // the transcript as bare prose (the "Wall time" soup).
                    let live = ui.borrow().live_box.clone();
                    apply_history_tool_result(ui, &live, &message);
                } else if role == "user" || role == "human" {
                    if let Some((_, text)) = parse_agent_message(&message) {
                        if ui.borrow().stream.assistant.is_none() && !text.is_empty() {
                            let dup = {
                                let u = ui.borrow();
                                u.stream.last_user_echo.as_ref().is_some_and(|(t, when)| {
                                    *t == text && when.elapsed() < std::time::Duration::from_secs(30)
                                })
                            };
                            if dup {
                                ui.borrow_mut().stream.last_user_echo = None;
                            } else {
                                append_user_bubble_inner(ui, &text, &[], true);
                            }
                        }
                    }
                } else {
                    // Ordered part walk — same sequence as the replay path, so a
                    // ['thinking','text'] message renders thinking-then-prose live
                    // instead of prose with the thinking card parked after it.
                    // The streaming view is a PLACEHOLDER: on completion it is
                    // removed and the text parts render with real markdown —
                    // streamed raw text must never be the final render.
                    let stale_stream = ui.borrow_mut().stream.assistant.take();
                    if let Some(tv) = stale_stream {
                        let live_box = ui.borrow().live_box.clone();
                        live_box.remove(&tv);
                    }
                    ui.borrow_mut().stream.pending_text.clear();
                    let live = ui.borrow().live_box.clone();
                    let skip_text = false;
                    if let Some(arr) = message.get("content").and_then(|c| c.as_array()) {
                        for part in arr {
                            let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                            match ty {
                                "thinking" | "redactedThinking" | "reasoning" => {
                                    if let Some(text) = thinking_text(part) {
                                        // A delta-driven live chip (if any) becomes
                                        // the final chip — no second card for the
                                        // same thinking block.
                                        if ui.borrow().stream.thinking_text.is_empty() {
                                            append_thinking_chip(ui, &live, &text, true);
                                        } else {
                                            strip_for(ui, &live, true).upsert_thinking(
                                                ui,
                                                "think-live",
                                                &text,
                                                false,
                                            );
                                            ui.borrow_mut().stream.thinking_text.clear();
                                        }
                                    }
                                }
                                "toolCall" => {
                                    let id = part
                                        .get("id")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("");
                                    if !id.is_empty()
                                        && !ui
                                            .borrow()
                                            .current_strip
                                            .as_ref()
                                            .is_some_and(|s| s.has(id))
                                    {
                                        append_history_tool_call_anim(ui, &live, part, true);
                                    }
                                }
                                _ => {
                                    if !skip_text {
                                        if let Some(t) = part_plain_text(part) {
                                            if !t.is_empty() {
                                                render_markdown_into(ui, &live, &t);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if !skip_text {
                        let text = message_plain_text(&message);
                        if !text.is_empty() {
                            render_markdown_into(ui, &live, &text);
                        }
                    }
                }
                ui.borrow_mut().stream.assistant = None;
                ui.borrow_mut().stream.thinking_body = None;
            }
        }
        SessionEvent::ToolStart {
            tool_call_id,
            tool_name,
            args,
            intent,
        } => {
            let live = ui.borrow().live_box.clone();
            let args_text = serde_json::to_string_pretty(&args).unwrap_or_default();
            strip_for(ui, &live, true).add_call(
                ui,
                &tool_call_id,
                &tool_name,
                intent.as_deref().unwrap_or(""),
                &args_text,
                true,
            );
        }
        SessionEvent::ToolUpdate {
            tool_call_id,
            partial,
        } => {
            let text = serde_json::to_string_pretty(&partial).unwrap_or_default();
            if let Some(strip) = &ui.borrow().current_strip {
                strip.set_partial(&tool_call_id, &text);
            }
        }
        SessionEvent::ToolEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
        } => {
            let live = ui.borrow().live_box.clone();
            let text = serde_json::to_string_pretty(&result).unwrap_or_default();
            strip_for(ui, &live, true).add_result(
                ui,
                &tool_call_id,
                &tool_name,
                &text,
                is_error,
            );
        }
        SessionEvent::AgentEnd => {
            set_streaming(ui, false);
            settle_live(ui);
        }
        SessionEvent::TodoChanged { phases } => {
            render_plan(ui, &phases);
        }
        SessionEvent::UiRequest(req) => {
            if ui.borrow().attached_kind == Some(BackendKind::Terminal) {
                show_toast(ui, "answer on the host terminal");
            } else {
                show_ui_request(ui, req);
            }
        }
        SessionEvent::UiRequestCancelled { target_id } => {
            let pending = ui.borrow().stream.pending_ui.clone();
            if pending.as_deref() == Some(target_id.as_str()) {
                clear_box(&ui.borrow().question_host);
                ui.borrow_mut().stream.pending_ui = None;
            }
        }
        SessionEvent::Notice { level, message } => {
            if level == "info" {
                show_toast(ui, &message);
            } else {
                append_advisory(ui, &level, &message);
                if level == "error" {
                    show_toast(ui, &format!("error: {message}"));
                }
            }
        }
        SessionEvent::SessionInfo { title, session_id } => {
            let mut u = ui.borrow_mut();
            if let Some(meta) = u.metas.iter_mut().find(|m| m.id == session_id) {
                meta.name = Some(title.clone());
            }
            if u.selected_id.as_deref() == Some(session_id.as_str()) {
                u.status_label.set_text(&title);
                set_window_title(&u);
            }
            drop(u);
            render_rail(ui);
        }
        SessionEvent::StateChanged => {
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshState);
        }
        SessionEvent::ProcessExited { code } => {
            {
                let mut u = ui.borrow_mut();
                let id = u.selected_id.clone();
                if let Some(id) = id {
                    if let Some(m) = u.metas.iter_mut().find(|m| m.id == id) {
                        m.live = Some(false);
                        m.working = Some(false);
                    }
                }
            }
            set_streaming(ui, false);
            show_toast(
                ui,
                &format!(
                    "process exited{}",
                    code.map(|c| format!(" ({c})")).unwrap_or_default()
                ),
            );
        }
        SessionEvent::Snapshot(snap) => {
            apply_snapshot(ui, snap);
        }
        SessionEvent::Raw(v) => {
            tracing::debug!(raw = %v, "unmapped session event");
        }
    }
}

// ── plan slide-over ──────────────────────────────────────────────────

fn render_plan(ui: &Rc<RefCell<Ui>>, phases: &[TodoPhase]) {
    let u = ui.borrow();
    // keep the title (first child), drop the rest
    let mut child = u.plan_box.first_child();
    let mut first = true;
    while let Some(c) = child {
        let next = c.next_sibling();
        if first {
            first = false;
        } else {
            u.plan_box.remove(&c);
        }
        child = next;
    }
    for phase in phases {
        let h = Label::new(Some(&phase.name));
        h.add_css_class("plan-phase");
        h.set_xalign(0.0);
        u.plan_box.append(&h);
        for task in &phase.tasks {
            let (glyph, class) = match task.status {
                TodoStatus::Pending => ("○", "pending"),
                TodoStatus::InProgress => ("◐", "in-progress"),
                TodoStatus::Completed => ("●", "completed"),
                TodoStatus::Blocked => ("✕", "blocked"),
                TodoStatus::Abandoned => ("✕", "abandoned"),
            };
            let line = if matches!(task.status, TodoStatus::Abandoned) {
                format!("{glyph}  <s>{}</s>", glib::markup_escape_text(&task.content))
            } else {
                format!("{glyph}  {}", glib::markup_escape_text(&task.content))
            };
            let l = Label::new(None);
            l.set_markup(&line);
            l.set_xalign(0.0);
            l.set_wrap(true);
            l.add_css_class("plan-task");
            l.add_css_class(class);
            u.plan_box.append(&l);
        }
    }
    // auto open on non-empty phases, auto close when empty
    u.plan_reveal.set_reveal_child(!phases.is_empty());
}

// ── question card ────────────────────────────────────────────────────

fn clear_question(ui: &Rc<RefCell<Ui>>) {
    clear_box(&ui.borrow().question_host);
    ui.borrow_mut().stream.pending_ui = None;
}

fn show_ui_request(ui: &Rc<RefCell<Ui>>, req: UiRequest) {
    match req.method {
        UiMethod::Notify | UiMethod::SetStatus => {
            show_toast(
                ui,
                req.message
                    .as_deref()
                    .or(req.title.as_deref())
                    .unwrap_or("notice"),
            );
            let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                request_id: req.id,
                response: UiAnswer::Value(String::new()),
            });
            return;
        }
        UiMethod::SetTitle => {
            if let Some(t) = req.title.or(req.message.clone()) {
                let mut u = ui.borrow_mut();
                u.status_label.set_text(&t);
                set_window_title(&u);
            }
            let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                request_id: req.id,
                response: UiAnswer::Value(String::new()),
            });
            return;
        }
        UiMethod::SetWidget | UiMethod::Other => {
            let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                request_id: req.id,
                response: UiAnswer::Cancelled,
            });
            return;
        }
        _ => {}
    }

    let host = ui.borrow().question_host.clone();
    clear_box(&host);
    ui.borrow_mut().stream.pending_ui = Some(req.id.clone());

    let card = GtkBox::new(Orientation::Vertical, 8);
    card.add_css_class("question-card");

    let title_text = req
        .title
        .clone()
        .or(req.message.clone())
        .unwrap_or_else(|| "Question".to_string());
    let title = Label::new(Some(&title_text));
    title.add_css_class("question-title");
    title.set_xalign(0.0);
    title.set_wrap(true);
    card.append(&title);
    if req.title.is_some() {
        if let Some(m) = &req.message {
            let body = Label::new(Some(m));
            body.add_css_class("question-body");
            body.set_xalign(0.0);
            body.set_wrap(true);
            card.append(&body);
        }
    }

    // countdown line when the request carries a timeout
    if let Some(secs) = req.timeout_secs {
        let cd = Label::new(Some(&format!("{secs}s")));
        cd.add_css_class("question-timeout");
        cd.set_xalign(1.0);
        card.append(&cd);
        let remaining = Rc::new(Cell::new(secs));
        let cd_c = cd.clone();
        let ui_c = ui.clone();
        let req_id = req.id.clone();
        glib::timeout_add_local(Duration::from_secs(1), move || {
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left == 0 {
                // only auto-cancel if this request is still pending
                if ui_c.borrow().stream.pending_ui.as_deref() == Some(req_id.as_str()) {
                    let _ = ui_c.borrow().cmd.try_send(Cmd::Answer {
                        request_id: req_id.clone(),
                        response: UiAnswer::Cancelled,
                    });
                    clear_question(&ui_c);
                }
                return glib::ControlFlow::Break;
            }
            cd_c.set_text(&format!("{left}s"));
            glib::ControlFlow::Continue
        });
    }

    let id = req.id.clone();
    match req.method {
        UiMethod::Select => {
            for opt in &req.options {
                let b = Button::with_label(opt);
                b.add_css_class("question-option");
                let opt_c = opt.clone();
                let id_c = id.clone();
                b.connect_clicked(glib::clone!(#[strong] ui, move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id_c.clone(),
                        response: UiAnswer::Value(opt_c.clone()),
                    });
                    clear_question(&ui);
                }));
                card.append(&b);
            }
            // "Other" free-text row
            let other = GtkBox::new(Orientation::Horizontal, 6);
            let entry = Entry::new();
            entry.set_placeholder_text(Some("Other…"));
            entry.set_hexpand(true);
            let go = Button::with_label("➤");
            go.add_css_class("flat-btn");
            let answer_other = glib::clone!(#[strong] ui, #[strong] entry, move || {
                let text = entry.text().to_string();
                if text.trim().is_empty() {
                    return;
                }
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Value(text),
                });
                clear_question(&ui);
            });
            go.connect_clicked(glib::clone!(#[strong] answer_other, move |_| answer_other()));
            entry.connect_activate(move |_| answer_other());
            other.append(&entry);
            other.append(&go);
            card.append(&other);
        }
        UiMethod::Confirm => {
            let row = GtkBox::new(Orientation::Horizontal, 8);
            let allow = Button::with_label("Allow");
            allow.add_css_class("allow-btn");
            let deny = Button::with_label("Deny");
            deny.add_css_class("deny-btn");
            allow.connect_clicked(glib::clone!(#[strong] ui, #[strong] id, move |_| {
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Confirmed(true),
                });
                clear_question(&ui);
            }));
            deny.connect_clicked(glib::clone!(#[strong] ui, #[strong] id, move |_| {
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Confirmed(false),
                });
                clear_question(&ui);
            }));
            row.append(&allow);
            row.append(&deny);
            card.append(&row);
        }
        UiMethod::Input => {
            let entry = Entry::new();
            entry.set_placeholder_text(req.placeholder.as_deref());
            if let Some(p) = &req.prefill {
                entry.set_text(p);
            }
            let go = Button::with_label("Submit");
            go.add_css_class("allow-btn");
            go.connect_clicked(glib::clone!(#[strong] ui, #[strong] id, #[strong] entry, move |_| {
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Value(entry.text().to_string()),
                });
                clear_question(&ui);
            }));
            entry.connect_activate(glib::clone!(#[strong] ui, #[strong] id, move |e| {
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Value(e.text().to_string()),
                });
                clear_question(&ui);
            }));
            card.append(&entry);
            card.append(&go);
        }
        UiMethod::Editor => {
            let tv = TextView::new();
            tv.set_wrap_mode(gtk4::WrapMode::WordChar);
            tv.set_size_request(-1, 120);
            if let Some(p) = &req.prefill {
                tv.buffer().set_text(p);
            }
            let scroll = ScrolledWindow::new();
            scroll.set_child(Some(&tv));
            let go = Button::with_label("Submit");
            go.add_css_class("allow-btn");
            go.connect_clicked(glib::clone!(#[strong] ui, #[strong] id, move |_| {
                let buf = tv.buffer();
                let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Value(text),
                });
                clear_question(&ui);
            }));
            card.append(&scroll);
            card.append(&go);
        }
        UiMethod::OpenUrl => {
            let url = req.url.clone().unwrap_or_default();
            let url_label = Label::new(Some(&url));
            url_label.add_css_class("question-body");
            url_label.set_xalign(0.0);
            url_label.set_selectable(true);
            url_label.set_ellipsize(pango::EllipsizeMode::Middle);
            card.append(&url_label);
            let row = GtkBox::new(Orientation::Horizontal, 8);
            let open = Button::with_label("Open");
            open.add_css_class("allow-btn");
            let win = ui.borrow().window.clone();
            let url_o = url.clone();
            open.connect_clicked(move |_| {
                gtk4::UriLauncher::new(&url_o)
                    .launch(Some(&win), None::<&gio::Cancellable>, |_| {});
            });
            let done = Button::with_label("Done");
            done.add_css_class("flat-btn");
            done.connect_clicked(glib::clone!(#[strong] ui, #[strong] id, move |_| {
                let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                    request_id: id.clone(),
                    response: UiAnswer::Value(url.clone()),
                });
                clear_question(&ui);
            }));
            row.append(&open);
            row.append(&done);
            card.append(&row);
        }
        _ => {}
    }
    host.append(&card);
}

// ── new session dialog ───────────────────────────────────────────────

fn show_new_session_dialog(ui: &Rc<RefCell<Ui>>) {
    let win = ui.borrow().window.clone();
    let dlg = Window::builder()
        .transient_for(&win)
        .modal(true)
        .title("New session")
        .default_width(420)
        .build();

    let col = GtkBox::new(Orientation::Vertical, 10);
    col.set_margin_start(20);
    col.set_margin_end(20);
    col.set_margin_top(20);
    col.set_margin_bottom(20);

    let title = Label::new(Some("New session"));
    title.add_css_class("login-subtle");
    title.set_xalign(0.0);

    let local = CheckButton::with_label("Local");
    let cloud = CheckButton::with_label("Cloud (wickrunner)");
    cloud.set_group(Some(&local));
    if ui.borrow().settings.last_backend == "local" {
        local.set_active(true);
    } else {
        cloud.set_active(true);
    }

    let cwd = Entry::new();
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    cwd.set_text(&home);
    cwd.set_placeholder_text(Some("working directory"));
    let model = Entry::new();
    model.set_placeholder_text(Some("model (optional)"));

    let go = Button::with_label("Create");
    go.add_css_class("login-button");
    col.append(&title);
    col.append(&local);
    col.append(&cloud);
    col.append(&cwd);
    col.append(&model);
    col.append(&go);
    go.connect_clicked(glib::clone!(#[strong] ui, #[strong] dlg, move |_| {
        let kind = if local.is_active() {
            BackendKind::Local
        } else {
            BackendKind::Cloud
        };
        let model_s = model.text().to_string();
        let model = if model_s.trim().is_empty() { None } else { Some(model_s) };
        let _ = ui.borrow().cmd.try_send(Cmd::NewSession {
            kind,
            cwd: cwd.text().to_string(),
            model,
        });
        dlg.close();
    }));

    dlg.set_child(Some(&col));
    dlg.present();
}

// ── misc ─────────────────────────────────────────────────────────────

fn show_toast(ui: &Rc<RefCell<Ui>>, text: &str) {
    let u = ui.borrow();
    u.toast.set_text(text);
    u.toast_reveal.set_reveal_child(true);
    let reveal = u.toast_reveal.clone();
    glib::timeout_add_local_once(Duration::from_secs(3), move || {
        reveal.set_reveal_child(false);
    });
}

fn clear_box(widget: &GtkBox) {
    while let Some(c) = widget.first_child() {
        widget.remove(&c);
    }
}
