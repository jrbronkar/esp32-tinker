#![no_std]
#![no_main]

use esp_backtrace as _;

esp_bootloader_esp_idf::esp_app_desc!();

#[cfg(not(feature = "wifi"))]
mod serial;
#[cfg(feature = "wifi")]
mod wifi;
