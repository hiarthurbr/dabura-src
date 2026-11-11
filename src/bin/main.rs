#![no_std]
#![no_main]
#![deny(
    clippy::mem_forget,
    reason = "mem::forget is generally not safe to do with esp_hal types."
)]
#![deny(clippy::pedantic)]
#![deny(clippy::large_stack_frames)]

use crate::alloc::string::ToString;
use alloc::format;
use alloc::string::String;
use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::analog::adc::{Adc, AdcConfig, Attenuation};
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::ControllerConfig;

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

enum IrrigationState {
    Idle,
    WaitingForRain,
    Irrigating,
}

#[allow(clippy::large_stack_frames)]
#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let config = esp_hal::Config::default().with_cpu_clock(CpuClock::max());

    let peripherals = esp_hal::init(config);

    #[allow(
        clippy::drop_non_drop,
        reason = "A intenção é inutilizar esses pinos através do compilador"
    )]
    {
        drop(peripherals.GPIO6);
        drop(peripherals.GPIO7);
        drop(peripherals.GPIO8);
        drop(peripherals.GPIO9);
        drop(peripherals.GPIO10);
        drop(peripherals.GPIO11);
        drop(peripherals.GPIO16);
        drop(peripherals.GPIO20);
    }

    esp_alloc::heap_allocator!(
        #[esp_hal::ram(reclaimed)]
        size: 98768
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let _wifi_controller =
        esp_radio::wifi::WifiController::new(peripherals.WIFI, ControllerConfig::default())
            .expect("Failed to initialize Wi-Fi controller");

    let _wifi_interface = esp_radio::wifi::Interface::station();

    let _ = spawner;

    let mut led = Output::new(peripherals.GPIO2, Level::Low, OutputConfig::default());

    // Relé ativo em LOW.
    let mut pump = Output::new(peripherals.GPIO13, Level::High, OutputConfig::default());

    let manual = Input::new(
        peripherals.GPIO12,
        InputConfig::default().with_pull(Pull::Up),
    );

    let (mut adc, mut humidity_pin, mut raindrop_pin) = {
        let mut adc_config = AdcConfig::new();

        let humidity_pin = adc_config.enable_pin(peripherals.GPIO34, Attenuation::_11dB);

        let raindrop_pin = adc_config.enable_pin(peripherals.GPIO33, Attenuation::_11dB);

        (
            Adc::new(peripherals.ADC1, adc_config),
            humidity_pin,
            raindrop_pin,
        )
    };

    let mut state = IrrigationState::Idle;

    let mut last_rain_seen = Instant::now();
    let mut last_irrigation_end: Option<Instant> = None;
    let mut irrigation_started = Instant::now();

    led.set_high();

    loop {
        Timer::after(Duration::from_millis(250)).await;

        let (Some(raindrop_value), Some(humidity_value)) = ({
            let mut raindrop_value = adc.read_oneshot(&mut raindrop_pin).ok();
            let mut humidity_value = adc.read_oneshot(&mut humidity_pin).ok();

            while raindrop_value.is_none() || humidity_value.is_none() {
                Timer::after(Duration::from_millis(250)).await;
                if let Ok(raindrop_read) = adc.read_oneshot(&mut raindrop_pin) {
                    raindrop_value = Some(raindrop_read);
                }
                if let Ok(humidity_read) = adc.read_oneshot(&mut humidity_pin) {
                    humidity_value = Some(humidity_read);
                }
            }

            (raindrop_value, humidity_value)
        }) else {
            unreachable!();
        };

        let humidity_percentage = (u32::from(4095u16 - humidity_value) * 100) / 4095;

        let raindrop_percentage = (u32::from(4095u16 - raindrop_value) * 100) / 4095;

        let emergency = humidity_percentage < 20;
        let dry = humidity_percentage < 40;
        let raining = raindrop_percentage > 40;

        let now = Instant::now();

        let next_irrigation: String = match state {
            IrrigationState::Idle => {
                if humidity_percentage >= 40 {
                    "Solo umido".to_string()
                } else if emergency {
                    "Emergencia: imediata".to_string()
                } else if let Some(last) = last_irrigation_end {
                    let elapsed = now.duration_since(last);
                    let cooldown = Duration::from_secs(60);

                    if elapsed < cooldown {
                        let remaining = cooldown - elapsed;
                        format!("Cooldown: {}s", remaining.as_secs())
                    } else if raining {
                        "Chuva detectada".to_string()
                    } else {
                        "Imediata".to_string()
                    }
                } else if raining {
                    "Chuva detectada".to_string()
                } else {
                    "Imediata".to_string()
                }
            }

            IrrigationState::WaitingForRain => {
                let elapsed = now.duration_since(last_rain_seen);
                let wait = Duration::from_secs(7200);

                let Some(remaining) = wait.checked_sub(elapsed) else {
                    unreachable!()
                };

                format!(
                    "Em {}m {}s",
                    remaining.as_secs() / 60,
                    remaining.as_secs() % 60
                )
            }

            IrrigationState::Irrigating => "Irrigando agora".to_string(),
        };

        println!(
            "Solo: {humidity_percentage}% ({humidity_value}); \
             Chuva: {raindrop_percentage}% ({raindrop_value}); \
             Manual: {}; \
             Proxima irrigação: {next_irrigation}",
            manual.is_low()
        );

        led.toggle();

        if manual.is_low() {
            irrigation_started = now;
            last_irrigation_end = Some(now);
            state = IrrigationState::Idle;
            pump.set_low();
            continue;
        }

        match state {
            IrrigationState::Idle => {
                pump.set_high();

                let cooldown_elapsed = last_irrigation_end
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(60));

                if dry && cooldown_elapsed {
                    if raining && !emergency {
                        last_rain_seen = now;
                        state = IrrigationState::WaitingForRain;
                    } else {
                        irrigation_started = now;
                        state = IrrigationState::Irrigating;
                        pump.set_low();
                    }
                }
            }

            IrrigationState::WaitingForRain => {
                if !dry {
                    state = IrrigationState::Idle;
                    pump.set_high();
                } else if emergency {
                    // Emergência: ignora a espera pela chuva.
                    irrigation_started = now;
                    state = IrrigationState::Irrigating;
                    pump.set_low();
                } else if raining {
                    last_rain_seen = now;
                } else if now.duration_since(last_rain_seen) >= Duration::from_secs(7200) {
                    irrigation_started = now;
                    state = IrrigationState::Irrigating;
                    pump.set_low();
                }
            }

            IrrigationState::Irrigating => {
                let irrigation_elapsed = now.duration_since(irrigation_started);

                if !dry || irrigation_elapsed >= Duration::from_secs(2) {
                    // Atingiu 40% ou completou 5 segundos.
                    pump.set_high();
                    state = IrrigationState::Idle;
                    last_irrigation_end = Some(now);
                } else {
                    pump.set_low();
                }
            }
        }
    }
}
