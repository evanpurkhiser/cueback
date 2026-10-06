# Cueback design

This document captures the preliminary design. It is expected to change as the
RX3 integrations are validated and the first real sessions are recorded.

## Purpose

Cueback is an always-available recorder and journal for DJ performances. It
captures the RX3 master output together with track, deck, mixer, and control
events. After capture, it identifies meaningful sessions and produces useful
artifacts without discarding the original evidence.

The initial target is Evan's XDJ-RX3 running firmware 1.19 and the existing RX3
Toolkit modules. The design should leave room for other devices and event
sources without generalizing their differences away prematurely.

## Goals

- Begin capturing without requiring a record button.
- Produce a lossless, trimmed recording of each useful session.
- Preserve enough raw information to revise session boundaries later.
- Generate an ordered playlist from tracks that were actually audible.
- Record a time-aligned history of deck, mixer, and physical-control activity.
- Detect multiple meaningful performances within one period of using the RX3.
- Support a synchronized visual replay of a captured performance.
- Recover cleanly from process, network, and device interruptions.

Physical playback of a captured performance on the RX3 is a possible later
feature. Exact audio reconstruction from control events is not an initial goal.

## Existing inputs

Cueback can build on several pieces that already exist.

### Master audio

The RX3 Toolkit PCM stream exposes the RX3 recorder bus on TCP port 7355 as
44.1 kHz, signed 16-bit little-endian stereo. The stream includes the completed
deck mix, channel faders, crossfader, and effects. Microphone inclusion follows
the RX3 recorder setting.

Each block includes an absolute sample-frame position, a sequence number, and
the sender's cumulative dropped-frame count. These fields make the audio stream
the authoritative session clock and make discontinuities observable.

The stream is live-edge only. Audio produced without a connected consumer is
not retained by the RX3, so Cueback must remain connected while the device is
available.

### Physical controls and remote control

The RX3 remote-control module exposes the firmware's central input dispatcher
on TCP port 7357. It publishes the same control tuple produced by physical
inputs:

- control code;
- operation;
- deck or mixer channel;
- integer value;
- floating-point value bits;
- auxiliary value;
- monotonic device timestamp; and
- physical or remote source.

It covers transport buttons, pads, jog wheels, tempo controls, channel faders,
crossfader, trim, EQ, Sound Color FX, Beat FX, browser controls, and encoders.
The protocol marks an event after a queue drop, which must become a visible
quality issue in the resulting session.

The same protocol can inject captured tuples. This is sufficient for
experimentation, but deterministic physical replay also needs explicit track
loading, initial-state restoration, checkpoints, and timing validation.

### Deck and track state

Several overlapping sources are available:

- the Toolkit Now Playing module publishes whole-deck snapshots containing
  loaded state, on-air state, track ID, BPM, tempo, play mode, and title;
- the earlier event-tracer prototype captures richer player state and accepted
  engine actions, including position, cue, loop, hot-cue, jog, and raw Pro DJ
  Link state;
- PRO DJ LINK status received by `rbl-linkd` describes loaded tracks, playback,
  tempo, master state, cue state, and player presence; and
- `prolink-connect` contains useful mix-status heuristics for deciding which
  tracks became part of a set.

The first implementation can combine PRO DJ LINK state with Now Playing and
the remote-control event stream. The richer event-tracer state should
eventually become a durable network protocol rather than a bounded RAM file.
The current Now Playing destination is the rear USB link-local broadcast, so
using it from the server over Wi-Fi requires a configurable destination or a
different transport.

### Library metadata

`rbl-linkd` already loads the rekordbox library and maps content IDs to titles,
artists, metadata, and playable file paths. Cueback should reuse that knowledge
through a narrow integration boundary instead of independently interpreting
the library database.

## Architecture

Cueback should run as a service separate from `rbl-linkd`. Library serving and
performance capture have different lifecycles, storage needs, and failure
modes. The services can share normalized track information without coupling
audio recording to the Link Export server.

```text
RX3 PCM stream ────────────┐
RX3 control events ────────┼──► collector ──► capture store
deck and track state ──────┤                      │
rekordbox library metadata ┘                      ▼
                                      session processor
                                                │
                           ┌────────────────────┼───────────────┐
                           ▼                    ▼               ▼
                      master.flac         playlist.m3u8    timeline/replay
```

The collector owns live connections, clock alignment, rolling buffers, durable
writes, and capture health. The processor operates on completed data and may be
run again whenever segmentation or metadata logic improves.

The PCM and remote-control services each admit one client. Cueback therefore
becomes their connection owner and must expose any future live consumers from
its own normalized stream rather than allowing tools to compete for the RX3
connections. The RX3 protocols are unauthenticated and belong only on the
trusted local network.

## Implementation

Cueback begins as one Rust crate and binary. Rust owns network ingestion,
session state, journaling, process supervision, and processing. Modules can be
split into separate crates when their ownership boundaries are demonstrated by
the implementation.

Audio encoding runs in a managed FFmpeg process. Cueback validates the RX3
framing and continuity, sends raw PCM to FFmpeg through a bounded pipe, drains
its diagnostics, and records its exit status. FLAC is the canonical lossless
format for capture chunks and published recordings.

## Capture model

Cueback distinguishes three scopes.

### Device run

A device run begins when the RX3 becomes reachable and ends when it powers off
or remains unreachable beyond a reconnect grace period. It records connection
generations, restarts, health information, and events that occur outside an
active recording.

### Live session

A live session is a durable ingestion window. It begins when non-silent audio
starts and remains open while there is audio or meaningful activity. It may
contain experimentation, pauses, and multiple finished mixes.

### Processed session

A processed session is a meaningful interval derived from a live session. It
has editable boundaries and metadata and can be split or merged without
changing its source capture. A processed session becomes published once its
name and outputs are accepted for normal use.

## Live-session lifecycle

When the RX3 is online, the collector is armed rather than immediately creating
a recording. It maintains a short rolling buffer of audio and recent events,
initially expected to cover 30 to 60 seconds.

Non-silent audio starts a live session. The rolling buffer is prepended so the
opening transient and setup immediately before it are preserved.

A live session becomes eligible to end only when all of the following remain
true:

- PCM is below a conservative silence threshold;
- neither deck is playing or on air;
- no control, browse, load, cue, jog, fader, or effect activity occurs; and
- no relevant deck or track state changes occur.

The preliminary inactivity timeout is ten minutes. The collector waits for the
timeout before finalizing, while the logical endpoint remains the last
meaningful audio or event plus a short tail, initially ten seconds.

If activity resumes before the timeout, the existing live session continues.
If audio begins after finalization, a new live session starts with the rolling
buffer prepended. Device loss ends the session after a short grace period so a
brief Wi-Fi interruption does not create an unnecessary boundary.

These values are policy, not file-format assumptions. Completed device-run
data should allow adjacent live sessions to be merged if the policy later
proves too aggressive.

## Durable capture

The collector should transcode PCM to FLAC as it arrives. It should write
bounded chunks, initially around five minutes each, instead of relying on one
large file remaining open for an entire evening. Chunking limits crash damage
and makes later splitting and merging straightforward.

The event journal is append-only. It should survive abrupt termination and
retain both normalized meaning and original protocol values. SQLite in WAL
mode is a candidate live store; completed captures can be archived as a compact
JSON Lines stream, optionally compressed with Zstandard.

Every capture records:

- protocol and schema versions;
- RX3 firmware and executable identity;
- connection and stream generations;
- FLAC chunk boundaries and hashes;
- PCM sequence, frame, and drop information;
- remote-control queue-drop flags;
- raw events and their semantic interpretations; and
- processing versions and decisions.

A recording with missing audio or events remains usable but must carry an
explicit quality warning.

## Time alignment

Audio frames are the canonical time domain. Events received from other sources
must be mapped onto that timeline.

The PCM and remote-control handshakes should eventually expose a shared RX3
boot or process identifier and a device monotonic timestamp. The collector can
then establish a mapping between device time, host monotonic time, wall time,
and PCM frame position.

Until the protocols share an identity and clock sample, the collector can
anchor connections using host monotonic timestamps. That is adequate for an
initial visual replay but makes restarts and reconnections less precise.

Each normalized event should retain all available clocks and the raw payload:

```json
{
  "deviceTimeUs": 1843920012,
  "hostTimeNs": 1791243045123456789,
  "audioFrame": 81294321,
  "source": "physical",
  "kind": "control",
  "control": "mixer.channel_fader",
  "channel": 2,
  "raw": {
    "keyCode": 20510,
    "operation": 5,
    "value": 811,
    "floatBits": 1061945344,
    "auxiliary": 0
  }
}
```

## Session processing

Processing begins with deterministic evidence:

- silence and audio energy;
- deck playback and on-air state;
- channel-fader and crossfader positions;
- track loads and unloads;
- cue, back-cue, loop, jog, and restart behavior;
- uninterrupted program duration; and
- connection or device boundaries.

This evidence produces candidate segments and boundary confidence. A period of
experimentation followed by silence, a new opening track, initialized mixer
controls, and a long uninterrupted progression is a strong boundary between
practice and a complete mix.

An AI-assisted stage may refine boundaries, classify the session, summarize
it, and suggest a name. Deterministic features remain available alongside the
suggestion, and accepting an AI decision never removes source data.

Possible classifications include:

- practice;
- transition practice;
- mix;
- preparation or browsing; and
- an unnamed recording needing review.

A suggested name can combine the local date, dominant genre or theme from
library metadata, and classification, such as `2026-10-09 trance practice`.
Every session also has a stable opaque identifier so renaming it does not break
references.

## Track inclusion and playlists

A track belongs in the generated playlist when evidence indicates that it was
audible, rather than merely loaded. On-air state, playback state, fader state,
mix-status timing, and audio overlap can contribute to that decision.

Order and repeated plays are preserved. Track identity resolves through the
rekordbox library when possible; unresolved tracks remain in session metadata
with the identifiers and titles that were observed.

Extended M3U cannot adequately express entry and exit positions within the
recorded mix. Cueback should therefore produce complementary artifacts:

- `playlist.m3u8` for source tracks in audible order;
- `tracklist.cue` or `chapters.json` for positions in the mixed recording; and
- `session.json` for complete resolved identity, timing, and confidence data.

## Session output

A preliminary published layout is:

```text
2026-10-09 trance practice/
  master.flac
  playlist.m3u8
  tracklist.cue
  session.json
  timeline.jsonl.zst
```

The published FLAC is assembled and trimmed from the lossless capture chunks.
Re-encoding remains lossless. Source chunks may be retained according to a
separate retention policy so boundaries can be revised later.

## Replay

### Visual replay

The first replay target synchronizes the recorded FLAC with a digital view of
the decks and mixer. It can show loaded tracks, playback position, cue and loop
state, faders, knobs, pads, jog movement, and effects as they changed during
the performance.

This replay is useful even when part of the event stream is incomplete. Gaps
should be shown rather than silently interpolated as known state.

### Physical replay

The remote-control protocol can submit captured control tuples to the RX3, but
literal timing playback is not sufficient for deterministic reproduction.
Physical replay additionally requires:

- explicit track loading by stable identity;
- a known initial deck and mixer state;
- checkpoints and observed acknowledgements;
- safe handling of controls that cannot be injected;
- compensation for loading and network latency; and
- a deliberate safety mode for hardware output.

The resulting performance may resemble the original without reproducing its
audio sample for sample. The captured FLAC remains the authoritative playback.

## Initial milestones

1. Maintain the PCM connection and record recoverable FLAC chunks with gap
   reporting.
2. Implement the armed, active, idle, and finalized live-session lifecycle.
3. Subscribe to physical control events and write an aligned durable journal.
4. Resolve deck state and track identities through the available RX3 and
   `rbl-linkd` sources.
5. Produce a deterministic trimmed FLAC, audible-only M3U8, track timings, and
   session metadata.
6. Add a small review workflow for splitting, merging, naming, and boundary
   adjustment.
7. Add assisted classification, naming, and boundary suggestions.
8. Build synchronized visual replay.
9. Experiment with guarded physical replay.

## Open questions

- Which process and protocol should expose normalized `rbl-linkd` player and
  library events to Cueback?
- Should completed source chunks be retained indefinitely, for a fixed period,
  or until every derived session is accepted?
- What silence threshold and inactivity timeout work well in real practice?
- Should deliberate control activity without audio create a journal-only live
  session?
- How should tracks from USB devices or sources outside the server library be
  resolved?
- Which event-tracer observations belong in the stable RX3 network protocol?
- What review interface is useful before a full web application exists?
