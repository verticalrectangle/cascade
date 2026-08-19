mod settings;
mod ui;
mod worker;

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{Application, CssProvider, STYLE_PROVIDER_PRIORITY_APPLICATION};

const APP_ID: &str = "com.wickrunner.cascade";
const THEME_CSS: &str = include_str!("theme/style.css");

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cascade_gtk=info".parse().unwrap()),
        )
        .init();

    let (cmd_tx, cmd_rx) = async_channel::unbounded::<worker::Cmd>();
    let (ui_tx, ui_rx) = async_channel::unbounded::<worker::UiMsg>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("cascade-tokio")
            .build()
            .expect("tokio runtime");
        rt.block_on(worker::worker(cmd_rx, ui_tx));
    });

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(move |app| {
        ui::build(app, cmd_tx.clone(), ui_rx.clone());
    });
    app.run();
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(THEME_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
