# `metor-db`

### Install

Install `metor-db` using the standalone installer script:

```sh
# Install the latest version
curl -LsSf https://storage.googleapis.com/metor-releases/install-db.sh | sh

# Install a specific version (e.g., 0.13.3)
curl -LsSf https://storage.googleapis.com/metor-releases/install-db.sh | sh -s v0.13.3
```

Alternatively, you can download the latest portable binary for your platform:

- [macOS (arm64)](https://storage.googleapis.com/metor-releases/latest/metor-db-aarch64-apple-darwin.tar.gz)
- [Linux (x86_64)](https://storage.googleapis.com/metor-releases/latest/metor-db-x86_64-unknown-linux-musl.tar.gz)
- [Linux (arm64)](https://storage.googleapis.com/metor-releases/latest/metor-db-aarch64-unknown-linux-musl.tar.gz)

### Run the database

```sh
# Run metor-db in the foreground:
# - Listening on port 2240
# - Storing data in the default user data directory ($HOME/.local/share/metor/db)
# - Using the ./examples/db-config.lua config
metor-db run [::]:2240 $HOME/.local/share/metor/db --config examples/db-config.lua
```

### Stream data to the database with C

See [./examples/client.c](./examples/client.c) for an example C client that streams fake sensor data to the database. Build and run the client:

```sh
cc examples/client.c -lm -o /tmp/client; /tmp/client
```


### Subscribe to data with C++

[./examples/client.cpp](./examples/client.cpp) includes an example of how to subscribe to data using C++. It can be built and run using:

This example uses C++23, but the library itself is C++20 compatible.

``` sh
c++ -std=c++23 examples/client.cpp -o /tmp/client-cpp; /tmp/client-cpp
```

### Connect to the database using the CLI

Launch a LUA REPL to interact with the database:
```sh
metor-db lua
```

Connect to the database and dump all of the metadata:
```
db ❯❯ client = connect("127.0.0.1:2240")
db ❯❯ client:dump_metadata()
```

Run `:help` in the REPL to see all available commands:
```
db ❯❯ :help
Impeller Lua REPL
- `connect(addr)`
   Connects to a new database and returns a client
- `Client:dump_metadata()`
   Dumps all metadata from the db
...
```

### Connect to the database using the Metor Editor

Install the [Metor Editor](https://docs.metor.systems/hello/quickstart/#install) if you haven't already. Then, launch the editor by providing the database IP and port:

```sh
metor editor 127.0.0.1:2240
```

The example C client just streams a sine wave component to entity "1". You can view this in the editor by creating a graph for entity "1" and selecting the only component available for that entity.

### Mirror data from one db instance to another

Launch a secondary db instance:

```sh
metor-db run [::]:2241 $HOME/.local/share/metor/ground-station
```

Run the `downlink.lua` script to sync metadata from the primary db instance to the secondary db instance *and* command the primary instance to start streaming data to the secondary instance:

```sh
FC_ADDR="127.0.0.1:2240" GROUND_STATION_ADDR="127.0.0.1:2241" metor-db lua examples/downlink.lua
```

Confirm that the secondary instance is receiving data by connecting to it via the Metor Editor:

```sh
metor editor 127.0.0.1:2241
```

### Generate C++ Header

metor-db ships with a single header C++20 library. The library includes message definitions for communicating with the DB.

> NOTE: Not all definitions have been added yet if you need something ASAP please contact us

You can generate the C++ library by running:

`cargo run gen-cpp > ./examples/db.hpp`

This will generate a C++ header file at `./examples/db.hpp`
