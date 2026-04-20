mod audio;
mod stt;
mod ui;
mod word_graph;

use ui::app::App;

fn main() {
    dioxus::launch(App);
}
