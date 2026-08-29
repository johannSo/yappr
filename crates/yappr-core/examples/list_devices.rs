//! Lists the input devices cpal can actually open, with the exact string to
//! put in `[audio] device` in config.toml.
use cpal::traits::{DeviceTrait, HostTrait};

fn main() {
    let host = cpal::default_host();
    match host.default_input_device() {
        Some(d) => println!("default -> {d}"),
        None => println!("default -> (none)"),
    }
    println!("--- all input devices ---");
    match host.input_devices() {
        Ok(devs) => {
            for d in devs {
                let cfg = d.default_input_config();
                match cfg {
                    Ok(c) => println!("{d}   [{} ch @ {} Hz, {:?}]", c.channels(), c.sample_rate(), c.sample_format()),
                    Err(e) => println!("{d}   [unopenable: {e}]"),
                }
            }
        }
        Err(e) => println!("enumeration failed: {e}"),
    }
}
