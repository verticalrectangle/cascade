mod highlight;
mod markdown;
mod settings;
mod ui;
mod worker;

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{Application, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

const APP_ID: &str = "com.wickrunner.cascade";
const DAWN_CSS: &str = include_str!("theme/dawn.css");
const MOON_CSS: &str = include_str!("theme/moon.css");

thread_local! {
    static PROVIDER: std::cell::RefCell<Option<CssProvider>> = const { std::cell::RefCell::new(None) };
}

/// Swap the app-wide theme ("dawn" default, "moon" dark).
pub fn apply_theme(name: &str) {
    PROVIDER.with(|p| {
        if let Some(provider) = p.borrow().as_ref() {
            match name {
                "moon" => provider.load_from_data(MOON_CSS),
                _ => provider.load_from_data(DAWN_CSS),
            }
        }
    });
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cascade_gtk=info".parse().unwrap()),
        )
        .init();

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<worker::Cmd>();
    let (ui_tx, ui_rx) = async_channel::unbounded::<worker::UiMsg>();

    std::thread::spawn({
        let cmd_tx = cmd_tx.clone();
        move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("cascade-tokio")
                .build()
                .expect("tokio runtime");
            rt.block_on(worker::worker(cmd_rx, ui_tx, cmd_tx));
        }
    });

    // CASCADE_AUTOTEST: deterministic UI driving for headless testing.
    // Semicolon steps: wait:<secs> | open-cloud:<index> | open-terminal:<index>
    //   | open-local:<index> | prompt:<text> | abort | new-cloud:<cwd>
    if let Ok(script) = std::env::var("CASCADE_AUTOTEST") {
        let tx = cmd_tx.clone();
        std::thread::spawn(move || {
            for step in script.split(';') {
                let (op, arg) = step.split_once(':').unwrap_or((step, ""));
                let cmd = match op {
                    "wait" => {
                        std::thread::sleep(std::time::Duration::from_secs_f64(
                            arg.parse().unwrap_or(1.0),
                        ));
                        continue;
                    }
                    "open-cloud" => worker::Cmd::AutotestOpen {
                        kind: worker::BackendKind::Cloud,
                        index: arg.parse().unwrap_or(0),
                    },
                    "open-terminal" => worker::Cmd::AutotestOpen {
                        kind: worker::BackendKind::Terminal,
                        index: arg.parse().unwrap_or(0),
                    },
                    "open-local" => worker::Cmd::AutotestOpen {
                        kind: worker::BackendKind::Local,
                        index: arg.parse().unwrap_or(0),
                    },
                    "prompt" => worker::Cmd::Prompt(arg.to_string()),
                    "abort" => worker::Cmd::Abort,
                    "new-cloud" => worker::Cmd::NewSession {
                        kind: worker::BackendKind::Cloud,
                        cwd: arg.to_string(),
                        model: None,
                    },
                    other => {
                        eprintln!("autotest: unknown step {other}");
                        continue;
                    }
                };
                let _ = tx.send_blocking(cmd);
            }
        });
    }

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(move |app| {
        ui::build(app, cmd_tx.clone(), ui_rx.clone());
    });
    app.run();
}

fn load_css() {
    let provider = CssProvider::new();
    let theme = settings::Settings::load().theme;
    match theme.as_str() {
        "moon" => provider.load_from_data(MOON_CSS),
        _ => provider.load_from_data(DAWN_CSS),
    }
    PROVIDER.with(|p| *p.borrow_mut() = Some(provider.clone()));
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
