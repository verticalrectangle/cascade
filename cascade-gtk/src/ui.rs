//! cascade-gtk UI — Rust port of the omperator linux GTK app
//! (AppWindow/TranscriptWidgets/PanesFactory). Layout: rail (left, drag
//! resize), slim topbar, full-width transcript with durable + live-tail
//! boxes, plan slide-over, composer with attachments, question card, inbox,
//! browser pane (right), first-run login overlay.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use cascade_core::{
    SessionEvent, SessionMeta, SessionSnapshot, TodoPhase, TodoStatus, UiAnswer, UiMethod,
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
    TextView, ToggleButton, Window,
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
    underline: bool,
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
    push("md-h1", TagSpec { fg: Some(p.gold), weight: 700, size_pt: 17.0, ..Default::default() });
    push("md-h2", TagSpec { fg: Some(p.gold), weight: 700, size_pt: 15.0, ..Default::default() });
    push("md-h3", TagSpec { fg: Some(p.text), weight: 600, size_pt: 13.0, ..Default::default() });
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
    push("md-link", TagSpec { fg: Some(p.iris), underline: true, ..Default::default() });
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
    }
    tag.set_underline(if spec.underline {
        pango::Underline::Single
    } else {
        pango::Underline::None
    });
}

/// Create (or restyle) all transcript tags on a buffer for `theme`.
fn style_buffer(buf: &TextBuffer, theme: &str) {
    let table = buf.tag_table();
    for (name, spec) in tag_specs(theme) {
        apply_tag(&table, name, &spec);
    }
}

// ── state ────────────────────────────────────────────────────────────

struct ToolCard {
    body: TextView,
    container: GtkBox,
}

#[derive(Default)]
struct StreamState {
    assistant: Option<TextView>,
    thinking_body: Option<TextView>,
    tools: HashMap<String, ToolCard>,
    pending_ui: Option<String>,
    streaming: bool,
    /// Text of the last optimistically-rendered user bubble + when, used to
    /// suppress the duplicate from the MessageEnd echo of the same message.
    last_user_echo: Option<(String, std::time::Instant)>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Grouping {
    Recent,
    Project,
}

pub struct Ui {
    window: ApplicationWindow,
    toast: Label,
    toast_reveal: Revealer,
    login_overlay: GtkBox,
    login_error: Label,
    email: Entry,
    password: PasswordEntry,
    status_label: Label,
    model_pill: Label,
    inbox_btn: MenuButton,
    inbox_list: GtkBox,
    rail_revealer: Revealer,
    rail_wrap: GtkBox,
    rail_search: Entry,
    seg_recent: ToggleButton,
    seg_project: ToggleButton,
    rail_list: GtkBox,
    transcript_scroll: ScrolledWindow,
    durable_box: GtkBox,
    live_box: GtkBox,
    follow: Cell<bool>,
    programmatic: Cell<bool>,
    plan_reveal: Revealer,
    plan_box: GtkBox,
    question_host: GtkBox,
    composer: TextView,
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
    selected_id: Option<String>,
    attached_kind: Option<BackendKind>,
    metas: Vec<SessionMeta>,
    settings: Settings,
    grouping: Grouping,
    collapsed: HashSet<String>,
    session_models: HashMap<String, String>,
    context_usage: HashMap<String, f64>,
    scroll_mem: HashMap<String, (f64, bool)>,
    buffers: Vec<TextBuffer>,
    inbox_items: Vec<InboxItem>,
    pane_visible: bool,
    local_mode: bool,
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
    let new_btn = Button::with_label("+");
    new_btn.add_css_class("flat-btn");
    new_btn.set_tooltip_text(Some("New session (Ctrl+N)"));
    let rail_theme_btn = Button::with_label("◐");
    rail_theme_btn.add_css_class("flat-btn");
    rail_theme_btn.set_tooltip_text(Some("Toggle theme"));
    let rail_btns = GtkBox::new(Orientation::Horizontal, 0);
    rail_btns.append(&new_btn);
    rail_btns.append(&rail_theme_btn);
    rail_head.set_start_widget(Some(&rail_title));
    rail_head.set_end_widget(Some(&rail_btns));

    let rail_search = Entry::new();
    rail_search.add_css_class("rail-search");
    rail_search.set_placeholder_text(Some("Filter…"));

    let segmented = GtkBox::new(Orientation::Horizontal, 0);
    segmented.add_css_class("rail-segmented");
    segmented.set_homogeneous(true);
    let seg_recent = ToggleButton::with_label("Recent");
    seg_recent.add_css_class("rail-segment");
    seg_recent.add_css_class("rail-segment-active");
    seg_recent.set_active(true);
    let seg_project = ToggleButton::with_label("Project");
    seg_project.add_css_class("rail-segment");
    seg_project.set_group(Some(&seg_recent));
    segmented.append(&seg_recent);
    segmented.append(&seg_project);

    let rail_list = GtkBox::new(Orientation::Vertical, 0);
    rail_list.set_vexpand(true);
    let rail_scroll = ScrolledWindow::new();
    rail_scroll.set_child(Some(&rail_list));
    rail_scroll.set_vexpand(true);
    rail_scroll.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);

    rail_wrap.append(&rail_head);
    rail_wrap.append(&rail_search);
    rail_wrap.append(&segmented);
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
    let login_btn = Button::with_label("Sign in");
    login_btn.add_css_class("login-button");
    let login_error = Label::new(None);
    login_error.add_css_class("login-error");
    login_error.set_wrap(true);
    let local_link = Button::with_label("Use locally without account");
    local_link.add_css_class("login-link");

    login_card.append(&login_title);
    login_card.append(&login_sub);
    login_card.append(&email);
    login_card.append(&password);
    login_card.append(&login_btn);
    login_card.append(&login_error);
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
    sp_col.append(&dark_check);
    sp_col.append(&sidebar_check);
    sp_col.append(&sp_sep);
    sp_col.append(&url_row);
    sp_col.append(&save_url_btn);
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
        status_label,
        model_pill,
        inbox_btn: inbox_btn.clone(),
        inbox_list,
        rail_revealer,
        rail_wrap,
        rail_search,
        seg_recent: seg_recent.clone(),
        seg_project: seg_project.clone(),
        rail_list,
        transcript_scroll,
        durable_box,
        live_box,
        follow: Cell::new(true),
        programmatic: Cell::new(false),
        plan_reveal,
        plan_box,
        question_host,
        composer,
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
        selected_id: None,
        attached_kind: None,
        metas: Vec::new(),
        settings,
        grouping: Grouping::Recent,
        collapsed: HashSet::new(),
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
    local_link.connect_clicked(glib::clone!(#[strong] ui, move |_| {
        let mut u = ui.borrow_mut();
        u.local_mode = true;
        u.settings.local_mode = true;
        let _ = u.settings.save();
        u.login_overlay.set_visible(false);
        drop(u);
        let _ = ui.borrow().cmd.try_send(Cmd::RefreshSessions);
    }));

    // rail search + grouping
    {
        let u = ui.borrow();
        u.rail_search.connect_changed(glib::clone!(#[strong] ui, move |_| render_rail(&ui)));
    }
    seg_recent.connect_toggled(glib::clone!(#[strong] ui, move |b| {
        if b.is_active() {
            ui.borrow_mut().grouping = Grouping::Recent;
            sync_segment_css(&ui);
            render_rail(&ui);
        }
    }));
    seg_project.connect_toggled(glib::clone!(#[strong] ui, move |b| {
        if b.is_active() {
            ui.borrow_mut().grouping = Grouping::Project;
            sync_segment_css(&ui);
            render_rail(&ui);
        }
    }));

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
            let u = ui.borrow();
            if !u.programmatic.get() {
                let at_bottom = adj.value() >= adj.upper() - adj.page_size() - FOLLOW_MARGIN;
                u.follow.set(at_bottom);
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
    let _ = u.cmd.try_send(Cmd::Login { email, password });
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

fn sync_segment_css(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    for (btn, mode) in [(&u.seg_recent, Grouping::Recent), (&u.seg_project, Grouping::Project)] {
        if u.grouping == mode {
            btn.add_css_class("rail-segment-active");
        } else {
            btn.remove_css_class("rail-segment-active");
        }
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
    tv
}

fn insert_tagged(buf: &TextBuffer, text: &str, tag: &str) {
    let mut end = buf.end_iter();
    buf.insert_with_tags_by_name(&mut end, text, &[tag]);
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
    for block in markdown::parse_blocks(body) {
        match block {
            Block::Prose(segs) => {
                let tv = new_view(ui);
                let buf = tv.buffer();
                for (text, tag) in segs {
                    insert_tagged(&buf, &text, tag);
                }
                attach_copy_menu(&tv, "Copy text");
                parent.append(&tv);
            }
            Block::Code { lang, code } => {
                parent.append(&code_block_widget(ui, &lang, &code));
            }
        }
    }
}

/// Code block: header (lang + copy) + highlighted body + hover ghost copy.
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
    let copy_btn = Button::with_label("copy");
    copy_btn.add_css_class("code-copy");
    let code_owned = code.to_string();
    copy_btn.connect_clicked(move |_| copy_text(&code_owned));
    header.append(&lang_label);
    header.append(&copy_btn);
    card.append(&header);

    let tv = new_view(ui);
    tv.set_wrap_mode(gtk4::WrapMode::None);
    let buf = tv.buffer();
    for (text, tag) in highlight::highlight(code, lang) {
        insert_tagged(&buf, &text, tag);
    }
    attach_copy_menu(&tv, "Copy code");

    // hover ghost copy button (top-right over the body)
    let body_overlay = Overlay::new();
    body_overlay.set_child(Some(&tv));
    let ghost = Button::with_label("copy");
    ghost.add_css_class("copy-ghost");
    ghost.set_halign(gtk4::Align::End);
    ghost.set_valign(gtk4::Align::Start);
    ghost.set_visible(false);
    let code_owned2 = code.to_string();
    ghost.connect_clicked(move |_| copy_text(&code_owned2));
    body_overlay.add_overlay(&ghost);
    let motion = EventControllerMotion::new();
    let ghost_in = ghost.clone();
    motion.connect_enter(move |_, _, _| ghost_in.set_visible(true));
    let ghost_out = ghost.clone();
    motion.connect_leave(move |_| ghost_out.set_visible(false));
    card.add_controller(motion);
    card.append(&body_overlay);
    card
}

/// Right-aligned user bubble card.
fn append_user_bubble(ui: &Rc<RefCell<Ui>>, text: &str, images: &[PathBuf]) {
    append_user_bubble_inner(ui, text, images, false)
}

/// Optimistic render at send time; MessageEnd(user) echoes the same message
/// back — skip the duplicate.
fn append_user_bubble_inner(ui: &Rc<RefCell<Ui>>, text: &str, images: &[PathBuf], from_echo: bool) {
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

    let target = if ui.borrow().stream.streaming {
        ui.borrow().live_box.clone()
    } else {
        ui.borrow().durable_box.clone()
    };
    target.append(&bubble);
}

fn show_lightbox(ui: &Rc<RefCell<Ui>>, texture: &gdk::Texture) {
    let u = ui.borrow();
    u.lightbox_pic.set_paintable(Some(texture));
    u.lightbox.set_can_target(true);
    u.lightbox.set_reveal_child(true);
}

/// Collapsible tool/thinking card.
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
    let _ = body_tag; // caller inserts with the given tag
    card.append(&body);

    let body_c = body.clone();
    let chev_c = chevron.clone();
    let card_c = card.clone();
    header_btn.connect_clicked(move |_| {
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

fn meta_kind(m: &SessionMeta) -> BackendKind {
    if m.kind == "terminal" {
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

fn session_status(u: &Ui, id: &str) -> &'static str {
    if u.selected_id.as_deref() == Some(id) {
        if u.stream.streaming {
            "ACTIVE"
        } else {
            "IDLE"
        }
    } else {
        "CLOSED"
    }
}

fn render_rail(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    clear_box(&u.rail_list);
    let query = u.rail_search.text().to_lowercase();
    let filtered: Vec<&SessionMeta> = u
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
            let status = session_status(&u, &m.id).to_lowercase();
            title.contains(&query)
                || m.cwd.to_lowercase().contains(&query)
                || m.machine.to_lowercase().contains(&query)
                || model.contains(&query)
                || status.contains(&query)
        })
        .collect();

    let sections: Vec<(String, Vec<&SessionMeta>)> = match u.grouping {
        Grouping::Recent => {
            let (running, saved): (Vec<_>, Vec<_>) = filtered
                .into_iter()
                .partition(|m| u.selected_id.as_deref() == Some(m.id.as_str()));
            let mut v = Vec::new();
            if !running.is_empty() {
                v.push(("Running".to_string(), running));
            }
            if !saved.is_empty() {
                v.push(("Saved".to_string(), saved));
            }
            v
        }
        Grouping::Project => {
            let mut order: Vec<String> = Vec::new();
            let mut map: HashMap<String, Vec<&SessionMeta>> = HashMap::new();
            for m in filtered {
                let key = m.cwd.clone();
                if !map.contains_key(&key) {
                    order.push(key.clone());
                }
                map.entry(key).or_default().push(m);
            }
            order
                .into_iter()
                .map(|k| (k.clone(), map.remove(&k).unwrap()))
                .collect()
        }
    };

    for (name, items) in sections {
        let collapsed = u.collapsed.contains(&name);
        let header_btn = Button::new();
        header_btn.add_css_class("card-header-btn");
        let row = GtkBox::new(Orientation::Horizontal, 4);
        let chev = Label::new(Some(if collapsed { "▸" } else { "▾" }));
        chev.add_css_class("card-chevron");
        let lbl = Label::new(Some(&name.to_uppercase()));
        lbl.add_css_class("rail-section");
        lbl.set_xalign(0.0);
        row.append(&chev);
        row.append(&lbl);
        header_btn.set_child(Some(&row));
        let name_c = name.clone();
        header_btn.connect_clicked(glib::clone!(#[strong] ui, move |_| {
            let mut u = ui.borrow_mut();
            if !u.collapsed.remove(&name_c) {
                u.collapsed.insert(name_c.clone());
            }
            drop(u);
            render_rail(&ui);
        }));
        u.rail_list.append(&header_btn);
        if collapsed {
            continue;
        }
        for meta in items {
            u.rail_list.append(&rail_row(ui, meta));
        }
    }
}

fn rail_row(ui: &Rc<RefCell<Ui>>, meta: &SessionMeta) -> GtkBox {
    let u = ui.borrow();
    let row = GtkBox::new(Orientation::Vertical, 2);
    row.add_css_class("rail-item");
    if u.selected_id.as_deref() == Some(meta.id.as_str()) {
        row.add_css_class("rail-item-selected");
    }

    let top = GtkBox::new(Orientation::Horizontal, 6);
    let title = meta
        .name
        .clone()
        .unwrap_or_else(|| {
            meta.cwd
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or(&meta.cwd)
                .to_string()
        });
    let t = Label::new(Some(&title));
    t.add_css_class("rail-row-title");
    t.set_xalign(0.0);
    t.set_ellipsize(pango::EllipsizeMode::End);
    let status = Label::new(Some(session_status(&u, &meta.id)));
    status.add_css_class("rail-status");
    if u.selected_id.as_deref() == Some(meta.id.as_str()) && u.stream.streaming {
        status.add_css_class("active");
    }
    let top_cb = gtk4::CenterBox::new();
    top_cb.set_start_widget(Some(&t));
    top_cb.set_end_widget(Some(&status));
    top.append(&top_cb);

    let sub = GtkBox::new(Orientation::Horizontal, 6);
    let model = u
        .session_models
        .get(&meta.id)
        .cloned()
        .unwrap_or_else(|| meta.machine.clone());
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
    click.connect_pressed(glib::clone!(#[strong] ui, move |_, _, _, _| {
        let _ = ui.borrow().cmd.try_send(Cmd::OpenSession {
            id: id.clone(),
            kind,
            join_handle: join_handle.clone(),
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
                b.connect_clicked(glib::clone!(#[strong] ui, move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::OpenSession {
                        id: id.clone(),
                        kind,
                        join_handle: jh.clone(),
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
            u.status_label.set_text("connecting…");
            clear_box(&u.durable_box);
            clear_box(&u.live_box);
            u.buffers.clear();
        }
        UiMsg::SessionList(list) => {
            ui.borrow_mut().metas = list;
            render_rail(ui);
        }
        UiMsg::Attached { id, kind, snapshot } => {
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
                u.attached_kind = Some(kind);
                let title = u
                    .metas
                    .iter()
                    .find(|m| m.id == id)
                    .and_then(|m| m.name.clone())
                    .unwrap_or_else(|| format!("session {}", &id[..8.min(id.len())]));
                u.status_label.set_text(&title);
                u.window.set_title(Some(&format!("cascade — {title}")));
                let model = u.session_models.get(&id).cloned();
                match model {
                    Some(m) => {
                        u.model_pill.set_text(&m);
                        u.model_pill.set_visible(true);
                    }
                    None => u.model_pill.set_visible(false),
                }
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
            if let Some(snap) = snapshot {
                apply_snapshot(ui, snap);
            }
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshState);
        }
        UiMsg::Event(ev) => handle_event(ui, ev),
        UiMsg::Toast(t) => show_toast(ui, &t),
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
    let text = if let Some(s) = v.get("content").and_then(|c| c.as_str()) {
        s.to_string()
    } else if let Some(arr) = v.get("content").and_then(|c| c.as_array()) {
        arr.iter()
            .filter_map(|p| {
                p.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| p.get("text").and_then(|t| t.as_str()).map(|s| s.to_string()))
            })
            .collect::<Vec<_>>()
            .join("")
    } else if let Some(s) = v.get("text").and_then(|t| t.as_str()) {
        s.to_string()
    } else {
        String::new()
    };
    Some((role, text))
}

fn apply_snapshot(ui: &Rc<RefCell<Ui>>, snap: SessionSnapshot) {
    let durable = ui.borrow().durable_box.clone();
    for msg in snap.messages {
        if let Some((role, text)) = parse_agent_message(&msg) {
            if text.is_empty() {
                continue;
            }
            if role == "user" || role == "human" {
                append_user_bubble(ui, &text, &[]);
            } else {
                render_markdown_into(ui, &durable, &text);
            }
        }
    }
    render_plan(ui, &snap.todos);
    if snap.streaming {
        set_streaming(ui, true);
    }
    for req in snap.pending_ui {
        show_ui_request(ui, req);
    }
}

fn set_streaming(ui: &Rc<RefCell<Ui>>, on: bool) {
    let mut u = ui.borrow_mut();
    u.stream.streaming = on;
    u.send_btn.set_visible(!on);
    u.stop_btn.set_visible(on);
    u.queue_btn.set_visible(on);
    drop(u);
    render_rail(ui); // ACTIVE/IDLE status refresh
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
    u.stream.tools.clear();
    clear_box(&u.queue_strip);
    u.queue_strip.set_visible(false);
}

fn handle_event(ui: &Rc<RefCell<Ui>>, ev: SessionEvent) {
    match ev {
        SessionEvent::Ready { .. } => {}
        SessionEvent::TurnStarted => {
            set_streaming(ui, true);
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
                    let mut u = ui.borrow_mut();
                    u.live_box.append(&tv);
                    u.stream.assistant = Some(tv.clone());
                    tv
                }
            };
            let buf = tv.buffer();
            insert_tagged(&buf, &delta, "assistant");
        }
        SessionEvent::ThinkingDelta { delta, .. } => {
            let existing_body = ui.borrow().stream.thinking_body.clone();
            let body = match existing_body {
                Some(tv) => tv,
                None => {
                    let (card, body, _chev) =
                        make_tool_card(ui, "tool-thinking", "THINKING", "", "thinking");
                    let mut u = ui.borrow_mut();
                    // insert right before the assistant text if it already
                    // exists (thinking belongs above the reply); else append.
                    if let Some(tv) = u.stream.assistant.as_ref() {
                        let before = tv.prev_sibling();
                        u.live_box.insert_child_after(&card, before.as_ref());
                    } else {
                        u.live_box.append(&card);
                    }
                    u.stream.thinking_body = Some(body.clone());
                    body
                }
            };
            let buf = body.buffer();
            insert_tagged(&buf, &delta, "thinking");
        }
        SessionEvent::MessageStart { role } => {
            if role == "assistant" || role == "model" {
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
            if let Some((role, text)) = parse_agent_message(&message) {
                if ui.borrow().stream.assistant.is_none() && !text.is_empty() {
                    if role == "user" || role == "human" {
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
                    } else {
                        let live = ui.borrow().live_box.clone();
                        render_markdown_into(ui, &live, &text);
                    }
                }
            }
            ui.borrow_mut().stream.assistant = None;
            ui.borrow_mut().stream.thinking_body = None;
        }
        SessionEvent::ToolStart {
            tool_call_id,
            tool_name,
            args,
            intent,
        } => {
            let meta = intent.as_deref().map(|i| format!("· {i}")).unwrap_or_default();
            let (card, body, _chevron) =
                make_tool_card(ui, "tool-tool-use", &tool_name.to_uppercase(), &meta, "tool");
            {
                let buf = body.buffer();
                insert_tagged(
                    &buf,
                    &serde_json::to_string_pretty(&args).unwrap_or_default(),
                    "tool",
                );
            }
            ui.borrow().live_box.append(&card);
            ui.borrow_mut().stream.tools.insert(
                tool_call_id,
                ToolCard {
                    body,
                    container: card,
                },
            );
        }
        SessionEvent::ToolUpdate {
            tool_call_id,
            partial,
        } => {
            if let Some(card) = ui.borrow().stream.tools.get(&tool_call_id) {
                let buf = card.body.buffer();
                buf.set_text("");
                insert_tagged(
                    &buf,
                    &serde_json::to_string_pretty(&partial).unwrap_or_default(),
                    "tool",
                );
            }
        }
        SessionEvent::ToolEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
        } => {
            if let Some(card) = ui.borrow().stream.tools.get(&tool_call_id) {
                let buf = card.body.buffer();
                buf.set_text("");
                if is_error {
                    insert_tagged(&buf, &format!("{tool_name} failed\n"), "md-bold");
                    card.container.add_css_class("advisory-error");
                }
                insert_tagged(
                    &buf,
                    &serde_json::to_string_pretty(&result).unwrap_or_default(),
                    "tool",
                );
            }
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
            append_advisory(ui, &level, &message);
            if level == "error" {
                show_toast(ui, &format!("error: {message}"));
            }
        }
        SessionEvent::SessionInfo { title, session_id } => {
            let mut u = ui.borrow_mut();
            if u.selected_id.as_deref() == Some(session_id.as_str()) {
                u.status_label.set_text(&title);
                u.window.set_title(Some(&format!("cascade — {title}")));
            }
            if let Some(meta) = u.metas.iter_mut().find(|m| m.id == session_id) {
                meta.name = Some(title);
            }
            drop(u);
            render_rail(ui);
        }
        SessionEvent::StateChanged => {
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshState);
        }
        SessionEvent::ProcessExited { code } => {
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
                let u = ui.borrow_mut();
                u.status_label.set_text(&t);
                u.window.set_title(Some(&format!("cascade — {t}")));
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
