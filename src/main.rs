mod config;
mod gamepad;
mod haybox;
mod usb;
mod web;
mod xinput;

use std::io::Write;
use std::time::{Duration, Instant};
use std::{fs, io};

use gilrs_core::Gilrs;
use haybox::Haybox;
use log::{error, info};
use minifb::{Key, ScaleMode, Window, WindowOptions};
use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind};
use tiny_skia::Pixmap;

use config::ConfigWatcher;
use gamepad::Gamepad;
use usb::UsbGamepad;
use xinput::XInput;

const FPS: usize = 60;
const BENCHMARK: bool = false;

fn main() -> Result<(), ()> {
    env_logger::init();
    color_eyre::install().unwrap();
    let mut gamepad = Gamepad::default();
    let mut watcher = ConfigWatcher::new(Duration::from_millis(100));
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut web = false;
    let mut port: u16 = 8080;
    let mut scale: f32 = 1.0;
    let mut positionals: Vec<String> = Vec::new();
    let mut rest = raw.into_iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            "--web" | "--serve" | "-w" => web = true,
            "--port" | "-p" => match rest.next().and_then(|v| v.parse().ok()) {
                Some(p) => port = p,
                None => {
                    println!("--port expects a number, e.g. --port 8080");
                    return Err(());
                }
            },
            s if s.starts_with("--port=") => match s["--port=".len()..].parse() {
                Ok(p) => port = p,
                Err(_) => {
                    println!("--port expects a number, e.g. --port=8080");
                    return Err(());
                }
            },
            "--scale" | "-s" => match rest.next().and_then(|v| v.parse::<f32>().ok()) {
                Some(x) if x > 0.0 => scale = x,
                _ => {
                    println!("--scale expects a positive number, e.g. --scale 2");
                    return Err(());
                }
            },
            s if s.starts_with("--scale=") => match s["--scale=".len()..].parse::<f32>() {
                Ok(x) if x > 0.0 => scale = x,
                _ => {
                    println!("--scale expects a positive number, e.g. --scale=2");
                    return Err(());
                }
            },
            s if s.starts_with('-') => {
                println!("Unknown flag '{s}'");
                return Err(());
            }
            _ => positionals.push(a),
        }
    }
    let arg = match positionals.as_slice() {
        [] => "layouts/test.toml",
        [path] => path.as_str(),
        _ => {
            println!(
                "Usage: obs-gamepad [--web] [--port <N>] [--scale <N>] [config.toml]\n  \
                 no args   = debug window on layouts/test.toml\n  \
                 --web     = serve the overlay over the local network instead of a window\n  \
                 --scale N = render the overlay at N\u{00d7} resolution (default 1)"
            );
            return Err(());
        }
    };
    gamepad.scale = scale;
    let watch_file = fs::canonicalize(arg).unwrap();
    watcher.change_file(&watch_file).unwrap();
    let mut last_change = Instant::now();

    let gilrs = Gilrs::new().unwrap();
    let ports = serialport::available_ports().unwrap_or_default();
    let max_gamepads = gilrs.last_gamepad_hint();
    let id = pick_input(max_gamepads, &gilrs);

    let config: Result<config::Gamepad, toml::de::Error> =
        toml::from_str(&fs::read_to_string(&watch_file).unwrap());
    if let Err(e) = config.map(|c| {
        let res = if id >= 20 {
            gamepad.load::<XInput>(&c, (id - 20) as u32)
        } else if id < 10 {
            gamepad.load::<UsbGamepad>(&c, (Gilrs::new().unwrap(), id))
        } else {
            let name =
                &ports.get(id - 10).expect("couldn't find or open serial port").port_name;
            gamepad.load::<Haybox>(&c, (name.clone(), 115200))
        };
        if let Err(e) = res {
            error!("Failed to initialize backend {e:?}");
        }
    }) {
        error!("Invalid config: {e}\n")
    }

    if web {
        return web::serve(gamepad, watcher, watch_file, port);
    }

    let options = WindowOptions {
        resize: false,
        scale_mode: ScaleMode::Stretch,
        ..Default::default()
    };

    let mut img = create_image(&gamepad);
    let mut width = img.width() as usize;
    let mut height = img.height() as usize;
    let mut buf = vec![0u32; width * height];
    gamepad.render(&mut img);
    update_screen(&mut img, &mut buf);
    let mut window = Window::new("Test", width, height, options).unwrap();
    window.set_target_fps(FPS);
    while watcher.rx.try_recv().is_ok() {} // drain initial file changes

    let mut times = 0;
    let mut total = 0u128;
    while window.is_open()
        && !(window.is_key_down(Key::Escape) || window.is_key_down(Key::Q))
    {
        while let Ok(DebouncedEvent { path, kind: DebouncedEventKind::Any }) =
            watcher.rx.try_recv()
        {
            let now = Instant::now();
            if now.duration_since(last_change) < Duration::from_millis(500) {
                continue;
            }
            last_change = now;

            if watch_file == path {
                match toml::from_str(&fs::read_to_string(path).unwrap()) {
                    Ok(config) => {
                        println!("Reloaded config...");
                        gamepad.reload(&config);
                        let (nw, nh) = gamepad.image_size();
                        if width != nw as usize || height != nh as usize {
                            info!("Resized, making new window...");
                            img = create_image(&gamepad);
                            width = img.width() as usize;
                            height = img.height() as usize;
                            buf = vec![0u32; width * height];
                            window = Window::new("Test", width, height, options).unwrap();
                            window.set_target_fps(FPS);
                        }
                        gamepad.render(&mut img);
                        update_screen(&mut img, &mut buf);
                    }
                    Err(e) => error!("Config reload failed: {}", e),
                }
            }
        }

        let frame_start = Instant::now();
        if gamepad.poll() || BENCHMARK {
            gamepad.render(&mut img);
            update_screen(&mut img, &mut buf);
        }
        let frame_end = Instant::now();
        total += (frame_end - frame_start).as_micros();
        times += 1;
        window.update_with_buffer(&buf, width, height).unwrap();
    }
    info!("{}us average render time per frame", total / times);
    Ok(())
}

// returns selected id
fn pick_input(max_gamepads: usize, gilrs: &Gilrs) -> usize {
    println!("\nDetected {} gamepads:", max_gamepads);
    for (id, name) in usb::get_devices(gilrs) {
        println!("{id}: {name}");
    }
    for (id, (name, desc)) in haybox::get_ports().iter().enumerate() {
        println!("{}: {name} {desc}", id + 10);
    }
    for slot in xinput::get_slots() {
        println!("{}: XInput controller (slot {slot})", 20 + slot);
    }
    print!("\nEnter an id: ");
    io::stdout().flush().unwrap();
    let mut line = String::new();
    io::stdin().read_line(&mut line).unwrap();
    line.trim().parse().expect("input a number")
}

fn update_screen(img: &mut Pixmap, buf: &mut [u32]) {
    for (pixel, n) in img.pixels_mut().iter().zip(buf.iter_mut()) {
        *n = (pixel.red() as u32) << 16 | (pixel.green() as u32) << 8 | pixel.blue() as u32;
    }
}

fn create_image(gamepad: &Gamepad) -> Pixmap {
    let (width, height) = gamepad.image_size();
    Pixmap::new(width, height).unwrap()
}
