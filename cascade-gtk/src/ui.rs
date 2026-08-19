use std::cell::RefCell;
use std::collections::HashMap;
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
    Application, ApplicationWindow, Box as GtkBox, Button, Entry, EventControllerKey, Expander,
    Label, ListBox, ListBoxRow, Orientation, Overlay, PasswordEntry, Revealer,
    RevealerTransitionType, ScrolledWindow, TextView, ToggleButton, Window,
};

use crate::settings::Settings;
use crate::worker::{BackendKind, Cmd, UiMsg};

struct StreamState {
    assistant: Option<Label>,
    thinking: Option<Label>,
    tools: HashMap<String, Label>,
    pending_ui: Option<String>,
}

pub struct Ui {
    window: ApplicationWindow,
    overlay: Overlay,
    stack_login: GtkBox,
    stack_main: GtkBox,
    login_error: Label,
    email: Entry,
    password: PasswordEntry,
    sessions_list: ListBox,
    sidebar: Revealer,
    plan_reveal: Revealer,
    plan_box: GtkBox,
    transcript: GtkBox,
    transcript_scroll: ScrolledWindow,
    composer: TextView,
    send_btn: Button,
    abort_btn: Button,
    question_host: GtkBox,
    toast: Label,
    toast_reveal: Revealer,
    machine: Label,
    sessions_pill: ToggleButton,
    plan_pill: ToggleButton,
    wordmark: Label,
    cmd: async_channel::Sender<Cmd>,
    stream: StreamState,
    selected_id: Option<String>,
    attached_kind: Option<BackendKind>,
    metas: Vec<SessionMeta>,
}

pub fn build(app: &Application, cmd: async_channel::Sender<Cmd>, ui_rx: async_channel::Receiver<UiMsg>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("cascade")
        .default_width(1200)
        .default_height(800)
        .build();
    window.add_css_class("cascade-window");

    let overlay = Overlay::new();
    let login = build_login();
    let main = build_main();

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.append(&login.root);
    root.append(&main.root);
    overlay.set_child(Some(&root));

    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(gtk4::Align::Center);
    toast.set_valign(gtk4::Align::Start);
    let toast_reveal = Revealer::new();
    toast_reveal.set_transition_type(RevealerTransitionType::SlideDown);
    toast_reveal.set_child(Some(&toast));
    overlay.add_overlay(&toast_reveal);

    window.set_child(Some(&overlay));

    let ui = Rc::new(RefCell::new(Ui {
        window: window.clone(),
        overlay,
        stack_login: login.root.clone(),
        stack_main: main.root.clone(),
        login_error: login.error,
        email: login.email.clone(),
        password: login.password.clone(),
        sessions_list: main.sessions_list.clone(),
        sidebar: main.sidebar,
        plan_reveal: main.plan_reveal,
        plan_box: main.plan_box,
        transcript: main.transcript,
        transcript_scroll: main.transcript_scroll,
        composer: main.composer.clone(),
        send_btn: main.send_btn.clone(),
        abort_btn: main.abort_btn.clone(),
        question_host: main.question_host,
        toast,
        toast_reveal,
        machine: main.machine,
        sessions_pill: main.sessions_pill.clone(),
        plan_pill: main.plan_pill.clone(),
        wordmark: main.wordmark,
        cmd: cmd.clone(),
        stream: StreamState {
            assistant: None,
            thinking: None,
            tools: HashMap::new(),
            pending_ui: None,
        },
        selected_id: None,
        attached_kind: None,
        metas: Vec::new(),
    }));

    ui.borrow().stack_main.set_visible(false);
    ui.borrow().abort_btn.set_visible(false);

    login.login_btn.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| {
            let u = ui.borrow();
            let email = u.email.text().to_string();
            let password = u.password.text().to_string();
            let _ = u.cmd.try_send(Cmd::Login { email, password });
        }
    ));

    main.sessions_pill.connect_toggled(glib::clone!(
        #[strong]
        ui,
        move |btn| {
            ui.borrow().sidebar.set_reveal_child(btn.is_active());
        }
    ));
    main.plan_pill.connect_toggled(glib::clone!(
        #[strong]
        ui,
        move |btn| {
            ui.borrow().plan_reveal.set_reveal_child(btn.is_active());
        }
    ));
    main.sidebar_toggle.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| {
            let u = ui.borrow();
            let next = !u.sidebar.reveals_child();
            u.sessions_pill.set_active(next);
            u.sidebar.set_reveal_child(next);
        }
    ));
    main.new_pill.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| show_new_session_dialog(&ui)
    ));
    main.settings_pill.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| show_settings_dialog(&ui)
    ));

    main.send_btn.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| send_prompt(&ui)
    ));
    main.abort_btn.connect_clicked(glib::clone!(
        #[strong]
        ui,
        move |_| {
            let _ = ui.borrow().cmd.try_send(Cmd::Abort);
        }
    ));

    let keys = EventControllerKey::new();
    keys.connect_key_pressed(glib::clone!(
        #[strong]
        ui,
        move |_, key, _, mods| {
            if key == gdk::Key::Return || key == gdk::Key::KP_Enter {
                if mods.contains(gdk::ModifierType::SHIFT_MASK) {
                    return glib::Propagation::Proceed;
                }
                send_prompt(&ui);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    main.composer.add_controller(keys);

    main.sessions_list.connect_row_activated(glib::clone!(
        #[strong]
        ui,
        move |_, row| {
            let id = row.widget_name().to_string();
            let kind = if row.has_css_class("kind-terminal") {
                BackendKind::Terminal
            } else if row.has_css_class("kind-cloud") {
                BackendKind::Cloud
            } else {
                BackendKind::Local
            };
            let join_handle = ui
                .borrow()
                .metas
                .iter()
                .find(|m| m.id == id)
                .and_then(|m| m.join_handle.clone());
            let _ = ui.borrow().cmd.try_send(Cmd::OpenSession {
                id,
                kind,
                join_handle,
            });
        }
    ));

    glib::timeout_add_local(Duration::from_millis(16), move || {
        while let Ok(msg) = ui_rx.try_recv() {
            dispatch(&ui, msg);
        }
        glib::ControlFlow::Continue
    });

    window.present();
}

struct LoginBits {
    root: GtkBox,
    email: Entry,
    password: PasswordEntry,
    login_btn: Button,
    error: Label,
}

fn build_login() -> LoginBits {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_halign(gtk4::Align::Center);
    root.set_valign(gtk4::Align::Center);

    let card = GtkBox::new(Orientation::Vertical, 12);
    card.add_css_class("login-card");
    card.set_width_request(380);

    let word = Label::new(None);
    word.add_css_class("wordmark-hero");
    word.set_markup("<span foreground=\"#b4637a\">CAS</span><span foreground=\"#907aa9\">CADE</span>");
    word.set_use_markup(true);
    let sub = Label::new(Some("sign in to wickrunner"));
    sub.add_css_class("login-sub");

    let email = Entry::new();
    email.set_placeholder_text(Some("email"));
    let password = PasswordEntry::new();
    password.set_show_peek_icon(true);
    password.set_placeholder_text(Some("password"));

    let login_btn = Button::with_label("Sign in");
    login_btn.add_css_class("cta-pine");
    let error = Label::new(None);
    error.add_css_class("login-error");
    error.set_wrap(true);

    card.append(&word);
    card.append(&sub);
    card.append(&email);
    card.append(&password);
    card.append(&login_btn);
    card.append(&error);
    root.append(&card);

    LoginBits {
        root,
        email,
        password,
        login_btn,
        error,
    }
}

struct MainBits {
    root: GtkBox,
    sessions_list: ListBox,
    sidebar: Revealer,
    plan_reveal: Revealer,
    plan_box: GtkBox,
    transcript: GtkBox,
    transcript_scroll: ScrolledWindow,
    composer: TextView,
    send_btn: Button,
    abort_btn: Button,
    question_host: GtkBox,
    machine: Label,
    sessions_pill: ToggleButton,
    plan_pill: ToggleButton,
    new_pill: Button,
    settings_pill: Button,
    sidebar_toggle: Button,
    wordmark: Label,
}

fn build_main() -> MainBits {
    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.add_css_class("app-shell");
    root.set_halign(gtk4::Align::Center);
    root.set_width_request(1200);
    root.set_size_request(1200, -1);

    let header = GtkBox::new(Orientation::Horizontal, 12);
    header.set_margin_start(16);
    header.set_margin_end(16);
    header.set_margin_top(12);
    header.set_margin_bottom(12);

    let wordmark = Label::new(None);
    wordmark.add_css_class("wordmark");
    wordmark.set_markup("cascade<span foreground=\"#b4637a\">.</span>");
    wordmark.set_use_markup(true);

    let sidebar_toggle = Button::with_label("☰");
    sidebar_toggle.add_css_class("nav-pill");

    let sessions_pill = ToggleButton::with_label("Sessions");
    sessions_pill.add_css_class("nav-pill");
    sessions_pill.set_active(true);
    let new_pill = Button::with_label("New");
    new_pill.add_css_class("nav-pill");
    let plan_pill = ToggleButton::with_label("Plan");
    plan_pill.add_css_class("nav-pill");
    let settings_pill = Button::with_label("Settings");
    settings_pill.add_css_class("nav-pill");

    let machine = Label::new(Some("cloud"));
    machine.add_css_class("machine-chip");
    machine.set_halign(gtk4::Align::End);
    machine.set_hexpand(true);

    header.append(&wordmark);
    header.append(&sidebar_toggle);
    header.append(&sessions_pill);
    header.append(&new_pill);
    header.append(&plan_pill);
    header.append(&settings_pill);
    header.append(&machine);

    let body = GtkBox::new(Orientation::Horizontal, 0);
    body.set_hexpand(true);
    body.set_vexpand(true);

    let sessions_list = ListBox::new();
    sessions_list.set_selection_mode(gtk4::SelectionMode::Single);
    let side_scroll = ScrolledWindow::new();
    side_scroll.set_child(Some(&sessions_list));
    side_scroll.set_min_content_width(280);
    side_scroll.set_vexpand(true);
    let side_wrap = GtkBox::new(Orientation::Vertical, 8);
    side_wrap.add_css_class("sidebar");
    let side_title = Label::new(Some("Sessions"));
    side_title.add_css_class("panel-title");
    side_title.set_xalign(0.0);
    side_title.set_margin_start(12);
    side_title.set_margin_top(12);
    side_wrap.append(&side_title);
    side_wrap.append(&side_scroll);

    let sidebar = Revealer::new();
    sidebar.set_transition_type(RevealerTransitionType::SlideLeft);
    sidebar.set_transition_duration(250);
    sidebar.set_reveal_child(true);
    sidebar.set_child(Some(&side_wrap));

    let center = GtkBox::new(Orientation::Vertical, 0);
    center.set_hexpand(true);
    center.set_vexpand(true);

    let question_host = GtkBox::new(Orientation::Vertical, 8);
    question_host.set_margin_start(12);
    question_host.set_margin_end(12);
    question_host.set_margin_top(8);

    let transcript = GtkBox::new(Orientation::Vertical, 12);
    transcript.add_css_class("transcript");
    transcript.set_valign(gtk4::Align::Start);
    let transcript_scroll = ScrolledWindow::new();
    transcript_scroll.set_child(Some(&transcript));
    transcript_scroll.set_vexpand(true);
    transcript_scroll.set_hexpand(true);

    let composer_row = GtkBox::new(Orientation::Horizontal, 8);
    composer_row.add_css_class("composer");
    let composer = TextView::new();
    composer.set_wrap_mode(gtk4::WrapMode::WordChar);
    composer.set_hexpand(true);
    composer.set_size_request(-1, 72);
    let send_btn = Button::with_label("Send");
    send_btn.add_css_class("cta-pine");
    let abort_btn = Button::with_label("Abort");
    abort_btn.add_css_class("cta-love");
    composer_row.append(&composer);
    composer_row.append(&send_btn);
    composer_row.append(&abort_btn);

    center.append(&question_host);
    center.append(&transcript_scroll);
    center.append(&composer_row);

    let plan_box = GtkBox::new(Orientation::Vertical, 6);
    plan_box.add_css_class("plan-panel");
    plan_box.set_margin_start(8);
    plan_box.set_margin_end(8);
    plan_box.set_margin_top(8);
    let plan_title = Label::new(Some("Plan"));
    plan_title.add_css_class("panel-title");
    plan_title.set_xalign(0.0);
    plan_box.append(&plan_title);
    let plan_scroll = ScrolledWindow::new();
    plan_scroll.set_child(Some(&plan_box));
    plan_scroll.set_min_content_width(280);
    plan_scroll.set_vexpand(true);
    let plan_reveal = Revealer::new();
    plan_reveal.set_transition_type(RevealerTransitionType::SlideRight);
    plan_reveal.set_transition_duration(250);
    plan_reveal.set_reveal_child(false);
    plan_reveal.set_child(Some(&plan_scroll));

    body.append(&sidebar);
    body.append(&center);
    body.append(&plan_reveal);

    root.append(&header);
    root.append(&body);

    MainBits {
        root,
        sessions_list,
        sidebar,
        plan_reveal,
        plan_box,
        transcript,
        transcript_scroll,
        composer,
        send_btn,
        abort_btn,
        question_host,
        machine,
        sessions_pill,
        plan_pill,
        new_pill,
        settings_pill,
        sidebar_toggle,
        wordmark,
    }
}

fn send_prompt(ui: &Rc<RefCell<Ui>>) {
    let u = ui.borrow();
    let buf = u.composer.buffer();
    let start = buf.start_iter();
    let end = buf.end_iter();
    let text = buf.text(&start, &end, false).to_string();
    if text.trim().is_empty() {
        return;
    }
    buf.set_text("");
    drop(u);
    append_user(ui, &text);
    let _ = ui.borrow().cmd.try_send(Cmd::Prompt(text));
}

fn append_user(ui: &Rc<RefCell<Ui>>, text: &str) {
    let label = Label::new(Some(text));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    label.add_css_class("transcript-user");
    ui.borrow().transcript.append(&label);
    scroll_to_end(ui);
}

fn append_assistant_label(ui: &Rc<RefCell<Ui>>) -> Label {
    let label = Label::new(Some(""));
    label.set_wrap(true);
    label.set_xalign(0.0);
    label.set_selectable(true);
    label.add_css_class("transcript-assistant");
    label.set_wrap_mode(pango::WrapMode::WordChar);
    ui.borrow().transcript.append(&label);
    label
}

fn scroll_to_end(ui: &Rc<RefCell<Ui>>) {
    let adj = ui.borrow().transcript_scroll.vadjustment();
    glib::idle_add_local_once(move || {
        adj.set_value(adj.upper());
    });
}

fn dispatch(ui: &Rc<RefCell<Ui>>, msg: UiMsg) {
    match msg {
        UiMsg::NeedLogin { error } => {
            let u = ui.borrow();
            u.stack_login.set_visible(true);
            u.stack_main.set_visible(false);
            u.login_error.set_text(&error.unwrap_or_default());
        }
        UiMsg::LoggedIn { url } => {
            let u = ui.borrow();
            u.stack_login.set_visible(false);
            u.stack_main.set_visible(true);
            u.machine.set_text("cloud");
            show_toast_borrow(&u, &format!("connected {url}"));
            let _ = u.cmd.try_send(Cmd::RefreshSessions);
        }
        UiMsg::LoggedOut => {
            let u = ui.borrow();
            u.stack_login.set_visible(true);
            u.stack_main.set_visible(false);
            u.login_error.set_text("");
            clear_box(&u.transcript);
        }
        UiMsg::SessionList(list) => {
            render_sessions(ui, list);
        }
        UiMsg::Attached { id, kind, snapshot } => {
            {
                let mut u = ui.borrow_mut();
                u.selected_id = Some(id.clone());
                u.stream.assistant = None;
                u.stream.thinking = None;
                u.stream.tools.clear();
                u.stream.pending_ui = None;
                u.abort_btn.set_visible(false);
                u.attached_kind = Some(kind);
                match kind {
                    BackendKind::Local => u.machine.set_text("local"),
                    BackendKind::Cloud => u.machine.set_text("cloud"),
                    BackendKind::Terminal => u.machine.set_text("terminal"),
                }
                clear_box(&u.transcript);
                clear_box(&u.question_host);
            }
            restyle_selected(ui, &id);
            if let Some(snap) = snapshot {
                apply_snapshot(ui, snap);
            }
        }
        UiMsg::Event(ev) => handle_event(ui, ev),
        UiMsg::Toast(t) => show_toast(ui, &t),
        UiMsg::Error(e) => show_toast(ui, &e),
    }
}

fn restyle_selected(ui: &Rc<RefCell<Ui>>, id: &str) {
    let u = ui.borrow();
    let mut child = u.sessions_list.first_child();
    while let Some(c) = child {
        if let Some(row) = c.downcast_ref::<ListBoxRow>() {
            if row.widget_name() == id {
                row.add_css_class("selected");
            } else {
                row.remove_css_class("selected");
            }
        }
        child = c.next_sibling();
    }
}

fn render_sessions(ui: &Rc<RefCell<Ui>>, list: Vec<SessionMeta>) {
    let u = ui.borrow();
    clear_list(&u.sessions_list);
    let selected = u.selected_id.clone();
    for meta in &list {
        let row = ListBoxRow::new();
        row.set_widget_name(&meta.id);
        row.add_css_class("session-card");
        let is_terminal = meta.kind == "terminal";
        if is_terminal {
            row.add_css_class("kind-terminal");
        } else if meta.machine == "cloud" || meta.machine.is_empty() {
            row.add_css_class("kind-cloud");
        } else {
            row.add_css_class("kind-local");
        }
        if selected.as_deref() == Some(meta.id.as_str()) {
            row.add_css_class("selected");
        }
        let col = GtkBox::new(Orientation::Vertical, 4);
        let title = meta
            .name
            .clone()
            .unwrap_or_else(|| meta.id.chars().take(8).collect::<String>());
        let title_row = GtkBox::new(Orientation::Horizontal, 8);
        let t = Label::new(Some(&title));
        t.add_css_class("session-title");
        t.set_xalign(0.0);
        t.set_hexpand(true);
        title_row.append(&t);
        if is_terminal {
            let chip = Label::new(Some("terminal"));
            chip.add_css_class("chip-gold");
            title_row.append(&chip);
        }
        let sub = Label::new(Some(&meta.cwd));
        sub.add_css_class("session-subtitle");
        sub.set_xalign(0.0);
        sub.set_ellipsize(pango::EllipsizeMode::Middle);
        let meta_text = if is_terminal {
            format!("{} · {}", meta.machine, meta.cwd)
        } else {
            let when = meta.last_active.format("%Y-%m-%d %H:%M").to_string();
            format!("{} · {}", meta.machine, when)
        };
        let meta_l = Label::new(Some(&meta_text));
        meta_l.add_css_class("session-meta");
        meta_l.set_xalign(0.0);
        col.append(&title_row);
        col.append(&sub);
        col.append(&meta_l);
        row.set_child(Some(&col));
        u.sessions_list.append(&row);
    }
    drop(u);
    ui.borrow_mut().metas = list;
}

fn apply_snapshot(ui: &Rc<RefCell<Ui>>, snap: SessionSnapshot) {
    for msg in snap.messages {
        if let Some((role, text)) = parse_agent_message(&msg) {
            if role == "user" || role == "human" {
                append_user(ui, &text);
            } else {
                let l = append_assistant_label(ui);
                l.set_text(&text);
            }
        }
    }
    render_plan(ui, &snap.todos);
    ui.borrow().abort_btn.set_visible(snap.streaming);
    for req in snap.pending_ui {
        show_ui_request(ui, req);
    }
}

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

fn handle_event(ui: &Rc<RefCell<Ui>>, ev: SessionEvent) {
    match ev {
        SessionEvent::Ready { .. } => {}
        SessionEvent::TurnStarted => {
            ui.borrow().abort_btn.set_visible(true);
            ui.borrow_mut().stream.assistant = None;
        }
        SessionEvent::TextDelta { delta, .. } => {
            let label = {
                let u = ui.borrow_mut();
                if u.stream.assistant.is_none() {
                    drop(u);
                    let l = append_assistant_label(ui);
                    ui.borrow_mut().stream.assistant = Some(l.clone());
                    l
                } else {
                    u.stream.assistant.clone().unwrap()
                }
            };
            let mut t = label.text().to_string();
            t.push_str(&delta);
            label.set_text(&t);
            scroll_to_end(ui);
        }
        SessionEvent::ThinkingDelta { delta, .. } => {
            let label = {
                let u = ui.borrow_mut();
                if let Some(l) = u.stream.thinking.clone() {
                    l
                } else {
                    drop(u);
                    let l = Label::new(Some(""));
                    l.add_css_class("transcript-thinking");
                    l.set_wrap(true);
                    l.set_xalign(0.0);
                    ui.borrow().transcript.append(&l);
                    ui.borrow_mut().stream.thinking = Some(l.clone());
                    l
                }
            };
            let mut t = label.text().to_string();
            t.push_str(&delta);
            label.set_text(&t);
        }
        SessionEvent::MessageStart { role } => {
            if role == "assistant" || role == "model" {
                let l = append_assistant_label(ui);
                ui.borrow_mut().stream.assistant = Some(l);
            }
        }
        SessionEvent::MessageEnd { message } => {
            if let Some((role, text)) = parse_agent_message(&message) {
                if ui.borrow().stream.assistant.is_none() && !text.is_empty() {
                    if role == "user" {
                        append_user(ui, &text);
                    } else {
                        let l = append_assistant_label(ui);
                        l.set_text(&text);
                    }
                }
            }
            ui.borrow_mut().stream.assistant = None;
            ui.borrow_mut().stream.thinking = None;
        }
        SessionEvent::ToolStart {
            tool_call_id,
            tool_name,
            args,
            intent,
        } => {
            let exp = Expander::new(Some(&format!(
                "{tool_name}{}",
                intent
                    .as_deref()
                    .map(|i| format!(" — {i}"))
                    .unwrap_or_default()
            )));
            let body = Label::new(Some(
                &serde_json::to_string_pretty(&args).unwrap_or_default(),
            ));
            body.set_wrap(true);
            body.set_xalign(0.0);
            body.set_selectable(true);
            body.add_css_class("tool-card");
            exp.set_child(Some(&body));
            ui.borrow().transcript.append(&exp);
            ui.borrow_mut().stream.tools.insert(tool_call_id, body);
        }
        SessionEvent::ToolUpdate {
            tool_call_id,
            partial,
        } => {
            if let Some(body) = ui.borrow().stream.tools.get(&tool_call_id).cloned() {
                body.set_text(&serde_json::to_string_pretty(&partial).unwrap_or_default());
            }
        }
        SessionEvent::ToolEnd {
            tool_call_id,
            tool_name,
            is_error,
            result,
        } => {
            if let Some(body) = ui.borrow().stream.tools.get(&tool_call_id).cloned() {
                let prefix = if is_error {
                    format!("{tool_name} error\n")
                } else {
                    format!("{tool_name}\n")
                };
                body.set_text(&format!(
                    "{prefix}{}",
                    serde_json::to_string_pretty(&result).unwrap_or_default()
                ));
            }
        }
        SessionEvent::AgentEnd => {
            ui.borrow().abort_btn.set_visible(false);
            ui.borrow_mut().stream.assistant = None;
        }
        SessionEvent::TodoChanged { phases } => { tracing::info!(phases = phases.len(), "TodoChanged received"); render_plan(ui, &phases); },
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
            show_toast(ui, &format!("{level}: {message}"));
        }
        SessionEvent::SessionInfo { title, session_id } => {
            ui.borrow().window.set_title(Some(&format!("cascade — {title}")));
            if let Some(meta) = ui
                .borrow_mut()
                .metas
                .iter_mut()
                .find(|m| m.id == session_id)
            {
                meta.name = Some(title);
            }
            let metas = ui.borrow().metas.clone();
            render_sessions(ui, metas);
        }
        SessionEvent::StateChanged => {
            let _ = ui.borrow().cmd.try_send(Cmd::RefreshState);
        }
        SessionEvent::ProcessExited { code } => {
            ui.borrow().abort_btn.set_visible(false);
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

fn render_plan(ui: &Rc<RefCell<Ui>>, phases: &[TodoPhase]) {
    let u = ui.borrow();
    let mut child = u.plan_box.first_child();
    let mut skip = true;
    while let Some(c) = child {
        let next = c.next_sibling();
        if skip {
            skip = false;
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
                TodoStatus::Abandoned => ("○", "abandoned"),
            };
            let line = if matches!(task.status, TodoStatus::Abandoned) {
                format!("{glyph}  <s>{}</s>", glib::markup_escape_text(&task.content))
            } else {
                format!("{glyph}  {}", glib::markup_escape_text(&task.content))
            };
            let l = Label::new(None);
            l.set_markup(&line);
            l.set_use_markup(true);
            l.set_xalign(0.0);
            l.set_wrap(true);
            l.add_css_class("plan-task");
            l.add_css_class(class);
            u.plan_box.append(&l);
        }
    }
    if !phases.is_empty() {
        u.plan_reveal.set_reveal_child(true);
        u.plan_pill.set_active(true);
    }
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
                ui.borrow().window.set_title(Some(&t));
            }
            let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                request_id: req.id,
                response: UiAnswer::Value(String::new()),
            });
            return;
        }
        UiMethod::SetWidget | UiMethod::Other => {
            tracing::debug!(id = %req.id, "ui method ignored");
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
    card.add_css_class("question-banner");
    if let Some(t) = &req.title {
        let l = Label::new(Some(t));
        l.add_css_class("panel-title");
        l.set_xalign(0.0);
        card.append(&l);
    }
    if let Some(m) = &req.message {
        let l = Label::new(Some(m));
        l.set_wrap(true);
        l.set_xalign(0.0);
        card.append(&l);
    }

    let id = req.id.clone();
    match req.method {
        UiMethod::Select => {
            let opts = GtkBox::new(Orientation::Horizontal, 8);
            for opt in req.options {
                let b = Button::with_label(&opt);
                b.add_css_class("nav-pill");
                let opt_c = opt.clone();
                let id_c = id.clone();
                b.connect_clicked(glib::clone!(
                    #[strong]
                    ui,
                    #[strong]
                    host,
                    move |_| {
                        let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                            request_id: id_c.clone(),
                            response: UiAnswer::Value(opt_c.clone()),
                        });
                        clear_box(&host);
                        ui.borrow_mut().stream.pending_ui = None;
                    }
                ));
                opts.append(&b);
            }
            card.append(&opts);
        }
        UiMethod::Confirm => {
            let row = GtkBox::new(Orientation::Horizontal, 8);
            let yes = Button::with_label("Approve");
            yes.add_css_class("cta-pine");
            let no = Button::with_label("Deny");
            no.add_css_class("cta-love");
            yes.connect_clicked(glib::clone!(
                #[strong]
                ui,
                #[strong]
                host,
                #[strong]
                id,
                move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id.clone(),
                        response: UiAnswer::Confirmed(true),
                    });
                    clear_box(&host);
                    ui.borrow_mut().stream.pending_ui = None;
                }
            ));
            no.connect_clicked(glib::clone!(
                #[strong]
                ui,
                #[strong]
                host,
                #[strong]
                id,
                move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id.clone(),
                        response: UiAnswer::Confirmed(false),
                    });
                    clear_box(&host);
                    ui.borrow_mut().stream.pending_ui = None;
                }
            ));
            row.append(&yes);
            row.append(&no);
            card.append(&row);
        }
        UiMethod::Input => {
            let entry = Entry::new();
            entry.set_placeholder_text(req.placeholder.as_deref());
            if let Some(p) = &req.prefill {
                entry.set_text(p);
            }
            let go = Button::with_label("Submit");
            go.add_css_class("cta-pine");
            go.connect_clicked(glib::clone!(
                #[strong]
                ui,
                #[strong]
                host,
                #[strong]
                id,
                #[strong]
                entry,
                move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id.clone(),
                        response: UiAnswer::Value(entry.text().to_string()),
                    });
                    clear_box(&host);
                    ui.borrow_mut().stream.pending_ui = None;
                }
            ));
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
            let go = Button::with_label("Submit");
            go.add_css_class("cta-pine");
            go.connect_clicked(glib::clone!(
                #[strong]
                ui,
                #[strong]
                host,
                #[strong]
                id,
                #[strong]
                tv,
                move |_| {
                    let buf = tv.buffer();
                    let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).to_string();
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id.clone(),
                        response: UiAnswer::Value(text),
                    });
                    clear_box(&host);
                    ui.borrow_mut().stream.pending_ui = None;
                }
            ));
            card.append(&tv);
            card.append(&go);
        }
        UiMethod::OpenUrl => {
            let url = req.url.clone().unwrap_or_default();
            let open = Button::with_label("Open");
            open.add_css_class("cta-pine");
            let copy = Button::with_label("Copy");
            copy.add_css_class("nav-pill");
            let url_o = url.clone();
            let win = ui.borrow().window.clone();
            open.connect_clicked(move |_| {
                let launcher = gtk4::UriLauncher::new(&url_o);
                launcher.launch(Some(&win), None::<&gio::Cancellable>, |_| {});
            });
            let url_c = url.clone();
            copy.connect_clicked(move |_| {
                if let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(&url_c);
                }
            });
            let done = Button::with_label("Done");
            done.add_css_class("cta-gold");
            done.connect_clicked(glib::clone!(
                #[strong]
                ui,
                #[strong]
                host,
                #[strong]
                id,
                move |_| {
                    let _ = ui.borrow().cmd.try_send(Cmd::Answer {
                        request_id: id.clone(),
                        response: UiAnswer::Value(url.clone()),
                    });
                    clear_box(&host);
                    ui.borrow_mut().stream.pending_ui = None;
                }
            ));
            let row = GtkBox::new(Orientation::Horizontal, 8);
            row.append(&open);
            row.append(&copy);
            row.append(&done);
            card.append(&row);
        }
        _ => {}
    }
    host.append(&card);
}

fn show_new_session_dialog(ui: &Rc<RefCell<Ui>>) {
    let win = ui.borrow().window.clone();
    let dlg = Window::builder()
        .transient_for(&win)
        .modal(true)
        .title("New session")
        .default_width(420)
        .build();
    dlg.add_css_class("cascade-window");

    let col = GtkBox::new(Orientation::Vertical, 10);
    col.set_margin_start(20);
    col.set_margin_end(20);
    col.set_margin_top(20);
    col.set_margin_bottom(20);

    let title = Label::new(Some("New session"));
    title.add_css_class("panel-title");
    title.set_xalign(0.0);

    let local = gtk4::CheckButton::with_label("Local");
    let cloud = gtk4::CheckButton::with_label("Cloud wickrunner");
    cloud.set_group(Some(&local));
    let settings = Settings::load();
    if settings.last_backend == "local" {
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
    go.add_css_class("cta-pine");
    go.connect_clicked(glib::clone!(
        #[strong]
        ui,
        #[strong]
        dlg,
        #[strong]
        local,
        #[strong]
        cwd,
        #[strong]
        model,
        move |_| {
            let kind = if local.is_active() {
                BackendKind::Local
            } else {
                BackendKind::Cloud
            };
            let model_s = model.text().to_string();
            let model = if model_s.trim().is_empty() {
                None
            } else {
                Some(model_s)
            };
            let _ = ui.borrow().cmd.try_send(Cmd::NewSession {
                kind,
                cwd: cwd.text().to_string(),
                model,
            });
            dlg.close();
        }
    ));

    col.append(&title);
    col.append(&local);
    col.append(&cloud);
    col.append(&cwd);
    col.append(&model);
    col.append(&go);
    dlg.set_child(Some(&col));
    dlg.present();
}

fn show_settings_dialog(ui: &Rc<RefCell<Ui>>) {
    let win = ui.borrow().window.clone();
    let dlg = Window::builder()
        .transient_for(&win)
        .modal(true)
        .title("Settings")
        .default_width(420)
        .build();
    dlg.add_css_class("cascade-window");
    let col = GtkBox::new(Orientation::Vertical, 10);
    col.set_margin_start(20);
    col.set_margin_end(20);
    col.set_margin_top(20);
    col.set_margin_bottom(20);
    let title = Label::new(Some("Settings"));
    title.add_css_class("panel-title");
    title.set_xalign(0.0);
    let url = Entry::new();
    url.set_text(&Settings::load().cloud_url);
    let save = Button::with_label("Save URL");
    save.add_css_class("cta-pine");
    save.connect_clicked(glib::clone!(
        #[strong]
        ui,
        #[strong]
        url,
        move |_| {
            let _ = ui
                .borrow()
                .cmd
                .try_send(Cmd::SaveCloudUrl(url.text().to_string()));
        }
    ));
    let logout = Button::with_label("Log out");
    logout.add_css_class("cta-love");
    logout.connect_clicked(glib::clone!(
        #[strong]
        ui,
        #[strong]
        dlg,
        move |_| {
            let _ = ui.borrow().cmd.try_send(Cmd::Logout);
            dlg.close();
        }
    ));
    col.append(&title);
    col.append(&url);
    col.append(&save);
    col.append(&logout);
    dlg.set_child(Some(&col));
    dlg.present();
}

fn show_toast(ui: &Rc<RefCell<Ui>>, text: &str) {
    show_toast_borrow(&ui.borrow(), text);
}

fn show_toast_borrow(u: &Ui, text: &str) {
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

fn clear_list(list: &ListBox) {
    while let Some(c) = list.first_child() {
        list.remove(&c);
    }
}
