//! Which control commands are coordinator commands, and what each one requires.
//!
//! The app drives the coordinator over the control channel; the coordinator
//! answers on a loopback HTTP API. This module is the decision in between: what
//! a frame is asking for, whether it may be asked, and whether it is a read or
//! a write.
//!
//! It lives in the PUBLIC listener on purpose. Deciding whether a peer may
//! answer a credential prompt or boot a VM is an authorization decision, and no
//! private component gets to make one. The coordinator answers requests that
//! have already been authorized here.
//!
//! Nothing in this module performs I/O, so every rule is directly testable and
//! the set of things a peer may reach is readable in one place.

/// A coordinator operation reachable from the control channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinatorCommand {
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
}

/// What a command does to the world.
///
/// Reads and writes are treated differently on purpose. A read answers from
/// current state and is safe to repeat; recording every list in the durable
/// command log would bury the events that describe actual changes under a
/// stream of polling. A write is recorded, so the log says who answered a
/// credential prompt and when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Answers from current state and changes nothing.
    Read,
    /// Changes state and belongs in the durable record.
    Write,
}

/// The privilege a command needs.
///
/// Separate from `agent_control` because they are not the same right. A peer
/// permitted to launch an agent is not thereby permitted to answer a credential
/// prompt or start a virtual machine, and collapsing the two would grant the
/// second every time someone wanted the first.
pub const COORDINATOR_CAPABILITY: &str = "coordinator_control";

/// The plan entitlement that includes agent teams.
///
/// An entitlement TURNS FUNCTIONALITY OFF; it does not deny access to
/// functionality that is present. That distinction decides where it belongs:
/// not in the authorization path.
///
/// The two are different in kind. A capability check is a trust boundary, and
/// getting it wrong is a privilege escalation. An entitlement is commercial,
/// and getting it wrong means somebody used a feature they had not paid for --
/// a billing problem. Running them through one decision makes the toggle look
/// like a security control and buries the control that actually is one.
///
/// So an unentitled account does not get "permission denied" for teams. It gets
/// a host with no team functionality on it, which is the same answer as a host
/// that has no coordinator, because from the caller's side it is the same fact.
/// The upgrade prompt belongs in the plan surface, driven by the entitlements
/// the client already holds -- not by a refused command.
pub const TEAMS_ENTITLEMENT: &str = "teams";

/// Whether team functionality exists for this caller at all.
///
/// Both inputs mean the same thing to a caller: there is nothing here. A host
/// without a coordinator has no team functionality installed; an account
/// without the entitlement has none enabled. Neither is a refusal.
pub fn teams_available(coordinator_installed: bool, entitlements: &[String]) -> bool {
    coordinator_installed && entitlements.iter().any(|held| held == TEAMS_ENTITLEMENT)
}

impl CoordinatorCommand {
    /// Recognise a wire `command_type`, or `None` if it is not a coordinator
    /// command.
    ///
    /// Returning `None` rather than an error lets the caller fall through to
    /// the commands it already handles, so this module does not have to know
    /// about them.
    pub fn from_wire(command_type: &str) -> Option<Self> {
        match command_type {
            "teammate_list" => Some(Self::TeammateList),
            "secret_pending_list" => Some(Self::SecretPendingList),
            "secret_fulfil" => Some(Self::SecretFulfil),
            "secret_deny" => Some(Self::SecretDeny),
            "vm_list" => Some(Self::VmList),
            _ => None,
        }
    }

    /// The wire name.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::TeammateList => "teammate_list",
            Self::SecretPendingList => "secret_pending_list",
            Self::SecretFulfil => "secret_fulfil",
            Self::SecretDeny => "secret_deny",
            Self::VmList => "vm_list",
        }
    }

    /// Whether this command changes anything.
    pub fn effect(self) -> Effect {
        match self {
            Self::TeammateList | Self::SecretPendingList | Self::VmList => Effect::Read,
            Self::SecretFulfil | Self::SecretDeny => Effect::Write,
        }
    }

    /// The capability a peer must hold to issue it.
    ///
    /// Every coordinator command requires the same one today. Written as a
    /// method rather than a constant so a future command needing something
    /// narrower has somewhere to say so.
    pub fn required_capability(self) -> &'static str {
        COORDINATOR_CAPABILITY
    }

    /// Whether the frame carrying this command may contain a secret value.
    ///
    /// Used to keep the value out of logs. A caller that logs frames must
    /// consult this before rendering one, because the redaction that protects
    /// the value on the app side does not travel with the JSON.
    pub fn carries_secret(self) -> bool {
        matches!(self, Self::SecretFulfil)
    }
}

/// Why a coordinator command was refused before it reached the coordinator.
///
/// Three reasons, kept apart because they need three different things from the
/// person reading them: buy something, get this device trusted, or stop looking
/// at this host. One shared "denied" would send someone to fix the wrong thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefusedReason {
    /// This device was not granted the capability.
    ///
    /// The only true refusal here, and the only one that is a trust boundary.
    MissingCapability {
        /// What it needed.
        required: &'static str,
    },
    /// There is no team functionality here.
    ///
    /// Either the host runs no coordinator or the account has no teams
    /// entitlement. Deliberately ONE answer: both mean "nothing here" from the
    /// caller's side, and an unentitled caller learning which hosts run a
    /// coordinator is information it has no use for.
    NotAvailable,
}

/// Decide whether `command` may proceed.
///
/// `available` says whether team functionality exists here at all -- see
/// [`teams_available`], which folds the entitlement in. `granted` is what this
/// DEVICE was allowed, and is the actual security decision.
///
/// `granted` must never be the capability list the client sent in its own
/// subscribe frame: that is what the client ASKED for, and checking against it
/// authorizes anything a caller cares to claim.
///
/// Availability is answered before the grant so that a host with nothing to
/// offer replies the same way to everyone. Otherwise the difference between the
/// two refusals tells an ungranted caller which hosts are worth attacking.
pub fn authorize(
    command: CoordinatorCommand,
    granted: &[String],
    available: bool,
) -> Result<(), RefusedReason> {
    if !available {
        return Err(RefusedReason::NotAvailable);
    }
    let required = command.required_capability();
    if granted.iter().any(|held| held == required) {
        Ok(())
    } else {
        Err(RefusedReason::MissingCapability { required })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device() -> Vec<String> {
        vec![COORDINATOR_CAPABILITY.to_owned()]
    }

    fn plan() -> Vec<String> {
        vec![TEAMS_ENTITLEMENT.to_owned()]
    }

    #[test]
    fn every_wire_name_round_trips() {
        for command in [
            CoordinatorCommand::TeammateList,
            CoordinatorCommand::SecretPendingList,
            CoordinatorCommand::SecretFulfil,
            CoordinatorCommand::SecretDeny,
            CoordinatorCommand::VmList,
        ] {
            assert_eq!(
                CoordinatorCommand::from_wire(command.as_wire()),
                Some(command),
                "{command:?} did not round-trip"
            );
        }
    }

    #[test]
    fn an_unrelated_command_is_not_claimed() {
        // agent_launch is handled by the existing path; claiming it here would
        // silently reroute it.
        assert_eq!(CoordinatorCommand::from_wire("agent_launch"), None);
        assert_eq!(CoordinatorCommand::from_wire(""), None);
        assert_eq!(CoordinatorCommand::from_wire("teammate_list "), None);
    }

    #[test]
    fn listing_commands_are_reads_and_mutations_are_writes() {
        assert_eq!(CoordinatorCommand::TeammateList.effect(), Effect::Read);
        assert_eq!(CoordinatorCommand::SecretPendingList.effect(), Effect::Read);
        assert_eq!(CoordinatorCommand::VmList.effect(), Effect::Read);
        assert_eq!(CoordinatorCommand::SecretFulfil.effect(), Effect::Write);
        assert_eq!(CoordinatorCommand::SecretDeny.effect(), Effect::Write);
    }

    #[test]
    fn only_the_fulfil_command_carries_a_secret() {
        // Anything logging a frame consults this. Getting it wrong in the
        // permissive direction writes a credential to disk.
        for command in [
            CoordinatorCommand::TeammateList,
            CoordinatorCommand::SecretPendingList,
            CoordinatorCommand::SecretDeny,
            CoordinatorCommand::VmList,
        ] {
            assert!(!command.carries_secret(), "{command:?} should not");
        }
        assert!(CoordinatorCommand::SecretFulfil.carries_secret());
    }

    // --- availability: the entitlement toggle, not a refusal ---------------

    #[test]
    fn teams_need_both_a_coordinator_and_the_entitlement() {
        assert!(teams_available(true, &plan()));
        assert!(!teams_available(false, &plan()), "no coordinator installed");
        assert!(!teams_available(true, &[]), "entitlement not held");
        assert!(!teams_available(false, &[]));
    }

    #[test]
    fn an_unentitled_account_sees_absence_not_denial() {
        // The correction that matters: an entitlement turns functionality OFF,
        // it does not deny access to functionality that is present. Answering
        // "permission denied" would make a billing state look like a trust
        // decision.
        let available = teams_available(true, &["memory".to_owned()]);
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &device(), available),
            Err(RefusedReason::NotAvailable)
        );
    }

    #[test]
    fn an_unentitled_account_and_a_coordinatorless_host_are_indistinguishable() {
        // One answer on purpose. Both mean "nothing here" from the caller's
        // side, and the difference is of no use to anyone but an attacker
        // mapping which hosts run a coordinator.
        let unentitled = teams_available(true, &[]);
        let no_coordinator = teams_available(false, &plan());
        assert_eq!(
            authorize(CoordinatorCommand::VmList, &device(), unentitled),
            authorize(CoordinatorCommand::VmList, &device(), no_coordinator)
        );
    }

    // --- authorization: the actual trust boundary --------------------------

    #[test]
    fn a_granted_device_on_an_available_host_is_authorized() {
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &device(), true),
            Ok(())
        );
    }

    #[test]
    fn agent_control_alone_does_not_authorize_coordinator_commands() {
        // Why this is its own capability. A peer allowed to launch an agent
        // must not thereby be able to answer a credential prompt.
        let granted = vec!["agent_control".to_owned(), "memory_read".to_owned()];
        assert_eq!(
            authorize(CoordinatorCommand::SecretFulfil, &granted, true),
            Err(RefusedReason::MissingCapability {
                required: COORDINATOR_CAPABILITY
            })
        );
    }

    #[test]
    fn an_ungranted_device_is_refused_even_on_an_entitled_account() {
        // Buying the tier is not the same as trusting the phone.
        assert!(authorize(CoordinatorCommand::SecretFulfil, &[], true).is_err());
    }

    #[test]
    fn nothing_is_authorized_without_a_grant() {
        for command in [
            CoordinatorCommand::TeammateList,
            CoordinatorCommand::SecretFulfil,
            CoordinatorCommand::VmList,
        ] {
            assert!(
                authorize(command, &[], true).is_err(),
                "{command:?} was allowed with no grant"
            );
        }
    }

    #[test]
    fn availability_is_answered_before_the_grant() {
        // A host with nothing to offer replies the same way to everyone, so the
        // difference between refusals cannot be used to find hosts worth
        // attacking.
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &[], false),
            Err(RefusedReason::NotAvailable)
        );
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &device(), false),
            Err(RefusedReason::NotAvailable)
        );
    }

    #[test]
    fn the_capability_name_matches_the_wire_string_the_app_sends() {
        // ferrosa-mobile serialises ControlCapability::CoordinatorControl as
        // "coordinator_control". A mismatch refuses every correctly granted
        // peer.
        assert_eq!(COORDINATOR_CAPABILITY, "coordinator_control");
    }
}
