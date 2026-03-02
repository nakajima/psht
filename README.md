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

### server commands

#### `sudo psht-server upgrade`

Grabs the latest release and upgrades your server. You'll wanna run `psht update` on clients to get the most recent version of the CLI when you've upgraded the server.

#### `sudo psht-server doctor`

Check to see how the server installation is doing.

#### `sudo psht-server health`

Check to see how apps are doing.

## on ur local computer, after u set up the server

```sh
$ ssh psht@<host> setup | sh
```

This will set up the `psht` cli on your computer.

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

#### `psht tailscale`

Manage tailscale for the app.

#### `psht env FOO=123 BAR="abc"`

Set environment variables for the app.

- Per-app persistent storage mounted at `/storage` (survives deploys/rebuilds, removed by `destroy`).

#### `psht update`

Update the `psht` cli to whatever is on the server.

## other stuff

Use `psht deploy --force` to force a deploy even when the binary payload hash is unchanged.
For git projects, `psht deploy` now verifies the last successful deploy when `git push` is up to date, and retries automatically if the last attempt failed.

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
