+++
title = "Quick Start"
description = "Install Metor and start simulating."
draft = false
weight = 101
sort_by = "weight"

[extra]
lead = "Install Metor and start simulating."
toc = true
top = false
order = 1
icon = ""
+++

## Install

Download the Metor Client:

| File                                                    | Platform            | Checksum                        |
| ------------------------------------------------------- | ------------------- | ------------------------------- |
| [metor-aarch64-apple-darwin.tar.gz][metor-macos]      | Apple Silicon macOS | [sha256][metor-macos-sha256]   |
| [metor-x86_64-unknown-linux-gnu.tar.gz][metor-linux]  | x64 Linux           | [sha256][metor-linux-sha256]   |
| [metor-x86_64-pc-windows-msvc.zip][metor-windows]     | x64 Windows         | [sha256][metor-windows-sha256] |

[metor-macos]: https://storage.googleapis.com/metor-releases/latest/metor-aarch64-apple-darwin.tar.gz
[metor-macos-sha256]: https://storage.googleapis.com/metor-releases/latest/metor-aarch64-apple-darwin.tar.gz.sha256
[metor-linux]: https://storage.googleapis.com/metor-releases/latest/metor-x86_64-unknown-linux-gnu.tar.gz
[metor-linux-sha256]: https://storage.googleapis.com/metor-releases/latest/metor-x86_64-unknown-linux-gnu.tar.gz.sha256
[metor-windows]: https://storage.googleapis.com/metor-releases/latest/metor-x86_64-pc-windows-msvc.zip
[metor-windows-sha256]: https://storage.googleapis.com/metor-releases/latest/metor-x86_64-pc-windows-msvc.zip.sha256

Install the Metor Python SDK using `pip`:

{% alert(kind="warning") %}
The SDK is only supported on macOS and Linux distributions with glibc 2.35+ (Ubuntu 22.04+, Debian 12+, Fedora 35+, NixOS 21.11+). Windows users can still use Metor by installing and running the simulation server in Windows Subsystem for Linux. Install the Metor Python SDK in WSL, after [installing WSL.](https://docs.microsoft.com/en-us/windows/wsl/install)
{% end %}


```sh
pip install -U metor
```

## Start Simulating

### Windows (WSL)

To use Metor on Windows, the simulation server must run in Windows Subsystem for Linux (WSL). The Metor Client itself can run natively on Windows.

[Video Walkthrough](https://www.loom.com/share/efcbf81e43074863807750d4ad2f8d7a?sid=9403e8c8-7893-4299-824e-2dacb6978120)

In a Windows terminal launch the Metor app.

```wsl
.\metor.exe
```

In a WSL terminal download and install `metor` binary into your path then run:

1. Create a new simulation using the three-body orbit template.
    ```sh
    metor create --template three-body
    ```
2. Run the simulation server.
    ```sh
    metor run three-body.py
    ```

### Linux / macOS

1. Create a new simulation using the three-body orbit template.
    ```sh
    metor create --template three-body
    ```
2. Launch the simulation using the `metor` CLI.
    ```sh
    metor editor three-body.py
    ```

## Perform Analysis

To analyze simulation data, use the `Exec` API to run the simulation for some number of ticks and collect the historical component data as a [Polars DataFrame].
The DataFrame can then be used to generate plots or perform other methods of data analysis.

Run the bouncing ball example code to see this in action:

The `ball/plot.py` example depends on `matplotlib`. Install it using `pip`:

```sh
pip install -U matplotlib
```

Then create & run the ball template:
```sh
metor create --template ball
python3 ball/plot.py
```

For more information on data frames check out
[Polars DataFrame](https://docs.pola.rs/user-guide/concepts/data-structures/#dataframe)

## Next Steps

Try out the following tutorials to learn how to build simulations using Metor:

{% cardlink(title="Three-Body Orbit Tutorial", icon="planet", href="/home/3-body") %}
Learn how to model a basic stable three-body problem
{% end %}
