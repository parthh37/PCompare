import { open } from "@tauri-apps/plugin-dialog";
import {
  Box,
  CircleCheck,
  Cloud,
  CloudUpload,
  Cylinder,
  Earth,
  Flame,
  FolderOpen,
  GitBranch,
  Globe,
  Hexagon,
  Library,
  Plus,
  Server,
  ShieldCheck,
  Triangle,
  type LucideIcon,
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  api,
  errorMessage,
  REMOTE_SCHEME,
  SIGN_IN_CANCELLED,
  type FieldSpec,
  type Profile,
  type ProfileInput,
  type ProviderKind,
  type ProviderSpec,
} from "../lib/api";

const REFRESH_TOKEN = "refresh_token";
/** A token pasted instead of signing in (Dropbox). */
const ACCESS_TOKEN = "access_token";

/** Display names for select options whose stored values are terse keys. */
const OPTION_LABELS: Record<string, string> = {
  password: "Password",
  "keyboard-interactive": "Keyboard-interactive (password, one-time codes)",
  key: "Private key file",
  agent: "SSH agent",
  token: "Personal access token",
  "github-cli": "GitHub CLI login (gh auth login)",
  anonymous: "Nothing (public repositories only)",
};

/** Generic glyphs tinted with each vendor's brand colour (no trademarked logos). */
const PROVIDER_ICON: Record<ProviderKind, [LucideIcon, string]> = {
  s3: [Cylinder, "#ff9900"],
  azblob: [Hexagon, "#0089d6"],
  gcs: [Cloud, "#4285f4"],
  b2: [Flame, "#e21e29"],
  ftp: [Server, "#94a3b8"],
  sftp: [ShieldCheck, "#22c55e"],
  webdav: [Earth, "#38bdf8"],
  onedrive: [CloudUpload, "#0364b8"],
  dropbox: [Box, "#0061ff"],
  gdrive: [Triangle, "#34a853"],
  sharepoint: [Library, "#038387"],
  github: [GitBranch, "#8b949e"],
};

/** Whose account the sign-in button asks for. */
const SIGN_IN_WITH: Partial<Record<ProviderKind, string>> = {
  onedrive: "Microsoft",
  sharepoint: "Microsoft",
  dropbox: "Dropbox",
  gdrive: "Google",
};

function ProviderIcon({ kind, size = 16 }: { kind: ProviderKind; size?: number }) {
  const [Icon, color] = PROVIDER_ICON[kind];
  return <Icon size={size} color={color} aria-hidden />;
}

interface Draft {
  id?: string;
  name: string;
  kind: ProviderKind;
  settings: Record<string, string>;
  /** Secrets the user typed or cleared in this edit; "" = remove. */
  secrets: Record<string, string>;
  stored: string[];
}

type Status = { tone: "ok" | "error" | "info"; text: string } | null;

function draftFrom(p: Profile): Draft {
  return { id: p.id, name: p.name, kind: p.kind, settings: { ...p.settings }, secrets: {}, stored: p.storedSecrets };
}

function newDraft(spec: ProviderSpec): Draft {
  const settings: Record<string, string> = {};
  spec.fields.forEach((f) => f.default && (settings[f.key] = f.default));
  return { name: "", kind: spec.kind, settings, secrets: {}, stored: [] };
}

const toInput = (d: Draft): ProfileInput => ({
  id: d.id,
  name: d.name,
  kind: d.kind,
  settings: d.settings,
  secrets: d.secrets,
});

export function Profiles() {
  const [providers, setProviders] = useState<ProviderSpec[]>([]);
  const [profiles, setProfiles] = useState<Profile[]>([]);
  const [redirectUri, setRedirectUri] = useState("");
  const [draft, setDraft] = useState<Draft | null>(null);
  const [dirty, setDirty] = useState(false);
  const [status, setStatus] = useState<Status>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [menu, setMenu] = useState<{ x: number; y: number; profile: Profile } | null>(null);
  /** A sign-in is waiting for the browser (so leaving the page cancels it). */
  const signingIn = useRef(false);

  useEffect(() => {
    Promise.all([api.listProviders(), api.listProfiles(), api.oauthRedirectUri()])
      .then(([pv, pf, uri]) => {
        setProviders(pv);
        setProfiles(pf);
        setRedirectUri(uri);
        if (pf.length) setDraft(draftFrom(pf[0]));
      })
      .catch((e) => setStatus({ tone: "error", text: errorMessage(e) }));
  }, []);

  useEffect(
    () => () => {
      // Leaving Profiles mid sign-in frees the redirect port for next time.
      if (signingIn.current) void api.cancelSignIn();
    },
    [],
  );

  useEffect(() => {
    if (!menu) return;
    const close = () => setMenu(null);
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && close();
    window.addEventListener("click", close);
    window.addEventListener("blur", close);
    window.addEventListener("keydown", onKey);
    return () => {
      window.removeEventListener("click", close);
      window.removeEventListener("blur", close);
      window.removeEventListener("keydown", onKey);
    };
  }, [menu]);

  const spec = useMemo(() => providers.find((p) => p.kind === draft?.kind), [providers, draft?.kind]);

  const edit = (patch: Partial<Draft>) => {
    setDraft((d) => (d ? { ...d, ...patch } : d));
    setDirty(true);
    setStatus(null);
    setConfirmDelete(false);
  };
  const setSetting = (key: string, value: string) => draft && edit({ settings: { ...draft.settings, [key]: value } });
  const setSecret = (key: string, value: string) => draft && edit({ secrets: { ...draft.secrets, [key]: value } });

  const select = (d: Draft | null) => {
    setDraft(d);
    setDirty(false);
    setStatus(null);
    setConfirmDelete(false);
  };

  const visible = (f: FieldSpec) => {
    if (!f.showIf || !draft) return true;
    // `value` may list alternatives: "password|keyboard-interactive".
    const [key, value] = f.showIf;
    return value.split("|").includes(draft.settings[key] ?? "");
  };

  const missing = useMemo(() => {
    if (!draft || !spec) return [];
    const out: string[] = [];
    if (!draft.name.trim()) out.push("Name");
    for (const f of spec.fields.filter((f) => f.required && visible(f))) {
      const has =
        f.kind === "secret"
          ? (draft.secrets[f.key] ?? "") !== "" || (draft.stored.includes(f.key) && draft.secrets[f.key] !== "")
          : (draft.settings[f.key] ?? "").trim() !== "";
      if (!has) out.push(f.label);
    }
    return out;
  }, [draft, spec]);

  const run = async (label: string, fn: () => Promise<void>) => {
    setBusy(label);
    setStatus(null);
    try {
      await fn();
    } catch (e) {
      setStatus({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  };

  const persist = async (): Promise<Profile> => {
    const saved = await api.saveProfile(toInput(draft!));
    setProfiles(await api.listProfiles());
    setDraft(draftFrom(saved));
    setDirty(false);
    return saved;
  };

  const save = () =>
    run("save", async () => {
      await persist();
      setStatus({ tone: "ok", text: "Saved. Secrets are stored in the system keychain." });
    });

  const test = () =>
    run("test", async () => {
      setStatus({ tone: "info", text: "Connecting…" });
      const msg = await api.testProfile(toInput(draft!));
      setStatus({ tone: "ok", text: msg });
    });

  const signIn = () =>
    run("signin", async () => {
      const saved = dirty || !draft?.id ? await persist() : null;
      setStatus({ tone: "info", text: "Complete the sign-in in your browser…" });
      signingIn.current = true;
      try {
        const updated = await api.signInProfile(saved?.id ?? draft!.id!);
        setProfiles(await api.listProfiles());
        setDraft(draftFrom(updated));
        setStatus({ tone: "ok", text: "Signed in." });
      } catch (e) {
        if (errorMessage(e) !== SIGN_IN_CANCELLED) throw e;
        setStatus({ tone: "info", text: "Sign-in cancelled." });
      } finally {
        signingIn.current = false;
      }
    });

  const deleteProfile = (id: string) =>
    run("delete", async () => {
      await api.deleteProfile(id);
      const rest = await api.listProfiles();
      setProfiles(rest);
      // Keep editing the open profile unless it was the one deleted.
      if (!draft || draft.id === id) select(rest.length ? draftFrom(rest[0]) : null);
    });

  const remove = () => deleteProfile(draft!.id!);

  /** From the list's context menu: asks first, since secrets go with it. */
  const removeFromMenu = (p: Profile) => {
    setMenu(null);
    if (busy) return;
    const secrets = p.storedSecrets.length ? " Its saved passwords and tokens are removed from the keychain too." : "";
    if (window.confirm(`Delete the connection profile “${p.name}”?${secrets}`)) void deleteProfile(p.id);
  };

  const renderField = (f: FieldSpec) => {
    if (!draft || !visible(f)) return null;
    const value = draft.settings[f.key] ?? "";
    let control;
    switch (f.kind) {
      case "checkbox":
        return (
          <label key={f.key} className="field check">
            <input
              type="checkbox"
              checked={value === "true"}
              onChange={(e) => setSetting(f.key, String(e.target.checked))}
            />
            <span>{f.label}</span>
          </label>
        );
      case "select":
        control = (
          <select value={value} onChange={(e) => setSetting(f.key, e.target.value)}>
            {f.options.map((o) => (
              <option key={o} value={o}>
                {OPTION_LABELS[o] ?? o}
              </option>
            ))}
          </select>
        );
        break;
      case "secret": {
        const stored = draft.stored.includes(f.key);
        const cleared = draft.secrets[f.key] === "" && stored;
        control = (
          <div className="input-row">
            <input
              type="password"
              value={draft.secrets[f.key] ?? ""}
              placeholder={cleared ? "Will be removed on save" : stored ? "•••••••• saved in keychain" : f.placeholder}
              onChange={(e) => setSecret(f.key, e.target.value)}
              autoComplete="off"
            />
            {stored && !cleared && (
              <button title="Remove the saved value" onClick={() => setSecret(f.key, "")}>
                Clear
              </button>
            )}
          </div>
        );
        break;
      }
      case "file":
        control = (
          <div className="input-row">
            <input value={value} placeholder={f.placeholder} onChange={(e) => setSetting(f.key, e.target.value)} spellCheck={false} />
            <button
              className="icon-btn"
              title="Browse…"
              onClick={async () => {
                const picked = await open({ multiple: false, directory: false, title: f.label });
                if (typeof picked === "string") setSetting(f.key, picked);
              }}
            >
              <FolderOpen size={15} />
            </button>
          </div>
        );
        break;
      default:
        control = (
          <input
            type={f.kind === "number" ? "number" : "text"}
            value={value}
            placeholder={f.placeholder}
            onChange={(e) => setSetting(f.key, e.target.value)}
            spellCheck={false}
          />
        );
    }
    return (
      <label key={f.key} className="field">
        <span>
          {f.label}
          {f.required && <em className="req"> *</em>}
        </span>
        {control}
        {f.help && <small className="muted">{f.help}</small>}
      </label>
    );
  };

  const signedIn = draft?.stored.includes(REFRESH_TOKEN);
  // A token typed in this edit counts too, so Test connection works before saving.
  const pastedToken =
    !!draft && ((draft.secrets[ACCESS_TOKEN] ?? "") !== "" || (draft.stored.includes(ACCESS_TOKEN) && draft.secrets[ACCESS_TOKEN] !== ""));
  const authorized = signedIn || pastedToken;
  const builtinApp = !!spec?.oauth?.hasBuiltinClient;
  const ownApp = !!draft?.settings.client_id?.trim();
  const basicFields = spec?.fields.filter((f) => !f.advanced) ?? [];
  const advancedFields = spec?.fields.filter((f) => f.advanced) ?? [];
  const appSetup = (
    <>
      Register an app in the{" "}
      <button className="link" onClick={() => draft && api.openProviderConsole(draft.kind)}>
        developer console
      </button>{" "}
      with redirect URI <code>{redirectUri}</code>, paste its Client ID {builtinApp ? "here" : "above"}, then sign in.
    </>
  );

  return (
    <div className="profiles">
      <aside className="profile-list">
        <div className="profile-list-head">
          <strong>Connection profiles</strong>
          <button
            className="primary"
            disabled={!providers.length}
            onClick={() => select(newDraft(providers[0]))}
            title="New connection profile"
          >
            <Plus size={14} /> New
          </button>
        </div>
        {profiles.map((p) => (
          <button
            key={p.id}
            className={`profile-item${draft?.id === p.id ? " active" : ""}`}
            onClick={() => select(draftFrom(p))}
            onContextMenu={(e) => {
              e.preventDefault();
              setMenu({ x: e.clientX, y: e.clientY, profile: p });
            }}
          >
            <span className="pi-icon">
              <ProviderIcon kind={p.kind} />
            </span>
            <span className="pi-text">
              <span className="pi-name">{p.name}</span>
              <span className="muted pi-sub">{providers.find((s) => s.kind === p.kind)?.label ?? p.kind}</span>
            </span>
          </button>
        ))}
        {!profiles.length && <p className="muted pad">No profiles yet. Click “New” to connect S3, SFTP, OneDrive, SharePoint, GitHub and more.</p>}
      </aside>

      <section className="profile-edit">
        {!draft || !spec ? (
          <div className="empty-state">
            <p className="muted">Select a profile or create a new one.</p>
          </div>
        ) : (
          <>
            <h2>
              <ProviderIcon kind={draft.kind} size={18} /> {draft.id ? draft.name || "Untitled" : "New connection"}
              {dirty && <span className="muted"> — unsaved</span>}
            </h2>
            <div className="form">
              <label className="field">
                <span>
                  Name<em className="req"> *</em>
                </span>
                <input value={draft.name} onChange={(e) => edit({ name: e.target.value })} placeholder="e.g. Production S3" autoFocus={!draft.id} />
              </label>
              <label className="field">
                <span>Provider</span>
                <select
                  value={draft.kind}
                  onChange={(e) => {
                    const next = providers.find((p) => p.kind === e.target.value)!;
                    // Fields differ per vendor; keep only the name.
                    edit({ ...newDraft(next), id: draft.id, name: draft.name, stored: draft.kind === next.kind ? draft.stored : [] });
                  }}
                >
                  {providers.map((p) => (
                    <option key={p.kind} value={p.kind}>
                      {p.label}
                    </option>
                  ))}
                </select>
                <small className="muted">{spec.description}</small>
              </label>
              {basicFields.map(renderField)}
              {advancedFields.length > 0 && (
                <details className="advanced-fields" open={ownApp || undefined}>
                  <summary className="muted">{spec.oauth ? "Advanced: use your own app registration" : "Advanced"}</summary>
                  {spec.oauth && <p className="muted">{appSetup}</p>}
                  {advancedFields.map(renderField)}
                </details>
              )}
            </div>

            {spec.oauth && (
              <div className="oauth-box">
                <div>
                  <strong className={authorized ? "signed-in" : undefined}>
                    {authorized && <CircleCheck size={14} />}{" "}
                    {pastedToken ? "Using a pasted access token" : signedIn ? "Signed in" : "Not signed in"}
                  </strong>
                  <p className="muted">
                    {busy === "signin"
                      ? "Finish in the browser window that opened, or cancel to stop waiting."
                      : pastedToken
                        ? "Clear the access token to use the sign-in instead; signing in also replaces it."
                        : builtinApp && !ownApp
                      ? `Click Sign in and approve PCompare with your ${SIGN_IN_WITH[draft.kind] ?? "provider"} account — no setup needed.`
                        : builtinApp
                          ? "Signs in through your own app registration (see Advanced). Sign in again after changing it."
                          : appSetup}
                  </p>
                </div>
                {busy === "signin" ? (
                  <div className="oauth-waiting">
                    <span className="muted">Waiting for browser…</span>
                    <button onClick={() => void api.cancelSignIn()} title="Stop waiting for the browser">
                      Cancel
                    </button>
                  </div>
                ) : (
                  <button onClick={signIn} disabled={!!busy || !draft.name.trim() || (!builtinApp && !ownApp)}>
                    {signedIn ? "Sign in again" : `Sign in with ${SIGN_IN_WITH[draft.kind] ?? "provider"}…`}
                  </button>
                )}
              </div>
            )}

            {draft.id && (
              <p className="muted uri-hint">
                Use <code>{`${REMOTE_SCHEME}${draft.id}/path`}</code> in any compare, or click <Globe size={12} className="inline-icon" /> next to a path field.
              </p>
            )}

            {status && <div className={`banner ${status.tone}`}>{status.text}</div>}
            {missing.length > 0 && dirty && <div className="muted">Required: {missing.join(", ")}</div>}

            <div className="profile-actions">
              <button className="primary" onClick={save} disabled={!!busy || missing.length > 0 || !dirty}>
                {busy === "save" ? "Saving…" : "Save"}
              </button>
              <button onClick={test} disabled={!!busy || missing.length > 0 || (!!spec.oauth && !authorized)}>
                {busy === "test" ? "Testing…" : "Test connection"}
              </button>
              {dirty && draft.id && (
                <button onClick={() => select(draftFrom(profiles.find((p) => p.id === draft.id)!))} disabled={!!busy}>
                  Revert
                </button>
              )}
              <span className="spacer" />
              {draft.id &&
                (confirmDelete ? (
                  <button className="danger" onClick={remove} disabled={!!busy}>
                    Really delete “{draft.name}”?
                  </button>
                ) : (
                  <button onClick={() => setConfirmDelete(true)} disabled={!!busy}>
                    Delete
                  </button>
                ))}
            </div>
          </>
        )}
      </section>

      {menu && (
        <div className="ctx-menu" style={{ left: menu.x, top: menu.y }} onClick={(e) => e.stopPropagation()}>
          <button onClick={() => (setMenu(null), select(draftFrom(menu.profile)))}>Edit</button>
          <hr />
          <button className="danger-text" onClick={() => removeFromMenu(menu.profile)} disabled={!!busy}>
            Delete…
          </button>
        </div>
      )}
    </div>
  );
}
