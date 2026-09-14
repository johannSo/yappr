//! The loopback endpoint that lets something other than yappr's own
//! injector ask this machine's models for text.
//!
//! One consumer exists: OpenClaw's dictation, which streams microphone audio
//! from its own client to a *realtime transcription provider* running in its
//! Node gateway (`openclaw-plugin/` in this repo is that provider). The
//! provider is given a WebSocket helper by its host and nothing else, so the
//! shape of this server is not a free choice -- it is a WebSocket, it speaks
//! JSON text frames out and binary PCM frames in, and the framing is
//! `tungstenite`'s.
//!
//! ## What this is not
//!
//! It is not a second pipeline. A segment cut here goes through
//! [`crate::pipeline::Pipeline::dictate_text`], which is
//! `process_with_capture` minus the injector -- same vocabulary, same
//! S1-mini prompt, same guardrail, same `finish`. The claim the OpenClaw
//! plugin makes to its users ("the text yappr would have typed") is only
//! true because there is no second implementation here for it to drift from.
//!
//! It is also not a microphone. Nothing in this module opens a capture
//! device; the audio arrives from the client that connected. A dictation
//! started in OpenClaw records in OpenClaw.
//!
//! ## Why a thread pair per session
//!
//! Transcribing one utterance takes hundreds of milliseconds to a couple of
//! seconds (S1-mini included), and the client keeps talking the whole time.
//! Doing that work on the reading thread would stop draining the socket
//! mid-sentence, so each session runs a reader (decode, segment, queue) and
//! a worker (transcribe), joined by a channel. Only the reader ever writes
//! to the socket -- `tungstenite::WebSocket` owns its stream and cannot be
//! split, and a mutex around it would be held for the whole of a blocking
//! `read()`, i.e. exactly when the worker most needs to answer. The worker
//! therefore posts outbound frames to [`Outbox`] and the reader flushes them
//! on its next turn, which is at most [`READ_TIMEOUT`] away.
//!
//! ## Loopback, always
//!
//! `start` binds `127.0.0.1` and there is no config key that changes it (see
//! [`crate::config::RealtimeConfig`]). `[realtime] token` is the answer for a
//! shared machine.

use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use tungstenite::{Message, WebSocket};

use crate::asr::SAMPLE_RATE;
use crate::config::RealtimeConfig;
use crate::vad::Segmenter;

/// How long the reader blocks in `read()` before looking at the stop flag
/// and the outbox.
///
/// It is therefore also the worst-case delay between the worker finishing a
/// transcript and that transcript reaching the socket -- and that is the
/// case that matters, not the stop flag: a finished utterance is produced
/// *after* the speaker stopped talking, which is exactly when no audio is
/// arriving to wake this loop early. 50 ms is imperceptible in dictation and
/// costs 20 wakeups a second per open session, each an `EAGAIN` read on a
/// socket with nothing on it. 200 ms was the first value here and was
/// wrong for one reason: it is long enough to feel, on every single
/// utterance.
///
/// The alternative -- having the worker wake the reader through a
/// self-pipe -- needs `poll` over two descriptors, i.e. a rewrite of this
/// loop around raw fds for a saving of at most 50 ms.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// How long the accept loop waits between polls when nothing is connecting.
/// The listener is non-blocking purely so `RealtimeHandle::stop` does not
/// have to make a throwaway connection to itself to wake it.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// Concurrent sessions. One client (OpenClaw's gateway) is the expected
/// load; the cap exists because every session holds its own Silero instance
/// and each transcription takes the daemon-wide pipeline lock, so a runaway
/// client would starve the user's own dictation rather than merely itself.
const MAX_SESSIONS: usize = 4;

/// Bytes of decoded audio a session will hold for the worker before it
/// starts refusing. At 16 kHz f32 this is ~30 s of speech waiting to be
/// transcribed, which only accumulates if transcription is slower than
/// real time for a sustained stretch. Dropping the oldest would silently
/// lose an utterance (invariant 1), so the session reports and closes
/// instead.
const MAX_QUEUED_SAMPLES: usize = SAMPLE_RATE as usize * 30;

/// What one session needs from the rest of the app, injected so this whole
/// module can be exercised with no model on disk and no daemon.
///
/// Every method is called from a session thread, never from the accept
/// loop.
pub trait RealtimeEngine: Send + Sync {
    /// Makes the models resident, or explains why not. Called once when a
    /// session opens and again before each utterance -- the second call is
    /// what keeps the idle unloader (`[models] idle_unload_seconds`) from
    /// pulling the models out from under a long, quiet session.
    fn ensure_ready(&self) -> Result<(), String>;
    /// A fresh utterance segmenter for one session. Sessions do not share
    /// one: Silero's state is the conversation it is listening to.
    fn segmenter(&self) -> Result<Box<dyn Segmenter>, String>;
    /// One finished utterance. `Ok(None)` means the segment held no words.
    fn transcribe(&self, samples: &[f32]) -> Result<Option<String>, String>;
    /// Marks the daemon busy so the idle unloader leaves the models alone.
    fn touch(&self);
    /// What goes in the `ready` frame, for a client that wants to log which
    /// model it is actually talking to.
    fn describe(&self) -> EngineInfo;
}

/// The `ready` frame's payload, beyond what the client already knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineInfo {
    /// `[asr] model`, as the settings window spells it.
    pub model: String,
    /// Whether transcripts get the S1-mini rewrite -- `[realtime] normalize`
    /// ANDed with `[normalize] enabled`, resolved server-side so the client
    /// does not have to guess from two settings it cannot see.
    pub normalize: bool,
}

/// A running listener. Dropping this does **not** stop it; call
/// [`RealtimeHandle::stop`], which is what `shutdown` and every config
/// change do.
pub struct RealtimeHandle {
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    /// The address actually bound, which is the port the settings window
    /// reports and the plugin connects to. Not always `cfg.port`: a test
    /// asks for 0 and needs to learn what it got.
    addr: SocketAddr,
    /// The config this listener was started from, so `sync` can tell a
    /// change that matters from a save that touched something else.
    cfg: RealtimeConfig,
}

impl RealtimeHandle {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn config(&self) -> &RealtimeConfig {
        &self.cfg
    }

    /// Stops accepting and joins the accept loop. In-flight sessions notice
    /// the same flag within [`READ_TIMEOUT`] and close themselves; they are
    /// deliberately not joined here, because a session blocked on a slow
    /// transcription would hold up `shutdown` -- and the socket it holds is
    /// not the listening one, so it cannot keep the port from being bound
    /// again.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

/// Binds the endpoint and starts accepting.
///
/// Returns `Err` when the port is taken, which is a state the settings
/// window reports rather than a fatal one: yappr's own dictation does not
/// depend on this listener existing.
pub fn start(cfg: &RealtimeConfig, engine: Arc<dyn RealtimeEngine>) -> Result<RealtimeHandle> {
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, cfg.port)))
        .with_context(|| format!("binding 127.0.0.1:{} for the realtime endpoint", cfg.port))?;
    let addr = listener.local_addr().context("reading the bound address")?;
    listener
        .set_nonblocking(true)
        .context("making the realtime listener non-blocking")?;

    let stop = Arc::new(AtomicBool::new(false));
    let accept = {
        let stop = Arc::clone(&stop);
        let cfg = cfg.clone();
        std::thread::Builder::new()
            .name("yappr-realtime".into())
            .spawn(move || accept_loop(listener, cfg, engine, stop))
            .context("spawning the realtime accept loop")?
    };

    tracing::info!(%addr, "realtime transcription endpoint listening");
    Ok(RealtimeHandle { stop, accept: Some(accept), addr, cfg: cfg.clone() })
}

fn accept_loop(
    listener: TcpListener,
    cfg: RealtimeConfig,
    engine: Arc<dyn RealtimeEngine>,
    stop: Arc<AtomicBool>,
) {
    let live = Arc::new(AtomicUsize::new(0));
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, peer)) => {
                if live.load(Ordering::SeqCst) >= MAX_SESSIONS {
                    tracing::warn!(%peer, "realtime session refused: too many open sessions");
                    drop(stream);
                    continue;
                }
                live.fetch_add(1, Ordering::SeqCst);
                let cfg = cfg.clone();
                let engine = Arc::clone(&engine);
                let stop = Arc::clone(&stop);
                let session_live = Arc::clone(&live);
                let spawned = std::thread::Builder::new()
                    .name("yappr-realtime-session".into())
                    .spawn(move || {
                        if let Err(e) = session(stream, &cfg, engine, &stop) {
                            // A client that hangs up mid-stream is the
                            // ordinary end of a dictation, not a fault, so
                            // this is not a warning: OpenClaw closes the
                            // socket the moment the user stops dictating.
                            tracing::debug!(%peer, error = %e, "realtime session ended");
                        }
                        session_live.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    // The closure took the counter's clone with it, so undo
                    // the increment through the accept loop's own handle.
                    live.fetch_sub(1, Ordering::SeqCst);
                    tracing::error!("could not spawn a realtime session thread");
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(ACCEPT_POLL),
            Err(e) => {
                tracing::warn!(error = %e, "realtime accept failed");
                std::thread::sleep(ACCEPT_POLL);
            }
        }
    }
    tracing::info!("realtime transcription endpoint stopped");
}

/// The audio format a client declares in its query string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub encoding: Encoding,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// PCM s16le, the shape a browser or a native composer records in.
    Linear16,
    /// G.711 µ-law at 8 kHz, the shape a Twilio media stream arrives in --
    /// OpenClaw's Voice Call path feeds the same provider registry, so
    /// refusing it would make this endpoint useless for half of what the
    /// contract is for.
    Mulaw,
}

impl Encoding {
    fn parse(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "linear16" | "pcm" | "pcm_s16le" | "s16le" => Ok(Encoding::Linear16),
            "mulaw" | "ulaw" | "g711_ulaw" | "g711-ulaw" => Ok(Encoding::Mulaw),
            other => Err(format!(
                "unsupported encoding {other:?} (expected linear16 or mulaw)"
            )),
        }
    }
}

impl Default for AudioFormat {
    fn default() -> Self {
        Self { sample_rate: SAMPLE_RATE as u32, encoding: Encoding::Linear16 }
    }
}

/// What the handshake extracted from the request line: the declared audio
/// format, and whether the caller proved it may be here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Handshake {
    format: AudioFormat,
}

/// Reads the query string and the `Authorization` header.
///
/// Split out from the handshake callback and kept pure so the interesting
/// cases -- a wrong token, a missing token where one is required, an
/// encoding nobody supports, a channel count above one -- are testable
/// without a socket.
fn negotiate(uri: &str, auth_header: Option<&str>, cfg: &RealtimeConfig) -> Result<Handshake, (StatusCode, String)> {
    let query = uri.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut token: Option<String> = None;
    let mut format = AudioFormat::default();
    let mut channels: u32 = 1;

    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = percent_decode(v);
        match k {
            "token" => token = Some(v),
            "sample_rate" | "sampleRate" => {
                format.sample_rate = v.parse().map_err(|_| {
                    (StatusCode::BAD_REQUEST, format!("sample_rate {v:?} is not a number"))
                })?;
            }
            "encoding" => {
                format.encoding =
                    Encoding::parse(&v).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
            }
            "channels" => {
                channels = v.parse().map_err(|_| {
                    (StatusCode::BAD_REQUEST, format!("channels {v:?} is not a number"))
                })?;
            }
            // `model` is accepted and ignored on purpose: OpenClaw lets a
            // session ask for a model, but which ASR model runs is
            // `[asr] model` in this app's own settings, and switching it
            // per connection would mean loading a second model into a
            // process that just spent a second loading the first. Logged,
            // not refused -- a refusal would break a caller that merely
            // passed its default through.
            "model" | "language" => {}
            _ => {}
        }
    }

    if channels != 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("only mono audio is supported, got channels={channels}"),
        ));
    }
    if format.sample_rate < 8_000 || format.sample_rate > 192_000 {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("sample_rate {} is outside 8000..=192000", format.sample_rate),
        ));
    }

    if !cfg.token.is_empty() {
        let bearer = auth_header
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .map(str::to_string);
        let given = bearer.or(token);
        // Constant-time-ish is not the point here -- the listener is
        // loopback-only and an attacker who can time it can also read the
        // config file this token lives in. Being *present and equal* is.
        if given.as_deref() != Some(cfg.token.as_str()) {
            return Err((StatusCode::UNAUTHORIZED, "invalid or missing token".to_string()));
        }
    }

    Ok(Handshake { format })
}

/// Enough percent-decoding for a token in a query string. Not a URL
/// library: the only values that reach it are ones the plugin wrote.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The 4xx `tungstenite` sends instead of an upgrade.
///
/// Built here rather than inline in the handshake callback only to keep
/// clippy's `result_large_err` quiet: an `ErrorResponse` is an
/// `http::Response`, which is large enough that returning one from a closure
/// is worth a lint. Nothing is hot about this path -- it runs once, on a
/// connection that is about to be refused.
fn refusal(status: StatusCode, message: String) -> ErrorResponse {
    let mut err = ErrorResponse::new(Some(message));
    *err.status_mut() = status;
    err
}

/// Frames the worker thread hands back to the reader for writing.
type Outbox = Arc<Mutex<VecDeque<String>>>;

fn push(outbox: &Outbox, frame: serde_json::Value) {
    if let Ok(mut q) = outbox.lock() {
        q.push_back(frame.to_string());
    }
}

/// One accepted connection, from the WebSocket handshake to the close.
///
/// `result_large_err` is allowed because the offending `Result` is
/// `tungstenite::accept_hdr`'s, not ours: the callback it takes must return
/// `Result<Response, ErrorResponse>`, and an `ErrorResponse` is an
/// `http::Response`, which is 136 bytes. Boxing it would mean not
/// implementing the signature. The path runs once per connection, on the
/// refusal branch.
#[allow(clippy::result_large_err)]
fn session(
    stream: TcpStream,
    cfg: &RealtimeConfig,
    engine: Arc<dyn RealtimeEngine>,
    stop: &AtomicBool,
) -> Result<()> {
    stream.set_nodelay(true).ok();
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .context("setting the session read timeout")?;

    // The handshake callback is the only place the request line and headers
    // are visible, so the whole negotiation happens inside it and leaves its
    // answer here.
    let negotiated: Arc<Mutex<Option<Handshake>>> = Arc::new(Mutex::new(None));
    let mut ws = {
        let negotiated = Arc::clone(&negotiated);
        let cfg = cfg.clone();
        tungstenite::accept_hdr(stream, move |req: &Request, res: Response| {
            let auth = req
                .headers()
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            match negotiate(&req.uri().to_string(), auth.as_deref(), &cfg) {
                Ok(h) => {
                    *negotiated.lock().expect("fresh mutex") = Some(h);
                    Ok(res)
                }
                Err((status, message)) => {
                    tracing::warn!(%status, %message, "realtime handshake refused");
                    Err(refusal(status, message))
                }
            }
        })
        .context("websocket handshake")?
    };
    let format = negotiated
        .lock()
        .expect("fresh mutex")
        .clone()
        .map(|h| h.format)
        .unwrap_or_default();

    // Before `ready`, so a client is never told to start talking into a
    // process that has no models and cannot get any.
    if let Err(e) = engine.ensure_ready() {
        let _ = ws.send(Message::Text(
            serde_json::json!({ "type": "error", "message": e }).to_string().into(),
        ));
        let _ = ws.close(None);
        anyhow::bail!("models unavailable: {e}");
    }
    engine.touch();

    let mut segmenter = match engine.segmenter() {
        Ok(s) => s,
        Err(e) => {
            let _ = ws.send(Message::Text(
                serde_json::json!({ "type": "error", "message": e }).to_string().into(),
            ));
            let _ = ws.close(None);
            anyhow::bail!("no segmenter: {e}");
        }
    };

    let info = engine.describe();
    ws.send(Message::Text(
        serde_json::json!({
            "type": "ready",
            "sample_rate": format.sample_rate,
            "encoding": match format.encoding {
                Encoding::Linear16 => "linear16",
                Encoding::Mulaw => "mulaw",
            },
            "normalize": info.normalize,
            "model": info.model,
        })
        .to_string()
        .into(),
    ))
    .context("sending the ready frame")?;

    let outbox: Outbox = Arc::new(Mutex::new(VecDeque::new()));
    // The backlog, in samples, shared so the worker can pay it down: the
    // reader adds a segment's length when it queues one and the worker
    // subtracts it when the transcript is out. A counter only the reader
    // touched would be a running total of everything ever said in this
    // session, and would close a perfectly healthy connection after
    // `MAX_QUEUED_SAMPLES` of *cumulative* speech.
    let queued = Arc::new(AtomicUsize::new(0));
    let (jobs_tx, jobs_rx) = mpsc::channel::<Vec<f32>>();
    let worker = {
        let outbox = Arc::clone(&outbox);
        let engine = Arc::clone(&engine);
        let queued = Arc::clone(&queued);
        std::thread::Builder::new()
            .name("yappr-realtime-asr".into())
            .spawn(move || transcribe_loop(jobs_rx, engine, outbox, queued))
            .context("spawning the realtime transcription thread")?
    };

    let mut decoder = PcmDecoder::new(format)?;
    let mut speaking = false;
    let result = read_loop(
        &mut ws,
        &mut decoder,
        segmenter.as_mut(),
        &jobs_tx,
        &outbox,
        &mut speaking,
        &queued,
        stop,
    );

    // The client is gone (or asked to stop). Whatever Silero still holds is
    // audio the user spoke, so it is flushed and transcribed rather than
    // dropped -- invariant 1's rule, applied one stage earlier than the
    // pipeline applies it. Nothing can be *sent* if the socket is already
    // closed, but the job still runs: the transcript reaches the debug log
    // and the rejections dataset either way, and a close that races a
    // finalize can still be answered.
    segmenter.flush();
    while let Some(seg) = segmenter.take_segment() {
        let _ = jobs_tx.send(seg);
    }
    drop(jobs_tx);
    let _ = worker.join();
    flush_outbox(&mut ws, &outbox);
    let _ = ws.close(None);
    let _ = ws.flush();
    result
}

/// The reader: decode, segment, queue, and flush whatever the worker has
/// finished.
#[allow(clippy::too_many_arguments)]
fn read_loop(
    ws: &mut WebSocket<TcpStream>,
    decoder: &mut PcmDecoder,
    segmenter: &mut dyn Segmenter,
    jobs: &mpsc::Sender<Vec<f32>>,
    outbox: &Outbox,
    speaking: &mut bool,
    queued: &AtomicUsize,
    stop: &AtomicBool,
) -> Result<()> {
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        flush_outbox(ws, outbox);

        match ws.read() {
            Ok(Message::Binary(data)) => {
                let samples = decoder.decode(&data);
                if !samples.is_empty() {
                    segmenter.accept(&samples);
                }
                // Checked after `accept`, not before: the transition into
                // speech is what `onSpeechStart` means, and it can only be
                // known once this chunk has been heard.
                if !*speaking && segmenter.speaking() {
                    *speaking = true;
                    push(outbox, serde_json::json!({ "type": "speech_start" }));
                }
                while let Some(seg) = segmenter.take_segment() {
                    *speaking = false;
                    let backlog = queued.fetch_add(seg.len(), Ordering::SeqCst) + seg.len();
                    if backlog > MAX_QUEUED_SAMPLES {
                        push(
                            outbox,
                            serde_json::json!({
                                "type": "error",
                                "message": format!(
                                    "transcription is not keeping up; more than {} seconds of audio are waiting",
                                    MAX_QUEUED_SAMPLES / SAMPLE_RATE as usize
                                ),
                            }),
                        );
                        flush_outbox(ws, outbox);
                        return Ok(());
                    }
                    if jobs.send(seg).is_err() {
                        return Ok(()); // the worker is gone; so is this session
                    }
                }
            }
            Ok(Message::Text(text)) => {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(v) => {
                        let kind = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if kind == "finalize" || kind == "flush" {
                            // Everything captured so far becomes segments
                            // now, which is what makes the last sentence of
                            // a dictation survive the user letting go of the
                            // button. The `finalized` answer is queued
                            // *behind* those jobs' results, so a client that
                            // waits for it has seen every transcript.
                            segmenter.flush();
                            while let Some(seg) = segmenter.take_segment() {
                                if jobs.send(seg).is_err() {
                                    return Ok(());
                                }
                            }
                            *speaking = false;
                            if jobs.send(Vec::new()).is_err() {
                                return Ok(());
                            }
                        } else {
                            tracing::debug!(kind, "ignoring unknown realtime control frame");
                        }
                    }
                    Err(e) => {
                        push(
                            outbox,
                            serde_json::json!({
                                "type": "error",
                                "message": format!("control frame is not JSON: {e}"),
                            }),
                        );
                    }
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {} // Ping/Pong/Frame: tungstenite answers pings itself
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
            {
                // The read timeout, which is the only reason this loop is
                // not permanently blocked: it is what gives the stop flag
                // and the outbox a turn.
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                return Ok(())
            }
            Err(e) => return Err(anyhow::anyhow!(e)),
        }
    }
}

/// The worker: one utterance at a time, in the order they were cut.
///
/// An empty job is the `finalize` marker -- it carries no audio and its only
/// effect is the `finalized` frame, queued behind every transcript that was
/// already in flight. That ordering is the entire contract: a client that
/// closes on `finalized` has been given everything.
fn transcribe_loop(
    jobs: mpsc::Receiver<Vec<f32>>,
    engine: Arc<dyn RealtimeEngine>,
    outbox: Outbox,
    queued: Arc<AtomicUsize>,
) {
    for samples in jobs {
        if samples.is_empty() {
            push(&outbox, serde_json::json!({ "type": "finalized" }));
            continue;
        }
        // Paid down whichever way this iteration ends, including the error
        // arms below: a backlog that only ever grew would close the session
        // on the first utterance that failed.
        let _paid = PayDown(&queued, samples.len());
        // Re-checked per utterance, not just at connect: `[models]
        // idle_unload_seconds` measures from the last *dictation*, and a
        // session that sat quiet through the deadline would otherwise find
        // the models gone. `touch` then keeps the next one from racing it.
        if let Err(e) = engine.ensure_ready() {
            push(&outbox, serde_json::json!({ "type": "error", "message": e }));
            continue;
        }
        engine.touch();
        match engine.transcribe(&samples) {
            Ok(Some(text)) => {
                push(&outbox, serde_json::json!({ "type": "final", "text": text }))
            }
            // No words in that segment. Not an error and not a frame: a
            // client that received `{"text":""}` would either post an empty
            // message or have to filter it, and the VAD cuts on breath
            // often enough to make that a real nuisance.
            Ok(None) => tracing::debug!("realtime segment produced no text"),
            Err(e) => push(&outbox, serde_json::json!({ "type": "error", "message": e })),
        }
        engine.touch();
    }
}

/// Subtracts one job's samples from the backlog when it goes out of scope.
///
/// A guard rather than a line at the end of the loop body, for the same
/// reason `DebugRecordGuard` is one: the body has three arms and a `continue`
/// above it, and the one that gets forgotten is always the error path.
struct PayDown<'a>(&'a AtomicUsize, usize);

impl Drop for PayDown<'_> {
    fn drop(&mut self) {
        // Saturating, not wrapping: an underflow here would read as an
        // enormous backlog and close the next session that queued anything.
        let _ = self.0.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
            Some(v.saturating_sub(self.1))
        });
    }
}

fn flush_outbox(ws: &mut WebSocket<TcpStream>, outbox: &Outbox) {
    loop {
        let next = outbox.lock().ok().and_then(|mut q| q.pop_front());
        let Some(frame) = next else { return };
        if ws.send(Message::Text(frame.into())).is_err() {
            return;
        }
    }
}

/// Turns the client's bytes into the 16 kHz mono f32 the models want.
///
/// Two pieces of state, both of which exist because a WebSocket frame is
/// whatever size the sender chose and has nothing to do with sample
/// boundaries: `odd` keeps a `linear16` sample that was split across two
/// frames, and the resampler is stateful by construction.
struct PcmDecoder {
    format: AudioFormat,
    odd: Option<u8>,
    resampler: Option<sherpa_onnx::LinearResampler>,
}

impl PcmDecoder {
    fn new(format: AudioFormat) -> Result<Self> {
        let resampler = if format.sample_rate == SAMPLE_RATE as u32 {
            None
        } else {
            Some(
                sherpa_onnx::LinearResampler::create(format.sample_rate as i32, SAMPLE_RATE)
                    .context("creating the realtime resampler")?,
            )
        };
        Ok(Self { format, odd: None, resampler })
    }

    fn decode(&mut self, data: &[u8]) -> Vec<f32> {
        let raw = match self.format.encoding {
            Encoding::Linear16 => self.decode_linear16(data),
            Encoding::Mulaw => data.iter().map(|b| mulaw_to_f32(*b)).collect(),
        };
        match &self.resampler {
            Some(r) => r.resample(&raw, false),
            None => raw,
        }
    }

    fn decode_linear16(&mut self, data: &[u8]) -> Vec<f32> {
        let mut out = Vec::with_capacity(data.len() / 2 + 1);
        let mut rest = data;
        if let Some(first) = self.odd.take() {
            if let Some((second, tail)) = rest.split_first() {
                out.push(i16::from_le_bytes([first, *second]) as f32 / 32768.0);
                rest = tail;
            } else {
                self.odd = Some(first);
                return out;
            }
        }
        let (pairs, remainder) = rest.as_chunks::<2>();
        for c in pairs {
            out.push(i16::from_le_bytes(*c) as f32 / 32768.0);
        }
        if let [last] = remainder {
            self.odd = Some(*last);
        }
        out
    }
}

/// G.711 µ-law expansion (ITU-T G.711), the same arithmetic every telephony
/// stack uses. Written out rather than pulled in: it is eight lines and the
/// alternative is a dependency whose only job is this table.
fn mulaw_to_f32(byte: u8) -> f32 {
    let u = !byte;
    let sign = (u & 0x80) != 0;
    let exponent = (u >> 4) & 0x07;
    let mantissa = u & 0x0F;
    let magnitude = (((mantissa as i32) << 3) + 0x84) << exponent;
    let sample = magnitude - 0x84;
    let sample = if sign { -sample } else { sample };
    sample as f32 / 32768.0
}

#[cfg(test)]
mod tests {
    //! Everything here runs with no model on disk and no daemon: the
    //! engine and the segmenter are the two seams that make that possible,
    //! and they exist for this. What is *not* covered here is Silero's own
    //! judgement about where an utterance ends -- that needs the model, and
    //! lives with the `--ignored` tests in `vad.rs`.

    use super::*;

    fn cfg() -> RealtimeConfig {
        RealtimeConfig { enabled: true, port: 0, ..RealtimeConfig::default() }
    }

    #[test]
    fn a_bare_connection_gets_the_dictation_defaults() {
        let h = negotiate("/v1/transcribe", None, &cfg()).unwrap();
        assert_eq!(h.format.sample_rate, 16_000);
        assert_eq!(h.format.encoding, Encoding::Linear16);
    }

    #[test]
    fn the_telephony_shape_is_accepted_too() {
        let h = negotiate(
            "/v1/transcribe?sample_rate=8000&encoding=mulaw&channels=1",
            None,
            &cfg(),
        )
        .unwrap();
        assert_eq!(h.format.sample_rate, 8_000);
        assert_eq!(h.format.encoding, Encoding::Mulaw);
    }

    #[test]
    fn an_unknown_encoding_is_refused_rather_than_guessed() {
        // Silently falling back to linear16 would turn a client's opus
        // stream into a few seconds of noise the VAD then earnestly cuts
        // into utterances. The error names the value.
        let (status, msg) = negotiate("/x?encoding=opus", None, &cfg()).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("opus"), "{msg}");
    }

    #[test]
    fn stereo_is_refused_because_nothing_downstream_mixes_it() {
        let (status, msg) = negotiate("/x?channels=2", None, &cfg()).unwrap_err();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(msg.contains("mono"), "{msg}");
    }

    #[test]
    fn no_token_configured_means_no_token_demanded() {
        assert!(negotiate("/x", None, &cfg()).is_ok());
        // And a client that sends one anyway is not punished for it: the
        // plugin sends whatever its own config carries, which may be a
        // leftover from before the token was cleared here.
        assert!(negotiate("/x?token=whatever", None, &cfg()).is_ok());
    }

    #[test]
    fn a_configured_token_is_required_and_checked() {
        let c = RealtimeConfig { token: "s3cret".into(), ..cfg() };
        assert_eq!(negotiate("/x", None, &c).unwrap_err().0, StatusCode::UNAUTHORIZED);
        assert_eq!(
            negotiate("/x?token=wrong", None, &c).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
        assert!(negotiate("/x?token=s3cret", None, &c).is_ok());
        assert!(negotiate("/x", Some("Bearer s3cret"), &c).is_ok());
        // The header wins when both are present, which is what makes a
        // stale `?token=` in a saved URL fail loudly instead of being
        // quietly overridden.
        assert_eq!(
            negotiate("/x?token=s3cret", Some("Bearer wrong"), &c).unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[test]
    fn a_percent_encoded_token_survives_the_query_string() {
        let c = RealtimeConfig { token: "a b+c/d".into(), ..cfg() };
        assert!(negotiate("/x?token=a%20b%2Bc%2Fd", None, &c).is_ok());
    }

    #[test]
    fn a_sample_split_across_two_frames_is_not_lost() {
        // The hazard is structural: a WebSocket frame boundary has nothing
        // to do with a 2-byte sample boundary, so this happens whenever the
        // sender's chunk size is odd. Dropping the stray byte would shift
        // every subsequent sample by one byte -- i.e. turn the rest of the
        // stream into noise, not merely lose 60 microseconds.
        let mut d = PcmDecoder::new(AudioFormat::default()).unwrap();
        let first = d.decode(&[0x00, 0x40, 0x00]); // one whole sample + half of one
        assert_eq!(first.len(), 1);
        assert!((first[0] - 0.5).abs() < 1e-6, "got {}", first[0]);
        let second = d.decode(&[0x80]); // the other half: 0x8000 = -1.0
        assert_eq!(second.len(), 1);
        assert!((second[0] + 1.0).abs() < 1e-6, "got {}", second[0]);
    }

    /// The test's mu-law encoder and the module's decoder are inverses to
    /// within the format's own quantisation. Without this, a bug in the
    /// *encoder* would make the telephony test fail and send the reader
    /// hunting through `PcmDecoder`.
    #[test]
    fn the_test_encoder_round_trips_through_the_real_decoder() {
        for step in -20..=20 {
            let original = step as f32 / 20.0;
            let back = mulaw_to_f32(f32_to_mulaw(original));
            // mu-law is logarithmic: ~8% relative error near full scale is
            // the format, not a defect. An absolute floor covers the values
            // near zero, where the relative measure is meaningless.
            let tolerance = (original.abs() * 0.1).max(0.01);
            assert!(
                (back - original).abs() <= tolerance,
                "{original} came back as {back}"
            );
        }
    }

    #[test]
    fn mulaw_decodes_to_the_expected_extremes() {
        // 0xFF is µ-law silence-ish (+0), 0x7F its negative counterpart.
        assert!(mulaw_to_f32(0xFF).abs() < 0.01);
        assert!(mulaw_to_f32(0x7F).abs() < 0.01);
        // 0x00 and 0x80 are the loudest code of each sign -- and in that
        // order, which is the trap: the sign bit is read from the
        // *complemented* byte (G.711's `u_val = ~u_val` first), so the
        // stored 0x00 is the largest *negative* sample, not the largest
        // positive one. Getting this backwards inverts every waveform, which
        // no ASR model notices and no listener could either.
        assert!(mulaw_to_f32(0x00) < -0.9, "{}", mulaw_to_f32(0x00));
        assert!(mulaw_to_f32(0x80) > 0.9, "{}", mulaw_to_f32(0x80));
    }

    #[test]
    fn eight_kilohertz_audio_arrives_at_sixteen() {
        let mut d = PcmDecoder::new(AudioFormat { sample_rate: 8_000, encoding: Encoding::Mulaw })
            .unwrap();
        let out = d.decode(&[0xFF; 800]); // 100 ms at 8 kHz
        // The resampler has a startup delay, so this is about the ratio,
        // not an exact count: what matters is that 8 kHz is not silently
        // fed to a 16 kHz model at half speed.
        assert!(out.len() > 1_200, "expected ~1600 samples, got {}", out.len());
    }

    /// Cuts a segment every `cut_at` samples, so a test can decide when an
    /// utterance ends instead of having to sound like one.
    struct CountingSegmenter {
        buf: Vec<f32>,
        cut_at: usize,
        queue: VecDeque<Vec<f32>>,
    }

    impl Segmenter for CountingSegmenter {
        fn accept(&mut self, samples: &[f32]) {
            self.buf.extend_from_slice(samples);
            while self.buf.len() >= self.cut_at {
                let rest = self.buf.split_off(self.cut_at);
                self.queue.push_back(std::mem::replace(&mut self.buf, rest));
            }
        }
        fn take_segment(&mut self) -> Option<Vec<f32>> {
            self.queue.pop_front()
        }
        fn speaking(&self) -> bool {
            !self.buf.is_empty()
        }
        fn flush(&mut self) {
            if !self.buf.is_empty() {
                self.queue.push_back(std::mem::take(&mut self.buf));
            }
        }
    }

    struct FakeEngine {
        cut_at: usize,
        transcribed: Arc<AtomicUsize>,
        ready: Result<(), String>,
    }

    impl RealtimeEngine for FakeEngine {
        fn ensure_ready(&self) -> Result<(), String> {
            self.ready.clone()
        }
        fn segmenter(&self) -> Result<Box<dyn Segmenter>, String> {
            Ok(Box::new(CountingSegmenter {
                buf: Vec::new(),
                cut_at: self.cut_at,
                queue: VecDeque::new(),
            }))
        }
        fn transcribe(&self, samples: &[f32]) -> Result<Option<String>, String> {
            let n = self.transcribed.fetch_add(1, Ordering::SeqCst);
            Ok(Some(format!("utterance {n} of {} samples", samples.len())))
        }
        fn touch(&self) {}
        fn describe(&self) -> EngineInfo {
            EngineInfo { model: "fake".into(), normalize: true }
        }
    }

    fn connect(addr: SocketAddr) -> tungstenite::WebSocket<std::net::TcpStream> {
        connect_as(addr, AudioFormat::default())
    }

    fn connect_as(
        addr: SocketAddr,
        fmt: AudioFormat,
    ) -> tungstenite::WebSocket<std::net::TcpStream> {
        let stream = std::net::TcpStream::connect(addr).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let encoding = match fmt.encoding {
            Encoding::Linear16 => "linear16",
            Encoding::Mulaw => "mulaw",
        };
        let (ws, _) = tungstenite::client(
            format!(
                "ws://{addr}/v1/transcribe?sample_rate={}&encoding={encoding}",
                fmt.sample_rate
            ),
            stream,
        )
        .expect("handshake");
        ws
    }

    fn next_json(ws: &mut tungstenite::WebSocket<std::net::TcpStream>) -> Value {
        loop {
            match ws.read().expect("read") {
                Message::Text(t) => {
                    return serde_json::from_str(&t).expect("server frames are JSON")
                }
                Message::Close(_) => panic!("the server closed before answering"),
                _ => {}
            }
        }
    }

    use serde_json::Value;

    /// The whole contract in one pass: the handshake, the ready frame, a
    /// turn's `speech_start`, a `final` per finished utterance, and
    /// `finalized` arriving *after* the transcript of audio that was still
    /// open when the client asked to stop.
    ///
    /// That last ordering is the one a plugin depends on -- OpenClaw closes
    /// its socket the moment the user releases the microphone, and the last
    /// sentence only survives because `finalize` flushes the segmenter and
    /// queues its answer behind the resulting job.
    #[test]
    fn a_session_speaks_the_protocol_the_plugin_expects() {
        let transcribed = Arc::new(AtomicUsize::new(0));
        let engine = Arc::new(FakeEngine {
            cut_at: 8_000,
            transcribed: Arc::clone(&transcribed),
            ready: Ok(()),
        });
        let handle = start(&cfg(), engine).expect("bind");
        let mut ws = connect(handle.addr());

        let ready = next_json(&mut ws);
        assert_eq!(ready["type"], "ready");
        assert_eq!(ready["sample_rate"], 16_000);
        assert_eq!(ready["model"], "fake");
        assert_eq!(ready["normalize"], true);

        // 12 000 samples: one full segment plus a tail the client never
        // pauses long enough to close.
        let pcm: Vec<u8> = (0..12_000).flat_map(|_| [0x00u8, 0x20]).collect();
        ws.send(Message::Binary(pcm.into())).unwrap();

        let first = next_json(&mut ws);
        assert_eq!(first["type"], "speech_start");
        let second = next_json(&mut ws);
        assert_eq!(second["type"], "final");
        assert_eq!(second["text"], "utterance 0 of 8000 samples");

        ws.send(Message::Text(r#"{"type":"finalize"}"#.into())).unwrap();
        let third = next_json(&mut ws);
        assert_eq!(third["type"], "final", "the open tail must be transcribed, not dropped");
        assert_eq!(third["text"], "utterance 1 of 4000 samples");
        let fourth = next_json(&mut ws);
        assert_eq!(fourth["type"], "finalized", "finalized comes last, or it means nothing");

        ws.close(None).ok();
        handle.stop();
    }

    /// A session longer than `MAX_QUEUED_SAMPLES` of speech is ordinary --
    /// half a minute of dictation is not a fault -- and it must not be shut
    /// down for it.
    ///
    /// The bug this pins was a counter only the reader ever touched: it
    /// added every segment and nothing subtracted, so what was meant to
    /// measure a backlog measured the session's whole transcript instead
    /// and closed the connection mid-sentence at the 30-second mark, with a
    /// message blaming transcription for not keeping up.
    #[test]
    fn a_long_session_is_not_mistaken_for_a_backlog() {
        let engine = Arc::new(FakeEngine {
            cut_at: 16_000, // one segment per second of audio
            transcribed: Arc::new(AtomicUsize::new(0)),
            ready: Ok(()),
        });
        let handle = start(&cfg(), engine).expect("bind");
        let mut ws = connect(handle.addr());
        assert_eq!(next_json(&mut ws)["type"], "ready");

        // Well past `MAX_QUEUED_SAMPLES` in total, one second at a time and
        // read back each time, so the actual backlog never exceeds a
        // segment or two.
        let second: Vec<u8> = (0..16_000).flat_map(|_| [0x00u8, 0x20]).collect();
        for i in 0..40 {
            ws.send(Message::Binary(second.clone().into())).unwrap();
            loop {
                let frame = next_json(&mut ws);
                match frame["type"].as_str() {
                    Some("final") => break,
                    Some("speech_start") => continue,
                    other => panic!("second {i}: unexpected {other:?} -- {frame}"),
                }
            }
        }

        ws.close(None).ok();
        handle.stop();
    }

    #[test]
    fn a_client_with_the_wrong_token_never_reaches_the_protocol() {
        let engine = Arc::new(FakeEngine {
            cut_at: 8_000,
            transcribed: Arc::new(AtomicUsize::new(0)),
            ready: Ok(()),
        });
        let c = RealtimeConfig { token: "s3cret".into(), ..cfg() };
        let handle = start(&c, engine).expect("bind");

        let stream = std::net::TcpStream::connect(handle.addr()).unwrap();
        let err = tungstenite::client(
            format!("ws://{}/v1/transcribe", handle.addr()),
            stream,
        )
        .expect_err("an unauthenticated client must be refused");
        let text = format!("{err:?}");
        assert!(text.contains("401") || text.to_lowercase().contains("unauthorized"), "{text}");

        handle.stop();
    }

    /// A client is told why it cannot dictate, rather than being left to
    /// talk into a session that will never answer. This is the state a
    /// fresh install is in until the models have been downloaded.
    #[test]
    fn a_session_that_cannot_load_the_models_says_so_and_closes() {
        let engine = Arc::new(FakeEngine {
            cut_at: 8_000,
            transcribed: Arc::new(AtomicUsize::new(0)),
            ready: Err("missing parakeet-tdt-0.6b-v3-int8".into()),
        });
        let handle = start(&cfg(), engine).expect("bind");
        let mut ws = connect(handle.addr());

        let frame = next_json(&mut ws);
        assert_eq!(frame["type"], "error");
        assert!(
            frame["message"].as_str().unwrap().contains("parakeet"),
            "the client must learn which model is missing: {frame}"
        );

        handle.stop();
    }

    /// The one test that exercises the real thing: real Silero
    /// segmentation, the real ASR, and a real `Pipeline` behind the real
    /// socket, fed a wav file the way a client feeds a microphone.
    ///
    /// `#[ignore]`d for the reason every model-backed test in this crate is
    /// -- and it earns its keep the same way `vad.rs`'s do: this is the only
    /// test in which `SileroSegmenter` decides anything, and the only one
    /// where the bytes arriving from a socket reach a model. Everything
    /// above it is fakes by design.
    ///
    /// The normalizer is deliberately absent (`[normalize] enabled =
    /// false`): S1-mini is half a gigabyte and its output is not what this
    /// test is about. `pipeline_e2e.rs` covers the rewrite on this path.
    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn a_wav_file_streamed_through_the_socket_comes_back_as_text() {
        let text = wav_through_socket(AudioFormat::default());
        eprintln!("streamed transcript (linear16 @ 16 kHz): {text:?}");
        assert!(!text.trim().is_empty(), "a fixture of speech must come back as words");
    }

    /// The same path in the format the only real consumer actually sends.
    ///
    /// OpenClaw's browser transcription relay emits G.711 mu-law at 8 kHz --
    /// `RELAY_INPUT_ENCODING` / `RELAY_INPUT_SAMPLE_RATE_HZ` in the host, with
    /// an assertion that refuses a provider declaring anything else. So this,
    /// not the 16 kHz case above, is the path a dictation into OpenClaw takes:
    /// mu-law expanded and resampled 8 -> 16 kHz by `PcmDecoder` before the
    /// VAD ever sees it. Worth its own test precisely because it is the lossy
    /// one -- if the companding or the resampler were wrong, the 16 kHz test
    /// would still pass and every real dictation would come back as noise.
    #[test]
    #[ignore = "requires downloaded models; run with --ignored"]
    fn the_telephony_format_openclaw_actually_sends_also_comes_back_as_text() {
        let text = wav_through_socket(AudioFormat {
            sample_rate: 8_000,
            encoding: Encoding::Mulaw,
        });
        eprintln!("streamed transcript (mulaw @ 8 kHz): {text:?}");
        assert!(
            !text.trim().is_empty(),
            "half the bandwidth, but it must still transcribe"
        );
    }

    /// Streams `fixtures/hallo_german.wav` through a real socket in `format`,
    /// with real models behind it, and returns everything the endpoint sent
    /// back before `finalized`.
    fn wav_through_socket(format: AudioFormat) -> String {
        use crate::config::Config;
        use crate::inject::MockInjector;
        use crate::lang::WhatlangDetector;
        use crate::normalize::Normalizer;
        use crate::pipeline::Pipeline;
        use crate::vad::{SileroSegmenter, SileroTrimmer};

        struct NoNormalizer;
        impl Normalizer for NoNormalizer {
            fn normalize(&self, _: &str, _: &str) -> anyhow::Result<String> {
                unreachable!("[normalize] enabled = false in this test's config")
            }
        }

        struct RealEngine(Mutex<Pipeline>);
        impl RealtimeEngine for RealEngine {
            fn ensure_ready(&self) -> Result<(), String> {
                Ok(())
            }
            fn segmenter(&self) -> Result<Box<dyn Segmenter>, String> {
                SileroSegmenter::new(&crate::paths::models_dir(), 500, 20)
                    .map(|s| Box::new(s) as Box<dyn Segmenter>)
                    .map_err(|e| format!("{e:#}"))
            }
            fn transcribe(&self, samples: &[f32]) -> Result<Option<String>, String> {
                self.0
                    .lock()
                    .expect("fresh mutex")
                    .dictate_text(samples, false)
                    .map_err(|e| format!("{e:#}"))
            }
            fn touch(&self) {}
            fn describe(&self) -> EngineInfo {
                EngineInfo { model: "test".into(), normalize: false }
            }
        }

        let app_cfg = Config::from_str("[normalize]\nenabled = false\n").unwrap();
        let pipeline = Pipeline::new(
            app_cfg.clone(),
            crate::asr::build(&crate::paths::models_dir(), &app_cfg.asr).expect("build asr"),
            Box::new(SileroTrimmer::new(&crate::paths::models_dir()).expect("build vad")),
            Box::new(WhatlangDetector),
            Box::new(NoNormalizer),
            Box::new(MockInjector::default()),
        );
        let engine = Arc::new(RealEngine(Mutex::new(pipeline)));
        let handle = start(&cfg(), engine).expect("bind");
        let mut ws = connect_as(handle.addr(), format);
        assert_eq!(next_json(&mut ws)["type"], "ready");

        // The fixture as a client would send it, in frames whose size has
        // nothing to do with the sample size or the VAD window.
        let mut reader = hound::WavReader::open("fixtures/hallo_german.wav").expect("fixture");
        let samples: Vec<i16> = reader.samples::<i16>().map(|s| s.expect("sample")).collect();
        let (pcm, silence) = match format.encoding {
            Encoding::Linear16 => (
                samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>(),
                vec![0u8; 32_000],
            ),
            // Decimated 2:1 to 8 kHz and companded, which is what the relay
            // hands over. `0xFF` is mu-law silence, not `0x00`.
            Encoding::Mulaw => (
                samples.iter().step_by(2).map(|s| f32_to_mulaw(*s as f32 / 32768.0)).collect(),
                vec![0xFFu8; 8_000],
            ),
        };
        for frame in pcm.chunks(1023) {
            ws.send(Message::Binary(frame.to_vec().into())).expect("send audio");
        }
        // A second of silence, so the VAD closes the utterance the way a
        // speaker pausing would, rather than leaving it to `finalize`.
        ws.send(Message::Binary(silence.into())).expect("send silence");
        ws.send(Message::Text(r#"{"type":"finalize"}"#.into())).expect("finalize");

        let mut transcript = String::new();
        loop {
            let frame = next_json(&mut ws);
            match frame["type"].as_str() {
                Some("final") => transcript.push_str(frame["text"].as_str().unwrap_or("")),
                Some("finalized") => break,
                Some("error") => panic!("the endpoint reported: {frame}"),
                _ => {}
            }
        }
        ws.close(None).ok();
        handle.stop();
        // Non-empty, not a specific sentence: which model runs is
        // `[asr] model`, and `tests/asr_fixture.rs` makes the same choice for
        // the same reason on this fixture. What is being proved here is the
        // path -- socket bytes to segment to transcript -- not the accuracy
        // of whichever model happens to be installed. The callers print what
        // came back, so a run that passes for the wrong reason is readable.
        transcript
    }

    /// The inverse of `mulaw_to_f32`, for the test that has to *produce*
    /// mu-law. Not in the module proper: yappr only ever decodes.
    fn f32_to_mulaw(sample: f32) -> u8 {
        const BIAS: i32 = 0x84;
        const MAX: i32 = 32635;
        let s = (sample * 32768.0).clamp(-32768.0, 32767.0) as i32;
        let sign = if s < 0 { 0x80u8 } else { 0 };
        let magnitude = s.abs().min(MAX) + BIAS;
        let mut exponent = 7;
        let mut mask = 0x4000;
        while exponent > 0 && (magnitude & mask) == 0 {
            exponent -= 1;
            mask >>= 1;
        }
        let mantissa = ((magnitude >> (exponent + 3)) & 0x0F) as u8;
        !(sign | ((exponent as u8) << 4) | mantissa)
    }

    /// Stopping is not advisory: the port has to be free immediately
    /// afterwards, because `sync_realtime` stops the old listener and binds
    /// the new one in the same breath when the port changes.
    #[test]
    fn stopping_releases_the_port_for_the_next_listener() {
        let engine = Arc::new(FakeEngine {
            cut_at: 8_000,
            transcribed: Arc::new(AtomicUsize::new(0)),
            ready: Ok(()),
        });
        let first = start(&cfg(), engine.clone()).expect("bind");
        let port = first.addr().port();
        first.stop();

        let again = start(&RealtimeConfig { port, ..cfg() }, engine)
            .expect("the port must be free the moment stop() returns");
        assert_eq!(again.addr().port(), port);
        again.stop();
    }
}
