# German finetuning dataset for S1-mini (yappr)

Every row must survive yappr's own runtime checks. Validate with:

    cargo run -q -p yappr-core --example validate_dataset -- <your-file>.jsonl

Exit code 0 and "0 bad" is the only acceptable result. The validator uses the
real `guardrail::evaluate`, `finish::finish` and `WhatlangDetector`, not a copy.

## Row format (JSONL, one object per line)

    {"styling":"casual","structure":"prose","context":"general",
     "raw":"<what the ASR emitted>","cleaned":"<what should be typed>"}

- styling:   casual | semi-casual | semi-formal | formal
- structure: prose | lists
- context:   general | email

## The `raw` side must look like real Parakeet TDT 0.6b v3 German output

This was measured on 26 real German clips. Do not invent a different style.

1. **Already punctuated and capitalized.** Sentence-shaped German. Starts
   uppercase, ends with `.` `?` or `!`. German nouns are ALREADY capitalized
   correctly. Do not write lowercase unpunctuated text.
2. **Restricted character set.** The ASR vocabulary can only emit these
   non-alphanumeric characters:  `,` `.` `-` `'` `?` `!` `:` `%` `/` `$`
   and the strings EUR GBP USD.
   NEVER put quotation marks, semicolons, parentheses, brackets, em dashes
   or ellipses in `raw`. They are physically impossible.
   The `cleaned` side MAY use them - that is one thing the model adds.
3. **Fillers are kept verbatim** by the ASR: "äh", "ähm", "also", "ja", "ne",
   "halt", "quasi", "genau", plus stutter repetitions ("wegen der wegen der").
4. **Numbers are usually words, not digits.** "um neun Uhr", "sieben Tagen",
   "ca. zwei tausend Euro". Digits appear sometimes for years and prices.
   Never write "14:30" in raw. Mix both forms across the dataset.
   Note the German ones-before-tens order the ASR faithfully transcribes:
   "siebenundachtzig" is 87, not 78. That inversion is the single thing the
   model got wrong most often before `part-g-numbers.jsonl` existed.
5. **Realistic ASR damage** to fix, sprinkle these in:
   - compound splitting: "Holz Dieb", "zwei tausend", "Fair Play"
   - plausible wrong words: "Lastschrift" heard as "Löffel"
   - truncated fragments, occasional lowercase fragment start
   - English leakage on unclear audio
   - contractions: "Gibt's"

## The `cleaned` side

- Correct written German. Proper commas. May use quotes, parentheses, dashes,
  semicolons, real digits, "z. B." etc.
- Must be in FINISHED form: starts with a capital letter or a digit, and ends
  with `.` `!` `?` or `…`. `finish::finish(cleaned)` must be a no-op.
- Never contains `[Styling:`, `[Structure:`, `[Context:`, `<think>`,
  `<|im_start|>`.
- NEVER empty. yappr rejects an empty reply unconditionally.

## Numbers on the `cleaned` side

A 0.75B model cannot compute, only recall, so it may only convert the forms
the dataset saturates. Everything else stays a number word -- writing "13:30"
for "vierzehn Uhr dreißig" is far worse than leaving the words alone, and that
is what it did before `part-g-numbers.jsonl` pinned the table.

Convert:
- **hour + "Uhr" [+ minute]** -> `14 Uhr`, `14:30 Uhr`. All 24 hour words and
  every common minute word appear at least once in `part-g-numbers.jsonl`;
  extend that file rather than relying on the model to generalise.
- **percentages** -> `87 Prozent`.
- **ordinal + month** -> `31. März`.
- **years** -> `2024`.
- **money** -> `2.800 Euro`, with a German thousands **point**. Never `2,800`;
  the comma is the decimal separator in German and that output is wrong by a
  factor of a thousand.

Leave as words:
- **relative clock times** -- "halb drei", "viertel nach acht", "drei viertel
  sieben". "halb drei" is 2:30, but only if the speaker meant the afternoon;
  nothing in the transcript settles that, and a wrong guess is worse than no
  conversion.
- small counts that read naturally as words: "zu dritt", "drei Eier".

## HARD CONSTRAINT: the guardrail (this is what fails rows)

Tokenize = split on every non-alphanumeric char, lowercase.
For German the thresholds are:

- **overlap >= 0.70** - at least 70% of the RAW tokens must still appear in
  `cleaned`, counted with multiplicity. THIS IS THE ONE THAT BITES.
- **word ratio in [0.55, 1.80]** - len(cleaned tokens) / len(raw tokens).
- no 6-token sequence repeated 3 or more times.
- Rows whose raw has fewer than 4 tokens skip the ratio and overlap checks.

Practical consequence: **you may delete at most ~30% of the raw words.**
Aim for overlap >= 0.78 to leave margin.

That means:
- Keep filler density in `raw` at about 15-25%, not 50%.
- Converting spoken numbers to digits DESTROYS tokens. "vierzehn Uhr dreißig"
  (3 tokens) -> "14:30 Uhr" (3 tokens, 1 matching). Keeping the word "Uhr" in
  `cleaned` buys back one of them and is the idiomatic German form anyway, so
  write "um 14:30 Uhr", never "um 14:30".
- du -> Sie conversion for formal styling costs 4-6 tokens no matter how it is
  written, because the pronoun, the verb ending and usually the greeting all
  move at once. On a short utterance that is fatal: "Kannst du das nochmal
  checken, ich komm da nicht weiter" -> "Können Sie das noch einmal prüfen?
  Ich komme da nicht weiter" scores **0.55** and is rejected, correct though
  it is. Only write du -> Sie rows of roughly 20 tokens or more, where the
  rest of the sentence carries the overlap.

  This was nearly "fixed" by giving formal styling its own, looser overlap
  floor. That is the wrong trade and the idea should stay dead: the floor
  never caught the failure it would have been loosened for. A half-converted
  "Kannst **Sie** das nochmal checken" changes exactly one token and sails
  through at any threshold, while what a lower floor does let in is the
  hallucination the floor exists to stop. Pronoun and verb move together
  because the *training data* pairs them, not because a threshold allows it.

## What the style axes must actually do (German)

- **casual**: du-form, contractions, relaxed. "Hey, kannst du mal kurz..."
- **semi-casual**: du-form but tidy, no slang. The default.
- **semi-formal**: neutral, Sie-form, no contractions.
- **formal**: Sie-form, full sentences, formal vocabulary, no contractions.
  For email: "Sehr geehrte Frau X", "Mit freundlichen Grüßen".
- **prose**: flowing sentences.
- **lists**: when the speaker enumerates, emit real list lines separated by
  `\n`, e.g. "- Punkt eins\n- Punkt zwei". Keep the words so overlap holds.

  German enumerations usually *elide* the shared part -- "Wir haben A
  trainiert, haben B gegessen, haben C gelernt und sind D gegangen" states the
  subject once and the auxiliary three times. Which of the two list shapes is
  correct depends on whether the auxiliary changes:

  - **Subject and auxiliary shared throughout** -> lift both into a lead-in
    and leave fragments underneath:
    `"Ich muss noch:\n- die Rechnungen schreiben\n- die Belege sortieren."`
  - **The auxiliary changes** (`haben ... haben ... und *sind*`) -> a lead-in
    cannot govern the odd one out, so every item becomes a full clause with
    the subject repeated:
    `"- Wir haben A trainiert.\n- Wir haben B gegessen.\n- Wir sind D gegangen."`
  - **The subject changes** -> repeat nothing; each item keeps its own
    subject. Copying the first one over the others is the same bug in reverse.

  Three ways this used to break, all of them in `part-h-enumeration.jsonl`
  now: the last item emitted as a copy of the previous one plus the
  remainder, the first item swallowed into the lead-in, and the odd
  auxiliary silently dropped ("Ich habe: ... - danach ins Meeting").
- **general**: plain text.
- **email**: greeting and sign-off shaping, subject-like phrasing.

Note: a `cleaned` value containing newlines is fine; write them as `\n`
inside the JSON string.

## Content domains

Everyday work dictation: chat messages, notes to self, todos, emails to
colleagues and clients, meeting notes, commit messages, code comments,
appointments, shopping and travel logistics, customer support replies.
Realistic German office and developer vocabulary, including anglicisms
("Meeting", "Deployment", "Call", "Feedback", "Ticket", "Branch").

## Quality bar

- No duplicated `raw` values across the file.
- Vary length: some 5-token utterances, most 15-40 tokens, a few 60+.
- Vary the ASR damage; not every row needs a filler.
- Real, specific content. No "Lorem ipsum", no "Beispiel eins, Beispiel zwei".

## The parts

`build_dataset.py` globs `part-*.jsonl`, so a part is just a file; the split
exists so a class can be reviewed, revalidated or dropped on its own.

| File | Rows | What it is for |
|---|---|---|
| `part-a-casual-prose.jsonl` | 90 | the base distribution |
| `part-b-formal-prose.jsonl` | 90 | " |
| `part-c-email-prose.jsonl` | 100 | " |
| `part-d-lists-general.jsonl` | 100 | " |
| `part-e-lists-email.jsonl` | 90 | " |
| `part-f-edge.jsonl` | 80 | self-corrections, stutters, filler-only, all 16 axis combinations |
| `part-g-numbers.jsonl` | 80 | the saturated clock/percent/date/money table |
| `part-h-enumeration.jsonl` | 46 | elided subjects and auxiliaries in enumerations |
| `part-i-formal-sie.jsonl` | 45 | du -> Sie with the verb, plus contrast rows that keep du |
| `part-j-fidelity.jsonl` | 65 | leaving correct words alone; colloquial verbs, particles, proper nouns, blunt words |
| `part-k-base-broad.jsonl` | 100 | breadth: the six thinnest axis combinations, commit messages and code comments, support replies, very long and very short dictations |
| `part-l-numbers-deep.jsonl` | 40 | Euro decimals with a German comma (`12,90 Euro`), `von 10 bis 12 Uhr` ranges, more ones-before-tens percents, more stay-as-words counterexamples |
| `part-m-hardcase-mix.jsonl` | 60 | second helping of g-j: more auxiliary switches, blunt words in new positions, anglicism verbs kept verbatim (gerebased, deployt, gemockt), more du -> Sie verb pairs, `lists` rows with nothing to enumerate |
| `part-n-residuals.jsonl` | 34 | what the v1->v2 retune left broken: bare-participle items in enumerations ("haben gelernt" without an object -- the shape that still duplicates the last bullet), the "antwortet wieder Müll" attractor with article subjects, and the observed digit attractors (vierzehn->13, sechzehn->12/17, neun Uhr fünfzehn->9:515) in fresh contexts, including 13/14 contrasted in one sentence |

| `part-o-v3-residuals.jsonl` | 18 | what the v3 retune left broken: four-item chains with a lead-in whose last item switches to "sind" (v3 drops the auxiliary there), anti-duplication rows for three-item chains (the x3 weight made v3 over-list them), three more "siebenundachtzig" contexts including 78/87 contrasted in one sentence, and "halt" next to "quasi" |

Parts g through o exist because of measured failures, not guesses -- see
`probes/`. Each was written after reproducing the failure it targets.
`build_dataset.py` refuses to build if any training `raw` equals a probe.

### Oversampling (`PART_WEIGHTS` in `build_dataset.py`)

The v2 evaluation showed why weights exist: after 3 epochs at 1e-5, v2
reproduces its own part-l/j/h rows perfectly but answers "78 Prozent" to an
unseen "siebenundachtzig" and converts du to "könntest du" -- everything that
must *override* a base-model prior was memorised per row instead of learned.
Digit and pronoun tokens are a rounding error in the average loss, so the
number and Sie-conversion parts are repeated verbatim on the train side
(g, l x4; i x3; n, o x2), after the val split; `val.jsonl` stays unweighted
and duplicate-free. If a later model overshoots, lower these before touching
the learning rate -- it has already happened once: part-n at x3 made v3
bullet and duplicate three-item chains that v2 handled as prose, which is why
n sits at x2 and part-o exists.

## The probes (`probes/`)

69 inputs, grouped by failure class, that are **not** training data and must
never be copied into a part: a probe the model has been trained on stops
measuring anything. Five were added after the v2 evaluation (the capitalised
version of the original screenshot failure, two Müll variants, two long-form
du -> Sie inputs); their v2 answers are recorded in
`after-v2-2026-08-31.json`, so `--diff` covers them from v2 onward while
`baseline-2026-08-31.json` predates them and skips them.

    llama-server -m <model>.gguf --port 8899 -c 4096
    python3 probes/run.py --out after.json
    python3 probes/run.py --diff probes/baseline-2026-08-31.json after.json

`run.py` renders the prompt byte-identically to `normalize::render_chat_prompt`
(reading `SYSTEM_PROMPT` out of the Rust source rather than retyping it) and
samples greedily, exactly as `LlamaEngine::generate` does. Runs are therefore
deterministic, and a difference between two runs is the training talking
rather than the sampler.

`baseline-2026-08-31.json` is the model as of the first S1-mini finetune, the
one whose failures parts g through j were written against.
`after-v2-2026-08-31.json` is the second finetune, evaluated the same day --
the run that showed memorisation-without-generalisation and led to part-n and
`PART_WEIGHTS`. Keep both: they are what "better" is measured from.
