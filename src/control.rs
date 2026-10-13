use std::time::{Duration, Instant};
pub fn channel(value: f32, gain: f32, deadzone: f32) -> u8 {
    if !value.is_finite() {
        return 128;
    }
    let value = value.clamp(-1., 1.);
    if value.abs() <= deadzone {
        return 128;
    }
    let value = value.signum() * (value.abs() - deadzone) / (1. - deadzone) * gain;
    (128. + value * if value >= 0. { 127. } else { 128. })
        .round()
        .clamp(0., 255.) as u8
}
pub fn packet(channels: [u8; 4], action: u8, headless: bool) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[..2].copy_from_slice(&[0x66, 20]);
    p[2..6].copy_from_slice(&if action == 2 { [128; 4] } else { channels });
    p[6] = action;
    p[7] = 0x0a | u8::from(headless);
    p[18] = p[2..18].iter().fold(0, |a, b| a ^ b);
    p[19] = 0x99;
    p
}
pub struct Controls {
    pub enabled: bool,
    pub axes: [f32; 4],
    pub gain: f32,
    pub deadzone: f32,
    pub headless: bool,
    pub updated: Instant,
    action: u8,
    until: Instant,
}
impl Default for Controls {
    fn default() -> Self {
        Self {
            enabled: false,
            axes: [0.; 4],
            gain: 0.35,
            deadzone: 0.12,
            headless: false,
            updated: Instant::now(),
            action: 0,
            until: Instant::now(),
        }
    }
}
impl Controls {
    pub fn disable(&mut self) {
        self.enabled = false;
        self.axes = [0.; 4];
        self.action = 0;
    }
    pub fn inhibit_input(&mut self) {
        // Missing sticks/calibration must never cancel a requested motor stop.
        if self.action == 2 && Instant::now() < self.until {
            self.axes = [0.; 4];
        } else {
            self.disable();
        }
    }
    pub fn trigger(&mut self, action: u8) {
        if ![1, 2].contains(&action)
            || (self.action == 2 && Instant::now() < self.until && action != 2)
        {
            return;
        }
        self.enabled = true;
        self.axes = [0.; 4];
        self.action = action;
        self.until = Instant::now() + Duration::from_secs(1);
        self.updated = Instant::now();
    }
    pub fn current_packet(&mut self, now: Instant) -> Option<[u8; 20]> {
        if self.action == 2 && now >= self.until {
            self.disable();
        }
        // A stalled GUI/controller must not leave an indefinitely held channel.
        if now.duration_since(self.updated) > Duration::from_millis(300)
            && !(self.action == 2 && now < self.until)
        {
            self.disable();
        }
        if !self.enabled {
            return None;
        }
        let action = if now < self.until { self.action } else { 0 };
        let channels = std::array::from_fn(|i| {
            channel(
                self.axes[i],
                if i == 2 { 1. } else { self.gain },
                self.deadzone,
            )
        });
        Some(packet(channels, action, self.headless))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn captured_takeoff_and_stop() {
        assert_eq!(
            crate::protocol::hex(&packet([128; 4], 1, false)),
            "661480808080010a000000000000000000000b99"
        );
        assert_eq!(
            crate::protocol::hex(&packet([0, 255, 0, 255], 2, false)),
            "661480808080020a000000000000000000000899"
        );
    }
    #[test]
    fn disabled_stale_and_stop_priority() {
        let mut c = Controls::default();
        assert!(c.current_packet(Instant::now()).is_none());
        c.trigger(2);
        c.trigger(1);
        c.inhibit_input();
        assert_eq!(c.current_packet(Instant::now()).unwrap()[6], 2);
        assert_eq!(
            c.current_packet(Instant::now() + Duration::from_millis(301))
                .unwrap()[6],
            2
        );
        assert!(
            c.current_packet(Instant::now() + Duration::from_millis(1100))
                .is_none()
        );
    }
}
