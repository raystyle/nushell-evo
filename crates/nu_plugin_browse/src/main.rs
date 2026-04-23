use nu_plugin::{MsgPackSerializer, serve_plugin};
use nu_plugin_browse::BrowsePlugin;

fn main() {
    serve_plugin(&BrowsePlugin, MsgPackSerializer)
}
