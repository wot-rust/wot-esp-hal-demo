# wot-esp-hal-demo

[![dependency status](https://deps.rs/repo/github/wot-rust/wot-esp-hal-demo/status.svg)](https://deps.rs/repo/github/wot-rust/wot-esp-hal-demo)
[![LICENSES][license badge apache]][license apache]
[![LICENSES][license badge mit]][license mit]

Web of Things demos for Espressif chips, built with `esp-hal`, `esp-radio`,
`embassy`, `picoserve`, and `wot-td`.

Each demo exposes a physical device as a [W3C Web Thing](https://www.w3.org/WoT/)
— serving a Thing Description over HTTP with readable/writable properties,
Server-Sent Events, and mDNS discovery.

## Project structure

This is a Cargo workspace:

```
lib/           # wot-esp-thing: shared infrastructure (WiFi, embassy-net, HTTP,
               #   mDNS, SSE, TD-serving, EspThing trait) — chip-agnostic
demo-c3/  # ESP32-C3 demos (thermometer, light, button)
demo-c6/  # ESP32-C6 demo (fan controller)
```

## Deploy

### Rust prerequisites

- Install `espflash` and `ldproxy`:
```
$ cargo install espflash ldproxy
```

### Building and running

Each binary targets a specific chip and architecture. Set `SSID` and
`PASSWORD` environment variables for your WiFi network.

The simplest way is via the xtask, which handles target selection and flashing:

```
$ cargo xtask list                                      # list available demos
$ SSID=<wifi> PASSWORD=<pass> cargo xtask check-all     # typecheck every demo
$ SSID=<wifi> PASSWORD=<pass> cargo xtask build fan     # build
$ SSID=<wifi> PASSWORD=<pass> cargo xtask run fan       # build + flash + monitor
$ SSID=<wifi> PASSWORD=<pass> cargo xtask run fan --port /dev/cu.usbmodem101
```

You can also use plain cargo if you prefer (note the `--target` and `-Z build-std` flags):

```
$ SSID=<wifi> PASSWORD=<pass> cargo run --bin thermometer --target riscv32imc-unknown-none-elf -Z build-std=alloc,core
$ SSID=<wifi> PASSWORD=<pass> cargo run --bin fan        --target riscv32imac-unknown-none-elf -Z build-std=alloc,core
```

Once running, each demo advertises itself via mDNS as `_wot._tcp` and serves
its Thing Description at `http://<ip>/`.

## ESP32-C3 demos

All target the [esp-rust-board](https://github.com/esp-rs/esp-rust-board)
(ESP32-C3-DevKitM-1).

### Hygro-Thermometer

Exposes the [SHTC3](https://www.sensirion.com/shtc3/) sensor plus the ESP32-C3
internal die temperature sensor.

**Properties:** `temperature`, `humidity`, `die_temperature` (read-only)
**Events:** `temperature` (SSE)

```
$ cargo run --bin thermometer --target riscv32imc-unknown-none-elf
```

### Light Source

Exposes the on-board WS2812 RGB LED as a dimmable color light.

**Properties:** `on` (R/W), `brightness` 0–255 (R/W), `color` RGB object (R/W)

```
$ cargo run --bin light --target riscv32imc-unknown-none-elf
```

### Button

Exposes the on-board BOOT button via Server-Sent Events.

**Properties:** `on` (read-only)
**Events:** `on` (SSE)

```
$ cargo run --bin button --target riscv32imc-unknown-none-elf
```

## ESP32-C6 demo

Targets the [SparkFun Qwiic Pocket Dev Board - ESP32-C6](https://www.sparkfun.com/sparkfun-qwiic-pocket-development-board-esp32-c6.html).

### Fan Controller

Controls a 5V Noctua 4-pin PWM fan and measures its speed, with ambient
temperature/humidity from an SHT41 sensor via Qwiic.

**Properties:**

| Property | Type | R/W | Description |
|---|---|---|---|
| `temperature` | number | R | Ambient temperature from SHT41 (°C) |
| `humidity` | number | R | Relative humidity from SHT41 (%) |
| `die_temperature` | number | R | ESP32-C6 internal die temperature (°C) |
| `fans` | integer | R | Number of fans exposed (1, or 2 with `dual-fan`) |
| `on` | boolean **or** array of booleans | R/W | Per-fan enable. Scalar broadcasts to every fan; array sets fans by index (out-of-range index → HTTP 400). Also toggled for every fan by **BOOT** |
| `speed` | integer 0–100 **or** array thereof | R/W | Per-fan PWM duty cycle (%). Same scalar/array semantics as `on` |
| `rpm` | integer **or** array thereof | R | Measured fan speed (RPM). Scalar when one fan, array when more |

With a single fan, reads return scalars (e.g. `true`, `75`, `1234`) and writes
accept either a scalar or a one-element array. With `dual-fan`, reads return
arrays (e.g. `[true, false]`, `[75, 100]`) and an array write targets fans by
position — `[true, false]` enables fan 0 and disables fan 1. A scalar write
applies to every fan.

**Events:** `on` (SSE, scalar-or-array like the property), `temperature` (SSE), `rpm` (SSE, scalar-or-array)

The board **BOOT** button toggles every fan's `on` state (each fan flips
independently; same effect as `PUT /properties/on` with the inverted value per
fan). Subscribers on `/events/on` get `value_changed` for button presses and
HTTP writes alike.

```
$ cargo run --bin fan --target riscv32imac-unknown-none-elf
```

### Bill of materials (fan controller)

| Component | Part | Qty | Notes |
|---|---|---|---|
| MCU board | SparkFun Qwiic Pocket Dev Board - ESP32-C6 | 1 | ESP32-C6-MINI-1, WiFi 6 + BT 5 |
| Temp/humidity sensor | Sensirion SHT41 (Qwiic breakout) | 1 | Connected via Qwiic connector |
| Fan | Noctua NF-A20 5V PWM (200mm) | 1–2 | Any 5V 4-pin PWM Noctua fan works. Two requires the `dual-fan` feature |

### Wiring (fan controller)

Single fan (default build):

```
ESP32-C6 board          Fan            SHT41
─────────────          ────           ─────
GPIO2  ─────────────── PWM (blue)
GPIO3  ─────────────── Tach (green)   [internal pull-up, no external resistor]
GND    ─────────────── GND (black)
V_USB  ─────────────── +5V (yellow)
Qwiic  ─────────────────────────────  SDA/SCL/VCC/GND
BOOT   ── (on-board)  toggles every fan `on`
```

Second fan (build with `--features dual-fan`):

```
ESP32-C6 board          Fan 2
─────────────          ──────
GPIO4  ─────────────── PWM (blue)
GPIO5  ─────────────── Tach (green)   [internal pull-up]
GND    ─────────────── GND (black)
V_USB  ─────────────── +5V (yellow)
```

Both fans share the LEDC timer (`lstimer0`) but use separate channels
(channel 0 / channel 1) and separate PCNT units (unit 0 / unit 1).

**Notes:**
- Fan power comes from `V_USB` (5V), **not** `3V3`.
- The tach line uses the ESP32-C6's internal pull-up (~45kΩ). The Noctua tach
  is open-collector, 2 pulses per revolution. No external resistor needed.
- The PWM control line works at 3.3V — no level shifter required for 5V
  Noctua fans (confirmed by Noctua).
- The SHT41 connects via the Qwiic connector (GPIO6 SDA / GPIO7 SCL).
- The on-board **BOOT** button is GPIO9 (active-low, internal pull-up) and
  toggles every fan's `on` without extra wiring.

### Dual-fan build

The `dual-fan` Cargo feature compiles in support for a second 4-pin PWM fan.
Without it the firmware is byte-for-byte the single-fan sketch. With it, the
`on` / `speed` / `rpm` properties become scalar-or-array (see the table above)
and a read-only `fans` property reports the count.

```
$ cargo xtask build fan --features dual-fan
$ SSID=<wifi> PASSWORD=<pass> cargo xtask run fan --features dual-fan
```

Or with plain cargo:

```
$ cargo run --bin fan --target riscv32imac-unknown-none-elf \
    --features dual-fan -Z build-std=alloc,core
```

### ESP32-C6 WiFi note

The ESP32-C6 WiFi 6 radio does not work reliably with power-saving modes.
The C6 fan demo overrides `EspThing::WIFI_POWER_SAVE` to `PowerSaveMode::None`
(C3 demos keep the default `Maximum`). The workspace also sets
`ESP_RADIO_CONFIG_PHY_ENABLE_USB=false` for stable connectivity
([esp-hal#3014](https://github.com/esp-rs/esp-hal/issues/3014)).

<!-- Links -->
[license apache]: LICENSES/Apache-2.0.txt
[license mit]: LICENSES/MIT.txt

<!-- Badges -->
[license badge apache]: https://img.shields.io/badge/license-Apache_2.0-blue.svg
[license badge mit]: https://img.shields.io/badge/license-MIT-blue.svg
