//! Slot configuration, occupant compatibility, and positional port planning.

use std::path::PathBuf;

use metor_fsw_2_core::{
    PortConn, PortDesc, PortId, SequenceStatus, SlotControlIn, SystemDescriptor, SystemKind,
    compatible,
};
use metor_proto_wkt::{SequenceChannelEvent, SequenceCommand};

use super::SlotStatus;
use crate::dl::DlSystem;

/// Where an [`AllowedOccupant`]'s code lives and how `Load` reaches it.
pub enum OccupantBacking {
    /// An opened library in this process; `Load` runs `fsw_pack_create` over the
    /// handle, which stays loaded across swaps.
    Dl(Box<DlSystem>),
    /// A WebAssembly module driven under an interpreter in this process, its
    /// ports joined to the slot's rings by a copy (`crate::wasm`). Bounded by
    /// fuel and sandboxed from the host, which is what neither other backing
    /// offers.
    ///
    /// `entry_identity` is the resolved manifest entry's ABI/schema identity.
    /// `Load` re-reads `path`, finds the entry by occupant name, and accepts
    /// changed module bytes only while that identity remains compatible.
    Wasm {
        path: PathBuf,
        entry_identity: Vec<u8>,
    },
    /// A built cdylib the occupant's **worker process** opens; the host
    /// keeps only the path and never loads the artifact itself. Slots never
    /// mix backings: `plan_slot` requires the whole allowed set on one side
    /// of the seam.
    Artifact(PathBuf),
}

/// Default instruction budget for one guest poll. Fuel bounds guest work;
/// elapsed time also depends on the workload and host.
pub const DEFAULT_FUEL_PER_CALL: u64 = 100_000_000;
/// Generous fixed budget for module setup; the target-configured budget applies
/// only after the occupant has bound successfully.
pub const WASM_SETUP_FUEL: u64 = 100_000_000;

/// A candidate occupant a slot is allowed to load, validated at build time.
/// `Load` selects it by `name`; `descriptor` is the self-description the
/// slot's contract was derived from and validated against (a dl open or a
/// describe worker sourced it, per the backing), and `params` is the
/// postcard blob `fsw_pack_create` decodes.
pub struct AllowedOccupant {
    pub name: String,
    pub params: Vec<u8>,
    pub descriptor: SystemDescriptor,
    pub backing: OccupantBacking,
}

impl AllowedOccupant {
    /// An in-process occupant over an opened library.
    pub fn dl(name: impl Into<String>, system: DlSystem, params: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            params,
            descriptor: system.descriptor().clone(),
            backing: OccupantBacking::Dl(Box::new(system)),
        }
    }
}

/// Names an [`AllowedOccupant`] to load at startup, so a slot comes up
/// populated instead of empty. Slot init applies it once, starting
/// the occupant too when `start` is set.
#[derive(Clone, Debug)]
pub struct InitialOccupant {
    /// The allowed-set occupant name to load at startup.
    pub occupant: String,
    /// Whether to also start it, so it is running from the first cycle.
    pub start: bool,
}

/// A slot registration `plan_slot` rejected. The wiring front-end maps these
/// onto its own `LoadError`s; a target author's typo is not a library bug, so
/// none of these panic.
#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum SlotConfigError {
    #[error("a slot needs at least one allowed occupant")]
    Empty,
    #[error("initial occupant `{occupant}` is not in the allowed set (allowed: {allowed})")]
    UnknownInitial {
        occupant: String,
        /// The comma-joined allowed occupant names, for the message.
        allowed: String,
    },
    #[error(
        "a slot's occupants must share one backing: all in-process or all process-mode \
         (`Load` may not silently change the slot's fault domain)"
    )]
    MixedBacking,
    #[error(
        "allowed occupant `{occupant}` is incompatible with the slot contract \
         (derived from `{base}`)"
    )]
    OccupantMismatch { occupant: String, base: String },
    #[error(
        "occupant `{occupant}` declares `{port}` itself; the mount appends it \
         (a pre-pack artifact must be rebuilt)"
    )]
    ReservedPort {
        occupant: String,
        port: &'static str,
    },
    #[error(
        "allowed occupant `{occupant}` declares a capability; capability \
         systems are wired for the whole run, never slot occupants"
    )]
    CapabilityOccupant { occupant: String },
}

/// The pure-spec half of slot validation: a non-empty allowed set, and an
/// `initial` name inside it. The wiring gate runs it before any occupant
/// artifact is opened (a typo must fail without a dlopen); the descriptor-level
/// checks live in [`plan_slot`], next to the descriptors they need.
pub(crate) fn validate_slot_spec(
    allowed: &[&str],
    initial: Option<&str>,
) -> Result<(), SlotConfigError> {
    if allowed.is_empty() {
        return Err(SlotConfigError::Empty);
    }
    if let Some(init) = initial
        && !allowed.contains(&init)
    {
        return Err(SlotConfigError::UnknownInitial {
            occupant: init.to_string(),
            allowed: allowed.join(", "),
        });
    }
    Ok(())
}

/// Derive a slot's registered contract from its occupant set, the one place the
/// descriptor-level checks run: backing homogeneity (per-slot means
/// all-occupants, so a mixed set is
/// [`MixedBacking`](SlotConfigError::MixedBacking)), mutual occupant
/// compatibility against the first occupant's contract, the [`SlotPorts`] plan,
/// and the flattened registered [`SystemDescriptor`]. The returned `bool` is
/// whether the slot runs process-mode (an all-`Artifact` allow set).
///
/// The pure-spec checks ([`validate_slot_spec`]: a non-empty allow set, a valid
/// `initial`) are the caller's gate, so `allowed` is trusted non-empty here.
pub(crate) fn plan_slot(
    name: &str,
    allowed: &[AllowedOccupant],
) -> Result<(SystemDescriptor, SlotPorts, bool), SlotConfigError> {
    let backing = core::mem::discriminant(&allowed[0].backing);
    if allowed
        .iter()
        .any(|a| core::mem::discriminant(&a.backing) != backing)
    {
        return Err(SlotConfigError::MixedBacking);
    }
    if let Some(occ) = allowed
        .iter()
        .find(|a| !a.descriptor.capabilities.is_empty())
    {
        return Err(SlotConfigError::CapabilityOccupant {
            occupant: occ.name.clone(),
        });
    }
    let process = matches!(&allowed[0].backing, OccupantBacking::Artifact(_));
    // Every allowed occupant must share the contract; the slot sizes and
    // validates to the first occupant's descriptor (mutual subset).
    let base = &allowed[0].descriptor;
    for occ in &allowed[1..] {
        let d = &occ.descriptor;
        let ports_match = |a: &[PortDesc], b: &[PortDesc]| {
            a.len() == b.len()
                && a.iter()
                    .zip(b)
                    .all(|(x, y)| compatible(x, y) && compatible(y, x))
        };
        if !(ports_match(&d.inputs, &base.inputs) && ports_match(&d.outputs, &base.outputs)) {
            return Err(SlotConfigError::OccupantMismatch {
                occupant: occ.name.clone(),
                base: allowed[0].name.clone(),
            });
        }
    }
    let ports = SlotPorts::for_occupant(base, &allowed[0].name)?;
    // The registered descriptor name is the slot's instance name (a leaked
    // `&'static str` for the descriptor field and the `SlotRunner` identity).
    let leaked: &'static str = Box::leak(name.to_string().into_boxed_str());
    let registered = ports.registered(leaked);
    Ok((registered, ports, process))
}

/// A slot's registered ports as the three concepts they are, not the flat
/// positional lists the build pass consumes:
///
/// - the occupant's **user ports**, in its descriptor's order;
/// - the **mount-appended occupant tail**: the [`SlotControlIn`] cancel input
///   and the [`SequenceStatus`] output (a mount property, not descriptor
///   content, so any entry can occupy a slot);
/// - the **runner tail**, which never crosses to the occupant.
///
/// The first two flatten into the occupant lists here because both cross to
/// the occupant, positionally, as the prefix of each registered list.
/// [`registered`](Self::registered) flattens prefix then tail into the
/// [`SystemDescriptor`] the build pass sees; edge resolution, registry keys,
/// and the occupant-side positional bind ABI all consume that flattening, so
/// its exact sequence is a compatibility surface (the
/// `registered_slot_descriptor_snapshot` test pins it).
pub(crate) struct SlotPorts {
    /// The occupant's ABI prefix: its user inputs plus the mount-appended,
    /// host-connected [`SlotControlIn`] (the runner holds the cancel writer).
    pub occupant_inputs: Vec<PortDesc>,
    /// The occupant's ABI prefix: its user outputs plus the mount-appended
    /// [`SequenceStatus`].
    pub occupant_outputs: Vec<PortDesc>,
    /// The runner's inputs: the `commands` fan-in (an ordinary edge input, so
    /// command wiring is ordinary message wiring) and the self-tap over the
    /// occupant's [`SequenceStatus`] output.
    pub tail_inputs: Vec<PortDesc>,
    /// The runner's outputs: the [`SlotStatus`] frame and the `"sequences"`
    /// events channel, both host-written and registry-tapped.
    pub tail_outputs: Vec<PortDesc>,
}

impl SlotPorts {
    /// Derive the slot's port plan from an occupant contract by extension,
    /// not surgery: the mount tail is appended to the occupant's own ports,
    /// then the runner tail is laid out after the prefix. Rejects an occupant
    /// that declares a mount-appended port itself (a pre-pack artifact).
    pub(crate) fn for_occupant(
        base: &SystemDescriptor,
        occupant: &str,
    ) -> Result<Self, SlotConfigError> {
        let control_id = PortId::Component(<SlotControlIn as crate::Frame>::FRAME_ID);
        let seq_status_id = PortId::Component(<SequenceStatus as crate::Frame>::FRAME_ID);
        let reserved = |port| SlotConfigError::ReservedPort {
            occupant: occupant.to_string(),
            port,
        };
        if base.inputs.iter().any(|p| p.id() == control_id) {
            return Err(reserved("SlotControlIn"));
        }
        if base.outputs.iter().any(|p| p.id() == seq_status_id) {
            return Err(reserved("SequenceStatus"));
        }
        let mut occupant_inputs = base.inputs.clone();
        occupant_inputs.push(PortDesc::of::<SlotControlIn>().with_conn(PortConn::Host));
        let mut occupant_outputs = base.outputs.clone();
        occupant_outputs.push(PortDesc::of::<SequenceStatus>());
        Ok(Self {
            occupant_inputs,
            occupant_outputs,
            tail_inputs: vec![
                PortDesc::msg::<SequenceCommand>(),
                PortDesc::of::<SequenceStatus>().with_conn(PortConn::SelfTap(seq_status_id)),
            ],
            tail_outputs: vec![
                PortDesc::of::<SlotStatus>().with_conn(PortConn::Host),
                PortDesc::msg_named::<SequenceChannelEvent>("sequences").with_conn(PortConn::Host),
            ],
        })
    }

    /// Flatten the plan into the registered descriptor: occupant prefix then
    /// runner tail, per direction. This sequence is load-bearing (see the
    /// type docs); extend the plan, never reorder it.
    pub(crate) fn registered(&self, name: &'static str) -> SystemDescriptor {
        SystemDescriptor {
            name: name.into(),
            kind: SystemKind::Cyclic,
            inputs: self
                .occupant_inputs
                .iter()
                .chain(&self.tail_inputs)
                .cloned()
                .collect(),
            outputs: self
                .occupant_outputs
                .iter()
                .chain(&self.tail_outputs)
                .cloned()
                .collect(),
            // Sequence occupants declare wired ports only (ReceiveAll is host-only).
            capabilities: Vec::new(),
        }
    }
}

/// A slot's configuration as recorded at registration, held until `build()`
/// assembles the [`SlotRunner`] from it.
pub(crate) struct SlotReg {
    pub allowed: Vec<AllowedOccupant>,
    pub initial: Option<InitialOccupant>,
    /// The named port plan the registered descriptor was flattened from; the
    /// bind arm reads the occupant/tail split and the tail-port indices off
    /// it instead of re-deriving them by shape.
    pub ports: SlotPorts,
    /// Run occupants out of process: the occupant prefix's crossing rings
    /// (its outputs, its Edge inputs' producers, and the host control ring)
    /// are allocated as session-dir files a worker process can attach. The
    /// runner tail stays host-side either way.
    pub process: bool,
}
