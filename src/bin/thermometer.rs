#![no_std]
#![no_main]
#![feature(type_alias_impl_trait)]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::{
    format,
    string::{String, ToString},
};

use embassy_executor::Spawner;
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, mutex::Mutex, watch::Watch};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    i2c::master::{Config, I2c},
    time::RateExtU32,
    Blocking,
};
use picoserve::{
    extract::State,
    response::{self, Redirect, Response, StatusCode},
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

use wot_esp_hal_demo::{mk_static, EspThing as _};

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut ShtCx<Sht2Gen, &'static mut I2c<'static, Blocking>>,
    >,
    td: &'static str,
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
}

impl wot_esp_hal_demo::EspThingState for AppState {
    fn new(
        spawner: embassy_executor::Spawner,
        td: String,
        peripherals: wot_esp_hal_demo::ThingPeripherals,
    ) -> &'static Self {
        // Initialize temperature sensor

        let sda = peripherals.GPIO10;
        let scl = peripherals.GPIO8;

        let i2c = mk_static!(
            I2c<'static, Blocking>,
            I2c::new(
                peripherals.I2C0,
                Config::default().with_frequency(100.kHz())
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

        let app_state = mk_static!(
            AppState,
            AppState {
                sensor,
                td: mk_static!(String, td),
            }
        );

        spawner.spawn(temperature_write_task(app_state)).ok();

        app_state
    }
}

#[derive(Default)]
struct AppProps;

impl wot_esp_hal_demo::EspThing<AppProps> for AppProps {
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
        picoserve::Router::new()
            .route(
                "/",
                get(|State(state): State<AppState>| async move {
                    Response::ok(state.td).with_header("Content-Type", "application/td+json")
                }),
            )
            .route("/.well-known/wot", get(|| Redirect::to("/")))
            .route(
                "/properties/temperature",
                get(|State(state): State<AppState>| async move {
                    let temperature = state.get_temperature().await;

                    if let Ok(temperature) = temperature {
                        let body = format!("{temperature}");

                        return Response::ok(body).with_header("Content-Type", "application/json");
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read temperature value.".into(),
                    )
                    .with_header("Content-Type", "text/plain")
                }),
            )
            .route(
                "/properties/humidity",
                get(|State(state): State<AppState>| async move {
                    let humidity = state.get_humidity().await;

                    if let Ok(humidity) = humidity {
                        let body = format!("{humidity}");

                        return Response::ok(body).with_header("Content-Type", "application/json");
                    }

                    Response::new(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "Failed to read humidity value.".into(),
                    )
                    .with_header("Content-Type", "text/plain")
                }),
            )
            .route(
                "/events/temperature",
                get(move || response::EventStream(Events(WATCH.receiver().unwrap()))),
            )
    }
}

#[embassy_executor::task]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn temperature_write_task(state: &'static AppState) -> ! {
    let sender = WATCH.sender();
    let t = state.get_temperature().await.unwrap_or(-500.0);

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
            if ((t - temperature) * 100f32) as u32 / 10 != 0 {
                sender.send(temperature);
            }
        }
    }
}

static WATCH: Watch<CriticalSectionRawMutex, f32, 2> = Watch::new();

struct Events<'a>(embassy_sync::watch::Receiver<'a, CriticalSectionRawMutex, f32, 2>);

impl response::sse::EventSource for Events<'_> {
    async fn write_events<W: picoserve::io::Write>(
        mut self,
        mut writer: response::sse::EventWriter<W>,
    ) -> Result<(), W::Error> {
        loop {
            match embassy_time::with_timeout(
                embassy_time::Duration::from_secs(15),
                self.0.changed(),
            )
            .await
            {
                Ok(value) => {
                    writer
                        .write_event("value_changed", value.to_string().as_str())
                        .await?;
                }
                Err(_) => writer.write_keepalive().await?,
            }
        }
    }
}

#[esp_hal_embassy::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
