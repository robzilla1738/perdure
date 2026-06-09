// Perdure for VS Code: diagnostics from `perdure check --json`, refreshed on
// open and save. Deliberately small — no LSP, no bundler, plain JavaScript.

"use strict";

const vscode = require("vscode");
const cp = require("child_process");
const fs = require("fs");
const path = require("path");
const { byteToPositionMapper } = require("./offsets");

let collection;
let statusWarned = false;

function activate(context) {
  collection = vscode.languages.createDiagnosticCollection("perdure");
  context.subscriptions.push(collection);

  const refresh = (doc) => {
    if (doc.languageId === "perdure") {
      runCheck(doc);
    }
  };
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument(refresh),
    vscode.workspace.onDidOpenTextDocument(refresh)
  );
  for (const doc of vscode.workspace.textDocuments) {
    refresh(doc);
  }
}

/** The directory `perdure check` should run in: the document's workspace
 * folder when there is one, else the file's own directory (single-file mode —
 * the CLI chases that file's imports itself). */
function checkRoot(doc) {
  const folder = vscode.workspace.getWorkspaceFolder(doc.uri);
  return folder ? folder.uri.fsPath : path.dirname(doc.uri.fsPath);
}

function runCheck(doc) {
  const bin = vscode.workspace.getConfiguration("perdure").get("path", "perdure");
  const cwd = checkRoot(doc);
  cp.execFile(
    bin,
    ["check", "--json"],
    { cwd, maxBuffer: 16 * 1024 * 1024 },
    (err, stdout) => {
      if (err && !stdout) {
        // Binary missing or unrunnable: warn once, fail soft.
        if (!statusWarned) {
          statusWarned = true;
          vscode.window.showWarningMessage(
            `perdure: could not run \`${bin} check\` — set the "perdure.path" setting. (${err.message})`
          );
        }
        return;
      }
      let diags;
      try {
        diags = JSON.parse(stdout);
      } catch {
        return; // non-JSON output (e.g. a panic) — leave diagnostics as-is
      }
      publish(cwd, diags);
    }
  );
}

/** Group the CLI's file-relative diagnostics per file and convert byte spans
 * to editor ranges using each file's own text. */
function publish(cwd, diags) {
  const byFile = new Map();
  for (const d of Array.isArray(diags) ? diags : []) {
    if (!d || !d.file || !d.span) continue;
    if (!byFile.has(d.file)) byFile.set(d.file, []);
    byFile.get(d.file).push(d);
  }

  collection.clear();
  for (const [rel, list] of byFile) {
    const abs = path.join(cwd, rel);
    let text;
    const open = vscode.workspace.textDocuments.find(
      (t) => t.uri.fsPath === abs
    );
    if (open) {
      text = open.getText();
    } else {
      try {
        text = fs.readFileSync(abs, "utf8");
      } catch {
        continue;
      }
    }
    const toPos = byteToPositionMapper(text);
    const out = list.map((d) => {
      const start = toPos(d.span.start);
      const end = toPos(d.span.end);
      const range = new vscode.Range(
        start.line,
        start.character,
        end.line,
        end.character
      );
      const severity =
        d.severity === "warning"
          ? vscode.DiagnosticSeverity.Warning
          : vscode.DiagnosticSeverity.Error;
      const message = d.notes && d.notes.length
        ? `${d.message}\n${d.notes.map((n) => `note: ${n}`).join("\n")}`
        : d.message;
      const diag = new vscode.Diagnostic(range, message, severity);
      diag.source = "perdure";
      diag.code = d.code;
      return diag;
    });
    collection.set(vscode.Uri.file(abs), out);
  }
}

function deactivate() {}

module.exports = { activate, deactivate };
