// Prevents an extra console window on Windows in release builds. Without
// this the binary uses the console subsystem and a terminal window opens
// alongside the app on launch. DO NOT REMOVE.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    termory_lib::run()
}
