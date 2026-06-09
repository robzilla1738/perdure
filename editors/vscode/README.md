# Perdure for VS Code

Language support for [Perdure](https://github.com/robzilla1738/perdure)
(`.pdr` files and `Perdurefile`):

- syntax highlighting (keywords, goal sections, tool calls, authority grants,
  strings, comments)
- diagnostics on open and save, straight from `perdure check --json` — every
  squiggle is the same machine-readable diagnostic an agent sees, including
  the error code

## Requirements

The `perdure` binary on your `PATH`, or point the `perdure.path` setting at
it.

## Install (from source)

```bash
cd editors/vscode
npx --yes @vscode/vsce package
code --install-extension perdure-*.vsix
```

No build step: the extension is plain JavaScript.

## Settings

| Setting        | Default   | Meaning                              |
| -------------- | --------- | ------------------------------------ |
| `perdure.path` | `perdure` | Binary used to produce diagnostics. |
