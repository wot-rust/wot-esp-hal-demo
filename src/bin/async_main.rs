#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]

extern crate alloc;

use alloc::{format, string::String};
use embassy_executor::Spawner;
use embassy_net::{Stack, StackResources};
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    gpio::Io,
    i2c::{self, I2c},
    peripherals::I2C0,
    prelude::*,
    rng::Rng,
    timer::timg::TimerGroup,
    Blocking,
};
use esp_println::println;
use esp_wifi::{
    init,
    wifi::{
        ClientConfiguration, Configuration, WifiController, WifiDevice, WifiEvent, WifiStaDevice,
        WifiState,
    },
    EspWifiInitFor,
};
use picoserve::{
    extract::State,
    response::{Response, StatusCode},
    routing::get,
};
use shtcx::{self, sensor_class::Sht2Gen, shtc3, PowerMode, ShtCx};
use wot_td::{builder::*, Thing};

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut ShtCx<Sht2Gen, &'static mut I2c<'static, I2C0, Blocking>>,
    >,
    td: &'static str,
}

type AppRouter = impl picoserve::routing::PathRouter<AppState>;

const WEB_TASK_POOL_SIZE: usize = 1;

#[embassy_executor::task(pool_size = WEB_TASK_POOL_SIZE)]
async fn web_task(
    id: usize,
    stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    app: &'static picoserve::Router<AppRouter, AppState>,
    config: &'static picoserve::Config<Duration>,
    state: &'static AppState,
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

// https://github.com/embassy-rs/static-cell/issues/16
macro_rules! mk_static {
    ($t:ty,$val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        STATIC_CELL.init_with(|| $val)
    }};
}

const SSID: &str = env!("SSID");
const PASSWORD: &str = env!("PASSWORD");

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger_from_env();
    let peripherals = esp_hal::init({
        let mut config = esp_hal::Config::default();
        config.cpu_clock = CpuClock::max();
        config
    });

    esp_alloc::heap_allocator!(72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);

    let init = init(
        EspWifiInitFor::Wifi,
        timg0.timer0,
        Rng::new(peripherals.RNG),
        peripherals.RADIO_CLK,
    )
    .unwrap();

    let wifi = peripherals.WIFI;
    let (wifi_interface, controller) =
        esp_wifi::wifi::new_with_mode(&init, wifi, WifiStaDevice).unwrap();

    use esp_hal::timer::systimer::{SystemTimer, Target};
    let systimer = SystemTimer::new(peripherals.SYSTIMER).split::<Target>();
    esp_hal_embassy::init(systimer.alarm0);

    let config = embassy_net::Config::dhcpv4(Default::default());

    let seed = 1234; // very random, very secure seed

    // Init network stack
    let stack = &*mk_static!(
        Stack<WifiDevice<'_, WifiStaDevice>>,
        Stack::new(
            wifi_interface,
            config,
            mk_static!(StackResources<3>, StackResources::<3>::new()),
            seed
        )
    );

    spawner.spawn(connection(controller)).ok();
    spawner.spawn(net_task(&stack)).ok();

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

    // Initialize temperature sensor
    let io = Io::new(peripherals.GPIO, peripherals.IO_MUX);

    let sda = io.pins.gpio10;
    let scl = io.pins.gpio8;
    let i2c = mk_static!(
        I2c<'static, I2C0, Blocking>,
        i2c::I2c::new(peripherals.I2C0, sda, scl, 100.kHz())
    );
    let sht = mk_static!(ShtCx<Sht2Gen, &'static mut I2c<'static, I2C0, Blocking>>, shtc3(i2c));

    let device_id = stack.hardware_address();

    let td = Thing::builder("shtc3")
        .finish_extend()
        .id(format!("urn:example/shtc3/{device_id}"))
        .base(base_uri)
        .description("Example Thing exposing a shtc3 sensor")
        .security(|builder| builder.no_sec().required().with_key("nosec_sc"))
        .property("temperature", |p| {
            p.finish_extend_data_schema()
                .attype("TemperatureProperty")
                .title("Temperature")
                .description("Current temperature")
                .form(|f| {
                    f.href("/properties/temperature")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
        })
        .property("humidity", |p| {
            p.finish_extend_data_schema()
                .attype("HumidityProperty")
                .title("Humidity")
                .description("Current humidity")
                .form(|f| {
                    f.href("/properties/humidity")
                        .op(wot_td::thing::FormOperation::ReadProperty)
                })
                .number()
                .read_only()
        })
        .build()
        .unwrap();

    let td = serde_json::to_string(&td).unwrap();

    sht.start_measurement(PowerMode::NormalMode).unwrap();

    let sensor = mk_static!(
            Mutex<
                CriticalSectionRawMutex,
                &'static mut
                ShtCx<
                    Sht2Gen,&'static mut
                    I2c<
                        'static,
                        I2C0,
                        Blocking
                    >
                >
            >,
        Mutex::<CriticalSectionRawMutex, &'static mut ShtCx<Sht2Gen,&'static mut I2c<'static, I2C0, Blocking>>>::new(sht)
    );

    let app_state = mk_static!(
        AppState,
        AppState {
            sensor,
            td: mk_static!(String, td),
        }
    );

    fn make_app() -> picoserve::Router<AppRouter, AppState> {
        picoserve::Router::new()
            .route(
                "/.well-known/wot",
                get(|State(state): State<AppState>| async move { state.td }),
            )
            .route(
                "/properties/temperature",
                get(|State(state): State<AppState>| async move {
                    let temperature = state
                        .sensor
                        .lock()
                        .await
                        .get_temperature_measurement_result();

                    if let Ok(temperature) = temperature {
                        let body = format!("{}", temperature.as_degrees_celsius());

                        return Response::ok(body);
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read temperature value.".into(),
                    )
                }),
            )
            .route(
                "/properties/humidity",
                get(|State(state): State<AppState>| async move {
                    let humidity = state.sensor.lock().await.get_humidity_measurement_result();

                    if let Ok(humidity) = humidity {
                        let body = format!("{}", humidity.as_percent());

                        return Response::ok(body);
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read humidity value.".into(),
                    )
                }),
            )
    }

    let app = mk_static!(picoserve::Router<AppRouter, AppState>, make_app());

    let config = mk_static!(
        picoserve::Config::<Duration>,
        picoserve::Config::new(picoserve::Timeouts {
            start_read_request: Some(Duration::from_secs(5)),
            read_request: Some(Duration::from_secs(1)),
            write: Some(Duration::from_secs(1)),
        })
        .keep_connection_alive()
    );

    for id in 0..WEB_TASK_POOL_SIZE {
        spawner.must_spawn(web_task(id, stack, app, config, app_state));
    }
}

#[embassy_executor::task]
async fn connection(mut controller: WifiController<'static>) {
    println!("start connection task");
    println!("Device capabilities: {:?}", controller.get_capabilities());
    loop {
        if esp_wifi::wifi::get_wifi_state() == WifiState::StaConnected {
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
            controller.start().await.unwrap();
            println!("Wifi started!");
        }
        println!("About to connect...");

        match controller.connect().await {
            Ok(_) => println!("Wifi connected!"),
            Err(e) => {
                println!("Failed to connect to wifi: {e:?}");
                Timer::after(Duration::from_millis(5000)).await
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>) {
    stack.run().await
}
