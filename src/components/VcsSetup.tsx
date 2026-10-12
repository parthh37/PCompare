// araxis:vcs — Settings → Source control: sets PCompare up as git's diff and
// merge tool (after showing the exact `git config --global` commands and
// asking), and copy-paste configuration for Mercurial, Perforce and Subversion.
import { Check, Copy, GitBranch, Terminal } from "lucide-react";
import { useEffect, useState } from "react";
import { errorMessage } from "../lib/api";
import { writeClipboardText } from "../lib/clipboard";
import { vcsApi, type GitSetup, type Snippet } from "../lib/vcsApi";
import "./Vcs.css";

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      className="icon-btn"
      title="Copy"
      aria-label="Copy"
      onClick={() =>
        writeClipboardText(text).then(() => {
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1500);
        })
      }
    >
      {copied ? <Check size={13} /> : <Copy size={13} />}
    </button>
  );
}

/** A preformatted block with a copy button. */
function CodeBlock({ text }: { text: string }) {
  return (
    <div className="vcs-code">
      <pre>{text}</pre>
      <CopyButton text={text} />
    </div>
  );
}

function GitSection() {
  const [makeDefault, setMakeDefault] = useState(true);
  /** The commands shown for confirmation; null until "Set up git…". */
  const [preview, setPreview] = useState<GitSetup | null>(null);
  const [busy, setBusy] = useState(false);
  const [status, setStatus] = useState<{ ok: boolean; text: string } | null>(null);

  const show = (withDefault = makeDefault) => {
    setStatus(null);
    vcsApi
      .gitSetupPreview(withDefault)
      .then(setPreview)
      .catch((e) => setStatus({ ok: false, text: errorMessage(e) }));
  };
  const toggleDefault = (on: boolean) => {
    setMakeDefault(on);
    // Keep an open preview in step with the checkbox.
    if (preview) show(on);
  };

  const apply = () => {
    setBusy(true);
    vcsApi
      .gitSetupApply(makeDefault)
      .then(() => {
        setPreview(null);
        setStatus({
          ok: true,
          text: makeDefault
            ? "Done. git difftool and git mergetool now open PCompare."
            : "Done. Run git difftool -t pcompare or git mergetool -t pcompare to use it.",
        });
      })
      .catch((e) => setStatus({ ok: false, text: errorMessage(e) }))
      .finally(() => setBusy(false));
  };

  const commands = preview?.settings.map((s) => s.command).join("\n") ?? "";
  return (
    <fieldset>
      <legend>Git</legend>
      <p className="muted vcs-text">
        Registers PCompare as <code>difftool.pcompare</code> and <code>mergetool.pcompare</code> in your global git
        configuration. Both pass <code>--wait</code>, so git waits for the comparison tab to close even when PCompare is
        already open; merges report whether you saved the result (<code>trustExitCode</code>).
      </p>
      <label className="field check">
        <input type="checkbox" checked={makeDefault} onChange={(e) => toggleDefault(e.target.checked)} />
        Also make PCompare git's default diff and merge tool (<code>diff.tool</code>, <code>merge.tool</code>)
      </label>
      {!preview ? (
        <button onClick={() => show()}>
          <GitBranch size={14} /> Set up git…
        </button>
      ) : !preview.gitAvailable ? (
        <div className="banner error">git isn't installed or isn't on PATH, so nothing can be configured.</div>
      ) : (
        <div className="vcs-confirm">
          <span>These commands will be run:</span>
          <CodeBlock text={commands} />
          {preview.settings.some((s) => s.current && s.current !== s.value) && (
            <div className="vcs-replaces">
              <span className="muted">They replace:</span>
              {preview.settings
                .filter((s) => s.current && s.current !== s.value)
                .map((s) => (
                  <code key={s.key}>
                    {s.key} = {s.current}
                  </code>
                ))}
            </div>
          )}
          <div className="vcs-actions">
            <button onClick={() => setPreview(null)} disabled={busy}>
              Cancel
            </button>
            <button className="primary" onClick={apply} disabled={busy}>
              <Terminal size={14} /> {busy ? "Running…" : "Run these commands"}
            </button>
          </div>
        </div>
      )}
      {status && <div className={`banner ${status.ok ? "ok" : "error"} vcs-status`}>{status.text}</div>}
    </fieldset>
  );
}

function SnippetsSection() {
  const [snippets, setSnippets] = useState<Snippet[] | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    vcsApi
      .snippets()
      .then(setSnippets)
      .catch((e) => setError(errorMessage(e)));
  }, []);
  return (
    <fieldset>
      <legend>Mercurial, Perforce and Subversion</legend>
      <p className="muted vcs-text">PCompare doesn't change these tools' settings; copy what you need.</p>
      {error && <div className="banner error">{error}</div>}
      {snippets?.map((s) => (
        <div key={s.tool} className="vcs-snippet">
          <div>
            <b>{s.tool}</b> <span className="muted">— {s.target}</span>
          </div>
          <CodeBlock text={s.text} />
        </div>
      ))}
    </fieldset>
  );
}

export function VcsSetupPage() {
  return (
    <div className="settings-grid single vcs-setup">
      <GitSection />
      <SnippetsSection />
      <fieldset>
        <legend>Comparing revisions</legend>
        <p className="muted vcs-text">
          Any path field takes <code>git:&lt;path&gt;@&lt;revision&gt;</code> or <code>hg:&lt;path&gt;@&lt;revision&gt;</code>
          — a file or folder of a clone at that revision, e.g. <code>git:C:\src\app\main.c@HEAD~1</code> against the
          working copy. The branch button next to a path field picks a revision from the history.
        </p>
      </fieldset>
    </div>
  );
}
