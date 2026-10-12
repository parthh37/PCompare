import { useEffect, useState } from "react";
import { textApi, type EncodingChoice } from "../lib/textApi";
import "./TextEncodingPicker.css";

interface Props {
  /** The encoding chosen for this side, or null to detect it. */
  value: string | null;
  /** What the file was read as (shown on the "Auto" entry). */
  detected: string;
  hasBom: boolean;
  guessed: boolean;
  disabled?: boolean;
  /** Unsaved edits: a new encoding converts on save instead of re-reading the file. */
  dirty: boolean;
  onChange: (encoding: string | null) => void;
}

/** Per-side encoding selector for the Text Compare info bar. */
export function TextEncodingPicker({ value, detected, hasBom, guessed, disabled, dirty, onChange }: Props) {
  const [choices, setChoices] = useState<EncodingChoice[]>([]);
  useEffect(() => {
    textApi.encodings().then(setChoices, () => setChoices([]));
  }, []);
  const groups = [...new Set(choices.map((c) => c.group))];
  const current = `${detected}${hasBom ? " BOM" : ""}`;
  return (
    <select
      className="tc-encoding"
      value={value ?? ""}
      disabled={disabled || !choices.length}
      title={
        dirty
          ? "Encoding used when saving (the edited text is converted)"
          : "Encoding the file is read and saved as; picking one re-reads the file"
      }
      onChange={(e) => onChange(e.target.value || null)}
    >
      <option value="">
        Auto: {current}
        {guessed ? " (guessed)" : ""}
      </option>
      {groups.map((g) => (
        <optgroup key={g} label={g}>
          {choices
            .filter((c) => c.group === g)
            .map((c) => (
              <option key={c.id} value={c.id}>
                {c.name}
              </option>
            ))}
        </optgroup>
      ))}
    </select>
  );
}
