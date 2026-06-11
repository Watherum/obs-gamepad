# OBS Gamepad Plugin

I wanted to display my controller inputs while speedrunning celeste (and later
while playing melee), and I just wanted to draw some transparent shapes for
the buttons. I didn't want to set up a browser capture for something like
<https://gamepadviewer.com>, and [input-overlay](https://github.com/univrsal/input-overlay)
was a bit complicated for my use case (though if I ever wanted more features
it seems really cool and customizable), so I just wrote a little plugin to do
it myself.

celeste | melee
--|--
<video src="https://github.com/user-attachments/assets/b3164361-9b0e-4bd0-a1eb-2e8e89ed3d3c"> | <video src="https://github.com/user-attachments/assets/8be063a2-31be-4124-9007-64da3ac5f33b">

## Installation

### Windows

Download the [latest release](https://github.com/P1n3appl3/obs-gamepad/releases/latest) and then [follow these instructions](https://obsproject.com/kb/plugins-guide#install-or-remove-plugins) to copy it into your OBS plugins directory.

### Nix

There's a home-manager module for OBS, so you can use this flake's overlay to install the plugin like you would any other:

```nix
programs.obs-studio = { enable = true;
  plugins = with pkgs.obs-studio-plugins; [ obs-gamepad ];
};
```

If you don't use flakes or home-manager, you can use flake-compat and manually override the OBS wrapper to add the plugin:

```nix
pkgs.wrapOBS.override {} { plugins = [ obs-gamepad ]; };
```

### Other Linux Distros

Run [`install.sh`](install.sh) (which is just `cargo build` + moving the files around). If you want to write a PKGBUILD/deb/rpm/etc. for your distro, just take a look in [flake.nix](flake.nix) for the required native deps.

## Building

You'll need a recent [Rust toolchain](https://rustup.rs) (the crate uses edition 2024). The native build deps (OBS headers, etc.) are listed in [flake.nix](flake.nix); on Nix you can get a dev shell with `nix develop`.

The crate builds two things at once: the OBS plugin (`gamepad.dll` / `libgamepad.so`) and a standalone tester (`obs-gamepad`).

```sh
cargo build --release   # builds both the plugin and the tester
cargo run               # runs the standalone tester on layouts/test.toml
cargo run my-config.toml # runs it on your own layout
```

To build *and* package/install into your OBS plugins directory, use the platform scripts instead of running `cargo` directly:

- **Windows:** [`bundle.ps1`](bundle.ps1) — builds release and stages `obs-gamepad/bin/64bit/` with the dll, tester exe, and layouts.
- **Linux:** [`install.sh`](install.sh) — builds release and copies into `~/.config/obs-studio/plugins/gamepad/`.
- **Nix:** `nix build` produces the plugin package (see the flake's overlay for installing via home-manager).

## Usage

Check out [the example](layouts/example.toml) to see the config options.

You don't have to open OBS to tweak your config, just
`cargo run <my-config.toml>` and it'll show your overlay in a separate window. Both
the OBS plugin and the standalone window support live-reloading, so if you tweak
your config file and save, the changes should show up in your overlay.

### Button labels

Give any button a `label = "..."` in your layout and the web overlay can draw
the names on top (handy for documenting a stickless/box layout). Arrow glyphs
work well for directions, e.g. `label = "←"`. See [`layouts/gram.toml`](layouts/gram.toml)
for an example. Labels show up via the `?labels` web view (below).

### Web mode

If you'd rather watch the overlay on another computer (or pull it into OBS on a
different machine), run the standalone app in web mode:

```sh
cargo run -- --web <my-config.toml>             # serves on port 8080 by default
cargo run -- --web --port 9000 <config>         # pick a different port
cargo run -- --web --scale 3 <config>           # render at 3x resolution (stays crisp when enlarged)
```

It picks an input device the same way as window mode, then serves the overlay
over HTTP on your local network (the app prints the exact URL on startup).
`--scale N` renders the whole layout at N× resolution so it stays sharp when an
OBS source or browser blows it up; it also works in window mode. Live-reloading
works in web mode too.

The server exposes a few endpoints — point an OBS **Browser Source** at whichever
you want:

| URL | what it is |
| --- | --- |
| `/` | a viewer page for humans (dark background, embeds `/stream`) |
| `/stream` | the rendered overlay as a **transparent** MJPEG stream — the usual OBS source |
| `/?labels` | the overlay with each button's `label` drawn on it (for reference/docs) |
| `/?labels=pressed` | same, but each label only appears while its button is held |
| `/skin` | an HTML/CSS controller skin driven live by your inputs (see below) |

### CSS skin overlay

`/skin` renders a [gamepadviewer.com](https://gamepadviewer.com)-style controller
skin (currently a Rivals of Aether 2 "fight-stick" skin, assets bundled locally)
that lights up and moves the sticks based on your inputs. It's driven over
Server-Sent Events, so it tracks input in real time and composites transparently
in an OBS Browser Source.

Which physical button maps to which skin element comes from `skin = "..."` tags
in your layout:

- buttons: `"a"`, `"b"`, `"x"`, `"y"`, `"lb"`, `"rb"`, `"lt"`, `"rt"`, `"l3"`,
  `"r3"`, `"start"` (space-separate to map one input to several, e.g. `"lb rb"`),
  or a digital direction `"lup"/"ldown"/"lleft"/"lright"` (and `"r…"` for the
  right stick) for stickless controllers
- sticks: `skin = "left"` or `"right"` for an analog stick (deflects proportionally)

Add `?hide=lb,start,…` to the URL to drop elements you don't want (e.g. a
GameCube controller has no separate LB, so `/skin?hide=lb` shows only RB).

### Controllers

The device picker (and the OBS source dropdown) lists three kinds of input:

- **USB/HID gamepads** via gilrs
- **Serial controllers** (B0XX/Haybox firmware that streams button states)
- **XInput controllers** — any Xbox-style controller, including a **GameCube
  adapter in XInput mode** (e.g. a Lossless adapter). This is the most reliable
  path on Windows, and reading XInput is shareable, so the overlay can run
  alongside a game that's also using the controller. See
  [`layouts/gc-rivals.toml`](layouts/gc-rivals.toml) for a GameCube → Rivals 2
  skin mapping.

## Future plans

- backend to read dolphin memory like [m-overlay](https://github.com/bkacjios/m-overlay)
  - will need additional button config for arbitrary bezier paths
  - octagonal gate option
- keyboard backend?
- pass through info about whether or not a backend is connected and render that somehow
