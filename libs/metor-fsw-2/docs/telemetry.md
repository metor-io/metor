# Telemetry link

The telemetry link serves telemetry and commands on one TCP socket. Ground tools connect to the flight software. The flight software does not dial a ground address.

Telemetry gives operators and ground tools a live view of the mission. The
uplink gives them a path to send commands back. Together they make the running
software observable and controllable without adding network code to each
system.

## Mission setup

This Python config serves all telemetry and accepts two command types:

```python
from metor_config import Downlink, Mission, TcpServer, Uplink

m = Mission(cycle_rate=100.0)

m.state(
    "link",
    TcpServer(addr="0.0.0.0:2240", name="sat-a"),
)

uplink = m.add(
    "uplink",
    Uplink(msgs=["SequenceCommand", "AlarmAck"]),
)

downlink = m.add("downlink", Downlink())
```

The address belongs to the server state, not to either system. Port `0` is valid. The server advertises the port that the OS chose.

`Downlink()` selects every output marked for telemetry. A subset can match an instance name or an output name:

```python
downlink = m.add(
    "downlink",
    Downlink(
        instances=["nav", "controller"],
        frames=["health", "AlarmRaised"],
    ),
)
```

The two lists use OR rules. An entry matches if either list contains its name. The `frames` list also matches message channel names.

The uplink creates one message output for each name in `msgs`. A route must connect that output to each user:

```python
m.route(uplink, mode_slot, msg="SequenceCommand")
m.route(uplink, alarms, msg="AlarmAck")
```

Uplink outputs do not enter the downlink. This rule stops a command from going back to ground as telemetry.

Three parts share the work:

- A `TcpServer` state owns the listener and all live links.
- A `Downlink` system reads selected output rings and sends their records.
- An `Uplink` system reads command packets and writes them to message output ports.

The two systems attach to the same `TcpServer` state. Socket work runs outside
the control cycle. The systems only move bytes between rings and bounded
queues.

## Cycle order

The uplink should run before its users. A command received between cycles can then reach its user in the next cycle.

The downlink has the `ReceiveAll` capability. The config loader places all static `ReceiveAll` systems after normal cyclic systems. This group can include the alarm system as well as the downlink.

The build step rejects a normal cyclic system placed after the first `ReceiveAll` system. This check makes each broad reader see records from the same cycle.

The downlink claims its read views in `init`. A view starts at the current write point. It cannot see a record that another system wrote only in `init`.

Publish one-time values in the first `execute`, after the downlink has claimed its view. Mark a one-time message as a snapshot when new TCP clients must receive its latest value. The link retains that value after the downlink reads it.

## Downlink records

Each output has two separate traits: schema and delivery.

The schema sets the packet form:

- A table output sends a `Table` packet. Its payload is the record from the ring.
- A message output sends a `Msg` packet. Its record starts with the message id, followed by postcard data.

The delivery setting controls how the downlink reads the ring:

- `Snapshot` sends the newest record after a new commit.
- `Log` sends every pending record in commit order.

For example, a state frame often uses table schema with snapshot delivery. An alarm event uses message schema with log delivery. The downlink handles both through the same tap code.

Table bytes need no data conversion. The downlink gives each table tap a packet id, then sends its vtable and component metadata before table data.

The link also sends `SetMsgMetadata` for each known message id. It sends each message schema once, even when many systems have an output with that id.

## Connection start

Each new TCP client receives this data in order:

1. `LinkInfo`, as the first packet.
2. Table vtables and component metadata.
3. Message metadata.
4. The latest retained snapshot messages.
5. Live cycle batches.

`LinkInfo` contains the link protocol version, feature bits, and the message ids that the uplink accepts. The current packet does not contain the mDNS node name.

The link retains snapshot messages because some of them report boot state once. It does not retain table snapshots. Systems tend to publish table state each cycle, so a new client soon gets a fresh value.

## Command input

All client readers feed one inbound FIFO. The FIFO keeps arrival order across clients.

The reader accepts `Msg` packets as commands. It ignores `Table`, the old `MsgStream` request, and node or link control messages. The uplink then matches the message id against its configured output list.

The uplink copies the payload without decoding it. Each receiving `MsgIn` decodes its own type and skips a payload that does not decode.

An id outside the uplink list records `uplink_unroutable`. A full output ring records `uplink_dropped`. In both cases the uplink continues to drain its input.

## Bounded loss

Network delay must not delay the control cycle.

Each client has at most 1 MiB of pending output. If a new cycle batch would cross that limit, that client misses the whole batch. Other clients still receive it, and the link keeps the slow client open.

The inbound command FIFO holds 256 messages. A new message is lost when the FIFO is full.

The downlink reports these cases through health:

- `link_conn_dropped` for a lost client batch
- `link_inbound_dropped` for a full command FIFO
- `link_disconnect` when a client link ends
- `telemetry_reader_slot` when a tap cannot claim a ring view
- `telemetry_input_corrupt` when a tap cannot read its ring

The `link_status` frame reports live links, accepted links, and lost output batches. It publishes when one of those values changes.

The downlink drains every tap even when no client is present. This keeps its ring reader current. Live records have no network user in that case, apart from retained snapshot messages.

## Local discovery

The current server advertises non-loopback binds with mDNS and DNS-SD. It uses this service type:

```text
_metor-fsw._tcp.local.
```

The service instance uses `TcpServer.name`. If the config omits the name, it uses the OS host name.

The TXT record includes the link protocol version as `pv` and the role `fsw`. A wildcard bind lets the mDNS service list the host network addresses. A fixed non-loopback bind advertises that address.

The server does not advertise a loopback bind. Local development with `127.0.0.1` still needs a direct address.

mDNS works on the local multicast link. Routers do not carry it by default. It does not prove that a found service is the expected flight software.

The TCP link has no auth or encryption. Any host that can reach the port can read telemetry and send accepted command ids. Use discovery and this link only on a trusted network until the link gains peer auth.

Stopping the server shuts down the mDNS service and closes all TCP tasks.
