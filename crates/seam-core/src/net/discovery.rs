//! mDNS discovery (M9, Tier 7.6): advertise this machine's control channel
//! and browse for peers on the LAN so pairing doesn't require typing an
//! IP address. Manual IP entry via [`crate::net::control::ControlChannel`]
//! remains the fallback for networks that block mDNS (Tier 8.1).
//!
//! Entirely portable — `mdns-sd` owns the OS-specific socket/interface
//! differences internally, so unlike capture/inject/screens this stays in
//! `seam-core` rather than behind a `seam-platform` trait.

use std::collections::HashMap;
use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc::UnboundedSender;

use crate::protocol::{OsKind, PROTOCOL_VERSION};
use crate::topology::NodeId;

/// Service type every Seam node advertises and browses under (Tier 6.5).
pub const SERVICE_TYPE: &str = "_seam._tcp.local.";

/// TXT record keys carried on the advertised service (Tier 7.6): enough
/// for a browser to show a human-readable peer and dial it without a
/// separate handshake first.
mod txt_keys {
    pub const NODE_ID: &str = "node_id";
    pub const DISPLAY_NAME: &str = "display_name";
    pub const OS: &str = "os";
    pub const PROTOCOL_VERSION: &str = "protocol_version";
}

/// A peer discovered via mDNS, resolved to enough detail to dial it
/// directly with [`crate::net::control::ControlChannel::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredPeer {
    /// The peer's stable node identity, from its TXT record — the actual
    /// `Handshake` still carries the authoritative copy; this is only used
    /// to filter our own advertisement out of our own browse results and
    /// to correlate a later `ServiceRemoved` back to the right peer.
    pub node_id: NodeId,
    /// The peer's user-facing display name, shown in the connection panel
    /// before any connection is made.
    pub display_name: String,
    /// Which OS the peer runs.
    pub os: OsKind,
    /// A LAN address the peer resolved to. `mdns-sd` may resolve several;
    /// v1 just picks one, matching the project's single-peer simplification
    /// (Tier 15).
    pub addr: IpAddr,
    /// The port to dial for the control channel.
    pub control_port: u16,
}

/// A change in the set of discovered peers, forwarded from the browse task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryEvent {
    /// A peer was found or re-resolved (e.g. its address changed).
    Found(DiscoveredPeer),
    /// A previously-found peer's advertisement disappeared (it went
    /// offline or stopped advertising).
    Lost(NodeId),
}

/// Everything that can go wrong registering or browsing a service.
#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    /// The `mdns-sd` daemon failed to start, register, or browse.
    #[error("mDNS daemon error: {0}")]
    Daemon(#[from] mdns_sd::Error),
}

/// Owns the mDNS daemon thread `mdns-sd` spawns internally. Advertising and
/// browsing are independent calls — a node normally does both (Tier 8.1's
/// "discovered devices" panel while also being discoverable itself).
pub struct Discovery {
    daemon: ServiceDaemon,
}

impl Discovery {
    /// Starts the mDNS daemon. Nothing is advertised or browsed yet —
    /// call [`Self::advertise`] and/or [`Self::browse`] once this machine's
    /// identity and control port are known.
    ///
    /// # Errors
    /// Returns an error if the daemon's background thread or its sockets
    /// fail to start.
    pub fn new() -> Result<Self, DiscoveryError> {
        Ok(Self {
            daemon: ServiceDaemon::new()?,
        })
    }

    /// Advertises this machine's control channel under [`SERVICE_TYPE`].
    /// Calling this again (e.g. after `display_name` changes) re-announces
    /// under the same instance name rather than requiring an unregister
    /// first — `mdns-sd`'s `register` already handles that.
    ///
    /// # Errors
    /// Returns an error if the service info is invalid (shouldn't happen —
    /// every value here is already validated elsewhere: `node_id` is a
    /// UUID, `control_port` is a real bound port) or the daemon rejects
    /// the registration.
    pub fn advertise(
        &self,
        node_id: NodeId,
        display_name: &str,
        os: OsKind,
        control_port: u16,
    ) -> Result<(), DiscoveryError> {
        let instance_name = node_id.0.to_string();
        let hostname = format!("{instance_name}.local.");
        let protocol_version_str = PROTOCOL_VERSION.to_string();
        let properties = [
            (txt_keys::NODE_ID, instance_name.as_str()),
            (txt_keys::DISPLAY_NAME, display_name),
            (txt_keys::OS, os_to_str(os)),
            (txt_keys::PROTOCOL_VERSION, protocol_version_str.as_str()),
        ];

        // Empty address + `enable_addr_auto()` is `mdns-sd`'s documented
        // way to have it fill in this host's real addresses rather than
        // us enumerating interfaces ourselves.
        let service = ServiceInfo::new(
            SERVICE_TYPE,
            &instance_name,
            &hostname,
            "",
            control_port,
            &properties[..],
        )?
        .enable_addr_auto();

        self.daemon.register(service)?;
        Ok(())
    }

    /// Starts browsing for other Seam nodes and spawns a task that
    /// forwards [`DiscoveryEvent`]s to `tx` until either the daemon shuts
    /// down or `tx`'s receiver is dropped. `local_node_id` is filtered out
    /// so a machine never "discovers" its own advertisement.
    ///
    /// # Errors
    /// Returns an error if the daemon fails to start the browse.
    pub fn browse(
        &self,
        local_node_id: NodeId,
        tx: UnboundedSender<DiscoveryEvent>,
    ) -> Result<(), DiscoveryError> {
        let receiver = self.daemon.browse(SERVICE_TYPE)?;

        tokio::spawn(async move {
            // Maps a resolved service's DNS fullname back to the node id it
            // carried, since `ServiceRemoved` only gives us the fullname —
            // not the TXT record that's already gone by then.
            let mut fullname_to_node: HashMap<String, NodeId> = HashMap::new();

            while let Ok(event) = receiver.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(info) => {
                        let Some(peer) = parse_peer(&info) else {
                            tracing::debug!(
                                fullname = info.get_fullname(),
                                "ignoring a resolved _seam._tcp service with missing/invalid TXT records"
                            );
                            continue;
                        };
                        if peer.node_id == local_node_id {
                            continue;
                        }
                        fullname_to_node.insert(info.get_fullname().to_string(), peer.node_id);
                        if tx.send(DiscoveryEvent::Found(peer)).is_err() {
                            break;
                        }
                    }
                    ServiceEvent::ServiceRemoved(_ty_domain, fullname) => {
                        if let Some(node_id) = fullname_to_node.remove(&fullname)
                            && tx.send(DiscoveryEvent::Lost(node_id)).is_err()
                        {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }
}

/// Extracts a [`DiscoveredPeer`] from a resolved service's TXT records and
/// addresses. Returns `None` for anything malformed rather than erroring
/// the whole browse task — a single bad peer (e.g. a future protocol
/// version we don't understand the TXT shape of yet) shouldn't take down
/// discovery of everyone else.
fn parse_peer(info: &ServiceInfo) -> Option<DiscoveredPeer> {
    let node_id = info.get_property_val_str(txt_keys::NODE_ID)?;
    let node_id = NodeId(node_id.parse().ok()?);
    let display_name = info
        .get_property_val_str(txt_keys::DISPLAY_NAME)?
        .to_string();
    let os = os_from_str(info.get_property_val_str(txt_keys::OS)?)?;
    let addr = *info.get_addresses_v4().into_iter().next()?;

    Some(DiscoveredPeer {
        node_id,
        display_name,
        os,
        addr: IpAddr::V4(addr),
        control_port: info.get_port(),
    })
}

/// TXT records are plain UTF-8 strings — this is `OsKind`'s wire form
/// there, separate from its `postcard` encoding on the actual protocol
/// messages.
fn os_to_str(os: OsKind) -> &'static str {
    match os {
        OsKind::MacOs => "macos",
        OsKind::Windows => "windows",
    }
}

fn os_from_str(s: &str) -> Option<OsKind> {
    match s {
        "macos" => Some(OsKind::MacOs),
        "windows" => Some(OsKind::Windows),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{os_from_str, os_to_str, parse_peer, txt_keys};
    use crate::protocol::OsKind;
    use crate::topology::NodeId;
    use mdns_sd::ServiceInfo;

    #[test]
    fn os_str_roundtrips() {
        for os in [OsKind::MacOs, OsKind::Windows] {
            assert_eq!(os_from_str(os_to_str(os)), Some(os));
        }
    }

    #[test]
    fn unknown_os_str_is_none() {
        assert_eq!(os_from_str("linux"), None);
    }

    fn service_info(properties: &[(&str, &str)]) -> ServiceInfo {
        ServiceInfo::new(
            super::SERVICE_TYPE,
            "test-instance",
            "test-instance.local.",
            "192.168.1.50",
            24800,
            properties,
        )
        .expect("valid service info")
    }

    #[test]
    fn parses_a_well_formed_peer() {
        let node_id = NodeId::new();
        let info = service_info(&[
            (txt_keys::NODE_ID, &node_id.0.to_string()),
            (txt_keys::DISPLAY_NAME, "Zach's PC"),
            (txt_keys::OS, "windows"),
            (txt_keys::PROTOCOL_VERSION, "1"),
        ]);

        let peer = parse_peer(&info).expect("should parse");
        assert_eq!(peer.node_id, node_id);
        assert_eq!(peer.display_name, "Zach's PC");
        assert_eq!(peer.os, OsKind::Windows);
        assert_eq!(peer.control_port, 24800);
    }

    #[test]
    fn missing_node_id_fails_to_parse() {
        let info = service_info(&[
            (txt_keys::DISPLAY_NAME, "Zach's PC"),
            (txt_keys::OS, "windows"),
        ]);
        assert!(parse_peer(&info).is_none());
    }

    #[test]
    fn garbage_node_id_fails_to_parse() {
        let info = service_info(&[
            (txt_keys::NODE_ID, "not-a-uuid"),
            (txt_keys::DISPLAY_NAME, "Zach's PC"),
            (txt_keys::OS, "windows"),
        ]);
        assert!(parse_peer(&info).is_none());
    }

    #[test]
    fn unknown_os_fails_to_parse() {
        let node_id = NodeId::new();
        let info = service_info(&[
            (txt_keys::NODE_ID, &node_id.0.to_string()),
            (txt_keys::DISPLAY_NAME, "Zach's PC"),
            (txt_keys::OS, "linux"),
        ]);
        assert!(parse_peer(&info).is_none());
    }
}
