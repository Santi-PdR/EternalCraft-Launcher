/home/Santipdr/.bashrc: line 47: /tmp/eternalcraft-cargo/env: No such file or directory
/home/Santipdr/.bash_profile: line 18: /tmp/eternalcraft-cargo/env: No such file or directory
# Development workflow

This launcher is a native Tauri 2 application. Keep changes grounded in the current Rust, Svelte and TypeScript implementation; tools used during development are not runtime dependencies.

## Current Codex tool inventory

Choose tools by the work in front of us; the connected catalog is broad and changes over time, so this guide records capabilities rather than a fixed tool count.

- Repository and review work: use the GitHub connector for focused reads of PRs, changed files, review threads, workflow jobs/logs and artifact metadata. Use Git/`gh` for the normal checkout, focused edits, commits and pushes. Do not download CI artifacts unless explicitly requested, and do not use connector write operations to merge or make unrelated repository changes.
- Build and runtime evidence: use repository scripts and GitHub Actions as the source of truth for Windows packaging, startup and installer checks. The Codex terminal/shell can run focused checks where a toolchain already exists. UI/browser automation is available for targeted diagnosis, but leave the launcher closed unless opening it is necessary to answer a concrete issue; never start Minecraft as a test.
- UI research: the Awwwards tools can inspect references, page structure and motion; 21st.dev tools can search components and design tokens. Use these only for a requested interface task, adapt concepts to the existing Tauri/Svelte app, and verify licensing, accessibility, scaling and runtime cost. They are not reasons to add a web stack or new package.
- Applicable skills: `launcher-engineering` for game/runtime lifecycle; `santi-debug-efficient` for root-cause work; `verification-build` and `santi-artifact-verification` for builds and packages; `santi-ui-engineering` for native launcher UI; `santi-github-engineering` for branches, PRs and Actions; `santi-project-intelligence` and `santi-task-contract` for project constraints and bounded changes. For a visual redesign, add `impeccable` and `ui-ux-pro-max`; use `awwwards-inspiration` or `awwwards-motion-study` only when references or motion are part of the requested change.
- Out of scope by default: Stripe/billing, Vercel hosting, Blockbench/Minecraft model authoring, and unrelated office/media tools do not fit this desktop launcher. Do not add them to the product or install plugins just because they are present in Codex. Do not generate launcher artwork; use owner-provided or properly attributed remote assets when artwork work is requested.

## Platform focus

- Prioritize the Windows experience and validate Windows behavior first. Keep the existing Linux/Fedora support intact when shared code changes, but do not expand Linux-only features unless requested.
- Do not open the installed launcher or start Minecraft during routine development. Use focused tests, GitHub Actions startup/install checks and repository evidence. Minecraft must never be started by CI smoke tests.
- Do not download release artifacts to the developer machine unless the owner explicitly asks. CI artifact metadata and logs are enough for routine verification.

## Skills to apply by work type

- Launcher lifecycle, Java, Forge, instances, downloads and process handling: `launcher-engineering`, `santi-debug-efficient`, `verification-build`, and `santi-artifact-verification` when packaging is involved.
- Native interface behavior and accessibility: `santi-ui-engineering`. For a requested visual redesign or critique, add `impeccable` and `ui-ux-pro-max`; inspect the existing implementation and screenshots before editing. Follow the requester's preference to leave the app closed.
- Git history, branch, pull request and Actions work: `santi-github-engineering`. Use `santi-project-intelligence` and `santi-task-contract` to recover project constraints before broad changes.
- The Awwwards and 21st.dev tools can provide visual references for a specific UI request. Treat both as research only, verify fit with Tauri/Svelte, and do not add packages or copy assets by default.

## GitHub and verification loop

1. Confirm the repository, active PR, branch head and working tree before editing.
2. Make a focused change and add a test for the failure mode. Avoid unrelated refactors in the same commit.
3. Run lightweight repository checks locally when their runtimes are already available; do not install toolchains merely to claim local verification.
4. Push the existing work branch and verify the exact commit's Actions jobs. For Windows changes, require Windows Rust tests, NSIS build, launcher startup smoke test, and installer install/uninstall verification. Keep Minecraft installation smoke tests separate from starting the game.
5. Review PR comments and inline review threads, inspect the final diff, and update the existing PR. Do not merge automatically.
6. Report exactly which checks passed, which remain in progress, and which were not run. Do not claim the launcher is playable based on a compile or app-startup smoke test alone.

The GitHub connector provides read access to PR metadata, patches, reviews, review threads, workflow jobs, logs and artifact listings; it also exposes write operations. Use read operations for review and CI inspection. Use write operations only for changes already authorized by the owner. Keep `gh`/Git for the normal clone, commit and push workflow. Never download CI artifacts as a substitute for an explicit request.

## Versioning

For launcher behavior changes, keep the version synchronized in `package.json`, `package-lock.json`, `src-tauri/Cargo.toml`, `src-tauri/Cargo.lock` and `src-tauri/tauri.conf.json`, then run `node scripts/check-versions.mjs`. Documentation-only changes do not require an application version bump.
