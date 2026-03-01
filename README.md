# psht

`psht` lets me deploy stuff easily. ymmv.

- `psht-server` runs on the host.
- `psht` runs on your local machine.

## What you get

- Deploy from your app directory with one command.
- Basic app lifecycle commands (`ps`, `logs`, `stop`, `destroy`).
- App environment management (`env`, `env:unset`) persisted across deploys/rebuilds.
- Host bootstrap, health checks, and upgrades.
- Per-app persistent storage mounted at `/storage` (survives deploys/rebuilds, removed by `destroy`).

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
psht deploy # first run creates psht.toml (release URL + start command)
```

Use `psht deploy --force` to force a deploy even when the binary payload hash is unchanged.

`psht.toml` (project root) is used for release deploys and optional deploy hooks:

```toml
url = "https://github.com/org/repo/releases/download/v1.2.3/my-app-linux-amd64.tar.gz"
start = "./my-app --port $PORT"
app = "my-app" # optional override for derived app name
bin = "my-app" # optional path inside archive when needed
preinstall = "echo preparing deploy" # optional, runs before dependency install
postinstall = "npm run migrate" # optional, runs after dependency install and before start
apt_packages = ["libvips", "ffmpeg"] # optional, apt packages installed in container on deploy
required_env = ["DATABASE_URL", "JWT_SECRET"] # optional, deploy/start fail if these vars are missing
```

Set project env vars:

```sh
psht env DATABASE_URL=postgres://... JWT_SECRET=...
psht env
psht env:unset JWT_SECRET
```
