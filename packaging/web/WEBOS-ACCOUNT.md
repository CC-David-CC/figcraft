# FigCraft in a WebOS account window

The WebOS editor uses the native FigCraft model, File commands, version history,
and branch codec. Desktop owns the account window and Storage owns the file path
and server revision. The accepted native association is `.figcraft`.

## Build

Use the locked WASM graph and `packaging/web/craft-fonts.rev`. Set absolute
`CRAFT_FONTS_DIR` and `CARGO_TARGET_DIR` paths. Provide Trunk with the locked
`wasm-bindgen` CLI from `Cargo.lock` and the builder's pinned `wasm-opt-v123`.
Use a fresh output directory outside the source and live site.

```sh
cargo fetch --locked --offline --target wasm32-unknown-unknown
cargo check --locked --offline --target wasm32-unknown-unknown -p figcraft-web
cargo test --locked --offline -p figcraft-engine --lib account_project
bash packaging/web/build-account.sh /path/to/fresh-output
```

The build command runs Trunk offline and copies the native editor, font licenses,
and notices into the output directory. Parent assembly records the source tree
and individual output hashes, and adapts the generated Trunk bootstrap for the
site CSP. Keep the normal `packaging/web/package.sh` recipe for standalone.

## Parent connection

Use a same-origin iframe at `figcraft/index.html?host=webos-account&profile=<owner>`.
The validated owner string names the persisted author setting. Hosted frames use
fresh egui widget memory and the parent journal. The standalone recovery store,
page-control API, and online collaboration transport are unavailable in this mode.
Use `&webgl` to exercise the renderer in headless Chromium when needed.

Packets use `type: "figcraft:account-<kind>"` and the 32-hex `generation`
announced by `ready`. An unsuccessful renderer startup emits `failed` with a
message when the parent is available. The parent fences requests to the original
iframe Document. The native side accepts its actual same-origin parent window
and current generation. Native numeric IDs, tickets, and revisions are decimal
strings. Binary data is a transferred `ArrayBuffer` of up to 128 MiB.

| Request or notification | Payload and result |
|---|---|
| Parent `start` | UUID `requestId`, `path` string or null, `bytes` buffer or null, `recover` boolean, `newSettings` JSON string or null. For a new design or board pass settings `kind`, optionally `name`. A template launch uses `kind: "template"` with a native template ID. `started` echoes the request and reports `ok`, accepted path and project state, or a message. A failed read leaves the frame empty for Retry. |
| Native `project-state` | `available`, `documentId`, `revision`, `recoveryRevision`, `dirty`, `capturable`, `autosaveEligible`, `operationPending`, `unfinishedEdit`. A monotonic revision accounts for edits and membership changes in the root and branch tabs. Save and recovery use that revision. |
| Native `dialog` | Decimal `requestId` and `operation: "new"`, `"open"`, or `"save"`. New sends settings, Open sends `path: null`, Save sends `suggested`. Parent `dialog-result` responds with the matching ID and `outcome: "opened"`, `"cancelled"`, `"failed"`, or `"destination"` with a full account-relative path. |
| Native `save` | `ticket`, `documentId`, `revision`, `path`, and native `.figcraft` bytes. Parent performs a conditional replacement or create-only write based on the verified binding. |
| Parent `save-result` | The exact save identity and `outcome: "written"`, `"failed"`, or `"unknown"`. A definitive outcome replies `save-settled` with the same identity, `adopted`, and `projectState`. An unknown outcome keeps the original ticket pending. The native editor adopts the path only for a confirmed write. Later edits keep their dirty baseline. |
| Parent `recovery-capture` | UUID `requestId`, `documentId`, and `revision` from `recoveryRevision`. `recovery-captured` echoes these with bytes or a message. Drafts use the native archive with current branch heads, images, and version history. A saved-base mismatch requires Save As. |
| Parent `autosave` or `session-save` | UUID `requestId` and reviewed `documentId`, `revision`, `path`. The matching `-result` returns `outcome: "captured"` and a normal save identity plus bytes, or `"busy"`/`"failed"`. `session-save-result` includes `reviewedRevision`. Automatic requests require the confirmed path and an eligible native state. |
| Native `close-intent` | Desktop runs its Save/Discard/Cancel review for the whole project. Closing a native branch tab retains its edits in the project and stays within the account window. A direct `close` request provides clean-state validation and returns `close-result {requestId, approved}`. |
| Parent `session` | UUID `requestId`, `operation: "pause"`, `"release"`, or `"finalize"`, and `lease`. Pause includes reviewed `documentId`, `revision`, `dirty`, and `unfinishedEdit`. `session-result` echoes the request, operation, lease, and `ok`. Pause fences native input and background work. A matching release resumes the editor, or matching finalization permits disposal. |
| Parent `operation` | UUID `requestId`, `documentId`, exact bound account `path` or null, `command`, and bounded JSON `args`. `operation-result` echoes the request and document with JSON `result` or a message. Desktop authorizes the delegated user and exact resource before dispatch. |

Scoped `document.inspect {}` returns the document name, revision, dirty state,
page/version/branch counts, and up to 256 layer IDs and names. `layer.rename
{layerId, name}` requires an active root project and an idle, editable native
session. It executes the same `object.rename` command used by the Layers panel and
leaves the normal Undo step. Native branch tabs retain human editing access.

## Qualification and remaining work

The native account tests cover exact save receipts, an unknown write, later edits,
branch and version recovery, and a reversible scoped rename. Until the real
browser suite passes, treat the following as qualification work.

| Area | Required browser evidence |
|---|---|
| Native editing and imports | Build a frame, component and instance, place a synthetic embedded image, edit a branch, and inspect version history. Ordinary text already committed to the model is recoverable. Finish active composition and gestures before capture. |
| Storage, windows, and recovery | Save and Save As through native controls, verify server bytes and fresh-context reopening, exercise activation and Retry, inject conflicts and lost acknowledgments, and test dirty-window and session close decisions. Test refresh and orphan recovery with a dirty branch. Reopen the branch from native branch history after restore. |
| Scope and preferences | Approve an exact view grant, rename and Undo, reject another view and stale resource scope, and check author and egui isolation across account switches. |
| Output and release | Check SVG and PDF browser downloads independently of project binding. Local linked libraries and upstream multiplayer are separate workflows. Verify the actual WASM output, hosted MIME and CSP, and screenshots before release. |
