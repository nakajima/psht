# psht

`psht` lets me deploy stuff easily. ymmv.

- `psht-server` runs on the host.
- `psht` runs on your local machine.

## What you get

- Deploy from your app directory with one command.
- Basic app lifecycle commands (`ps`, `logs`, `stop`, `destroy`).
- Host bootstrap, health checks, and upgrades.

## Quickstart

### 1) Prepare a host

On the server:

```sh
sudo psht-server bootstrap
psht-server doctor
```

### 2) Install the client locally

On your local machine:

```sh
ssh psht@<host> setup | sh
```

This installs the `psht` cli and writes `~/.psht/config.toml`, which tracks where your `psht-server` is and what projects you have.

### 3) Deploy your app

```sh
cd your-app
psht setup
psht deploy # tries to use git, if no git, scp
```

