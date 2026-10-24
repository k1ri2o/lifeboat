# Lifeboat

Lifeboat is a small, open-source Windows app that keeps encrypted, versioned backups of the folders you pick on a hidden partition that ransomware and viruses can't easily reach. A Tauri 2 UI talks to a Rust Windows service (`lifeboat-svc`) over a local named pipe, and the service drives a bundled restic engine. See [docs/PLAN.md](docs/PLAN.md) for the design and milestones.

## Build

Requires Rust (stable, MSVC toolchain), Node 20+ and the WebView2 runtime.

```
cd app && npm install && npm run build && cd ..
cargo build --workspace
cargo test --workspace
```

Try the service in a console (pipe `\\.\pipe\lifeboat`):

```
cargo run -p lifeboat-svc -- run
```

Install it as a Windows service (admin shell): `lifeboat-svc install`, remove with `lifeboat-svc uninstall`.
Run the UI in dev mode: `cd app && npm run tauri dev`.

## Licence

MIT, see [LICENSE](LICENSE).
