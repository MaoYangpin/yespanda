mod app;
mod config;
mod pdf;
mod toc;

use relm4::gtk::glib;
use relm4::gtk::gio::{self, prelude::{ApplicationExt, ApplicationExtManual, FileExt}};
use relm4::{MessageBroker, RelmApp};

use crate::app::{AppModel, AppMsg};
use crate::config::Config;

pub const APP_ID: &str = "io.github.yespanda.Yespanda";

pub(crate) static APP_BROKER: MessageBroker<AppMsg> = MessageBroker::new();

fn main() {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    // GApplication uniqueness guarantees a single instance: this process owns
    // APP_ID on the session bus, while every later launch registers remotely,
    // hands its files over through this signal and leaves.
    app.connect_open(|app, files, _hint| {
        // gio skips ::activate for file launches, so request it explicitly;
        // this is what makes relm4 present its window. Without it the
        // application would sit idle with a hidden window.
        app.activate();
        if let Some(file) = files.first() {
            APP_BROKER.send(AppMsg::OpenFile(file.uri().into()));
        }
    });

    // RelmApp's runner performs one blocking main-context iteration after
    // g_application_run returns. Secondary instances return almost instantly
    // and would sit in that iteration forever on an otherwise idle context,
    // so provide a recurring source that lets it finish promptly.
    glib::timeout_add_local(std::time::Duration::from_millis(200), || {
        glib::ControlFlow::Continue
    });

    RelmApp::from_app(app)
        .with_broker(&APP_BROKER)
        .run::<AppModel>(Config::load());
}
