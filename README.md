# psht

`psht` lets me deploy stuff easily. ymmv.

- `psht-server` runs on the host.
- `psht` runs on your local machine.

## setup

### on ur server

```sh
$ curl -fsSL https://github.com/nakajima/psht/releases/latest/download/install.sh | sh
```

or grab one of the releases at https://github.com/nakajima/psht/releases.

Then:

```sh
$ sudo psht-server bootstrap
```

This will setup incus, a `psht` user and git hooks. You'll need a tailscale key so that apps can automatically be added to your tailnet.
For Tailscale OAuth, configure these scopes:

- `Keys: Write` with `tag:psht`
- `Devices: Core Read`
- `Devices: Core Write`

OAuth settings: https://login.tailscale.com/admin/settings/oauth

### server commands

#### `sudo psht-server upgrade`

Grabs the latest release and upgrades your server. You'll wanna run `psht update` on clients to get the most recent version of the CLI when you've upgraded the server.

#### `sudo psht-server doctor`

Check to see how the server installation is doing.

#### `sudo psht-server health`

Check to see how apps are doing.

## on ur local computer, after u set up the server

Install `psht` from the latest release (same version as your server is recommended), then configure your project host in `~/.psht/config.toml`:

```toml
host = "<host>"
```

### client commands

#### `psht setup`

Generates a `psht.toml` file for your project.

#### `psht deploy`

It'll try to deploy the current directory. If it's a git repo, it'll try to deploy `HEAD`. Otherwise it'll try to tar up the current directory and scp it.

You can also run `psht deploy path/to/bin` or `psht deploy [release url]` and it'll use those.

The app will be started and added to your tailnet. The app will have access to a `/storage` directory that sticks around between deploys.

#### `psht ps`

See what's running.

#### `psht logs [-f]`

See the logs for the current app (or pass `--app` to see a specific app). Passing `-f` will follow the logs as they come in.

#### `psht stop|start|restart`

Control the current app lifecycle from your project directory.

#### `psht tailscale`

Manage tailscale for the app.

#### `psht env FOO=123 BAR="abc"`

Set environment variables for the app.
These vars are also loaded in interactive Tailscale SSH logins to the app container.

- Per-app persistent storage mounted at `/storage` (survives deploys/rebuilds, removed by `destroy`).

### Optional InfluxDB Stats

`psht-server` can report deploy/health stats to InfluxDB (v2 API) when these env vars are set:

- `PSHT_STATS_INFLUX_URL` (example: `https://influx.example.com`)
- `PSHT_STATS_INFLUX_ORG`
- `PSHT_STATS_INFLUX_BUCKET`
- `PSHT_STATS_INFLUX_TOKEN`

Optional:

- `PSHT_STATS_INFLUX_MEASUREMENT` (default: `psht_stats`)
- `PSHT_STATS_INFLUX_TIMEOUT_SECS` (default: `2`)
- `PSHT_STATS_INFLUX_DEBUG=1` (prints write failures to stderr)

Measurements written:

- `<measurement>`: deploy/push attempt outcomes
- `<measurement>_health`: health summary checks

#### `psht update`

Fetch update metadata from the server and update your local `psht` CLI natively in Rust.

## other stuff

Use `psht deploy --force` to force a deploy even when the binary payload hash is unchanged.
For git projects, `psht deploy` now verifies the last successful deploy when `git push` is up to date, and retries automatically if the last attempt failed.
`psht deploy` now runs as an auto-recovery loop: it keeps retrying until deploy succeeds.

When a deploy is blocked by Incus operations, psht now uses a fixed escalation path:

1. wait briefly
2. cancel blocking operations
3. recheck
4. for any non-cancelable blocking op, force-stop + delete the blocked non-serving target instance, then continue

Blocked-operation recovery uses a default 15-second budget per cycle.
You can override this with `PSHT_BLOCKED_OP_BUDGET_SECS`.

`psht.toml` (project root) is used for release deploys and optional deploy hooks:

```toml
url = "https://github.com/org/repo/releases/download/v1.2.3/my-app-linux-amd64.tar.gz"
start = "./my-app --port $PORT"
app = "my-app" # optional override for derived app name
bin = "my-app" # optional path inside archive when needed
preinstall = "echo preparing deploy" # optional, runs before dependency install
postinstall = "npm run migrate" # optional, runs after dependency install and before start
apt_packages = ["libvips", "ffmpeg"] # optional, apt packages installed in container
required_env = ["DATABASE_URL", "JWT_SECRET"] # optional, deploy/start fail if these vars are missing
```
