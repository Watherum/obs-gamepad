use std::{
    fs,
    io::Write,
    net::UdpSocket,
    path::PathBuf,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use log::{error, info};
use notify_debouncer_mini::{DebouncedEvent, DebouncedEventKind};
use tiny_http::{Header, Request, Response, Server, StatusCode};
use tiny_skia::Pixmap;

use crate::config::ConfigWatcher;
use crate::gamepad::Gamepad;

const FPS: u64 = 60;
const BOUNDARY: &str = "obsgamepadframe";

const INDEX_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>obs-gamepad</title><style>\
html,body{margin:0;height:100%;background:#1e1e1e;display:flex;\
align-items:center;justify-content:center}\
img{width:80%;height:80%;object-fit:contain}\
</style></head><body>\
<img src=\"/stream\" alt=\"gamepad overlay\"></body></html>";

// CSS-skin view (served at `/skin`): HTML + localized stylesheet + button
// sprites, all embedded so it works from any cwd and when bundled. The skin is
// driven live over SSE (`/events`) rather than the MJPEG stream.
const SKIN_HTML: &str = include_str!("../assets/skin/skin.html");
const SKIN_CSS: &str = include_str!("../assets/skin/skin.css");
const SKIN_IMAGES: &[(&str, &[u8])] = &[
    ("menus_buttons_xbox_A.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_A.png")),
    ("menus_buttons_xbox_B.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_B.png")),
    ("menus_buttons_xbox_X.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_X.png")),
    ("menus_buttons_xbox_Y.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_Y.png")),
    ("menus_buttons_xbox_LB.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_LB.png")),
    ("menus_buttons_xbox_RB.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_RB.png")),
    ("menus_buttons_xbox_LT.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_LT.png")),
    ("menus_buttons_xbox_RT.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_RT.png")),
    ("menus_buttons_xbox_LSB_allGrey.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_LSB_allGrey.png")),
    ("menus_buttons_xbox_LSB_white.png", include_bytes!("../assets/skin/images/menus_buttons_xbox_LSB_white.png")),
    ("Empty.png", include_bytes!("../assets/skin/images/Empty.png")),
];

// Teko (OFL-licensed variable font, see assets/fonts/OFL.txt), served at
// `/fonts/teko.ttf` and used for the `?labels` overlay text.
const TEKO_TTF: &[u8] = include_bytes!("../assets/fonts/teko.ttf");

/// The most recently rendered frame, encoded as a PNG. `seq` is bumped on every
/// new frame so streaming clients can tell when there's something new to send.
/// `mask` is the matching pressed-button bitmask (by serial id) for the `/skin` view.
struct Shared {
    frame: Mutex<Frame>,
    cond: Condvar,
    /// Pre-rendered `/?labels` overlay page (regenerated on config reload).
    labels_html: Mutex<String>,
}

struct Frame {
    seq: u64,
    png: Vec<u8>,
    /// SSE body for the `/skin` view: "<button-mask>;<lx>,<ly>,<rx>,<ry>".
    event: String,
}

/// Render `gamepad` in a loop and serve the result as a `multipart/x-mixed-replace`
/// MJPEG stream (of PNG frames, so transparency is preserved) on `0.0.0.0:port`.
/// Blocks forever; the existing window/OBS paths are unaffected.
pub fn serve(
    mut gamepad: Gamepad<'_>,
    watcher: ConfigWatcher,
    watch_file: PathBuf,
    port: u16,
) -> Result<(), ()> {
    let server = match Server::http(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            error!("couldn't start web server on port {port}: {e}");
            return Err(());
        }
    };

    let shared = Arc::new(Shared {
        frame: Mutex::new(Frame { seq: 0, png: Vec::new(), event: String::new() }),
        cond: Condvar::new(),
        labels_html: Mutex::new(build_labels_page(&gamepad)),
    });

    match local_ip() {
        Some(ip) => println!("Serving overlay at http://{ip}:{port} (also reachable from other devices on your network)"),
        None => println!("Serving overlay on port {port} at http://<this-machine-ip>:{port}"),
    }
    println!("  rendered overlay:  /stream   (transparent MJPEG, point OBS here)");
    println!("  CSS-skin overlay:  /skin     (Xbox fight-stick skin)");
    println!("  labeled overlay:   /?labels  (button names, for reference)");

    // Accept connections on a background thread; each stream client gets its own thread.
    {
        let shared = shared.clone();
        thread::spawn(move || {
            for request in server.incoming_requests() {
                let shared = shared.clone();
                let url = request.url().to_owned();
                let (path, query) = url.split_once('?').unwrap_or((&url, ""));
                match path {
                    "/stream" => {
                        thread::spawn(move || stream_to(request, shared));
                    }
                    "/events" => {
                        thread::spawn(move || events_to(request, shared));
                    }
                    "/" if query.split('&').any(|p| p == "labels" || p.starts_with("labels=")) => {
                        let html = shared.labels_html.lock().unwrap().clone();
                        respond_static(request, html.as_bytes(), "text/html; charset=utf-8");
                    }
                    "/" => respond_static(request, INDEX_HTML.as_bytes(), "text/html; charset=utf-8"),
                    "/skin" => respond_static(request, SKIN_HTML.as_bytes(), "text/html; charset=utf-8"),
                    "/skin/skin.css" => {
                        respond_static(request, SKIN_CSS.as_bytes(), "text/css; charset=utf-8")
                    }
                    "/fonts/teko.ttf" => respond_static(request, TEKO_TTF, "font/ttf"),
                    url if url.starts_with("/skin/images/") => {
                        let name = &url["/skin/images/".len()..];
                        match SKIN_IMAGES.iter().find(|(n, _)| *n == name) {
                            Some((_, bytes)) => respond_static(request, bytes, "image/png"),
                            None => respond_404(request),
                        }
                    }
                    _ => respond_404(request),
                }
            }
        });
    }

    // Render loop on this thread (gamepad polling isn't `Send`, so it stays here).
    let mut img = new_pixmap(&gamepad);
    let (mut width, mut height) = (img.width(), img.height());
    gamepad.render(&mut img);
    publish(&shared, &img, skin_event(&gamepad));
    while watcher.rx.try_recv().is_ok() {} // drain initial file-change events

    let frame_time = Duration::from_millis(1000 / FPS);
    let mut last_change = Instant::now();
    loop {
        while let Ok(DebouncedEvent { path, kind: DebouncedEventKind::Any }) =
            watcher.rx.try_recv()
        {
            let now = Instant::now();
            if now.duration_since(last_change) < Duration::from_millis(500) {
                continue;
            }
            last_change = now;
            if watch_file != path {
                continue;
            }
            match fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|c| {
                toml::from_str(&c).map_err(|e| e.to_string())
            }) {
                Ok(config) => {
                    info!("Reloaded config");
                    gamepad.reload(&config);
                    let (nw, nh) = gamepad.image_size();
                    if width != nw || height != nh {
                        img = new_pixmap(&gamepad);
                        width = img.width();
                        height = img.height();
                    }
                    gamepad.render(&mut img);
                    publish(&shared, &img, skin_event(&gamepad));
                    *shared.labels_html.lock().unwrap() = build_labels_page(&gamepad);
                }
                Err(e) => error!("Config reload failed: {e}"),
            }
        }

        if gamepad.poll() {
            // Input changed: re-render and re-encode the frame.
            gamepad.render(&mut img);
            publish(&shared, &img, skin_event(&gamepad));
        } else {
            // No change: re-send the current frame anyway (cheap, no re-encode).
            // A browser showing a multipart <img> only commits a frame once the
            // *next* one begins arriving, so a steady ~60fps stream is what makes
            // a release show immediately instead of lingering until the next input.
            republish(&shared);
        }
        thread::sleep(frame_time);
    }
}

fn new_pixmap(gamepad: &Gamepad) -> Pixmap {
    let (width, height) = gamepad.image_size();
    Pixmap::new(width, height).unwrap()
}

fn publish(shared: &Shared, img: &Pixmap, event: String) {
    match img.encode_png() {
        Ok(png) => {
            let mut frame = shared.frame.lock().unwrap();
            frame.seq += 1;
            frame.png = png;
            frame.event = event;
            drop(frame);
            shared.cond.notify_all();
        }
        Err(e) => error!("failed to encode frame as png: {e}"),
    }
}

/// Bit position of a named skin button, or `None` for a non-button role.
fn skin_button_bit(role: &str) -> Option<u8> {
    Some(match role {
        "a" => 0,
        "b" => 1,
        "x" => 2,
        "y" => 3,
        "lb" => 4,
        "rb" => 5,
        "lt" => 6,
        "rt" => 7,
        "l3" => 8,
        "r3" => 9,
        "start" => 10,
        _ => return None,
    })
}

/// Normalized state the `/skin` view consumes, computed from the layout's `skin`
/// role tags so any controller maps onto the same skin. Returns the SSE body
/// "<button-mask>;<lx>,<ly>,<rx>,<ry>" with stick floats in screen orientation.
fn skin_event(gamepad: &Gamepad) -> String {
    let mut mask: u16 = 0;
    let (mut lx, mut ly, mut rx, mut ry) = (0.0f32, 0.0, 0.0, 0.0);
    // Raw pressed-button bitmask keyed by layout `id` — used by the `?labels`
    // view (whose labels are keyed by raw id, not skin element).
    let mut raw: u64 = 0;

    for (b, &pressed) in gamepad.inputs.buttons.iter().zip(&gamepad.input_state.buttons) {
        if !pressed {
            continue;
        }
        raw |= 1u64 << b.id;
        let Some(skin) = &b.skin else { continue };
        for role in skin.split_whitespace() {
            if let Some(bit) = skin_button_bit(role) {
                mask |= 1 << bit;
            } else {
                // Digital directions deflect a stick (for stickless controllers).
                match role {
                    "lup" => ly -= 1.0,
                    "ldown" => ly += 1.0,
                    "lleft" => lx -= 1.0,
                    "lright" => lx += 1.0,
                    "rup" => ry -= 1.0,
                    "rdown" => ry += 1.0,
                    "rleft" => rx -= 1.0,
                    "rright" => rx += 1.0,
                    _ => {}
                }
            }
        }
    }
    // Analog sticks override any digital deflection.
    for (s, &(x, y)) in gamepad.inputs.sticks.iter().zip(&gamepad.input_state.sticks) {
        match s.skin.as_deref() {
            Some("left") => (lx, ly) = (x, y),
            Some("right") => (rx, ry) = (x, y),
            _ => {}
        }
    }
    let c = |v: f32| v.clamp(-1.0, 1.0);
    format!("{mask};{:.3},{:.3},{:.3},{:.3};{raw}", c(lx), c(ly), c(rx), c(ry))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Build the `/?labels` overlay page: the rendered overlay (`/stream`) with each
/// labeled button's name centered on it. Label positions are the buttons' centers
/// in final (scaled) image coordinates, placed inside a wrapper that scales to fit
/// the viewport so the names track the image at any display size.
fn build_labels_page(gamepad: &Gamepad) -> String {
    let (w, h) = gamepad.image_size();
    let scale = gamepad.scale;
    let mut labels = String::new();
    for button in &gamepad.inputs.buttons {
        let Some(text) = &button.label else { continue };
        let id = button.id;
        let b = button.path.bounds();
        let cx = (b.left() + b.right()) / 2.0 * scale;
        let cy = (b.top() + b.bottom()) / 2.0 * scale;
        let diameter = (b.bottom() - b.top()) * scale;
        // Shrink the font for longer names so they stay inside the button.
        let shrink = (4.0 / text.chars().count().max(1) as f32).clamp(0.6, 1.0);
        let fs = diameter * 0.5 * shrink;
        let mut text = html_escape(text);
        // Arrow glyphs (from the Arial fallback) are thin; wrap them so CSS can
        // thicken just the arrows via text-stroke, leaving lettered labels as-is.
        for arrow in ['↑', '↓', '←', '→'] {
            text = text.replace(arrow, &format!("<span class=\"ar\">{arrow}</span>"));
        }
        labels.push_str(&format!(
            "<div class=\"l\" data-id=\"{id}\" style=\"left:{cx:.1}px;top:{cy:.1}px;font-size:{fs:.1}px\">{text}</div>"
        ));
    }
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\">\
<title>obs-gamepad labels</title><style>\
@font-face{{font-family:'Teko';src:url('/fonts/teko.ttf') format('truetype');\
font-weight:300 700;font-display:swap}}\
html,body{{margin:0;height:100%;overflow:hidden;background:#1e1e1e}}\
#w{{position:absolute;top:0;left:0;width:{w}px;height:{h}px;transform-origin:top left}}\
#w img{{position:absolute;top:0;left:0;width:100%;height:100%}}\
.l{{position:absolute;transform:translate(-50%,-45%);color:#fff;line-height:1;\
font-family:'Teko',Arial,sans-serif;font-weight:700;white-space:nowrap;\
pointer-events:none;text-shadow:0 0 4px #000,0 0 4px #000}}\
.ar{{-webkit-text-stroke:0.08em currentColor;paint-order:stroke fill}}\
</style></head><body>\
<div id=\"w\"><img src=\"/stream\" alt=\"overlay\">{labels}</div>\
<script>\
var w=document.getElementById('w');\
function fit(){{var s=Math.min(innerWidth/{w},innerHeight/{h})*0.8;\
w.style.transform='translate('+((innerWidth-{w}*s)/2)+'px,'+((innerHeight-{h}*s)/2)+'px) scale('+s+')';}}\
addEventListener('resize',fit);fit();\
if(location.search.indexOf('pressed')>=0){{\
var ls=document.querySelectorAll('.l');\
ls.forEach(function(el){{el.style.visibility='hidden';}});\
var es=new EventSource('/events');\
es.onmessage=function(e){{var m=parseInt(e.data.split(';')[2],10)||0;\
ls.forEach(function(el){{el.style.visibility=(m&(1<<+el.dataset.id))?'visible':'hidden';}});}};\
}}\
</script></body></html>"
    )
}

fn respond_static(request: Request, body: &[u8], content_type: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes()).unwrap();
    let _ = request.respond(Response::from_data(body).with_header(header));
}

fn respond_404(request: Request) {
    let _ = request
        .respond(Response::from_string("not found").with_status_code(StatusCode(404)));
}

/// Push the pressed-button bitmask to a `/skin` client as Server-Sent Events.
/// Wakes on every frame `seq` bump but only emits when the mask actually changes,
/// so the browser only does work on real input transitions.
fn events_to(request: Request, shared: Arc<Shared>) {
    let mut writer = request.into_writer();
    let head = "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n";
    if writer.write_all(head.as_bytes()).is_err() {
        return;
    }

    let mut last_seq = 0;
    let mut last_event = String::new();
    let mut first = true;
    loop {
        let event = {
            let mut frame = shared.frame.lock().unwrap();
            while frame.seq == last_seq {
                frame = shared.cond.wait(frame).unwrap();
            }
            last_seq = frame.seq;
            frame.event.clone()
        };
        if !first && event == last_event {
            continue;
        }
        first = false;
        last_event = event.clone();
        let msg = format!("data: {event}\n\n");
        if writer.write_all(msg.as_bytes()).is_err() || writer.flush().is_err() {
            break; // client disconnected
        }
    }
}

/// Re-send the current frame without re-encoding: bump `seq` so blocked stream
/// clients wake and flush the previous (idle) frame. Cheap keepalive heartbeat.
fn republish(shared: &Shared) {
    let mut frame = shared.frame.lock().unwrap();
    if frame.png.is_empty() {
        return;
    }
    frame.seq += 1;
    drop(frame);
    shared.cond.notify_all();
}

/// Stream PNG frames as `multipart/x-mixed-replace`, writing straight to the
/// socket and flushing after every frame. Going through `tiny_http`'s buffered
/// `Read` response instead would leave each frame's tail unflushed until later
/// bytes accumulated, so updates lagged ~a frame behind ("one input at a time").
fn stream_to(request: Request, shared: Arc<Shared>) {
    let mut writer = request.into_writer();
    // Send the opening boundary as part of the head. Thereafter every frame is
    // closed by the boundary that *follows* it, written immediately, so the
    // client renders each frame as it arrives instead of waiting for the next.
    let head = format!(
        "HTTP/1.1 200 OK\r\n\
         Content-Type: multipart/x-mixed-replace; boundary={BOUNDARY}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\r\n\
         --{BOUNDARY}\r\n"
    );
    if writer.write_all(head.as_bytes()).is_err() {
        return;
    }

    let trailer = format!("\r\n--{BOUNDARY}\r\n");
    let mut last_seq = 0;
    loop {
        let png = {
            let mut frame = shared.frame.lock().unwrap();
            while frame.seq == last_seq {
                frame = shared.cond.wait(frame).unwrap();
            }
            last_seq = frame.seq;
            frame.png.clone()
        };
        let header =
            format!("Content-Type: image/png\r\nContent-Length: {}\r\n\r\n", png.len());
        // Header, frame, then the closing boundary — flushed at once so the
        // browser finalizes and paints this frame now, not on the next change.
        let sent = writer.write_all(header.as_bytes()).is_ok()
            && writer.write_all(&png).is_ok()
            && writer.write_all(trailer.as_bytes()).is_ok()
            && writer.flush().is_ok();
        if !sent {
            break; // client disconnected
        }
    }
}

/// Best-effort guess of this machine's LAN IP, just for a friendlier log line.
/// "Connecting" a UDP socket only sets a default route; no packets are sent.
fn local_ip() -> Option<String> {
    let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}
