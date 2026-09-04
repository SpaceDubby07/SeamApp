//! M1 demo (Tier 13 of the build guide): log every mouse/key event to the
//! console for 10 seconds, then inject a synthetic left-click and watch it
//! land.
//!
//! Run on Windows with:
//!   cargo run -p seam-platform --example windows_capture_demo
//!
//! On any other OS this just prints a message and exits — there's nothing
//! to demo without the real Windows hooks.

#[cfg(windows)]
fn main() {
    use seam_core::protocol::{InputEvent, MouseButton};
    use seam_core::traits::{InputCapture, InputSink};
    use seam_platform::windows::{Capture, Sink};

    tracing_subscriber::fmt::init();

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let mut capture = Capture::new();
        capture.start(tx).expect(
            "failed to start capture — run this interactively (not as a scheduled task/service)",
        );

        println!("Capturing global mouse/keyboard input for 10 seconds.");
        println!("Move the mouse or press some keys now.");

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

#[cfg(not(windows))]
fn main() {
    println!("windows_capture_demo is Windows-only; nothing to run on this platform.");
}
