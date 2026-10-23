# Development workflow

This launcher is a native Tauri 2 application. Keep changes grounded in the current Rust, Svelte and TypeScript implementation; tools used during development are not runtime dependencies.

## Platform focus

- Prioritize the Windows experience and validate Windows behavior first. Keep the existing Linux/Fedora support intact when shared code changes, but do not expand Linux-only features unless requested.
- Do not open the installed launcher or start Minecraft during routine development. Use focused tests, GitHub Actions startup/install checks and repository evidence. Minecraft must never be started by CI smoke tests.
- Do not download release artifacts to the developer machine unless the owner explicitly asks. CI artifact metadata and logs are enough for routine verification.

## Skills to apply by work type

- Launcher lifecycle, Java, Forge, instances, downloads and process handling: `launcher-engineering`, `santi-debug-efficient`, `verification-build`, and `santi-artifact-verification` when packaging is involved.
- Native interface behavior and accessibility: `santi-ui-engineering`. For a requested visual redesign or critique, add `impeccable` and `ui-ux-pro-max`; inspect the existing implementation and screenshots before editing. Follow the requester's preference to leave the app closed.
- Git history, branch, pull request and Actions work: `santi-github-engineering`. Use `santi-project-intelligence` and `santi-task-contract` to recover project constraints before broad changes.
- `awwwards` can provide visual references for a specific UI request; `21st.dev` can help find optional components or design tokens. Treat both as research only, verify fit with Tauri/Svelte, and do not add packages or copy assets by default.
- Do not use image generation for launcher artwork. Use owner-provided or properly attributed existing remote assets when artwork work is requested.

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
