#![no_std]
#![no_main]
#![recursion_limit = "1024"]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::string::String;
use embassy_executor::Spawner;
use embassy_sync::{
    blocking_mutex::{raw::CriticalSectionRawMutex, CriticalSectionMutex},
    mutex::Mutex,
    watch::Watch,
};
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::{
    gpio::{Input, InputConfig, Pull},
    i2c::master::{Config as I2cConfig, I2c},
    ledc::{
        channel::{self, ChannelIFace},
        timer::{self, TimerIFace, LSClockSource},
        LowSpeed, Ledc,
    },
    pcnt::{channel::*, Pcnt},
    tsens::{Config as TsensConfig, TemperatureSensor},
    Async,
};
use picoserve::{
    extract::State,
    response::{self, StatusCode},
    routing::get,
    AppWithStateBuilder,
};
use portable_atomic::{AtomicBool, AtomicI16, Ordering};
use sht4x_rjw::asynch::SHT4x;
use wot_esp_thing::{
    mk_static, td_routes, to_json_response, to_json_result, EspThing as _, PowerSaveMode,
    SseEvents, TdCell, TdState,
};
use wot_td::{
    builder::{
        BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
        IntegerDataSchemaBuilderLike, ReadableWriteableDataSchema, SpecializableDataSchema,
    },
    Thing,
};

static FAN_RPM: AtomicI16 = AtomicI16::new(0);

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut SHT4x<I2c<'static, Async>>,
    >,
    die_sensor: &'static TemperatureSensor<'static>,
    fan_channel:
        &'static CriticalSectionMutex<esp_hal::ledc::channel::Channel<'static, LowSpeed>>,
    fan_on: &'static AtomicBool,
    fan_duty: &'static CriticalSectionMutex<core::cell::Cell<u8>>,
    td: &'static TdCell,
}

impl AppState {
    async fn get_temperature(&self) -> Result<f32, sht4x_rjw::error::Error<esp_hal::i2c::master::Error>> {
        let mut sensor = self.sensor.lock().await;
        let m = sensor.measure(embassy_time::Delay).await?;
        Ok(m.celsius())
    }

    async fn get_humidity(&self) -> Result<f32, sht4x_rjw::error::Error<esp_hal::i2c::master::Error>> {
        let mut sensor = self.sensor.lock().await;
        let m = sensor.measure(embassy_time::Delay).await?;
        Ok(m.humidity())
    }

    fn get_die_temperature(&self) -> f32 {
        self.die_sensor.get_temperature().to_celsius()
    }

    fn get_fan_speed(&self) -> u8 {
        self.fan_duty.lock(|d| d.get())
    }

    fn set_fan_speed(&self, duty: u8) {
        let on = self.fan_on.load(Ordering::Relaxed);
        let effective = if on { duty } else { 0 };
        self.fan_channel.lock(|ch| {
            let _ = ch.set_duty(effective);
        });
        self.fan_duty.lock(|d| d.set(duty));
    }

    fn get_fan_on(&self) -> bool {
        self.fan_on.load(Ordering::Relaxed)
    }

    fn set_fan_on(&self, on: bool) {
        self.fan_on.store(on, Ordering::Relaxed);
        let duty = self.fan_duty.lock(|d| d.get());
        let effective = if on { duty } else { 0 };
        self.fan_channel.lock(|ch| {
            let _ = ch.set_duty(effective);
        });
    }

    fn get_fan_rpm(&self) -> i16 {
        FAN_RPM.load(Ordering::Relaxed)
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

        // --- SHT41 via Qwiic (LP_I2C: GPIO6/GPIO7) ---
        let i2c = I2c::new(
                peripherals.I2C0,
                I2cConfig::default().with_frequency(esp_hal::time::Rate::from_khz(100))
            )
            .expect("Cannot access I2C")
            .with_sda(peripherals.GPIO6)
            .with_scl(peripherals.GPIO7)
            .into_async();

        let sht = mk_static!(
            SHT4x<I2c<'static, Async>>,
            SHT4x::new(i2c, Default::default())
        );

        let sensor = mk_static!(
            Mutex<CriticalSectionRawMutex, &'static mut SHT4x<I2c<'static, Async>>>,
            Mutex::new(sht)
        );

        // --- Internal die temperature sensor ---
        let die_sensor = mk_static!(
            TemperatureSensor<'static>,
            TemperatureSensor::new(peripherals.TSENS, TsensConfig::default())
                .expect("Cannot access the internal temperature sensor")
        );

        // --- Fan PWM via LEDC (25 kHz, 10-bit duty) ---
        let ledc = Ledc::new(peripherals.LEDC);
        let lstimer0 = mk_static!(
            esp_hal::ledc::timer::Timer<'static, LowSpeed>,
            ledc.timer::<LowSpeed>(timer::Number::Timer0)
        );
        lstimer0
            .configure(timer::config::Config {
                duty: timer::config::Duty::Duty10Bit,
                clock_source: LSClockSource::APBClk,
                frequency: esp_hal::time::Rate::from_khz(25),
            })
            .unwrap();

        let mut fan_channel = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
        fan_channel
            .configure(channel::config::Config {
                timer: lstimer0,
                duty_pct: 100,
                drive_mode: esp_hal::gpio::DriveMode::PushPull,
            })
            .unwrap();

        let fan_channel = mk_static!(
            CriticalSectionMutex<esp_hal::ledc::channel::Channel<'static, LowSpeed>>,
            CriticalSectionMutex::new(fan_channel)
        );

        // --- Fan tach via PCNT (GPIO3, internal pull-up) ---
        let tach_pin = Input::new(peripherals.GPIO3, InputConfig::default().with_pull(Pull::Up));
        let tach_signal = tach_pin.peripheral_input();

        let pcnt = mk_static!(Pcnt<'static>, Pcnt::new(peripherals.PCNT));
        let unit_ref: &'static esp_hal::pcnt::unit::Unit<'static, 0> = &pcnt.unit0;
        let ch0 = &unit_ref.channel0;
        ch0.set_edge_signal(tach_signal);
        ch0.set_ctrl_mode(CtrlMode::Keep, CtrlMode::Keep);
        ch0.set_input_mode(EdgeMode::Increment, EdgeMode::Hold);
        let _ = unit_ref.set_filter(Some(10));
        unit_ref.resume();

        // --- State ---
        let fan_on = mk_static!(AtomicBool, AtomicBool::new(true));
        let fan_duty = mk_static!(
            CriticalSectionMutex<core::cell::Cell<u8>>,
            CriticalSectionMutex::new(core::cell::Cell::new(100))
        );

        let app_state = mk_static!(
            AppState,
            AppState {
                sensor,
                die_sensor,
                fan_channel,
                fan_on,
                fan_duty,
                td: mk_static!(TdCell, TdCell::new()),
            }
        );

        spawner.spawn(tach_sample_task(unit_ref).expect("tach_sample_task"));
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
    const NAME: &'static str = "fan";

    // Maximum power-save breaks WiFi on ESP32-C6 (esp-rs/esp-hal#3014, #3075, #3079).
    const WIFI_POWER_SAVE: PowerSaveMode = PowerSaveMode::None;

    fn build_td(name: &str, base_uri: String, id: String) -> Thing {
        Thing::builder(name)
            .finish_extend()
            .id(id)
            .base(base_uri)
            .description("Noctua 5V fan controller with SHT41 sensor")
            .security(|builder| builder.no_sec().required().with_key("nosec_sc"))
            .property("temperature", |p| {
                p.finish_extend_data_schema()
                    .attype("TemperatureProperty")
                    .title("Temperature")
                    .description("Ambient temperature from SHT41")
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
                    .description("Relative humidity from SHT41")
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
                    .description("ESP32-C6 internal die temperature")
                    .form(|f| {
                        f.href("/properties/die_temperature")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                    })
                    .number()
                    .read_only()
                    .unit("Celsius")
            })
            .property("on", |p| {
                p.finish_extend_data_schema()
                    .attype("OnOffProperty")
                    .title("Fan on/off")
                    .description("Whether the fan is running")
                    .form(|f| {
                        f.href("/properties/on")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                            .op(wot_td::thing::FormOperation::WriteProperty)
                    })
                    .bool()
            })
            .property("speed", |p| {
                p.finish_extend_data_schema()
                    .attype("LevelProperty")
                    .title("Fan speed")
                    .description("Fan PWM duty cycle (0-100%)")
                    .form(|f| {
                        f.href("/properties/speed")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                            .op(wot_td::thing::FormOperation::WriteProperty)
                    })
                    .integer()
                    .minimum(0)
                    .maximum(100)
                    .unit("percent")
            })
            .property("rpm", |p| {
                p.finish_extend_data_schema()
                    .attype("SpeedProperty")
                    .title("Fan RPM")
                    .description("Measured fan speed in revolutions per minute")
                    .form(|f| {
                        f.href("/properties/rpm")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                    })
                    .integer()
                    .read_only()
                    .unit("rpm")
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
            .event("fan_rpm", |b| {
                b.data(|b| b.finish_extend().integer().unit("rpm"))
                    .form(|form_builder| {
                        form_builder
                            .href("/events/fan_rpm")
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
        td_routes::<AppState>()
            .route(
                "/properties/temperature",
                get(async move |State(state): State<AppState>| {
                    to_json_result(
                        state.get_temperature().await,
                        "Failed to read temperature",
                    )
                }),
            )
            .route(
                "/properties/humidity",
                get(async move |State(state): State<AppState>| {
                    to_json_result(state.get_humidity().await, "Failed to read humidity")
                }),
            )
            .route(
                "/properties/die_temperature",
                get(async move |State(state): State<AppState>| {
                    to_json_response(&state.get_die_temperature())
                }),
            )
            .route(
                "/properties/on",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.get_fan_on())
                })
                .put(
                    |State(state): State<AppState>,
                     picoserve::extract::Json::<_>(on)| async move {
                        state.set_fan_on(on);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/properties/speed",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.get_fan_speed())
                })
                .put(
                    |State(state): State<AppState>,
                     picoserve::extract::Json::<_>(speed)| async move {
                        state.set_fan_speed(speed);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/properties/rpm",
                get(async move |State(state): State<AppState>| {
                    to_json_response(&state.get_fan_rpm())
                }),
            )
            .route(
                "/events/temperature",
                get(async move || response::EventStream(SseEvents(WATCH.receiver().unwrap()))),
            )
            .route(
                "/events/fan_rpm",
                get(async move || response::EventStream(SseEvents(RPM_WATCH.receiver().unwrap()))),
            )
    }
}

static WATCH: Watch<CriticalSectionRawMutex, f32, 2> = Watch::new();
static RPM_WATCH: Watch<CriticalSectionRawMutex, i16, 2> = Watch::new();

#[embassy_executor::task]
async fn tach_sample_task(unit: &'static esp_hal::pcnt::unit::Unit<'static, 0>) -> ! {
    let sender = RPM_WATCH.sender();
    let mut last_rpm: i16 = 0;
    loop {
        Timer::after(Duration::from_secs(1)).await;
        let count = unit.value();
        unit.clear();
        let rpm = (count as i32 * 60) / 2;
        let rpm_i16 = rpm as i16;
        FAN_RPM.store(rpm_i16, Ordering::Relaxed);
        if ((rpm_i16 - last_rpm).unsigned_abs() / 10) != 0 {
            sender.send(rpm_i16);
            last_rpm = rpm_i16;
        }
    }
}

#[embassy_executor::task]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
async fn temperature_write_task(state: &'static AppState) -> ! {
    let sender = WATCH.sender();
    let mut last_temp = state.get_temperature().await.unwrap_or(-500.0);

    loop {
        Timer::after(Duration::from_secs(1)).await;

        if let Ok(temp) = state.get_temperature().await {
            if ((last_temp - temp) * 100f32) as u32 / 10 != 0 {
                sender.send(temp);
                last_temp = temp;
            }
        }
    }
}

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
