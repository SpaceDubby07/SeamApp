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
//! Both the control port (24800) and the bulk port (24801, Tier 6.5:
//! control port + 1) are fixed — this demo doesn't take a custom port.
//!
//! Push your cursor to the shared edge (A's right / B's left) to hand
//! off. Ctrl+Alt+Shift+Escape forces control back to whichever machine
//! you press it on, regardless of who's currently driving. Copying text or
//! an image on either machine syncs it to the other's clipboard (M7).
//!
//! Append `--remap windows-on-mac` to either invocation to load the
//! Ctrl<->Cmd remap preset for that run (M6, Tier 7.3) — mostly useful for
//! poking at the remap table's config persistence with two Windows boxes,
//! since the real Ctrl<->Cmd swap only matters once one side is a Mac.

/// Control port (Tier 6.5). Fixed for this demo — no custom-port support.
#[cfg(windows)]
const CONTROL_PORT: u16 = 24800;

/// Bulk port: control port + 1, by the same Tier 6.5 convention.
#[cfg(windows)]
const BULK_PORT: u16 = 24801;

#[cfg(windows)]
fn main() {
    use seam_core::config::Config;
    use seam_core::net::bulk::BulkChannel;
    use seam_core::net::control::ControlChannel;
    use seam_core::protocol::OsKind;
    use seam_core::session::Session;
    use seam_core::state::StateMachine;
    use seam_core::topology::{Layout, Rect};
    use seam_core::traits::ScreenInfo;
    use seam_platform::windows::{Capture, Clipboard, Screens, Sink};

    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let role = args.get(1).cloned();
    let peer_arg = args.get(2).cloned();

    // M6: node identity, display name, and the remap table now come from
    // persisted config rather than being regenerated every run — pass
    // `--remap windows-on-mac` to load the Ctrl<->Cmd preset when this
    // Windows machine is driving a Mac peer.
    let config_path = Config::default_path().expect("could not determine config directory");
    let mut config = Config::load_or_create(&config_path).expect("failed to load config");
    if args.get(3).map(String::as_str) == Some("--remap")
        && args.get(4).map(String::as_str) == Some("windows-on-mac")
    {
        config.remap = seam_core::remap::RemapTable::windows_keyboard_on_mac();
        println!("Using the windows-keyboard-on-mac remap preset for this run.");
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async move {
        let role = role.as_deref();
        let screens = Screens::new();
        let local_bounds = screens.virtual_bounds();
        println!("Local virtual desktop bounds: {local_bounds:?}");

        let local_node = config.node_id;

        let (control, bulk, peer_on_right) = match role {
            Some("listen") => {
                let control_addr = format!("0.0.0.0:{CONTROL_PORT}");
                let bulk_addr = format!("0.0.0.0:{BULK_PORT}");
                let control_listener = tokio::net::TcpListener::bind(&control_addr)
                    .await
                    .expect("failed to bind the control port — already in use?");
                let bulk_listener = tokio::net::TcpListener::bind(&bulk_addr)
                    .await
                    .expect("failed to bind the bulk port — already in use?");
                println!(
                    "Listening on {control_addr} (control) and {bulk_addr} (bulk). Waiting for the peer..."
                );
                let control = ControlChannel::accept(
                    &control_listener,
                    local_node,
                    &config.display_name,
                    OsKind::Windows,
                )
                .await
                .expect("control handshake failed");
                let bulk = BulkChannel::accept(&bulk_listener)
                    .await
                    .expect("bulk channel accept failed");
                (control, bulk, true)
            }
            Some("connect") => {
                let host = peer_arg.expect("usage: connect <peer-host>");
                // Both ports are fixed by convention (Tier 6.5); strip any
                // port the user tacked on rather than trying to derive the
                // bulk port from a custom control port.
                let host = host.split(':').next().unwrap_or(&host).to_string();
                let control_target = format!("{host}:{CONTROL_PORT}");
                let bulk_target = format!("{host}:{BULK_PORT}");
                println!("Connecting to {control_target} (control) and {bulk_target} (bulk)...");
                let control = ControlChannel::connect(
                    control_target,
                    local_node,
                    &config.display_name,
                    OsKind::Windows,
                )
                .await
                .expect("control handshake failed");
                let bulk = BulkChannel::connect(bulk_target)
                    .await
                    .expect("bulk channel connect failed");
                (control, bulk, false)
            }
            _ => {
                eprintln!("usage: windows_handoff_demo listen | connect <peer-host>");
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
        let clipboard = Box::new(Clipboard::new());

        let session = Session::new(state_machine, control, bulk, capture, sink, clipboard, &config)
            .expect(
                "failed to start input capture/clipboard watch — run this interactively, not \
                 as a scheduled task",
            );

        println!("Session running. Push your cursor to the shared edge to hand off.");
        println!("Ctrl+Alt+Shift+Escape forces control back to whichever machine you press it on.");
        println!("Clipboard sync is live — copy text or an image on either machine.");

        if let Err(e) = session.run().await {
            eprintln!("session ended: {e}");
        }
    });
}

#[cfg(not(windows))]
fn main() {
    println!("windows_handoff_demo is Windows-only; nothing to run on this platform.");
}
