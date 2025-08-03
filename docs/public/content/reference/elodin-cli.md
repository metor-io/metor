+++
title = "Metor CLI"
description = "Metor CLI"
draft = false
weight = 104
sort_by = "weight"

[extra]
toc = true
top = false
icon = ""
order = 4
+++

# Command-Line Help for `metor`

This document contains the help content for the `metor` command-line program.

**Command Overview:**

* [`metor`↴](#metor)
* [`metor login`↴](#metor-login)
* [`metor editor`↴](#metor-editor)
* [`metor run`↴](#metor-run)
* [`metor create`↴](#metor-create)

## `metor`

**Usage:** `metor [OPTIONS] [COMMAND]`

###### **Subcommands:**

* `login` — Obtain access credentials for your user account
* `editor` — Launch the Metor editor (default)
* `run` — Run an Metor simulaton in headless mode
* `create` — Create template

###### **Options:**

* `-u`, `--url <URL>`

  Default value: `https://app.metor.systems`



## `metor login`

Obtain access credentials for your user account

**Usage:** `metor login`



## `metor editor`

Launch the Metor editor (default)

**Usage:** `metor editor [addr/path]`

###### **Arguments:**

* `<addr/path>`

  Default value: `127.0.0.1:2240`



## `metor run`

Run an Metor simulaton in headless mode

**Usage:** `metor run [addr/path]`

###### **Arguments:**

* `<addr/path>`

  Default value: `127.0.0.1:2240`



## `metor create`

Create template

**Usage:** `metor create [OPTIONS] --template <TEMPLATE>`

###### **Options:**

* `-t`, `--template <TEMPLATE>` — Name of the template

  Possible values: `rocket`, `drone`, `cube-sat`, `three-body`, `ball`

* `-p`, `--path <PATH>` — Path where the result will be located

  Default value: `.`



<hr/>

<small><i>
    This document was generated automatically by
    <a href="https://crates.io/crates/clap-markdown"><code>clap-markdown</code></a>.
</i></small>
