#![no_std]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]
#![feature(never_type)]
#![feature(impl_trait_in_bindings)]

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
use picoserve::{
    response::{IntoResponse, Response},
    AppRouter, AppWithStateBuilder,
};

pub mod mdns;
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
#[must_use]
pub fn get_urn_or_uuid(stack: Stack) -> String {
    if cfg!(feature = "uuid-id") {
        let uuid = uuid::Builder::from_random_bytes(UUID_SEED).into_uuid();

        uuid.urn().to_string()
    } else {
        let device_id = stack.hardware_address().to_string();
        format!("urn:example/shtc3/{device_id}")
    }
}

/// # Panics
#[must_use]
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
            Timer::after(Duration::from_millis(5000)).await;
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
            Ok(()) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await;
            }
        }
    }
}

#[embassy_executor::task]
pub async fn net_task(mut runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>) {
    runner.run().await;
}

#[allow(clippy::similar_names)]
pub async fn web_task<Props: AppWithStateBuilder>(
    id: usize,
    stack: Stack<'static>,
    app: &'static AppRouter<Props>,
    config: &'static picoserve::Config<Duration>,
    state: &'static Props::State,
) -> ! {
    let port = 80;
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::listen_and_serve_with_state(
        id,
        app,
        config,
        stack,
        port,
        &mut tcp_rx_buffer,
        &mut tcp_tx_buffer,
        &mut http_buffer,
        state,
    )
    .await
}

#[allow(non_snake_case)]
pub struct ThingPeripherals {
    pub I2C0: esp_hal::peripherals::I2C0,
    pub GPIO2: esp_hal::gpio::GpioPin<2>,
    pub GPIO8: esp_hal::gpio::GpioPin<8>,
    pub GPIO9: esp_hal::gpio::GpioPin<9>,
    pub GPIO10: esp_hal::gpio::GpioPin<10>,
    pub RMT: esp_hal::peripherals::RMT,
}

pub trait EspThingState {
    fn new(
        spawner: embassy_executor::Spawner,
        td: String,
        thing_peripherals: ThingPeripherals,
    ) -> &'static Self;
}

pub trait EspThing<Props>
where
    Props: AppWithStateBuilder + Default + 'static,
    Props::State: EspThingState + 'static,
{
    const NAME: &'static str;

    fn build_td(name: &str, base_uri: String, id: String) -> wot_td::Thing;

    #[allow(async_fn_in_trait, clippy::must_use_candidate)]
    async fn run(spawner: embassy_executor::Spawner) {
        esp_println::logger::init_logger_from_env();
        let peripherals = esp_hal::init({
            let mut config = esp_hal::Config::default();
            config.cpu_clock = esp_hal::clock::CpuClock::max();
            config
        });

        let rng = esp_hal::rng::Rng::new(peripherals.RNG);

        esp_alloc::heap_allocator!(144 * 1024);

        let timg0 = esp_hal::timer::timg::TimerGroup::new(peripherals.TIMG0);

        let init = &*mk_static!(
            esp_wifi::EspWifiController<'static>,
            esp_wifi::init(timg0.timer0, rng, peripherals.RADIO_CLK,).unwrap()
        );

        let wifi = peripherals.WIFI;
        let (wifi_interface, controller) =
            esp_wifi::wifi::new_with_mode(init, wifi, WifiStaDevice).unwrap();

        let systimer = esp_hal::timer::systimer::SystemTimer::new(peripherals.SYSTIMER);
        esp_hal_embassy::init(systimer.alarm0);

        let config = embassy_net::Config::dhcpv4(embassy_net::DhcpConfig::default());

        let seed = 1234; // very random, very secure seed

        // Init network stack
        let (stack, runner) = embassy_net::new(
            wifi_interface,
            config,
            alloc::boxed::Box::leak(alloc::boxed::Box::new(embassy_net::StackResources::<
                { 8 * mdns::MDNS_STACK_SIZE + 2 },
            >::new())),
            seed,
        );

        spawner.spawn(connection(controller)).ok();
        spawner.spawn(net_task(runner)).ok();

        loop {
            if stack.is_link_up() {
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }

        let base_uri;
        println!("Waiting to get IP address...");
        loop {
            if let Some(config) = stack.config_v4() {
                println!("Got IP: {}", config.address);
                base_uri = format!("http://{}", config.address.address());
                break;
            }
            Timer::after(Duration::from_millis(500)).await;
        }

        let id = get_urn_or_uuid(stack);

        let name = Self::NAME;

        let td = Self::build_td(Self::NAME, base_uri, id);

        let td = serde_json::to_string(&td).unwrap();

        let thing_peripherals = ThingPeripherals {
            I2C0: peripherals.I2C0,
            GPIO2: peripherals.GPIO2,
            GPIO8: peripherals.GPIO8,
            GPIO9: peripherals.GPIO9,
            GPIO10: peripherals.GPIO10,
            RMT: peripherals.RMT,
        };

        let app_state = Props::State::new(spawner, td, thing_peripherals);

        let app = alloc::boxed::Box::leak(alloc::boxed::Box::new(Props::default().build_app()));

        let config = mk_static!(
            picoserve::Config::<Duration>,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Some(Duration::from_secs(5)),
                read_request: Some(Duration::from_secs(1)),
                write: Some(Duration::from_secs(1)),
            })
            .keep_connection_alive()
        );

        spawner.spawn(mdns::mdns_task(stack, rng, name)).ok();

        let web_tasks: [core::pin::Pin<alloc::boxed::Box<impl core::future::Future<Output = !>>>;
            8] = core::array::from_fn(|id| {
            alloc::boxed::Box::pin(web_task::<Props>(id, stack, app, config, app_state))
        });

        embassy_futures::join::join_array(web_tasks).await;
    }
}
