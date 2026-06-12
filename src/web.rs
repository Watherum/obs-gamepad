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
fn css_color(c: tiny_skia::Color) -> String {
    let u = c.to_color_u8();
    format!("rgba({},{},{},{})", u.red(), u.green(), u.blue(), u.alpha())
}

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
        let (color_style, active_attr) = match &button.label_color {
            Some(pair) => {
                let inactive = css_color(pair.inactive);
                let active = css_color(pair.active);
                let attr = if active != inactive {
                    format!(" data-lca=\"{active}\"")
                } else {
                    String::new()
                };
                (format!(";color:{inactive}"), attr)
            }
            None => (String::new(), String::new()),
        };
        labels.push_str(&format!(
            "<div class=\"l\" data-id=\"{id}\"{active_attr} style=\"left:{cx:.1}px;top:{cy:.1}px;font-size:{fs:.1}px{color_style}\">{text}</div>"
        ));
    }

    // Static CSS, HTML and JS for the hidden color-picker panel (backtick to toggle).
    let panel_css = r#"
#cm{position:fixed;top:50%;left:50%;transform:translate(-50%,-50%);
background:#1a1a1a;border:1px solid #444;border-radius:10px;padding:18px 20px;
display:none;z-index:999;color:#ccc;font-family:sans-serif;font-size:13px;
box-shadow:0 8px 32px #000c;min-width:230px;user-select:none}
#cm h3{margin:0 0 10px;font-size:14px;color:#fff;letter-spacing:.04em}
.cr{display:flex;align-items:center;gap:10px;margin:9px 0}
.cr label{flex:0 0 58px;font-size:12px;color:#888}
#cc{width:42px;height:28px;border:none;padding:1px 2px;border-radius:4px;
background:#333;cursor:pointer;flex-shrink:0}
#co{flex:1;accent-color:#aaa;cursor:pointer}
#ch{flex:1;background:#252525;border:1px solid #444;border-radius:4px;
color:#ddd;font-size:12px;padding:3px 6px;font-family:monospace;width:0}
#cpv{padding:3px 12px;border-radius:4px;background:#2a2a2a;
font-weight:700;font-size:16px;font-family:'Teko',Arial,sans-serif}
.cm-hint{font-size:11px;color:#555;margin-top:14px;text-align:center}
#cwbtn{width:100%;background:#252525;border:1px solid #3a3a3a;border-radius:5px;
color:#888;padding:5px 8px;cursor:pointer;margin-bottom:8px;font-size:12px;text-align:center}
#cwbtn:hover{background:#2e2e2e;color:#ccc}
#cwrap{display:none;justify-content:center;padding:4px 0 8px}
#cwheel{display:block;cursor:crosshair}
"#;

    let panel_html = r##"<div id="cm">
<h3>Label Color</h3>
<button id="cwbtn">&#9660; Color wheel</button>
<div id="cwrap"><canvas id="cwheel" width="180" height="180"></canvas></div>
<div class="cr"><label>Color</label><input type="color" id="cc" value="#ffffff"></div>
<div class="cr"><label>Opacity</label><input type="range" id="co" min="0" max="100" value="100"></div>
<div class="cr"><label>Hex</label><input type="text" id="ch" maxlength="9" placeholder="#rrggbbaa" spellcheck="false"></div>
<div class="cr"><label>Preview</label><span id="cpv">Aa</span></div>
<div class="cm-hint">press ` to close &nbsp;·&nbsp; resets on reload</div>
</div>"##;

    let panel_js = r##"
var cm=document.getElementById('cm');
var cc=document.getElementById('cc');
var co=document.getElementById('co');
var ch=document.getElementById('ch');
var cpv=document.getElementById('cpv');
function h2r(h){return[parseInt(h.slice(1,3),16),parseInt(h.slice(3,5),16),parseInt(h.slice(5,7),16)];}
function toHex2(n){return('0'+Math.round(n).toString(16)).slice(-2);}
function applyLC(col){
  document.querySelectorAll('.l').forEach(function(el){
    el.style.color=col;
    if(el.dataset.lca)el.style.setProperty('--lci',col);
  });
  cpv.style.color=col;
}
function fromPicker(){
  var rgb=h2r(cc.value);var a=co.value/100;
  var col='rgba('+rgb[0]+','+rgb[1]+','+rgb[2]+','+a+')';
  ch.value=cc.value+toHex2(co.value*2.55);
  applyLC(col);
  localStorage.setItem('lc',cc.value+'|'+co.value);
}
function fromHex(){
  var v=ch.value.trim();
  if(!/^#[0-9a-fA-F]{6,8}$/.test(v))return;
  cc.value=v.slice(0,7);
  var a=v.length===9?parseInt(v.slice(7,9),16)/255:1;
  co.value=Math.round(a*100);
  fromPicker();
}
cc.addEventListener('input',fromPicker);
co.addEventListener('input',fromPicker);
ch.addEventListener('change',fromHex);
document.addEventListener('keydown',function(e){
  if(e.key==='`'&&e.target.tagName!=='INPUT'){
    cm.style.display=cm.style.display==='none'?'block':'none';
  }
});
var lc=localStorage.getItem('lc');
if(lc){var p=lc.split('|');cc.value=p[0];co.value=p[1];fromPicker();}
// === Color wheel (HSV: hue ring + saturation/value square) ===
(function(){
var cw=document.getElementById('cwheel');
var ctx=cw.getContext('2d');
var S=180,C=90,OR=87,IR=67,SQ=44;
var H=0,Sv=1,V=1,driving=false,drag=null;
function hsv2rgb(h,s,v){
  var c=v*s,x=c*(1-Math.abs(h/60%2-1)),m=v-c,r,g,b;
  if(h<60){r=c;g=x;b=0}else if(h<120){r=x;g=c;b=0}
  else if(h<180){r=0;g=c;b=x}else if(h<240){r=0;g=x;b=c}
  else if(h<300){r=x;g=0;b=c}else{r=c;g=0;b=x}
  return[Math.round((r+m)*255),Math.round((g+m)*255),Math.round((b+m)*255)];
}
function rgb2hsv(r,g,b){
  r/=255;g/=255;b/=255;
  var mx=Math.max(r,g,b),mn=Math.min(r,g,b),d=mx-mn,h=0,s=mx?d/mx:0;
  if(d){if(mx===r)h=((g-b)/d+6)%6;else if(mx===g)h=(b-r)/d+2;else h=(r-g)/d+4;h*=60;}
  return[h,s,mx];
}
function draw(){
  ctx.clearRect(0,0,S,S);
  for(var a=0;a<360;a++){
    ctx.beginPath();ctx.moveTo(C,C);
    ctx.arc(C,C,OR,(a-0.6)*Math.PI/180,(a+1.6)*Math.PI/180);
    ctx.fillStyle='hsl('+a+',100%,50%)';ctx.fill();
  }
  ctx.beginPath();ctx.arc(C,C,IR,0,2*Math.PI);
  ctx.fillStyle='#1a1a1a';ctx.fill();
  var x0=C-SQ,y0=C-SQ,w=SQ*2;
  var g1=ctx.createLinearGradient(x0,0,x0+w,0);
  g1.addColorStop(0,'#fff');g1.addColorStop(1,'hsl('+H+',100%,50%)');
  ctx.fillStyle=g1;ctx.fillRect(x0,y0,w,w);
  var g2=ctx.createLinearGradient(0,y0,0,y0+w);
  g2.addColorStop(0,'rgba(0,0,0,0)');g2.addColorStop(1,'#000');
  ctx.fillStyle=g2;ctx.fillRect(x0,y0,w,w);
  var ha=H*Math.PI/180,hr=(IR+OR)/2;
  var hx=C+hr*Math.cos(ha),hy=C+hr*Math.sin(ha);
  ctx.beginPath();ctx.arc(hx,hy,7,0,2*Math.PI);
  ctx.strokeStyle='#fff';ctx.lineWidth=2;ctx.stroke();
  var sx=x0+Sv*w,sy=y0+(1-V)*w;
  ctx.beginPath();ctx.arc(sx,sy,5,0,2*Math.PI);
  ctx.strokeStyle=V>0.4?'#000':'#fff';ctx.lineWidth=2;ctx.stroke();
}
function syncFromWheel(){
  var rgb=hsv2rgb(H,Sv,V);
  driving=true;cc.value='#'+toHex2(rgb[0])+toHex2(rgb[1])+toHex2(rgb[2]);driving=false;
  fromPicker();
}
function syncToWheel(){
  var rgb=h2r(cc.value),hsv=rgb2hsv(rgb[0],rgb[1],rgb[2]);
  H=hsv[0];Sv=hsv[1];V=hsv[2];draw();
}
function evPos(e){
  var r=cw.getBoundingClientRect(),t=e.touches?e.touches[0]:e;
  return{x:t.clientX-r.left,y:t.clientY-r.top};
}
function onDown(e){
  e.preventDefault();
  var p=evPos(e),dx=p.x-C,dy=p.y-C,d=Math.sqrt(dx*dx+dy*dy);
  drag=(d>IR&&d<OR)?'h':(Math.abs(dx)<SQ&&Math.abs(dy)<SQ)?'sv':null;
  if(drag)onMove(p);
}
function onMove(p){
  if(!drag)return;
  var dx=p.x-C,dy=p.y-C,x0=C-SQ,y0=C-SQ,w=SQ*2;
  if(drag==='h')H=(Math.atan2(dy,dx)*180/Math.PI+360)%360;
  else{Sv=Math.max(0,Math.min(1,(p.x-x0)/w));V=Math.max(0,Math.min(1,1-(p.y-y0)/w));}
  draw();syncFromWheel();
}
cw.addEventListener('mousedown',onDown);
cw.addEventListener('touchstart',onDown,{passive:false});
document.addEventListener('mousemove',function(e){if(drag)onMove(evPos(e));});
document.addEventListener('touchmove',function(e){if(drag)onMove(evPos(e));},{passive:false});
document.addEventListener('mouseup',function(){drag=null;});
document.addEventListener('touchend',function(){drag=null;});
cc.addEventListener('input',function(){if(!driving)syncToWheel();});
document.getElementById('cwbtn').addEventListener('click',function(){
  var wr=document.getElementById('cwrap');
  var show=wr.style.display!=='flex';
  wr.style.display=show?'flex':'none';
  this.textContent=(show?'▲':'▼')+' Color wheel';
  if(show)syncToWheel();
});
})();
"##;

    let sse_js = format!(
        "var ls=document.querySelectorAll('.l');\
        ls.forEach(function(el){{if(el.dataset.lca)el.style.setProperty('--lci',el.style.color);}});\
        var pressed=location.search.indexOf('pressed')>=0;\
        if(pressed)ls.forEach(function(el){{el.style.visibility='hidden';}});\
        var es=new EventSource('/events');\
        es.onmessage=function(e){{\
        var m=parseInt(e.data.split(';')[2],10)||0;\
        ls.forEach(function(el){{\
        var on=!!(m&(1<<+el.dataset.id));\
        if(pressed)el.style.visibility=on?'visible':'hidden';\
        if(el.dataset.lca)el.style.color=on?el.dataset.lca:(el.style.getPropertyValue('--lci')||'');\
        }});}};",
    );

    let mut out = String::new();
    out.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    out.push_str("<title>obs-gamepad labels</title><style>");
    out.push_str("@font-face{font-family:'Teko';src:url('/fonts/teko.ttf') format('truetype');font-weight:300 700;font-display:swap}");
    out.push_str("html,body{margin:0;height:100%;overflow:hidden;background:#1e1e1e}");
    out.push_str(&format!("#w{{position:absolute;top:0;left:0;width:{w}px;height:{h}px;transform-origin:top left}}"));
    out.push_str("#w img{position:absolute;top:0;left:0;width:100%;height:100%}");
    out.push_str(".l{position:absolute;transform:translate(-50%,-45%);color:#fff;line-height:1;");
    out.push_str("font-family:'Teko',Arial,sans-serif;font-weight:700;white-space:nowrap;");
    out.push_str("pointer-events:none;text-shadow:0 0 4px #000,0 0 4px #000}");
    out.push_str(".ar{-webkit-text-stroke:0.08em currentColor;paint-order:stroke fill}");
    out.push_str(panel_css);
    out.push_str("</style></head><body>");
    out.push_str(&format!("<div id=\"w\"><img src=\"/stream\" alt=\"overlay\">{labels}</div>"));
    out.push_str(panel_html);
    out.push_str("<script>");
    out.push_str(&format!(
        "var w=document.getElementById('w');\
        function fit(){{var s=Math.min(innerWidth/{w},innerHeight/{h})*0.8;\
        w.style.transform='translate('+((innerWidth-{w}*s)/2)+'px,'+((innerHeight-{h}*s)/2)+'px) scale('+s+')';}}
        addEventListener('resize',fit);fit();"
    ));
    out.push_str(&sse_js);
    out.push_str(panel_js);
    out.push_str("</script></body></html>");
    out
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
