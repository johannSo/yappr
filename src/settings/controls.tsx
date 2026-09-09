import React, { useId, useMemo, useState } from "react";
import { AnimatePresence, motion } from "motion/react";
import { Icon } from "./icons";
import {
  COLUMN_LABELS,
  ENUMS,
  ENUM_LABELS,
  HELP,
  Json,
  Section,
  TABLE_COLUMNS,
  UNITS,
  labelFor,
} from "./schema";

/// When an edit should reach the daemon. A toggle or a dropdown is a finished
/// decision the moment it changes; a half-typed number is not, so text and
/// number fields ask to be coalesced instead. `Settings.tsx` owns the timer —
/// this file only says which kind of edit this was.
export type Commit = "now" | "later";
export type Change = (value: Json, commit: Commit) => void;

export type Device = { name: string; is_default: boolean };

/// The literal `audio.device` value meaning "whatever the system says", and
/// also `Config::default()`'s value for the field. It is not a device name, so
/// it never appears in the enumerated list and has to be offered separately.
const SYSTEM_DEFAULT_DEVICE = "default";

/// The house spring. Critically damped — no overshoot — because none of these
/// controls is thrown or flicked; they are set. Overshoot belongs to motion
/// that had momentum behind it, and a dropdown opening had none.
const SETTLE = { type: "spring", bounce: 0, duration: 0.3 } as const;
/// The one exception: a toggle knob. It is the only control here that models
/// a physical thing travelling between two stops, and a hair of overshoot is
/// what makes it read as having arrived rather than been teleported.
const KNOB = { type: "spring", bounce: 0.28, duration: 0.32 } as const;

/// A popover, not a CSS `:hover` tooltip.
///
/// Two reasons it grew a state hook. It has to open on focus as well as
/// hover, so a keyboard can reach every explanation in this window; and it
/// has to scale *from the ⓘ that opened it* rather than from its own middle,
/// which is what tells you at a glance which row the text belongs to. A
/// `transform-origin` at the trigger corner is the whole of that idea.
export function InfoTip({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  const id = useId();
  return (
    <span
      className="infotip"
      onPointerEnter={() => setOpen(true)}
      onPointerLeave={() => setOpen(false)}
    >
      {/* No click toggle. Pointer-enter and focus both open it, and a click
          on a button the pointer is already over would arrive *after* both --
          so a toggle here closed the popover on the very gesture meant to
          summon it. Clicking focuses the button, which keeps it open, which
          is also what a tap does on a touchscreen. */}
      <button
        type="button"
        className="infotip-btn"
        aria-label="Hinweis"
        aria-describedby={open ? id : undefined}
        onFocus={() => setOpen(true)}
        onBlur={() => setOpen(false)}
      >
        <Icon name="info" className="icon-sm" />
      </button>
      <AnimatePresence>
        {open && (
          <motion.span
            id={id}
            className="tip"
            role="tooltip"
            // Blur and scale together, so the panel reads as a material
            // arriving rather than a rectangle whose opacity went up.
            initial={{ opacity: 0, scale: 0.9, y: -4, filter: "blur(6px)" }}
            animate={{ opacity: 1, scale: 1, y: 0, filter: "blur(0px)" }}
            exit={{ opacity: 0, scale: 0.94, y: -4, filter: "blur(6px)" }}
            transition={SETTLE}
          >
            {text}
          </motion.span>
        )}
      </AnimatePresence>
    </span>
  );
}

export function ResetButton({ onClick }: { onClick: () => void }) {
  return (
    <motion.button
      type="button"
      className="reset"
      onClick={onClick}
      title="Auf Standard zurücksetzen"
      aria-label="Auf Standard zurücksetzen"
      // The gutter it sits in is permanently reserved (see `.row-reset`), so
      // this can scale in and out without moving anything around it.
      initial={{ opacity: 0, scale: 0.6 }}
      animate={{ opacity: 1, scale: 1 }}
      exit={{ opacity: 0, scale: 0.6 }}
      whileTap={{ scale: 0.88 }}
      transition={SETTLE}
    >
      <Icon name="reset" className="icon-sm" />
    </motion.button>
  );
}

export function Toggle({
  value,
  onChange,
}: {
  value: boolean;
  onChange: Change;
}) {
  return (
    <motion.button
      type="button"
      role="switch"
      aria-checked={value}
      className={`toggle${value ? " on" : ""}`}
      onClick={() => onChange(!value, "now")}
      // Feedback on the press, not on the release: the track dips the instant
      // a finger lands on it, before the state has changed at all.
      whileTap={{ scale: 0.94 }}
      transition={SETTLE}
    >
      <motion.span
        className="knob"
        animate={{ x: value ? 18 : 0 }}
        transition={KNOB}
      />
    </motion.button>
  );
}

/// The row every setting is rendered as: label and its ⓘ on the left, the
/// control in a fixed column on the right, the reset in a gutter that stays
/// reserved whether or not a reset is available — otherwise every row would
/// shift sideways the moment a value stopped being its default.
export function Row({
  label,
  help,
  control,
  unit,
  onReset,
  block,
  disabled,
}: {
  label: string;
  help?: string;
  control: React.ReactNode;
  unit?: string;
  onReset?: () => void;
  /** A table or list: the editor gets the full row width beneath the label. */
  block?: React.ReactNode;
  disabled?: boolean;
}) {
  return (
    <div className={`row${block ? " has-block" : ""}${disabled ? " disabled" : ""}`}>
      <div className="row-head">
        <div className="row-label">
          <span className="label-text">{label}</span>
          {help && <InfoTip text={help} />}
        </div>
        <div className="row-control">
          {control}
          {unit && <span className="unit">{unit}</span>}
        </div>
        <div className="row-reset">
          <AnimatePresence initial={false}>
            {onReset && <ResetButton key="reset" onClick={onReset} />}
          </AnimatePresence>
        </div>
      </div>
      {block && <div className="row-block">{block}</div>}
    </div>
  );
}

function EnumSelect({
  options,
  labels,
  value,
  onChange,
}: {
  options: string[];
  labels?: Record<string, string>;
  value: string;
  onChange: Change;
}) {
  // A value the enum has never heard of still has to be selectable, or opening
  // this window would silently rewrite a hand-edited config on the first save.
  // It shows its raw self: there is no label for a value nobody declared, and
  // inventing one would hide what is actually in the file.
  const known = options.includes(value);
  return (
    <div className="select">
      <select value={value} onChange={(e) => onChange(e.target.value, "now")}>
        {!known && <option value={value}>{value}</option>}
        {options.map((o) => (
          <option key={o} value={o}>
            {labels?.[o] ?? o}
          </option>
        ))}
      </select>
      <Icon name="chevron" className="select-caret" />
    </div>
  );
}

export function DeviceSelect({
  value,
  devices,
  onChange,
}: {
  value: string;
  devices: Device[];
  onChange: Change;
}) {
  const listed = devices.some((d) => d.name === value);
  return (
    <div className="select">
      <select value={value} onChange={(e) => onChange(e.target.value, "now")}>
        <option value={SYSTEM_DEFAULT_DEVICE}>Systemstandard</option>
        {/* A device configured by name that `cpal` cannot currently see — an
            unplugged interface, say — must stay selected rather than being
            quietly replaced by the first entry of the list. */}
        {!listed && value !== SYSTEM_DEFAULT_DEVICE && (
          <option value={value}>{value} (nicht gefunden)</option>
        )}
        {devices.map((d) => (
          <option key={d.name} value={d.name}>
            {d.is_default ? `${d.name} (Systemstandard)` : d.name}
          </option>
        ))}
      </select>
      <Icon name="chevron" className="select-caret" />
    </div>
  );
}

function NumberInput({ value, onChange }: { value: number; onChange: Change }) {
  // The field is held as a draft string while it has focus. Without that, a
  // cleared field would autosave as 0 between two keystrokes — with the Save
  // button gone, "briefly invalid while typing" must not mean "written".
  const [draft, setDraft] = useState<string | null>(null);
  return (
    <input
      type="number"
      // `step="any"` on purpose: a ratio like 0.25 must stay editable, and the
      // integer fields are validated by the daemon anyway.
      step="any"
      value={draft ?? String(value)}
      onChange={(e) => {
        setDraft(e.target.value);
        const next = Number(e.target.value);
        if (e.target.value !== "" && !Number.isNaN(next)) onChange(next, "later");
      }}
      onBlur={() => setDraft(null)}
    />
  );
}

export function ScalarInput({
  path,
  value,
  onChange,
}: {
  path: string;
  value: Json;
  onChange: Change;
}) {
  const options = ENUMS[path];
  if (options)
    return (
      <EnumSelect
        options={options}
        labels={ENUM_LABELS[path]}
        value={String(value)}
        onChange={onChange}
      />
    );
  if (typeof value === "boolean") return <Toggle value={value} onChange={onChange} />;
  if (typeof value === "number") return <NumberInput value={value} onChange={onChange} />;
  return (
    <input
      type="text"
      value={value === null ? "" : String(value)}
      onChange={(e) => onChange(e.target.value, "later")}
    />
  );
}

/// `vocabulary.terms`: an add-field sitting on the row itself, the entries as
/// chips beneath it. A term is a short token, so a column of full-width text
/// inputs spent a lot of space saying very little.
///
/// A component rather than a helper returning two fragments, because it holds
/// the draft in `useState`: called from one of `Field`'s branches it would
/// make that component's hook order depend on the shape of the value.
export function StringListRow({
  label,
  help,
  items,
  onReset,
  onChange,
}: {
  label: string;
  help?: string;
  items: string[];
  onReset?: () => void;
  onChange: (items: string[]) => void;
}) {
  const [draft, setDraft] = useState("");
  const add = () => {
    const trimmed = draft.trim();
    if (!trimmed) return;
    onChange([...items, trimmed]);
    setDraft("");
  };
  return (
    <Row
      label={label}
      help={help}
      onReset={onReset}
      control={
        <div className="add-inline">
          <input
            type="text"
            placeholder="Neuer Begriff"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                add();
              }
            }}
          />
          <button type="button" className="add" onClick={add} disabled={!draft.trim()}>
            Hinzufügen
          </button>
        </div>
      }
      block={
        items.length === 0 ? (
          <p className="empty">Noch keine Begriffe.</p>
        ) : (
          <div className="chips">
            {/* `layout` on each chip, so removing one from the middle closes
                the gap by sliding its neighbours rather than by snapping
                them — the row the eye was following stays followable. */}
            <AnimatePresence initial={false}>
              {items.map((item, i) => (
                <motion.span
                  className="chip"
                  key={`${item}-${i}`}
                  layout
                  initial={{ opacity: 0, scale: 0.8 }}
                  animate={{ opacity: 1, scale: 1 }}
                  exit={{ opacity: 0, scale: 0.8 }}
                  transition={SETTLE}
                >
                  {item}
                  <button
                    type="button"
                    onClick={() => onChange(items.filter((_, j) => j !== i))}
                    aria-label={`${item} entfernen`}
                  >
                    <Icon name="close" className="icon-xs" />
                  </button>
                </motion.span>
              ))}
            </AnimatePresence>
          </div>
        )
      }
    />
  );
}

export function TableEditor({
  path,
  rows,
  onChange,
}: {
  path: string;
  rows: Section[];
  onChange: (rows: Section[]) => void;
}) {
  const columns = useMemo(() => {
    const declared = TABLE_COLUMNS[path];
    if (declared) return declared;
    const seen = new Set<string>();
    rows.forEach((row) => Object.keys(row).forEach((k) => seen.add(k)));
    return [...seen];
  }, [path, rows]);

  const setCell = (index: number, column: string, value: Json) => {
    onChange(rows.map((row, i) => (i === index ? { ...row, [column]: value } : row)));
  };

  const addRow = () => {
    const blank: Section = {};
    // A brand-new row's optional columns start as null, which the config
    // writer turns into an absent key -- TOML has no null.
    columns.forEach((c) => {
      blank[c] = ENUMS[`${path}.${c}`] ? null : "";
    });
    onChange([...rows, blank]);
  };

  const gridStyle = { "--cols": columns.length } as React.CSSProperties;

  return (
    <div className="table">
      {rows.length > 0 && (
        <div className="trow head" style={gridStyle}>
          {columns.map((c) => (
            <span key={c}>{COLUMN_LABELS[c] ?? c}</span>
          ))}
          <span />
        </div>
      )}
      <AnimatePresence initial={false}>
        {rows.map((row, i) => (
          <motion.div
            className="trow"
            key={i}
            style={gridStyle}
            layout
            initial={{ opacity: 0, y: -6 }}
            animate={{ opacity: 1, y: 0 }}
            exit={{ opacity: 0, y: -6 }}
            transition={SETTLE}
          >
            {columns.map((c) => {
              const options = ENUMS[`${path}.${c}`];
              const value = row[c];
              return options ? (
                <div className="select" key={c}>
                  <select
                    value={value === null || value === undefined ? "" : String(value)}
                    onChange={(e) => setCell(i, c, e.target.value === "" ? null : e.target.value)}
                  >
                    <option value="">(vererbt)</option>
                    {options.map((o) => (
                      <option key={o} value={o}>
                        {o}
                      </option>
                    ))}
                  </select>
                  <Icon name="chevron" className="select-caret" />
                </div>
              ) : (
                <input
                  key={c}
                  type="text"
                  value={value === null || value === undefined ? "" : String(value)}
                  onChange={(e) => setCell(i, c, e.target.value)}
                />
              );
            })}
            <button
              type="button"
              className="icon-btn danger"
              onClick={() => onChange(rows.filter((_, j) => j !== i))}
              title="Zeile entfernen"
              aria-label="Zeile entfernen"
            >
              <Icon name="trash" className="icon-sm" />
            </button>
          </motion.div>
        ))}
      </AnimatePresence>
      {rows.length === 0 && <p className="empty">Noch keine Einträge.</p>}
      <div className="table-foot">
        <button type="button" className="add" onClick={addRow}>
          <Icon name="plus" className="icon-sm" />
          Zeile hinzufügen
        </button>
      </div>
    </div>
  );
}

/// One config key, dispatched on its JSON type. The three shapes it can take —
/// scalar, string list, table of objects — are the three the config can hold.
export function Field({
  section,
  fieldKey,
  value,
  devices,
  canReset,
  onChange,
  onReset,
}: {
  section: string;
  fieldKey: string;
  value: Json;
  devices: Device[];
  canReset: boolean;
  onChange: Change;
  onReset: () => void;
}) {
  const path = `${section}.${fieldKey}`;
  const label = labelFor(path, fieldKey);
  const help = HELP[path];
  const unit = UNITS[path];
  const reset = canReset ? onReset : undefined;

  if (path === "audio.device") {
    return (
      <Row
        label={label}
        help={help}
        onReset={reset}
        control={
          <DeviceSelect value={String(value)} devices={devices} onChange={onChange} />
        }
      />
    );
  }

  if (Array.isArray(value)) {
    // An *empty* array carries no shape, and `[].every(...)` is vacuously
    // true — so "does every element look like an object?" answers "yes" for
    // an empty `terms` list and would render it as a column-less table. The
    // declared columns decide first; only a non-empty array is allowed to
    // speak for itself.
    const isTable =
      TABLE_COLUMNS[path] !== undefined ||
      (value.length > 0 &&
        value.every((v) => v !== null && typeof v === "object" && !Array.isArray(v)));

    if (isTable) {
      return (
        <Row
          label={label}
          help={help}
          onReset={reset}
          control={null}
          block={
            <TableEditor
              path={path}
              rows={value as Section[]}
              onChange={(rows) => onChange(rows as Json, "now")}
            />
          }
        />
      );
    }

    return (
      <StringListRow
        label={label}
        help={help}
        items={value.map(String)}
        onReset={reset}
        onChange={(items) => onChange(items as Json, "now")}
      />
    );
  }

  return (
    <Row
      label={label}
      help={help}
      unit={unit}
      onReset={reset}
      control={<ScalarInput path={path} value={value} onChange={onChange} />}
    />
  );
}
