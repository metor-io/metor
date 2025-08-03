+++
title = "Overview"
description = "Overview of Metor DB"
draft = false
weight = 104
sort_by = "weight"

[extra]
lead = "Overview of Metor DB"
toc = true
top = false
order = 7
icon = ""
+++

Metor DB is a time-series database purpose built for flight computers. It is designed as a central telemetry store and message bus. Flight software publish and subscribes to telemetry data and messages from Metor DB.

Metor DB is based on the same ECS system as Metor Sim (and the rest of the Metor ecosystem). Metor DB sorts data into "entities" and "components". Entities are best thought as objects that emit telemetry. For instance an individual sensor would be an entity. A component is a piece of telemetry data that is associated with an entity. For example an accelerator's acceleration reading would be a component. Entities can have multiple components, and each component can be associated with multiple entities.

Metor DB chiefly communicates over `impeller` -- Metor's lightweight message protocol designed for flight software. In addition to `impeller`, Metor DB can be queries via SQL and results are returned in the Arrow IPC format.

## Quick Start

You can install Metor DB using the following command:

```sh
curl -LsSf https://storage.googleapis.com/metor-releases/install-db.sh | sh
```

Alternatively, you can download the latest portable binary for your platform:

- [macOS (arm64)](https://storage.googleapis.com/metor-releases/latest/metor-db-aarch64-apple-darwin.tar.gz)
- [Linux (x86_64)](https://storage.googleapis.com/metor-releases/latest/metor-db-x86_64-unknown-linux-musl.tar.gz)
- [Linux (arm64)](https://storage.googleapis.com/metor-releases/latest/metor-db-aarch64-unknown-linux-musl.tar.gz)

To start a new instance of Metor DB, use the following command:
```sh
metor-db run
```

### Lua REPL

The easiest way to interact with Metor DB is through its Lua REPL. To start the REPL, run the following command:

```sh
metor-db lua
```

You can connect to the database by running:

```lua
client = connect("localhost:2240")
```


### SQL

You can query Metor DB via SQL. The easiest way to access this interface is through the REPL where you can run the following command to connect to the database using SQL

```lua
:sql [::]:2240
```

This will connect to your local instance of Metor DB, and drop you into a SQL REPL.

To list the available tables, run:

```sql
show tables;
```

We use Datafusion to power the SQL interface. [Their docs](https://datafusion.apache.org/user-guide/sql/index.html) are the best place for details on the dialect.
