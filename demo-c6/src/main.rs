#![no_std]
#![no_main]
#![recursion_limit = "1024"]
#![feature(impl_trait_in_assoc_type)]

extern crate alloc;

use alloc::{format, string::String};
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
    pcnt::Pcnt,
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
use serde_json::Value;
use wot_esp_thing::{
    mk_static, td_routes, to_json_response, to_json_result, EspThing as _, PowerSaveMode,
    SseEvents, TdCell, TdState,
};
use wot_td::{
    builder::{
        BuildableDataSchema, BuildableHumanReadableInfo, BuildableInteractionAffordance,
        IntegerDataSchemaBuilderLike, ReadableWriteableDataSchema, SpecializableDataSchema,
        UnionDataSchema, VecDataSchemaBuilderLike,
    },
    Thing,
};

/// Number of fans exposed by this firmware.
///
/// The `dual-fan` Cargo feature compiles in a second 4-pin PWM fan on
/// GPIO4 (PWM) / GPIO5 (tach). Without it the sketch behaves exactly like the
/// single-fan version.
#[cfg(feature = "dual-fan")]
const FAN_COUNT: u32 = 2;
#[cfg(not(feature = "dual-fan"))]
const FAN_COUNT: u32 = 1;

/// Per-fan control state.
///
/// `rpm` is written by the per-fan tach sample task and read by HTTP handlers
/// without a lock. `channel`, `on` and `duty` are guarded because they are
/// touched from both HTTP handlers and the BOOT button task.
struct FanCtrl {
    channel: CriticalSectionMutex<esp_hal::ledc::channel::Channel<'static, LowSpeed>>,
    on: AtomicBool,
    duty: CriticalSectionMutex<core::cell::Cell<u8>>,
    rpm: AtomicI16,
}

impl FanCtrl {
    fn new(
        channel: esp_hal::ledc::channel::Channel<'static, LowSpeed>,
        initial_duty: u8,
        initial_on: bool,
    ) -> Self {
        Self {
            channel: CriticalSectionMutex::new(channel),
            on: AtomicBool::new(initial_on),
            duty: CriticalSectionMutex::new(core::cell::Cell::new(initial_duty)),
            rpm: AtomicI16::new(0),
        }
    }

    fn get_on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    /// Apply a new on/off state to the hardware.
    ///
    /// Returns the *previous* value so callers can detect changes and emit
    /// watch events without re-reading shared state.
    fn set_on(&self, on: bool) -> bool {
        let was = self.on.swap(on, Ordering::AcqRel);
        let duty = self.duty.lock(|d| d.get());
        let effective = if on { duty } else { 0 };
        self.channel.lock(|ch| {
            let _ = ch.set_duty(effective);
        });
        was
    }

    fn get_speed(&self) -> u8 {
        self.duty.lock(|d| d.get())
    }

    fn set_speed(&self, duty: u8) {
        let on = self.on.load(Ordering::Relaxed);
        let effective = if on { duty } else { 0 };
        self.channel.lock(|ch| {
            let _ = ch.set_duty(effective);
        });
        self.duty.lock(|d| d.set(duty));
    }

    fn get_rpm(&self) -> i16 {
        self.rpm.load(Ordering::Relaxed)
    }

    fn set_rpm(&self, rpm: i16) {
        self.rpm.store(rpm, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
struct AppState {
    sensor: &'static Mutex<
        CriticalSectionRawMutex,
        &'static mut SHT4x<I2c<'static, Async>>,
    >,
    die_sensor: &'static TemperatureSensor<'static>,
    /// Fan controllers — length 1 by default, 2 with the `dual-fan` feature.
    fans: &'static [&'static FanCtrl],
    td: &'static TdCell,
}

impl AppState {
    async fn get_temperature(
        &self,
    ) -> Result<f32, sht4x_rjw::error::Error<esp_hal::i2c::master::Error>> {
        let mut sensor = self.sensor.lock().await;
        let m = sensor.measure(embassy_time::Delay).await?;
        Ok(m.celsius())
    }

    async fn get_humidity(
        &self,
    ) -> Result<f32, sht4x_rjw::error::Error<esp_hal::i2c::master::Error>> {
        let mut sensor = self.sensor.lock().await;
        let m = sensor.measure(embassy_time::Delay).await?;
        Ok(m.humidity())
    }

    fn get_die_temperature(&self) -> f32 {
        self.die_sensor.get_temperature().to_celsius()
    }

    fn fan_count(&self) -> u32 {
        self.fans.len() as u32
    }

    // --- on (scalar-or-array, read/write) ---

    fn get_fan_on(&self) -> Value {
        scalar_or_array(self.fans, FanCtrl::get_on)
    }

    /// Apply a scalar (broadcast to every fan) or array (per-index) on/off value.
    ///
    /// Returns `Err(first_out_of_range_index)` if an array element targets a
    /// fan that does not exist, or `Err(0)` if the payload is neither a boolean
    /// nor an array of booleans.
    fn set_fan_on(&self, value: Value) -> Result<(), usize> {
        let mut changed = false;
        match value {
            Value::Bool(b) => {
                for fan in self.fans {
                    if fan.set_on(b) != b {
                        changed = true;
                    }
                }
            }
            Value::Array(items) => {
                for (i, item) in items.into_iter().enumerate() {
                    let Some(fan) = self.fans.get(i) else {
                        return Err(i);
                    };
                    match item {
                        Value::Bool(b) => {
                            if fan.set_on(b) != b {
                                changed = true;
                            }
                        }
                        _ => return Err(i),
                    }
                }
            }
            _ => return Err(0),
        }
        if changed {
            ON_WATCH.sender().send(self.on_state_json());
        }
        Ok(())
    }

    fn on_state_json(&self) -> String {
        serde_json::to_string(&self.get_fan_on()).unwrap()
    }

    // --- speed (scalar-or-array, read/write) ---

    fn get_fan_speed(&self) -> Value {
        scalar_or_array(self.fans, FanCtrl::get_speed)
    }

    /// Apply a scalar (broadcast) or array (per-index) PWM duty cycle.
    fn set_fan_speed(&self, value: Value) -> Result<(), usize> {
        match value {
            Value::Number(n) => match number_as_u8(&n) {
                Some(d) => {
                    for fan in self.fans {
                        fan.set_speed(d);
                    }
                }
                None => return Err(0),
            },
            Value::Array(items) => {
                for (i, item) in items.into_iter().enumerate() {
                    let Some(fan) = self.fans.get(i) else {
                        return Err(i);
                    };
                    match item {
                        Value::Number(n) => match number_as_u8(&n) {
                            Some(d) => fan.set_speed(d),
                            None => return Err(i),
                        },
                        _ => return Err(i),
                    }
                }
            }
            _ => return Err(0),
        }
        Ok(())
    }

    // --- rpm (scalar-or-array, read-only) ---

    fn get_fan_rpm(&self) -> Value {
        scalar_or_array(self.fans, FanCtrl::get_rpm)
    }

    fn rpm_state_json(&self) -> String {
        serde_json::to_string(&self.get_fan_rpm()).unwrap()
    }

    /// Toggle every fan (BOOT button). Notifies [`ON_WATCH`] if any changed.
    fn toggle_fans(&self) {
        let mut changed = false;
        for fan in self.fans {
            let new = !fan.get_on();
            if fan.set_on(new) != new {
                changed = true;
            }
        }
        if changed {
            ON_WATCH.sender().send(self.on_state_json());
        }
    }
}

/// Build a JSON value that is a scalar when exactly one fan is present, or a
/// JSON array otherwise — matching the `oneOf` shape advertised by the TD.
fn scalar_or_array<T: Copy + Into<Value>>(fans: &[&FanCtrl], get: fn(&FanCtrl) -> T) -> Value {
    if fans.len() == 1 {
        get(fans[0]).into()
    } else {
        Value::Array(fans.iter().map(|f| get(f).into()).collect())
    }
}

/// Decode a JSON number into a `u8`, rejecting non-integers or out-of-range values.
fn number_as_u8(n: &serde_json::Number) -> Option<u8> {
    n.as_u64().and_then(|v| u8::try_from(v).ok())
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
        // All fans share lstimer0; each fan gets its own LEDC channel.
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

        // --- Fan tach via PCNT (shared by all fans) ---
        let pcnt = mk_static!(Pcnt<'static>, Pcnt::new(peripherals.PCNT));

        // Fan 0: PWM on GPIO2 (LEDC channel 0), tach on GPIO3 (PCNT unit 0).
        let mut fan0_channel = ledc.channel(channel::Number::Channel0, peripherals.GPIO2);
        fan0_channel
            .configure(channel::config::Config {
                timer: lstimer0,
                duty_pct: 100,
                drive_mode: esp_hal::gpio::DriveMode::PushPull,
            })
            .unwrap();
        setup_tach_unit(
            &pcnt.unit0,
            Input::new(peripherals.GPIO3, InputConfig::default().with_pull(Pull::Up)),
        );

        let fan0 = mk_static!(FanCtrl, FanCtrl::new(fan0_channel, 100, true));

        // Fan 1 (only with `dual-fan`): PWM on GPIO4 (channel 1), tach on GPIO5 (unit 1).
        #[cfg(feature = "dual-fan")]
        let fan1 = {
            let mut fan1_channel = ledc.channel(channel::Number::Channel1, peripherals.GPIO4);
            fan1_channel
                .configure(channel::config::Config {
                    timer: lstimer0,
                    duty_pct: 100,
                    drive_mode: esp_hal::gpio::DriveMode::PushPull,
                })
                .unwrap();
            setup_tach_unit(
                &pcnt.unit1,
                Input::new(peripherals.GPIO5, InputConfig::default().with_pull(Pull::Up)),
            );
            mk_static!(FanCtrl, FanCtrl::new(fan1_channel, 100, true))
        };

        // Assemble the fan slice — length 1 by default, 2 with `dual-fan`.
        #[cfg(feature = "dual-fan")]
        let fans_arr = mk_static!([&'static FanCtrl; 2], [fan0, fan1]);
        #[cfg(not(feature = "dual-fan"))]
        let fans_arr = mk_static!([&'static FanCtrl; 1], [fan0]);
        let fans: &'static [&'static FanCtrl] = fans_arr;

        // --- State ---
        let app_state = mk_static!(
            AppState,
            AppState {
                sensor,
                die_sensor,
                fans,
                td: mk_static!(TdCell, TdCell::new()),
            }
        );

        // --- BOOT button (GPIO9) toggles every fan ---
        let btn = Input::new(
            peripherals.GPIO9,
            InputConfig::default().with_pull(Pull::Up),
        );

        spawner
            .spawn(tach0_sample_task(app_state, &pcnt.unit0).expect("tach0_sample_task"));
        #[cfg(feature = "dual-fan")]
        spawner
            .spawn(tach1_sample_task(app_state, &pcnt.unit1).expect("tach1_sample_task"));
        spawner
            .spawn(temperature_write_task(app_state).expect("temperature_write_task"));
        spawner.spawn(button_task(app_state, btn).expect("button_task"));

        (app_state, net)
    }

    fn set_td(&self, td: &'static str) {
        self.td.set(td);
    }
}

/// Wire up a PCNT unit for a Noctua-style open-collector tach (2 pulses/rev):
/// increment on every edge, no control mode, light debounce filter.
fn setup_tach_unit<const N: usize>(
    unit: &esp_hal::pcnt::unit::Unit<'_, N>,
    tach_pin: Input<'static>,
) {
    use esp_hal::pcnt::channel::{CtrlMode, EdgeMode};
    let tach_signal = tach_pin.peripheral_input();
    let ch0 = &unit.channel0;
    ch0.set_edge_signal(tach_signal);
    ch0.set_ctrl_mode(CtrlMode::Keep, CtrlMode::Keep);
    ch0.set_input_mode(EdgeMode::Increment, EdgeMode::Hold);
    let _ = unit.set_filter(Some(10));
    unit.resume();
    // `tach_pin` stays configured as input; its GPIO matrix settings persist
    // after the owned `Input` drops.
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
            .description(concat!(
                "Noctua 5V PWM fan controller with SHT41 sensor. ",
                "Properties `on`/`speed`/`rpm` are scalar-or-array: a scalar ",
                "broadcasts to every fan, an array targets fans by index. BOOT ",
                "toggles every fan."
            ))
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
            .property("fans", |p| {
                p.finish_extend_data_schema()
                    .attype("LevelProperty")
                    .title("Fan count")
                    .description("Number of fans exposed by this device (compile-time)")
                    .form(|f| {
                        f.href("/properties/fans")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                    })
                    .integer()
                    .read_only()
                    .minimum(1)
                    .maximum(i64::from(FAN_COUNT))
            })
            .property("on", |p| {
                p.finish_extend_data_schema()
                    .attype("OnOffProperty")
                    .title("Fan on/off")
                    .description(
                        "Per-fan enable. A scalar broadcasts to every fan; \
                         an array sets fans by index (out-of-range index → HTTP 400).",
                    )
                    .form(|f| {
                        f.href("/properties/on")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                            .op(wot_td::thing::FormOperation::WriteProperty)
                    })
                    .one_of(|b| b.finish_extend().bool())
                    .one_of(|b| {
                        b.finish_extend()
                            .vec()
                            .min_items(1)
                            .max_items(FAN_COUNT)
                            .set_item(|b| b.finish_extend().bool())
                    })
            })
            .property("speed", |p| {
                p.finish_extend_data_schema()
                    .attype("LevelProperty")
                    .title("Fan speed")
                    .description("Per-fan PWM duty cycle (0-100%): scalar broadcasts, array is per-index")
                    .form(|f| {
                        f.href("/properties/speed")
                            .op(wot_td::thing::FormOperation::ReadProperty)
                            .op(wot_td::thing::FormOperation::WriteProperty)
                    })
                    .one_of(|b| b.finish_extend().integer().minimum(0).maximum(100).unit("percent"))
                    .one_of(|b| {
                        b.finish_extend()
                            .vec()
                            .min_items(1)
                            .max_items(FAN_COUNT)
                            .set_item(|b| b.finish_extend().integer().minimum(0).maximum(100).unit("percent"))
                    })
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
                    .one_of(|b| b.finish_extend().integer().read_only().unit("rpm"))
                    .one_of(|b| {
                        b.finish_extend()
                            .vec()
                            .min_items(1)
                            .max_items(FAN_COUNT)
                            .set_item(|b| b.finish_extend().integer().read_only().unit("rpm"))
                    })
            })
            .event("on", |b| {
                b.data(|b| {
                    b.finish_extend()
                        .one_of(|b| b.finish_extend().bool())
                        .one_of(|b| {
                            b.finish_extend()
                                .vec()
                                .min_items(1)
                                .max_items(FAN_COUNT)
                                .set_item(|b| b.finish_extend().bool())
                        })
                })
                .form(|form_builder| {
                    form_builder
                        .href("/events/on")
                        .op(wot_td::thing::FormOperation::SubscribeEvent)
                        .op(wot_td::thing::FormOperation::UnsubscribeEvent)
                        .subprotocol("sse")
                })
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
            .event("rpm", |b| {
                b.data(|b| {
                    b.finish_extend()
                        .one_of(|b| b.finish_extend().integer().unit("rpm"))
                        .one_of(|b| {
                            b.finish_extend()
                                .vec()
                                .min_items(1)
                                .max_items(FAN_COUNT)
                                .set_item(|b| b.finish_extend().integer().unit("rpm"))
                        })
                })
                .form(|form_builder| {
                    form_builder
                        .href("/events/rpm")
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
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.get_die_temperature())
                }),
            )
            .route(
                "/properties/fans",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.fan_count())
                }),
            )
            .route(
                "/properties/on",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.get_fan_on())
                })
                .put(
                    |State(state): State<AppState>,
                     picoserve::extract::Json::<Value>(value)| async move {
                        state.set_fan_on(value).map(|()| StatusCode::NO_CONTENT).map_err(|i| {
                            (
                                StatusCode::BAD_REQUEST,
                                format!(
                                    "fan index {i} out of range ({} fan(s) configured)",
                                    state.fan_count()
                                ),
                            )
                        })
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
                     picoserve::extract::Json::<Value>(value)| async move {
                        state.set_fan_speed(value).map(|()| StatusCode::NO_CONTENT).map_err(|i| {
                            (
                                StatusCode::BAD_REQUEST,
                                format!(
                                    "fan index {i} out of range ({} fan(s) configured)",
                                    state.fan_count()
                                ),
                            )
                        })
                    },
                ),
            )
            .route(
                "/properties/rpm",
                get(|State(state): State<AppState>| async move {
                    to_json_response(&state.get_fan_rpm())
                }),
            )
            .route(
                "/events/on",
                get(
                    async move || response::EventStream(SseEvents(ON_WATCH.receiver().unwrap())),
                ),
            )
            .route(
                "/events/temperature",
                get(async move || response::EventStream(SseEvents(WATCH.receiver().unwrap()))),
            )
            .route(
                "/events/rpm",
                get(async move || response::EventStream(SseEvents(RPM_WATCH.receiver().unwrap()))),
            )
    }
}

// SSE watches. `on` and `rpm` carry pre-serialized JSON strings so the same
// event payload matches the scalar-or-array shape of the property: `true` for
// a single fan, `[true,false]` for two.
static WATCH: Watch<CriticalSectionRawMutex, f32, 2> = Watch::new();
static RPM_WATCH: Watch<CriticalSectionRawMutex, String, 2> = Watch::new();
static ON_WATCH: Watch<CriticalSectionRawMutex, String, 2> = Watch::new();

/// Per-fan tach sampling loop. Each fan has its own task so they never share a
/// PCNT unit; on a value change it broadcasts the full rpm snapshot (scalar if
/// one fan, array otherwise).
async fn tach_loop<const N: usize>(
    unit: &esp_hal::pcnt::unit::Unit<'_, N>,
    state: &'static AppState,
    fan_index: usize,
) -> ! {
    let sender = RPM_WATCH.sender();
    let mut last_rpm: i16 = 0;
    loop {
        Timer::after(Duration::from_secs(1)).await;
        let count = unit.value();
        unit.clear();
        // Noctua tach is open-collector, 2 pulses per revolution.
        let rpm = (count as i32 * 60) / 2;
        let rpm_i16 = rpm as i16;
        state.fans[fan_index].set_rpm(rpm_i16);
        if (rpm_i16 - last_rpm).unsigned_abs() / 10 != 0 {
            sender.send(state.rpm_state_json());
            last_rpm = rpm_i16;
        }
    }
}

#[embassy_executor::task]
async fn tach0_sample_task(
    state: &'static AppState,
    unit: &'static esp_hal::pcnt::unit::Unit<'static, 0>,
) -> ! {
    tach_loop(unit, state, 0).await
}

#[cfg(feature = "dual-fan")]
#[embassy_executor::task]
async fn tach1_sample_task(
    state: &'static AppState,
    unit: &'static esp_hal::pcnt::unit::Unit<'static, 1>,
) -> ! {
    tach_loop(unit, state, 1).await
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

/// BOOT button (active-low) toggles every fan's on state via [`AppState::toggle_fans`].
#[embassy_executor::task]
async fn button_task(state: &'static AppState, mut btn: Input<'static>) -> ! {
    loop {
        btn.wait_for_low().await;
        state.toggle_fans();
        btn.wait_for_high().await;
    }
}

esp_bootloader_esp_idf::esp_app_desc!();

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    AppProps::run(spawner).await;
}
