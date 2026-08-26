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
/// Team features are a paid tier, so a peer needs BOTH: an account whose plan
/// includes teams, and the capability granted to this particular device. They
/// answer different questions -- what was bought, and what this device may do
/// with it -- and neither implies the other. A shared account on an untrusted
/// device should not reach the coordinator because the plan allows it, and a
/// fully trusted device should not reach it on a plan that never included it.
pub const TEAMS_ENTITLEMENT: &str = "teams";

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
    /// The account's plan does not include teams.
    ///
    /// An upgrade prompt, not an error. Rendering this as a permission failure
    /// tells a paying customer they did something wrong when the honest answer
    /// is that the feature is on a tier they have not bought.
    NotInPlan {
        /// The entitlement that would allow it.
        required: &'static str,
    },
    /// The plan allows it but this device was not granted the capability.
    MissingCapability {
        /// What it needed.
        required: &'static str,
    },
    /// This host has no coordinator configured.
    ///
    /// Distinct from both refusals: nothing was denied, the machine simply does
    /// not offer it. Collapsing them would tell an operator they lack
    /// permission, or need to upgrade, on a host that has no coordinator at all.
    NotAvailable,
}

/// Decide whether `command` may proceed.
///
/// `entitlements` come from the account's plan grant and `granted` from what
/// this device was allowed. Both are the SERVER's view. Neither may ever be the
/// capability list the client sent in its own subscribe frame: that is what the
/// client ASKED for, and checking against it authorizes anything a caller cares
/// to claim.
///
/// Order matters. Availability is answered first so a host without a
/// coordinator replies identically whether or not the caller was entitled --
/// otherwise the difference between the two answers tells an unauthorized
/// caller which hosts run one. Plan comes before capability so a customer on
/// the wrong tier is told to upgrade rather than told their device is
/// untrusted, which would be true but useless.
pub fn authorize(
    command: CoordinatorCommand,
    entitlements: &[String],
    granted: &[String],
    coordinator_available: bool,
) -> Result<(), RefusedReason> {
    if !coordinator_available {
        return Err(RefusedReason::NotAvailable);
    }
    if !entitlements.iter().any(|held| held == TEAMS_ENTITLEMENT) {
        return Err(RefusedReason::NotInPlan {
            required: TEAMS_ENTITLEMENT,
        });
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

    fn plan() -> Vec<String> {
        vec![TEAMS_ENTITLEMENT.to_owned()]
    }

    fn device() -> Vec<String> {
        vec![COORDINATOR_CAPABILITY.to_owned()]
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

    #[test]
    fn a_peer_on_the_teams_plan_with_the_capability_is_authorized() {
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &plan(), &device(), true),
            Ok(())
        );
    }

    #[test]
    fn a_plan_without_teams_is_an_upgrade_prompt_not_a_permission_error() {
        // The distinction the tiering exists for. Telling a customer on a lower
        // plan that permission was denied sends them to check device trust when
        // the honest answer is that they have not bought the feature.
        let lower = vec!["memory".to_owned()];
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &lower, &device(), true),
            Err(RefusedReason::NotInPlan {
                required: TEAMS_ENTITLEMENT
            })
        );
    }

    #[test]
    fn the_teams_plan_alone_does_not_authorize_an_ungranted_device() {
        // Buying the tier is not the same as trusting the phone. A shared
        // account on a device that was never granted coordinator control must
        // not reach it because the plan allows it.
        assert_eq!(
            authorize(CoordinatorCommand::SecretFulfil, &plan(), &[], true),
            Err(RefusedReason::MissingCapability {
                required: COORDINATOR_CAPABILITY
            })
        );
    }

    #[test]
    fn agent_control_alone_does_not_authorize_coordinator_commands() {
        // The reason this is its own capability. A peer allowed to launch an
        // agent must not thereby be able to answer a credential prompt.
        let granted = vec!["agent_control".to_owned(), "memory_read".to_owned()];
        assert_eq!(
            authorize(CoordinatorCommand::SecretFulfil, &plan(), &granted, true),
            Err(RefusedReason::MissingCapability {
                required: COORDINATOR_CAPABILITY
            })
        );
    }

    #[test]
    fn nothing_is_authorized_with_neither_plan_nor_grant() {
        for command in [
            CoordinatorCommand::TeammateList,
            CoordinatorCommand::SecretFulfil,
            CoordinatorCommand::VmList,
        ] {
            assert!(
                authorize(command, &[], &[], true).is_err(),
                "{command:?} was allowed with no plan and no grant"
            );
        }
    }

    #[test]
    fn a_host_without_a_coordinator_says_so_rather_than_refusing() {
        // Not a permission problem and not a billing one. Telling an operator
        // to upgrade, on a machine that has no coordinator at all, sends them
        // to spend money that would change nothing.
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &plan(), &device(), false),
            Err(RefusedReason::NotAvailable)
        );
    }

    #[test]
    fn availability_is_answered_before_plan_or_grant() {
        // A host with no coordinator answers identically however entitled the
        // caller is, so the difference between answers cannot be used to
        // discover which hosts run one.
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &[], &[], false),
            Err(RefusedReason::NotAvailable)
        );
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &plan(), &device(), false),
            Err(RefusedReason::NotAvailable)
        );
    }

    #[test]
    fn plan_is_answered_before_device_grant() {
        // Both are missing here. The customer needs to know about the tier
        // first; "your device is untrusted" would be true and useless.
        assert_eq!(
            authorize(CoordinatorCommand::TeammateList, &[], &[], true),
            Err(RefusedReason::NotInPlan {
                required: TEAMS_ENTITLEMENT
            })
        );
    }

    #[test]
    fn the_capability_name_matches_the_wire_string_the_app_sends() {
        // ferrosa-mobile serialises ControlCapability::CoordinatorControl as
        // "coordinator_control". A mismatch here refuses every request from a
        // correctly granted peer.
        assert_eq!(COORDINATOR_CAPABILITY, "coordinator_control");
    }
}
