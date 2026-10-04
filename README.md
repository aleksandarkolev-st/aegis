# Aegis — Terminal Agent Runtime

Aegis (`aegis`, also `arun`) is a Rust terminal agent runtime. It connects to supported model providers, runs tools through permission-filtered capabilities, and records task progress and evidence in a durable SQLite event log. You can follow work in the terminal, steer an active task, and inspect its results without handing control to a provider CLI.

## Get started

Launch `aegis` or `arun` with no arguments to open the interactive terminal. Choose a provider, sign in or configure a compatible custom endpoint, select a model, review workspace access, and describe the task. Aegis creates and follows the task for you; you do not need to enter run or attach commands.

ChatGPT and Grok use Aegis-owned direct HTTP flows. Browser approval is the recommended desktop sign-in flow; a device-code option is available for remote or headless terminals. Custom endpoints use an OpenAI-compatible interface. Claude subscription sign-in is pending. See [provider status and evidence](docs/direct-providers.md) and [browser sign-in boundaries](docs/browser-signin.md).

For a source build and advanced CLI use:

```powershell
cargo build --release
target/release/arun login chatgpt
target/release/arun run "Inspect this repository" --provider chatgpt
```

The npm package `aegis-arun@0.2.0` is published on npm. Install it globally and launch either command:

```sh
npm install --global aegis-arun
aegis
```

The package supports Windows x64 and Linux x64/arm64; macOS is unsupported. Published packages download a platform-specific runtime. Local native builds require Node 20+ and Rust. See [npm publishing](docs/npm-publishing.md) for package and release details.

## Working in the terminal

The interactive interface keeps ordinary terminal scrollback rather than taking over an alternate screen. It shows readable tool activity and results, supports steering an active task from the composer, and provides searchable menus and keyboard controls. Use `/` for the command catalog; F1 explains keyboard controls. Set `NO_COLOR=1` to disable colors or `AEGIS_REDUCED_MOTION=1` to reduce animation. See [terminal behavior and verification](docs/terminal-ui.md).

Aegis asks when intent or a decision is unclear. An unanswered question blocks dependent work, while independent work can continue. Ctrl+C interrupts active work; Ctrl+D detaches from a task. F3 opens saved tasks and recovery choices, and F5 starts a fresh conversation without deleting task history. Advanced users can inspect tasks with commands such as `arun status <run-id>`, `arun replay <run-id>`, `arun artifacts <run-id>`, and `arun metrics <run-id>`.

## Permissions and evidence

Workspace reads and search are available by default. Writes require permission; command execution is disabled unless a specific program and locally available container image are granted. Process workers use a network-disabled container with a read-only root filesystem and resource limits. Images are not pulled implicitly during task execution. Trusted-host MCP servers are an explicit exception and are not sandboxed. See [runtime contracts](docs/contracts.md) for details.

Task completion is not established by a model's claim alone. Successful operations produce evidence artifacts; explicit user requirements are tracked separately and need current-revision evidence. Interrupted operations with uncertain external effects are paused for reconciliation rather than blindly replayed. Independent completion checks can be configured, but their availability and scope should be reviewed for each project. See [task controls and proof boundaries](docs/control-surface.md).

Aegis records provider-reported usage when available and labels missing or estimated measurements rather than treating them as exact. Deadlines and resource limits still apply; there is no task-level model-turn or token cap. Metrics are measurements, not a guarantee of provider billing accuracy or task efficiency.

## Build and validate

```powershell
cargo test
cargo test --test docker -- --ignored
```

The ignored Docker test requires a running Docker daemon and the local `node:22-alpine` image. The npm package smoke test is run after building the native binary with `npm run build:native`, then `npm run test:package`. These checks do not establish live inference, hosted sign-in, or interactive UI quality. Review [verification notes](docs/verification.md) for dated observations and known gaps.

## Further documentation

- [Current provider evidence and remaining work](docs/direct-providers.md)
- [Terminal UI behavior](docs/terminal-ui.md)
- [Task controls and evidence boundaries](docs/control-surface.md)
- [Runtime contracts](docs/contracts.md)
- [Saved-chat continuation](docs/saved-chat-continuation.md)
- [npm release process](docs/npm-publishing.md)
- [Windows local setup](relay/local/README.md)

Aegis is under active development. Provider compatibility, account access, packaging, and verification status can change; consult the linked evidence notes rather than assuming every documented path has been independently validated.