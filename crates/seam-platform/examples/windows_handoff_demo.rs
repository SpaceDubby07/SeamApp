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
//! Or skip the IP entirely (M9, Tier 7.6) — both sides advertise
//! themselves over mDNS regardless of role, so `discover` finds whichever
//! peer answers first and connects to it:
//!   cargo run -p seam-platform --example windows_handoff_demo -- discover
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
//!
//! The FIRST time these two machines connect, both TLS certificates are
//! new to each other — this demo drops into the pairing flow (M8, Tier
//! 7.6): it prints a 6-digit code and waits for you to press Enter after
//! confirming the SAME code printed on the other machine, then pins the
//! peer's fingerprint to config so every later run connects straight
//! through. Running `connect`/`listen` again against an already-paired
//! peer whose certificate has since changed (e.g. a re-imaged machine, or
//! an actual attacker) hard-fails instead of silently reconnecting.
//!
//! Append `--send <path>` to either invocation to offer that file to the
//! peer once connected (M10, Tier 7.5) — progress and completion print to
//! the console, standing in for the eventual UI's progress bar. This demo
//! always auto-accepts incoming offers (writing them to the OS Downloads
//! folder) rather than exercising `AcceptPolicy::Ask`'s prompt flow, which
//! needs real UI (M11) to be worth wiring up here.

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
    use seam_core::net::discovery::{Discovery, DiscoveryEvent};
    use seam_core::net::pairing::pairing_code;
    use seam_core::net::tls::{NodeIdentity, Trust};
    use seam_core::protocol::OsKind;
    use seam_core::session::Session;
    use seam_core::session::{SessionCommand, SessionEvent};
    use seam_core::state::StateMachine;
    use seam_core::topology::{Layout, Rect};
    use seam_core::traits::ScreenInfo;
    use seam_core::transfer::AcceptPolicy;
    use seam_platform::windows::{Capture, Clipboard, Screens, Sink};

    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let role = args.get(1).cloned();
    let peer_arg = args.get(2).cloned();
    // M10: `--send <path>` anywhere in the arguments queues that file for
    // send once connected — independent of `--remap`'s fixed position,
    // since either flag may or may not be present on a given run.
    let send_path = args
        .iter()
        .position(|a| a == "--send")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from);

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
    // M10: this demo has no prompt UI for `AcceptPolicy::Ask`, so it
    // always auto-accepts — a real UI (M11) is where `Ask`'s prompt flow
    // actually belongs.
    config.accept_policy = AcceptPolicy::AlwaysAccept;

    // M8: this machine's own TLS identity, generated once and reused for
    // every run (Tier 7.6).
    let identity_dir = Config::identity_dir().expect("could not determine identity directory");
    let identity =
        NodeIdentity::load_or_create(&identity_dir).expect("failed to load TLS identity");
    let trust = config.trust_mode();
    let is_pairing = matches!(trust, Trust::OnFirstUse);
    if is_pairing {
        println!("No paired peer on file yet — this connection will go through the pairing flow.");
    }

    let rt = tokio::runtime::Runtime::new().expect("failed to start the tokio runtime");
    rt.block_on(async move {
        let role = role.as_deref();
        let screens = Screens::new();
        let local_bounds = screens.virtual_bounds();
        println!("Local virtual desktop bounds: {local_bounds:?}");

        let local_node = config.node_id;

        // M9: advertise ourselves via mDNS regardless of role, so the
        // OTHER side's `discover` can find us — and resolve our own
        // `discover` against whatever peer answers, rather than requiring
        // an IP to be typed on either machine (Tier 8.1's "primary path").
        let discovery = Discovery::new().expect("failed to start the mDNS daemon");
        discovery
            .advertise(
                local_node,
                &config.display_name,
                OsKind::Windows,
                CONTROL_PORT,
            )
            .expect("failed to advertise via mDNS");

        let (role, peer_arg) = if role == Some("discover") {
            println!("Browsing for a Seam peer via mDNS...");
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            discovery
                .browse(local_node, tx)
                .expect("failed to start mDNS browse");
            let peer = loop {
                match rx
                    .recv()
                    .await
                    .expect("mDNS browse task ended unexpectedly")
                {
                    DiscoveryEvent::Found(peer) => break peer,
                    DiscoveryEvent::Lost(_) => continue,
                }
            };
            println!(
                "Discovered peer '{}' ({:?}) at {}:{} — connecting.",
                peer.display_name, peer.os, peer.addr, peer.control_port
            );
            (Some("connect"), Some(peer.addr.to_string()))
        } else {
            (role, peer_arg)
        };

        let control = match role {
            Some("listen") => {
                let control_addr = format!("0.0.0.0:{CONTROL_PORT}");
                let control_listener = tokio::net::TcpListener::bind(&control_addr)
                    .await
                    .expect("failed to bind the control port — already in use?");
                println!("Listening on {control_addr} (control). Waiting for the peer...");
                ControlChannel::accept(
                    &control_listener,
                    local_node,
                    &config.display_name,
                    OsKind::Windows,
                    &identity,
                    trust,
                )
                .await
                .expect(
                    "control handshake failed — a Pinned mismatch means this peer's \
                          certificate no longer matches who you paired with",
                )
            }
            Some("connect") => {
                let host = peer_arg.clone().expect("usage: connect <peer-host>");
                // Both ports are fixed by convention (Tier 6.5); strip any
                // port the user tacked on rather than trying to derive the
                // bulk port from a custom control port.
                let host = host.split(':').next().unwrap_or(&host).to_string();
                let control_target = format!("{host}:{CONTROL_PORT}");
                println!("Connecting to {control_target} (control)...");
                ControlChannel::connect(
                    control_target,
                    local_node,
                    &config.display_name,
                    OsKind::Windows,
                    &identity,
                    trust,
                )
                .await
                .expect(
                    "control handshake failed — a Pinned mismatch means this peer's \
                          certificate no longer matches who you paired with",
                )
            }
            _ => {
                eprintln!("usage: windows_handoff_demo listen | connect <peer-host> | discover");
                std::process::exit(1);
            }
        };
        let peer_on_right = role == Some("listen");

        println!(
            "Connected to peer '{}' ({:?}).",
            control.peer_display_name, control.peer_node_id
        );

        // Pairing (M8, Tier 7.6): only reached the first time we've ever
        // talked to this peer. A human confirms the same code shows on
        // both screens before we pin the fingerprint — this is what
        // actually defeats a MITM (see `net::pairing`'s doc comment).
        if is_pairing {
            let code = pairing_code(identity.fingerprint, control.peer_fingerprint);
            println!("\nPAIRING CODE: {code}");
            println!(
                "Confirm this EXACT code is shown on the other machine, then press Enter to \
                 trust this peer. Ctrl+C to abort if it doesn't match."
            );
            let mut input = String::new();
            std::io::stdin()
                .read_line(&mut input)
                .expect("failed to read confirmation");
            config.pin_peer(control.peer_node_id, control.peer_fingerprint);
            config
                .save(&config_path)
                .expect("failed to save paired peer");
            println!("Peer paired and pinned — future runs will connect straight through.\n");
        }

        // Bulk channel: always pinned to whatever the control channel just
        // authenticated (never its own OnFirstUse) — see net::bulk's
        // module docs.
        let bulk_addr_host = peer_arg
            .as_deref()
            .and_then(|h| h.split(':').next())
            .unwrap_or("0.0.0.0")
            .to_string();
        let bulk = match role {
            Some("listen") => {
                let bulk_addr = format!("0.0.0.0:{BULK_PORT}");
                let bulk_listener = tokio::net::TcpListener::bind(&bulk_addr)
                    .await
                    .expect("failed to bind the bulk port — already in use?");
                println!("Listening on {bulk_addr} (bulk)...");
                BulkChannel::accept(&bulk_listener, &identity, control.peer_fingerprint)
                    .await
                    .expect("bulk channel accept failed")
            }
            _ => {
                let bulk_target = format!("{bulk_addr_host}:{BULK_PORT}");
                println!("Connecting to {bulk_target} (bulk)...");
                BulkChannel::connect(bulk_target, &identity, control.peer_fingerprint)
                    .await
                    .expect("bulk channel connect failed")
            }
        };

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

        let (mut session, handle) = Session::new(
            state_machine,
            control,
            bulk,
            capture,
            sink,
            clipboard,
            &config,
        )
        .expect(
            "failed to start input capture/clipboard watch — run this interactively, not \
                 as a scheduled task",
        );

        // M11: tells the peer our real screens once connected, so its
        // layout canvas can draw our tile to scale (Tier 8.1).
        session
            .send_screen_config(screens.displays(), local_bounds)
            .await
            .expect("failed to send screen config");
        let seam_core::session::SessionHandle {
            command_tx,
            mut event_rx,
        } = handle;
        let event_task_command_tx = command_tx.clone();

        println!("Session running. Push your cursor to the shared edge to hand off.");
        println!("Ctrl+Alt+Shift+Escape forces control back to whichever machine you press it on.");
        println!("Clipboard sync is live — copy text or an image on either machine.");
        println!(
            "Downloads folder for incoming transfers: {}",
            config.resolved_download_dir().display()
        );

        // M10: prints transfer progress/completion to the console, standing
        // in for the eventual UI's progress bar (Tier 8.1's Transfers
        // panel).
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    SessionEvent::OfferReceived {
                        transfer_id,
                        manifest,
                    } => {
                        println!(
                            "Incoming file '{}' ({} bytes) offered — accepting.",
                            manifest.name, manifest.size
                        );
                        let _ = event_task_command_tx.send(SessionCommand::RespondToOffer {
                            transfer_id,
                            accept: true,
                        });
                    }
                    SessionEvent::Progress {
                        bytes_done, total, ..
                    } => {
                        println!("transfer progress: {bytes_done}/{total} bytes");
                    }
                    SessionEvent::Rejected { reason, .. } => {
                        println!("transfer rejected: {reason}");
                    }
                    SessionEvent::Completed { path, .. } => {
                        println!("transfer complete: {}", path.display());
                    }
                    SessionEvent::Failed { reason, .. } => {
                        println!("transfer failed: {reason}");
                    }
                    SessionEvent::PeerScreenConfig {
                        displays,
                        virtual_bounds,
                    } => {
                        println!(
                            "Peer screens: {} display(s), virtual bounds {virtual_bounds:?}",
                            displays.len()
                        );
                    }
                    SessionEvent::LayoutChanged { peer_bounds } => {
                        println!("Layout changed: peer is now at {peer_bounds:?}");
                    }
                }
            }
        });

        if let Some(path) = send_path {
            println!("Offering {} to the peer...", path.display());
            command_tx
                .send(SessionCommand::SendFile(path))
                .expect("session command channel closed");
        }

        if let Err(e) = session.run().await {
            eprintln!("session ended: {e}");
        }
    });
}

#[cfg(not(windows))]
fn main() {
    println!("windows_handoff_demo is Windows-only; nothing to run on this platform.");
}
