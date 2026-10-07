# sweetsidle

`sweetsidle` is a lightweight Wayland idle daemon for the Sweets compositor.
It runs commands when you become idle, return, lock the session, or suspend the
system.

## Quick start

On NixOS, first enter the [development shell](#nixos-development).

Build the release binary:

```sh
cargo build --release
```

Create the default configuration:

```sh
mkdir -p ~/.config/sweetsidle
cp config/example.toml ~/.config/sweetsidle/config.toml
```

Check it before starting:

```sh
./target/release/sweetsidle --check-config
```

Run sweetsidle:

```sh
./target/release/sweetsidle
```

Use another config with `--config`:

```sh
./target/release/sweetsidle --config ~/.config/sweetsidle/laptop.toml
```

## Configuration examples

Configuration uses TOML. Commands run through `sh -c`, so shell quoting and
expansion work as expected. Each command starts in its own process group.
Stopping sweetsidle with Ctrl-C, SIGINT, or SIGTERM leaves running commands
alive, including session lockers and helpers.

### Lock after five minutes

```toml
[[listener]]
timeout = 300
on_timeout = "sweetslock"
ignore_inhibit = true
```

`ignore_inhibit = true` lets the lock run even while a browser or media player
is preventing other idle actions. Its `on_resume`, if configured, waits for that
listener's own resume notification rather than another listener's.

### Turn displays off after ten minutes

```toml
[[listener]]
timeout = 600
on_timeout = "sweetsmsg output '*' power off"
on_resume = "sweetsmsg output '*' power on"
```

This listener respects idle inhibitors by default, keeping displays on during
video playback or presentations.

### Lock before suspend

```toml
[general]
before_sleep_command = "sweetslock --daemonize"
after_resume_command = "sweetsmsg output '*' power on"
wait_before_sleep = true
```

With `wait_before_sleep = true`, sweetsidle waits for the lock command before
releasing its logind sleep-delay inhibitor. Logind can proceed after
`InhibitDelayMaxSec`; sweetsidle releases that cycle's inhibitor when logind
reports that sleep completed or failed, even if the command is still running.
Use a locker that daemonizes only after the session is locked. A command that
keeps running can delay each sleep attempt up to logind's maximum delay.

### Handle session lock requests

```toml
[general]
lock_command = "sweetslock"
# Only set this when the locker has a safe control command.
# unlock_command = "your-locker-control unlock"
```

Lock and unlock requests are routed in logind signal order. Hooks run
asynchronously, so one hook can start before the previous hook finishes.

### Report idle state to logind

```toml
[idle_hint]
timeout = 300
```

See the complete [example configuration](config/example.toml).

## Configuration reference

### `[general]`

| Option | Default | Purpose |
|---|---:|---|
| `lock_command` | unset | Run on a logind lock request |
| `unlock_command` | unset | Run on a logind unlock request |
| `before_sleep_command` | unset | Run before suspend |
| `after_resume_command` | unset | Run after resume |
| `wait_before_sleep` | `true` | Wait for the before-sleep command |
| `ignore_dbus_inhibit` | `false` | Ignore ScreenSaver D-Bus inhibitors |
| `ignore_systemd_inhibit` | `false` | Ignore systemd-logind idle inhibitors |

### `[[listener]]`

| Option | Default | Purpose |
|---|---:|---|
| `timeout` | required | Idle timeout in seconds (1–4,294,967) |
| `on_timeout` | unset | Run when the timeout expires |
| `on_resume` | unset | Run when this listener resumes after timeout |
| `ignore_inhibit` | `false` | Run despite all supported idle inhibitors |

### `[idle_hint]`

| Option | Default | Purpose |
|---|---:|---|
| `timeout` | required | Seconds before reporting the session as idle (1–4,294,967) |

Unknown fields, missing timeouts, timeouts outside 1–4,294,967 seconds, and
empty commands are rejected by `--check-config`.

## Control commands

Inspect the running daemon:

```sh
sweetsidle status
```

Get JSON for scripts or status bars:

```sh
sweetsidle status --json
sweetsidle status --json | jq -r '.paused'
```

Pause and resume idle timers:

```sh
sweetsidle pause
sweetsidle resume
```

`resume` starts every listener with a fresh timeout. Pause does not stop commands
that are already running or disable sleep and session hooks.

Ask the running daemon to reload its current config:

```sh
sweetsidle reload
```

The command confirms that the reload was queued. Check daemon logs for the
validation and application result. You can also request the same reload with
SIGHUP:

```sh
pkill -HUP -x sweetsidle
```

Requests received while a reload is pending are coalesced into one follow-up
read, even if the pending read is rejected. This picks up edits saved after the
worker read the file.

A reload rejected during config or compositor compatibility validation keeps the
previous config active. Changes to required D-Bus or logind integrations need a
full restart.

Commands already running keep running after a reload. Unchanged listener
definitions keep duplicate suppression even when reordered. Changing or
removing a listener releases its old command slot; its replacement can run
while the old command is still running. A definition includes its timeout,
both commands, and inhibitor policy. Session hooks keep their running slots
across reloads.

## Idle inhibitors

- Compositor, ScreenSaver D-Bus, and systemd-logind idle inhibitors are
  respected by default.
- Set `ignore_inhibit = true` on listeners that must always run.
- `ignore_inhibit = true` requires `ext_idle_notifier_v1` version 2.
- The idle-hint timer always respects inhibitors.
- ScreenSaver inhibitors are released when their D-Bus client disconnects,
  including during an `Inhibit` call.
- ScreenSaver clients can call `Inhibit` and `UnInhibit` at either
  `/org/freedesktop/ScreenSaver` or `/ScreenSaver`.
- `status` can list D-Bus and systemd inhibitors. Wayland does not expose
  compositor inhibitor details.

To disable a global inhibitor source:

```toml
[general]
ignore_dbus_inhibit = true
ignore_systemd_inhibit = true
```

## Systemd user service

The manual installation below is for distributions with writable `/usr/local`
directories. On NixOS, develop with the shell below and run the local binary
from a Wayland terminal. Permanent installation should use a Nix package and a
declarative NixOS or Home Manager user service; this repository currently
provides a development shell, without package or service module outputs.

Build and install from the repository on those distributions:

```sh
cargo build --release
sudo install -Dm755 target/release/sweetsidle /usr/local/bin/sweetsidle
sudo install -Dm644 systemd/sweetsidle.service /usr/local/lib/systemd/user/sweetsidle.service
sudo install -Dm644 man/sweetsidle.1 /usr/local/share/man/man1/sweetsidle.1
```

Enable the service:

```sh
systemctl --user daemon-reload
systemctl --user enable --now sweetsidle.service
```

Useful commands:

```sh
systemctl --user status sweetsidle.service
systemctl --user reload sweetsidle.service
journalctl --user --unit=sweetsidle.service --follow
```

Do not also start sweetsidle from compositor configuration while the service is
enabled.

## Command-line help

```sh
sweetsidle --help
man sweetsidle
```

The checked-in manual can be opened without installing it:

```sh
man -l man/sweetsidle.1
```

## Development

### NixOS development

From the directory containing `Cargo.toml`, enter the pinned environment:

```sh
nix develop
```

The shell provides Rust, Cargo, Clippy, rustfmt, rust-analyzer, cargo-deny, a C
linker, Bash, and D-Bus tools for the private-bus tests. `flake.lock` pins
Nixpkgs 26.05 and its toolchain independently of the system's Rustup setup.
Flakes require the `nix-command` and `flakes` experimental features in Nix.
See the [Nix development-shell documentation](https://nix.dev/manual/nix/2.24/command-ref/new-cli/nix3-develop).

For automatic activation with direnv and nix-direnv installed, review `.envrc`
and run `direnv allow`. Build and run editor tooling inside that environment.
If these flake files are still untracked in Git, use `nix develop path:.`
instead; normal local Git flakes only include files known to Git.

Rebuild binaries carried over from another distribution. An old Arch build can
refer to a dynamic loader that NixOS does not provide. The current Wayland
backend and zbus use Rust implementations, so this project does not require
Wayland or systemd development libraries, `pkg-config`, or `LD_LIBRARY_PATH`.

Configured hooks inherit sweetsidle's `PATH`; `sh` and every configured helper
must be available there. A future Nix user service should use absolute Nix
package paths for the daemon and hooks, and explicitly provide its shell and
helper environment. Start the local daemon from the active Wayland session so
it inherits `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR`, and the session D-Bus address.

Use the private mock-compositor tests below for development. A live daemon using
the example configuration can lock your session and power off displays. Check
the commands and any existing idle or lock-before-sleep services before testing
against your desktop.

### Checks

Run the local checks:

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/check-deps
```

Run the private mock-compositor integration tests:

```sh
cargo test --locked --test reload -- --ignored --test-threads=1
```

## License

[GPL-3.0-or-later](LICENSE)
