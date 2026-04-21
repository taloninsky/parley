mod audio;
mod stt;
mod ui;

use ui::root::Root;

fn main() {
    dioxus::launch(Root);
}
