//! Diagnostic demo: measures captured mouse-delta magnitude on macOS,
//! unsuppressed vs suppressed, with zero `seam-core::Session`/network code
//! in the loop — the macOS twin of `windows_suppression_delta_demo`,
//! isolating whether a real physical sweep of the mouse produces sane
//! `MouseDelta`s at the capture layer itself while `RemoteActive`-style
//! suppression is on.
//!
//! Why this exists: "Windows driving Mac" is reported as perfect; "Mac
//! driving Windows" is reported as "doesn't work well." Since the platform
//! capture code was rewritten on both sides to mirror Barrier, this isolates
//! whether the macOS capture layer itself (delta computation, the
//! `CGWarpMouseCursorPosition` anchor warp, the bogus-zone filter, and
//! `CGSetLocalEventsSuppressionInterval`) is producing sane deltas while
//! suppressed, before looking anywhere else (session relay, Windows
//! injection, etc).
//!
//! Run on macOS with:
//!   cargo run -p seam-platform --example macos_suppression_delta_demo
//!
//! Requires the terminal (or whatever process ends up running this) to have
//! Accessibility + Input Monitoring permission, same as the real app.
//!
//! It runs two 6-second phases. In BOTH, move the mouse in one continuous,
//! deliberate sweep (e.g. all the way across a desk-sized mousepad, one
//! direction) — the same kind of motion you'd use to drive a peer:
//!   Phase 1 (unsuppressed): capture as normal, nothing hidden.
//!   Phase 2 (suppressed): `set_suppression(true)` — mirrors exactly what
//!   happens while `RemoteActive` in the real app. The cursor will
//!   disappear and stay pinned near the display centre; keep sweeping the
//!   physical mouse the same way regardless.
//!
//! Each phase prints a running total and, at the end, a summary: sample
//! count, sum of |dx| (total raw travel regardless of direction), net dx
//! (final accumulated position — what `Session::driving_cursor` would see),
//! and the largest single-sample delta. If phase 2's sum-of-|dx| is far
//! below phase 1's for the same physical effort, suppression itself is
//! eating motion — the same signature the Windows version originally found.
//!
//! On any other OS this just prints a message and exits.

#[cfg(target_os = "macos")]
fn main() {
    use seam_core::protocol::InputEvent;
    use seam_core::traits::InputCapture;
    use seam_platform::macos::Capture;

    tracing_subscriber::fmt::init();

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<InputEvent>();
        let mut capture = Capture::new();
        capture.start(tx).expect(
            "failed to start capture — grant Accessibility + Input Monitoring to this process",
        );

        run_phase(&mut rx, "PHASE 1 — unsuppressed", 6).await;

        println!("\nEnabling suppression (same as `RemoteActive` in the real app)...");
        capture
            .set_suppression(true)
            .expect("set_suppression failed");

        run_phase(&mut rx, "PHASE 2 — suppressed", 6).await;

        capture
            .set_suppression(false)
            .expect("set_suppression failed");
        capture.stop().expect("failed to stop capture");
    });
}

#[cfg(target_os = "macos")]
async fn run_phase(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<seam_core::protocol::InputEvent>,
    label: &str,
    seconds: u64,
) {
    use seam_core::protocol::InputEvent;

    println!("\n{label}: move the mouse in one continuous sweep for {seconds}s now...");
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(seconds);

    let mut samples: u64 = 0;
    let mut sum_abs_dx: i64 = 0;
    let mut sum_abs_dy: i64 = 0;
    let mut net_x: i64 = 0;
    let mut net_y: i64 = 0;
    let mut max_abs_single_dx: i32 = 0;

    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Some(InputEvent::MouseDelta { dx, dy }) => {
                        samples += 1;
                        sum_abs_dx += i64::from(dx.abs());
                        sum_abs_dy += i64::from(dy.abs());
                        net_x += i64::from(dx);
                        net_y += i64::from(dy);
                        max_abs_single_dx = max_abs_single_dx.max(dx.abs());
                    }
                    Some(_) => {}
                    None => break,
                }
            }
            () = tokio::time::sleep_until(deadline) => break,
        }
    }

    println!(
        "{label} result: {samples} delta samples, sum|dx|={sum_abs_dx} sum|dy|={sum_abs_dy}, \
         net=({net_x},{net_y}), largest single dx={max_abs_single_dx}"
    );
    if samples == 0 {
        println!("  -> NO MouseDelta samples arrived at all during this phase.");
    } else if sum_abs_dx < 200 && sum_abs_dy < 200 {
        println!(
            "  -> total travel is tiny even summed across the whole phase — suspect capture \
             itself (the bogus-zone filter or the anchor warp), not anything downstream."
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("macos_suppression_delta_demo is macOS-only; nothing to run on this platform.");
}
