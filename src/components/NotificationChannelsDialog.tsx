// Notification channels for Sync Automation: Microsoft Teams, Slack, email
// (SMTP) and generic webhooks. Secrets (webhook URLs, SMTP password, signing
// key) live in the OS keychain; this dialog only learns whether one is saved.
import { CircleCheck, CircleX, LoaderCircle, Mail, MessageSquare, Plus, Send, Trash, Users, Webhook, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useState, type ReactNode } from "react";
import { errorMessage } from "../lib/api";
import {
  automationApi,
  type ChannelSecrets,
  type ChannelSettings,
  type ChannelWithSecrets,
  type EmailSecurity,
  type SecretsPresent,
} from "../lib/automationApi";
import { splitList } from "../lib/automationFormat";
import { Modal } from "./Modal";
import "./Automation.css";

export type ChannelKind = ChannelSettings["kind"];

/** Generic glyphs tinted with each service's colour (no trademarked logos). */
const KIND_INFO: Record<ChannelKind, { label: string; icon: typeof Mail; color: string }> = {
  teams: { label: "Microsoft Teams", icon: Users, color: "#7b83eb" },
  slack: { label: "Slack", icon: MessageSquare, color: "#e01e5a" },
  email: { label: "Email (SMTP)", icon: Mail, color: "#38bdf8" },
  webhook: { label: "Webhook", icon: Webhook, color: "#a3e635" },
};

export const channelKindLabel = (k: ChannelKind) => KIND_INFO[k].label;

export function ChannelIcon({ kind, size = 15 }: { kind: ChannelKind; size?: number }) {
  const { icon: Icon, color } = KIND_INFO[kind];
  return <Icon size={size} color={color} aria-hidden />;
}

/** One line under a channel's name: recipients, or whether its URL is saved. */
export function channelSummary(c: ChannelWithSecrets): string {
  const s = c.channel.settings;
  if (s.kind === "email") {
    const to = s.to.join(", ");
    return to ? `To ${to}` : "No recipients";
  }
  return c.secrets.url ? "Webhook URL saved" : "No webhook URL yet";
}

interface Draft {
  /** "" until saved. */
  id: string;
  name: string;
  enabled: boolean;
  settings: ChannelSettings;
  /** Typed this edit: undefined = keep, "" = remove. */
  secrets: ChannelSecrets;
  present: SecretsPresent;
  /** Email recipients as typed (split on save). */
  toText: string;
  ccText: string;
}

const NO_SECRETS: SecretsPresent = { url: false, password: false, signingKey: false };

function draftFrom(c: ChannelWithSecrets): Draft {
  const s = c.channel.settings;
  return {
    id: c.channel.id,
    name: c.channel.name,
    enabled: c.channel.enabled,
    settings: s,
    secrets: {},
    present: c.secrets,
    toText: s.kind === "email" ? s.to.join(", ") : "",
    ccText: s.kind === "email" ? s.cc.join(", ") : "",
  };
}

function newDraft(kind: ChannelKind): Draft {
  const settings: ChannelSettings =
    kind === "email"
      ? { kind, host: "", port: 0, security: "startTls", username: "", from: "", to: [], cc: [] }
      : kind === "webhook"
        ? { kind, headers: [] }
        : { kind };
  return { id: "", name: KIND_INFO[kind].label, enabled: true, settings, secrets: {}, present: NO_SECRETS, toText: "", ccText: "" };
}

/** The settings as they'll be saved (recipient lists parsed, blank header rows dropped). */
function settingsOf(d: Draft): ChannelSettings {
  const s = d.settings;
  if (s.kind === "email") return { ...s, to: splitList(d.toText), cc: splitList(d.ccText) };
  if (s.kind === "webhook") return { ...s, headers: s.headers.filter(([k]) => k.trim()) };
  return s;
}

/** Whether a secret will exist after saving. */
const willHave = (d: Draft, key: keyof SecretsPresent) => {
  const typed = d.secrets[key];
  return typed == null ? d.present[key] : typed !== "";
};

const DEFAULT_PORTS: Record<EmailSecurity, number> = { tls: 465, startTls: 587, none: 25 };

type Status = { tone: "ok" | "error" | "info"; text: string } | null;

interface Props {
  onClose: () => void;
  /** Channels were added, changed or deleted. */
  onChanged?: (channels: ChannelWithSecrets[]) => void;
}

/** Add, edit, test and delete notification channels. */
export function NotificationChannelsDialog({ onClose, onChanged }: Props) {
  const [channels, setChannels] = useState<ChannelWithSecrets[] | null>(null);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [dirty, setDirty] = useState(false);
  const [busy, setBusy] = useState<"save" | "test" | "delete" | null>(null);
  const [status, setStatus] = useState<Status>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [adding, setAdding] = useState(false);

  const reload = useCallback(async () => {
    const list = await automationApi.channels();
    setChannels(list);
    onChanged?.(list);
    return list;
  }, [onChanged]);

  useEffect(() => {
    automationApi.channels().then(
      (list) => {
        setChannels(list);
        if (list.length) setDraft(draftFrom(list[0]));
      },
      (e) => {
        setChannels([]);
        setStatus({ tone: "error", text: errorMessage(e) });
      },
    );
  }, []);

  const select = (d: Draft | null) => {
    if (dirty && !window.confirm("Discard the unsaved changes to this channel?")) return;
    setDraft(d);
    setDirty(false);
    setStatus(null);
    setConfirmDelete(false);
    setAdding(false);
  };

  const edit = (patch: Partial<Draft>) => {
    setDraft((d) => (d ? { ...d, ...patch } : d));
    setDirty(true);
    setStatus(null);
    setConfirmDelete(false);
  };
  const editSettings = (patch: Partial<Extract<ChannelSettings, { kind: "email" }>> | Partial<ChannelSettings>) =>
    draft && edit({ settings: { ...draft.settings, ...patch } as ChannelSettings });
  const setSecret = (key: keyof ChannelSecrets, value: string | undefined) =>
    draft && edit({ secrets: { ...draft.secrets, [key]: value } });

  const missing = useMemo(() => {
    if (!draft) return [];
    const out: string[] = [];
    if (!draft.name.trim()) out.push("Name");
    const s = draft.settings;
    if (s.kind !== "email" && !willHave(draft, "url")) out.push("Webhook URL");
    if (s.kind === "email") {
      if (!s.host.trim()) out.push("SMTP server");
      if (!s.from.trim()) out.push("From");
      if (!splitList(draft.toText).length) out.push("To");
    }
    return out;
  }, [draft]);

  const persist = async (d: Draft): Promise<string> => {
    const saved = await automationApi.saveChannel(d.id, d.name.trim(), d.enabled, settingsOf(d), d.secrets);
    const list = await reload();
    const fresh = list.find((c) => c.channel.id === saved.id);
    setDraft(fresh ? draftFrom(fresh) : { ...d, id: saved.id, secrets: {} });
    setDirty(false);
    return saved.id;
  };

  const run = async (kind: "save" | "test" | "delete", fn: () => Promise<void>) => {
    setBusy(kind);
    try {
      await fn();
    } catch (e) {
      setStatus({ tone: "error", text: errorMessage(e) });
    } finally {
      setBusy(null);
    }
  };

  const save = () =>
    run("save", async () => {
      await persist(draft!);
      setStatus({ tone: "ok", text: "Saved. Webhook URLs and passwords are stored in the system keychain." });
    });

  // Unsaved edits are saved first, so the test uses what's on screen.
  const test = () =>
    run("test", async () => {
      const id = dirty || !draft!.id ? await persist(draft!) : draft!.id;
      setStatus({ tone: "info", text: "Sending a test notification…" });
      await automationApi.testChannel(id);
      setStatus({ tone: "ok", text: "Test notification sent. Check that it arrived." });
    });

  const remove = () =>
    run("delete", async () => {
      await automationApi.deleteChannel(draft!.id);
      const list = await reload();
      setDirty(false);
      setDraft(list.length ? draftFrom(list[0]) : null);
      setConfirmDelete(false);
      setStatus(null);
    });

  const close = () => {
    if (dirty && !window.confirm("Discard the unsaved changes to this channel?")) return;
    onClose();
  };

  /** A keychain-backed secret: blank keeps the saved value; "Remove" clears it on save. */
  const secretField = (key: keyof SecretsPresent, label: string, placeholder: string, help?: ReactNode) => {
    if (!draft) return null;
    const typed = draft.secrets[key];
    const stored = draft.present[key];
    const removing = stored && typed === "";
    return (
      <label className="field">
        <span>{label}</span>
        <div className="input-row">
          <input
            type="password"
            value={typed ?? ""}
            placeholder={removing ? "Will be removed when you save" : stored ? "Saved — leave blank to keep" : placeholder}
            onChange={(e) => setSecret(key, e.target.value === "" && stored ? undefined : e.target.value)}
            autoComplete="off"
            spellCheck={false}
          />
          {stored && !removing && (
            <button title="Remove the saved value when you save" onClick={() => setSecret(key, "")}>
              Remove
            </button>
          )}
          {removing && (
            <button title="Keep the saved value" onClick={() => setSecret(key, undefined)}>
              Undo
            </button>
          )}
        </div>
        {help && <small className="muted">{help}</small>}
      </label>
    );
  };

  const renderKindFields = () => {
    if (!draft) return null;
    const s = draft.settings;
    switch (s.kind) {
      case "teams":
        return (
          <>
            {secretField("url", "Workflow webhook URL", "https://…/workflows/…/triggers/manual/paths/invoke?…")}
            <div className="auto-howto">
              <b>Getting the URL in Teams</b>
              <ol>
                <li>In the channel, open <b>⋯ › Workflows</b> (or the Workflows app).</li>
                <li>
                  Choose the template <b>“Post to a channel when a webhook request is received”</b>, pick the team and
                  channel, and finish.
                </li>
                <li>Copy the URL it shows and paste it above. Legacy “Incoming Webhook” connector URLs work too.</li>
              </ol>
            </div>
          </>
        );
      case "slack":
        return secretField(
          "url",
          "Incoming webhook URL",
          "https://hooks.slack.com/services/…",
          <>In Slack: create an app › Incoming Webhooks › Add New Webhook to Workspace, pick a channel, copy the URL.</>,
        );
      case "webhook":
        return (
          <>
            {secretField(
              "url",
              "URL",
              "https://example.com/hooks/pcompare",
              "PCompare POSTs a JSON summary of the run to this address.",
            )}
            {secretField(
              "signingKey",
              "Signing secret (optional)",
              "Shared secret for an HMAC-SHA256 signature",
              "When set, each request carries a signature header your service can verify.",
            )}
            <div className="field">
              <span>Extra headers</span>
              {s.headers.map(([k, v], i) => (
                <div className="input-row auto-header-row" key={i}>
                  <input
                    value={k}
                    placeholder="Header"
                    spellCheck={false}
                    onChange={(e) =>
                      editSettings({ headers: s.headers.map((h, j) => (j === i ? [e.target.value, h[1]] : h)) })
                    }
                  />
                  <input
                    value={v}
                    placeholder="Value"
                    spellCheck={false}
                    onChange={(e) =>
                      editSettings({ headers: s.headers.map((h, j) => (j === i ? [h[0], e.target.value] : h)) })
                    }
                  />
                  <button
                    className="icon-btn"
                    title="Remove header"
                    onClick={() => editSettings({ headers: s.headers.filter((_, j) => j !== i) })}
                  >
                    <X size={14} />
                  </button>
                </div>
              ))}
              <div>
                <button onClick={() => editSettings({ headers: [...s.headers, ["", ""]] })}>
                  <Plus size={14} /> Add header
                </button>
              </div>
            </div>
          </>
        );
      case "email":
        return (
          <>
            <div className="field-row">
              <label className="field auto-grow2">
                <span>
                  SMTP server<em className="req"> *</em>
                </span>
                <input
                  value={s.host}
                  placeholder="smtp.office365.com"
                  spellCheck={false}
                  onChange={(e) => editSettings({ host: e.target.value })}
                />
              </label>
              <label className="field">
                <span>Security</span>
                <select value={s.security} onChange={(e) => editSettings({ security: e.target.value as EmailSecurity })}>
                  <option value="startTls">STARTTLS</option>
                  <option value="tls">TLS (implicit)</option>
                  <option value="none">None (not encrypted)</option>
                </select>
              </label>
              <label className="field auto-port">
                <span>Port</span>
                <input
                  type="number"
                  min={0}
                  max={65535}
                  value={s.port || ""}
                  placeholder={String(DEFAULT_PORTS[s.security])}
                  onChange={(e) => editSettings({ port: Math.max(0, Math.min(65535, Math.round(Number(e.target.value) || 0))) })}
                />
              </label>
            </div>
            {s.security === "none" && (
              <p className="sync-confirm-warn auto-inline-warn">The password and messages are sent unencrypted.</p>
            )}
            <div className="field-row">
              <label className="field">
                <span>Username</span>
                <input
                  value={s.username}
                  placeholder="alerts@contoso.com"
                  spellCheck={false}
                  autoComplete="off"
                  onChange={(e) => editSettings({ username: e.target.value })}
                />
              </label>
              {secretField("password", "Password", "Password or app password")}
            </div>
            <label className="field">
              <span>
                From<em className="req"> *</em>
              </span>
              <input
                value={s.from}
                placeholder="PCompare <alerts@contoso.com>"
                spellCheck={false}
                onChange={(e) => editSettings({ from: e.target.value })}
              />
            </label>
            <label className="field">
              <span>
                To<em className="req"> *</em>
              </span>
              <textarea
                rows={2}
                value={draft.toText}
                placeholder="it-team@contoso.com, ops@contoso.com"
                spellCheck={false}
                onChange={(e) => edit({ toText: e.target.value })}
              />
              <small className="muted">Separate addresses with commas or new lines.</small>
            </label>
            <label className="field">
              <span>Cc</span>
              <input
                value={draft.ccText}
                spellCheck={false}
                onChange={(e) => edit({ ccText: e.target.value })}
              />
            </label>
            <small className="muted auto-smtp-tips">
              Microsoft 365: <code>smtp.office365.com</code>, port 587, STARTTLS (SMTP AUTH must be enabled for the
              mailbox). Gmail: <code>smtp.gmail.com</code>, 587, STARTTLS, with an app password (Google Account ›
              Security › App passwords).
            </small>
          </>
        );
    }
  };

  return (
    <Modal
      title="Notification channels"
      className="auto-channels"
      onClose={close}
      footer={<button onClick={close}>Close</button>}
    >
      <div className="auto-channels-body">
        <aside className="auto-channel-list">
          {channels?.map((c) => (
            <button
              key={c.channel.id}
              className={`auto-channel-item${draft?.id === c.channel.id ? " active" : ""}`}
              onClick={() => draft?.id !== c.channel.id && select(draftFrom(c))}
            >
              <ChannelIcon kind={c.channel.settings.kind} size={16} />
              <span className="auto-channel-text">
                <span className="auto-channel-name">
                  {c.channel.name}
                  {!c.channel.enabled && <span className="badge">Off</span>}
                </span>
                <span className="muted auto-channel-sub">{channelSummary(c)}</span>
              </span>
            </button>
          ))}
          {channels?.length === 0 && !draft && (
            <p className="muted auto-channel-empty">No channels yet. Add one to hear about failed syncs.</p>
          )}
          {draft && !draft.id && (
            <div className="auto-channel-item active">
              <ChannelIcon kind={draft.settings.kind} size={16} />
              <span className="auto-channel-text">
                <span className="auto-channel-name">{draft.name || "New channel"}</span>
                <span className="muted auto-channel-sub">Not saved yet</span>
              </span>
            </div>
          )}
          <div className="auto-channel-add">
            {adding ? (
              (Object.keys(KIND_INFO) as ChannelKind[]).map((k) => (
                <button key={k} onClick={() => select(newDraft(k))}>
                  <ChannelIcon kind={k} size={14} /> {KIND_INFO[k].label}
                </button>
              ))
            ) : (
              <button className="primary" onClick={() => setAdding(true)}>
                <Plus size={14} /> Add channel
              </button>
            )}
          </div>
        </aside>

        <section className="auto-channel-edit">
          {!draft ? (
            <div className="empty-state">
              {channels === null ? "Loading…" : "Add a channel: Microsoft Teams, Slack, email or a webhook."}
            </div>
          ) : (
            <>
              <h3 className="auto-channel-title">
                <ChannelIcon kind={draft.settings.kind} size={17} /> {KIND_INFO[draft.settings.kind].label}
                {dirty && <span className="muted"> — unsaved</span>}
              </h3>
              <div className="field-row">
                <label className="field auto-grow2">
                  <span>
                    Name<em className="req"> *</em>
                  </span>
                  <input
                    value={draft.name}
                    onChange={(e) => edit({ name: e.target.value })}
                    placeholder="e.g. IT alerts"
                    autoFocus={!draft.id}
                  />
                </label>
                <label className="field check auto-enabled">
                  <input type="checkbox" checked={draft.enabled} onChange={(e) => edit({ enabled: e.target.checked })} />
                  Enabled
                </label>
              </div>
              {renderKindFields()}

              {status && (
                <div className={`banner ${status.tone} auto-channel-status`}>
                  {status.tone === "ok" ? (
                    <CircleCheck size={14} />
                  ) : status.tone === "error" ? (
                    <CircleX size={14} />
                  ) : (
                    <LoaderCircle size={14} className="sync-spin" />
                  )}{" "}
                  {status.text}
                </div>
              )}
              {missing.length > 0 && dirty && <div className="muted auto-missing">Required: {missing.join(", ")}</div>}

              <div className="auto-channel-actions">
                <button className="primary" onClick={save} disabled={!!busy || !dirty || missing.length > 0}>
                  {busy === "save" ? "Saving…" : "Save"}
                </button>
                <button
                  onClick={test}
                  disabled={!!busy || missing.length > 0}
                  title={dirty || !draft.id ? "Saves your changes, then sends a sample notification" : "Send a sample notification"}
                >
                  {busy === "test" ? <LoaderCircle size={14} className="sync-spin" /> : <Send size={14} />} Send test
                </button>
                {dirty && draft.id && (
                  <button
                    disabled={!!busy}
                    onClick={() => {
                      const c = channels?.find((x) => x.channel.id === draft.id);
                      if (c) {
                        setDraft(draftFrom(c));
                        setDirty(false);
                        setStatus(null);
                      }
                    }}
                  >
                    Revert
                  </button>
                )}
                <span className="spacer" />
                {draft.id &&
                  (confirmDelete ? (
                    <>
                      <button className="danger" onClick={remove} disabled={!!busy}>
                        Delete “{draft.name}”
                      </button>
                      <button onClick={() => setConfirmDelete(false)}>Keep</button>
                    </>
                  ) : (
                    <button onClick={() => setConfirmDelete(true)} disabled={!!busy} title="Tasks using it stop sending here">
                      <Trash size={14} /> Delete
                    </button>
                  ))}
                {!draft.id && (
                  <button onClick={() => select(channels?.length ? draftFrom(channels[0]) : null)}>Discard</button>
                )}
              </div>
            </>
          )}
        </section>
      </div>
    </Modal>
  );
}
