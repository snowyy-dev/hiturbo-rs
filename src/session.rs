use crate::{
    capture::{CaptureSettings, CaptureStats, FrameCapture, new_file},
    control::Controls,
    protocol::{self, Descriptor, Frame, Receiver, Stats},
};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Sender, SyncSender},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub fn unique_id() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}
pub fn epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub fn session_folder(root: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(root).map_err(|e| e.to_string())?;
    let folder = root.join(format!("session-{}", unique_id()));
    fs::create_dir(&folder).map_err(|e| e.to_string())?;
    Ok(folder)
}
#[derive(Clone)]
pub struct Preview {
    pub jpeg: Arc<Vec<u8>>,
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub number: u64,
}
pub fn decode_jpeg(jpeg: Vec<u8>, number: u64) -> Result<Preview, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(&jpeg))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let (w, h) = reader.into_dimensions().map_err(|e| e.to_string())?;
    if w == 0 || h == 0 || w > 4096 || h > 4096 {
        return Err(format!("JPEG dimensions exceed limit: {w}x{h}"));
    }
    let image = image::load_from_memory_with_format(&jpeg, image::ImageFormat::Jpeg)
        .map_err(|e| e.to_string())?
        .to_rgba8();
    Ok(Preview {
        width: w,
        height: h,
        rgba: image.into_raw(),
        jpeg: Arc::new(jpeg),
        number,
    })
}
#[derive(Clone, Default)]
pub struct Snapshot {
    pub stats: Stats,
    pub capture: CaptureStats,
    pub decoded: u64,
    pub decode_errors: u64,
    pub elapsed: f64,
    pub folder: Option<PathBuf>,
    pub info: BTreeMap<String, String>,
    pub descriptor: Option<Descriptor>,
    pub reported_battery: Option<u8>,
}
#[derive(Clone)]
pub enum Event {
    Log(String),
    Error(String),
    State(String),
    Ended,
    Photo(PathBuf),
}
#[derive(Clone)]
pub struct Shared {
    pub latest: Arc<Mutex<Option<Preview>>>,
    pub snapshot: Arc<Mutex<Snapshot>>,
    pub controls: Arc<Mutex<Controls>>,
    pub capture: Arc<Mutex<CaptureSettings>>,
    pub ready: Arc<AtomicBool>,
    pub events: SyncSender<Event>,
}
impl Shared {
    pub fn log(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(Event::Log(msg.into()));
    }
    pub fn state(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(Event::State(msg.into()));
    }
    pub fn error(&self, msg: impl Into<String>) {
        let _ = self.events.try_send(Event::Error(msg.into()));
    }
}
#[derive(Clone)]
pub struct NetworkConfig {
    pub drone: Ipv4Addr,
    pub local: Ipv4Addr,
    pub root: PathBuf,
    pub packet_log: bool,
    pub save_stream: bool,
}
pub struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub commands: Sender<Vec<u8>>,
    shared: Shared,
}
impl Worker {
    pub fn live(config: NetworkConfig, shared: Shared) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let s = stop.clone();
        let sh = shared.clone();
        let thread = thread::spawn(move || {
            if let Err(e) = network(config, &sh, &s, rx)
                && !s.load(Ordering::Relaxed)
            {
                sh.error(e);
            }
            sh.ready.store(false, Ordering::Relaxed);
            sh.controls.lock().unwrap().disable();
            let _ = sh.events.try_send(Event::Ended);
        });
        Self {
            stop,
            thread: Some(thread),
            commands: tx,
            shared,
        }
    }
    pub fn replay(path: PathBuf, root: PathBuf, shared: Shared) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, _) = mpsc::channel();
        let s = stop.clone();
        let sh = shared.clone();
        let thread = thread::spawn(move || {
            sh.ready.store(false, Ordering::Relaxed);
            sh.controls.lock().unwrap().disable();
            if let Err(e) = replay(path, root, &sh, &s)
                && !s.load(Ordering::Relaxed)
            {
                sh.error(e);
            }
            let _ = sh.events.try_send(Event::Ended);
        });
        Self {
            stop,
            thread: Some(thread),
            commands: tx,
            shared,
        }
    }
    pub fn stop(&mut self) {
        self.shared.ready.store(false, Ordering::Relaxed);
        self.shared.controls.lock().unwrap().disable();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Processor {
    live: bool,
    receiver: Receiver,
    shared: Shared,
    capture: FrameCapture,
    stream: Option<BufWriter<File>>,
    start: Instant,
    decoded: u64,
    errors: u64,
}
impl Processor {
    fn new(folder: PathBuf, shared: &Shared, stream: bool, live: bool) -> Result<Self, String> {
        *shared.snapshot.lock().unwrap() = Snapshot {
            folder: Some(folder.clone()),
            ..Default::default()
        };
        *shared.latest.lock().unwrap() = None;
        let stream = if stream {
            Some(BufWriter::new(new_file(&folder.join("preview.mjpeg"))?))
        } else {
            None
        };
        shared.log(format!("Session files: {}", folder.display()));
        Ok(Self {
            live,
            receiver: Receiver::default(),
            shared: shared.clone(),
            capture: FrameCapture::new(folder),
            stream,
            start: Instant::now(),
            decoded: 0,
            errors: 0,
        })
    }
    fn packet(&mut self, data: &[u8], time: f64) -> Result<(), String> {
        for frame in self.receiver.feed(data, time) {
            self.frame(frame, time)?;
        }
        Ok(())
    }
    fn frame(&mut self, frame: Frame, time: f64) -> Result<(), String> {
        if let Some(s) = &mut self.stream {
            s.write_all(&frame.jpeg).map_err(|e| e.to_string())?;
        }
        let capture = self.shared.capture.lock().unwrap().clone();
        if let Err(e) = self.capture.accept(&frame, time, &capture) {
            self.shared.capture.lock().unwrap().enabled = false;
            self.shared.error(format!("Frame capture stopped: {e}"));
        }
        match decode_jpeg(frame.jpeg, self.decoded + 1) {
            Ok(preview) => {
                self.decoded += 1;
                if self.decoded == 1 {
                    self.shared.state(if self.live {
                        "Streaming JPEG"
                    } else {
                        "Offline replay · JPEG"
                    });
                    self.shared.log(format!(
                        "JPEG {} × {}; private envelope advertises {} × {}",
                        preview.width,
                        preview.height,
                        frame.advertised_width,
                        frame.advertised_height
                    ));
                }
                *self.shared.latest.lock().unwrap() = Some(preview);
            }
            Err(e) => {
                self.errors += 1;
                self.shared.log(format!("JPEG decode: {e}"));
            }
        }
        self.snapshot();
        Ok(())
    }
    fn snapshot(&self) {
        let mut s = self.shared.snapshot.lock().unwrap();
        s.stats = self.receiver.stats.clone();
        s.capture = self.capture.stats.clone();
        s.decoded = self.decoded;
        s.decode_errors = self.errors;
        s.elapsed = self.start.elapsed().as_secs_f64();
    }
    fn flush(&mut self) -> Result<(), String> {
        self.snapshot();
        self.capture.flush()?;
        if let Some(s) = &mut self.stream {
            s.flush().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}
fn bind_tcp(local: Ipv4Addr, drone: Ipv4Addr, timeout: Duration) -> Result<TcpStream, String> {
    let socket = socket2::Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .map_err(|e| e.to_string())?;
    socket
        .bind(&SocketAddrV4::new(local, 0).into())
        .map_err(|e| e.to_string())?;
    socket
        .connect_timeout(&SocketAddrV4::new(drone, 8888).into(), timeout)
        .map_err(|e| e.to_string())?;
    let stream: TcpStream = socket.into();
    stream.set_nodelay(true).map_err(|e| e.to_string())?;
    Ok(stream)
}
fn send_udp(
    socket: &UdpSocket,
    drone: Ipv4Addr,
    data: &[u8],
    log: &mut Option<BufWriter<File>>,
) -> Result<(), String> {
    socket
        .send_to(data, (drone, 8080))
        .map_err(|e| e.to_string())?;
    if let Some(log) = log {
        writeln!(
            log,
            "{}",
            serde_json::json!({"direction":"out","t":epoch(),"payload_hex":protocol::hex(data)})
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}
struct FlightSender {
    stopped: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl FlightSender {
    fn start(
        command: &UdpSocket,
        drone: Ipv4Addr,
        folder: &Path,
        logging: bool,
        shared: &Shared,
    ) -> Result<Self, String> {
        let command = command.try_clone().map_err(|e| e.to_string())?;
        let mut log = if logging {
            Some(BufWriter::new(new_file(
                &folder.join("flight-packets.jsonl"),
            )?))
        } else {
            None
        };
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let shared = shared.clone();
        let thread = thread::spawn(move || {
            let mut next = Instant::now();
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next {
                    thread::sleep((next - now).min(Duration::from_millis(3)));
                    continue;
                }
                next += Duration::from_millis(50);
                if next < now {
                    next = now + Duration::from_millis(50);
                }
                if shared.ready.load(Ordering::Relaxed) {
                    let packet = shared.controls.lock().unwrap().current_packet(now);
                    if let Some(p) = packet
                        && let Err(e) = send_udp(&command, drone, &p, &mut log)
                    {
                        shared.controls.lock().unwrap().disable();
                        shared.error(format!("Flight sender stopped: {e}"));
                        break;
                    }
                }
            }
            if let Some(log) = &mut log
                && let Err(e) = log.flush()
            {
                shared.error(format!("Flight log flush: {e}"));
            }
        });
        Ok(Self {
            stopped,
            thread: Some(thread),
        })
    }
}
impl Drop for FlightSender {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
fn network(
    config: NetworkConfig,
    shared: &Shared,
    stop: &AtomicBool,
    commands: mpsc::Receiver<Vec<u8>>,
) -> Result<(), String> {
    let folder = session_folder(&config.root)?;
    let mut processor = Processor::new(folder.clone(), shared, config.save_stream, true)?;
    let mut log = if config.packet_log {
        Some(BufWriter::new(new_file(&folder.join("packets.jsonl"))?))
    } else {
        None
    };
    let command = UdpSocket::bind((config.local, 0)).map_err(|e| e.to_string())?;
    let video = UdpSocket::bind((config.local, 8888)).map_err(|e| format!("Video socket: {e}"))?;
    let status =
        UdpSocket::bind((config.local, 8081)).map_err(|e| format!("Status socket: {e}"))?;
    socket2::SockRef::from(&video)
        .set_recv_buffer_size(2 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
    for s in [&command, &video, &status] {
        s.set_nonblocking(true).map_err(|e| e.to_string())?;
    }
    // Keep the motor sender independent of JPEG decoding and capture disk I/O.
    let _flight = FlightSender::start(&command, config.drone, &folder, config.packet_log, shared)?;
    let start = Instant::now();
    let mut query = start;
    let mut udp = start;
    let mut update = start;
    let mut tcp: Option<TcpStream> = None;
    let mut tcp_next = start;
    let mut tcp_count = 0;
    let mut found = false;
    let mut no_video_notice = false;
    let mut buffer = vec![0u8; 65535];
    shared.state("Looking for the drone");
    shared.log(format!("Binding all sockets to {}", config.local));
    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        if !found && now >= query {
            send_udp(&command, config.drone, &[0x0f], &mut log)?;
            query = now + Duration::from_millis(500);
        }
        if !found && start.elapsed() > Duration::from_secs(6) {
            return Err(
                "No descriptor reply. Check the selected Wi-Fi interface and drone IP.".into(),
            );
        }
        if found && now >= udp {
            send_udp(
                &command,
                config.drone,
                &protocol::ip_command(9, config.local),
                &mut log,
            )?;
            send_udp(
                &command,
                config.drone,
                &protocol::ip_command(8, config.local),
                &mut log,
            )?;
            udp = now + Duration::from_secs(1);
        }
        for cmd in commands.try_iter().take(64) {
            if found {
                send_udp(&command, config.drone, &cmd, &mut log)?;
                shared.log(format!("Camera command {}", protocol::hex(&cmd)));
            }
        }
        if now >= tcp_next
            && let Some(t) = tcp.as_mut()
        {
            if let Err(e) = t.write_all(&protocol::HEARTBEAT) {
                shared.log(format!("TCP heartbeat closed: {e}"));
                tcp = None;
            }
            tcp_count += 1;
            tcp_next = now + Duration::from_millis(if tcp_count < 10 { 100 } else { 1500 });
        }
        for (socket, role) in [
            (&command, "command"),
            (&video, "video"),
            (&status, "status"),
        ] {
            for _ in 0..256 {
                let (length, peer) = match socket.recv_from(&mut buffer) {
                    Ok(p) => p,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.to_string()),
                };
                if peer.ip() != config.drone {
                    continue;
                }
                let data = &buffer[..length];
                let time = epoch();
                if let Some(log) = &mut log {
                    writeln!(log,"{}",serde_json::json!({"direction":"in","t":time,"role":role,"peer":[peer.ip().to_string(),peer.port()],"payload_hex":protocol::hex(data)})).map_err(|e|e.to_string())?;
                }
                if role == "video" {
                    processor.packet(data, time)?;
                    continue;
                }
                if data.starts_with(&[0xaa, 0xff]) {
                    let d = protocol::descriptor(data)?;
                    if (d.chip_maker, d.chip_type, d.transport.as_str()) != (3, 1, "UDP") {
                        return Err(
                            "Unsupported drone profile (expected maker 3/type 1/UDP)".into()
                        );
                    }
                    shared.snapshot.lock().unwrap().descriptor = Some(d);
                    if !found {
                        found = true;
                        shared.ready.store(true, Ordering::Relaxed);
                        shared.state("Connected · waiting for video");
                        for cmd in [
                            vec![0x28],
                            vec![0xff, 0x53, 0x54, 0x33, 0x16],
                            vec![0x2c],
                            vec![0x27],
                        ] {
                            send_udp(&command, config.drone, &cmd, &mut log)?;
                        }
                        match bind_tcp(config.local, config.drone, Duration::from_millis(750)) {
                            Ok(t) => {
                                t.set_nonblocking(true).map_err(|e| e.to_string())?;
                                tcp = Some(t);
                                tcp_next = Instant::now();
                            }
                            Err(e) => shared.log(format!(
                                "TCP status unavailable; UDP requests continue: {e}"
                            )),
                        }
                    }
                } else {
                    let mut s = shared.snapshot.lock().unwrap();
                    if let Some(battery) = protocol::reported_battery(data) {
                        s.reported_battery = Some(battery);
                    }
                    if data.starts_with(b"V") {
                        s.info.insert(
                            "Firmware".into(),
                            String::from_utf8_lossy(data).trim().into(),
                        );
                    } else if data.starts_with(&[0xff, 0x53, 0x54, 0x33, 0x16]) && data.len() == 6 {
                        s.info.insert("Camera index".into(), data[5].to_string());
                    } else if data.first() == Some(&0x0b) && data.len() == 2 {
                        s.info.insert("Camera index".into(), data[1].to_string());
                    } else if data.starts_with(b"mirror=") {
                        s.info.insert(
                            "Orientation reply".into(),
                            String::from_utf8_lossy(data).trim().into(),
                        );
                    } else if data.starts_with(&[0xff, 0x53, 0x54, 0x08]) {
                        s.info
                            .insert("Opaque status (not battery)".into(), protocol::hex(data));
                    } else if role == "status" {
                        s.info.insert("UDP 8081 status".into(), protocol::hex(data));
                    }
                    drop(s);
                    if role == "command" && !data.starts_with(&[0xff, 0x53, 0x54, 0x08]) {
                        shared.log(format!(
                            "Reply {} / {:?}",
                            protocol::hex(data),
                            String::from_utf8_lossy(data)
                        ));
                    }
                }
            }
        }
        if let Some(t) = tcp.as_mut() {
            match t.read(&mut buffer) {
                Ok(0) => {
                    tcp = None;
                    shared.log("TCP status closed");
                }
                Ok(n) => {
                    if let Some(log) = &mut log {
                        writeln!(log,"{}",serde_json::json!({"direction":"in","t":epoch(),"role":"tcp","payload_hex":protocol::hex(&buffer[..n])})).map_err(|e|e.to_string())?;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    shared.log(format!("TCP read: {e}"));
                    tcp = None;
                }
            }
        }
        if now >= update {
            processor.flush()?;
            if let Some(log) = &mut log {
                log.flush().map_err(|e| e.to_string())?;
            }
            update = now + Duration::from_secs(1);
        }
        if found
            && processor.decoded == 0
            && start.elapsed() > Duration::from_secs(12)
            && !no_video_notice
        {
            shared.state(if processor.receiver.stats.packets == 0 {
                "Connected · no video packets received"
            } else {
                "Video packets received · no complete JPEG decoded"
            });
            shared.log(format!("Video diagnostics: {:?}", processor.receiver.stats));
            no_video_notice = true;
        }
        thread::sleep(Duration::from_millis(2));
    }
    processor.flush()?;
    if let Some(log) = &mut log {
        log.flush().map_err(|e| e.to_string())?;
    }
    shared.state("Disconnected · files saved");
    Ok(())
}
fn wait_until(start: Instant, offset: f64, stop: &AtomicBool) -> bool {
    while start.elapsed().as_secs_f64() < offset {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        thread::sleep(Duration::from_millis(3));
    }
    !stop.load(Ordering::Relaxed)
}
pub fn log_packets(
    path: &Path,
    mut accept: impl FnMut(f64, Vec<u8>) -> Result<bool, String>,
) -> Result<(), String> {
    incoming_log_packets(path, &["video"], |time, _, data| accept(time, data))
}
fn incoming_log_packets(
    path: &Path,
    roles: &[&str],
    mut accept: impl FnMut(f64, &str, Vec<u8>) -> Result<bool, String>,
) -> Result<(), String> {
    for (line_no, line) in BufReader::new(File::open(path).map_err(|e| e.to_string())?)
        .lines()
        .enumerate()
    {
        let record: serde_json::Value = serde_json::from_str(&line.map_err(|e| e.to_string())?)
            .map_err(|e| format!("Line {}: {e}", line_no + 1))?;
        if record["direction"] == "in"
            && let Some(role) = record["role"].as_str().filter(|role| roles.contains(role))
        {
            let time = record["t"].as_f64().ok_or("Missing timestamp")?;
            let data = protocol::unhex(record["payload_hex"].as_str().ok_or("Missing payload")?)?;
            if !accept(time, role, data)? {
                break;
            }
        }
    }
    Ok(())
}
pub fn mjpeg_frames(
    path: &Path,
    mut accept: impl FnMut(Vec<u8>) -> Result<bool, String>,
) -> Result<(), String> {
    let mut file = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let mut buffer = vec![];
    let mut chunk = [0; 65536];
    loop {
        let n = file.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buffer.extend(&chunk[..n]);
        while let Some(start) = buffer.windows(2).position(|w| w == [0xff, 0xd8]) {
            let Some(end) = buffer[start + 2..]
                .windows(2)
                .position(|w| w == [0xff, 0xd9])
                .map(|end| start + 2 + end + 2)
            else {
                break;
            };
            let jpeg = buffer[start..end].to_vec();
            buffer.drain(..end);
            if !accept(jpeg)? {
                return Ok(());
            }
        }
        if buffer.len() > 2 * 1024 * 1024 {
            return Err("MJPEG frame exceeds 2 MiB limit".into());
        }
    }
    if !buffer.is_empty() {
        return Err("Incomplete JPEG at end of MJPEG file".into());
    }
    Ok(())
}
fn replay(path: PathBuf, root: PathBuf, shared: &Shared, stop: &AtomicBool) -> Result<(), String> {
    let folder = session_folder(&root)?;
    let mut processor = Processor::new(folder, shared, false, false)?;
    let start = Instant::now();
    shared.state(format!(
        "Offline replay · {}",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    if path.extension().is_some_and(|s| s == "jsonl") {
        let mut first = None;
        incoming_log_packets(
            &path,
            &["video", "status", "command"],
            |time, role, data| {
                let offset = time - *first.get_or_insert(time);
                if !wait_until(start, offset, stop) {
                    return Ok(false);
                }
                if role == "video" {
                    processor.packet(&data, time)?;
                } else if let Some(battery) = protocol::reported_battery(&data) {
                    shared.snapshot.lock().unwrap().reported_battery = Some(battery);
                }
                Ok(true)
            },
        )?;
    } else {
        let mut index = 0;
        mjpeg_frames(&path, |jpeg| {
            if !wait_until(start, index as f64 / 13., stop) {
                return Ok(false);
            }
            index += 1;
            processor.receiver.stats.frames += 1;
            processor.receiver.stats.bytes += jpeg.len() as u64;
            processor.frame(
                Frame {
                    kind: 0xa1,
                    format: 0,
                    sequence: index as u8,
                    advertised_width: 0,
                    advertised_height: 0,
                    timestamp_raw: 0,
                    jpeg,
                },
                epoch(),
            )?;
            Ok(true)
        })?;
    }
    processor.flush()?;
    shared.state(format!(
        "Replay ended · {} decoded frames",
        processor.decoded
    ));
    Ok(())
}
pub fn photo(config: NetworkConfig, path: PathBuf, shared: Shared) {
    thread::spawn(move || {
        let result = (|| -> Result<(), String> {
            let mut t = bind_tcp(config.local, config.drone, Duration::from_secs(6))?;
            t.set_read_timeout(Some(Duration::from_secs(6)))
                .map_err(|e| e.to_string())?;
            t.set_write_timeout(Some(Duration::from_secs(6)))
                .map_err(|e| e.to_string())?;
            t.write_all(&protocol::PHOTO_REQUEST)
                .map_err(|e| e.to_string())?;
            let mut h = [0u8; 6];
            t.read_exact(&mut h).map_err(|e| e.to_string())?;
            if h[..2] != [0xff, 0xff] {
                return Err("Unexpected original-photo response".into());
            }
            let len = u32::from_be_bytes(h[2..6].try_into().unwrap()) as usize;
            if len == 0 || len > 32 * 1024 * 1024 {
                return Err("Invalid photo length".into());
            }
            let mut jpeg = vec![0; len];
            t.read_exact(&mut jpeg).map_err(|e| e.to_string())?;
            if !jpeg.starts_with(&[0xff, 0xd8]) {
                return Err("Photo response is not JPEG".into());
            }
            new_file(&path)?
                .write_all(&jpeg)
                .map_err(|e| e.to_string())?;
            Ok(())
        })();
        match result {
            Ok(()) => {
                let _ = shared.events.try_send(Event::Photo(path));
            }
            Err(e) => shared.error(format!("Original photo unavailable: {e}")),
        }
    });
}
#[derive(Serialize)]
struct Verification {
    stats: Stats,
    decoded: u64,
    dimensions: HashMap<String, u64>,
    exif_frames: u64,
    capture: CaptureStats,
}
pub fn verify_log(args: &[String]) -> Result<(), String> {
    let path = args
        .first()
        .ok_or("Usage: hiturbo-rs --verify-log PATH [--capture-dir DIR] [--every N]")?;
    let capture = args
        .windows(2)
        .find(|a| a[0] == "--capture-dir")
        .map(|a| PathBuf::from(&a[1]));
    let every = args
        .windows(2)
        .find(|a| a[0] == "--every")
        .map(|a| a[1].parse::<u32>())
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(1);
    if every == 0 {
        return Err("--every must be at least 1".into());
    }
    let root = capture.as_ref().map(|p| session_folder(p)).transpose()?;
    let mut capture = root.map(FrameCapture::new);
    let settings = CaptureSettings {
        enabled: true,
        sample: every > 1,
        every,
        generation: 1,
    };
    let mut r = Receiver::default();
    let mut decoded = 0;
    let mut dimensions = HashMap::new();
    let mut exif_frames = 0;
    log_packets(Path::new(path), |time, data| {
        for frame in r.feed(&data, time) {
            let preview = decode_jpeg(frame.jpeg.clone(), decoded + 1)?;
            decoded += 1;
            *dimensions
                .entry(format!("{}x{}", preview.width, preview.height))
                .or_insert(0) += 1;
            if exif::Reader::new()
                .read_from_container(&mut std::io::Cursor::new(&frame.jpeg))
                .is_ok()
            {
                exif_frames += 1;
            }
            if let Some(c) = &mut capture {
                c.accept(&frame, time, &settings)?;
            }
        }
        Ok(true)
    })?;
    if let Some(c) = &mut capture {
        c.flush()?;
    }
    let result = Verification {
        stats: r.stats,
        decoded,
        dimensions,
        exif_frames,
        capture: capture.map(|c| c.stats).unwrap_or_default(),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
    );
    if decoded == 0 {
        return Err("No JPEG frames decoded".into());
    }
    Ok(())
}
