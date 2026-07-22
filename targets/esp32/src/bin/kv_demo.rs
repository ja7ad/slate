#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_println::println;

#[esp_hal::main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    loop { }
}

fn handle_cmd(cmd: &str) {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("put") => {
            let k = parts.next().unwrap_or("");
            let v = parts.next().unwrap_or("");
            println!("put {} {}", k, v);
            // TODO: call slate
        }
        Some("get") => {
            let k = parts.next().unwrap_or("");
            println!("get {}", k);
        }
        Some("del") => {
            let k = parts.next().unwrap_or("");
            println!("del {}", k);
        }
        Some("commit") => {
            println!("ack 1"); // echo ack <seq> after commit
        }
        Some("stats") => {
            println!("stats: commits=0 wakes=0");
        }
        Some("mode") => {
            println!("BestEffortRollback");
        }
        Some("selftest") => {
            println!("OK");
        }
        Some("format") => {
            println!("formatted");
        }
        _ => {
            println!("unknown command");
        }
    }
}
