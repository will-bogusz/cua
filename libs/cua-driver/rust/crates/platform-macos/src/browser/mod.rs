pub mod ax_page_reader;
pub mod browser_js;
pub mod cdp_client;
mod consent_ui;
pub mod electron_js;
pub mod platform;
mod setup_ui;
pub mod wk_web_view;

/// Whether this process renders its UI with Chromium (Chrome, Edge, Brave,
/// Arc, an Electron host). Those controls are commonly driven by
/// `pointerdown`, which no background route can deliver; native toolkits are
/// not affected, so advice about them must not be handed to every app.
pub fn is_chromium_family(pid: i32) -> bool {
    let name = crate::apps::get_app_name_for_pid(pid).unwrap_or_default();
    let bundle_id = crate::apps::bundle_id_for_pid(pid).unwrap_or_default();
    platform::is_chromium(&name, &bundle_id) || ElectronJs::is_electron(pid)
}

pub use ax_page_reader::AXPageReader;
pub use browser_js::BrowserJs;
pub use cdp_client::{CdpClient, CdpSessionCache};
pub use electron_js::ElectronJs;
pub use platform::MacOsBrowserPlatform;
pub use wk_web_view::is_wk_web_view_app;
