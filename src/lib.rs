#![no_std]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
};
use embassy_net::{Runner, Stack};
use embassy_time::{Duration, Timer};
use esp_println::println;
use esp_wifi::wifi::{
    ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
    WifiState,
};
use picoserve::response::{IntoResponse, Response};

pub mod smartled;

// https://github.com/embassy-rs/static-cell/issues/16
#[macro_export]
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init_with(|| $val)
    }};
}

pub const SSID: &str = env!("SSID");
pub const PASSWORD: &str = env!("PASSWORD");

// TODO: Remove this horrible workaround once https://github.com/tkaitchuck/constrandom/issues/36 has been resolved
const UUID_SEED: [u8; 16] = [
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
    const_random::const_random!(u8),
];

/// Produce an urn that can be used as id
pub fn get_urn_or_uuid(stack: Stack) -> String {
    if cfg!(feature = "uuid-id") {
        let uuid = uuid::Builder::from_random_bytes(UUID_SEED).into_uuid();

        uuid.urn().to_string()
    } else {
        let device_id = stack.hardware_address().to_string();
        format!("urn:example/shtc3/{device_id}")
    }
}

pub fn to_json_response<T: serde::Serialize>(data: &T) -> impl IntoResponse {
    let body = serde_json::to_string(data).unwrap();
    Response::ok(body).with_header("Content-Type", "application/json")
}

#[embassy_executor::task]
pub async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.capabilities());
    loop {
        if esp_wifi::wifi::wifi_state() == WifiState::StaConnected {
            // wait until we're no longer connected
            controller.wait_for_event(WifiEvent::StaDisconnected).await;
            Timer::after(Duration::from_millis(5000)).await
        }
        if !matches!(controller.is_started(), Ok(true)) {
            let client_config = Configuration::Client(ClientConfiguration {
                ssid: SSID.try_into().unwrap(),
                password: PASSWORD.try_into().unwrap(),
                ..Default::default()
            });
            controller.set_configuration(&client_config).unwrap();
            println!("Starting wifi");
            controller.start_async().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect_async().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await
}
