use sdl2::{
    controller::{Axis, Button, GameController},
    joystick::Joystick,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AxisBinding {
    pub axis: u32,
    pub invert: bool,
    pub min: i32,
    pub center: i32,
    pub max: i32,
}
impl AxisBinding {
    pub fn value(&self, raw: i16) -> f32 {
        if !(self.min < self.center && self.center < self.max) {
            return 0.;
        }
        let raw = raw as i32;
        let value = if raw >= self.center {
            (raw - self.center) as f32 / (self.max - self.center) as f32
        } else {
            (raw - self.center) as f32 / (self.center - self.min) as f32
        };
        value.clamp(-1., 1.) * if self.invert { -1. } else { 1. }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RadioMapping {
    pub channels: [AxisBinding; 4],
    pub takeoff_button: Option<u32>,
    pub stop_button: Option<u32>,
}
impl Default for RadioMapping {
    fn default() -> Self {
        Self {
            channels: std::array::from_fn(|i| AxisBinding {
                axis: i as u32,
                invert: false,
                min: -32768,
                center: 0,
                max: 32767,
            }),
            takeoff_button: None,
            stop_button: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub index: u32,
    pub instance_id: u32,
    pub name: String,
    pub guid: String,
    pub gamepad: bool,
}
#[derive(Clone, Default)]
pub struct Reading {
    pub axes: [f32; 4],
    pub raw: Vec<i16>,
    pub buttons: Vec<bool>,
    pub takeoff: bool,
    pub stop: bool,
    pub issue: Option<String>,
}
enum OpenDevice {
    Gamepad(GameController),
    Raw(Joystick),
}
pub struct Controllers {
    _sdl: sdl2::Sdl,
    joystick: sdl2::JoystickSubsystem,
    gamepad: sdl2::GameControllerSubsystem,
    open: Option<OpenDevice>,
    pub name: String,
    pub guid: String,
    pub raw_mode: bool,
}
impl Controllers {
    pub fn new() -> Result<Self, String> {
        sdl2::hint::set("SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS", "1");
        #[cfg(target_os = "macos")]
        // SDL's HIDAPI backend pumps the default Cocoa run loop during device
        // enumeration, re-entering winit from App::new and hotplug updates.
        // Use SDL's native IOKit / GameController backends on macOS instead.
        sdl2::hint::set("SDL_JOYSTICK_HIDAPI", "0");
        let sdl = sdl2::init()?;
        let joystick = sdl.joystick()?;
        let gamepad = sdl.game_controller()?;
        joystick.set_event_state(false);
        gamepad.set_event_state(false);
        Ok(Self {
            _sdl: sdl,
            joystick,
            gamepad,
            open: None,
            name: String::new(),
            guid: String::new(),
            raw_mode: false,
        })
    }
    pub fn devices(&self) -> Result<Vec<Device>, String> {
        // egui owns the event loop, so SDL discovery must also run before a device is selected.
        self.joystick.update();
        (0..self.joystick.num_joysticks()?)
            .map(|i| {
                Ok(Device {
                    index: i,
                    instance_id: device_instance_id(i)?,
                    name: self.joystick.name_for_index(i).map_err(|e| e.to_string())?,
                    guid: self
                        .joystick
                        .device_guid(i)
                        .map_err(|e| e.to_string())?
                        .to_string(),
                    gamepad: self.gamepad.is_game_controller(i),
                })
            })
            .collect()
    }
    pub fn select(&mut self, device: &Device, raw: bool) -> Result<(), String> {
        self.joystick.update();
        if device_instance_id(device.index)? != device.instance_id {
            return Err("Controller changed or disconnected; select it again".into());
        }
        self.open = None;
        self.open = Some(if device.gamepad && !raw {
            OpenDevice::Gamepad(self.gamepad.open(device.index).map_err(|e| e.to_string())?)
        } else {
            OpenDevice::Raw(
                self.joystick
                    .open(device.index)
                    .map_err(|e| e.to_string())?,
            )
        });
        self.name = device.name.clone();
        self.guid = device.guid.clone();
        self.raw_mode = raw || !device.gamepad;
        Ok(())
    }
    pub fn close(&mut self) {
        self.open = None;
        self.name.clear();
        self.guid.clear();
    }
    pub fn active(&self) -> bool {
        self.open.is_some()
    }
    pub fn read(&mut self, mapping: &RadioMapping) -> Result<Reading, String> {
        self.gamepad.update();
        self.joystick.update();
        match &self.open {
            Some(OpenDevice::Gamepad(g)) => {
                if !g.attached() {
                    return Err("Controller detached".into());
                }
                let raw: Vec<i16> = [Axis::LeftX, Axis::LeftY, Axis::RightX, Axis::RightY]
                    .map(|a| g.axis(a))
                    .to_vec();
                let n = |v: i16| v as f32 / if v < 0 { 32768. } else { 32767. };
                Ok(Reading {
                    axes: [n(raw[2]), -n(raw[3]), -n(raw[1]), n(raw[0])],
                    raw,
                    takeoff: g.button(Button::Y),
                    stop: g.button(Button::B),
                    buttons: vec![g.button(Button::B), g.button(Button::Y)],
                    issue: None,
                })
            }
            Some(OpenDevice::Raw(j)) => {
                if !j.attached() {
                    return Err("USB joystick detached".into());
                }
                let raw: Vec<i16> = (0..j.num_axes())
                    .map(|a| j.axis(a).map_err(|e| e.to_string()))
                    .collect::<Result<_, _>>()?;
                let buttons: Vec<bool> = (0..j.num_buttons())
                    .map(|b| j.button(b).map_err(|e| e.to_string()))
                    .collect::<Result<_, _>>()?;
                let mut axes = [0.; 4];
                let mut issue = None;
                for (i, b) in mapping.channels.iter().enumerate() {
                    if let Some(value) = raw.get(b.axis as usize) {
                        if !(b.min < b.center && b.center < b.max) {
                            issue = Some(format!("Channel {} needs min < neutral < max", i + 1));
                        } else {
                            axes[i] = b.value(*value);
                        }
                    } else {
                        issue = Some(format!("Channel {} maps to missing axis {}", i + 1, b.axis));
                    }
                }
                let mut button = |id: Option<u32>| -> bool {
                    match id {
                        None => false,
                        Some(i) => match buttons.get(i as usize) {
                            Some(b) => *b,
                            None => {
                                issue = Some(format!("Button {i} is unavailable"));
                                false
                            }
                        },
                    }
                };
                let takeoff = button(mapping.takeoff_button);
                let stop = button(mapping.stop_button);
                if issue.is_some() {
                    axes = [0.; 4];
                }
                Ok(Reading {
                    axes,
                    raw,
                    takeoff: takeoff && issue.is_none(),
                    stop: stop && issue.is_none(),
                    buttons,
                    issue,
                })
            }
            None => Ok(Reading::default()),
        }
    }
}
fn device_instance_id(index: u32) -> Result<u32, String> {
    let index = i32::try_from(index).map_err(|_| "Invalid controller index")?;
    // SDL provides an instance ID without opening the device; indices can change after unplugging.
    let id = unsafe { sdl2::sys::SDL_JoystickGetDeviceInstanceID(index) };
    u32::try_from(id).map_err(|_| sdl2::get_error())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn calibrated_axis_respects_nonzero_center_and_inversion() {
        let b = AxisBinding {
            axis: 2,
            invert: false,
            min: -20000,
            center: 1000,
            max: 25000,
        };
        assert_eq!(
            (b.value(-20000), b.value(1000), b.value(25000)),
            (-1., 0., 1.)
        );
        assert_eq!(AxisBinding { invert: true, ..b }.value(25000), -1.);
    }
    #[test]
    fn virtual_usb_joystick_reads_custom_radio_channels_and_buttons() {
        let mut c = Controllers::new().unwrap();
        let index = unsafe {
            sdl2::sys::SDL_JoystickAttachVirtual(
                sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
                6,
                8,
                0,
            )
        };
        assert!(index >= 0);
        let handle = unsafe { sdl2::sys::SDL_JoystickOpen(index) };
        assert!(!handle.is_null());
        let device = c
            .devices()
            .unwrap()
            .into_iter()
            .find(|d| d.index == index as u32)
            .unwrap();
        c.select(&device, true).unwrap();
        let mut mapping = RadioMapping {
            stop_button: Some(1),
            ..Default::default()
        };
        mapping.channels[0].axis = 4;
        mapping.channels[0].invert = true;
        unsafe {
            sdl2::sys::SDL_JoystickSetVirtualAxis(handle, 4, 16384);
            sdl2::sys::SDL_JoystickSetVirtualButton(handle, 1, 1);
        }
        let r = c.read(&mapping).unwrap();
        assert!((r.axes[0] + 0.5).abs() < 0.01);
        assert!(r.stop);
        assert_eq!(r.raw.len(), 6);
        c.close();
        c.gamepad.add_mapping(&format!("{},Virtual Xbox,a:b0,b:b1,x:b2,y:b3,leftx:a0,lefty:a1,rightx:a2,righty:a3,lefttrigger:a4,righttrigger:a5",device.guid)).unwrap();
        c.select(
            &Device {
                gamepad: true,
                ..device.clone()
            },
            false,
        )
        .unwrap();
        unsafe {
            sdl2::sys::SDL_JoystickSetVirtualAxis(handle, 0, -32768);
            sdl2::sys::SDL_JoystickSetVirtualAxis(handle, 1, -16384);
            sdl2::sys::SDL_JoystickSetVirtualAxis(handle, 2, 10000);
            sdl2::sys::SDL_JoystickSetVirtualAxis(handle, 3, -20000);
            sdl2::sys::SDL_JoystickSetVirtualButton(handle, 3, 1);
        }
        let r = c.read(&mapping).unwrap();
        assert!((r.axes[0] - 10000. / 32767.).abs() < 0.001);
        assert!((r.axes[1] - 20000. / 32768.).abs() < 0.001);
        assert_eq!(r.axes[2], 0.5);
        assert_eq!(r.axes[3], -1.);
        assert!(r.takeoff && r.stop);
        c.close();
        unsafe {
            sdl2::sys::SDL_JoystickClose(handle);
            sdl2::sys::SDL_JoystickDetachVirtual(index);
        }
        assert!(
            c.devices()
                .unwrap()
                .iter()
                .all(|d| d.instance_id != device.instance_id)
        );
        let replacement = unsafe {
            sdl2::sys::SDL_JoystickAttachVirtual(
                sdl2::sys::SDL_JoystickType::SDL_JOYSTICK_TYPE_GAMECONTROLLER,
                6,
                8,
                0,
            )
        };
        assert!(replacement >= 0);
        let replacement_device = c
            .devices()
            .unwrap()
            .into_iter()
            .find(|d| d.index == replacement as u32)
            .unwrap();
        assert_ne!(replacement_device.instance_id, device.instance_id);
        assert!(
            c.select(&device, true).is_err(),
            "A stale selection must not open a replacement controller"
        );
        c.select(&replacement_device, true).unwrap();
        assert!(c.active());
        c.close();
        unsafe {
            sdl2::sys::SDL_JoystickDetachVirtual(replacement);
        }
    }
}
