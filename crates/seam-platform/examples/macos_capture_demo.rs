//! M5 demo (Tier 13 of the build guide): log every mouse/key event to the
//! console for 10 seconds, then inject a synthetic left-click and watch it
//! land. The macOS equivalent of `windows_capture_demo`.
//!
//! Run on macOS with:
//!   cargo run -p seam-platform --example macos_capture_demo
//!
//! Requires Accessibility permission for the terminal/binary running this.
//! If capture fails to start, the error names Accessibility (and possibly
//! Input Monitoring — see permissions.rs) as the likely cause; grant it in
//! System Settings > Privacy & Security, then re-run.
//!
//! On any other OS this just prints a message and exits.

#[cfg(target_os = "macos")]
fn main() {
    use seam_core::protocol::{InputEvent, MouseButton};
    use seam_core::traits::{InputCapture, InputSink, PermissionGate, ScreenInfo};
    use seam_platform::macos::{Capture, Permissions, Screens, Sink};

    tracing_subscriber::fmt::init();

    let screens = Screens::new();
    println!("Displays: {:#?}", screens.displays());
    println!("Virtual bounds: {:?}", screens.virtual_bounds());

    let permissions = Permissions::new();
    if !permissions.has_input_permission() {
        println!("Accessibility permission not yet granted.");
        println!("Opening System Settings > Privacy & Security > Accessibility...");
        let _ = permissions.request_input_permission();
        println!("Grant permission there (to your terminal app), then re-run this demo.");
        return;
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async {
        for n in (1..=3).rev() {
            println!("Starting capture in {n}...");
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let mut capture = Capture::new();
        capture.start(tx).expect(
            "failed to start capture — check Accessibility AND Input Monitoring permission",
        );

        println!("Capturing global mouse/keyboard input for 10 seconds — move the mouse or press keys now.");

        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(10);
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Some(event) => println!("captured: {event:?}"),
                        None => break,
                    }
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }

        capture.stop().expect("failed to stop capture");

        println!("\nInjecting a synthetic left-click at (200, 200)...");
        let mut sink = Sink::new();
        sink.warp_cursor(200, 200).expect("warp_cursor failed");
        sink.inject(&InputEvent::MouseDown {
            button: MouseButton::Left,
        })
        .expect("inject mouse-down failed");
        sink.inject(&InputEvent::MouseUp {
            button: MouseButton::Left,
        })
        .expect("inject mouse-up failed");
        println!("Done — check that the cursor moved to (200, 200) and a click landed there.");
    });
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macos_capture_demo is macOS-only; nothing to run on this platform.");
}
