# sweetidle

`sweetidle` is a lightweight Wayland idle daemon for the Sweets compositor.
It runs commands when you go idle, come back, lock the session, or suspend.

## Quick start

On NixOS, first enter the [development shell](#nixos-development).

```sh
cargo build --release
mkdir -p ~/.config/sweetidle
cp config/example.toml ~/.config/sweetidle/config.toml
./target/release/sweetidle --check-config
./target/release/sweetidle
```

Use another config with `--config`:

```sh
./target/release/sweetidle --config ~/.config/sweetidle/laptop.toml
```

## Configuration examples

Config is TOML, UTF-8, and at most 1 MiB. Commands run through `sh -c` in their
own process group, so stopping sweetidle leaves them running. Their stdout is
discarded and their stderr goes to sweetidle's stderr, which is the journal
under the systemd unit. Use `journalctl --user --unit=sweetidle.service` to see
why a hook failed.

The default path needs an absolute `HOME`. Use `--config <PATH>` to avoid that.

### Lock after five minutes

```toml
[[listener]]
timeout = 300
on_timeout = "sweetslock"
ignore_inhibit = true
```

`ignore_inhibit = true` locks even while a video or browser blocks idle.

### Turn displays off after ten minutes

```toml
[[listener]]
timeout = 600
on_timeout = "sweetsmsg output '*' power off"
on_resume = "sweetsmsg output '*' power on"
```

This listener respects inhibitors, so displays stay on during video playback.

### Lock before suspend

```toml
[general]
before_sleep_command = "sweetslock --daemonize"
after_resume_command = "sweetsmsg output '*' power on"
wait_before_sleep = true
```

`wait_before_sleep` delays suspend until the lock command exits, up to logind's
`InhibitDelayMaxSec`. Use a locker that daemonizes only after locking.

### Handle session lock requests

```toml
[general]
lock_command = "sweetslock"
# only if your locker has a safe unlock command
# unlock_command = "your-locker-control unlock"
```

### Report idle state to logind

```toml
[idle_hint]
timeout = 300
```

See the full [example configuration](config/example.toml).

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
| `on_resume` | unset | Run when activity returns after this timeout |
| `ignore_inhibit` | `false` | Run despite all idle inhibitors |

### `[idle_hint]`

| Option | Default | Purpose |
|---|---:|---|
| `timeout` | required | Seconds before reporting idle to logind (1–4,294,967) |

Unknown fields, bad timeouts, and empty commands are rejected.

## Control commands

```sh
sweetidle status          # show daemon state
sweetidle status --json   # same, for scripts and bars
sweetidle pause           # stop idle timers
sweetidle resume          # restart timers from zero
sweetidle reload          # reload config (or: pkill -HUP -x sweetidle)
```

Pause does not stop running commands or sleep and lock hooks.

`reload` only queues the reload; check the logs for the result. An invalid
config keeps the old one active. Running commands keep running.

Changing a listener that has already timed out rejects the reload. Retry after
you become active again. D-Bus or logind changes need a restart.

## Idle inhibitors

Compositor, ScreenSaver D-Bus, and systemd-logind inhibitors are respected by
default.

- `ignore_inhibit = true` makes a listener ignore all of them. It needs
  `ext_idle_notifier_v1` version 2.
- The idle hint always respects inhibitors.
- `status` lists D-Bus and systemd inhibitors. Compositor inhibitors are not
  visible to clients.

To ignore a whole inhibitor source:

```toml
[general]
ignore_dbus_inhibit = true
ignore_systemd_inhibit = true
```

## Systemd user service

For distributions with a writable `/usr/local`:

```sh
cargo build --release
sudo install -Dm755 target/release/sweetidle /usr/local/bin/sweetidle
sudo install -Dm644 systemd/sweetidle.service /usr/local/lib/systemd/user/sweetidle.service
sudo install -Dm644 man/sweetidle.1 /usr/local/share/man/man1/sweetidle.1
systemctl --user daemon-reload
systemctl --user enable --now sweetidle.service
```

Useful commands:

```sh
systemctl --user status sweetidle.service
systemctl --user reload sweetidle.service
journalctl --user --unit=sweetidle.service --follow
```

Don't also start sweetidle from your compositor config. A second instance exits
with `another sweetidle instance is listening` before it touches the session.

NixOS has no package or service module yet. Run the local build from a Wayland
terminal instead.

## Command-line help

```sh
sweetidle --help
man sweetidle
man -l man/sweetidle.1   # without installing
```

## Development

### NixOS development

From the directory containing `Cargo.toml`:

```sh
nix develop
```

The shell pins Rust tooling, cargo-deny, and D-Bus test tools. It needs the
`nix-command` and `flakes` features. Use `nix develop path:.` while the flake
files are untracked. With direnv, review `.envrc` and run `direnv allow`.

Rebuild binaries copied from other distributions. No Wayland or systemd
development libraries are needed.

Hooks inherit sweetidle's `PATH`, so `sh` and your helpers must be on it. Start
the daemon from your Wayland session so it gets `WAYLAND_DISPLAY`,
`XDG_RUNTIME_DIR`, and the D-Bus address.

A live daemon with the example config can lock your session and turn off
displays. Prefer the mock-compositor tests during development.

### Checks

```sh
cargo fmt --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
./scripts/check-deps
```

Mock-compositor integration tests:

```sh
cargo test --locked --test reload -- --ignored --test-threads=1
```

## License

[GPL-3.0-or-later](LICENSE)
