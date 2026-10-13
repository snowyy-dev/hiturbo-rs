use eframe::egui::{self, RichText, TextureHandle, TextureOptions, Vec2};
use hiturbo_rs::{
    capture::{self, CaptureSettings},
    control::Controls,
    input::{Controllers, Device, RadioMapping, Reading},
    session::{self, Event, NetworkConfig, Preview, Shared, Snapshot, Worker},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, VecDeque},
    net::{IpAddr, Ipv4Addr},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    drone: String,
    interface: String,
    local: String,
    root: PathBuf,
    rotate: bool,
    filter: String,
    zoom: f32,
    packet_log: bool,
    stream: bool,
    every: u32,
    sample: bool,
    gain: f32,
    deadzone: f32,
    radios: HashMap<String, RadioMapping>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            drone: "172.16.10.1".into(),
            interface: String::new(),
            local: "172.16.10.2".into(),
            root: default_capture_root(),
            rotate: true,
            filter: "Original".into(),
            zoom: 1.,
            packet_log: true,
            stream: true,
            every: 10,
            sample: false,
            gain: 0.35,
            deadzone: 0.12,
            radios: HashMap::new(),
        }
    }
}
fn default_capture_root() -> PathBuf {
    let folder = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    let folder = if folder
        .file_name()
        .is_some_and(|n| n == "release" || n == "debug")
        && folder
            .parent()
            .is_some_and(|p| p.file_name().is_some_and(|n| n == "target"))
    {
        folder
            .parent()
            .and_then(|p| p.parent())
            .unwrap_or(&folder)
            .to_path_buf()
    } else {
        folder
    };
    folder.join("captures")
}
#[derive(Clone)]
struct Interface {
    name: String,
    ip: Ipv4Addr,
}
pub struct App {
    settings: Settings,
    interfaces: Vec<Interface>,
    events: Receiver<Event>,
    shared: Shared,
    worker: Option<Worker>,
    state: String,
    logs: VecDeque<String>,
    errors: VecDeque<String>,
    snapshot: Snapshot,
    frame: Option<Preview>,
    texture: Option<TextureHandle>,
    displayed: Option<image::RgbaImage>,
    effect_key: String,
    window_title: String,
    controllers: Option<Controllers>,
    devices: Vec<Device>,
    selected_device: Option<usize>,
    next_device_refresh: Instant,
    device_error: Option<String>,
    force_raw: bool,
    input_source: u8,
    reading: Reading,
    previous_buttons: Option<(bool, bool)>,
    axis: [f32; 4],
    keyboard_active: bool,
    slider_active: [bool; 4],
    mapping: RadioMapping,
    learn: Option<(usize, Vec<i16>)>,
    input_issue: Option<String>,
    settings_open: bool,
    controller_open: bool,
    metadata_open: bool,
    metadata: serde_json::Value,
    log_filter: String,
    log_follow: bool,
    pending_replay: Option<PathBuf>,
    smoke: bool,
    smoke_started: Instant,
    smoke_done: bool,
}
impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, replay: Option<PathBuf>, smoke: bool) -> Self {
        let settings = if smoke {
            Settings::default()
        } else {
            cc.storage
                .and_then(|s| eframe::get_value(s, "settings"))
                .unwrap_or_default()
        };
        let (tx, rx) = mpsc::sync_channel(512);
        let shared = Shared {
            latest: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            controls: Arc::new(Mutex::new(Controls::default())),
            capture: Arc::new(Mutex::new(CaptureSettings::default())),
            ready: Arc::new(AtomicBool::new(false)),
            events: tx,
        };
        let mut app = Self {
            settings,
            interfaces: vec![],
            events: rx,
            shared,
            worker: None,
            state: "No drone connected".into(),
            logs: VecDeque::new(),
            errors: VecDeque::new(),
            snapshot: Snapshot::default(),
            frame: None,
            texture: None,
            displayed: None,
            effect_key: String::new(),
            window_title: String::new(),
            controllers: None,
            devices: vec![],
            selected_device: None,
            next_device_refresh: Instant::now(),
            device_error: None,
            force_raw: false,
            input_source: 0,
            reading: Reading::default(),
            previous_buttons: None,
            axis: [0.; 4],
            keyboard_active: false,
            slider_active: [false; 4],
            mapping: RadioMapping::default(),
            learn: None,
            input_issue: None,
            settings_open: false,
            controller_open: false,
            metadata_open: false,
            metadata: serde_json::Value::Null,
            log_filter: String::new(),
            log_follow: true,
            pending_replay: replay,
            smoke,
            smoke_started: Instant::now(),
            smoke_done: false,
        };
        app.refresh_interfaces();
        match Controllers::new() {
            Ok(c) => app.controllers = Some(c),
            Err(e) => {
                app.log(format!("Controller input unavailable: {e}"));
                app.device_error = Some(e);
            }
        }
        app.refresh_devices();
        app.log("Ready · JPEG preview · rotation 180° · flight link off");
        app
    }
    fn log(&mut self, text: impl Into<String>) {
        self.logs
            .push_back(format!("[{:.3}] {}", session::epoch(), text.into()));
        while self.logs.len() > 2000 {
            self.logs.pop_front();
        }
    }
    fn error(&mut self, text: String) {
        self.log(format!("ERROR · {text}"));
        self.errors.push_back(text);
    }
    fn refresh_interfaces(&mut self) {
        match if_addrs::get_if_addrs() {
            Ok(interfaces) => {
                self.interfaces = interfaces
                    .into_iter()
                    .filter_map(|i| match i.ip() {
                        IpAddr::V4(ip) if !ip.is_loopback() => Some(Interface { name: i.name, ip }),
                        _ => None,
                    })
                    .collect();
                self.interfaces.sort_by_key(|i| (i.name.clone(), i.ip));
                if self.settings.interface.is_empty()
                    && let Some(i) = self
                        .interfaces
                        .iter()
                        .find(|i| i.ip.octets()[..3] == [172, 16, 10])
                        .or_else(|| self.interfaces.first())
                {
                    self.settings.interface = i.name.clone();
                    self.settings.local = i.ip.to_string();
                }
                self.log(format!(
                    "Found {} IPv4 interface addresses",
                    self.interfaces.len()
                ));
            }
            Err(e) => self.error(e.to_string()),
        }
    }
    fn refresh_devices(&mut self) {
        self.next_device_refresh = Instant::now() + Duration::from_millis(500);
        let selected = self
            .selected_device
            .and_then(|i| self.devices.get(i))
            .map(|d| d.instance_id);
        if let Some(c) = &self.controllers {
            match c.devices() {
                Ok(d) => {
                    if d != self.devices {
                        self.log(format!("Detected {} controller devices", d.len()));
                    }
                    self.selected_device = selected
                        .and_then(|id| d.iter().position(|device| device.instance_id == id))
                        .or_else(|| d.iter().position(|device| device.gamepad))
                        .or_else(|| (d.len() == 1).then_some(0));
                    self.devices = d;
                    self.device_error = None;
                }
                Err(e) => {
                    if self.device_error.as_ref() != Some(&e) {
                        self.error(e.clone());
                    }
                    self.device_error = Some(e);
                }
            }
        }
    }
    fn config(&self) -> Result<NetworkConfig, String> {
        let drone = self
            .settings
            .drone
            .parse::<Ipv4Addr>()
            .map_err(|e| e.to_string())?;
        let local = self
            .settings
            .local
            .parse::<Ipv4Addr>()
            .map_err(|e| e.to_string())?;
        if local.is_unspecified() || local == Ipv4Addr::new(10, 215, 173, 1) {
            return Err("Choose the actual drone Wi-Fi address".into());
        }
        Ok(NetworkConfig {
            drone,
            local,
            root: self.settings.root.clone(),
            packet_log: self.settings.packet_log,
            save_stream: self.settings.stream,
        })
    }
    fn connect(&mut self) {
        if self.worker.is_some() {
            return;
        }
        match self.config() {
            Ok(c) => {
                self.shared.controls.lock().unwrap().disable();
                self.frame = None;
                self.texture = None;
                self.state = "Connecting".into();
                self.worker = Some(Worker::live(c, self.shared.clone()));
            }
            Err(e) => self.error(e),
        }
    }
    fn disconnect(&mut self) {
        if let Some(mut w) = self.worker.take() {
            w.stop();
        }
        self.shared.controls.lock().unwrap().disable();
        self.axis = [0.; 4];
        // Drain the stopped worker's Ended event before a subsequent connect.
        self.events();
    }
    fn open_replay(&mut self, path: Option<PathBuf>) {
        if self.worker.is_some() {
            self.error("Disconnect before opening a recording".into());
            return;
        }
        self.shared.controls.lock().unwrap().disable();
        let path = path.or_else(|| {
            rfd::FileDialog::new()
                .add_filter("Camera recording", &["jsonl", "mjpeg", "mjpg"])
                .pick_file()
        });
        if let Some(path) = path {
            self.frame = None;
            self.texture = None;
            self.state = "Opening recording".into();
            self.worker = Some(Worker::replay(
                path,
                self.settings.root.clone(),
                self.shared.clone(),
            ));
        }
    }
    fn send(&mut self, p: Vec<u8>) {
        if !self.shared.ready.load(Ordering::Relaxed) {
            return;
        }
        if let Some(w) = &self.worker
            && let Err(e) = w.commands.send(p)
        {
            self.error(e.to_string());
        }
    }
    fn trigger(&mut self, action: u8) {
        if action == 1 && (self.controller_open || self.settings_open || self.input_issue.is_some())
        {
            return;
        }
        if self.shared.ready.load(Ordering::Relaxed) {
            self.axis = [0.; 4];
            self.shared.controls.lock().unwrap().trigger(action);
            self.log(if action == 2 {
                "Emergency stop · one-second motor-stop pulse"
            } else {
                "Takeoff/land · one-second pulse"
            });
        }
    }
    fn input(&mut self, ctx: &egui::Context) {
        let ready = self.shared.ready.load(Ordering::Relaxed);
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && ready {
            self.trigger(2);
        }
        if !ready {
            self.shared.controls.lock().unwrap().inhibit_input();
        }
        if self.input_source != 0 {
            let result = self.controllers.as_mut().map(|c| c.read(&self.mapping));
            match result {
                Some(Ok(reading)) => {
                    if reading.issue.is_some() {
                        self.shared.controls.lock().unwrap().inhibit_input();
                    }
                    if reading.issue != self.input_issue {
                        if let Some(e) = &reading.issue {
                            self.log(format!("Controller setup: {e}"));
                        }
                        self.input_issue = reading.issue.clone();
                    }
                    self.axis = reading.axes;
                    if let Some((takeoff, stop)) = self.previous_buttons {
                        if reading.stop && !stop && ready {
                            self.trigger(2);
                        } else if reading.takeoff
                            && !takeoff
                            && ready
                            && self.shared.controls.lock().unwrap().enabled
                        {
                            self.trigger(1);
                        }
                    }
                    self.previous_buttons = Some((reading.takeoff, reading.stop));
                    if let Some((channel, baseline)) = &self.learn
                        && let Some((axis, _)) = reading
                            .raw
                            .iter()
                            .enumerate()
                            .filter_map(|(i, v)| {
                                baseline.get(i).map(|b| (i, (*v as i32 - *b as i32).abs()))
                            })
                            .max_by_key(|(_, delta)| *delta)
                            .filter(|(_, d)| *d > 6000)
                    {
                        self.mapping.channels[*channel].axis = axis as u32;
                        self.learn = None;
                    }
                    self.reading = reading;
                }
                Some(Err(e)) => {
                    self.log(format!("{e}; flight link disabled"));
                    self.shared.controls.lock().unwrap().inhibit_input();
                    if let Some(c) = &mut self.controllers {
                        c.close();
                    }
                    self.input_source = 0;
                    self.axis = [0.; 4];
                    self.previous_buttons = None;
                }
                None => {}
            }
        } else {
            let focused = ctx.input(|i| i.focused);
            if !focused {
                self.shared.controls.lock().unwrap().inhibit_input();
                self.axis = [0.; 4];
            }
            if !ctx.wants_keyboard_input()
                && focused
                && !self.controller_open
                && !self.settings_open
            {
                let key_axes = ctx.input(|i| {
                    [
                        (i.key_down(egui::Key::D) as i8 - i.key_down(egui::Key::A) as i8) as f32,
                        (i.key_down(egui::Key::W) as i8 - i.key_down(egui::Key::S) as i8) as f32,
                        (i.key_down(egui::Key::ArrowUp) as i8
                            - i.key_down(egui::Key::ArrowDown) as i8)
                            as f32,
                        (i.key_down(egui::Key::ArrowRight) as i8
                            - i.key_down(egui::Key::ArrowLeft) as i8)
                            as f32,
                    ]
                });
                if key_axes.iter().any(|v| *v != 0.) {
                    self.axis = key_axes;
                    self.keyboard_active = true;
                } else if self.keyboard_active {
                    self.axis = [0.; 4];
                    self.keyboard_active = false;
                }
                if ctx.input(|i| i.key_pressed(egui::Key::Space))
                    && self.shared.controls.lock().unwrap().enabled
                {
                    self.trigger(1);
                }
            } else if self.keyboard_active {
                self.axis = [0.; 4];
                self.keyboard_active = false;
            }
        }
        let mut controls = self.shared.controls.lock().unwrap();
        controls.axes = self.axis;
        controls.updated = Instant::now();
        controls.gain = self.settings.gain;
        controls.deadzone = self.settings.deadzone;
    }
    fn events(&mut self) {
        for _ in 0..256 {
            let Ok(e) = self.events.try_recv() else {
                break;
            };
            match e {
                Event::Log(s) => self.log(s),
                Event::State(s) => self.state = s,
                Event::Error(s) => self.error(s),
                Event::Ended => {
                    self.worker.take();
                    self.shared.controls.lock().unwrap().disable();
                }
                Event::Photo(path) => {
                    self.log(format!("Original JPEG saved: {}", path.display()));
                    self.inspect(path);
                }
            }
        }
        self.snapshot = self.shared.snapshot.lock().unwrap().clone();
        let latest = self.shared.latest.lock().unwrap().take();
        if let Some(p) = latest {
            self.frame = Some(p);
            self.effect_key.clear();
        }
    }
    fn update_texture(&mut self, ctx: &egui::Context) {
        let key = format!(
            "{}:{}:{}",
            self.settings.rotate, self.settings.filter, self.settings.zoom
        );
        if key == self.effect_key {
            return;
        }
        let Some(frame) = &self.frame else {
            return;
        };
        let mut image =
            image::RgbaImage::from_raw(frame.width, frame.height, frame.rgba.clone()).unwrap();
        if self.settings.rotate {
            image = image::imageops::rotate180(&image);
        }
        if self.settings.zoom > 1. {
            let w = (image.width() as f32 / self.settings.zoom) as u32;
            let h = (image.height() as f32 / self.settings.zoom) as u32;
            let (x, y) = ((image.width() - w) / 2, (image.height() - h) / 2);
            image = image::imageops::crop_imm(&image, x, y, w, h).to_image();
        }
        match self.settings.filter.as_str() {
            "Grayscale" => {
                for p in image.pixels_mut() {
                    let y = (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) as u8;
                    p[0] = y;
                    p[1] = y;
                    p[2] = y;
                }
            }
            "Contrast" => image = image::imageops::contrast(&image, 25.),
            "Warm" => {
                for p in image.pixels_mut() {
                    p[0] = (p[0] as f32 * 1.08).min(255.) as u8;
                    p[2] = (p[2] as f32 * 0.9) as u8;
                }
            }
            _ => {}
        }
        let color = egui::ColorImage::from_rgba_unmultiplied(
            [image.width() as usize, image.height() as usize],
            image.as_raw(),
        );
        if let Some(t) = &mut self.texture {
            t.set(color, TextureOptions::LINEAR);
        } else {
            self.texture = Some(ctx.load_texture("camera", color, TextureOptions::LINEAR));
        }
        self.displayed = Some(image);
        self.effect_key = key;
    }
    fn inspect(&mut self, path: PathBuf) {
        match std::fs::read(&path) {
            Ok(data) => {
                self.metadata = capture::metadata(&data);
                self.metadata_open = true;
            }
            Err(e) => self.error(e.to_string()),
        }
    }
    fn save_frame(&mut self, original: bool) {
        if self.frame.is_none() {
            self.error("Wait for a camera image first".into());
            return;
        }
        let path = if original {
            rfd::FileDialog::new()
                .add_filter("Camera JPEG", &["jpg"])
                .set_file_name("camera-frame.jpg")
                .save_file()
        } else {
            rfd::FileDialog::new()
                .add_filter("PNG", &["png"])
                .set_file_name("preview.png")
                .save_file()
        };
        if let Some(path) = path {
            let result = if original {
                std::fs::write(&path, self.frame.as_ref().unwrap().jpeg.as_ref())
                    .map_err(|e| e.to_string())
            } else {
                self.displayed
                    .as_ref()
                    .ok_or("No preview".into())
                    .and_then(|im| im.save(&path).map_err(|e| e.to_string()))
            };
            match result {
                Ok(()) => self.log(format!("Saved {}", path.display())),
                Err(e) => self.error(e),
            }
        }
    }
    fn pick_folder(&mut self) {
        if let Some(p) = rfd::FileDialog::new()
            .set_directory(&self.settings.root)
            .pick_folder()
        {
            self.settings.root = p;
        }
    }
    fn top(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("menus").show(ctx, |ui| {
            egui::MenuBar::new().ui(ui, |ui| {
                ui.menu_button("File", |ui| {
                    if ui.button("Open recording…").clicked() {
                        ui.close();
                        self.open_replay(None);
                    }
                    if ui
                        .add_enabled(
                            self.frame.is_some(),
                            egui::Button::new("Save original camera JPEG…"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.save_frame(true);
                    }
                    if ui
                        .add_enabled(
                            self.frame.is_some(),
                            egui::Button::new("Save displayed preview…"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.save_frame(false);
                    }
                    if ui.button("Inspect photo metadata…").clicked() {
                        ui.close();
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("Image", &["jpg", "jpeg", "png"])
                            .pick_file()
                        {
                            self.inspect(p);
                        }
                    }
                    if ui.button("Export debug log…").clicked() {
                        ui.close();
                        if let Some(p) = rfd::FileDialog::new()
                            .set_file_name("hiturbo-debug.log")
                            .save_file()
                        {
                            let content = self.logs.iter().cloned().collect::<Vec<_>>().join("\n");
                            if let Err(e) = std::fs::write(p, content) {
                                self.error(e.to_string());
                            }
                        }
                    }
                    ui.separator();
                    if ui.button("Exit").clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
                ui.menu_button("Connection", |ui| {
                    if ui
                        .add_enabled(self.worker.is_none(), egui::Button::new("Connect"))
                        .clicked()
                    {
                        ui.close();
                        self.connect();
                    }
                    if ui
                        .add_enabled(
                            self.worker.is_some(),
                            egui::Button::new("Disconnect / stop replay"),
                        )
                        .clicked()
                    {
                        ui.close();
                        self.disconnect();
                    }
                    if ui.button("Network & storage settings…").clicked() {
                        ui.close();
                        self.settings_open = true;
                    }
                });
                if ui.button("Controller setup…").clicked() {
                    self.shared.controls.lock().unwrap().inhibit_input();
                    self.controller_open = true;
                    self.refresh_devices();
                }
            });
        });
    }
    fn log_panel(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::bottom("debug")
            .resizable(true)
            .default_height(170.)
            .min_height(80.)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.strong("Debug log");
                    ui.checkbox(&mut self.log_follow, "Follow");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.log_filter)
                            .hint_text("Filter log")
                            .desired_width(220.),
                    );
                    if ui.button("Clear").clicked() {
                        self.logs.clear();
                    }
                });
                egui::ScrollArea::vertical()
                    .stick_to_bottom(self.log_follow)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        for line in &self.logs {
                            if self.log_filter.is_empty()
                                || line
                                    .to_lowercase()
                                    .contains(&self.log_filter.to_lowercase())
                            {
                                ui.label(RichText::new(line).monospace().size(11.).color(
                                    if line.contains("ERROR") {
                                        ui.visuals().error_fg_color
                                    } else {
                                        ui.visuals().text_color()
                                    },
                                ));
                            }
                        }
                    });
            });
    }
    fn left(&mut self, ctx: &egui::Context) {
        egui::SidePanel::left("information")
            .default_width(255.)
            .width_range(215. ..=380.)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Information");
                    ui.separator();
                    ui.strong("Connection");
                    ui.label(format!("Drone  {}", self.settings.drone));
                    ui.label(format!("Interface  {}", self.settings.interface));
                    ui.label(format!("Local IPv4  {}", self.settings.local));
                    ui.label(&self.state);
                    ui.separator();
                    ui.strong("Camera stream");
                    if let Some(f) = &self.frame {
                        ui.label(format!("{} × {} · JPEG", f.width, f.height));
                    }
                    let s = &self.snapshot;
                    ui.label(format!("{} received frames", s.stats.frames));
                    ui.label(format!(
                        "{} decoded · {} errors",
                        s.decoded, s.decode_errors
                    ));
                    ui.label(format!(
                        "{:.1} frames/s average",
                        s.decoded as f64 / s.elapsed.max(1.)
                    ));
                    ui.label(format!("{} UDP packets", s.stats.packets));
                    ui.label(format!(
                        "{} incomplete · {} invalid",
                        s.stats.incomplete, s.stats.invalid
                    ));
                    ui.label(format!("{} duplicate fragments", s.stats.duplicates));
                    ui.label(format!("{} sequence gaps", s.stats.sequence_gaps));
                    if s.stats.unsupported > 0 {
                        ui.label(format!("{} unsupported envelopes", s.stats.unsupported));
                    }
                    ui.separator();
                    ui.strong("Frame capture");
                    ui.label(format!(
                        "{} saved / {} received",
                        s.capture.saved, s.capture.received
                    ));
                    ui.label(format!("{:.2} MiB", s.capture.bytes as f64 / 1048576.));
                    if let Some(folder) = &s.capture.folder {
                        ui.small(folder.to_string_lossy());
                    }
                    ui.separator();
                    ui.strong("Device");
                    ui.label(match s.reported_battery.filter(|value| *value <= 100) {
                        Some(value) => format!("Battery: {value}%"),
                        None => "Battery: —".into(),
                    });
                    if let Some(d) = &s.descriptor {
                        ui.label(format!(
                            "Maker {} · type {} · {}",
                            d.chip_maker, d.chip_type, d.transport
                        ));
                        ui.label(format!(
                            "Device ID {} · pixel ID {}",
                            d.device_id, d.pixel_id
                        ));
                        ui.label(format!("Advertised {} fps", d.advertised_fps));
                    }
                    for (key, value) in &s.info {
                        ui.label(format!("{key}: {value}"));
                    }
                    ui.separator();
                    ui.strong("Input");
                    ui.label(if self.input_source == 0 {
                        "Keyboard / sliders"
                    } else if self.input_source == 1 {
                        "Xbox / mapped gamepad"
                    } else {
                        "USB joystick / RadioMaster"
                    });
                    if let Some(c) = &self.controllers
                        && c.active()
                    {
                        ui.label(&c.name);
                    }
                    if let Some(issue) = &self.input_issue {
                        ui.colored_label(ui.visuals().error_fg_color, issue);
                    }
                    ui.separator();
                    ui.strong("Session files");
                    if let Some(folder) = &s.folder {
                        ui.small(folder.to_string_lossy());
                    }
                    if ui
                        .add_enabled(
                            self.frame.is_some(),
                            egui::Button::new("Current frame metadata…"),
                        )
                        .clicked()
                    {
                        self.metadata =
                            capture::metadata(self.frame.as_ref().unwrap().jpeg.as_ref());
                        self.metadata_open = true;
                    }
                });
            });
    }
    fn right(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("controls")
            .default_width(295.)
            .width_range(270. ..=420.)
            .resizable(true)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    ui.heading("Controls & settings");
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(self.worker.is_none(), egui::Button::new("Connect"))
                            .clicked()
                        {
                            self.connect();
                        }
                        if ui
                            .add_enabled(self.worker.is_some(), egui::Button::new("Disconnect"))
                            .clicked()
                        {
                            self.disconnect();
                        }
                        if ui.button("Settings…").clicked() {
                            self.settings_open = true;
                        }
                    });
                    ui.separator();
                    let ready = self.shared.ready.load(Ordering::Relaxed);
                    if ui
                        .add_enabled(
                            ready,
                            egui::Button::new(RichText::new("EMERGENCY STOP · Esc").strong())
                                .min_size(Vec2::new(ui.available_width(), 36.)),
                        )
                        .clicked()
                    {
                        self.trigger(2);
                    }
                    ui.horizontal(|ui| {
                        let mut enabled = self.shared.controls.lock().unwrap().enabled;
                        if ui
                            .add_enabled(
                                ready && self.input_issue.is_none() && !self.controller_open,
                                egui::Checkbox::new(&mut enabled, "Flight link · 20 Hz"),
                            )
                            .changed()
                        {
                            let mut c = self.shared.controls.lock().unwrap();
                            if enabled {
                                c.enabled = true;
                                c.updated = Instant::now();
                            } else {
                                c.inhibit_input();
                                self.axis = [0.; 4];
                            }
                        }
                        if ui
                            .add_enabled(ready, egui::Button::new("Takeoff / land"))
                            .clicked()
                        {
                            self.trigger(1);
                        }
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Controller setup…").clicked() {
                            self.shared.controls.lock().unwrap().inhibit_input();
                            self.controller_open = true;
                            self.refresh_devices();
                        }
                        if ui.button("Keyboard").clicked() {
                            self.shared.controls.lock().unwrap().inhibit_input();
                            self.input_source = 0;
                            self.axis = [0.; 4];
                        }
                    });
                    for (i, label) in ["Roll", "Pitch", "Throttle", "Yaw"].iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(*label);
                            let r = ui.add_enabled(
                                self.input_source == 0,
                                egui::Slider::new(&mut self.axis[i], -1. ..=1.).fixed_decimals(2),
                            );
                            if r.dragged() {
                                self.slider_active[i] = true;
                            } else if self.slider_active[i] {
                                self.axis[i] = 0.;
                                self.slider_active[i] = false;
                            }
                        });
                    }
                    ui.add(
                        egui::Slider::new(&mut self.settings.gain, 0.1..=1.).text("Sensitivity"),
                    );
                    {
                        let mut c = self.shared.controls.lock().unwrap();
                        ui.checkbox(&mut c.headless, "Headless mode");
                    }
                    ui.separator();
                    ui.strong("Camera");
                    ui.horizontal(|ui| {
                        if ui.add_enabled(ready, egui::Button::new("Main")).clicked() {
                            self.send(vec![0x0b, 0]);
                        }
                        if ui.add_enabled(ready, egui::Button::new("Second")).clicked() {
                            self.send(vec![0x0b, 1]);
                        }
                        if ui
                            .add_enabled(ready, egui::Button::new("Device info"))
                            .clicked()
                        {
                            for p in [
                                vec![0x0f],
                                vec![0x28],
                                vec![0xff, 0x53, 0x54, 0x33, 0x16],
                                vec![0x2c],
                            ] {
                                self.send(p);
                            }
                        }
                    });
                    ui.collapsing("Camera orientation commands", |ui| {
                        ui.horizontal(|ui| {
                            if ui
                                .add_enabled(ready, egui::Button::new("Normal (01)"))
                                .clicked()
                            {
                                self.send(vec![1]);
                            }
                            if ui
                                .add_enabled(ready, egui::Button::new("Reverse (02)"))
                                .clicked()
                            {
                                self.send(vec![2]);
                            }
                        });
                    });
                    ui.checkbox(&mut self.settings.rotate, "Rotate preview 180°");
                    ui.horizontal(|ui| {
                        ui.label("Filter");
                        egui::ComboBox::from_id_salt("filter")
                            .selected_text(&self.settings.filter)
                            .show_ui(ui, |ui| {
                                for f in ["Original", "Grayscale", "Contrast", "Warm"] {
                                    ui.selectable_value(&mut self.settings.filter, f.into(), f);
                                }
                            });
                    });
                    ui.add(
                        egui::Slider::new(&mut self.settings.zoom, 1. ..=3.).text("Digital zoom"),
                    );
                    ui.separator();
                    ui.strong("Continuous frame capture");
                    let mut cap = self.shared.capture.lock().unwrap().clone();
                    let mut enabled = cap.enabled;
                    if ui
                        .checkbox(&mut enabled, "Save incoming JPEG frames")
                        .changed()
                    {
                        cap.enabled = enabled;
                        if enabled {
                            cap.generation += 1;
                        }
                        *self.shared.capture.lock().unwrap() = cap.clone();
                    }
                    ui.add_enabled_ui(!cap.enabled, |ui| {
                        ui.checkbox(&mut self.settings.sample, "Capture every N frames");
                        ui.horizontal(|ui| {
                            ui.label("N");
                            ui.add_enabled(
                                self.settings.sample,
                                egui::DragValue::new(&mut self.settings.every).range(1..=100000),
                            );
                        });
                    });
                    {
                        let mut c = self.shared.capture.lock().unwrap();
                        c.sample = self.settings.sample;
                        c.every = self.settings.every.max(1);
                    }
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(self.frame.is_some(), egui::Button::new("Camera JPEG…"))
                            .clicked()
                        {
                            self.save_frame(true);
                        }
                        if ui
                            .add_enabled(self.frame.is_some(), egui::Button::new("Preview PNG…"))
                            .clicked()
                        {
                            self.save_frame(false);
                        }
                    });
                    if ui
                        .add_enabled(ready, egui::Button::new("Download original photo…"))
                        .clicked()
                        && let Some(path) = rfd::FileDialog::new()
                            .add_filter("JPEG", &["jpg"])
                            .set_file_name("original.jpg")
                            .save_file()
                    {
                        match self.config() {
                            Ok(c) => session::photo(c, path, self.shared.clone()),
                            Err(e) => self.error(e),
                        }
                    }
                    ui.small(format!("Capture root: {}", self.settings.root.display()));
                });
            });
    }
    fn center(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Live camera");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let active = self.shared.controls.lock().unwrap().enabled;
                    ui.label(
                        RichText::new(if active {
                            "FLIGHT LINK ON"
                        } else {
                            "FLIGHT LINK OFF"
                        })
                        .color(if active {
                            ui.visuals().warn_fg_color
                        } else {
                            ui.visuals().text_color()
                        }),
                    );
                });
            });
            if let Some(t) = &self.texture {
                let size = t.size_vec2();
                let available = ui.available_size();
                let scale = (available.x / size.x).min(available.y / size.y);
                ui.centered_and_justified(|ui| {
                    ui.add(egui::Image::new(t).fit_to_exact_size(size * scale));
                });
            } else {
                ui.centered_and_justified(|ui| {
                    ui.vertical_centered(|ui| {
                        ui.heading("No drone connected");
                        if ui.button("Open saved recordings…").clicked() {
                            self.open_replay(None);
                        }
                    });
                });
            }
        });
    }
    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Network & storage").open(&mut open).default_width(540.).show(ctx,|ui|{
            let stopped=self.worker.is_none();ui.add_enabled_ui(stopped,|ui|{
                ui.horizontal(|ui| {
                    ui.strong("Network device");
                    let help = "Choose the interface already connected to the drone Wi-Fi. All sockets bind to its IPv4 address.";
                    ui.small_button("?").on_hover_text(help).on_disabled_hover_text(help);
                });
                let selected=format!("{} · {}",self.settings.interface,self.settings.local);
                egui::ComboBox::from_id_salt("interface").selected_text(selected).width(410.).show_ui(ui,|ui|{
                    for i in self.interfaces.clone(){let selected=self.settings.interface==i.name&&self.settings.local==i.ip.to_string();
                        if ui.selectable_label(selected,format!("{} · {}",i.name,i.ip)).clicked(){self.settings.interface=i.name;self.settings.local=i.ip.to_string();}}
                });
                if ui.button("Refresh interfaces").clicked(){self.refresh_interfaces();}
                ui.horizontal(|ui|{ui.label("Local IPv4");ui.text_edit_singleline(&mut self.settings.local);});
                ui.horizontal(|ui|{ui.label("Drone IPv4");ui.text_edit_singleline(&mut self.settings.drone);});
                ui.separator();
                ui.horizontal(|ui| {
                    ui.strong("Storage");
                    let help = "These session recordings are independent of sampled JPEG capture.";
                    ui.small_button("?").on_hover_text(help).on_disabled_hover_text(help);
                });
                ui.label(self.settings.root.to_string_lossy());
                if ui.button("Choose capture folder…").clicked(){self.pick_folder();}
                ui.checkbox(&mut self.settings.packet_log,"Save packet log (JSONL)");ui.checkbox(&mut self.settings.stream,"Save full MJPEG stream");
            });
            if !stopped{ui.label("Disconnect or stop replay to change the network or capture root.");}
        });
        self.settings_open = open;
    }
    fn controller_dialog(&mut self, ctx: &egui::Context) {
        if !self.controller_open {
            return;
        }
        let mut open = true;
        egui::Window::new("Controller setup").open(&mut open).default_width(820.).show(ctx,|ui|{
            ui.horizontal(|ui|{
                egui::ComboBox::from_id_salt("device").width(420.).selected_text(self.selected_device.and_then(|i|self.devices.get(i)).map(|d|d.name.as_str()).unwrap_or("Choose a controller")).show_ui(ui,|ui|{
                    for (i,d) in self.devices.iter().enumerate(){ui.selectable_value(&mut self.selected_device,Some(i),format!("{} [{}]",d.name,if d.gamepad{"gamepad"}else{"USB joystick"}));}
                });
                if ui.button("Refresh").clicked(){self.refresh_devices();}
            });
            if let Some(error) = &self.device_error {ui.colored_label(ui.visuals().error_fg_color,error);}
            else if self.devices.is_empty() {ui.label("No controllers detected");}
            ui.checkbox(&mut self.force_raw,"Use raw USB joystick (RadioMaster / custom mapping)");
            if ui.add_enabled(self.selected_device.is_some(),egui::Button::new("Use selected device")).clicked() && let Some(d)=self.selected_device.and_then(|i|self.devices.get(i)).cloned()
                && let Some(c)=&mut self.controllers {match c.select(&d,self.force_raw){
                    Ok(())=>{self.input_source=if c.raw_mode {2} else {1};self.mapping=self.settings.radios.get(&d.guid).cloned().unwrap_or_default();
                        self.previous_buttons=None;self.axis=[0.;4];self.log(format!("Selected controller {}",d.name));},Err(e)=>self.error(e)
                }}
            if self.input_source==1 {ui.label("Xbox: right stick roll/pitch, left stick throttle/yaw; Y takeoff/land, B emergency stop.");}
            if self.input_source==2 {
                ui.separator();ui.label("RadioMaster: select USB Joystick mode on the radio. USB axes depend on its model/mixer settings.");
                ui.label("Set neutral with throttle at its midpoint. This drone uses centered throttle for no vertical input.");
                ui.label("Axis and button numbers below start at 0. Changes are saved per USB device GUID.");
                egui::Grid::new("calibration").striped(true).show(ui,|ui|{
                    for h in ["Channel","Axis","Invert","Min","Neutral","Max","Read current","Learn"]{ui.strong(h);}ui.end_row();
                    for (index,label) in ["Roll","Pitch","Throttle","Yaw"].iter().enumerate(){
                        let binding=&mut self.mapping.channels[index];ui.label(*label);ui.add(egui::DragValue::new(&mut binding.axis).range(0..=31));ui.checkbox(&mut binding.invert,"");
                        ui.add(egui::DragValue::new(&mut binding.min).range(-32768..=32767));ui.add(egui::DragValue::new(&mut binding.center).range(-32768..=32767));ui.add(egui::DragValue::new(&mut binding.max).range(-32768..=32767));
                        let raw=self.reading.raw.get(binding.axis as usize).copied();ui.horizontal(|ui|{
                            if ui.add_enabled(raw.is_some(),egui::Button::new("Min")).clicked(){binding.min=raw.unwrap() as i32;}
                            if ui.add_enabled(raw.is_some(),egui::Button::new("Neutral")).clicked(){binding.center=raw.unwrap() as i32;}
                            if ui.add_enabled(raw.is_some(),egui::Button::new("Max")).clicked(){binding.max=raw.unwrap() as i32;}
                        });
                        if ui.button("Learn").clicked(){self.learn=Some((index,self.reading.raw.clone()));}ui.end_row();
                    }
                });
                if self.learn.is_some(){ui.colored_label(ui.visuals().warn_fg_color,"Move the desired stick to assign its axis…");if ui.button("Cancel learn").clicked(){self.learn=None;}}
                ui.horizontal(|ui|{ui.label("Takeoff/land button");button_choice(ui,"takeoff_btn",&mut self.mapping.takeoff_button,self.reading.buttons.len());
                    ui.label("Emergency-stop button");button_choice(ui,"stop_btn",&mut self.mapping.stop_button,self.reading.buttons.len());});
            }
            ui.separator();ui.strong("Live input");for (i,raw) in self.reading.raw.iter().enumerate(){ui.horizontal(|ui|{ui.label(format!("Axis {i}: {raw:6}"));ui.add(egui::ProgressBar::new((*raw as f32+32768.)/65535.).desired_width(350.));});}
            let pressed:Vec<_>=self.reading.buttons.iter().enumerate().filter(|(_,v)|**v).map(|(i,_)|i).collect();ui.label(format!("Pressed buttons: {pressed:?}"));
            if let Some(issue)=&self.input_issue {ui.colored_label(ui.visuals().error_fg_color,issue);}
            ui.add(egui::Slider::new(&mut self.settings.deadzone,0. ..=0.4).text("Deadzone"));
            if ui.button("Save mapping & close").clicked(){if let Some(c)=&self.controllers&&c.raw_mode&&c.active(){self.settings.radios.insert(c.guid.clone(),self.mapping.clone());}self.controller_open=false;}
        });
        self.controller_open = self.controller_open && open;
    }
    fn dialogs(&mut self, ctx: &egui::Context) {
        self.settings_dialog(ctx);
        self.controller_dialog(ctx);
        if self.metadata_open {
            egui::Window::new("Image metadata")
                .open(&mut self.metadata_open)
                .default_width(550.)
                .show(ctx, |ui| {
                    egui::ScrollArea::vertical()
                        .max_height(500.)
                        .show(ui, |ui| {
                            let text =
                                serde_json::to_string_pretty(&self.metadata).unwrap_or_default();
                            ui.label(RichText::new(text).monospace());
                        });
                });
        }
        if let Some(error) = self.errors.front().cloned() {
            egui::Window::new("Hiturbo message")
                .collapsible(false)
                .resizable(false)
                .show(ctx, |ui| {
                    ui.label(error);
                    if ui.button("OK").clicked() {
                        self.errors.pop_front();
                    }
                });
        }
    }
}
fn button_choice(ui: &mut egui::Ui, id: &str, value: &mut Option<u32>, count: usize) {
    egui::ComboBox::from_id_salt(id)
        .selected_text(value.map(|v| v.to_string()).unwrap_or("Unassigned".into()))
        .show_ui(ui, |ui| {
            ui.selectable_value(value, None, "Unassigned");
            for i in 0..count as u32 {
                ui.selectable_value(value, Some(i), i.to_string());
            }
        });
}
impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if Instant::now() >= self.next_device_refresh {
            self.refresh_devices();
        }
        if let Some(p) = self.pending_replay.take() {
            self.open_replay(Some(p));
        }
        self.events();
        let title = if let Some(frame) = &self.frame {
            format!(
                "hiturbo-rs: JPEG {}×{} · {:.1} fps · {} decoded frames",
                frame.width,
                frame.height,
                self.snapshot.decoded as f64 / self.snapshot.elapsed.max(1.),
                self.snapshot.decoded
            )
        } else {
            "hiturbo-rs: JPEG decoder".into()
        };
        if title != self.window_title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.window_title = title;
        }
        self.input(ctx);
        self.update_texture(ctx);
        self.top(ctx);
        self.log_panel(ctx);
        self.left(ctx);
        self.right(ctx);
        self.center(ctx);
        self.dialogs(ctx);
        if self.smoke
            && !self.smoke_done
            && self.frame.is_some()
            && self.smoke_started.elapsed() > Duration::from_secs(2)
        {
            self.smoke_done = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::new(
                "smoke",
            )));
        }
        for event in ctx.input(|i| i.events.clone()) {
            if let egui::Event::Screenshot { image, .. } = event {
                let bytes: Vec<u8> = image.pixels.iter().flat_map(|p| p.to_array()).collect();
                let path = std::env::var("HITURBO_SCREENSHOT")
                    .unwrap_or_else(|_| "hiturbo-rs-preview.png".into());
                if let Err(e) = image::save_buffer(
                    &path,
                    &bytes,
                    image.size[0] as u32,
                    image.size[1] as u32,
                    image::ColorType::Rgba8,
                ) {
                    eprintln!("Screenshot: {e}");
                }
                println!(
                    "GUI smoke passed: {:?}, {} decoded frames",
                    self.frame.as_ref().map(|f| (f.width, f.height)),
                    self.snapshot.decoded
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if self.smoke && self.smoke_started.elapsed() > Duration::from_secs(12) && !self.smoke_done
        {
            eprintln!("GUI smoke failed: {}", self.state);
            std::process::exit(1);
        }
        ctx.request_repaint_after(Duration::from_millis(20));
    }
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        if !self.smoke {
            eframe::set_value(storage, "settings", &self.settings);
        }
    }
    fn on_exit(&mut self, _: Option<&eframe::glow::Context>) {
        self.disconnect();
    }
}
