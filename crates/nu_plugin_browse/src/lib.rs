use nu_plugin::Plugin;

mod commands;
mod launch;
mod page;
mod session;
mod utils;

pub use commands::{Browse, BrowseClose, BrowseOpen, BrowseStatus};

#[derive(Debug)]
pub struct BrowsePlugin;

impl Plugin for BrowsePlugin {
    fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").into()
    }

    fn commands(&self) -> Vec<Box<dyn nu_plugin::PluginCommand<Plugin = Self>>> {
        vec![
            Box::new(Browse),
            Box::new(BrowseOpen),
            Box::new(BrowseStatus),
            Box::new(BrowseClose),
        ]
    }
}
