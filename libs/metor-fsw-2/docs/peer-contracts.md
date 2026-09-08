# Peer endpoint contracts

The first peer telemetry increment implements offline endpoint ICDs in
`metor_fsw_2_core::peer` (also re-exported by `metor_fsw_2`). Contracts can be
generated and compared now. Target bindings, sockets, and cyclic import/export
are subsequent increments in the [implementation plan](peer-telemetry-plan.md).

## Declare and export

Define the frame types in a shared contract crate using the existing `Frame`
derive. The derive now also exports fixed layout, timestamp offset, padding,
and direct `FrameList`/`FrameMap` bounds.

```rust,ignore
use metor_fsw_2_core::peer::{EndpointContract, FrameChannel};
use metor_fsw_2_core::Delivery;

let mut imu = FrameChannel::of::<Imu>("imu_a", Delivery::Snapshot)?;
imu.semantics.insert("omega.units".into(), "rad/s".into());
imu.semantics.insert("omega.coordinate_frame".into(), "body".into());

let endpoint = EndpointContract::new(
    "plant_sensors", "1.0", "simulation", vec![imu],
)?;
std::fs::write("plant-sensors.json", endpoint.to_json()?)?;
println!("{}", endpoint.hash()?);
```

The channel key belongs to the endpoint; it does not rename the frame type.
Several channels may carry `Imu` independently. Channel keys begin with an ASCII
letter and contain letters, digits, and underscores. Endpoint manifests contain
no addresses, graph instance names, or namespace prefixes.

Run the [complete example](../core/examples/peer_contract.rs) to export a contract
containing two IMU channels:

```sh
cargo run -p metor-fsw-2-core --example peer_contract > plant-sensors.json
```

The JSON goes to stdout and its hash to stderr. The manifest can be shipped as
a plain file without shipping or running the producing system.

## Validate and compare

```rust,ignore
let expected = EndpointContract::from_json(&std::fs::read_to_string("plant-sensors.json")?)?;
expected.channels[0].require_frame::<Imu>()?;
expected.require_exact(&received_contract)?;
```

`require_frame` checks a local Rust frame's exact schema and native layout.
Delivery and semantic bindings still need target-level checks. `require_exact`
compares whole endpoint contracts and reports the differing property.
This deliberately rejects the component-subset compatibility allowed on ordinary
local edges. Equal names or equal record sizes do not prove compatibility.

The version-1 hash is SHA-256 over compact UTF-8 JSON with recursively sorted
object keys, channels sorted by key, and component metadata sorted by ID/name.
Endpoint/channel `description` fields are excluded. All semantic and component
metadata entries, vtable arrays, bounds, layouts, byte order, clock domain, and
ICD name/version participate. `format_version` versions these representation
rules. Unknown versions, duplicate channel keys, inconsistent layouts, and
manifests larger than 1 MiB are rejected by the import/export API.

## Current limits

This increment supports frame channels with snapshot or log delivery. Direct
bounded lists/maps are supported when their fixed element values have complete
vtables. Recursive dynamic bounds and element layouts with undescribed bytes
(including element timestamps suppressed by today's vtables) are rejected.
Skipped `u8` arrays may serve as explicit padding; hidden typed values cannot
silently disappear from an ICD. Hand-written `Frame` implementations must supply
`peer_layout` to opt in; existing local uses do not require it.

These checks describe and compare contracts. They do not validate received record
bytes, synchronize clocks, or make unlike native layouts interoperable. The later
transport will validate incoming data against its locally pinned contract before
publishing records into graph rings. Channel-to-port aliases, Python target and
bundle integration, and typed freshness outputs are still pending.
