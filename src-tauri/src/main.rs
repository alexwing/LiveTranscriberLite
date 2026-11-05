// Sin consola en release; en debug se deja para ver las trazas.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    live_transcriber_lite_lib::run()
}
