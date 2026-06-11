//! XInput backend. gilrs doesn't enumerate controllers in our (windowless)
//! context, so we read XInput directly. This covers any Xbox-style controller,
//! including a GameCube adapter in XInput mode.
//!
//! Layout `id` conventions for this backend:
//! - buttons `id` 0..=15 = bit position in the XInput button mask
//!   (0 Up,1 Down,2 Left,3 Right,4 Start,5 Back,6 L3,7 R3,8 LB,9 RB,12 A,13 B,14 X,15 Y)
//! - button `id` 16 = Left Trigger, 17 = Right Trigger (pressed past a threshold)
//! - stick axes / single axes `id`: 0 LX, 1 LY, 2 RX, 3 RY, 16 LT, 17 RT.
//!   Stick values are returned in screen orientation (x right+, y down+).

use std::ffi::c_void;

use color_eyre::{Report, eyre::eyre};

use crate::gamepad::{Backend, InputState, Inputs};

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputGamepad {
    buttons: u16,
    left_trigger: u8,
    right_trigger: u8,
    thumb_lx: i16,
    thumb_ly: i16,
    thumb_rx: i16,
    thumb_ry: i16,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct XInputState {
    packet: u32,
    gamepad: XInputGamepad,
}

type XInputGetStateFn = unsafe extern "system" fn(u32, *mut XInputState) -> u32;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
}

const ERROR_SUCCESS: u32 = 0;
const TRIGGER_THRESHOLD: u8 = 40;

/// Load `XInputGetState` from whichever xinput DLL is present (no import lib).
fn load_xinput() -> Option<XInputGetStateFn> {
    for dll in [c"xinput1_4.dll", c"xinput1_3.dll", c"xinput9_1_0.dll"] {
        let h = unsafe { LoadLibraryA(dll.as_ptr() as *const u8) };
        if !h.is_null() {
            let p = unsafe { GetProcAddress(h, c"XInputGetState".as_ptr() as *const u8) };
            if !p.is_null() {
                return Some(unsafe { std::mem::transmute::<*mut c_void, XInputGetStateFn>(p) });
            }
        }
    }
    None
}

#[derive(Debug)]
pub struct XInput {
    get_state: XInputGetStateFn,
    slot: u32,
    button_ids: Vec<u8>,
    sticks: Vec<(u8, u8)>,
    axis_ids: Vec<u8>,
}

impl XInput {
    fn map(&mut self, inputs: &Inputs) {
        self.button_ids = inputs.buttons.iter().map(|b| b.id).collect();
        self.sticks = inputs.sticks.iter().map(|s| (s.x.id, s.y.id)).collect();
        self.axis_ids = inputs.axes.iter().map(|a| a.axis.id).collect();
    }

    fn read(&self) -> Option<XInputGamepad> {
        let mut st = XInputState::default();
        let res = unsafe { (self.get_state)(self.slot, &mut st) };
        (res == ERROR_SUCCESS).then_some(st.gamepad)
    }
}

/// Stick axis value in screen orientation, normalized to -1.0..1.0.
/// `id`: 0 LX, 1 LY, 2 RX, 3 RY (Y is negated so down is positive).
fn axis(g: &XInputGamepad, id: u8) -> f32 {
    let raw = match id {
        0 => g.thumb_lx as f32,
        1 => -(g.thumb_ly as f32),
        2 => g.thumb_rx as f32,
        3 => -(g.thumb_ry as f32),
        16 => return g.left_trigger as f32 / 255.0,
        17 => return g.right_trigger as f32 / 255.0,
        _ => 0.0,
    };
    (raw / 32767.0).clamp(-1.0, 1.0)
}

fn button(g: &XInputGamepad, id: u8) -> bool {
    match id {
        0..=15 => g.buttons & (1 << id) != 0,
        16 => g.left_trigger > TRIGGER_THRESHOLD,
        17 => g.right_trigger > TRIGGER_THRESHOLD,
        _ => false,
    }
}

impl Backend for XInput {
    type InitState = u32; // XInput slot 0-3
    type Err = Report;

    fn init(slot: Self::InitState, inputs: &Inputs) -> Result<Self, Self::Err> {
        let get_state = load_xinput().ok_or_else(|| eyre!("no XInput DLL available"))?;
        let mut xi = XInput {
            get_state,
            slot,
            button_ids: Vec::new(),
            sticks: Vec::new(),
            axis_ids: Vec::new(),
        };
        xi.map(inputs);
        Ok(xi)
    }

    fn poll(&mut self, state: &mut InputState) -> bool {
        let Some(g) = self.read() else { return false };
        let mut changed = false;
        for (i, &id) in self.button_ids.iter().enumerate() {
            let new = button(&g, id);
            changed |= state.buttons[i] != new;
            state.buttons[i] = new;
        }
        for (i, &(xid, yid)) in self.sticks.iter().enumerate() {
            let new = (axis(&g, xid), axis(&g, yid));
            changed |= state.sticks[i] != new;
            state.sticks[i] = new;
        }
        for (i, &id) in self.axis_ids.iter().enumerate() {
            let new = axis(&g, id);
            changed |= state.axes[i] != new;
            state.axes[i] = new;
        }
        changed
    }

    fn reload(&mut self, inputs: &Inputs) {
        self.map(inputs);
    }
}

/// XInput slots (0-3) that currently have a controller connected.
pub fn get_slots() -> Vec<u32> {
    let Some(get_state) = load_xinput() else { return Vec::new() };
    (0..4)
        .filter(|&slot| {
            let mut st = XInputState::default();
            unsafe { get_state(slot, &mut st) == ERROR_SUCCESS }
        })
        .collect()
}
