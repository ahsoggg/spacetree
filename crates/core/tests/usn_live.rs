//! Needs administrator rights and a real NTFS volume: `SPACETREE_USN_DRIVE=C cargo test -p spacetree-core
//! --test usn_live -- --ignored --nocapture`. Creates and grows a file in the temp folder (which must be on that
//! volume) and expects the journal monitor to report both.
#![cfg(windows)]

use std::io::Write;
use std::time::{Duration, Instant};

use spacetree_core::usn::{self, Change};

#[test]
#[ignore = "needs administrator rights"]
fn monitor_reports_a_created_and_grown_file() {
    let drive = std::env::var("SPACETREE_USN_DRIVE")
        .ok()
        .and_then(|s| s.chars().next())
        .unwrap_or('C');
    let monitor = usn::start(drive, None, || {}).expect("start monitor (administrator? journal enabled?)");
    let name = format!("spacetree-usn-test-{}.bin", std::process::id());
    let path = std::env::temp_dir().join(&name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&vec![7u8; 3 * 1024 * 1024]).unwrap();
    f.sync_all().unwrap();
    drop(f);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = None;
    while Instant::now() < deadline && seen.is_none() {
        while let Ok(c) = monitor.rx.try_recv() {
            println!("{c:?}");
            match c {
                Change::Created { name: n, size, .. } | Change::Resized { name: n, size, .. } if n == name => {
                    seen = Some(size);
                }
                Change::Lost(why) => panic!("monitor lost: {why}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(&path);
    assert_eq!(
        seen,
        Some(3 * 1024 * 1024),
        "the monitor must report the file with its size"
    );
}
