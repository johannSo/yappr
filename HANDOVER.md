# Good morning — overnight handover

**Everything is built, merged to `main`, installed, and running.** 224 tests, clippy clean.
Nothing touched your microphone, your Hyprland config, or your system settings.

Two safety tags exist: `known-good-m1` (where you went to bed) and `known-good-m2` (now).
`git reset --hard known-good-m1` undoes the entire night.

---

## Do this first (2 minutes)

The daemon is already running on the new build. To get the **overlay** and the fixed
keybindings, add the emitted config:

```bash
owf-ctl setup --print-hypr
```

It prints three blocks — bindings, autostart, and window rules. Paste them into
`~/.config/hypr/bindings.lua`, `autostart.lua`, and `windows.lua` respectively, then:

```bash
hyprctl reload && hyprctl configerrors
```

**I deliberately did not apply these myself.** A bad window rule on an unattended machine
is exactly the damage you told me to avoid. Your `bindings.lua` and `autostart.lua`
already have the dictation keybinds from yesterday; the **window rules block is new** and
is what places the overlay bottom-centre and keeps it unfocused.

Then start the overlay (or log out and back in, since it now autostarts):

```bash
openwhisprflow &
```

Hold **SUPER+D**, speak, release. **SUPER+ALT+D** cancels.

---

## The thing you actually care about: why you got single words

**I could not diagnose this.** It needs you to speak, and I was not willing to record
audio in your room while you were asleep. What I did instead was build the instrument
that will answer it in one shot.

Debug capture is **already enabled** in your config. Dictate one full sentence, then:

```bash
owf-ctl debug
```

The number that matters is **captured vs expected samples**. Hold the key 5 seconds at
48 kHz stereo and the audio callback should deliver ~480,000 samples.

- **ratio ≈ 1.0** → capture is fine, and the problem is downstream (VAD over-trimming, or
  ASR itself). Compare `~/owf/audio/<ts>-raw.wav` against `<ts>-trimmed.wav` — the first
  is what your mic gave us, the second is what Parakeet actually received.
- **ratio well below 1.0** → ALSA is dropping your audio, which fully explains single
  words. `stream_errors` in the same record counts the `Xrun`s.

Both numbers come from genuinely independent sources (an atomic fed by the audio callback
vs. monotonic wall-clock on another thread), so the ratio can't lie by construction. A
reviewer verified that specifically, because a self-referential ratio would have read 1.0
always and sent us after the wrong component.

Everything lands in `~/owf/`: `logs/<ts>.json` per utterance, `logs/daemon.log`,
`audio/*.wav`.

---

## What changed overnight

| | |
|---|---|
| **Capture-rate bug fixed** | Your mic was unusable. We asked for 16 kHz because cpal *advertised* a range containing it; real ALSA hardware rejected the stream build. PipeWire had been hiding this by resampling silently. The resampler this project carries for exactly that case was unreachable code. Now it probes by building a throwaway stream and falls back to 48 kHz + resampling. |
| **Debug capture** | Per-utterance WAVs + JSON records + daemon log under `~/owf/`. |
| **Overlay** | Tauri window, spec §12's eight states, live waveform bars, 280×72, bottom-centre, never focused. |
| **State broadcast** | The daemon publishes `OverlayEvent`s over its socket. `Normalizing` and `Injecting` are now real states — previously everything after key-release collapsed into "transcribing". |
| **`llama-server` supervision** | 10 s health poll, backoff restart 1→30 s, zombie reaping. |
| **`status` stopped lying** | Now reports `normalize_available`. |
| **`--purge-logs`, `last_ms`, `[overlay]` config** | Small spec gaps closed. |

### Your `llama-server` died while you slept

Around 02:00 it became a zombie, `/health` served nothing, and the daemon went right on
reporting `"warm":true`. You'd have woken, dictated, and gotten raw unpunctuated text with
no explanation. I restarted it, and that incident is why supervision jumped the queue —
it was on the list as a theoretical spec gap and became a real one.

---

## What I verified myself

- Overlay window appears at **exactly 280×72**, floating (`hyprctl clients`).
- **Focus stayed on `kitty` through a full replay cycle** — the overlay does not steal it.
  This is the invariant the product depends on: a focused overlay means `wtype` types your
  dictation *into the overlay* instead of your editor.
- Daemon reaches `idle`/`warm` in ~6 s with `llama-server` healthy.
- `SIGTERM` reaps everything cleanly — no orphaned 600 MB model, no stale socket, and an
  immediate restart works.
- All eight overlay states render (headless renders of the shipped CSS, since your display
  was blanked — an agent tried to wake it for real screenshots and was correctly blocked).

## What I could NOT verify

**Nobody has spoken into this system since yesterday.** That means:

- Whether the single-word bug is fixed or even changed.
- Whether the 48 kHz → 16 kHz resample path produces usable audio. It had never run on
  real audio until yesterday evening.
- Whether text lands correctly in Firefox, Electron apps, or XWayland windows (`wtype` is
  known to fail on XWayland; you should get a "copied to clipboard" notification rather
  than silence — if text vanishes with no notification, that's a bug I want to hear about).
- Whether the overlay's timing *feels* right in real use.
- The `.conf`-format window rules — verified against published configs on GitHub, not this
  machine, because your setup is Lua-only. The Lua block got the strong verification.

---

## Morning checklist

- [ ] Apply the window rules, `hyprctl reload && hyprctl configerrors`
- [ ] Dictate one full sentence into a terminal → text appears
- [ ] `owf-ctl debug` → check the captured/expected ratio
- [ ] Watch the overlay: does it show recording bars, then "transcribing", then "cleaning",
      then flash what it typed?
- [ ] Dictate into Firefox
- [ ] Dictate into an XWayland window (`xterm`) — expect text *or* a clipboard notification
- [ ] Say two words ("yes ok") — below 4 words the guardrail is off by design
- [ ] Dictate a phone number as digits — known limitation, falls back to raw ASR
- [ ] Dictate in German — Parakeet handles it; S1-mini is English-only so expect fallback

---

## Known limitations (documented, not bugs)

- **Below 4 words the guardrail is effectively off.** A meaning inversion could be typed
  verbatim. This is intrinsic: the legitimate case ("k thx" → "Okay, thanks!") also has
  zero token overlap, so bag-of-words can't separate good from bad at that length.
- **Dense digit sequences** (a phone number as separate digits) compress word count past
  the guardrail floor and fall back to raw ASR. Pinned by a named test.
- **Guardrail thresholds are still guesses.** They get tuned against real rejection data —
  which `~/owf/` and `rejections.jsonl` now collect.
- **`owf-ctl reload` refuses to change `normalize.enabled`** — restart instead. Rebuilding
  live would freeze the daemon for up to 120 s of model cold-start.
- **`owf-ctl` is a 39.5 MB binary** because it links ONNX Runtime. It starts in 2.1–2.3 ms
  against a 10 ms budget, so it's deferred — but if the keybind ever feels laggy, that's
  the first thing to fix.

---

## Judgement calls I made without you

Full reasoning is in `.superpowers/sdd/*/progress.md`. The ones worth knowing:

1. **Didn't apply your window rules.** Emitted them instead. A bad rule while you're away
   is exactly the "don't destroy anything" case.
2. **Refused to record audio**, including for testing. Every agent got this instruction
   explicitly. It's why the mic half is unverified.
3. **Rebuilt and reinstalled your binaries.** The ones you had running predated last
   night's supervision work — `status` didn't even have `normalize_available`. Without
   this you'd have been testing yesterday's code.
4. **Deferred the `owf-ctl` crate split** on measured evidence (2.1–2.3 ms vs 10 ms).
5. **Overruled my own contrast complaint.** I said the overlay text was too dim; an agent
   measured actual rendered pixels and found I'd judged from a screenshot captured
   mid-animation. Only one state genuinely failed WCAG AA. It fixed that one and left the
   rest alone — correctly.
