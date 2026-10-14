//! End-to-end network test on loopback only; never contacts a physical drone.
use hiturbo_rs::{
    capture::CaptureSettings,
    control::Controls,
    protocol,
    session::{self, Event, NetworkConfig, Shared, Snapshot, Worker},
};
use std::{
    fs,
    io::{Read, Write},
    net::{Ipv4Addr, TcpListener, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

fn fragments(jpeg: &[u8], id: u16) -> Vec<Vec<u8>> {
    let mut block = vec![0u8; 44];
    block[..4].copy_from_slice(&[0, 0, 1, 0xa1]);
    block[10] = id as u8;
    block[16..20].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
    block[24..26].copy_from_slice(&360u16.to_le_bytes());
    block[26..28].copy_from_slice(&640u16.to_le_bytes());
    block.extend(jpeg);
    let count = block.len().div_ceil(1390) as u8;
    block
        .chunks(1390)
        .enumerate()
        .map(|(i, data)| {
            let mut p = vec![0x40, 0x40];
            p.extend(id.to_le_bytes());
            p.extend([count, i as u8]);
            p.extend(((data.len() + 10) as u16).to_le_bytes());
            p.extend(data);
            p.extend([0xab, 0xcd]);
            p
        })
        .collect()
}
fn wait(mut predicate: impl FnMut() -> bool) {
    let start = Instant::now();
    while !predicate() {
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "Mock test timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
#[test]
fn discovery_jpeg_camera_photo_capture_and_motor_pulses() {
    let udp = UdpSocket::bind(("127.0.0.1", 8080)).unwrap();
    udp.set_nonblocking(true).unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 8888)).unwrap();
    listener.set_nonblocking(true).unwrap();
    let rgb = image::RgbImage::from_fn(320, 240, |x, y| {
        image::Rgb([(x % 255) as u8, (y % 255) as u8, ((x * y) % 255) as u8])
    });
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(rgb)
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .unwrap();
    let jpeg = encoded.into_inner();
    let photo = jpeg.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let commands = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
    let seen = commands.clone();
    let server = thread::spawn(move || {
        let mut target = None;
        let mut id = 0;
        let mut next_frame = Instant::now();
        let mut buf = [0u8; 65535];
        while !s.load(Ordering::Relaxed) {
            while let Ok((n, peer)) = udp.recv_from(&mut buf) {
                let p = &buf[..n];
                seen.lock().unwrap().push(p.to_vec());
                match p {
                    [0x0f] => {
                        udp.send_to(
                            &protocol::unhex("aaff0301011717010d01010036").unwrap(),
                            peer,
                        )
                        .unwrap();
                    }
                    [0x28] => {
                        udp.send_to(b"V4.0.3", peer).unwrap();
                    }
                    [0x08, a, b, c, d] => {
                        target = Some((Ipv4Addr::new(*a, *b, *c, *d), 8888));
                    }
                    [0x0b, _] => {
                        udp.send_to(p, peer).unwrap();
                    }
                    _ => {}
                }
            }
            if Instant::now() >= next_frame
                && let Some(target) = target
            {
                id += 1;
                for fragment in fragments(&jpeg, id).iter().rev() {
                    udp.send_to(fragment, target).unwrap();
                }
                udp.send_to(
                    &protocol::unhex("662a000000000000002a").unwrap(),
                    (target.0, 8081),
                )
                .unwrap();
                next_frame = Instant::now() + Duration::from_millis(77);
            }
            if let Ok((mut client, _)) = listener.accept() {
                let jpeg = jpeg.clone();
                let stopped = s.clone();
                thread::spawn(move || {
                    client
                        .set_read_timeout(Some(Duration::from_millis(1500)))
                        .unwrap();
                    let mut header = [0u8; 12];
                    if client.read_exact(&mut header).is_err() {
                        return;
                    }
                    if header[10..12] == [0x11, 0x11] {
                        let mut tail = [0u8; 2];
                        client.read_exact(&mut tail).unwrap();
                        let mut reply = vec![0xff, 0xff];
                        reply.extend((jpeg.len() as u32).to_be_bytes());
                        reply.extend(&jpeg);
                        for p in [&reply[..1], &reply[1..4], &reply[4..]] {
                            let _ = client.write_all(p);
                            thread::sleep(Duration::from_millis(5));
                        }
                    } else {
                        while !stopped.load(Ordering::Relaxed) {
                            let _ = client.write_all(b"noact\r\n");
                            if client.read_exact(&mut header).is_err() {
                                break;
                            }
                        }
                    }
                });
            }
            thread::sleep(Duration::from_millis(2));
        }
    });
    let root = std::env::temp_dir().join(format!("hiturbo-mock-{}", session::unique_id()));
    let (tx, rx) = mpsc::sync_channel(512);
    let shared = Shared {
        latest: Arc::new(Mutex::new(None)),
        snapshot: Arc::new(Mutex::new(Snapshot::default())),
        controls: Arc::new(Mutex::new(Controls::default())),
        capture: Arc::new(Mutex::new(CaptureSettings {
            enabled: true,
            sample: true,
            every: 2,
            generation: 1,
        })),
        ready: Arc::new(AtomicBool::new(false)),
        events: tx,
    };
    let config = NetworkConfig {
        drone: Ipv4Addr::LOCALHOST,
        // macOS only configures 127.0.0.1 by default; other 127/8 addresses
        // require an explicit loopback alias. The mock uses distinct ports.
        local: Ipv4Addr::LOCALHOST,
        root: root.clone(),
        packet_log: true,
        save_stream: true,
    };
    let mut worker = Worker::live(config.clone(), shared.clone());
    wait(|| shared.snapshot.lock().unwrap().decoded >= 5);
    wait(|| shared.snapshot.lock().unwrap().reported_battery == Some(42));
    assert!(
        commands
            .lock()
            .unwrap()
            .iter()
            .all(|p| p.first() != Some(&0x66))
    );
    let size = {
        let latest = shared.latest.lock().unwrap();
        let frame = latest.as_ref().unwrap();
        (frame.width, frame.height)
    };
    assert_eq!(size, (320, 240));
    worker.commands.send(vec![0x0b, 1]).unwrap();
    wait(|| {
        shared
            .snapshot
            .lock()
            .unwrap()
            .info
            .get("Camera index")
            .is_some_and(|v| v == "1")
    });
    let downloaded = root.join("original.jpg");
    session::photo(config, downloaded.clone(), shared.clone());
    wait(|| downloaded.exists());
    wait(|| fs::metadata(&downloaded).is_ok_and(|m| m.len() == photo.len() as u64));
    assert_eq!(fs::read(&downloaded).unwrap(), photo);
    shared.controls.lock().unwrap().trigger(1);
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1100) {
        shared.controls.lock().unwrap().updated = Instant::now();
        thread::sleep(Duration::from_millis(10));
    }
    shared.controls.lock().unwrap().trigger(2);
    thread::sleep(Duration::from_millis(1150)); // Motor stop completes even without UI updates.
    assert!(!shared.controls.lock().unwrap().enabled);
    worker.stop();
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    let snap = shared.snapshot.lock().unwrap().clone();
    let controls: Vec<_> = commands
        .lock()
        .unwrap()
        .iter()
        .filter(|p| p.first() == Some(&0x66))
        .cloned()
        .collect();
    for action in [1, 2] {
        let count = controls.iter().filter(|p| p[6] == action).count();
        assert!(
            (17..=22).contains(&count),
            "Action {action}: {count} packets"
        );
    }
    assert!(
        controls
            .iter()
            .filter(|p| p[6] == 2)
            .all(|p| p[2..6] == [128; 4])
    );
    assert_eq!(snap.capture.saved, snap.decoded.div_ceil(2));
    let stream = fs::read(snap.folder.unwrap().join("preview.mjpeg")).unwrap();
    assert_eq!(stream, photo.repeat(snap.decoded as usize));
    for event in rx.try_iter() {
        assert!(!matches!(event, Event::Error(_)), "Unexpected worker error");
    }
    fs::remove_dir_all(root).unwrap();
}
