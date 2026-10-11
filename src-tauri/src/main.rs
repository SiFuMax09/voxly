#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if voxly_lib::run_client() {
        return;
    }
    voxly_lib::run()
}
