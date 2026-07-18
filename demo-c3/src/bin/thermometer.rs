#![no_std]
#![no_main]
#![recursion_limit = "1024"]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::string::String;

use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::raw::CriticalSectionRawMutex,
    mutex::Mutex,
    watch::Watch,
};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    i2c::master::{Config, I2c},
    tsens::{Config as TsensConfig, TemperatureSensor},
    Blocking,
};
use picoserve::{
    extract::State,
    response::{self},
    routing::get,
    AppWithStateBuilder,
};
use shtcx::{self, sensor_class::Sht2Gen, shtc3, PowerMode, ShtCx};
use wot_td::{
    builder::{
        BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
        ReadableWriteableDataSchema, SpecializableDataSchema,
    },
    Thing,
};

use wot_esp_thing::{
    mk_static, to_json_response, to_json_result, EspThing as _, SseEvents, TdCell, TdState,
};

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut ShtCx<Sht2Gen, &'static mut I2c<'static, Blocking>>,
    >,
    die_sensor: &'static TemperatureSensor<'static>,
    td: &'static TdCell,
}

impl AppState {
    /// Returns the latest temperature measurement in degrees celsius.
    async fn get_temperature(&self) -> Result<f32, shtcx::Error<esp_hal::i2c::master::Error>> {
        let t = self
            .sensor
            .lock()
            .await
            .get_temperature_measurement_result()?
            .as_degrees_celsius();
        Ok(t)
    }

    /// Returns the latest humidity measurement in percent.
    async fn get_humidity(&self) -> Result<f32, shtcx::Error<esp_hal::i2c::master::Error>> {
        Ok(self
            .sensor
            .lock()
            .await
            .get_humidity_measurement_result()?
            .as_percent())
    }

    /// Returns the ESP32-C3 internal die temperature in degrees celsius.
    fn get_die_temperature(&self) -> f32 {
        self.die_sensor.get_temperature().to_celsius()
    }
}

impl TdState for AppState {
    fn td(&self) -> &'static str {
        self.td.get()
    }
}

impl wot_esp_thing::EspThingState for AppState {
    fn new(
        spawner: embassy_executor::Spawner,
        peripherals: esp_hal::peripherals::Peripherals,
    ) -> (&'static Self, wot_esp_thing::NetworkPeripherals<'static>) {
        let net = wot_esp_thing::NetworkPeripherals {
            timg0: peripherals.TIMG0,
            sw_interrupt: peripherals.SW_INTERRUPT,
            wifi: peripherals.WIFI,
        };

        // Initialize temperature sensor
        let sda = peripherals.GPIO10;
        let scl = peripherals.GPIO8;

        let i2c = mk_static!(
            I2c<'static, Blocking>,
            I2c::new(
                peripherals.I2C0,
                Config::default().with_frequency(esp_hal::time::Rate::from_khz(100))
            )
            .expect("Cannot access the thermometer")
            .with_sda(sda)
            .with_scl(scl)
        );

        let sht = mk_static!(
            ShtCx < Sht2Gen,
            &'static mut I2c<'static, Blocking>>,
            shtc3(i2c)
        );

        let sensor = mk_static!(
            Mutex<
                CriticalSectionRawMutex,
            &'static mut
                ShtCx<
                Sht2Gen,&'static mut
                I2c<
                'static,
            Blocking,
            >
                >
                >,
            Mutex::<CriticalSectionRawMutex, _>::new(sht)
        );

        let die_sensor = mk_static!(
            TemperatureSensor<'static>,
            TemperatureSensor::new(peripherals.TSENS, TsensConfig::default())
                .expect("Cannot access the internal temperature sensor")
        );

        let app_state = mk_static!(
            AppState,
            AppState {
                sensor,
                die_sensor,
                td: mk_static!(TdCell, TdCell::new()),
            }
        );

        spawner.spawn(temperature_write_task(app_state).expect("temperature_write_task"));

        (app_state, net)
    }

    fn set_td(&self, td: &'static str) {
        self.td.set(td);
    }
}

#[derive(Default)]
struct AppProps;

impl wot_esp_thing::EspThing<AppProps> for AppProps {
    const NAME: &'static str = "shtc3";

    fn build_td(name: &str, base_uri: String, id: String) -> Thing {
        Thing::builder(name)
            .finish_extend()
            .id(id)
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
                    .unit("Celsius")
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
                    .unit("%")
            })
            .property("die_temperature", |p| {
                p.finish_extend_data_schema()
                    .attype("TemperatureProperty")
                    .title("Die temperature")
                    .description("ESP32-C3 internal die temperature")
                    .form(|f| {
                        f.href("/properties/die_temperature")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                    })
                    .number()
                    .read_only()
                    .unit("Celsius")
            })
            .event("temperature", |b| {
                b.data(|b| b.finish_extend().number().unit("Celsius"))
                    .form(|form_builder| {
                        form_builder
                            .href("/events/temperature")
                            .op(wot_td::thing::FormOperation::SubscribeEvent)
                            .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                            .subprotocol("sse")
                    })
            })
            .build()
            .unwrap()
    }
}

impl AppWithStateBuilder for AppProps {
    type State = AppState;
    type PathRouter = impl picoserve::routing::PathRouter<Self::State>;

    fn build_app(self) -> picoserve::Router<Self::PathRouter, Self::State> {
        wot_esp_thing::td_routes::<AppState>()
            .route(
                "/properties/temperature",
                get(async move |State(state): State<AppState>| {
                    to_json_result(
                        state.get_temperature().await,
                        "Failed to read temperature value.",
                    )
                }),
            )
            .route(
                "/properties/humidity",
                get(async move |State(state): State<AppState>| {
                    to_json_result(
                        state.get_humidity().await,
                        "Failed to read humidity value.",
                    )
                }),
            )
            .route(
                "/properties/die_temperature",
                get(async move |State(state): State<AppState>| {
                    to_json_response(&state.get_die_temperature())
                }),
            )
            .route(
                "/events/temperature",
                get(async move || response::EventStream(SseEvents(WATCH.receiver().unwrap()))),
            )
    }
}

#[embassy_executor::task]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn temperature_write_task(state: &'static AppState) -> ! {
    let sender = WATCH.sender();
    let mut last_temp = state.get_temperature().await.unwrap_or(-500.0);

    loop {
        state
            .sensor
            .lock()
            .await
            .start_measurement(PowerMode::NormalMode)
            .unwrap();

        Timer::after(Duration::from_secs(1)).await;
        let temperature = state.get_temperature().await;

        if let Ok(temperature) = temperature {
            if ((last_temp - temperature) * 100f32) as u32 / 10 != 0 {
                sender.send(temperature);
                last_temp = temperature;
            }
        }
    }
}

static WATCH: Watch<CriticalSectionRawMutex, f32, 2> = Watch::new();

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
