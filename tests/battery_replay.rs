use hiturbo_rs::{
    capture::CaptureSettings,
    control::Controls,
    session::{self, Event, Shared, Snapshot, Worker},
};
use std::{
    fs,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

#[test]
fn replay_restores_incoming_battery_and_ignores_outgoing_or_corrupt_packets() {
    let root = std::env::temp_dir().join(format!("hiturbo-battery-{}", session::unique_id()));
    fs::create_dir(&root).unwrap();
    let log = root.join("packets.jsonl");
    fs::write(
        &log,
        concat!(
            "{\"direction\":\"out\",\"role\":\"command\",\"t\":1.0,\"payload_hex\":\"665f000000000000005f\"}\n",
            "{\"direction\":\"in\",\"role\":\"status\",\"t\":1.01,\"payload_hex\":\"662a000000000000002a\"}\n",
            "{\"direction\":\"in\",\"role\":\"status\",\"t\":1.02,\"payload_hex\":\"66140000000000000015\"}\n",
        ),
    )
    .unwrap();
    let (tx, rx) = mpsc::sync_channel(64);
    let shared = Shared {
        latest: Arc::new(Mutex::new(None)),
        snapshot: Arc::new(Mutex::new(Snapshot {
            reported_battery: Some(99),
            ..Default::default()
        })),
        controls: Arc::new(Mutex::new(Controls::default())),
        capture: Arc::new(Mutex::new(CaptureSettings::default())),
        ready: Arc::new(AtomicBool::new(false)),
        events: tx,
    };
    let mut worker = Worker::replay(log.clone(), root.clone(), shared.clone());
    loop {
        match rx.recv_timeout(Duration::from_secs(5)).unwrap() {
            Event::Ended => break,
            Event::Error(error) => panic!("Replay failed: {error}"),
            _ => {}
        }
    }
    worker.stop();
    let snapshot = shared.snapshot.lock().unwrap().clone();
    assert_eq!(snapshot.reported_battery, Some(42));
    assert_eq!(snapshot.stats.packets, 0);
    assert!(!shared.ready.load(Ordering::Relaxed));
    assert!(!shared.controls.lock().unwrap().enabled);
    session::log_packets(&log, |_, _| panic!("Status is not video")).unwrap();
    fs::remove_dir_all(root).unwrap();
}
