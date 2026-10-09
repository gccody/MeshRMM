use super::*;

/// Adds, resizes and removes a virtual display on a console without a
/// monitor, installing the driver if needed, through the same helpers the
/// service starts. Run it as LocalSystem, with `MESHRMM_TEST_HELPER` naming a
/// built `meshrmm-agent.exe`.
#[test]
#[ignore = "needs LocalSystem, a console without a monitor and a built Agent; installs a driver"]
fn headless_console_gets_a_resizable_virtual_display() {
    use meshrmm_protocol::HeadlessResolution;
    use std::sync::atomic::AtomicUsize;

    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init();
    let credentials = std::env::temp_dir().join("meshrmm-headless-test-credentials.dat");
    let mut streamer = DesktopCaptureStreamer::new(
        "Headless test".into(),
        String::new(),
        false,
        None,
        credentials,
    );
    let frames = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&frames);
    let sink: EncodedFrameSink = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
    });
    let config = StreamConfig {
        frames_per_second: 30,
        bitrate_bits_per_second: 4_000_000,
        codec: VideoCodec::H264,
        pixel_format: VideoPixelFormat::Yuv420,
        capture_cursor: true,
        grayscale: false,
    };
    let wait_for_frames = |frames: &AtomicUsize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while frames.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no frames arrived");
            thread::sleep(Duration::from_millis(50));
        }
    };

    for (resolution, restart) in [
        (HeadlessResolution::default(), false),
        (HeadlessResolution::new(1920, 1080), true),
        (HeadlessResolution::new(1920, 1080), false),
        (HeadlessResolution::new(2560, 1440), true),
    ] {
        assert_eq!(streamer.set_headless_resolution(resolution), restart);
        streamer.stop().unwrap();
        frames.store(0, Ordering::Relaxed);
        let begun = Instant::now();
        let started = streamer.start(config, None, Arc::clone(&sink)).unwrap();
        eprintln!(
            "started {}x{} on {:?} in {} ms",
            started.format.width,
            started.format.height,
            started.active_display.name,
            begun.elapsed().as_millis()
        );
        assert_eq!(
            (started.active_display.width, started.active_display.height),
            (resolution.width, resolution.height)
        );
        assert_eq!(
            (started.format.width, started.format.height),
            (resolution.width, resolution.height)
        );
        wait_for_frames(&frames);
    }
    drop(streamer);
    thread::sleep(Duration::from_secs(2));
    // With the virtual display gone, the console has no display again.
    let mut streamer = DesktopCaptureStreamer::new(
        "Headless test".into(),
        String::new(),
        false,
        None,
        std::env::temp_dir().join("meshrmm-headless-test-credentials.dat"),
    );
    let error = streamer
        .start_on_desktop(preferred_desktop(), config, None, Arc::clone(&sink))
        .err()
        .expect("capture started without a monitor");
    assert!(error.is::<NoDisplays>(), "{error:#}");
}

/// Like `headless_console_gets_a_resizable_virtual_display`, for a computer
/// whose monitors stay connected: adds the virtual display directly, then
/// captures and resizes it through the helpers.
#[test]
#[ignore = "needs LocalSystem and a built Agent; installs a driver and adds a monitor"]
fn virtual_display_is_captured_at_the_requested_size() {
    use meshrmm_protocol::HeadlessResolution;
    use std::sync::atomic::AtomicUsize;

    let _ = tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_ansi(false)
        .try_init();
    let credentials = std::env::temp_dir().join("meshrmm-headless-test-credentials.dat");
    let mut streamer = DesktopCaptureStreamer::new(
        "Headless test".into(),
        String::new(),
        false,
        None,
        credentials,
    );
    let frames = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&frames);
    let sink: EncodedFrameSink = Arc::new(move |_| {
        counted.fetch_add(1, Ordering::Relaxed);
    });
    let config = StreamConfig {
        frames_per_second: 30,
        bitrate_bits_per_second: 4_000_000,
        codec: VideoCodec::H264,
        pixel_format: VideoPixelFormat::Yuv420,
        capture_cursor: true,
        grayscale: false,
    };
    let begun = Instant::now();
    streamer.virtual_display = Some(VirtualDisplay::add(HeadlessResolution::HD).unwrap());
    eprintln!(
        "added the virtual display in {} ms",
        begun.elapsed().as_millis()
    );
    for (resolution, restart) in [
        (HeadlessResolution::HD, false),
        (HeadlessResolution::new(1920, 1080), true),
    ] {
        assert_eq!(streamer.set_headless_resolution(resolution), restart);
        streamer.stop().unwrap();
        let begun = Instant::now();
        let listed = streamer.start(config, None, Arc::clone(&sink)).unwrap();
        let names: Vec<_> = listed
            .displays
            .iter()
            .map(|d| format!("{} {}x{} id {}", d.name, d.width, d.height, d.id.0))
            .collect();
        eprintln!(
            "displays after {} ms: {names:?}",
            begun.elapsed().as_millis()
        );
        let virtual_display = listed
            .displays
            .iter()
            .find(|display| display.name.contains("MeshRMM"))
            .expect("the virtual display is not listed")
            .clone();
        assert_eq!(
            (virtual_display.width, virtual_display.height),
            (resolution.width, resolution.height)
        );
        streamer.stop().unwrap();
        frames.store(0, Ordering::Relaxed);
        let started = streamer
            .start(config, Some(virtual_display.id), Arc::clone(&sink))
            .unwrap();
        assert_eq!(started.active_display.id, virtual_display.id);
        assert_eq!(
            (started.format.width, started.format.height),
            (resolution.width, resolution.height)
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while frames.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "no frames arrived");
            thread::sleep(Duration::from_millis(50));
        }
        eprintln!("captured {}x{}", resolution.width, resolution.height);
    }
    drop(streamer);
    thread::sleep(Duration::from_secs(2));
    let displays = enumerate_console_displays().unwrap();
    assert!(
        displays
            .iter()
            .all(|display| !display.name.contains("MeshRMM")),
        "the virtual display outlived the session"
    );
}
