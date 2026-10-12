import { KeyRound } from "lucide-react";
import { useEffect, useState } from "react";
import { remoteApi, type AuthPromptEvent } from "../lib/remoteApi";
import { Modal } from "./Modal";
import "./Remote.css";

/**
 * Answers an SSH server's sign-in questions — a one-time code, or a password
 * that wasn't saved or was rejected. Connections wait (up to a few minutes)
 * while the dialog is open; questions from several connections queue up.
 */
export function RemoteAuthPrompt() {
  const [queue, setQueue] = useState<AuthPromptEvent[]>([]);
  const [answers, setAnswers] = useState<string[]>([]);

  useEffect(() => {
    let cancelled = false;
    let unlisten: (() => void) | undefined;
    remoteApi
      .onAuthPrompt((e) => setQueue((q) => [...q, e]))
      .then((u) => (cancelled ? u() : (unlisten = u)))
      .catch(() => {});
    return () => {
      cancelled = true;
      unlisten?.();
    };
  }, []);

  const current = queue[0];
  useEffect(() => {
    setAnswers(current ? current.prompt.fields.map(() => "") : []);
  }, [current]);

  if (!current) return null;
  const { prompt } = current;
  const finish = (reply: string[] | null) => {
    remoteApi.answerAuth(current.id, reply).catch(() => {});
    setQueue((q) => q.slice(1));
  };

  return (
    <Modal
      title={`Sign in to ${prompt.profile}`}
      onClose={() => finish(null)}
      footer={
        <>
          <span className="spacer" />
          <button onClick={() => finish(null)}>Cancel</button>
          <button className="primary" type="submit" form="remote-auth-form">
            Continue
          </button>
        </>
      }
    >
      <form
        id="remote-auth-form"
        className="form remote-auth"
        onSubmit={(e) => {
          e.preventDefault();
          finish(answers);
        }}
      >
        <p className="muted">
          <KeyRound size={13} className="inline-icon" /> {prompt.target} asks:
        </p>
        {prompt.name && <strong>{prompt.name}</strong>}
        {prompt.instructions && <p className="remote-auth-instructions">{prompt.instructions}</p>}
        {prompt.fields.map((f, i) => (
          <label key={i} className="field">
            <span>{f.prompt.trim() || "Response"}</span>
            <input
              type={f.echo ? "text" : "password"}
              value={answers[i] ?? ""}
              autoFocus={i === 0}
              autoComplete="off"
              spellCheck={false}
              onChange={(e) => setAnswers((a) => a.map((v, j) => (j === i ? e.target.value : v)))}
            />
          </label>
        ))}
      </form>
    </Modal>
  );
}
