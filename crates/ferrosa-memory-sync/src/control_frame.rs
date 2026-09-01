//! Module: Building a control frame the listener will accept.
//!
//! The listener's parser is strict in three ways that are easy to get wrong by
//! hand, and each produces a different unhelpful refusal: the frame needs a
//! non-empty `frame_id` of at most 128 bytes, `body.command_id` must parse as a
//! UUID and be **version 7** specifically, and `body.command_type` must be a
//! name the shared vocabulary knows.
//!
//! Written here rather than assembled at a shell prompt because a v7 UUID is
//! not something you type, and a v4 one is refused with "command_id must be a
//! UUIDv7" -- which reads like a malformed id rather than the wrong kind.
//!
//! Correctness: correct when the frame it produces satisfies every check
//! `dispatch_command` makes before it looks at the command itself.
//!
//! Last revised: 2026-08-31
//! Last changed: New module.

use uuid::Uuid;

/// One control frame, ready to send as a line of text.
///
/// `payload` is placed under `body.payload` verbatim, because that is where
/// every command's arguments are read from.
pub fn control_frame(command: &str, payload: serde_json::Value) -> serde_json::Value {
    serde_json::json!({
        "frame_id": Uuid::now_v7().to_string(),
        "body": {
            // Version 7 specifically. The listener filters on the version, so a
            // v4 id is refused with a message about the id rather than about
            // its version.
            "command_id": Uuid::now_v7().to_string(),
            "command_type": command,
            "payload": payload,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_id_is_a_uuidv7_because_the_listener_checks_the_version() {
        let frame = control_frame("vm_hibernate", serde_json::json!({"id": "hib-demo"}));
        let id = frame
            .pointer("/body/command_id")
            .and_then(serde_json::Value::as_str)
            .expect("a command id");
        let parsed = Uuid::parse_str(id).expect("parses");
        assert_eq!(
            parsed.get_version_num(),
            7,
            "a v4 id is refused with a message about the id, not its version"
        );
    }

    #[test]
    fn the_frame_id_is_present_and_within_the_listeners_limit() {
        let frame = control_frame("vm_resume", serde_json::json!({}));
        let id = frame
            .get("frame_id")
            .and_then(serde_json::Value::as_str)
            .expect("a frame id");
        assert!(!id.is_empty(), "an empty frame id is refused");
        assert!(id.len() <= 128, "the listener caps this at 128 bytes");
    }

    #[test]
    fn the_payload_is_carried_verbatim_where_commands_read_it() {
        let frame = control_frame("vm_hibernate", serde_json::json!({"id": "vm-1"}));
        assert_eq!(
            frame
                .pointer("/body/payload/id")
                .and_then(serde_json::Value::as_str),
            Some("vm-1")
        );
        assert_eq!(
            frame
                .pointer("/body/command_type")
                .and_then(serde_json::Value::as_str),
            Some("vm_hibernate")
        );
    }

    #[test]
    fn two_frames_do_not_share_an_id() {
        // The listener keys durable commands by command_id; two frames with the
        // same one would be the same command asked twice.
        let a = control_frame("vm_list", serde_json::json!({}));
        let b = control_frame("vm_list", serde_json::json!({}));
        assert_ne!(a.pointer("/body/command_id"), b.pointer("/body/command_id"));
        assert_ne!(a.get("frame_id"), b.get("frame_id"));
    }
}
