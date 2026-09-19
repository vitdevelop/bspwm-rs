//! The compositor's view of real outputs and input devices, as
//! `bspc output`/`bspc input` (`docs/design.md`, "Configuration beyond
//! bspwm") see them.
//!
//! `bsp_ipc::exec` runs with only `&mut Wm`, the node registry and the
//! `Adapter` (`crate::adapter::WindowAdapter`) — no `Output`, DRM device
//! or libinput handle. So this module keeps a plain-data *model* of the
//! hardware (kept current by the backend on connector/device hotplug),
//! validates each requested change against it synchronously (so a bad
//! mode or an unknown device fails the command with a proper reply), and
//! queues the validated change; `crate::ipc::execute_and_broadcast`
//! then drains the queue and applies it for real on `State`.

use bsp_ipc::command::{InputAction, OutputAction, OutputMode};

/// The virtual input device name that addresses the seat keyboard
/// (`bspc input keyboard -r RATE DELAY`).
pub const KEYBOARD: &str = "keyboard";

/// Two refresh rates (millihertz) this close are the same mode: `bspc
/// output -m 1920x1080@60` must match a real 59.94 Hz mode.
const REFRESH_TOLERANCE_MHZ: i32 = 500;

/// One known output.
#[derive(Debug, Clone, PartialEq)]
pub struct HwOutput {
    /// Connector-shaped name, e.g. `HDMI-A-1`.
    pub name: String,
    /// Every mode the output supports.
    pub modes: Vec<OutputMode>,
    /// The mode in use.
    pub mode: OutputMode,
    /// Fractional scale.
    pub scale: f64,
    /// Position in the global (logical) layout.
    pub position: (i32, i32),
    /// Rotation and flip.
    pub transform: bsp_ipc::command::OutputTransform,
}

/// Seat keyboard repeat and per-pointer acceleration, plus queued changes.
#[derive(Debug, Clone, Default)]
pub struct HwModel {
    /// Known outputs, in creation order.
    pub outputs: Vec<HwOutput>,
    /// `(name, acceleration)` per known pointer device.
    pub pointers: Vec<(String, f64)>,
    /// Keyboard repeat `(rate, delay_ms)`.
    pub repeat: (i32, i32),
    /// Validated, not yet applied output changes.
    pub pending_outputs: Vec<(String, OutputAction)>,
    /// Validated, not yet applied input changes.
    pub pending_inputs: Vec<(String, InputAction)>,
}

/// The mode of `modes` matching `wanted`: same size, nearest refresh
/// within [`REFRESH_TOLERANCE_MHZ`].
pub fn resolve_mode(modes: &[OutputMode], wanted: OutputMode) -> Option<OutputMode> {
    modes
        .iter()
        .filter(|m| m.width == wanted.width && m.height == wanted.height)
        .filter(|m| (m.refresh_mhz - wanted.refresh_mhz).abs() <= REFRESH_TOLERANCE_MHZ)
        .min_by_key(|m| (m.refresh_mhz - wanted.refresh_mhz).abs())
        .copied()
}

fn format_mode(m: OutputMode) -> String {
    format!("{}x{}@{:.2}", m.width, m.height, m.refresh_mhz as f64 / 1000.0)
}

impl HwModel {
    /// Every output name, in creation order.
    pub fn output_names(&self) -> Vec<String> {
        self.outputs.iter().map(|o| o.name.clone()).collect()
    }

    /// `name`'s settings: current mode, scale, position, then the
    /// available modes.
    pub fn output_settings(&self, name: &str) -> Option<String> {
        let o = self.outputs.iter().find(|o| o.name == name)?;
        let modes: Vec<String> = o.modes.iter().map(|m| format_mode(*m)).collect();
        Some(format!(
            "mode {}\nscale {}\ntransform {}\nposition {} {}\nmodes {}\n",
            format_mode(o.mode),
            o.scale,
            o.transform.name(),
            o.position.0,
            o.position.1,
            modes.join(" ")
        ))
    }

    /// Whether `action` would be accepted for output `name`, without
    /// recording or queueing anything (`zwlr_output_configuration_v1.test`).
    pub fn check_output(&self, name: &str, action: &OutputAction) -> Result<(), String> {
        self.clone().set_output(name, action)
    }

    /// Validates `action` against output `name`, records it in the model
    /// and queues it for the backend to apply.
    pub fn set_output(&mut self, name: &str, action: &OutputAction) -> Result<(), String> {
        let Some(o) = self.outputs.iter_mut().find(|o| o.name == name) else {
            return Err(format!("output: unknown output '{name}'.\n"));
        };
        let action = match action {
            OutputAction::SetMode(wanted) => {
                let Some(mode) = resolve_mode(&o.modes, *wanted) else {
                    return Err(format!(
                        "output: '{name}' has no mode {}.\n",
                        format_mode(*wanted)
                    ));
                };
                o.mode = mode;
                OutputAction::SetMode(mode)
            }
            OutputAction::SetScale(s) => {
                if !s.is_finite() || *s <= 0.0 {
                    return Err("output: scale must be a positive number.\n".to_string());
                }
                o.scale = *s;
                action.clone()
            }
            OutputAction::SetPosition(x, y) => {
                o.position = (*x, *y);
                action.clone()
            }
            OutputAction::SetTransform(t) => {
                o.transform = *t;
                action.clone()
            }
        };
        self.pending_outputs.push((name.to_string(), action));
        Ok(())
    }

    /// Every input device name: the virtual `keyboard`, then each pointer.
    pub fn input_names(&self) -> Vec<String> {
        std::iter::once(KEYBOARD.to_string())
            .chain(self.pointers.iter().map(|(n, _)| n.clone()))
            .collect()
    }

    /// `device`'s settings.
    pub fn input_settings(&self, device: &str) -> Option<String> {
        if device == KEYBOARD {
            return Some(format!("rate {}\ndelay {}\n", self.repeat.0, self.repeat.1));
        }
        let (_, accel) = self.pointers.iter().find(|(n, _)| n == device)?;
        Some(format!("accel {accel}\n"))
    }

    /// Validates `action` against `device`, records and queues it.
    pub fn set_input(&mut self, device: &str, action: &InputAction) -> Result<(), String> {
        match action {
            InputAction::SetRepeat { rate, delay } => {
                if device != KEYBOARD {
                    return Err(format!("input: '{device}' is not a keyboard (use 'keyboard').\n"));
                }
                if *rate < 0 || *delay < 0 {
                    return Err("input: rate and delay must not be negative.\n".to_string());
                }
                self.repeat = (*rate, *delay);
            }
            InputAction::SetAccel(a) => {
                if !a.is_finite() || !(-1.0..=1.0).contains(a) {
                    return Err("input: accel must be between -1 and 1.\n".to_string());
                }
                let Some(p) = self.pointers.iter_mut().find(|(n, _)| n == device) else {
                    return Err(format!("input: unknown pointer device '{device}'.\n"));
                };
                p.1 = *a;
            }
        }
        self.pending_inputs.push((device.to_string(), action.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: i32, h: i32, mhz: i32) -> OutputMode {
        OutputMode { width: w, height: h, refresh_mhz: mhz }
    }

    fn model() -> HwModel {
        let m = mode(1920, 1080, 59_940);
        HwModel {
            outputs: vec![HwOutput {
                name: "HDMI-A-1".into(),
                modes: vec![m, mode(1920, 1080, 30_000), mode(1280, 720, 60_000)],
                mode: m,
                scale: 1.0,
                position: (0, 0),
                transform: Default::default(),
            }],
            pointers: vec![("mouse".into(), 0.0)],
            repeat: (25, 600),
            ..Default::default()
        }
    }

    #[test]
    fn resolve_mode_picks_nearest_refresh_within_tolerance() {
        let m = model();
        let got = resolve_mode(&m.outputs[0].modes, mode(1920, 1080, 60_000));
        assert_eq!(got, Some(mode(1920, 1080, 59_940)));
        assert_eq!(resolve_mode(&m.outputs[0].modes, mode(1920, 1080, 50_000)), None);
        assert_eq!(resolve_mode(&m.outputs[0].modes, mode(800, 600, 60_000)), None);
    }

    #[test]
    fn set_output_mode_records_resolved_mode_and_queues() {
        let mut m = model();
        m.set_output("HDMI-A-1", &OutputAction::SetMode(mode(1280, 720, 60_000))).unwrap();
        assert_eq!(m.outputs[0].mode, mode(1280, 720, 60_000));
        assert_eq!(m.pending_outputs.len(), 1);
    }

    #[test]
    fn set_output_rejects_bad_input_without_queueing() {
        let mut m = model();
        assert!(m.set_output("nope", &OutputAction::SetScale(1.0)).is_err());
        assert!(m.set_output("HDMI-A-1", &OutputAction::SetScale(0.0)).is_err());
        assert!(m.set_output("HDMI-A-1", &OutputAction::SetMode(mode(1, 1, 60_000))).is_err());
        assert!(m.pending_outputs.is_empty());
    }

    #[test]
    fn output_settings_lists_mode_scale_position() {
        let s = model().output_settings("HDMI-A-1").unwrap();
        assert!(s.contains("mode 1920x1080@59.94"));
        assert!(s.contains("scale 1"));
        assert!(s.contains("position 0 0"));
        assert!(model().output_settings("x").is_none());
    }

    #[test]
    fn input_names_start_with_keyboard() {
        assert_eq!(model().input_names(), vec!["keyboard", "mouse"]);
    }

    #[test]
    fn repeat_only_on_keyboard_accel_only_on_pointers() {
        let mut m = model();
        assert!(m.set_input("mouse", &InputAction::SetRepeat { rate: 30, delay: 300 }).is_err());
        assert!(m.set_input("keyboard", &InputAction::SetAccel(0.5)).is_err());
        m.set_input("keyboard", &InputAction::SetRepeat { rate: 30, delay: 300 }).unwrap();
        m.set_input("mouse", &InputAction::SetAccel(0.5)).unwrap();
        assert_eq!(m.input_settings("keyboard").unwrap(), "rate 30\ndelay 300\n");
        assert_eq!(m.input_settings("mouse").unwrap(), "accel 0.5\n");
        assert_eq!(m.pending_inputs.len(), 2);
    }

    #[test]
    fn accel_out_of_range_rejected() {
        let mut m = model();
        assert!(m.set_input("mouse", &InputAction::SetAccel(2.0)).is_err());
    }
}
