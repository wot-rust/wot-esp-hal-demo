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
| `on` | boolean | R/W | Fan enable/disable |
| `speed` | integer 0–100 | R/W | Fan PWM duty cycle (%) |
| `rpm` | integer | R | Measured fan speed (RPM) |

**Events:** `temperature` (SSE), `fan_rpm` (SSE)

```
$ cargo run --bin fan --target riscv32imac-unknown-none-elf
```

### Bill of materials (fan controller)

| Component | Part | Qty | Notes |
|---|---|---|---|
| MCU board | SparkFun Qwiic Pocket Dev Board - ESP32-C6 | 1 | ESP32-C6-MINI-1, WiFi 6 + BT 5 |
| Temp/humidity sensor | Sensirion SHT41 (Qwiic breakout) | 1 | Connected via Qwiic connector |
| Fan | Noctua NF-A20 5V PWM (200mm) | 1 | Any 5V 4-pin PWM Noctua fan works |

### Wiring (fan controller)

```
ESP32-C6 board          Fan            SHT41
─────────────          ────           ─────
GPIO2  ─────────────── PWM (blue)
GPIO3  ─────────────── Tach (green)   [internal pull-up, no external resistor]
GND    ─────────────── GND (black)
V_USB  ─────────────── +5V (yellow)
Qwiic  ─────────────────────────────  SDA/SCL/VCC/GND
```

**Notes:**
- Fan power comes from `V_USB` (5V), **not** `3V3`.
- The tach line uses the ESP32-C6's internal pull-up (~45kΩ). The Noctua tach
  is open-collector, 2 pulses per revolution. No external resistor needed.
- The PWM control line works at 3.3V — no level shifter required for 5V
  Noctua fans (confirmed by Noctua).
- The SHT41 connects via the Qwiic connector (GPIO6 SDA / GPIO7 SCL).

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
