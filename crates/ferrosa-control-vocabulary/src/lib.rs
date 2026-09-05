//! Module: The control-channel command vocabulary, defined once.
//!
//! Every side that speaks the control channel needs the same answers about a
//! command: what it is called on the wire, whether it changes anything, what a
//! peer must hold to issue it, and whether the frame may contain a secret.
//!
//! Those answers used to live in two places -- `ControlCommandType` in the
//! mobile core and `CoordinatorCommand` in the listener -- and the wire names
//! had to match exactly or a command silently became "unknown". They drifted
//! the first time anyone added one:
//!
//!   `coordinator_offer` was added to the listener, the streamer built cleanly
//!   against a DIFFERENT worktree of the same repo, and the resulting binary
//!   did not know the command. Nothing failed. The app simply waited.
//!
//! One definition removes the class of bug rather than that instance of it: a
//! name added here is a name both sides know, and a name they disagree about
//! cannot be expressed.
//!
//! ## Why it has no dependencies
//!
//! It is compiled by the phone, the desktop app, the listener and the
//! streamer. Anything it pulls in, they all pull in. Names and classifications
//! need nothing, so it takes nothing.
//!
//! Correctness: correct when a command's wire name round-trips, an unknown
//! name is preserved rather than discarded, and every command classifies its
//! effect, its capability and whether it may carry a secret.
//!
//! Last revised: 2026-08-31
//! Last changed: Added the VM lifecycle and elevation commands used by mobile.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The capability a peer must hold to drive a coordinator.
///
/// One capability covers every coordinator command today. Named here rather
/// than repeated so a future command needing something narrower has one place
/// to say so.
pub const COORDINATOR_CAPABILITY: &str = "coordinator_control";

/// Whether a command changes anything on the machine that runs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Answers a question and alters nothing.
    Read,
    /// Changes state.
    Write,
}

/// A command that can travel the control channel.
///
/// `Unknown` keeps the raw name rather than discarding it, because a peer
/// running a newer build will send names this one does not have, and the
/// difference between "a command I do not know" and "no command" decides
/// whether the reply is a refusal that says so or a silence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Launch an agent with an instruction.
    AgentLaunch,
    /// List the team and its enforcement report.
    TeammateList,
    /// List secret requests awaiting a human.
    SecretPendingList,
    /// Supply the value for one secret request.
    SecretFulfil,
    /// Refuse one secret request.
    SecretDeny,
    /// List running microVMs.
    VmList,
    /// Start a microVM from an image the machine advertised.
    ///
    /// Named `vm_launch` and not `launch`, one word from `agent_launch`, which
    /// starts a teammate INSIDE a runtime rather than creating one.
    VmLaunch,
    /// Stop a microVM and discard its running state without archiving it.
    VmStop,
    /// Write a running microVM to disk and stop it.
    VmHibernate,
    /// Wake a hibernated microVM from its snapshot.
    VmResume,
    /// Move a stopped microVM workspace to durable storage.
    VmArchive,
    /// Report what this machine's coordinator can run: which tiers are live,
    /// what images it holds, and how much room is left.
    CoordinatorOffer,
    /// List elevation requests waiting for a human on this machine.
    ApprovalList,
    /// Mark an elevation request handled.
    ApprovalResolve,
    /// Begin a note.
    NoteOpen,
    /// Add one finalised utterance, or a typed note's whole body.
    NoteAppend,
    /// Close a note.
    NoteCommit,
    /// A name this build does not know, kept verbatim.
    Unknown(String),
}

impl Command {
    /// The wire name.
    pub fn as_wire(&self) -> &str {
        match self {
            Self::AgentLaunch => "agent_launch",
            Self::TeammateList => "teammate_list",
            Self::SecretPendingList => "secret_pending_list",
            Self::SecretFulfil => "secret_fulfil",
            Self::SecretDeny => "secret_deny",
            Self::VmList => "vm_list",
            Self::VmLaunch => "vm_launch",
            Self::VmStop => "vm_stop",
            Self::VmHibernate => "vm_hibernate",
            Self::VmResume => "vm_resume",
            Self::VmArchive => "vm_archive",
            Self::CoordinatorOffer => "coordinator_offer",
            Self::ApprovalList => "approval_list",
            Self::ApprovalResolve => "approval_resolve",
            Self::NoteOpen => "note_open",
            Self::NoteAppend => "note_append",
            Self::NoteCommit => "note_commit",
            Self::Unknown(raw) => raw,
        }
    }

    /// Read a wire name. Never fails: an unrecognised name is preserved.
    pub fn from_wire(raw: &str) -> Self {
        match raw {
            "agent_launch" => Self::AgentLaunch,
            "teammate_list" => Self::TeammateList,
            "secret_pending_list" => Self::SecretPendingList,
            "secret_fulfil" => Self::SecretFulfil,
            "secret_deny" => Self::SecretDeny,
            "vm_list" => Self::VmList,
            "vm_launch" => Self::VmLaunch,
            "vm_stop" => Self::VmStop,
            "vm_hibernate" => Self::VmHibernate,
            "vm_resume" => Self::VmResume,
            "vm_archive" => Self::VmArchive,
            "coordinator_offer" => Self::CoordinatorOffer,
            "approval_list" => Self::ApprovalList,
            "approval_resolve" => Self::ApprovalResolve,
            "note_open" => Self::NoteOpen,
            "note_append" => Self::NoteAppend,
            "note_commit" => Self::NoteCommit,
            other => Self::Unknown(other.to_owned()),
        }
    }

    /// Whether this build can execute it.
    pub fn is_known(&self) -> bool {
        !matches!(self, Self::Unknown(_))
    }

    /// Whether this command is one the coordinator answers.
    ///
    /// The listener uses this to decide what to forward; the app uses it to
    /// decide what needs the coordinator capability. One answer, so the two
    /// cannot disagree about which commands those are.
    pub fn is_coordinator_command(&self) -> bool {
        matches!(
            self,
            Self::TeammateList
                | Self::SecretPendingList
                | Self::SecretFulfil
                | Self::SecretDeny
                | Self::VmList
                | Self::VmLaunch
                | Self::VmStop
                | Self::VmHibernate
                | Self::VmResume
                | Self::VmArchive
                | Self::CoordinatorOffer
        )
    }

    /// Whether it changes anything.
    ///
    /// An unknown command is treated as a WRITE. Guessing "read" for something
    /// this build cannot classify would let an unrecognised name past a
    /// read-only guard, and the safe direction for an unknown is the
    /// restrictive one.
    pub fn effect(&self) -> Effect {
        match self {
            Self::TeammateList
            | Self::SecretPendingList
            | Self::VmList
            | Self::ApprovalList
            | Self::CoordinatorOffer => Effect::Read,
            Self::AgentLaunch
            | Self::VmLaunch
            | Self::VmStop
            | Self::VmHibernate
            | Self::VmResume
            | Self::VmArchive
            | Self::SecretFulfil
            | Self::SecretDeny
            | Self::ApprovalResolve
            | Self::NoteOpen
            | Self::NoteAppend
            | Self::NoteCommit
            | Self::Unknown(_) => Effect::Write,
        }
    }

    /// Whether the frame carrying this command may contain a secret value.
    ///
    /// Consulted before rendering a frame in a log. The redaction that protects
    /// the value on the app side does not travel with the JSON, so anything
    /// logging frames must ask here.
    pub fn carries_secret(&self) -> bool {
        matches!(self, Self::SecretFulfil)
    }

    /// Every command this build knows, for exhaustive tests and for listing
    /// what a peer may send.
    pub fn all_known() -> Vec<Command> {
        vec![
            Self::AgentLaunch,
            Self::TeammateList,
            Self::SecretPendingList,
            Self::SecretFulfil,
            Self::SecretDeny,
            Self::VmList,
            Self::VmLaunch,
            Self::VmStop,
            Self::VmHibernate,
            Self::VmResume,
            Self::VmArchive,
            Self::CoordinatorOffer,
            Self::ApprovalList,
            Self::ApprovalResolve,
            Self::NoteOpen,
            Self::NoteAppend,
            Self::NoteCommit,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launching_a_vm_is_a_write_and_a_coordinator_command() {
        // The last coordinator command that CREATES something. Classified with
        // hibernate and resume rather than with the listings.
        assert_eq!(Command::VmLaunch.effect(), Effect::Write);
        assert!(Command::VmLaunch.is_coordinator_command());
        assert!(!Command::VmLaunch.carries_secret());
    }

    #[test]
    fn vm_launch_is_not_agent_launch() {
        // Two different things a peer can ask for, and the names are one word
        // apart. agent_launch starts a teammate inside an existing runtime;
        // vm_launch creates the runtime. Sending one for the other would be
        // accepted by the listener and do something else entirely.
        assert_eq!(Command::VmLaunch.as_wire(), "vm_launch");
        assert_eq!(Command::AgentLaunch.as_wire(), "agent_launch");
        assert_eq!(Command::from_wire("vm_launch"), Command::VmLaunch);
        assert!(!Command::AgentLaunch.is_coordinator_command());
    }

    /// Hibernation is the first pair of commands that CHANGES a VM rather than
    /// listing one, so the classification matters more than the spelling.
    #[test]
    fn hibernate_and_resume_are_writes_not_reads() {
        // vm_list sits next to these in every listing and is a Read. Copying
        // its classification across would put a command that stops a running
        // machine behind a read-only guard, which is the one direction that
        // must never happen.
        assert_eq!(Command::VmHibernate.effect(), Effect::Write);
        assert_eq!(Command::VmResume.effect(), Effect::Write);
    }

    #[test]
    fn hibernate_and_resume_are_answered_by_the_coordinator() {
        assert!(Command::VmHibernate.is_coordinator_command());
        assert!(Command::VmResume.is_coordinator_command());
    }

    #[test]
    fn hibernate_and_resume_are_spelled_the_way_both_sides_expect() {
        assert_eq!(Command::VmHibernate.as_wire(), "vm_hibernate");
        assert_eq!(Command::VmResume.as_wire(), "vm_resume");
        assert_eq!(Command::from_wire("vm_hibernate"), Command::VmHibernate);
        assert_eq!(Command::from_wire("vm_resume"), Command::VmResume);
    }

    #[test]
    fn neither_hibernate_nor_resume_carries_a_secret() {
        // Both name a VM and nothing else, so a frame carrying one is safe to
        // render in a log. Saying so explicitly means a future field that DID
        // carry something has to change this test to land.
        assert!(!Command::VmHibernate.carries_secret());
        assert!(!Command::VmResume.carries_secret());
    }

    /// The property the crate exists for: a name written by one side is read
    /// back as the same command by the other. Both sides call these functions,
    /// so a disagreement cannot be expressed.
    #[test]
    fn every_known_command_round_trips() {
        for command in Command::all_known() {
            let wire = command.as_wire().to_owned();
            assert_eq!(
                Command::from_wire(&wire),
                command,
                "{wire} did not survive a round trip"
            );
        }
    }

    /// The exact command that drifted. Named on its own so a rename cannot
    /// pass by only updating the generic round-trip above.
    #[test]
    fn coordinator_offer_is_spelled_the_way_both_sides_expect() {
        assert_eq!(Command::CoordinatorOffer.as_wire(), "coordinator_offer");
        assert_eq!(
            Command::from_wire("coordinator_offer"),
            Command::CoordinatorOffer
        );
        assert!(Command::CoordinatorOffer.is_coordinator_command());
        assert_eq!(Command::CoordinatorOffer.effect(), Effect::Read);
        assert!(!Command::CoordinatorOffer.carries_secret());
    }

    /// A peer on a newer build sends names this one lacks. Keeping the raw
    /// name lets the reply say "I do not know that" instead of nothing, which
    /// is the difference between a diagnosable refusal and the silence the app
    /// sat in.
    #[test]
    fn an_unknown_name_is_kept_rather_than_discarded() {
        let unknown = Command::from_wire("something_from_a_newer_build");
        assert!(!unknown.is_known());
        assert_eq!(unknown.as_wire(), "something_from_a_newer_build");
    }

    /// An unclassifiable command must not slip past a read-only guard.
    #[test]
    fn an_unknown_command_is_treated_as_a_write() {
        assert_eq!(
            Command::from_wire("who_knows").effect(),
            Effect::Write,
            "an unknown command was classified as a read"
        );
    }

    /// Exactly one command may carry a credential. Asserted across the whole
    /// set, so adding one that carries a secret without saying so fails here.
    #[test]
    fn only_secret_fulfil_carries_a_secret() {
        for command in Command::all_known() {
            let expected = command == Command::SecretFulfil;
            assert_eq!(
                command.carries_secret(),
                expected,
                "{} classified its secret-carrying wrongly",
                command.as_wire()
            );
        }
    }

    #[test]
    fn only_coordinator_commands_are_coordinator_commands() {
        for command in Command::all_known() {
            let expected = matches!(
                command,
                Command::TeammateList
                    | Command::SecretPendingList
                    | Command::SecretFulfil
                    | Command::SecretDeny
                    | Command::VmList
                    | Command::VmLaunch
                    | Command::VmHibernate
                    | Command::VmResume
                    | Command::VmStop
                    | Command::VmArchive
                    | Command::CoordinatorOffer
            );
            assert_eq!(
                command.is_coordinator_command(),
                expected,
                "{}",
                command.as_wire()
            );
        }
    }

    /// Two commands sharing a wire name would make one unreachable.
    #[test]
    fn no_two_commands_share_a_wire_name() {
        let known = Command::all_known();
        let mut names: Vec<&str> = known.iter().map(|c| c.as_wire()).collect();
        let before = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), before, "two commands share a wire name");
    }

    /// A typo, an empty name, or one with stray whitespace must not silently
    /// match a real command.
    #[test]
    fn near_misses_do_not_match() {
        for raw in [
            "",
            " coordinator_offer",
            "coordinator_offer ",
            "CoordinatorOffer",
        ] {
            assert!(
                !Command::from_wire(raw).is_known(),
                "{raw:?} matched a known command"
            );
        }
    }

    #[test]
    fn current_mobile_commands_round_trip_and_keep_their_effects() {
        for (wire, expected, effect, coordinator) in [
            ("vm_stop", "vm_stop", Effect::Write, true),
            ("vm_archive", "vm_archive", Effect::Write, true),
            ("approval_list", "approval_list", Effect::Read, false),
            ("approval_resolve", "approval_resolve", Effect::Write, false),
        ] {
            let command = Command::from_wire(wire);
            assert_eq!(command.as_wire(), expected);
            assert_eq!(command.effect(), effect);
            assert_eq!(command.is_coordinator_command(), coordinator);
            assert!(command.is_known());
        }
    }
}
