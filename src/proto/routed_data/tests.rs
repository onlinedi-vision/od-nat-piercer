#![allow(clippy::disallowed_methods)] // Parser boundary assertions, no networking.

use super::*;

#[test]
fn counted_fields_leave_the_remaining_payload_opaque() {
    let raw = "RELAY_DATA origin 2 first second 00ff  7  DATA direct\t00\n";
    let mirror = RelayData::parse(raw).expect("valid mirror");
    assert_eq!(mirror.data.sender, "origin");
    assert_eq!(mirror.direct, ["first", "second"]);
    assert_eq!(mirror.data.payload, "00ff  7  DATA direct\t00");
    assert_eq!(mirror.encode().expect("encode"), raw.trim_end_matches('\n'));
    let empty_set = RelayData::parse("RELAY_DATA origin 0 00ff").expect("zero exclusions");
    assert_eq!(empty_set.direct, Vec::<&str>::new());
}

#[test]
fn invalid_counts_and_truncated_fields_are_rejected() {
    for raw in [
        "RELAY_DATA",
        "RELAY_DATA origin",
        "RELAY_DATA origin 0",
        "RELAY_DATA origin 0 ",
        "RELAY_DATA origin 1 direct",
        "RELAY_DATA origin 2 direct 00ff",
        "RELAY_DATA origin -1 00ff",
        "RELAY_DATA origin +0 00ff",
        "RELAY_DATA origin x 00ff",
        "RELAY_DATA origin 99999999999999999999999999999 00ff",
        "RELAY_DATA origin  0 00ff",
        "RELAY_DATA origin 1 \t 00ff",
        "RELAY_DATA origin 0 00ff\n\n",
        "RELAY_DATA origin 0 00ff\r\n",
    ] {
        assert!(RelayData::parse(raw).is_none(), "accepted {raw:?}");
    }
    for raw in [
        "DATA origin",
        "DATA  00ff",
        "DATA origin ",
        "DATA origin 00ff\nDATA origin 0000",
    ] {
        assert!(Data::parse(raw).is_none(), "accepted {raw:?}");
    }
}

#[test]
fn exact_size_boundary_is_accepted_without_truncation() {
    let prefix = "RELAY_DATA origin 1 direct ";
    let payload = "a".repeat(MAX_ROUTED_DATA_BYTES - prefix.len());
    let raw = format!("{prefix}{payload}");
    let mirror = RelayData::parse(&raw).expect("exact boundary");
    assert_eq!(mirror.encode().expect("exact encode"), raw);
    assert_eq!(mirror.data.payload.len(), payload.len());
    assert!(RelayData::parse(&format!("{raw}a")).is_none());
    let oversized = format!("{payload}a");
    assert!(
        RelayData {
            data: Data {
                sender: "origin",
                payload: &oversized
            },
            direct: vec!["direct"]
        }
        .encode()
        .is_none()
    );

    let raw = format!(
        "DATA origin {}",
        "a".repeat(MAX_ROUTED_DATA_BYTES - "DATA origin ".len())
    );
    assert!(Data::parse(&raw).is_some());
    // A trailing LF counts towards the datagram size; do not trim before checking.
    assert!(Data::parse(&format!("{raw}\n")).is_none());
}
