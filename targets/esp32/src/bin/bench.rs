#![no_std]
#![no_main]

use esp_backtrace as _;
use esp_println::println;

#[esp_hal::main]
fn main() -> ! {
    let _peripherals = esp_hal::init(esp_hal::Config::default());
    println!("Booting bench...");
    loop {}
}
