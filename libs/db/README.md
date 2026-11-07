# `metor-db`

### Run the database

```sh
# Run metor-db in the foreground:
# - Listening on port 2240
# - Storing data in the default user data directory ($HOME/.local/share/metor/db)
metor-db run [::]:2240 $HOME/.local/share/metor/db
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

The example C client just streams a sine wave component to entity "1". You can view this in the editor by creating a graph for entity "1" and selecting the only component available for that entity.

### Generate C++ Header

metor-db ships with a single header C++20 library. The library includes message definitions for communicating with the DB.

> NOTE: Not all definitions have been added yet if you need something ASAP please contact us

You can generate the C++ library by running:

`cargo run gen-cpp > ./examples/db.hpp`

This will generate a C++ header file at `./examples/db.hpp`
