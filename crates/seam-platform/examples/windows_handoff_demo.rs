//! M4 demo (Tier 13 of the build guide): a real Windows<->Windows handoff
//! over the network. Wires seam-platform's real Capture/Sink/Screens into
//! seam-core's `Session`, connects two machines over the control channel,
//! and lets you drive the cursor across the shared edge to see it land on
//! the other machine.
//!
//! REQUIRES TWO WINDOWS MACHINES (or two Windows VMs) on the same network.
//! Windows' low-level input hooks are global and per-process-independent —
//! running two instances of this demo on ONE machine means both processes
//! see and react to the SAME physical mouse/keyboard simultaneously, which
//! doesn't exercise anything meaningful. For single-machine protocol/state
//! testing without real hardware, see `seam-core`'s `session::tests`
//! (mock platforms driven over real loopback TCP — Tier 12.3's "two
//! in-process nodes over loopback" pattern) instead.
//!
//! Usage, on machine A (the "listener"):
//!   cargo run -p seam-platform --example windows_handoff_demo -- listen
//!
//! On machine B (the "connector"), using A's LAN IP:
//!   cargo run -p seam-platform --example windows_handoff_demo -- connect 192.168.1.50
//!
//! Push your cursor to the shared edge (A's right / B's left) to hand
//! off. Ctrl+Alt+Shift+Escape forces control back to whichever machine
//! you press it on, regardless of who's currently driving.

#[cfg(windows)]
fn main() {
    use seam_core::net::control::ControlChannel;
    use seam_core::protocol::OsKind;
    use seam_core::session::Session;
    use seam_core::state::StateMachine;
    use seam_core::topology::{Layout, NodeId, Rect};
    use seam_core::traits::ScreenInfo;
    use seam_platform::windows::{Capture, Screens, Sink};

    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let role = args.get(1).map(String::as_str);

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async move {
        let screens = Screens::new();
        let local_bounds = screens.virtual_bounds();
        println!("Local virtual desktop bounds: {local_bounds:?}");

        let local_node = NodeId::new();

        let (control, peer_on_right) = match role {
            Some("listen") => {
                let addr = "0.0.0.0:24800";
                let listener = tokio::net::TcpListener::bind(addr)
                    .await
                    .expect("failed to bind — is port 24800 already in use?");
                println!("Listening on {addr}. Waiting for the peer to connect...");
                let control =
                    ControlChannel::accept(&listener, local_node, &hostname(), OsKind::Windows)
                        .await
                        .expect("handshake failed");
                (control, true)
            }
            Some("connect") => {
                let target = args.get(2).expect("usage: connect <peer-ip>[:24800]");
                let target = if target.contains(':') {
                    target.clone()
                } else {
                    format!("{target}:24800")
                };
                println!("Connecting to {target}...");
                let control =
                    ControlChannel::connect(target, local_node, &hostname(), OsKind::Windows)
                        .await
                        .expect("handshake failed");
                (control, false)
            }
            _ => {
                eprintln!("usage: windows_handoff_demo listen | connect <peer-ip>[:24800]");
                std::process::exit(1);
            }
        };

        println!(
            "Connected to peer '{}' ({:?}).",
            control.peer_display_name, control.peer_node_id
        );

        let mut layout = Layout::new();
        layout.set_placement(local_node, local_bounds);
        let peer_bounds = if peer_on_right {
            Rect {
                x: local_bounds.x + local_bounds.width.cast_signed(),
                ..local_bounds
            }
        } else {
            Rect {
                x: local_bounds.x - local_bounds.width.cast_signed(),
                ..local_bounds
            }
        };
        layout.set_placement(control.peer_node_id, peer_bounds);

        let state_machine = StateMachine::new(local_node, local_bounds, layout);
        let capture = Box::new(Capture::new());
        let sink = Box::new(Sink::new());

        let session = Session::new(state_machine, control, capture, sink).expect(
            "failed to start input capture — run this interactively, not as a scheduled task",
        );

        println!("Session running. Push your cursor to the shared edge to hand off.");
        println!("Ctrl+Alt+Shift+Escape forces control back to whichever machine you press it on.");

        if let Err(e) = session.run().await {
            eprintln!("session ended: {e}");
        }
    });
}

#[cfg(windows)]
fn hostname() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "seam-node".to_string())
}

#[cfg(not(windows))]
fn main() {
    println!("windows_handoff_demo is Windows-only; nothing to run on this platform.");
}
