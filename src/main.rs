mod audio;
mod stt;
mod ui;

use ui::app::App;

fn main() {
    dioxus::launch(App);
}
